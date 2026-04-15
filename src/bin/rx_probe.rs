use orderbook::config::TimestampingMode;
use orderbook::parser::SeqExtractor;
use orderbook::pool::{PacketPool, Pkt, TsKind};
use orderbook::rx_udp::{self, UdpRxConfig};
use orderbook::spsc::SpscQueue;
use orderbook::util::{now_nanos, BarrierFlag};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

struct LeU64Seq;

impl SeqExtractor for LeU64Seq {
    fn extract_seq(&self, pkt: &[u8]) -> Option<u64> {
        let bytes: [u8; 8] = pkt.get(0..8)?.try_into().ok()?;
        Some(u64::from_le_bytes(bytes))
    }
}

struct ProbeArgs {
    packets: u64,
    payload_size: usize,
    batch: usize,
    timestamping: TimestampingMode,
    rate_pps: u64,
    queue_capacity: usize,
    pool_size: usize,
    recv_buffer_bytes: usize,
    timeout_ms: u64,
}

fn main() -> anyhow::Result<()> {
    let args = ProbeArgs::parse()?;
    let receiver = build_probe_receiver(args.recv_buffer_bytes)?;
    enable_probe_timestamping(receiver.as_raw_fd(), &args.timestamping)?;
    let dest = receiver.local_addr()?;
    let sender = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;

    let shutdown = Arc::new(BarrierFlag::default());
    let send_stop = Arc::new(AtomicBool::new(false));
    let send_done = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicU64::new(0));
    let send_errors = Arc::new(AtomicU64::new(0));
    let pool = Arc::new(PacketPool::new(args.pool_size, args.payload_size)?);
    let q = Arc::new(SpscQueue::new(args.queue_capacity));

    let rx_shutdown = shutdown.clone();
    let rx_pool = pool.clone();
    let rx_q = q.clone();
    let rx_batch = args.batch;
    let rx_ts = args.timestamping.clone();
    let rx_thread = thread::Builder::new()
        .name("rx-probe".into())
        .spawn(move || {
            rx_udp::rx_udp_loop(
                "P",
                &receiver,
                Arc::new(LeU64Seq),
                rx_q,
                rx_pool,
                rx_shutdown,
                UdpRxConfig {
                    spin_loops_per_yield: 64,
                    rx_batch,
                    ts_mode: Some(rx_ts),
                },
            )
        })?;

    let tx_stop = send_stop.clone();
    let tx_done = send_done.clone();
    let tx_sent = sent.clone();
    let tx_errors = send_errors.clone();
    let payload_size = args.payload_size;
    let packets = args.packets;
    let rate_pps = args.rate_pps;
    let tx_thread = thread::Builder::new()
        .name("tx-probe".into())
        .spawn(move || {
            let mut payload = vec![0_u8; payload_size];
            let nanos_per_packet = if rate_pps == 0 {
                0
            } else {
                1_000_000_000_u64 / rate_pps
            };
            thread::sleep(Duration::from_millis(10));
            for seq in 1..=packets {
                if tx_stop.load(Ordering::Relaxed) {
                    break;
                }
                payload[..8].copy_from_slice(&seq.to_le_bytes());
                match sender.send_to(&payload, dest) {
                    Ok(n) if n == payload.len() => {
                        tx_sent.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(_) | Err(_) => {
                        tx_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                if nanos_per_packet > 0 {
                    busy_sleep_nanos(nanos_per_packet);
                }
            }
            tx_done.store(true, Ordering::Release);
        })?;

    let start = Instant::now();
    let timeout = Duration::from_millis(args.timeout_ms);
    let mut idle_iters = 0_u32;
    let mut received = 0_u64;
    let mut expected_next = 1_u64;
    let mut sequence_gaps = 0_u64;
    let mut out_of_order_or_duplicate = 0_u64;
    let mut sw_timestamps = 0_u64;
    let mut hw_sys_timestamps = 0_u64;
    let mut hw_raw_timestamps = 0_u64;
    let mut latency_ns = Vec::new();
    let mut last_progress = Instant::now();

    loop {
        if let Some(pkt) = q.pop() {
            received = received.saturating_add(1);
            classify_sequence(
                pkt.seq,
                &mut expected_next,
                &mut sequence_gaps,
                &mut out_of_order_or_duplicate,
            );
            match pkt._ts_kind {
                TsKind::Sw => sw_timestamps = sw_timestamps.saturating_add(1),
                TsKind::HwSys => hw_sys_timestamps = hw_sys_timestamps.saturating_add(1),
                TsKind::HwRaw => hw_raw_timestamps = hw_raw_timestamps.saturating_add(1),
                TsKind::None => {}
            }
            record_latency(&pkt, &mut latency_ns);
            pkt.recycle(&pool);
            idle_iters = 0;
            last_progress = Instant::now();
            if received >= args.packets {
                break;
            }
        } else {
            if send_done.load(Ordering::Acquire)
                && last_progress.elapsed() > Duration::from_millis(250)
            {
                break;
            }
            if start.elapsed() > timeout {
                send_stop.store(true, Ordering::Release);
                break;
            }
            orderbook::util::adaptive_wait(&mut idle_iters, 64);
        }
    }

    send_stop.store(true, Ordering::Release);
    tx_thread
        .join()
        .map_err(|_| anyhow::anyhow!("tx-probe thread panicked"))?;
    shutdown.raise();
    rx_thread
        .join()
        .map_err(|_| anyhow::anyhow!("rx-probe thread panicked"))??;

    latency_ns.sort_unstable();
    let elapsed = start.elapsed();
    let sent = sent.load(Ordering::Relaxed);
    let send_errors = send_errors.load(Ordering::Relaxed);
    let missing = sent.saturating_sub(received);
    println!(
        "rx_probe: sent={} received={} missing={} send_errors={} elapsed_ms={:.3} rx_pps={:.3} seq_gaps={} dup_or_ooo={} pool_available={} sw_ts={} hw_sys_ts={} hw_raw_ts={} latency_samples={} p50_ns={} p99_ns={} max_ns={}",
        sent,
        received,
        missing,
        send_errors,
        elapsed.as_secs_f64() * 1000.0,
        received as f64 / elapsed.as_secs_f64(),
        sequence_gaps,
        out_of_order_or_duplicate,
        pool.available(),
        sw_timestamps,
        hw_sys_timestamps,
        hw_raw_timestamps,
        latency_ns.len(),
        percentile(&latency_ns, 50),
        percentile(&latency_ns, 99),
        latency_ns.last().copied().unwrap_or(0),
    );

    if send_errors > 0 {
        anyhow::bail!("rx_probe send_errors={send_errors}");
    }
    if missing > 0 || sequence_gaps > 0 || out_of_order_or_duplicate > 0 {
        anyhow::bail!(
            "rx_probe packet integrity failed: missing={} seq_gaps={} dup_or_ooo={}",
            missing,
            sequence_gaps,
            out_of_order_or_duplicate
        );
    }

    Ok(())
}

impl ProbeArgs {
    fn parse() -> anyhow::Result<Self> {
        let args = std::env::args().collect::<Vec<_>>();
        if args.get(1).is_some_and(|s| s == "-h" || s == "--help") {
            eprintln!(
                "usage: rx_probe [packets=100000] [payload_size=64] [batch=32] [timestamping=software|off] [rate_pps=0] [queue_capacity=65536] [pool_size=131072] [recv_buffer_bytes=4194304] [timeout_ms=10000]"
            );
            std::process::exit(2);
        }
        let packets = parse_arg(&args, 1, 100_000_u64)?;
        let payload_size = parse_arg(&args, 2, 64_usize)?.max(8);
        let batch = parse_arg(&args, 3, 32_usize)?.max(1);
        let timestamping = args
            .get(4)
            .map(|s| parse_timestamping(s))
            .transpose()?
            .unwrap_or(TimestampingMode::Software);
        let rate_pps = parse_arg(&args, 5, 0_u64)?;
        let queue_capacity = parse_arg(&args, 6, 65_536_usize)?.max(2);
        let pool_size = parse_arg(&args, 7, 131_072_usize)?.max(queue_capacity);
        let recv_buffer_bytes = parse_arg(&args, 8, 4_194_304_usize)?;
        let timeout_ms = parse_arg(&args, 9, 10_000_u64)?;
        Ok(Self {
            packets,
            payload_size,
            batch,
            timestamping,
            rate_pps,
            queue_capacity,
            pool_size,
            recv_buffer_bytes,
            timeout_ms,
        })
    }
}

fn parse_arg<T>(args: &[String], idx: usize, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    args.get(idx)
        .map(|s| s.parse::<T>().map_err(anyhow::Error::from))
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn parse_timestamping(value: &str) -> anyhow::Result<TimestampingMode> {
    match value {
        "off" => Ok(TimestampingMode::Off),
        "software" | "sw" => Ok(TimestampingMode::Software),
        "hardware" | "hardware_raw" => {
            anyhow::bail!("rx_probe supports timestamping=off|software")
        }
        other => anyhow::bail!("unknown timestamping mode {other:?}"),
    }
}

fn build_probe_receiver(recv_buffer_bytes: usize) -> anyhow::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true).ok();
    if recv_buffer_bytes > 0 {
        sock.set_recv_buffer_size(recv_buffer_bytes)?;
        let actual = sock.recv_buffer_size()?;
        if actual < recv_buffer_bytes {
            eprintln!(
                "rx_probe: requested recv_buffer_bytes={} actual={}",
                recv_buffer_bytes, actual
            );
        }
    }
    sock.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into())?;
    let udp: UdpSocket = sock.into();
    udp.set_nonblocking(true)?;
    Ok(udp)
}

fn enable_probe_timestamping(fd: libc::c_int, mode: &TimestampingMode) -> anyhow::Result<()> {
    match mode {
        TimestampingMode::Off => Ok(()),
        TimestampingMode::Software => {
            #[cfg(target_os = "linux")]
            let opt = libc::SO_TIMESTAMPNS;
            #[cfg(target_os = "macos")]
            let opt = libc::SO_TIMESTAMP_MONOTONIC;
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            anyhow::bail!("software RX timestamping is only supported on Linux and macOS");

            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let on: libc::c_int = 1;
                let rc = unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        opt,
                        &on as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    )
                };
                if rc != 0 {
                    anyhow::bail!(
                        "set software RX timestamping opt {}: {}",
                        opt,
                        std::io::Error::last_os_error()
                    );
                }
                Ok(())
            }
        }
        TimestampingMode::Hardware | TimestampingMode::HardwareRaw => {
            anyhow::bail!("rx_probe supports timestamping=off|software")
        }
    }
}

fn classify_sequence(
    seq: u64,
    expected_next: &mut u64,
    sequence_gaps: &mut u64,
    out_of_order_or_duplicate: &mut u64,
) {
    if seq == *expected_next {
        *expected_next = expected_next.saturating_add(1);
    } else if seq > *expected_next {
        *sequence_gaps = sequence_gaps.saturating_add(seq - *expected_next);
        *expected_next = seq.saturating_add(1);
    } else {
        *out_of_order_or_duplicate = out_of_order_or_duplicate.saturating_add(1);
    }
}

fn record_latency(pkt: &Pkt, latency_ns: &mut Vec<u64>) {
    if pkt.ts_nanos == 0 {
        return;
    }
    let now = now_nanos();
    if now > pkt.ts_nanos {
        latency_ns.push(now - pkt.ts_nanos);
    }
}

fn percentile(values: &[u64], percentile: u64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let idx = ((values.len() - 1) as u64)
        .saturating_mul(percentile)
        .saturating_div(100) as usize;
    values[idx]
}

#[inline]
fn busy_sleep_nanos(ns: u64) {
    let start = Instant::now();
    while start.elapsed().as_nanos() as u64 <= ns {
        std::hint::spin_loop();
    }
}
