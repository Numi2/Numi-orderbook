// src/recovery.rs
use crate::metrics;
use bytes::BufMut;
use crossbeam_channel::{Receiver, Sender};
use serde::Deserialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum RecoveryRequest {
    /// Request to recover [from, to] inclusive range (sequence numbers).
    Gap { from: u64, to: u64 },
}

pub struct Client {
    tx: Sender<RecoveryRequest>,
}

impl Client {
    pub fn notify_gap(&self, from: u64, to: u64) {
        if from > to {
            return;
        }
        if self.tx.try_send(RecoveryRequest::Gap { from, to }).is_err() {
            metrics::inc_recovery_dropped_request();
        }
    }
}

/// Trait for pluggable replayers to unify gap notifications across components.
pub trait Replayer: Send + Sync {
    fn notify_gap(&self, from: u64, to: u64);
}

pub type RecoveryClient = Arc<dyn Replayer>;

impl Replayer for Client {
    #[inline]
    fn notify_gap(&self, from: u64, to: u64) {
        self.notify_gap(from, to);
    }
}

pub struct RecoveryHandle {
    _join: thread::JoinHandle<()>,
}

impl RecoveryHandle {
    #[allow(dead_code)]
    pub fn join(self) {
        let _ = self._join.join();
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RecoveryOptions {
    pub retry_attempts: u32,
    pub retry_backoff_ms: u64,
    pub min_request_interval_ms: u64,
    pub slo_ms: u64,
    pub unrecoverable_policy: UnrecoverablePolicy,
    pub request_timeout_ms: u64,
    pub replay_protocol: ReplayProtocol,
}

impl Default for RecoveryOptions {
    fn default() -> Self {
        Self {
            retry_attempts: 3,
            retry_backoff_ms: 10,
            min_request_interval_ms: 0,
            slo_ms: 100,
            unrecoverable_policy: UnrecoverablePolicy::Log,
            request_timeout_ms: 250,
            replay_protocol: ReplayProtocol::LenSeqPayload,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayProtocol {
    /// Control request is `REPLAY <from> <to>\n`.
    /// Response stream is repeated `[u32_be len][u64_be seq][payload]`,
    /// terminated by a zero len frame or EOF.
    LenSeqPayload,
}

impl Default for ReplayProtocol {
    fn default() -> Self {
        Self::LenSeqPayload
    }
}

impl ReplayProtocol {
    fn request_bytes(self, from: u64, to: u64) -> Vec<u8> {
        match self {
            Self::LenSeqPayload => format!("REPLAY {from} {to}\n").into_bytes(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnrecoverablePolicy {
    Log,
    Panic,
    Exit,
}

impl Default for UnrecoverablePolicy {
    fn default() -> Self {
        Self::Log
    }
}

/// Spawn a basic recovery manager that logs requests.
/// Replace internals with exchange-specific replay logic.
pub fn spawn_logger() -> (RecoveryClient, RecoveryHandle) {
    let (tx, rx) = crossbeam_channel::bounded::<RecoveryRequest>(1024);
    let join = std::thread::Builder::new()
        .name("recovery".into())
        .spawn(move || run(rx))
        .expect("spawn recovery");
    let client: RecoveryClient = Arc::new(Client { tx });
    (client, RecoveryHandle { _join: join })
}

fn run(rx: Receiver<RecoveryRequest>) {
    log::info!("recovery manager running (logger mode)");
    let mut last_log_ns: u64 = 0;
    while let Ok(req) = rx.recv() {
        match req {
            RecoveryRequest::Gap { from, to } => {
                if from > to {
                    continue;
                }
                metrics::inc_recovery_request();
                let now = crate::util::now_nanos();
                if now.saturating_sub(last_log_ns) >= 100_000_000 {
                    last_log_ns = now;
                    log::warn!("GAP detected; recommend out-of-band recovery for [{from}..{to}]");
                }
            }
        }
    }
}

// -------------------- Optional: TCP replay injector --------------------
// Feed recovered sequences directly into the merged decode queue. Keeps
// the Pkt contract intact. The on-wire replay protocol is venue-specific;
// replace the body of `fetch_and_inject` accordingly.

use crate::pool::{PacketPool, Pkt, PktBuf, TsKind};
use crate::spsc::SpscQueue;

pub fn spawn_tcp_injector<A: std::net::ToSocketAddrs + Send + 'static>(
    addr: A,
    q_recovery: Arc<SpscQueue<Pkt>>, // dedicated recovery->merge SPSC queue
    pool: Arc<PacketPool>,
    backlog_path: Option<String>,
    opts: RecoveryOptions,
) -> (RecoveryClient, RecoveryHandle) {
    let (tx, rx) = crossbeam_channel::bounded::<RecoveryRequest>(1024);
    let join = std::thread::Builder::new()
        .name("recovery-tcp".into())
        .spawn(move || run_injector(addr, q_recovery, pool, rx, backlog_path, opts))
        .expect("spawn recovery injector");
    let client: RecoveryClient = Arc::new(Client { tx });
    (client, RecoveryHandle { _join: join })
}

fn run_injector<A: std::net::ToSocketAddrs>(
    addr: A,
    q_recovery: Arc<SpscQueue<Pkt>>, // recovery->merge input
    pool: Arc<PacketPool>,
    rx: Receiver<RecoveryRequest>,
    backlog_path: Option<String>,
    opts: RecoveryOptions,
) {
    log::info!(
        "recovery injector running (tcp={:?})",
        addr.to_socket_addrs().ok().and_then(|mut it| it.next())
    );
    let mut backlog =
        backlog_path.and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok());
    let mut last_fetch_started: Option<Instant> = None;
    // Coalesce pending gaps on each wakeup. Drain available requests so every
    // non-overlapping range is fetched, not only the first merged range.
    while let Ok(first) = rx.recv() {
        let first_range = match first {
            RecoveryRequest::Gap { from, to } => (from, to),
        };
        if first_range.0 > first_range.1 {
            continue;
        }

        let mut ranges = vec![first_range];
        while let Ok(next) = rx.try_recv() {
            let (from, to) = match next {
                RecoveryRequest::Gap { from, to } => (from, to),
            };
            if from <= to {
                ranges.push((from, to));
            }
        }

        for (lo, hi) in coalesce_gap_ranges(ranges) {
            let range_start = Instant::now();
            metrics::inc_recovery_request();
            write_backlog(&mut backlog, "requested", lo, hi, None);

            let attempts = opts.retry_attempts.max(1);
            let mut fetched = false;
            for attempt in 1..=attempts {
                throttle_replay_request(&mut last_fetch_started, opts.min_request_interval_ms);
                match fetch_and_inject(&addr, lo, hi, &q_recovery, &pool, opts) {
                    Ok(injected) => {
                        metrics::inc_recovery_fetched();
                        write_backlog(&mut backlog, "fetched", lo, hi, Some(injected));
                        fetched = true;
                        break;
                    }
                    Err(e) => {
                        if attempt >= attempts {
                            metrics::inc_recovery_failure();
                            write_backlog(&mut backlog, "failed", lo, hi, None);
                            log::error!(
                                "replay fetch failed after {} attempt(s) for [{lo}..{hi}]: {e:?}",
                                attempts
                            );
                        } else {
                            metrics::inc_recovery_retry();
                            let sleep_ms = opts.retry_backoff_ms.saturating_mul(attempt as u64);
                            log::warn!(
                                "replay fetch attempt {attempt}/{attempts} failed for [{lo}..{hi}]: {e:?}; retrying in {sleep_ms}ms"
                            );
                            if sleep_ms > 0 {
                                thread::sleep(Duration::from_millis(sleep_ms));
                            }
                        }
                    }
                }
            }

            if !fetched {
                handle_unrecoverable_gap(&mut backlog, lo, hi, opts.unrecoverable_policy);
            }

            record_recovery_range_duration(&mut backlog, lo, hi, range_start, opts.slo_ms);
        }
    }
}

fn handle_unrecoverable_gap(
    backlog: &mut Option<std::fs::File>,
    from: u64,
    to: u64,
    policy: UnrecoverablePolicy,
) {
    metrics::inc_recovery_unrecoverable_gap();
    write_backlog(backlog, "unrecoverable", from, to, None);

    match policy {
        UnrecoverablePolicy::Log => {
            log::error!("recovery range [{from}..{to}] is unrecoverable by configured replay");
        }
        UnrecoverablePolicy::Panic => {
            panic!("recovery range [{from}..{to}] is unrecoverable by configured replay");
        }
        UnrecoverablePolicy::Exit => {
            log::error!(
                "recovery range [{from}..{to}] is unrecoverable; terminating process by policy"
            );
            std::process::exit(2);
        }
    }
}

fn throttle_replay_request(last_fetch_started: &mut Option<Instant>, min_interval_ms: u64) {
    let min_interval = Duration::from_millis(min_interval_ms);
    if min_interval_ms > 0 {
        if let Some(last) = *last_fetch_started {
            let elapsed = last.elapsed();
            if elapsed < min_interval {
                thread::sleep(min_interval - elapsed);
            }
        }
    }
    *last_fetch_started = Some(Instant::now());
}

fn record_recovery_range_duration(
    backlog: &mut Option<std::fs::File>,
    from: u64,
    to: u64,
    started: Instant,
    slo_ms: u64,
) {
    let elapsed = started.elapsed();
    let elapsed_ns = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
    metrics::observe_recovery_range_ns(elapsed_ns);

    let elapsed_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
    if slo_ms > 0 && elapsed_ms > slo_ms {
        metrics::inc_recovery_slo_violation();
        write_backlog(backlog, "slo_violation", from, to, None);
        log::warn!(
            "recovery range [{from}..{to}] exceeded SLO: elapsed_ms={} slo_ms={}",
            elapsed_ms,
            slo_ms
        );
    }
}

fn write_backlog(
    backlog: &mut Option<std::fs::File>,
    status: &str,
    from: u64,
    to: u64,
    packets: Option<usize>,
) {
    if let Some(f) = backlog.as_mut() {
        if let Some(n) = packets {
            let _ = writeln!(f, "{status} {from} {to} packets={n}");
        } else {
            let _ = writeln!(f, "{status} {from} {to}");
        }
        let _ = f.flush();
    }
}

fn coalesce_gap_ranges(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.retain(|(from, to)| from <= to);
    ranges.sort_unstable_by_key(|(from, to)| (*from, *to));

    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        if let Some((_, last_to)) = out.last_mut() {
            if from <= last_to.saturating_add(1) {
                if to > *last_to {
                    *last_to = to;
                }
                continue;
            }
        }
        out.push((from, to));
    }
    out
}

fn fetch_and_inject<A: std::net::ToSocketAddrs>(
    addr: &A,
    from: u64,
    to: u64,
    q_recovery: &Arc<SpscQueue<Pkt>>, // recovery->merge input
    pool: &Arc<PacketPool>,
    opts: RecoveryOptions,
) -> anyhow::Result<usize> {
    use std::io::Read;
    use std::net::TcpStream;
    // Establish TCP to replay service
    let mut stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true).ok();
    if opts.request_timeout_ms > 0 {
        let timeout = Duration::from_millis(opts.request_timeout_ms);
        stream.set_read_timeout(Some(timeout)).ok();
        stream.set_write_timeout(Some(timeout)).ok();
    }

    let req = opts.replay_protocol.request_bytes(from, to);
    stream.write_all(&req)?;
    stream.flush().ok();

    let mut injected = 0usize;
    loop {
        let Some((seq, bufm, len)) = read_replay_frame(opts.replay_protocol, &mut stream, pool)?
        else {
            break;
        };
        if !seq_in_requested_range(seq, from, to) {
            metrics::inc_recovery_stale_packet();
            pool.put(bufm);
            continue;
        }
        let pkt = Pkt {
            buf: PktBuf::Bytes(bufm),
            len,
            seq,
            ts_nanos: crate::util::now_nanos(),
            chan: b'R',
            _ts_kind: TsKind::Sw,
            merge_emit_ns: 0,
        };
        q_recovery.push_blocking(pkt);
        injected += 1;
    }

    metrics::inc_recovery_injected_packets(injected as u64);
    Ok(injected)
}

fn read_replay_frame(
    protocol: ReplayProtocol,
    stream: &mut impl Read,
    pool: &Arc<PacketPool>,
) -> anyhow::Result<Option<(u64, bytes::BytesMut, usize)>> {
    match protocol {
        ReplayProtocol::LenSeqPayload => read_len_seq_payload_frame(stream, pool),
    }
}

fn read_len_seq_payload_frame(
    stream: &mut impl Read,
    pool: &Arc<PacketPool>,
) -> anyhow::Result<Option<(u64, bytes::BytesMut, usize)>> {
    let mut hdr = [0u8; 12];
    match stream.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
    let seq = u64::from_be_bytes([
        hdr[4], hdr[5], hdr[6], hdr[7], hdr[8], hdr[9], hdr[10], hdr[11],
    ]);
    if len == 0 {
        return Ok(None);
    }

    let mut bufm = pool.get();
    if len > bufm.capacity() {
        pool.put(bufm);
        anyhow::bail!("replay packet too large: {}", len);
    }

    let dst = unsafe {
        let s = bufm.chunk_mut();
        std::slice::from_raw_parts_mut(s.as_mut_ptr(), s.len())
    };
    let mut read_so_far = 0usize;
    while read_so_far < len {
        let n = stream.read(&mut dst[read_so_far..len])?;
        if n == 0 {
            pool.put(bufm);
            anyhow::bail!("unexpected EOF from replay server");
        }
        read_so_far += n;
    }
    unsafe {
        bufm.advance_mut(len);
    }

    Ok(Some((seq, bufm, len)))
}

#[inline]
fn seq_in_requested_range(seq: u64, from: u64, to: u64) -> bool {
    from <= seq && seq <= to
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Arc;

    #[test]
    fn coalesces_overlapping_adjacent_and_disordered_gaps() {
        let ranges = vec![(10, 12), (2, 4), (5, 7), (20, 20), (18, 19), (7, 9)];
        assert_eq!(coalesce_gap_ranges(ranges), vec![(2, 12), (18, 20)]);
    }

    #[test]
    fn coalescing_drops_invalid_ranges() {
        let ranges = vec![(5, 3), (1, 1), (3, 2), (2, 2)];
        assert_eq!(coalesce_gap_ranges(ranges), vec![(1, 2)]);
    }

    #[test]
    fn requested_range_rejects_stale_replay_sequences() {
        assert!(seq_in_requested_range(10, 10, 12));
        assert!(seq_in_requested_range(11, 10, 12));
        assert!(seq_in_requested_range(12, 10, 12));
        assert!(!seq_in_requested_range(9, 10, 12));
        assert!(!seq_in_requested_range(13, 10, 12));
    }

    #[test]
    fn len_seq_payload_request_is_stable() {
        assert_eq!(
            ReplayProtocol::LenSeqPayload.request_bytes(100, 105),
            b"REPLAY 100 105\n"
        );
    }

    #[test]
    fn len_seq_payload_frame_reader_parses_one_frame() {
        let pool = Arc::new(PacketPool::new(2, 64).unwrap());
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&3u32.to_be_bytes());
        bytes.extend_from_slice(&42u64.to_be_bytes());
        bytes.extend_from_slice(b"abc");
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&0u64.to_be_bytes());

        let mut cursor = Cursor::new(bytes);
        let (seq, buf, len) = read_len_seq_payload_frame(&mut cursor, &pool)
            .unwrap()
            .unwrap();
        assert_eq!(seq, 42);
        assert_eq!(len, 3);
        assert_eq!(&buf[..len], b"abc");
        pool.put(buf);

        assert!(read_len_seq_payload_frame(&mut cursor, &pool)
            .unwrap()
            .is_none());
    }
}
