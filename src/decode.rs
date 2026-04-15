// src/decode.rs Numan Thabit:
use crate::codec_raw::channel_id;
use crate::codec_raw::msg_type;
use crate::metrics;
use crate::obo::{map_event_to_obo_parts, OboEventV1};
use crate::orderbook::{OrderBook, OrderBookCapacity};
use crate::parser::Parser;
use crate::pool::{PacketPool, Pkt};
use crate::pubsub::Publisher as OboPublisher;
use crate::spsc::{AdaptiveBatchCap, SpscQueue, DEFAULT_BATCH_CAP};
use crate::util::{now_nanos, BarrierFlag};
use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use log::{info, warn};
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zerocopy::AsBytes;

pub struct DecodeConfig {
    pub max_depth: usize,
    pub snapshot_interval_ms: u64,
    pub consume_trades: bool,
    pub default_slab_capacity: usize,
    pub default_tick: i64,
    pub grid_span: usize,
    pub book_capacity: OrderBookCapacity,
    pub instrument_ticks: Vec<(u32, i64)>,
    pub snapshot_tx: Option<Sender<crate::snapshot::SnapshotImage>>,
    pub initial_book: Option<OrderBook>,
    pub snapshot_trigger_rx: Option<Receiver<()>>,
    pub obo_publisher: Option<OboPublisher>,
    pub journal_path: Option<String>,
    pub journal_record_state_hash: bool,
}

pub fn decode_loop(
    q_in: Arc<SpscQueue<Pkt>>,
    pool: Arc<PacketPool>,
    parser: Parser,
    shutdown: Arc<BarrierFlag>,
    cfg: DecodeConfig,
) -> anyhow::Result<()> {
    let DecodeConfig {
        max_depth,
        snapshot_interval_ms,
        consume_trades,
        default_slab_capacity,
        default_tick,
        grid_span,
        book_capacity,
        instrument_ticks,
        snapshot_tx,
        initial_book,
        snapshot_trigger_rx,
        obo_publisher,
        journal_path,
        journal_record_state_hash,
    } = cfg;

    let mut book = initial_book.unwrap_or_else(|| {
        OrderBook::new_with_tick_table_and_capacity(
            max_depth,
            consume_trades,
            default_slab_capacity,
            default_tick,
            grid_span,
            instrument_ticks.iter().copied(),
            book_capacity,
        )
        .unwrap_or_else(|e| {
            warn!("invalid book tick table ({e}); falling back to default book config");
            OrderBook::new(max_depth)
        })
    });
    if let Err(e) = book.configure_defaults(default_slab_capacity, default_tick, grid_span) {
        warn!("invalid book defaults for loaded book: {e}");
    }
    if let Err(e) = book.set_instrument_ticks(instrument_ticks.iter().copied()) {
        warn!("invalid book tick table for loaded book: {e}");
    }
    book.reserve_capacity(book_capacity);
    book.set_consume_trades(consume_trades);
    // Cache sizing to avoid per-packet re-evaluations
    let max_msgs = parser.max_messages_per_packet;
    let mut events = Vec::with_capacity(max_msgs);
    let mut last_snap = Instant::now();
    let snap_every = Duration::from_millis(snapshot_interval_ms);
    let mut journal = if let Some(path) = journal_path {
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => Some(BufWriter::new(file)),
            Err(e) => {
                warn!("failed to open journal {path}: {e:?}");
                None
            }
        }
    } else {
        None
    };

    let mut processed_pkts: u64 = 0;
    let mut processed_msgs: u64 = 0;

    let mut decode_batch_cap = AdaptiveBatchCap::new(1, DEFAULT_BATCH_CAP.min(q_in.capacity()));
    let mut decode_batch: Vec<Pkt> = Vec::with_capacity(decode_batch_cap.max());
    let mut idle_iters: u32 = 0;
    while !shutdown.is_raised() {
        let requested = decode_batch_cap.current();
        let popped = q_in.pop_batch(&mut decode_batch, requested);
        if popped > 0 {
            idle_iters = 0;
            decode_batch_cap.record(requested, popped);
        } else {
            decode_batch_cap.reset();
            crate::util::adaptive_wait(&mut idle_iters, 64);
            continue;
        }

        for pkt in decode_batch.drain(..) {
            processed_pkts += 1;
            metrics::inc_decode_pkts();

            events.clear();
            let ts_nanos = pkt.ts_nanos;
            let _ts_kind = pkt._ts_kind;
            let merge_emit_ns = pkt.merge_emit_ns;
            let payload = pkt.payload();
            let cap_before = events.capacity();
            parser.decode_into(payload, &mut events);
            if events.capacity() > cap_before {
                metrics::inc_decode_event_vec_realloc();
                warn!(
                    "decode events vector reallocated: old_cap={} new_cap={} len={}",
                    cap_before,
                    events.capacity(),
                    events.len()
                );
            }
            processed_msgs += events.len() as u64;
            metrics::inc_decode_msgs(events.len() as u64);

            // Stage latency (merge -> decode)
            if merge_emit_ns > 0 {
                let now_ns = now_nanos();
                if now_ns > merge_emit_ns {
                    metrics::observe_stage_merge_to_decode_ns(now_ns - merge_emit_ns);
                }
                if merge_emit_ns > ts_nanos {
                    metrics::observe_stage_rx_to_merge_ns(merge_emit_ns - ts_nanos);
                }
            }

            for (event_index, ev) in events.iter().enumerate() {
                let instr_before_apply = if obo_publisher.is_some() {
                    match *ev {
                        crate::parser::Event::Mod { order_id, .. }
                        | crate::parser::Event::Del { order_id }
                        | crate::parser::Event::Execute { order_id, .. } => {
                            book.instrument_for_order(order_id)
                        }
                        _ => None,
                    }
                } else {
                    None
                };

                book.apply(ev);
                let journal_write_result = if let Some(writer) = journal.as_mut() {
                    let state_hash_after = journal_record_state_hash.then(|| book.state_hash());
                    let event_index = u16::try_from(event_index).unwrap_or(u16::MAX);
                    let record = crate::journal::JournalRecord::new_at(
                        pkt.seq,
                        event_index,
                        ev,
                        state_hash_after,
                    );
                    Some(crate::journal::append_record(writer, &record))
                } else {
                    None
                };
                if let Some(Err(e)) = journal_write_result {
                    warn!("disabling journal after write failure: {e:?}");
                    journal = None;
                }
                if let Some(pubh) = &obo_publisher {
                    let (maybe_instr, maybe_obo) = map_event_to_obo_parts(ev);
                    if let Some(obo_ev) = maybe_obo {
                        // Determine instrument id for this event
                        let instr_opt: Option<u32> =
                            maybe_instr.or_else(|| match *ev {
                                crate::parser::Event::Mod { order_id, .. } => instr_before_apply
                                    .or_else(|| book.instrument_for_order(order_id)),
                                crate::parser::Event::Del { order_id } => instr_before_apply
                                    .or_else(|| book.instrument_for_order(order_id)),
                                crate::parser::Event::MassDel { instr } => Some(instr),
                                crate::parser::Event::Execute { instr, .. } => Some(instr),
                                crate::parser::Event::Trade { instr, .. } => Some(instr),
                                _ => None,
                            });
                        let Some(instr) = instr_opt.map(u64::from) else {
                            warn!(
                                "skipping OBO publish for event without instrument: {:?}",
                                ev
                            );
                            continue;
                        };
                        publish_obo_event(pubh, instr, obo_ev);
                    }
                }
            }

            let now_ns = now_nanos();
            if ts_nanos != 0 && now_ns > ts_nanos {
                let d = now_ns - ts_nanos;
                metrics::observe_latency_ns(d);
                metrics::observe_latency_by_kind_ns(_ts_kind, d);
            }

            // Return backing buffer to pool (if Bytes variant)
            pkt.recycle(&pool);

            let mut should_snapshot = last_snap.elapsed() >= snap_every;
            if !should_snapshot {
                if let Some(ref rx) = snapshot_trigger_rx {
                    if rx.try_recv().is_ok() {
                        should_snapshot = true;
                    }
                }
            }
            if should_snapshot {
                metrics::set_live_orders(book.order_count());
                if let Some(ref tx) = snapshot_tx {
                    let image = crate::snapshot::SnapshotImage {
                        export: book.export(),
                        replay_from: obo_publisher
                            .as_ref()
                            .map(|pubh| pubh.next_global_sequence()),
                    };
                    let _ = tx.try_send(image);
                }
                let (bbo_bid, bbo_ask) = book.bbo();
                info!(
                    "pkts={} msgs={} live_orders={} bbo_bid={:?} bbo_ask={:?}",
                    processed_pkts,
                    processed_msgs,
                    book.order_count(),
                    bbo_bid,
                    bbo_ask
                );
                if let Some(writer) = journal.as_mut() {
                    let _ = writer.flush();
                }
                last_snap = Instant::now();
            }
        }
    }
    if let Some(mut writer) = journal {
        let _ = writer.flush();
    }
    Ok(())
}

#[inline]
fn publish_obo_event(pubh: &OboPublisher, instr: u64, obo_ev: OboEventV1) {
    let seq = pubh.next_seq_for_instrument(instr);
    match obo_ev {
        OboEventV1::Add(payload) => {
            pubh.publish_raw(
                msg_type::OBO_ADD,
                channel_id::OBO_L3,
                instr,
                seq,
                payload.as_bytes(),
            );
        }
        OboEventV1::Modify(payload) => {
            pubh.publish_raw(
                msg_type::OBO_MODIFY,
                channel_id::OBO_L3,
                instr,
                seq,
                payload.as_bytes(),
            );
        }
        OboEventV1::Cancel(payload) => {
            pubh.publish_raw(
                msg_type::OBO_CANCEL,
                channel_id::OBO_L3,
                instr,
                seq,
                payload.as_bytes(),
            );
        }
        OboEventV1::Execute(payload) => {
            pubh.publish_raw(
                msg_type::OBO_EXECUTE,
                channel_id::OBO_L3,
                instr,
                seq,
                payload.as_bytes(),
            );
        }
    }
}
