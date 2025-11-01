use std::time::Instant;

// Pull orderbook directly into this bench to avoid compiling the full binary graph
#[path = "../orderbook.rs"]
mod orderbook;

// Minimal parser types to satisfy orderbook interfaces
mod parser {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Side { Bid, Ask }

    #[derive(Debug, Clone)]
    pub enum Event {
        Add { order_id: u64, instr: u32, px: i64, qty: i64, side: Side },
        Mod { order_id: u64, qty: i64 },
        Del { order_id: u64 },
        Trade { instr: u32, px: i64, qty: i64, maker_order_id: Option<u64>, taker_side: Option<Side> },
        Heartbeat,
    }
}

use crate::orderbook::OrderBook;
use crate::parser::{Event, Side};

fn parse_arg_usize(args: &[String], idx: usize, default: usize) -> usize {
    args.get(idx).and_then(|s| s.parse::<usize>().ok()).unwrap_or(default)
}

fn main() {
    // Args: [instr_count] [orders_per_instr] [batch_size]
    let args: Vec<String> = std::env::args().collect();
    let instr_count = parse_arg_usize(&args, 1, 32);
    let orders_per_instr = parse_arg_usize(&args, 2, 5000);
    let batch_size = parse_arg_usize(&args, 3, 64);

    let mut book = OrderBook::new(10);

    let start_total = Instant::now();
    let mut total_events: usize = 0;

    // Phase 1: Adds
    let t0 = Instant::now();
    for instr in 0..(instr_count as u32) {
        let mut buf: Vec<Event> = Vec::with_capacity(batch_size);
        for i in 0..orders_per_instr {
            let oid: u64 = ((instr as u64) << 32) | (i as u64);
            let price = 1_000_000i64 + ((i % 200) as i64);
            let qty = 100 + ((i % 50) as i64);
            let side = if (i & 1) == 0 { Side::Bid } else { Side::Ask };
            buf.push(Event::Add { order_id: oid, instr, px: price, qty, side });
            if buf.len() == batch_size { book.apply_many_for_instr(instr, &buf); total_events += buf.len(); buf.clear(); }
        }
        if !buf.is_empty() { book.apply_many_for_instr(instr, &buf); total_events += buf.len(); }
    }
    let adds_dur = t0.elapsed();

    // Phase 2: Mods (about half of orders)
    let t1 = Instant::now();
    for instr in 0..(instr_count as u32) {
        let mut x: u64 = 0x9E3779B97F4A7C15; // simple xorshift64* state
        let mut buf: Vec<Event> = Vec::with_capacity(batch_size);
        let mods = orders_per_instr / 2;
        for _ in 0..mods {
            // xorshift64* to pick an order index uniformly
            x ^= x >> 12; x ^= x << 25; x ^= x >> 27; x = x.wrapping_mul(0x2545F4914F6CDD1D);
            let i = (x as usize) % orders_per_instr;
            let oid: u64 = ((instr as u64) << 32) | (i as u64);
            // New qty in [1, 200]
            let new_qty = 1 + (((x as i64) & 0x7F)) + 72;
            buf.push(Event::Mod { order_id: oid, qty: new_qty });
            if buf.len() == batch_size { book.apply_many_for_instr(instr, &buf); total_events += buf.len(); buf.clear(); }
        }
        if !buf.is_empty() { book.apply_many_for_instr(instr, &buf); total_events += buf.len(); }
    }
    let mods_dur = t1.elapsed();

    // Phase 3: Cancels every 3rd order
    let t2 = Instant::now();
    for instr in 0..(instr_count as u32) {
        let mut buf: Vec<Event> = Vec::with_capacity(batch_size);
        for i in (0..orders_per_instr).step_by(3) {
            let oid: u64 = ((instr as u64) << 32) | (i as u64);
            buf.push(Event::Del { order_id: oid });
            if buf.len() == batch_size { book.apply_many_for_instr(instr, &buf); total_events += buf.len(); buf.clear(); }
        }
        if !buf.is_empty() { book.apply_many_for_instr(instr, &buf); total_events += buf.len(); }
    }
    let dels_dur = t2.elapsed();

    // Touch BBO to ensure hot-path remains O(1)
    let _ = book.bbo();

    let total_dur = start_total.elapsed();

    println!(
        "bench_orderbook: instr={} orders/instr={} batch={} total_events={} total_time_ms={:.3} adds_ms={:.3} mods_ms={:.3} dels_ms={:.3} throughput_meps={:.3}",
        instr_count,
        orders_per_instr,
        batch_size,
        total_events,
        total_dur.as_secs_f64() * 1000.0,
        adds_dur.as_secs_f64() * 1000.0,
        mods_dur.as_secs_f64() * 1000.0,
        dels_dur.as_secs_f64() * 1000.0,
        (total_events as f64) / 1_000_000.0 / total_dur.as_secs_f64(),
    );
}


