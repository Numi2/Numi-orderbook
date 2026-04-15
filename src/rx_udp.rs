use crate::config::TimestampingMode;
use crate::parser::SeqExtractor;
use crate::pool::{PacketPool, Pkt};
use crate::spsc::SpscQueue;
use crate::util::BarrierFlag;
use std::net::UdpSocket;
use std::sync::Arc;

pub struct UdpRxConfig {
    pub spin_loops_per_yield: u32,
    pub rx_batch: usize,
    pub ts_mode: Option<TimestampingMode>,
}

#[allow(clippy::too_many_arguments)]
pub fn rx_udp_loop(
    chan_name: &str,
    sock: &UdpSocket,
    seq: Arc<dyn SeqExtractor>,
    q_out: Arc<SpscQueue<Pkt>>,
    pool: Arc<PacketPool>,
    shutdown: Arc<BarrierFlag>,
    cfg: UdpRxConfig,
) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        crate::rx_darwin_udp::rx_darwin_udp_loop(
            chan_name,
            sock,
            seq,
            q_out,
            pool,
            shutdown,
            crate::rx_darwin_udp::DarwinUdpRxConfig {
                spin_loops_per_yield: cfg.spin_loops_per_yield,
                rx_batch: cfg.rx_batch,
                ts_mode: cfg.ts_mode,
            },
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        crate::rx::rx_loop(
            chan_name,
            sock,
            seq,
            q_out,
            pool,
            shutdown,
            crate::rx::RxConfig {
                spin_loops_per_yield: cfg.spin_loops_per_yield,
                rx_batch: cfg.rx_batch,
                ts_mode: cfg.ts_mode,
            },
        )
    }
}
