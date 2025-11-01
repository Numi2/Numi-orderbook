// src/main.rs (updated: integrate metrics, snapshot, recovery)
mod config;
mod decode;
mod merge;
mod metrics;
mod net;
mod orderbook;
mod parser;
mod decoder_eobi;
mod decoder_itch;
mod decoder_fast;
mod rx_afxdp;
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
use crossbeam_channel::{bounded, Sender, Receiver};

fn main() -> anyhow::Result<()> {
    let cfg_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    // Load config before logger to allow JSON formatting choice
    let cfg = AppConfig::from_file(&cfg_path)?;

    if cfg.general.json_logs {
        let mut b = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
        b.format(|buf, record| {
            use std::io::Write;
            let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            writeln!(buf, "{{\"ts\":\"{}\",\"level\":\"{}\",\"target\":\"{}\",\"msg\":\"{}\"}}",
                ts, record.level(), record.target(), record.args().to_string().replace('"', "'"))
        }).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

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

    // Snapshot trigger channel (for HTTP /snapshot) and Metrics HTTP
    let (snaptr_tx, snaptr_rx): (Sender<()>, Receiver<()>) = bounded(8);
    let metrics_handle = if let Some(m) = &cfg.metrics {
        Some(metrics::spawn_http(m.bind.clone(), Some(snaptr_tx.clone())))
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

    // Sockets (support multi-worker via SO_REUSEPORT)
    let a_workers = cfg.channels.a.workers.unwrap_or(1).max(1);
    let b_workers = cfg.channels.b.workers.unwrap_or(1).max(1);
    let mut socks_a = Vec::with_capacity(a_workers);
    let mut socks_b = Vec::with_capacity(b_workers);
    for _ in 0..a_workers { socks_a.push(net::build_mcast_socket(&cfg.channels.a)?); }
    for _ in 0..b_workers { socks_b.push(net::build_mcast_socket(&cfg.channels.b)?); }

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

    // Recovery manager: TCP injector if enabled, else logger-only
    let (recovery_client, recovery_handle) = if let Some(rcfg) = &cfg.recovery {
        if rcfg.enable_injector {
            recovery::spawn_tcp_injector(rcfg.endpoint.clone(), q_merged.clone(), pool.clone(), rcfg.backlog_path.clone())
        } else {
            recovery::spawn_logger()
        }
    } else {
        recovery::spawn_logger()
    };

    // RX threads
    let rx_a_shutdown = shutdown.clone();
    let pool_a = pool.clone();
    let q_a = q_rx_a.clone();

    let t_rx_a = if cfg.afxdp.as_ref().map(|c| c.enable).unwrap_or(false) {
        let ifname = cfg.afxdp.as_ref().unwrap().ifname.clone();
        let qid = cfg.afxdp.as_ref().unwrap().queue_id;
        let seq_a = parser.seq_extractor();
        thread::Builder::new().name("afxdp".into()).spawn(move || {
            pin_to_core_if_set(cfg.cpu.a_rx_core);
            set_realtime_priority_if(cfg.cpu.rt_priority);
            if let Err(e) = rx_afxdp::afxdp_loop(&ifname, qid, seq_a, "A", q_a, pool_a, rx_a_shutdown) {
                error!("afxdp failed: {e:?}");
            }
        })?
    } else {
        // Spawn N workers for A
        let mut joins = Vec::with_capacity(a_workers);
        for (i, sa) in socks_a.into_iter().enumerate() {
            let rx_a_shutdown_i = shutdown.clone();
            let pool_ai = pool.clone();
            let q_ai = q_rx_a.clone();
            let parser_ai = parser.clone();
            let cfg = cfg.clone();
            let name = format!("rx-A-{i}");
            let t = thread::Builder::new().name(name).spawn(move || {
                crate::util::pin_to_core_with_offset(cfg.cpu.a_rx_core, i);
                set_realtime_priority_if(cfg.cpu.rt_priority);
                if let Err(e) = rx_loop(
                    "A",
                    &sa,
                    parser_ai.seq_extractor(),
                    q_ai,
                    pool_ai,
                    rx_a_shutdown_i,
                    cfg.general.spin_loops_per_yield,
                    cfg.general.rx_recvmmsg_batch.unwrap_or(0),
                    cfg.channels.a.timestamping.clone(),
                ) {
                    error!("rx-A failed: {e:?}");
                }
            })?;
            joins.push(t);
        }
        // Join all A workers using a proxy handle
        thread::Builder::new().name("rx-A-join".into()).spawn(move || {
            for j in joins { let _ = j.join(); }
        })?
    };

    let t_rx_b = {
        let mut joins = Vec::with_capacity(b_workers);
        for (i, sb) in socks_b.into_iter().enumerate() {
            let rx_b_shutdown_i = shutdown.clone();
            let pool_bi = pool.clone();
            let q_bi = q_rx_b.clone();
            let parser_bi = parser.clone();
            let cfg = cfg.clone();
            let name = format!("rx-B-{i}");
            let t = thread::Builder::new().name(name).spawn(move || {
                crate::util::pin_to_core_with_offset(cfg.cpu.b_rx_core, i);
                set_realtime_priority_if(cfg.cpu.rt_priority);
                if let Err(e) = rx_loop(
                    "B",
                    &sb,
                    parser_bi.seq_extractor(),
                    q_bi,
                    pool_bi,
                    rx_b_shutdown_i,
                    cfg.general.spin_loops_per_yield,
                    cfg.general.rx_recvmmsg_batch.unwrap_or(0),
                    cfg.channels.b.timestamping.clone(),
                ) {
                    error!("rx-B failed: {e:?}");
                }
            })?;
            joins.push(t);
        }
        thread::Builder::new().name("rx-B-join".into()).spawn(move || { for j in joins { let _ = j.join(); } })?
    };

    // Merge thread
    let merge_shutdown = shutdown.clone();
    let parser_m = parser.clone();
    let recovery_cli = recovery_client.clone();
    let q_merged_for_merge = q_merged.clone();
    let t_merge = thread::Builder::new().name("merge".into()).spawn(move || {
        pin_to_core_if_set(cfg.cpu.merge_core);
        set_realtime_priority_if(cfg.cpu.rt_priority);
        if let Err(e) = merge_loop(
            q_rx_a,
            q_rx_b,
            q_merged_for_merge,
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
            Some(snaptr_rx),
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
    // Gracefully stop metrics HTTP (poke /shutdown and join)
    if let Some(m) = &cfg.metrics {
        request_http_shutdown(&m.bind);
    }
    if let Some(h) = metrics_handle { let _ = h.join(); }
    info!("clean shutdown");
    Ok(())
}

fn request_http_shutdown(addr: &str) {
    use std::io::Write;
    if let Ok(mut s) = std::net::TcpStream::connect(addr) {
        let _ = s.write_all(b"GET /shutdown HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        let _ = s.flush();
    }
}