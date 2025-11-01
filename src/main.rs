// src/main.rs (updated: integrate metrics, snapshot, recovery)
mod config;
mod decode;
mod merge;
mod metrics;
mod net;
mod orderbook;
mod parser;
mod decoder_itch;
mod decoder_eobi;
mod pool;
mod recovery;
mod rx;
mod snapshot;
mod util;

use crate::config::AppConfig;
use crate::decode::decode_loop;
use crate::merge::merge_loop;
use crate::parser::{build_parser, SeqCfg};
use crate::pool::PacketPool;
use crate::rx::rx_loop;
use crate::util::{pin_to_core_if_set, BarrierFlag, lock_all_memory_if, set_realtime_priority_if};
use crossbeam::queue::ArrayQueue;
use log::{error, info};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cfg_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    let cfg = AppConfig::from_file(&cfg_path)?;
    info!("loaded config: {:?}", cfg);

    let shutdown = Arc::new(BarrierFlag::default());
    {
        let s = shutdown.clone();
        ctrlc::set_handler(move || {
            s.raise();
        })?;
    }

    // Lock memory (optional) before spinning up threads
    lock_all_memory_if(cfg.general.mlock_all);

    // Metrics HTTP
    let _metrics_handle = if let Some(m) = &cfg.metrics {
        Some(metrics::spawn_http(m.bind.clone()))
    } else { None };

    // Global packet pool
    let pool = Arc::new(PacketPool::new(
        cfg.general.pool_size,
        cfg.general.max_packet_size as usize,
    )?);

    // Queues
    let q_rx_a = Arc::new(ArrayQueue::new(cfg.general.rx_queue_capacity));
    let q_rx_b = Arc::new(ArrayQueue::new(cfg.general.rx_queue_capacity));
    let q_merged = Arc::new(ArrayQueue::new(cfg.general.merge_queue_capacity));

    // Parser & Sequence
    let seq_cfg = SeqCfg {
        offset: cfg.sequence.offset,
        length: cfg.sequence.length,
        endian: cfg.sequence.endian.clone(),
    };
    let parser = build_parser(cfg.parser.kind.clone(), seq_cfg, cfg.parser.max_messages_per_packet)?;

    // Sockets
    let sock_a = net::build_mcast_socket(&cfg.channels.a)?;
    let sock_b = net::build_mcast_socket(&cfg.channels.b)?;

    // Snapshot manager
    let (snapshot_tx, snapshot_handle) = if let Some(snap) = &cfg.snapshot {
        if snap.enable_writer {
            let (tx, handle) = snapshot::SnapshotWriter::spawn(PathBuf::from(&snap.path));
            (Some(tx), Some(handle))
        } else { (None, None) }
    } else { (None, None) };

    // Try loading snapshot
    let initial_book = if let Some(snap) = &cfg.snapshot {
        if snap.load_on_start {
            match snapshot::load(PathBuf::from(&snap.path).as_path()) {
                Ok(book) => {
                    info!("Loaded snapshot from {}", snap.path);
                    Some(book)
                }
                Err(e) => {
                    error!("Failed to load snapshot {}: {e:?}", snap.path);
                    None
                }
            }
        } else { None }
    } else { None };

    // Recovery manager: either TCP injector or logger-only
    let q_merged_for_recovery = q_merged.clone();
    let pool_for_recovery = pool.clone();
    let (recovery_client, recovery_handle) = if let Some(rc) = &cfg.recovery {
        if rc.enable_injector {
            info!("recovery injector enabled -> {}", rc.endpoint);
            recovery::spawn_tcp_injector(rc.endpoint.clone(), q_merged_for_recovery, pool_for_recovery)
        } else {
            recovery::spawn_logger()
        }
    } else {
        recovery::spawn_logger()
    };

    // RX threads
    let rx_a_shutdown = shutdown.clone();
    let rx_b_shutdown = shutdown.clone();
    let pool_a = pool.clone();
    let pool_b = pool.clone();
    let q_a = q_rx_a.clone();
    let q_b = q_rx_b.clone();
    let parser_a = parser.clone();
    let parser_b = parser.clone();

    let t_rx_a = thread::Builder::new().name("rx-A".into()).spawn(move || {
        pin_to_core_if_set(cfg.cpu.a_rx_core);
        set_realtime_priority_if(cfg.cpu.rt_priority);
        if let Err(e) = rx_loop(
            "A",
            &sock_a,
            parser_a.seq_extractor(),
            q_a,
            pool_a,
            rx_a_shutdown,
            cfg.general.spin_loops_per_yield,
            cfg.general.rx_recvmmsg_batch.unwrap_or(0),
            cfg.channels.a.timestamping.as_ref().map(|m| !matches!(m, crate::config::TimestampingMode::Off)).unwrap_or(false),
        ) {
            error!("rx-A failed: {e:?}");
        }
    })?;

    let t_rx_b = thread::Builder::new().name("rx-B".into()).spawn(move || {
        pin_to_core_if_set(cfg.cpu.b_rx_core);
        set_realtime_priority_if(cfg.cpu.rt_priority);
        if let Err(e) = rx_loop(
            "B",
            &sock_b,
            parser_b.seq_extractor(),
            q_b,
            pool_b,
            rx_b_shutdown,
            cfg.general.spin_loops_per_yield,
            cfg.general.rx_recvmmsg_batch.unwrap_or(0),
            cfg.channels.b.timestamping.as_ref().map(|m| !matches!(m, crate::config::TimestampingMode::Off)).unwrap_or(false),
        ) {
            error!("rx-B failed: {e:?}");
        }
    })?;

    // Merge thread
    let merge_shutdown = shutdown.clone();
    let parser_m = parser.clone();
    let recovery_cli = recovery_client.clone();
    let t_merge = thread::Builder::new().name("merge".into()).spawn(move || {
        pin_to_core_if_set(cfg.cpu.merge_core);
        set_realtime_priority_if(cfg.cpu.rt_priority);
        if let Err(e) = merge_loop(
            q_rx_a,
            q_rx_b,
            q_merged,
            parser_m.seq_extractor(),
            cfg.merge.initial_expected_seq,
            cfg.merge.reorder_window,
            cfg.merge.max_pending_packets,
            merge_shutdown,
            Some(recovery_cli),
        ) {
            error!("merge failed: {e:?}");
        }
    })?;

    // Decode thread
    let decode_shutdown = shutdown.clone();
    let t_decode = thread::Builder::new().name("decode".into()).spawn(move || {
        pin_to_core_if_set(cfg.cpu.decode_core);
        set_realtime_priority_if(cfg.cpu.rt_priority);
        if let Err(e) = decode_loop(
            q_merged,
            pool,
            parser,
            cfg.book.max_depth,
            cfg.book.snapshot_interval_ms,
            cfg.book.consume_trades,
            decode_shutdown,
            snapshot_tx,
            initial_book,
        ) {
            error!("decode failed: {e:?}");
        }
    })?;

    // Join (log panics explicitly to aid diagnosis in production)
    if t_rx_a.join().is_err() { error!("rx-A thread panicked"); }
    if t_rx_b.join().is_err() { error!("rx-B thread panicked"); }
    if t_merge.join().is_err() { error!("merge thread panicked"); }
    if t_decode.join().is_err() { error!("decode thread panicked"); }
    if let Some(h) = snapshot_handle { h.join(); }
    recovery_handle.join();
    info!("clean shutdown");
    Ok(())
}