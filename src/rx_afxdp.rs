// src/rx_afxdp.rs
// Optional AF_XDP receiver stub. Integrate by spawning this loop instead of `rx::rx_loop`. Keeps the same Pkt
// contract and queueing model.


use crate::metrics;
use crate::pool::{PacketPool, Pkt};
use crate::util::{now_nanos, BarrierFlag, spin_wait};
use crossbeam::queue::ArrayQueue;
use std::sync::Arc;

/// Receive loop using AF_XDP: Linux-only. On non-Linux, return a clear error.
#[cfg(not(target_os = "linux"))]
pub fn afxdp_loop(
    _ifname: &str,
    _queue_id: u32,
    _q_out: Arc<ArrayQueue<Pkt>>,
    _pool: Arc<PacketPool>,
    _shutdown: Arc<BarrierFlag>,
) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("AF_XDP is only supported on Linux"))
}

/// Linux stub: placeholder until full UMEM/ring wiring is implemented.
#[cfg(target_os = "linux")]
pub fn afxdp_loop(
    _ifname: &str,
    _queue_id: u32,
    _q_out: Arc<ArrayQueue<Pkt>>,
    _pool: Arc<PacketPool>,
    _shutdown: Arc<BarrierFlag>,
) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("AF_XDP path not implemented yet: disable [afxdp] or use socket RX"))
}


