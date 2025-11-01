// src/decode.rs Numan Thabit: experiment to work with the advanced order book + richer events)
use crate::metrics;
use crate::orderbook::OrderBook;
use crate::parser::Parser;
use crate::pool::{PacketPool, Pkt};
use crate::util::{now_nanos, BarrierFlag};
use crossbeam::queue::ArrayQueue;
use crossbeam_channel::Sender;
use crossbeam_channel::Receiver;
use log::info;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn decode_loop(
    q_in: Arc<ArrayQueue<Pkt>>,
    pool: Arc<PacketPool>,
    parser: Parser,
    max_depth: usize,
    snapshot_interval_ms: u64,
    consume_trades: bool,
    shutdown: Arc<BarrierFlag>,
    snapshot_tx: Option<Sender<crate::orderbook::BookExport>>,
    mut initial_book: Option<OrderBook>,
    snapshot_trigger_rx: Option<Receiver<()>>,
) -> anyhow::Result<()> {
    let mut book = initial_book.take().unwrap_or_else(|| OrderBook::new(max_depth));
    book.set_consume_trades(consume_trades);
    // Cache decoder and sizing to avoid per-packet Arc clone and re-evaluations
    let dec = parser.decoder();
    let max_msgs = parser.max_messages_per_packet;
    let mut events = Vec::with_capacity(max_msgs);
    let mut last_snap = Instant::now();
    let snap_every = Duration::from_millis(snapshot_interval_ms);

    let mut processed_pkts: u64 = 0;
    let mut processed_msgs: u64 = 0;

    let mut idle_iters: u32 = 0;
    while !shutdown.is_raised() {
        if let Some(pkt) = q_in.pop() {
            processed_pkts += 1;
            metrics::inc_decode_pkts();

            events.clear();
            // Destructure to move buffer without extra allocation
            let Pkt { buf, len, ts_nanos, merge_emit_ns, .. } = pkt;
            dec.decode_messages(&buf[..len], &mut events);
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
            }

            let now_ns = now_nanos();
            if ts_nanos != 0 && now_ns > ts_nanos {
                metrics::observe_latency_ns(now_ns - ts_nanos);
            }

            // Return buffer to pool without creating a new BytesMut allocation
            pool.put(buf);

            let mut should_snapshot = last_snap.elapsed() >= snap_every;
            if !should_snapshot {
                if let Some(ref rx) = snapshot_trigger_rx {
                    if rx.try_recv().is_ok() { should_snapshot = true; }
                }
            }
            if should_snapshot {
                metrics::set_live_orders(book.order_count());
                if let Some(ref tx) = snapshot_tx {
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