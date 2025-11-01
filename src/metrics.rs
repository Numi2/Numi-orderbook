// src/metrics.rs
use once_cell::sync::Lazy;
use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder,
};
use std::net::ToSocketAddrs;
use std::thread;
use crossbeam_channel::Sender;
use std::sync::Mutex;
use hashbrown::HashMap;

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

// Per-channel merge forwards and gaps
static MERGE_FORWARD_BY_CHAN: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(Opts::new("merge_forward_packets", "Packets forwarded by merge per channel"), &["chan"]) 
        .expect("merge_forward_packets");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static MERGE_GAPS_BY_CHAN: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(Opts::new("merge_gaps_by_chan", "Gaps signaled by merge per channel"), &["chan"]) 
        .expect("merge_gaps_by_chan");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static MERGE_FAILOVERS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new("merge_failovers", "Number of preferred-channel switches due to hysteresis")
        .expect("merge_failovers");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static MERGE_PREFERRED_IS_A: Lazy<IntGauge> = Lazy::new(|| {
    let g = IntGauge::new("merge_preferred_is_a", "1 if channel A is currently preferred, else 0").expect("merge_preferred_is_a");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
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

// SummaryVec is not available in our prometheus version; keep histograms only

static STAGE_RX_TO_MERGE: Lazy<Histogram> = Lazy::new(|| {
    let buckets = vec![1e-7, 2e-7, 5e-7, 1e-6, 2e-6, 5e-6, 1e-5, 2e-5];
    let h = Histogram::with_opts(HistogramOpts::new("stage_rx_to_merge_seconds", "RX to merge forwarding latency").buckets(buckets)).expect("stage_rx_to_merge");
    REGISTRY.register(Box::new(h.clone())).ok();
    h
});

static STAGE_MERGE_TO_DECODE: Lazy<Histogram> = Lazy::new(|| {
    let buckets = vec![1e-7, 2e-7, 5e-7, 1e-6, 2e-6, 5e-6, 1e-5, 2e-5];
    let h = Histogram::with_opts(HistogramOpts::new("stage_merge_to_decode_seconds", "Merge to decode dequeue latency").buckets(buckets)).expect("stage_merge_to_decode");
    REGISTRY.register(Box::new(h.clone())).ok();
    h
});

static STAGE_DECODE_APPLY: Lazy<Histogram> = Lazy::new(|| {
    let buckets = vec![1e-7, 2e-7, 5e-7, 1e-6, 2e-6, 5e-6, 1e-5, 2e-5, 5e-5, 1e-4];
    let h = Histogram::with_opts(HistogramOpts::new("stage_decode_apply_seconds", "Decode and apply time per packet").buckets(buckets)).expect("stage_decode_apply");
    REGISTRY.register(Box::new(h.clone())).ok();
    h
});

static QUEUE_LEN: Lazy<IntGaugeVec> = Lazy::new(|| {
    let g = IntGaugeVec::new(Opts::new("queue_len", "Current length of internal queues"), &["queue"]).expect("queue_len");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
});

static QUEUE_HWM: Lazy<IntGaugeVec> = Lazy::new(|| {
    let g = IntGaugeVec::new(Opts::new("queue_hwm", "High-water mark of internal queues"), &["queue"]).expect("queue_hwm");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
});

static HWM_TRACK: Lazy<Mutex<HashMap<&'static str, i64>>> = Lazy::new(|| Mutex::new(HashMap::new()));

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

pub fn inc_merge_forward_chan(chan: &str) { MERGE_FORWARD_BY_CHAN.with_label_values(&[chan]).inc(); }
pub fn inc_merge_gap_chan(chan: &str) { MERGE_GAPS_BY_CHAN.with_label_values(&[chan]).inc(); }
pub fn inc_merge_failover() { MERGE_FAILOVERS.inc(); }
pub fn set_merge_preferred_is_a(is_a: bool) { MERGE_PREFERRED_IS_A.set(if is_a { 1 } else { 0 }); }

pub fn inc_decode_pkts() { DECODE_PKTS.inc(); }
pub fn inc_decode_msgs(n: u64) { DECODE_MSGS.inc_by(n); }

pub fn set_live_orders(n: usize) { BOOK_LIVE_ORDERS.set(n as i64); }

pub fn observe_latency_ns(ns: u64) {
    let secs = (ns as f64) / 1_000_000_000.0;
    E2E_LATENCY.observe(secs);
}

// pub fn observe_e2e_by_ts_ns(ns: u64, ts_kind: &str) { /* removed */ }

pub fn observe_stage_rx_to_merge_ns(ns: u64) {
    let secs = (ns as f64) / 1_000_000_000.0;
    STAGE_RX_TO_MERGE.observe(secs);
}

pub fn observe_stage_merge_to_decode_ns(ns: u64) {
    let secs = (ns as f64) / 1_000_000_000.0;
    STAGE_MERGE_TO_DECODE.observe(secs);
}

pub fn observe_stage_decode_apply_ns(ns: u64) {
    let secs = (ns as f64) / 1_000_000_000.0;
    STAGE_DECODE_APPLY.observe(secs);
}

pub fn set_queue_len(queue: &'static str, len: usize) {
    QUEUE_LEN.with_label_values(&[queue]).set(len as i64);
    let mut hwm = HWM_TRACK.lock().unwrap();
    let e = hwm.entry(queue).or_insert(0);
    if *e < len as i64 {
        *e = len as i64;
        QUEUE_HWM.with_label_values(&[queue]).set(*e);
    }
}

pub fn spawn_http<A: ToSocketAddrs + Send + 'static>(addr: A, snapshot_trigger: Option<Sender<()>>) -> thread::JoinHandle<()> {
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
                } else if url == "/snapshot" {
                    let ok = snapshot_trigger.as_ref().map(|tx| tx.try_send(())).is_some();
                    let status = if ok { 202 } else { 503 };
                    let _ = req.respond(tiny_http::Response::empty(status));
                } else if url == "/live" || url == "/healthz" {
                    let _ = req.respond(tiny_http::Response::from_string("OK").with_status_code(200));
                } else if url == "/ready" {
                    // Minimal readiness: server up and metrics registry available
                    let _ = req.respond(tiny_http::Response::from_string("READY").with_status_code(200));
                } else {
                    let _ = req.respond(tiny_http::Response::empty(404));
                }
            }
        }
    })
}