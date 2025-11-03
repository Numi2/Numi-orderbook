// src/recovery.rs
use crate::metrics;
use bytes::BufMut;
use crossbeam_channel::{Receiver, Sender};
use std::fs::OpenOptions;
use std::io::Write as IoWrite;
use std::sync::Arc;
use std::thread;

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
        let _ = self.tx.try_send(RecoveryRequest::Gap { from, to });
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
                let now = crate::util::now_nanos();
                if now.saturating_sub(last_log_ns) >= 100_000_000 {
                    last_log_ns = now;
                    log::warn!("GAP detected; recommend out-of-band recovery for [{from}..{to}]");
                }
                // TODO: integrate TCP/unicast replay client here.
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
) -> (RecoveryClient, RecoveryHandle) {
    let (tx, rx) = crossbeam_channel::bounded::<RecoveryRequest>(1024);
    let join = std::thread::Builder::new()
        .name("recovery-tcp".into())
        .spawn(move || run_injector(addr, q_recovery, pool, rx, backlog_path))
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
) {
    log::info!(
        "recovery injector running (tcp={:?})",
        addr.to_socket_addrs().ok().and_then(|mut it| it.next())
    );
    let mut backlog =
        backlog_path.and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok());
    // Simple coalescing of pending gaps: on each received gap, drain additional
    // requests non-blockingly and merge overlapping/adjacent ranges before fetch.
    while let Ok(first) = rx.recv() {
        let (mut lo, mut hi) = match first {
            RecoveryRequest::Gap { from, to } => (from, to),
        };
        if lo > hi {
            continue;
        }
        // Drain more and coalesce
        while let Ok(next) = rx.try_recv() {
            let (from, to) = match next {
                RecoveryRequest::Gap { from, to } => (from, to),
            };
            if from <= hi.saturating_add(1) && to >= lo.saturating_sub(1) {
                // overlap or adjacent
                if from < lo {
                    lo = from;
                }
                if to > hi {
                    hi = to;
                }
            } else {
                // Non-overlapping; log individually
                if let Some(f) = backlog.as_mut() {
                    let _ = writeln!(f, "gap {} {}", from, to);
                    let _ = f.flush();
                }
            }
        }
        if let Some(f) = backlog.as_mut() {
            let _ = writeln!(f, "gap {} {}", lo, hi);
            let _ = f.flush();
        }
        if let Err(e) = fetch_and_inject(&addr, lo, hi, &q_recovery, &pool) {
            log::error!("replay fetch failed: {e:?}");
        }
    }
}

fn fetch_and_inject<A: std::net::ToSocketAddrs>(
    addr: &A,
    from: u64,
    to: u64,
    q_recovery: &Arc<SpscQueue<Pkt>>, // recovery->merge input
    pool: &Arc<PacketPool>,
) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    // Establish TCP to replay service
    let mut stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true).ok();
    // Example control request: "REPLAY from to\n" (replace with real venue protocol)
    let req = format!("REPLAY {} {}\n", from, to);
    stream.write_all(req.as_bytes())?;
    stream.flush().ok();

    // Example payload framing: [u32 len][u64 seq][bytes...]
    let mut hdr = [0u8; 12];
    loop {
        if stream.read_exact(&mut hdr).is_err() {
            break;
        }
        let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let seq = u64::from_be_bytes([
            hdr[4], hdr[5], hdr[6], hdr[7], hdr[8], hdr[9], hdr[10], hdr[11],
        ]);
        if len == 0 {
            break;
        }
        let mut bufm = pool.get();
        // Safety: buffer is at least pool's max packet size
        let dst = unsafe {
            let s = bufm.chunk_mut();
            std::slice::from_raw_parts_mut(s.as_mut_ptr(), s.len())
        };
        if len > dst.len() {
            anyhow::bail!("replay packet too large: {}", len);
        }
        let mut read_so_far = 0usize;
        while read_so_far < len {
            let n = stream.read(&mut dst[read_so_far..len])?;
            if n == 0 {
                anyhow::bail!("unexpected EOF from replay server");
            }
            read_so_far += n;
        }
        unsafe {
            bufm.advance_mut(len);
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
        metrics::inc_decode_pkts();
    }

    Ok(())
}
