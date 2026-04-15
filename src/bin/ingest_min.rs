use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use orderbook::config::AppConfig;
use orderbook::metrics;
use orderbook::net;
use orderbook::parser::{build_parser, SeqCfg};
use orderbook::pool::{PacketPool, Pkt};
use orderbook::rx_udp;
use orderbook::spsc::SpscQueue;
use orderbook::util::{
    lock_all_memory_if, now_nanos, pin_to_core_if_set, set_realtime_priority_if, BarrierFlag,
};
use orderbook::{merge, util};

fn main() -> anyhow::Result<()> {
    // Load config
    let cfg_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let cfg = AppConfig::from_file(&cfg_path)?;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    lock_all_memory_if(cfg.general.mlock_all)?;

    // Metrics server
    let _metrics_handle = cfg
        .metrics
        .as_ref()
        .map(|m| metrics::spawn_http(m.bind.clone(), None));

    let shutdown = Arc::new(BarrierFlag::default());
    {
        let s = shutdown.clone();
        ctrlc::set_handler(move || {
            s.raise();
        })
        .ok();
    }

    // Packet pool
    let pool = Arc::new(PacketPool::new(
        cfg.general.pool_size,
        cfg.general.max_packet_size as usize,
    )?);

    // Queues
    let a_workers = cfg.channels.a.workers.unwrap_or(1).max(1);
    let b_workers = cfg.channels.b.workers.unwrap_or(1).max(1);
    let q_rx_a_list: Vec<Arc<SpscQueue<Pkt>>> = (0..a_workers)
        .map(|_| Arc::new(SpscQueue::new(cfg.general.rx_queue_capacity)))
        .collect();
    let q_rx_b_list: Vec<Arc<SpscQueue<Pkt>>> = (0..b_workers)
        .map(|_| Arc::new(SpscQueue::new(cfg.general.rx_queue_capacity)))
        .collect();
    let q_merged = Arc::new(SpscQueue::new(cfg.general.merge_queue_capacity));

    // Parser (sequence only)
    let seq_cfg = SeqCfg {
        offset: cfg.sequence.offset,
        length: cfg.sequence.length,
        endian: cfg.sequence.endian.clone(),
    };
    let parser = build_parser(
        cfg.parser.kind.clone(),
        seq_cfg,
        cfg.parser.max_messages_per_packet,
    )?;
    let _ = parser.max_messages_per_packet;

    // Sockets per worker
    let mut socks_a = Vec::with_capacity(a_workers);
    let mut socks_b = Vec::with_capacity(b_workers);
    for _ in 0..a_workers {
        socks_a.push(net::build_mcast_socket(&cfg.channels.a)?);
    }
    for _ in 0..b_workers {
        socks_b.push(net::build_mcast_socket(&cfg.channels.b)?);
    }

    // RX A threads
    let mut rx_joins = Vec::new();
    for (i, sa) in socks_a.into_iter().enumerate() {
        let rx_shutdown = shutdown.clone();
        let pool_i = pool.clone();
        let q_i = q_rx_a_list[i].clone();
        let parser_i = parser.clone();
        let cfgc = cfg.clone();
        let t = thread::Builder::new()
            .name(format!("rx-A-{i}"))
            .spawn(move || {
                util::pin_to_core_with_offset(cfgc.cpu.a_rx_core, i);
                set_realtime_priority_if(cfgc.cpu.rt_priority);
                let _ = rx_udp::rx_udp_loop(
                    "A",
                    &sa,
                    parser_i.seq_extractor(),
                    q_i,
                    pool_i,
                    rx_shutdown,
                    rx_udp::UdpRxConfig {
                        spin_loops_per_yield: cfgc.general.spin_loops_per_yield,
                        rx_batch: cfgc.general.rx_recvmmsg_batch.unwrap_or(0),
                        ts_mode: cfgc.channels.a.timestamping.clone(),
                    },
                );
            })?;
        rx_joins.push(t);
    }
    // RX B threads
    for (i, sb) in socks_b.into_iter().enumerate() {
        let rx_shutdown = shutdown.clone();
        let pool_i = pool.clone();
        let q_i = q_rx_b_list[i].clone();
        let parser_i = parser.clone();
        let cfgc = cfg.clone();
        let t = thread::Builder::new()
            .name(format!("rx-B-{i}"))
            .spawn(move || {
                util::pin_to_core_with_offset(cfgc.cpu.b_rx_core, i);
                set_realtime_priority_if(cfgc.cpu.rt_priority);
                let _ = rx_udp::rx_udp_loop(
                    "B",
                    &sb,
                    parser_i.seq_extractor(),
                    q_i,
                    pool_i,
                    rx_shutdown,
                    rx_udp::UdpRxConfig {
                        spin_loops_per_yield: cfgc.general.spin_loops_per_yield,
                        rx_batch: cfgc.general.rx_recvmmsg_batch.unwrap_or(0),
                        ts_mode: cfgc.channels.b.timestamping.clone(),
                    },
                );
            })?;
        rx_joins.push(t);
    }

    // Merge thread
    let q_merged_for_merge = q_merged.clone();
    let merge_shutdown = shutdown.clone();
    let merge_cfg = merge::MergeConfig {
        next_seq: cfg.merge.initial_expected_seq,
        reorder_window: cfg.merge.reorder_window,
        max_pending: cfg.merge.max_pending_packets,
        dwell_ns: cfg.merge.dwell_ns.unwrap_or(2_000_000),
        adaptive: cfg.merge.adaptive,
        reorder_window_max: cfg.merge.reorder_window_max.unwrap_or(
            cfg.merge
                .reorder_window
                .saturating_mul(8)
                .max(cfg.merge.reorder_window + 1),
        ),
    };
    let t_merge = thread::Builder::new().name("merge".into()).spawn(move || {
        pin_to_core_if_set(cfg.cpu.merge_core);
        set_realtime_priority_if(cfg.cpu.rt_priority);
        let _ = merge::merge_loop(
            q_rx_a_list,
            q_rx_b_list,
            q_merged_for_merge,
            merge_cfg,
            merge_shutdown,
            None,
            None,
        );
    })?;

    // Minimal decode/sink loop: update stage/e2e metrics and recycle packets
    let t_decode = thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            pin_to_core_if_set(cfg.cpu.decode_core);
            set_realtime_priority_if(cfg.cpu.rt_priority);
            let mut idle = 0u32;
            while !shutdown.is_raised() {
                if let Some(pkt) = q_merged.pop() {
                    metrics::inc_decode_pkts();
                    let now = now_nanos();
                    if pkt.merge_emit_ns > 0 && now > pkt.merge_emit_ns {
                        metrics::observe_stage_merge_to_decode_ns(now - pkt.merge_emit_ns);
                        if pkt.merge_emit_ns > pkt.ts_nanos {
                            metrics::observe_stage_rx_to_merge_ns(pkt.merge_emit_ns - pkt.ts_nanos);
                        }
                    }
                    if pkt.ts_nanos != 0 && now > pkt.ts_nanos {
                        metrics::observe_latency_ns(now - pkt.ts_nanos);
                        metrics::observe_latency_by_kind_ns(pkt._ts_kind, now - pkt.ts_nanos);
                    }
                    pkt.recycle(&pool);
                    idle = 0;
                } else {
                    util::adaptive_wait(&mut idle, 64);
                }
            }
        })?;

    // Join
    for (idx, j) in rx_joins.into_iter().enumerate() {
        if j.join().is_err() {
            log::error!("rx worker {idx} panicked");
        }
    }
    if t_merge.join().is_err() {
        log::error!("merge thread panicked");
    }
    if t_decode.join().is_err() {
        log::error!("decode thread panicked");
    }
    Ok(())
}
