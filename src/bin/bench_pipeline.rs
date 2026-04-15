use orderbook::bench_support::{
    benchmark_order_book, expected_eobi_replay, format_kv_line, read_capture_payloads,
    stable_config_hash, BenchmarkFixtures, FixtureConfig,
};
use orderbook::codec_raw::{channel_id, msg_type};
use orderbook::config::{AppConfig, Endian, ParserKind, TimestampingMode};
use orderbook::journal::{append_record, replay_after_snapshot, replay_reader, JournalRecord};
use orderbook::obo::{map_event_to_obo_parts, OboEventV1};
use orderbook::orderbook::OrderBook;
use orderbook::parser::{build_parser, Event, Parser, SeqCfg, SeqExtractor};
use orderbook::pool::{PacketPool, Pkt, PktBuf, TsKind};
use orderbook::pubsub::{Bus, Publisher};
use orderbook::recovery::Replayer;
use orderbook::rx_udp::{rx_udp_loop, UdpRxConfig};
use orderbook::spsc::SpscQueue;
use orderbook::util::{adaptive_wait, now_nanos, BarrierFlag};
use orderbook::{merge, net};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use zerocopy::AsBytes;

#[derive(Debug, Clone)]
struct Args {
    profile: String,
    config_path: PathBuf,
    packets: usize,
    duration: Duration,
    capture_path: Option<PathBuf>,
    artifact_dir: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    match args.profile.as_str() {
        "local-core" => run_local_pipeline(&args, "local-core", false, false),
        "local-distribution" => run_local_pipeline(&args, "local-distribution", true, false),
        "rx-proof" | "rx_proof" | "eobi-replay" | "eobi_replay" => {
            run_local_pipeline(&args, "rx-proof", true, true)
        }
        "target-rx" => run_target_rx(&args),
        "target-failover-recovery" => run_target_failover_recovery(&args),
        "target-persistence" => run_target_persistence(&args),
        other => {
            usage();
            anyhow::bail!("unknown bench_pipeline profile {other:?}");
        }
    }
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut args = std::env::args().skip(1);
        let Some(profile) = args.next() else {
            usage();
            anyhow::bail!("missing profile");
        };
        if profile == "-h" || profile == "--help" {
            usage();
            std::process::exit(0);
        }

        let mut parsed = Self {
            profile,
            config_path: PathBuf::from("config.toml"),
            packets: 4096,
            duration: Duration::from_secs(10),
            capture_path: None,
            artifact_dir: None,
        };

        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--config" => {
                    parsed.config_path = args
                        .next()
                        .map(PathBuf::from)
                        .ok_or_else(|| anyhow::anyhow!("--config requires a path"))?;
                }
                "--packets" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--packets requires a value"))?;
                    parsed.packets = value.parse::<usize>()?.max(1);
                }
                "--duration-sec" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--duration-sec requires a value"))?;
                    parsed.duration = Duration::from_secs(value.parse::<u64>()?.max(1));
                }
                "--capture" => {
                    parsed.capture_path = Some(
                        args.next()
                            .map(PathBuf::from)
                            .ok_or_else(|| anyhow::anyhow!("--capture requires a path"))?,
                    );
                }
                "--artifact-dir" => {
                    parsed.artifact_dir = Some(
                        args.next()
                            .map(PathBuf::from)
                            .ok_or_else(|| anyhow::anyhow!("--artifact-dir requires a path"))?,
                    );
                }
                other => anyhow::bail!("unknown argument {other:?}"),
            }
        }

        Ok(parsed)
    }
}

fn usage() {
    eprintln!(
        "usage: bench_pipeline <local-core|local-distribution|rx-proof|target-rx|target-failover-recovery|target-persistence> [--config config.toml] [--packets N] [--duration-sec N] [--capture file.pcap] [--artifact-dir dir]"
    );
}

fn run_local_pipeline(
    args: &Args,
    profile: &'static str,
    distribution: bool,
    proof_mode: bool,
) -> anyhow::Result<()> {
    let cfg = fixture_config(args.packets);
    let payloads = load_payloads(args, cfg)?;
    let parser = default_eobi_parser()?;
    let seq_extractor = parser.seq_extractor();
    let expected = if proof_mode {
        let ordered_payloads = payloads_in_sequence_order(&payloads, seq_extractor.as_ref());
        Some(expected_eobi_replay(cfg, &ordered_payloads))
    } else {
        None
    };
    let packet_count = payloads.len().max(1);
    let max_packet_size = payloads
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(2048)
        .max(2048);
    let pool_size = packet_count.saturating_mul(2).saturating_add(1024);
    let pool = Arc::new(PacketPool::new(pool_size, max_packet_size)?);
    let q_a = Arc::new(SpscQueue::new(packet_count + 1024));
    let q_b = Arc::new(SpscQueue::new(packet_count + 1024));
    let q_out = Arc::new(SpscQueue::new(packet_count + 1024));
    let shutdown = Arc::new(BarrierFlag::default());
    let merge_join = {
        let merge_q_a = q_a.clone();
        let merge_q_b = q_b.clone();
        let merge_out = q_out.clone();
        let merge_shutdown = shutdown.clone();
        let merge_drop_pool = pool.clone();
        thread::Builder::new()
            .name("bench-merge".into())
            .spawn(move || {
                merge::merge_loop(
                    vec![merge_q_a],
                    vec![merge_q_b],
                    merge_out,
                    merge::MergeConfig {
                        next_seq: 1,
                        reorder_window: 512,
                        max_pending: packet_count + 1024,
                        dwell_ns: 1,
                        adaptive: true,
                        reorder_window_max: 4096,
                    },
                    merge_shutdown,
                    merge::MergeRuntime {
                        drop_pool: Some(merge_drop_pool),
                        ..merge::MergeRuntime::default()
                    },
                )
            })?
    };

    for (idx, payload) in payloads.iter().enumerate() {
        let seq = seq_extractor
            .extract_seq(payload)
            .unwrap_or(idx as u64 + 1)
            .max(1);
        let chan = if idx & 1 == 0 { b'A' } else { b'B' };
        let pkt = make_pkt(&pool, payload, seq, chan)?;
        if chan == b'A' {
            push_pkt(&q_a, pkt)?;
        } else {
            push_pkt(&q_b, pkt)?;
        }
    }

    let bus = (distribution || proof_mode).then(|| Bus::new(packet_count + 1024));
    let publisher = bus.as_ref().map(Bus::publisher);
    let mut book = benchmark_order_book(cfg);
    let mut proof_journal = if proof_mode {
        Some(ProofJournal::create(profile)?)
    } else {
        None
    };
    let mut events = Vec::with_capacity(128);
    let mut seen = SequenceStats::default();
    let mut decoded_events = 0usize;
    let mut decoder_sequence_gap_events = 0usize;
    let mut event_vec_reallocs = 0usize;
    let start = Instant::now();
    let deadline = start + Duration::from_secs(10);
    let mut idle = 0u32;

    while seen.forwarded < packet_count {
        if Instant::now() > deadline {
            shutdown.raise();
            let _ = join_merge(merge_join);
            anyhow::bail!(
                "{} timed out after forwarding {} of {} packets",
                profile,
                seen.forwarded,
                packet_count
            );
        }
        if let Some(pkt) = q_out.pop() {
            classify_sequence(
                pkt.seq,
                &mut seen.last_seq,
                &mut seen.sequence_gaps,
                &mut seen.duplicate_or_ooo,
            );
            seen.forwarded += 1;

            let cap_before = events.capacity();
            events.clear();
            parser.decode_into(pkt.payload(), &mut events);
            if events.capacity() > cap_before {
                event_vec_reallocs += 1;
            }
            decoded_events += events.len();

            for (event_index, event) in events.iter().enumerate() {
                if matches!(event, Event::SequenceGap { .. }) {
                    decoder_sequence_gap_events += 1;
                }
                let instr_before_apply = match *event {
                    Event::Mod { order_id, .. }
                    | Event::Del { order_id }
                    | Event::Execute { order_id, .. } => book.instrument_for_order(order_id),
                    _ => None,
                };
                book.apply(event);
                if let Some(proof_journal) = proof_journal.as_mut() {
                    proof_journal.append(
                        pkt.seq,
                        u16::try_from(event_index).unwrap_or(u16::MAX),
                        event,
                    )?;
                }
                if let Some(publisher) = &publisher {
                    publish_obo_event(publisher, &book, instr_before_apply, event);
                }
            }
            pkt.recycle(&pool);
            idle = 0;
        } else {
            adaptive_wait(&mut idle, 64);
        }
    }

    shutdown.raise();
    join_merge(merge_join)?;

    let elapsed = start.elapsed();
    let pool_available = pool.available();
    let state_hash = book.state_hash();
    let journal_proof = if let Some(proof_journal) = proof_journal.take() {
        Some(proof_journal.finish_and_replay(cfg)?)
    } else {
        None
    };
    let expected_hash_match = expected
        .as_ref()
        .map(|expected| {
            expected.state_hash == state_hash
                && expected.events == decoded_events
                && expected.sequence_gap_events == decoder_sequence_gap_events
        })
        .unwrap_or(true);
    let journal_hash_match = journal_proof
        .as_ref()
        .map(|journal| {
            journal.matched
                && journal.final_hash == state_hash
                && journal.non_monotonic_sequences == 0
        })
        .unwrap_or(true);
    let obo_frames = publisher
        .as_ref()
        .map(Publisher::next_global_sequence)
        .unwrap_or(0);
    let proof_ok = if proof_mode {
        expected_hash_match
            && journal_hash_match
            && decoder_sequence_gap_events == 0
            && obo_frames > 0
    } else {
        true
    };
    let status = if seen.sequence_gaps == 0
        && seen.duplicate_or_ooo == 0
        && event_vec_reallocs == 0
        && pool_available == pool_size
        && decoded_events > 0
        && proof_ok
    {
        "ok"
    } else {
        "fail"
    };
    let mut fields = report_fields(profile, status, None, None, "software", "synthetic")
        .into_iter()
        .chain([
            ("packets".to_string(), seen.forwarded.to_string()),
            ("events".to_string(), decoded_events.to_string()),
            (
                "decoder_sequence_gap_events".to_string(),
                decoder_sequence_gap_events.to_string(),
            ),
            ("elapsed_ms".to_string(), millis(elapsed)),
            ("throughput_mpps".to_string(), mpps(seen.forwarded, elapsed)),
            ("sequence_gaps".to_string(), seen.sequence_gaps.to_string()),
            ("dup_or_ooo".to_string(), seen.duplicate_or_ooo.to_string()),
            (
                "event_vec_reallocs".to_string(),
                event_vec_reallocs.to_string(),
            ),
            ("pool_available".to_string(), pool_available.to_string()),
            ("pool_size".to_string(), pool_size.to_string()),
            ("live_orders".to_string(), book.order_count().to_string()),
            ("state_hash".to_string(), state_hash.to_string()),
            ("obo_frames".to_string(), obo_frames.to_string()),
        ])
        .collect::<Vec<_>>();
    fields.extend(input_metadata(args));
    if let Some(expected) = expected {
        fields.extend([
            ("expected_packets".to_string(), expected.packets.to_string()),
            ("expected_events".to_string(), expected.events.to_string()),
            (
                "expected_sequence_gap_events".to_string(),
                expected.sequence_gap_events.to_string(),
            ),
            (
                "expected_state_hash".to_string(),
                expected.state_hash.to_string(),
            ),
            (
                "expected_live_orders".to_string(),
                expected.live_orders.to_string(),
            ),
            (
                "expected_hash_match".to_string(),
                expected_hash_match.to_string(),
            ),
        ]);
    }
    if let Some(journal) = &journal_proof {
        fields.extend([
            ("journal_records".to_string(), journal.records.to_string()),
            ("journal_bytes".to_string(), journal.bytes.to_string()),
            (
                "journal_final_hash".to_string(),
                journal.final_hash.to_string(),
            ),
            (
                "journal_non_monotonic_sequences".to_string(),
                journal.non_monotonic_sequences.to_string(),
            ),
            ("journal_matched".to_string(), journal.matched.to_string()),
            (
                "journal_hash_match".to_string(),
                journal_hash_match.to_string(),
            ),
        ]);
    }
    let proof_lines =
        local_proof_lines(proof_mode, expected_hash_match, journal_hash_match, &fields);
    emit_report(args, profile, fields, proof_lines)?;

    if status != "ok" {
        anyhow::bail!(
            "{} failed: sequence_gaps={} dup_or_ooo={} event_vec_reallocs={} pool_available={}/{} decoded_events={} expected_hash_match={} journal_hash_match={} decoder_sequence_gap_events={}",
            profile,
            seen.sequence_gaps,
            seen.duplicate_or_ooo,
            event_vec_reallocs,
            pool_available,
            pool_size,
            decoded_events,
            expected_hash_match,
            journal_hash_match,
            decoder_sequence_gap_events
        );
    }
    Ok(())
}

fn fixture_config(packets: usize) -> FixtureConfig {
    FixtureConfig {
        packet_count: packets,
        messages_per_packet: 4,
        ..FixtureConfig::default()
    }
}

fn load_payloads(args: &Args, cfg: FixtureConfig) -> anyhow::Result<Vec<Vec<u8>>> {
    if let Some(path) = &args.capture_path {
        let payloads = read_capture_payloads(path, args.packets)?;
        if payloads.is_empty() {
            anyhow::bail!("capture {:?} did not contain payloads", path);
        }
        Ok(payloads)
    } else {
        Ok(BenchmarkFixtures::new(cfg).eobi_packets)
    }
}

fn input_metadata(args: &Args) -> Vec<(String, String)> {
    if let Some(path) = &args.capture_path {
        let capture_hash = fs::read(path)
            .map(|bytes| stable_config_hash(&bytes))
            .unwrap_or_else(|_| "unreadable".to_string());
        vec![
            ("input_source".to_string(), "capture".to_string()),
            ("capture_path".to_string(), path.display().to_string()),
            ("capture_hash".to_string(), capture_hash),
        ]
    } else {
        vec![("input_source".to_string(), "synthetic".to_string())]
    }
}

fn payloads_in_sequence_order(
    payloads: &[Vec<u8>],
    seq_extractor: &dyn SeqExtractor,
) -> Vec<Vec<u8>> {
    let mut indexed = payloads
        .iter()
        .enumerate()
        .map(|(idx, payload)| {
            (
                seq_extractor
                    .extract_seq(payload)
                    .unwrap_or(idx as u64 + 1)
                    .max(1),
                idx,
                payload.clone(),
            )
        })
        .collect::<Vec<_>>();
    indexed.sort_by_key(|(seq, idx, _)| (*seq, *idx));
    indexed.into_iter().map(|(_, _, payload)| payload).collect()
}

#[derive(Debug)]
struct JournalProof {
    records: usize,
    bytes: u64,
    final_hash: u64,
    non_monotonic_sequences: usize,
    matched: bool,
}

struct ProofJournal {
    dir: PathBuf,
    path: PathBuf,
    writer: BufWriter<File>,
    records: usize,
}

impl ProofJournal {
    fn create(profile: &str) -> anyhow::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "numi-orderbook-{}-{}-{}",
            sanitize_component(profile),
            std::process::id(),
            now_nanos()
        ));
        fs::create_dir_all(&dir)?;
        let path = dir.join("proof.journal");
        let writer = BufWriter::new(File::create(&path)?);
        Ok(Self {
            dir,
            path,
            writer,
            records: 0,
        })
    }

    fn append(&mut self, seq: u64, event_index: u16, event: &Event) -> anyhow::Result<()> {
        append_record(
            &mut self.writer,
            &JournalRecord::new_at(seq, event_index, event, None),
        )?;
        self.records += 1;
        Ok(())
    }

    fn finish_and_replay(mut self, cfg: FixtureConfig) -> anyhow::Result<JournalProof> {
        self.writer.flush()?;
        drop(self.writer);

        let bytes = fs::metadata(&self.path)?.len();
        let mut replayed = benchmark_order_book(cfg);
        let mut reader = BufReader::new(File::open(&self.path)?);
        let report = replay_reader(&mut reader, &mut replayed)?;
        let proof = JournalProof {
            records: self.records,
            bytes,
            final_hash: report.final_hash,
            non_monotonic_sequences: report.non_monotonic_sequences,
            matched: report.matched,
        };

        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.dir);
        Ok(proof)
    }
}

fn local_proof_lines(
    proof_mode: bool,
    expected_hash_match: bool,
    journal_hash_match: bool,
    fields: &[(String, String)],
) -> Vec<String> {
    if !proof_mode {
        return vec!["proof_scope=local_smoke".to_string()];
    }

    vec![
        "proof_scope=rx_pool_merge_eobi_decode_book_obo_journal".to_string(),
        format!(
            "wire_replay_hash_match={} state_hash={} expected_state_hash={}",
            expected_hash_match,
            field_value(fields, "state_hash").unwrap_or("missing"),
            field_value(fields, "expected_state_hash").unwrap_or("missing")
        ),
        format!(
            "journal_replay_hash_match={} journal_final_hash={} journal_records={}",
            journal_hash_match,
            field_value(fields, "journal_final_hash").unwrap_or("missing"),
            field_value(fields, "journal_records").unwrap_or("missing")
        ),
        format!(
            "venue_sequence_gap_events={} obo_frames={}",
            field_value(fields, "decoder_sequence_gap_events").unwrap_or("missing"),
            field_value(fields, "obo_frames").unwrap_or("missing")
        ),
    ]
}

fn run_target_rx(args: &Args) -> anyhow::Result<()> {
    let config_bytes = fs::read(&args.config_path)?;
    let cfg = AppConfig::from_file(&args.config_path)?;
    let parser = parser_from_config(&cfg)?;
    let pool = Arc::new(PacketPool::new(
        cfg.general.pool_size,
        cfg.general.max_packet_size as usize,
    )?);
    let q_a = Arc::new(SpscQueue::new(cfg.general.rx_queue_capacity));
    let q_b = Arc::new(SpscQueue::new(cfg.general.rx_queue_capacity));
    let shutdown = Arc::new(BarrierFlag::default());
    let sock_a = net::build_mcast_socket(&cfg.channels.a)?;
    let sock_b = net::build_mcast_socket(&cfg.channels.b)?;

    let join_a = spawn_rx_worker(RxWorkerArgs {
        name: "A",
        socket: sock_a,
        parser: parser.clone(),
        queue: q_a.clone(),
        pool: pool.clone(),
        shutdown: shutdown.clone(),
        spin_loops_per_yield: cfg.general.spin_loops_per_yield,
        rx_batch: cfg.general.rx_recvmmsg_batch.unwrap_or(1),
        ts_mode: cfg.channels.a.timestamping.clone(),
    })?;
    let join_b = spawn_rx_worker(RxWorkerArgs {
        name: "B",
        socket: sock_b,
        parser,
        queue: q_b.clone(),
        pool: pool.clone(),
        shutdown: shutdown.clone(),
        spin_loops_per_yield: cfg.general.spin_loops_per_yield,
        rx_batch: cfg.general.rx_recvmmsg_batch.unwrap_or(1),
        ts_mode: cfg.channels.b.timestamping.clone(),
    })?;

    let start = Instant::now();
    let mut received = 0usize;
    let mut a_packets = 0usize;
    let mut b_packets = 0usize;
    let mut sw_ts = 0usize;
    let mut hw_sys_ts = 0usize;
    let mut hw_raw_ts = 0usize;
    let mut no_ts = 0usize;
    let mut sequence_gaps = 0usize;
    let mut duplicate_or_ooo = 0usize;
    let mut last_seq = None;
    let mut idle = 0u32;

    while start.elapsed() < args.duration && received < args.packets {
        let pkt = q_a.pop().or_else(|| q_b.pop());
        if let Some(pkt) = pkt {
            received += 1;
            if pkt.chan == b'A' {
                a_packets += 1;
            } else {
                b_packets += 1;
            }
            classify_timestamp(
                pkt._ts_kind,
                &mut sw_ts,
                &mut hw_sys_ts,
                &mut hw_raw_ts,
                &mut no_ts,
            );
            classify_sequence(
                pkt.seq,
                &mut last_seq,
                &mut sequence_gaps,
                &mut duplicate_or_ooo,
            );
            pkt.recycle(&pool);
            idle = 0;
        } else {
            adaptive_wait(&mut idle, 64);
        }
    }

    shutdown.raise();
    join_rx(join_a)?;
    join_rx(join_b)?;

    let elapsed = start.elapsed();
    let status = if sequence_gaps == 0 && duplicate_or_ooo == 0 {
        "ok"
    } else {
        "fail"
    };
    let fields = report_fields(
        "target-rx",
        status,
        Some(&args.config_path),
        Some(&config_bytes),
        timestamping_label(cfg.channels.a.timestamping.as_ref()),
        format!("{}:{}", cfg.channels.a.iface_addr, cfg.channels.a.port),
    )
    .into_iter()
    .chain([
        ("packets".to_string(), received.to_string()),
        ("a_packets".to_string(), a_packets.to_string()),
        ("b_packets".to_string(), b_packets.to_string()),
        ("elapsed_ms".to_string(), millis(elapsed)),
        ("rx_mpps".to_string(), mpps(received, elapsed)),
        ("sw_ts".to_string(), sw_ts.to_string()),
        ("hw_sys_ts".to_string(), hw_sys_ts.to_string()),
        ("hw_raw_ts".to_string(), hw_raw_ts.to_string()),
        ("no_ts".to_string(), no_ts.to_string()),
        ("sequence_gaps".to_string(), sequence_gaps.to_string()),
        ("dup_or_ooo".to_string(), duplicate_or_ooo.to_string()),
        ("pool_available".to_string(), pool.available().to_string()),
    ])
    .collect::<Vec<_>>();
    emit_report(
        args,
        "target-rx",
        fields,
        vec!["proof_scope=target_udp_rx".to_string()],
    )?;

    if status != "ok" {
        anyhow::bail!(
            "target-rx failed: sequence_gaps={} dup_or_ooo={}",
            sequence_gaps,
            duplicate_or_ooo
        );
    }
    Ok(())
}

fn run_target_failover_recovery(args: &Args) -> anyhow::Result<()> {
    const GAP_START: u64 = 65;
    const GAP_LEN: u64 = 1_000;
    const GAP_END: u64 = GAP_START + GAP_LEN - 1;
    const POST_GAP_SEQ: u64 = GAP_END + 1;

    let pool_size = 4096usize;
    let pool = Arc::new(PacketPool::new(pool_size, 256)?);
    let q_a = Arc::new(SpscQueue::new(2048));
    let q_b = Arc::new(SpscQueue::new(2048));
    let q_rec = Arc::new(SpscQueue::new(2048));
    let q_out = Arc::new(SpscQueue::new(2048));
    let shutdown = Arc::new(BarrierFlag::default());
    let recovery = Arc::new(RecordingRecovery::default());
    let merge_join = {
        let q_a = q_a.clone();
        let q_b = q_b.clone();
        let q_rec = q_rec.clone();
        let q_out = q_out.clone();
        let shutdown = shutdown.clone();
        let recovery = recovery.clone();
        let merge_drop_pool = pool.clone();
        thread::Builder::new()
            .name("bench-failover-merge".into())
            .spawn(move || {
                merge::merge_loop(
                    vec![q_a],
                    vec![q_b],
                    q_out,
                    merge::MergeConfig {
                        next_seq: 1,
                        reorder_window: 4,
                        max_pending: 128,
                        dwell_ns: 1,
                        adaptive: false,
                        reorder_window_max: 4,
                    },
                    shutdown,
                    merge::MergeRuntime {
                        recovery: Some(recovery),
                        q_recovery_in: Some(q_rec),
                        drop_pool: Some(merge_drop_pool),
                    },
                )
            })?
    };

    for seq in 1..=32 {
        push_pkt(&q_a, make_pkt(&pool, &[0; 8], seq, b'A')?)?;
    }
    let mut seen = SequenceStats::default();
    drain_until(&q_out, &pool, 32, &mut seen, Duration::from_secs(2))?;

    let failover_start = Instant::now();
    for seq in 33..=64 {
        push_pkt(&q_b, make_pkt(&pool, &[0; 8], seq, b'B')?)?;
    }
    drain_until(&q_out, &pool, 64, &mut seen, Duration::from_secs(2))?;
    let failover_ms = failover_start.elapsed();

    let recovery_start = Instant::now();
    push_pkt(&q_b, make_pkt(&pool, &[0; 8], POST_GAP_SEQ, b'B')?)?;
    wait_for_gap(&recovery, Duration::from_secs(2))?;
    for seq in GAP_START..=GAP_END {
        push_pkt(&q_rec, make_pkt(&pool, &[0; 8], seq, b'R')?)?;
    }
    drain_until(&q_out, &pool, GAP_END, &mut seen, Duration::from_secs(2))?;
    push_pkt(&q_b, make_pkt(&pool, &[0; 8], POST_GAP_SEQ, b'B')?)?;
    drain_until(
        &q_out,
        &pool,
        POST_GAP_SEQ,
        &mut seen,
        Duration::from_secs(2),
    )?;
    let recovery_ms = recovery_start.elapsed();

    shutdown.raise();
    join_merge(merge_join)?;

    let gaps = recovery.gaps.lock().unwrap().clone();
    let pool_available = pool.available();
    let status = if seen.sequence_gaps == 0
        && seen.duplicate_or_ooo == 0
        && gaps == vec![(GAP_START, GAP_END)]
        && recovery_ms <= Duration::from_millis(100)
        && pool_available == pool_size
    {
        "ok"
    } else {
        "fail"
    };

    let fields = report_fields(
        "target-failover-recovery",
        status,
        Some(&args.config_path),
        fs::read(&args.config_path).ok().as_deref(),
        "synthetic",
        "synthetic",
    )
    .into_iter()
    .chain([
        ("forwarded".to_string(), seen.forwarded.to_string()),
        (
            "last_seq".to_string(),
            seen.last_seq.unwrap_or(0).to_string(),
        ),
        ("gap_len".to_string(), GAP_LEN.to_string()),
        ("sequence_gaps".to_string(), seen.sequence_gaps.to_string()),
        ("dup_or_ooo".to_string(), seen.duplicate_or_ooo.to_string()),
        ("notified_gaps".to_string(), format!("{:?}", gaps)),
        ("failover_ms".to_string(), millis(failover_ms)),
        ("recovery_ms".to_string(), millis(recovery_ms)),
        ("pool_available".to_string(), pool_available.to_string()),
        ("pool_size".to_string(), pool_size.to_string()),
    ])
    .collect::<Vec<_>>();
    emit_report(
        args,
        "target-failover-recovery",
        fields,
        vec!["proof_scope=merge_failover_gap_replay".to_string()],
    )?;

    if status != "ok" {
        anyhow::bail!("target-failover-recovery failed");
    }
    Ok(())
}

fn run_target_persistence(args: &Args) -> anyhow::Result<()> {
    let cfg = fixture_config(args.packets);
    let fixtures = BenchmarkFixtures::new(cfg);
    let mut live = benchmark_order_book(cfg);
    let temp_base = std::env::temp_dir().join(format!(
        "numi-orderbook-bench-{}-{}",
        std::process::id(),
        now_nanos()
    ));
    fs::create_dir_all(&temp_base)?;
    let journal_path = temp_base.join("book.journal");
    let snapshot_path = temp_base.join("book.snap");
    let mut journal = BufWriter::new(File::create(&journal_path)?);
    let snapshot_at = fixtures.events.len() / 2;
    let start = Instant::now();

    for (idx, event) in fixtures.events.iter().enumerate() {
        live.apply(event);
        append_record(
            &mut journal,
            &JournalRecord::new_at(idx as u64 + 1, 0, event, Some(live.state_hash())),
        )?;
        if idx == snapshot_at {
            orderbook::snapshot::write_atomic(
                &snapshot_path,
                &orderbook::snapshot::SnapshotImage {
                    export: live.export(),
                    replay_from: Some(idx as u64 + 1),
                },
            )?;
        }
    }
    journal.flush()?;

    let loaded = orderbook::snapshot::load_image(&snapshot_path)?;
    let mut restored = loaded.book;
    let mut reader = BufReader::new(File::open(&journal_path)?);
    let report = replay_after_snapshot(&mut reader, &mut restored)?;
    let elapsed = start.elapsed();
    let final_hash = live.state_hash();
    let restored_hash = restored.state_hash();
    let status = if report.matched && final_hash == restored_hash {
        "ok"
    } else {
        "fail"
    };

    let _ = fs::remove_file(&journal_path);
    let _ = fs::remove_file(&snapshot_path);
    let _ = fs::remove_dir(&temp_base);

    let fields = report_fields(
        "target-persistence",
        status,
        Some(&args.config_path),
        fs::read(&args.config_path).ok().as_deref(),
        "synthetic",
        "synthetic",
    )
    .into_iter()
    .chain([
        ("events".to_string(), fixtures.events.len().to_string()),
        ("elapsed_ms".to_string(), millis(elapsed)),
        (
            "snapshot_replay_from".to_string(),
            loaded.replay_from.unwrap_or(0).to_string(),
        ),
        ("anchored".to_string(), report.anchored.to_string()),
        (
            "replayed_events".to_string(),
            report.replay.events.to_string(),
        ),
        (
            "non_monotonic_sequences".to_string(),
            report.replay.non_monotonic_sequences.to_string(),
        ),
        ("final_hash".to_string(), final_hash.to_string()),
        ("restored_hash".to_string(), restored_hash.to_string()),
    ])
    .collect::<Vec<_>>();
    emit_report(
        args,
        "target-persistence",
        fields,
        vec!["proof_scope=snapshot_journal_restart".to_string()],
    )?;

    if status != "ok" {
        anyhow::bail!("target-persistence failed");
    }
    Ok(())
}

fn parser_from_config(cfg: &AppConfig) -> anyhow::Result<Parser> {
    build_parser(
        cfg.parser.kind.clone(),
        SeqCfg {
            offset: cfg.sequence.offset,
            length: cfg.sequence.length,
            endian: cfg.sequence.endian.clone(),
        },
        cfg.parser.max_messages_per_packet,
    )
}

fn default_eobi_parser() -> anyhow::Result<Parser> {
    build_parser(
        ParserKind::Eobi,
        SeqCfg {
            offset: 8,
            length: 4,
            endian: Endian::Le,
        },
        128,
    )
}

struct RxWorkerArgs {
    name: &'static str,
    socket: std::net::UdpSocket,
    parser: Parser,
    queue: Arc<SpscQueue<Pkt>>,
    pool: Arc<PacketPool>,
    shutdown: Arc<BarrierFlag>,
    spin_loops_per_yield: u32,
    rx_batch: usize,
    ts_mode: Option<TimestampingMode>,
}

fn spawn_rx_worker(args: RxWorkerArgs) -> anyhow::Result<thread::JoinHandle<anyhow::Result<()>>> {
    thread::Builder::new()
        .name(format!("bench-rx-{}", args.name))
        .spawn(move || {
            rx_udp_loop(
                args.name,
                &args.socket,
                args.parser.seq_extractor(),
                args.queue,
                args.pool,
                args.shutdown,
                UdpRxConfig {
                    spin_loops_per_yield: args.spin_loops_per_yield,
                    rx_batch: args.rx_batch,
                    ts_mode: args.ts_mode,
                },
            )
        })
        .map_err(anyhow::Error::from)
}

fn make_pkt(pool: &PacketPool, payload: &[u8], seq: u64, chan: u8) -> anyhow::Result<Pkt> {
    let mut buf = pool.get();
    if buf.capacity() < payload.len() {
        anyhow::bail!(
            "packet payload len {} exceeds pool buffer capacity {}",
            payload.len(),
            buf.capacity()
        );
    }
    buf.extend_from_slice(payload);
    Ok(Pkt {
        buf: PktBuf::Bytes(buf),
        len: payload.len(),
        seq,
        ts_nanos: now_nanos(),
        chan,
        _ts_kind: TsKind::Sw,
        merge_emit_ns: 0,
    })
}

fn push_pkt(queue: &SpscQueue<Pkt>, pkt: Pkt) -> anyhow::Result<()> {
    queue
        .push(pkt)
        .map_err(|_| anyhow::anyhow!("benchmark queue is full"))
}

fn publish_obo_event(
    publisher: &Publisher,
    book: &OrderBook,
    instr_before_apply: Option<u32>,
    event: &Event,
) {
    let (maybe_instr, maybe_obo) = map_event_to_obo_parts(event);
    let Some(obo_event) = maybe_obo else {
        return;
    };
    let instr = maybe_instr.or(instr_before_apply).or_else(|| match *event {
        Event::Mod { order_id, .. } | Event::Del { order_id } => {
            book.instrument_for_order(order_id)
        }
        Event::MassDel { instr }
        | Event::Trade { instr, .. }
        | Event::Add { instr, .. }
        | Event::Execute { instr, .. } => Some(instr),
        _ => None,
    });
    let Some(instr) = instr.map(u64::from) else {
        return;
    };
    let seq = publisher.next_seq_for_instrument(instr);
    match obo_event {
        OboEventV1::Add(payload) => {
            publisher.publish_raw(
                msg_type::OBO_ADD,
                channel_id::OBO_L3,
                instr,
                seq,
                payload.as_bytes(),
            );
        }
        OboEventV1::Modify(payload) => {
            publisher.publish_raw(
                msg_type::OBO_MODIFY,
                channel_id::OBO_L3,
                instr,
                seq,
                payload.as_bytes(),
            );
        }
        OboEventV1::Cancel(payload) => {
            publisher.publish_raw(
                msg_type::OBO_CANCEL,
                channel_id::OBO_L3,
                instr,
                seq,
                payload.as_bytes(),
            );
        }
        OboEventV1::Execute(payload) => {
            publisher.publish_raw(
                msg_type::OBO_EXECUTE,
                channel_id::OBO_L3,
                instr,
                seq,
                payload.as_bytes(),
            );
        }
    };
}

fn classify_timestamp(
    kind: TsKind,
    sw: &mut usize,
    hw_sys: &mut usize,
    hw_raw: &mut usize,
    none: &mut usize,
) {
    match kind {
        TsKind::Sw => *sw += 1,
        TsKind::HwSys => *hw_sys += 1,
        TsKind::HwRaw => *hw_raw += 1,
        TsKind::None => *none += 1,
    }
}

fn classify_sequence(
    seq: u64,
    last_seq: &mut Option<u64>,
    sequence_gaps: &mut usize,
    duplicate_or_ooo: &mut usize,
) {
    if let Some(last) = *last_seq {
        if seq <= last {
            *duplicate_or_ooo += 1;
        } else if seq != last + 1 {
            *sequence_gaps += 1;
        }
    }
    *last_seq = Some(seq.max((*last_seq).unwrap_or(0)));
}

#[derive(Default)]
struct SequenceStats {
    forwarded: usize,
    last_seq: Option<u64>,
    sequence_gaps: usize,
    duplicate_or_ooo: usize,
}

fn drain_until(
    q_out: &SpscQueue<Pkt>,
    pool: &PacketPool,
    target_seq: u64,
    seen: &mut SequenceStats,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut idle = 0u32;
    while seen.last_seq.unwrap_or(0) < target_seq {
        if Instant::now() > deadline {
            anyhow::bail!(
                "timed out waiting for seq {} after forwarding {}",
                target_seq,
                seen.forwarded
            );
        }
        if let Some(pkt) = q_out.pop() {
            classify_sequence(
                pkt.seq,
                &mut seen.last_seq,
                &mut seen.sequence_gaps,
                &mut seen.duplicate_or_ooo,
            );
            seen.forwarded += 1;
            pkt.recycle(pool);
            idle = 0;
        } else {
            adaptive_wait(&mut idle, 64);
        }
    }
    Ok(())
}

#[derive(Default)]
struct RecordingRecovery {
    gaps: Mutex<Vec<(u64, u64)>>,
}

impl Replayer for RecordingRecovery {
    fn notify_gap(&self, from: u64, to: u64) {
        self.gaps.lock().unwrap().push((from, to));
    }
}

fn wait_for_gap(recovery: &RecordingRecovery, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while recovery.gaps.lock().unwrap().is_empty() {
        if Instant::now() > deadline {
            anyhow::bail!("timed out waiting for recovery gap notification");
        }
        std::thread::yield_now();
    }
    Ok(())
}

fn join_merge(join: thread::JoinHandle<anyhow::Result<()>>) -> anyhow::Result<()> {
    join.join()
        .map_err(|_| anyhow::anyhow!("merge thread panicked"))?
}

fn join_rx(join: thread::JoinHandle<anyhow::Result<()>>) -> anyhow::Result<()> {
    join.join()
        .map_err(|_| anyhow::anyhow!("rx thread panicked"))?
}

fn report_fields(
    profile: &str,
    status: &str,
    config_path: Option<&Path>,
    config_bytes: Option<&[u8]>,
    timestamp_source: &str,
    nic: impl Into<String>,
) -> Vec<(String, String)> {
    let config_hash = config_bytes
        .map(stable_config_hash)
        .unwrap_or_else(|| "synthetic".to_string());
    vec![
        ("report_schema".to_string(), "bench_pipeline.v1".to_string()),
        ("profile".to_string(), profile.to_string()),
        ("status".to_string(), status.to_string()),
        (
            "git_sha".to_string(),
            command_output("git", &["rev-parse", "--short=12", "HEAD"]).unwrap_or("unknown".into()),
        ),
        ("git_dirty".to_string(), git_dirty().to_string()),
        (
            "rustc".to_string(),
            command_output("rustc", &["--version"]).unwrap_or("unknown".into()),
        ),
        ("allocator".to_string(), allocator_label().to_string()),
        (
            "os".to_string(),
            format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        ),
        (
            "kernel".to_string(),
            command_output("uname", &["-sr"]).unwrap_or("unknown".into()),
        ),
        ("cpu".to_string(), cpu_label()),
        ("nic".to_string(), nic.into()),
        ("timestamp_source".to_string(), timestamp_source.to_string()),
        (
            "config_path".to_string(),
            config_path
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "synthetic".to_string()),
        ),
        ("config_hash".to_string(), config_hash),
    ]
}

fn emit_report(
    args: &Args,
    profile: &str,
    mut fields: Vec<(String, String)>,
    proof_lines: Vec<String>,
) -> anyhow::Result<()> {
    if let Some(root) = &args.artifact_dir {
        let git_sha = field_value(&fields, "git_sha")
            .unwrap_or("unknown")
            .to_string();
        let dir = root.join(format!(
            "{}-{}-{}",
            sanitize_component(profile),
            now_nanos(),
            sanitize_component(&git_sha)
        ));
        fs::create_dir_all(&dir)?;
        fields.push(("artifact_dir".to_string(), dir.display().to_string()));
        write_artifact_files(&dir, &fields, &proof_lines)?;
    }

    println!("{}", format_kv_line(fields));
    Ok(())
}

fn write_artifact_files(
    dir: &Path,
    fields: &[(String, String)],
    proof_lines: &[String],
) -> anyhow::Result<()> {
    let summary = format_kv_line(
        fields
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    fs::write(dir.join("summary.kv"), format!("{summary}\n"))?;

    let manifest = format_kv_line([
        ("artifact_schema", "bench_pipeline_artifact.v1".to_string()),
        (
            "profile",
            field_value(fields, "profile")
                .unwrap_or("unknown")
                .to_string(),
        ),
        (
            "status",
            field_value(fields, "status")
                .unwrap_or("unknown")
                .to_string(),
        ),
        (
            "git_sha",
            field_value(fields, "git_sha")
                .unwrap_or("unknown")
                .to_string(),
        ),
        (
            "git_dirty",
            field_value(fields, "git_dirty")
                .unwrap_or("unknown")
                .to_string(),
        ),
        ("created_ns", now_nanos().to_string()),
    ]);
    fs::write(dir.join("manifest.kv"), format!("{manifest}\n"))?;

    let mut proof = String::new();
    for line in proof_lines {
        proof.push_str(line);
        proof.push('\n');
    }
    fs::write(dir.join("proof.txt"), proof)?;
    Ok(())
}

fn field_value<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(field_key, _)| field_key == key)
        .map(|(_, value)| value.as_str())
}

fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_dirty() -> bool {
    command_output("git", &["status", "--porcelain"])
        .map(|status| !status.is_empty())
        .unwrap_or(true)
}

fn allocator_label() -> &'static str {
    if cfg!(all(target_os = "linux", feature = "jemalloc")) {
        "jemalloc"
    } else if cfg!(feature = "mimalloc") {
        "mimalloc"
    } else {
        "system"
    }
}

fn cpu_label() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") {
            if let Some(line) = cpuinfo.lines().find(|line| line.starts_with("model name")) {
                if let Some((_, value)) = line.split_once(':') {
                    return value.trim().to_string();
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(value) = command_output("sysctl", &["-n", "machdep.cpu.brand_string"]) {
            return value;
        }
    }
    "unknown".to_string()
}

fn timestamping_label(mode: Option<&TimestampingMode>) -> &'static str {
    match mode {
        Some(TimestampingMode::Software) => "software",
        Some(TimestampingMode::Hardware) => "hardware",
        Some(TimestampingMode::HardwareRaw) => "hardware_raw",
        Some(TimestampingMode::Off) | None => "off",
    }
}

fn millis(duration: Duration) -> String {
    format!("{:.3}", duration.as_secs_f64() * 1000.0)
}

fn mpps(packets: usize, duration: Duration) -> String {
    format!(
        "{:.6}",
        packets as f64 / 1_000_000.0 / duration.as_secs_f64()
    )
}
