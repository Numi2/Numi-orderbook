// src/metrics.rs
use once_cell::sync::Lazy;
use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Opts, Registry, TextEncoder,
};
use std::net::ToSocketAddrs;
use std::thread;

static REGISTRY: Lazy<Registry> = Lazy::new(Registry::new);

static RX_PACKETS: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(Opts::new("rx_packets", "Packets received per channel"), &["chan"])
        .expect("rx_packets");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RX_BYTES: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(Opts::new("rx_bytes", "Bytes received per channel"), &["chan"])
        .expect("rx_bytes");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RX_DROPS: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(Opts::new("rx_drops", "Dropped packets due to backpressure"), &["chan"])
        .expect("rx_drops");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static MERGE_DUPS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new("merge_duplicates", "Duplicate packets filtered by merge").expect("merge_duplicates");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static MERGE_GAPS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new("merge_gaps", "Gaps detected by merge (out-of-band recovery advisable)")
        .expect("merge_gaps");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static MERGE_OOO: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new("merge_out_of_order", "Out-of-order packets buffered within reorder window")
        .expect("merge_out_of_order");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static DECODE_PKTS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new("decode_packets", "Packets processed by decoder").expect("decode_packets");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static DECODE_MSGS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new("decode_messages", "Messages decoded from packets").expect("decode_messages");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static BOOK_LIVE_ORDERS: Lazy<IntGauge> = Lazy::new(|| {
    let g = IntGauge::new("book_live_orders", "Number of live orders across all instruments").expect("book_live_orders");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
});

static E2E_LATENCY: Lazy<Histogram> = Lazy::new(|| {
    // Buckets in seconds: 100ns .. 10ms
    let buckets = vec![
        1e-7, 2e-7, 5e-7,
        1e-6, 2e-6, 5e-6,
        1e-5, 2e-5, 5e-5,
        1e-4,
    ];
    let h = Histogram::with_opts(HistogramOpts::new("e2e_latency_seconds", "End-to-end packet latency")
        .buckets(buckets)).expect("e2e_latency");
    REGISTRY.register(Box::new(h.clone())).ok();
    h
});

pub fn inc_rx(chan: &str, bytes: usize) {
    RX_PACKETS.with_label_values(&[chan]).inc();
    RX_BYTES.with_label_values(&[chan]).inc_by(bytes as u64);
}

pub fn inc_rx_drop(chan: &str) {
    RX_DROPS.with_label_values(&[chan]).inc();
}

pub fn inc_merge_dup() { MERGE_DUPS.inc(); }
pub fn inc_merge_gap() { MERGE_GAPS.inc(); }
pub fn inc_merge_ooo() { MERGE_OOO.inc(); }

pub fn inc_decode_pkts() { DECODE_PKTS.inc(); }
pub fn inc_decode_msgs(n: u64) { DECODE_MSGS.inc_by(n); }

pub fn set_live_orders(n: usize) { BOOK_LIVE_ORDERS.set(n as i64); }

pub fn observe_latency_ns(ns: u64) {
    let secs = (ns as f64) / 1_000_000_000.0;
    E2E_LATENCY.observe(secs);
}

pub fn spawn_http<A: ToSocketAddrs + Send + 'static>(addr: A) -> thread::JoinHandle<()> {
    let addr_string = addr.to_socket_addrs().ok()
        .and_then(|mut it| it.next())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "0.0.0.0:9090".to_string());

    thread::spawn(move || {
        let server = tiny_http::Server::http(&addr_string).expect("start metrics http");
        log::info!("prometheus metrics listening on http://{addr_string}/metrics");
        let encoder = TextEncoder::new();
        loop {
            if let Ok(req) = server.recv() {
                let url = req.url().to_string();
                if url == "/metrics" {
                    let metric_families = REGISTRY.gather();
                    let mut buf = Vec::with_capacity(16 * 1024);
                    encoder.encode(&metric_families, &mut buf).ok();
                    let resp = tiny_http::Response::from_data(buf)
                        .with_status_code(200)
                        .with_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain; version=0.0.4"[..]).unwrap());
                    let _ = req.respond(resp);
                } else {
                    let _ = req.respond(tiny_http::Response::empty(404));
                }
            }
        }
    })
}