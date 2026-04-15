use crossbeam_channel::{bounded, Receiver, Sender};
use log::{error, info};
use orderbook::config::AppConfig;
use orderbook::decode::decode_loop;
use orderbook::merge::merge_loop;
use orderbook::parser::{build_parser, SeqCfg};
use orderbook::pool::PacketPool;
use orderbook::rx::rx_loop;
use orderbook::util::{
    lock_all_memory_if, pin_to_core_if_set, set_realtime_priority_if, BarrierFlag,
};
use orderbook::{metrics, net, pubsub, recovery, refdata, rx_packet_mmap, snapshot, ws_server};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

fn main() -> anyhow::Result<()> {
    let cfg_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    // Load config before logger to allow JSON formatting choice
    let cfg = AppConfig::from_file(&cfg_path)?;

    if cfg.general.json_logs {
        let mut b =
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
        b.format(|buf, record| {
            use std::io::Write;
            let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            writeln!(
                buf,
                "{{\"ts\":\"{}\",\"level\":\"{}\",\"target\":\"{}\",\"msg\":\"{}\"}}",
                ts,
                record.level(),
                record.target(),
                record.args().to_string().replace('"', "'")
            )
        })
        .init();
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
    lock_all_memory_if(cfg.general.mlock_all)?;

    // NUMA advisories for PACKET_MMAP path: ensure threads/cores align to NIC node
    if let Some(pm) = &cfg.packet_mmap {
        if pm.enable {
            let node = orderbook::util::iface_numa_node(&pm.ifname);
            if let Some(n) = node {
                if let Some(list) = orderbook::util::node_cpulist(n) {
                    let check = |name: &str, core: Option<usize>| {
                        if let Some(c) = core {
                            if !orderbook::util::cpulist_contains(&list, c) {
                                log::warn!(
                                    "NUMA: core {} for {} not in NIC node {} cpulist {}",
                                    c,
                                    name,
                                    n,
                                    list
                                );
                            }
                        }
                    };
                    check("a_rx_core", cfg.cpu.a_rx_core);
                    check("b_rx_core", cfg.cpu.b_rx_core);
                    check("merge_core", cfg.cpu.merge_core);
                    check("decode_core", cfg.cpu.decode_core);
                }
            } else {
                log::warn!("NUMA: could not read NUMA node for iface {}", pm.ifname);
            }
        }
    }

    // Snapshot trigger channel (for HTTP /snapshot) and Metrics HTTP
    let (snaptr_tx, snaptr_rx): (Sender<()>, Receiver<()>) = bounded(8);
    let metrics_handle = cfg
        .metrics
        .as_ref()
        .map(|m| metrics::spawn_http(m.bind.clone(), Some(snaptr_tx.clone())));

    // Global packet pool
    let pool = Arc::new(PacketPool::new(
        cfg.general.pool_size,
        cfg.general.max_packet_size as usize,
    )?);

    // Queues
    let packet_mmap_queues = cfg
        .packet_mmap
        .as_ref()
        .filter(|c| c.enable)
        .and_then(|c| c.queues)
        .unwrap_or(1)
        .max(1);
    let a_workers = if cfg.packet_mmap.as_ref().map(|c| c.enable).unwrap_or(false) {
        packet_mmap_queues
    } else {
        cfg.channels.a.workers.unwrap_or(1).max(1)
    };
    let b_workers = cfg.channels.b.workers.unwrap_or(1).max(1);
    let mut q_rx_a_list: Vec<Arc<orderbook::spsc::SpscQueue<orderbook::pool::Pkt>>> =
        Vec::with_capacity(a_workers);
    let mut q_rx_b_list: Vec<Arc<orderbook::spsc::SpscQueue<orderbook::pool::Pkt>>> =
        Vec::with_capacity(b_workers);
    for _ in 0..a_workers {
        q_rx_a_list.push(Arc::new(orderbook::spsc::SpscQueue::new(
            cfg.general.rx_queue_capacity,
        )));
    }
    for _ in 0..b_workers {
        q_rx_b_list.push(Arc::new(orderbook::spsc::SpscQueue::new(
            cfg.general.rx_queue_capacity,
        )));
    }
    let q_merged = Arc::new(orderbook::spsc::SpscQueue::new(
        cfg.general.merge_queue_capacity,
    ));

    // Parser & Sequence
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

    let mut instrument_ticks: Vec<(u32, i64)> =
        if let Some(path) = cfg.book.instrument_ticks_path.as_ref() {
            let ticks = refdata::load_instrument_ticks(PathBuf::from(path).as_path())?;
            info!("loaded {} instrument ticks from {}", ticks.len(), path);
            ticks
                .into_iter()
                .map(|tick| (tick.instr, tick.tick))
                .collect()
        } else {
            Vec::new()
        };
    instrument_ticks.extend(
        cfg.book
            .instrument_ticks
            .iter()
            .map(|tick| (tick.instr, tick.tick)),
    );

    // Sockets (support multi-worker via SO_REUSEPORT)
    let mut socks_a = Vec::with_capacity(a_workers);
    let mut socks_b = Vec::with_capacity(b_workers);
    if !cfg.packet_mmap.as_ref().map(|c| c.enable).unwrap_or(false) {
        for _ in 0..a_workers {
            socks_a.push(net::build_mcast_socket(&cfg.channels.a)?);
        }
    }
    for _ in 0..b_workers {
        socks_b.push(net::build_mcast_socket(&cfg.channels.b)?);
    }

    // Snapshot manager
    let (snapshot_tx, snapshot_handle) = if let Some(snap) = &cfg.snapshot {
        if snap.enable_writer {
            let (tx, handle) = snapshot::SnapshotWriter::spawn(PathBuf::from(&snap.path));
            (Some(tx), Some(handle))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

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
        } else {
            None
        }
    } else {
        None
    };

    // Recovery manager: TCP injector if enabled, else logger-only
    let (recovery_client, recovery_handle, q_recovery_opt): (
        recovery::RecoveryClient,
        recovery::RecoveryHandle,
        Option<Arc<orderbook::spsc::SpscQueue<orderbook::pool::Pkt>>>,
    ) = if let Some(rcfg) = &cfg.recovery {
        if rcfg.enable_injector {
            let q_recovery = Arc::new(orderbook::spsc::SpscQueue::new(
                cfg.general.merge_queue_capacity,
            ));
            let (cli, handle) = recovery::spawn_tcp_injector(
                rcfg.endpoint.clone(),
                q_recovery.clone(),
                pool.clone(),
                rcfg.backlog_path.clone(),
                recovery::RecoveryOptions {
                    retry_attempts: rcfg.retry_attempts.unwrap_or(3),
                    retry_backoff_ms: rcfg.retry_backoff_ms.unwrap_or(10),
                    min_request_interval_ms: rcfg.min_request_interval_ms.unwrap_or(0),
                    slo_ms: rcfg.slo_ms.unwrap_or(100),
                    unrecoverable_policy: rcfg.unrecoverable_policy.unwrap_or_default(),
                    request_timeout_ms: rcfg.request_timeout_ms.unwrap_or(250),
                    replay_protocol: rcfg.replay_protocol.unwrap_or_default(),
                },
            );
            (cli, handle, Some(q_recovery))
        } else {
            let (cli, handle) = recovery::spawn_logger();
            (cli, handle, None)
        }
    } else {
        let (cli, handle) = recovery::spawn_logger();
        (cli, handle, None)
    };

    // RX threads

    let t_rx_a = if cfg.packet_mmap.as_ref().map(|c| c.enable).unwrap_or(false) {
        // Spawn one PACKET_RX_RING worker per requested queue.
        let ifname = cfg.packet_mmap.as_ref().unwrap().ifname.clone();
        let queues = packet_mmap_queues;
        let mut joins = Vec::with_capacity(queues);
        for (i, q_ai) in q_rx_a_list.iter().take(queues).enumerate() {
            let rx_a_shutdown_i = shutdown.clone();
            let pool_ai = pool.clone();
            let q_ai = q_ai.clone();
            let parser_ai = parser.clone();
            let cfg = cfg.clone();
            let ifn = ifname.clone();
            let pm = cfg.packet_mmap.as_ref().unwrap();
            let opts = rx_packet_mmap::PacketMmapOptions {
                queue_id: i as u32,
                frame_size: pm.frame_size,
                frames_per_block: pm.frames_per_block,
                block_count: pm.block_count,
            };
            let name = format!("packet-mmap-A-{i}");
            let t = thread::Builder::new().name(name).spawn(move || {
                orderbook::util::pin_to_core_with_offset(cfg.cpu.a_rx_core, i);
                set_realtime_priority_if(cfg.cpu.rt_priority);
                if let Err(e) = rx_packet_mmap::packet_mmap_loop(
                    &ifn,
                    opts,
                    parser_ai.seq_extractor(),
                    "A",
                    q_ai,
                    pool_ai,
                    rx_a_shutdown_i,
                ) {
                    error!("packet_mmap failed: {e:?}");
                }
            })?;
            joins.push(t);
        }
        thread::Builder::new()
            .name("rx-A-join".into())
            .spawn(move || {
                for (idx, j) in joins.into_iter().enumerate() {
                    if j.join().is_err() {
                        error!("packet-mmap-A worker {idx} panicked");
                    }
                }
            })?
    } else {
        // Spawn N workers for A
        let mut joins = Vec::with_capacity(a_workers);
        for (i, sa) in socks_a.into_iter().enumerate() {
            let rx_a_shutdown_i = shutdown.clone();
            let pool_ai = pool.clone();
            let q_ai = q_rx_a_list[i].clone();
            let parser_ai = parser.clone();
            let cfg = cfg.clone();
            let name = format!("rx-A-{i}");
            let t = thread::Builder::new().name(name).spawn(move || {
                orderbook::util::pin_to_core_with_offset(cfg.cpu.a_rx_core, i);
                set_realtime_priority_if(cfg.cpu.rt_priority);
                if let Err(e) = rx_loop(
                    "A",
                    &sa,
                    parser_ai.seq_extractor(),
                    q_ai,
                    pool_ai,
                    rx_a_shutdown_i,
                    orderbook::rx::RxConfig {
                        spin_loops_per_yield: cfg.general.spin_loops_per_yield,
                        rx_batch: cfg.general.rx_recvmmsg_batch.unwrap_or(0),
                        ts_mode: cfg.channels.a.timestamping.clone(),
                    },
                ) {
                    error!("rx-A failed: {e:?}");
                }
            })?;
            joins.push(t);
        }
        // Join all A workers using a proxy handle
        thread::Builder::new()
            .name("rx-A-join".into())
            .spawn(move || {
                for (idx, j) in joins.into_iter().enumerate() {
                    if j.join().is_err() {
                        error!("rx-A worker {idx} panicked");
                    }
                }
            })?
    };

    let t_rx_b = {
        let mut joins = Vec::with_capacity(b_workers);
        for (i, sb) in socks_b.into_iter().enumerate() {
            let rx_b_shutdown_i = shutdown.clone();
            let pool_bi = pool.clone();
            let q_bi = q_rx_b_list[i].clone();
            let parser_bi = parser.clone();
            let cfg = cfg.clone();
            let name = format!("rx-B-{i}");
            let t = thread::Builder::new().name(name).spawn(move || {
                orderbook::util::pin_to_core_with_offset(cfg.cpu.b_rx_core, i);
                set_realtime_priority_if(cfg.cpu.rt_priority);
                if let Err(e) = rx_loop(
                    "B",
                    &sb,
                    parser_bi.seq_extractor(),
                    q_bi,
                    pool_bi,
                    rx_b_shutdown_i,
                    orderbook::rx::RxConfig {
                        spin_loops_per_yield: cfg.general.spin_loops_per_yield,
                        rx_batch: cfg.general.rx_recvmmsg_batch.unwrap_or(0),
                        ts_mode: cfg.channels.b.timestamping.clone(),
                    },
                ) {
                    error!("rx-B failed: {e:?}");
                }
            })?;
            joins.push(t);
        }
        thread::Builder::new()
            .name("rx-B-join".into())
            .spawn(move || {
                for (idx, j) in joins.into_iter().enumerate() {
                    if j.join().is_err() {
                        error!("rx-B worker {idx} panicked");
                    }
                }
            })?
    };

    // Merge thread
    let merge_shutdown = shutdown.clone();
    let recovery_cli = recovery_client.clone();
    let q_merged_for_merge = q_merged.clone();
    let t_merge = thread::Builder::new().name("merge".into()).spawn(move || {
        pin_to_core_if_set(cfg.cpu.merge_core);
        set_realtime_priority_if(cfg.cpu.rt_priority);
        if let Err(e) = merge_loop(
            q_rx_a_list,
            q_rx_b_list,
            q_merged_for_merge,
            orderbook::merge::MergeConfig {
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
            },
            merge_shutdown,
            Some(recovery_cli),
            q_recovery_opt,
        ) {
            error!("merge failed: {e:?}");
        }
    })?;

    // Decode thread
    let decode_shutdown = shutdown.clone();
    // Feeds / Publishers setup (WS A/B)
    let feeds_cfg = cfg.feeds.clone();
    let obo_enabled = feeds_cfg
        .as_ref()
        .and_then(|f| f.obo.as_ref())
        .map(|o| o.enabled)
        .unwrap_or(false);
    let pub_queue = feeds_cfg
        .as_ref()
        .and_then(|f| f.obo.as_ref())
        .and_then(|o| o.buffers.as_ref())
        .map(|b| b.pub_queue)
        .unwrap_or(65536);
    let client_write_timeout_ms = feeds_cfg
        .as_ref()
        .and_then(|f| f.obo.as_ref())
        .map(|o| o.client_write_timeout_ms)
        .unwrap_or(250);
    let client_handshake_timeout_ms = feeds_cfg
        .as_ref()
        .and_then(|f| f.obo.as_ref())
        .map(|o| o.client_handshake_timeout_ms)
        .unwrap_or(1_000);
    let client_heartbeat_interval_ms = feeds_cfg
        .as_ref()
        .and_then(|f| f.obo.as_ref())
        .map(|o| o.client_heartbeat_interval_ms)
        .unwrap_or(1_000);
    let client_max_connections = feeds_cfg
        .as_ref()
        .and_then(|f| f.obo.as_ref())
        .map(|o| o.client_max_connections)
        .unwrap_or(1024);
    let client_nodelay = feeds_cfg
        .as_ref()
        .and_then(|f| f.obo.as_ref())
        .map(|o| o.client_nodelay)
        .unwrap_or(true);
    let obo_bus = if obo_enabled {
        Some(pubsub::Bus::new(pub_queue))
    } else {
        None
    };

    // Spawn WS endpoints per POP if configured
    let ws_handles: Vec<(std::thread::JoinHandle<()>, std::thread::JoinHandle<()>)> =
        if let Some(feeds) = &feeds_cfg {
            if feeds.enabled {
                let mut hs = Vec::new();
                for pop in &feeds.pops {
                    if pop.ws_endpoints.len() >= 2 {
                        if let Some(bus) = &obo_bus {
                            let snap_path = cfg.snapshot.as_ref().map(|s| s.path.clone());
                            let h = ws_server::spawn_pair(
                                bus.clone(),
                                pop.ws_endpoints[0].clone(),
                                pop.ws_endpoints[1].clone(),
                                ws_server::WsServerConfig {
                                    snapshot_path: snap_path,
                                    auth_token: feeds.auth_token.clone(),
                                    client_write_timeout_ms,
                                    client_handshake_timeout_ms,
                                    client_heartbeat_interval_ms,
                                    client_max_connections,
                                    client_nodelay,
                                },
                            );
                            hs.push(h);
                        }
                    }
                }
                hs
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

    let obo_pub_for_decode = obo_bus.as_ref().map(|b| b.publisher());

    let t_decode = thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            pin_to_core_if_set(cfg.cpu.decode_core);
            set_realtime_priority_if(cfg.cpu.rt_priority);
            if let Err(e) = decode_loop(
                q_merged,
                pool,
                parser,
                decode_shutdown,
                orderbook::decode::DecodeConfig {
                    max_depth: cfg.book.max_depth,
                    snapshot_interval_ms: cfg.book.snapshot_interval_ms,
                    consume_trades: cfg.book.consume_trades,
                    default_slab_capacity: cfg.book.order_slab_capacity,
                    default_tick: cfg.book.default_tick,
                    grid_span: cfg.book.grid_span,
                    instrument_ticks,
                    snapshot_tx,
                    initial_book,
                    snapshot_trigger_rx: Some(snaptr_rx),
                    obo_publisher: obo_pub_for_decode,
                    journal_path: cfg
                        .journal
                        .as_ref()
                        .filter(|j| j.enable_writer)
                        .map(|j| j.path.clone()),
                    journal_record_state_hash: cfg
                        .journal
                        .as_ref()
                        .map(|j| j.record_state_hash)
                        .unwrap_or(true),
                },
            ) {
                error!("decode failed: {e:?}");
            }
        })?;

    // Join (log panics explicitly to aid diagnosis in production)
    if t_rx_a.join().is_err() {
        error!("rx-A thread panicked");
    }
    if t_rx_b.join().is_err() {
        error!("rx-B thread panicked");
    }
    if t_merge.join().is_err() {
        error!("merge thread panicked");
    }
    if t_decode.join().is_err() {
        error!("decode thread panicked");
    }
    // WS handles
    for (idx, (a, b)) in ws_handles.into_iter().enumerate() {
        if a.join().is_err() {
            error!("ws endpoint pair {idx} first thread panicked");
        }
        if b.join().is_err() {
            error!("ws endpoint pair {idx} second thread panicked");
        }
    }
    if let Some(h) = snapshot_handle {
        h.join();
    }
    recovery_handle.join();
    // Gracefully stop metrics HTTP (poke /shutdown and join)
    if let Some(m) = &cfg.metrics {
        request_http_shutdown(&m.bind);
    }
    if let Some(h) = metrics_handle {
        if h.join().is_err() {
            error!("metrics thread panicked");
        }
    }
    info!("clean shutdown");
    Ok(())
}

fn request_http_shutdown(addr: &str) {
    use std::io::Write;
    if let Ok(mut s) = std::net::TcpStream::connect(addr) {
        let _ =
            s.write_all(b"GET /shutdown HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        let _ = s.flush();
    }
}
