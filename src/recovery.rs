// src/recovery.rs
use crossbeam_channel::{Receiver, Sender};
use crate::metrics;
use crossbeam::queue::ArrayQueue;
use std::sync::Arc;
use std::thread;

#[derive(Debug, Clone)]
pub enum RecoveryRequest {
    /// Request to recover [from, to] inclusive range (sequence numbers).
    Gap { from: u64, to: u64 },
}

#[derive(Clone)]
pub struct Client {
    tx: Sender<RecoveryRequest>,
}

impl Client {
    pub fn notify_gap(&self, from: u64, to: u64) {
        let _ = self.tx.try_send(RecoveryRequest::Gap { from, to });
    }
}

pub struct RecoveryHandle {
    join: thread::JoinHandle<()>,
}

impl RecoveryHandle {
    pub fn join(self) {
        let _ = self.join.join();
    }
}

/// Spawn a basic recovery manager that logs requests.
/// Replace internals with exchange-specific replay logic.
pub fn spawn_logger() -> (Client, RecoveryHandle) {
    let (tx, rx) = crossbeam_channel::bounded::<RecoveryRequest>(1024);
    let join = std::thread::Builder::new()
        .name("recovery".into())
        .spawn(move || run(rx))
        .expect("spawn recovery");
    (Client { tx }, RecoveryHandle { join })
}

fn run(rx: Receiver<RecoveryRequest>) {
    log::info!("recovery manager running (logger mode)");
    while let Ok(req) = rx.recv() {
        match req {
            RecoveryRequest::Gap { from, to } => {
                log::warn!("GAP detected; recommend out-of-band recovery for [{from}..{to}]");
                // TODO: integrate TCP/unicast replay client here.
            }
        }
    }
}

// -------------------- Optional: TCP replay injector --------------------
// Feed recovered sequences directly into the merged decode queue. Keeps
// the Pkt contract intact. The on-wire replay protocol is venue-specific;
// replace the body of `fetch_and_inject` accordingly.

use crate::pool::{PacketPool, Pkt};

pub fn spawn_tcp_injector<A: std::net::ToSocketAddrs + Send + 'static>(
    addr: A,
    q_merged: Arc<ArrayQueue<Pkt>>,
    pool: Arc<PacketPool>,
) -> (Client, RecoveryHandle) {
    let (tx, rx) = crossbeam_channel::bounded::<RecoveryRequest>(1024);
    let join = std::thread::Builder::new()
        .name("recovery-tcp".into())
        .spawn(move || run_injector(addr, q_merged, pool, rx))
        .expect("spawn recovery injector");
    (Client { tx }, RecoveryHandle { join })
}

fn run_injector<A: std::net::ToSocketAddrs>(
    addr: A,
    q_merged: Arc<ArrayQueue<Pkt>>,
    pool: Arc<PacketPool>,
    rx: Receiver<RecoveryRequest>,
) {
    log::info!("recovery injector running (tcp={:?})", addr.to_socket_addrs().ok().and_then(|mut it| it.next()));
    while let Ok(req) = rx.recv() {
        match req {
            RecoveryRequest::Gap { from, to } => {
                if from > to { continue; }
                if let Err(e) = fetch_and_inject(&addr, from, to, &q_merged, &pool) {
                    log::error!("replay fetch failed: {e:?}");
                }
            }
        }
    }
}

fn fetch_and_inject<A: std::net::ToSocketAddrs>(
    addr: &A,
    from: u64,
    to: u64,
    q_merged: &Arc<ArrayQueue<Pkt>>,
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
        if let Err(_) = stream.read_exact(&mut hdr) { break; }
        let len = u32::from_be_bytes([hdr[0],hdr[1],hdr[2],hdr[3]]) as usize;
        let seq = u64::from_be_bytes([hdr[4],hdr[5],hdr[6],hdr[7],hdr[8],hdr[9],hdr[10],hdr[11]]);
        if len == 0 { break; }
        let mut bufm = pool.get();
        // Safety: buffer is at least pool's max packet size
        let dst = unsafe {
            let s = bufm.chunk_mut();
            std::slice::from_raw_parts_mut(s.as_mut_ptr() as *mut u8, s.len())
        };
        if len > dst.len() { anyhow::bail!("replay packet too large: {}", len); }
        let mut read_so_far = 0usize;
        while read_so_far < len {
            let n = stream.read(&mut dst[read_so_far..len])?;
            if n == 0 { anyhow::bail!("unexpected EOF from replay server"); }
            read_so_far += n;
        }
        unsafe { bufm.advance_mut(len); }
        let pkt = Pkt { buf: bufm, len, seq, ts_nanos: crate::util::now_nanos(), chan: b'R' };
        if let Err(_) = q_merged.push(pkt) {
            log::warn!("replay injection dropped due to full queue (seq={seq})");
        } else {
            metrics::inc_decode_pkts();
        }
    }

    Ok(())
}