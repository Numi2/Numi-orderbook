// src/decode.rs Numan Thabit: 
use crate::metrics;
use crate::orderbook::OrderBook;
use crate::parser::Parser;
use crate::pool::{PacketPool, Pkt};
use crate::util::{now_nanos, BarrierFlag};
use crate::obo::{map_event_to_obo_parts, OboEventV1};
use crate::codec_raw::msg_type;
use crate::codec_raw::channel_id;
use crate::pubsub::Publisher as OboPublisher;
use zerocopy::AsBytes;
use crate::spsc::SpscQueue;
use crossbeam_channel::Sender;
use crossbeam_channel::Receiver;
use log::{info, warn};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct DecodeConfig {
    pub max_depth: usize,
    pub snapshot_interval_ms: u64,
    pub consume_trades: bool,
    pub snapshot_tx: Option<Sender<crate::orderbook::BookExport>>,
    pub initial_book: Option<OrderBook>,
    pub snapshot_trigger_rx: Option<Receiver<()>>,
    pub obo_publisher: Option<OboPublisher>,
}

// TODO: Group arguments into a DecodeConfig struct to reduce parameter count.
pub fn decode_loop(
    q_in: Arc<SpscQueue<Pkt>>,
    pool: Arc<PacketPool>,
    parser: Parser,
    shutdown: Arc<BarrierFlag>,
    cfg: DecodeConfig,
) -> anyhow::Result<()> {
    let mut book = cfg.initial_book.unwrap_or_else(|| OrderBook::new(cfg.max_depth));
    book.set_consume_trades(cfg.consume_trades);
    // Cache sizing to avoid per-packet re-evaluations
    let max_msgs = parser.max_messages_per_packet;
    let mut events = Vec::with_capacity(max_msgs);
    let mut last_snap = Instant::now();
    let snap_every = Duration::from_millis(cfg.snapshot_interval_ms);

    let mut processed_pkts: u64 = 0;
    let mut processed_msgs: u64 = 0;

    let mut idle_iters: u32 = 0;
    while !shutdown.is_raised() {
        if let Some(pkt) = q_in.pop() {
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
                warn!("decode events vector reallocated: old_cap={} new_cap={} len={}", cap_before, events.capacity(), events.len());
            }
            processed_msgs += events.len() as u64;
            metrics::inc_decode_msgs(events.len() as u64);

            // Stage latency (merge -> decode)
            if merge_emit_ns > 0 {
                let now_ns = now_nanos();
                if now_ns > merge_emit_ns { metrics::observe_stage_merge_to_decode_ns(now_ns - merge_emit_ns); }
                if merge_emit_ns > ts_nanos { metrics::observe_stage_rx_to_merge_ns(merge_emit_ns - ts_nanos); }
            }

            for ev in &events {
                book.apply(ev);
                if let Some(pubh) = &cfg.obo_publisher {
                    let (maybe_instr, maybe_obo) = map_event_to_obo_parts(ev);
                    if let Some(obo_ev) = maybe_obo {
                        // Determine instrument id for this event
                        let instr_opt: Option<u32> = if let Some(i) = maybe_instr { Some(i) } else {
                            match *ev {
                                crate::parser::Event::Mod { order_id, .. } => book.instrument_for_order(order_id),
                                crate::parser::Event::Del { order_id } => book.instrument_for_order(order_id),
                                crate::parser::Event::Trade { instr, .. } => Some(instr),
                                _ => None,
                            }
                        };
                        let instr = instr_opt.unwrap_or(0) as u64;
                        let (msg_ty, payload_bytes) = match obo_ev {
                            OboEventV1::Add(p) => (msg_type::OBO_ADD, p.as_bytes().to_vec()),
                            OboEventV1::Modify(p) => (msg_type::OBO_MODIFY, p.as_bytes().to_vec()),
                            OboEventV1::Cancel(p) => (msg_type::OBO_CANCEL, p.as_bytes().to_vec()),
                            OboEventV1::Execute(p) => (msg_type::OBO_EXECUTE, p.as_bytes().to_vec()),
                        };
                        let seq = pubh.next_seq_for_instrument(instr);
                        pubh.publish_raw(msg_ty, channel_id::OBO_L3, instr, seq, &payload_bytes);
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
                if let Some(ref rx) = cfg.snapshot_trigger_rx {
                    if rx.try_recv().is_ok() { should_snapshot = true; }
                }
            }
            if should_snapshot {
                metrics::set_live_orders(book.order_count());
                if let Some(ref tx) = cfg.snapshot_tx {
                    let export = book.export();
                    let _ = tx.try_send(export);
                }
                let (bbo_bid, bbo_ask) = book.bbo();
                info!(
                    "pkts={} msgs={} live_orders={} bbo_bid={:?} bbo_ask={:?}",
                    processed_pkts,
                    processed_msgs,
                    book.order_count(),
                    bbo_bid, bbo_ask
                );
                last_snap = Instant::now();
            }
        } else {
            crate::util::adaptive_wait(&mut idle_iters, 64);
        }
    }
    Ok(())
}