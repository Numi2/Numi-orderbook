// src/metrics.rs
use crossbeam_channel::Sender;
use hashbrown::HashMap;
use once_cell::sync::Lazy;
use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
};
use std::net::ToSocketAddrs;
use std::sync::Mutex;
use std::thread;

static REGISTRY: Lazy<Registry> = Lazy::new(Registry::new);

static RX_PACKETS: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new("rx_packets", "Packets received per channel"),
        &["chan"],
    )
    .expect("rx_packets");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RX_BYTES: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new("rx_bytes", "Bytes received per channel"),
        &["chan"],
    )
    .expect("rx_bytes");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RX_DROPS: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new("rx_drops", "Dropped packets due to backpressure"),
        &["chan"],
    )
    .expect("rx_drops");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static PACKET_POOL_MISSES: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "packet_pool_misses_total",
        "Runtime packet buffer allocations because the preallocated pool was empty",
    )
    .expect("packet_pool_misses_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static PACKET_POOL_RETURN_DROPS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "packet_pool_return_drops_total",
        "Returned packet buffers dropped because the preallocated pool was full",
    )
    .expect("packet_pool_return_drops_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static PACKET_POOL_PREALLOC_BYTES: Lazy<IntGauge> = Lazy::new(|| {
    let g = IntGauge::new(
        "packet_pool_preallocated_bytes",
        "Bytes reserved and page-touched during packet pool startup",
    )
    .expect("packet_pool_preallocated_bytes");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
});

static MERGE_DUPS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new("merge_duplicates", "Duplicate packets filtered by merge")
        .expect("merge_duplicates");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static MERGE_GAPS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "merge_gaps",
        "Gaps detected by merge (out-of-band recovery advisable)",
    )
    .expect("merge_gaps");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static MERGE_OOO: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "merge_out_of_order",
        "Out-of-order packets buffered within reorder window",
    )
    .expect("merge_out_of_order");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

// Per-channel merge forwards and gaps
static MERGE_FORWARD_BY_CHAN: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new(
            "merge_forward_packets",
            "Packets forwarded by merge per channel",
        ),
        &["chan"],
    )
    .expect("merge_forward_packets");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static MERGE_GAPS_BY_CHAN: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new("merge_gaps_by_chan", "Gaps signaled by merge per channel"),
        &["chan"],
    )
    .expect("merge_gaps_by_chan");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static MERGE_FAILOVERS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "merge_failovers",
        "Number of preferred-channel switches due to hysteresis",
    )
    .expect("merge_failovers");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static MERGE_PREFERRED_IS_A: Lazy<IntGauge> = Lazy::new(|| {
    let g = IntGauge::new(
        "merge_preferred_is_a",
        "1 if channel A is currently preferred, else 0",
    )
    .expect("merge_preferred_is_a");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
});

static RECOVERY_REQUESTS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new("recovery_requests_total", "Recovery ranges requested")
        .expect("recovery_requests_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RECOVERY_RETRIES: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "recovery_retries_total",
        "Recovery replay retry attempts after fetch failures",
    )
    .expect("recovery_retries_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RECOVERY_DROPPED_REQUESTS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "recovery_dropped_requests_total",
        "Recovery requests dropped because the request queue was full or disconnected",
    )
    .expect("recovery_dropped_requests_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RECOVERY_FAILURES: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "recovery_failures_total",
        "Recovery ranges that exhausted retry attempts",
    )
    .expect("recovery_failures_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RECOVERY_UNRECOVERABLE_GAPS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "recovery_unrecoverable_gaps_total",
        "Recovery ranges escalated as unrecoverable",
    )
    .expect("recovery_unrecoverable_gaps_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RECOVERY_FETCHED: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "recovery_fetched_ranges_total",
        "Recovery ranges fetched successfully",
    )
    .expect("recovery_fetched_ranges_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RECOVERY_INJECTED_PACKETS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "recovery_injected_packets_total",
        "Replay packets injected into the merge recovery queue",
    )
    .expect("recovery_injected_packets_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RECOVERY_STALE_PACKETS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "recovery_stale_packets_total",
        "Replay packets rejected because their sequence is outside the requested range",
    )
    .expect("recovery_stale_packets_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RECOVERY_SLO_VIOLATIONS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "recovery_slo_violations_total",
        "Recovery ranges that exceeded the configured SLO",
    )
    .expect("recovery_slo_violations_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static RECOVERY_RANGE_LATENCY: Lazy<Histogram> = Lazy::new(|| {
    let buckets = vec![1e-4, 5e-4, 1e-3, 5e-3, 1e-2, 5e-2, 1e-1, 5e-1, 1.0, 5.0];
    let h = Histogram::with_opts(
        HistogramOpts::new(
            "recovery_range_seconds",
            "Time from coalesced recovery request to fetched or failed status",
        )
        .buckets(buckets),
    )
    .expect("recovery_range_seconds");
    REGISTRY.register(Box::new(h.clone())).ok();
    h
});

static DECODE_PKTS: Lazy<IntCounter> = Lazy::new(|| {
    let c =
        IntCounter::new("decode_packets", "Packets processed by decoder").expect("decode_packets");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static DECODE_MSGS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new("decode_messages", "Messages decoded from packets")
        .expect("decode_messages");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static DECODE_EVENT_VEC_REALLOCS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "decode_event_vec_reallocs_total",
        "Decode event vector capacity growth events",
    )
    .expect("decode_event_vec_reallocs_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static BOOK_LIVE_ORDERS: Lazy<IntGauge> = Lazy::new(|| {
    let g = IntGauge::new(
        "book_live_orders",
        "Number of live orders across all instruments",
    )
    .expect("book_live_orders");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
});

static ORDERBOOK_SLAB_GROWS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "orderbook_slab_grows_total",
        "Order-book slab capacity growth events",
    )
    .expect("orderbook_slab_grows_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static ORDERBOOK_DEPTH_VEC_GROWS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "orderbook_depth_vec_grows_total",
        "Cold-path depth assembly vector capacity growth events",
    )
    .expect("orderbook_depth_vec_grows_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static ORDERBOOK_EXPORT_VEC_GROWS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "orderbook_export_vec_grows_total",
        "Snapshot export vector capacity growth events",
    )
    .expect("orderbook_export_vec_grows_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static SNAPSHOT_PAYLOAD_VEC_GROWS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "snapshot_payload_vec_grows_total",
        "Snapshot writer payload vector capacity growth events",
    )
    .expect("snapshot_payload_vec_grows_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static SNAPSHOT_PAYLOAD_BYTES: Lazy<IntGauge> = Lazy::new(|| {
    let g = IntGauge::new(
        "snapshot_payload_bytes",
        "Most recent serialized snapshot payload size in bytes",
    )
    .expect("snapshot_payload_bytes");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
});

static E2E_LATENCY: Lazy<Histogram> = Lazy::new(|| {
    // Buckets in seconds: 100ns .. 10ms
    let buckets = vec![1e-7, 2e-7, 5e-7, 1e-6, 2e-6, 5e-6, 1e-5, 2e-5, 5e-5, 1e-4];
    let h = Histogram::with_opts(
        HistogramOpts::new("e2e_latency_seconds", "End-to-end packet latency").buckets(buckets),
    )
    .expect("e2e_latency");
    REGISTRY.register(Box::new(h.clone())).ok();
    h
});

// Optional per-timestamp-source latency histograms for deeper analysis
static E2E_LATENCY_SW: Lazy<Histogram> = Lazy::new(|| {
    let buckets = vec![1e-7, 2e-7, 5e-7, 1e-6, 2e-6, 5e-6, 1e-5, 2e-5, 5e-5, 1e-4];
    let h = Histogram::with_opts(
        HistogramOpts::new(
            "e2e_latency_seconds_sw",
            "E2E latency (software timestamps)",
        )
        .buckets(buckets),
    )
    .expect("e2e_latency_sw");
    REGISTRY.register(Box::new(h.clone())).ok();
    h
});

static E2E_LATENCY_SYS: Lazy<Histogram> = Lazy::new(|| {
    let buckets = vec![1e-7, 2e-7, 5e-7, 1e-6, 2e-6, 5e-6, 1e-5, 2e-5, 5e-5, 1e-4];
    let h = Histogram::with_opts(
        HistogramOpts::new(
            "e2e_latency_seconds_hw_sys",
            "E2E latency (system hardware timestamps)",
        )
        .buckets(buckets),
    )
    .expect("e2e_latency_hw_sys");
    REGISTRY.register(Box::new(h.clone())).ok();
    h
});

static E2E_LATENCY_RAW: Lazy<Histogram> = Lazy::new(|| {
    let buckets = vec![1e-7, 2e-7, 5e-7, 1e-6, 2e-6, 5e-6, 1e-5, 2e-5, 5e-5, 1e-4];
    let h = Histogram::with_opts(
        HistogramOpts::new(
            "e2e_latency_seconds_hw_raw",
            "E2E latency (raw hardware timestamps)",
        )
        .buckets(buckets),
    )
    .expect("e2e_latency_hw_raw");
    REGISTRY.register(Box::new(h.clone())).ok();
    h
});

// SummaryVec is not available in our prometheus version; keep histograms only

static STAGE_RX_TO_MERGE: Lazy<Histogram> = Lazy::new(|| {
    let buckets = vec![1e-7, 2e-7, 5e-7, 1e-6, 2e-6, 5e-6, 1e-5, 2e-5];
    let h = Histogram::with_opts(
        HistogramOpts::new(
            "stage_rx_to_merge_seconds",
            "RX to merge forwarding latency",
        )
        .buckets(buckets),
    )
    .expect("stage_rx_to_merge");
    REGISTRY.register(Box::new(h.clone())).ok();
    h
});

static STAGE_MERGE_TO_DECODE: Lazy<Histogram> = Lazy::new(|| {
    let buckets = vec![1e-7, 2e-7, 5e-7, 1e-6, 2e-6, 5e-6, 1e-5, 2e-5];
    let h = Histogram::with_opts(
        HistogramOpts::new(
            "stage_merge_to_decode_seconds",
            "Merge to decode dequeue latency",
        )
        .buckets(buckets),
    )
    .expect("stage_merge_to_decode");
    REGISTRY.register(Box::new(h.clone())).ok();
    h
});

static QUEUE_LEN: Lazy<IntGaugeVec> = Lazy::new(|| {
    let g = IntGaugeVec::new(
        Opts::new("queue_len", "Current length of internal queues"),
        &["queue"],
    )
    .expect("queue_len");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
});

static QUEUE_HWM: Lazy<IntGaugeVec> = Lazy::new(|| {
    let g = IntGaugeVec::new(
        Opts::new("queue_hwm", "High-water mark of internal queues"),
        &["queue"],
    )
    .expect("queue_hwm");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
});

static HWM_TRACK: Lazy<Mutex<HashMap<&'static str, i64>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn inc_rx(chan: &str, bytes: usize) {
    inc_rx_batch(chan, 1, bytes);
}

pub fn inc_rx_batch(chan: &str, packets: usize, bytes: usize) {
    if packets == 0 {
        return;
    }
    RX_PACKETS.with_label_values(&[chan]).inc_by(packets as u64);
    RX_BYTES.with_label_values(&[chan]).inc_by(bytes as u64);
}

pub fn inc_rx_drop(chan: &str) {
    inc_rx_drop_batch(chan, 1);
}

pub fn inc_rx_drop_batch(chan: &str, packets: usize) {
    if packets == 0 {
        return;
    }
    RX_DROPS.with_label_values(&[chan]).inc_by(packets as u64);
}

pub fn inc_packet_pool_miss() {
    PACKET_POOL_MISSES.inc();
}

pub fn inc_packet_pool_return_drop() {
    PACKET_POOL_RETURN_DROPS.inc();
}

pub fn set_packet_pool_preallocated_bytes(n: usize) {
    PACKET_POOL_PREALLOC_BYTES.set(n as i64);
}

pub fn packet_pool_misses() -> u64 {
    PACKET_POOL_MISSES.get()
}

pub fn packet_pool_return_drops() -> u64 {
    PACKET_POOL_RETURN_DROPS.get()
}

pub fn packet_pool_preallocated_bytes() -> i64 {
    PACKET_POOL_PREALLOC_BYTES.get()
}

pub fn inc_merge_dup() {
    MERGE_DUPS.inc();
}
pub fn inc_merge_gap() {
    MERGE_GAPS.inc();
}
pub fn inc_merge_ooo() {
    MERGE_OOO.inc();
}

pub fn inc_merge_forward_chan(chan: &str) {
    MERGE_FORWARD_BY_CHAN.with_label_values(&[chan]).inc();
}
pub fn inc_merge_gap_chan(chan: &str) {
    MERGE_GAPS_BY_CHAN.with_label_values(&[chan]).inc();
}
pub fn inc_merge_failover() {
    MERGE_FAILOVERS.inc();
}
pub fn set_merge_preferred_is_a(is_a: bool) {
    MERGE_PREFERRED_IS_A.set(if is_a { 1 } else { 0 });
}

pub fn inc_recovery_request() {
    RECOVERY_REQUESTS.inc();
}
pub fn inc_recovery_retry() {
    RECOVERY_RETRIES.inc();
}
pub fn inc_recovery_dropped_request() {
    RECOVERY_DROPPED_REQUESTS.inc();
}
pub fn inc_recovery_failure() {
    RECOVERY_FAILURES.inc();
}
pub fn inc_recovery_unrecoverable_gap() {
    RECOVERY_UNRECOVERABLE_GAPS.inc();
}
pub fn inc_recovery_fetched() {
    RECOVERY_FETCHED.inc();
}
pub fn inc_recovery_injected_packets(n: u64) {
    RECOVERY_INJECTED_PACKETS.inc_by(n);
}
pub fn inc_recovery_stale_packet() {
    RECOVERY_STALE_PACKETS.inc();
}
pub fn inc_recovery_slo_violation() {
    RECOVERY_SLO_VIOLATIONS.inc();
}
pub fn observe_recovery_range_ns(ns: u64) {
    let secs = (ns as f64) / 1_000_000_000.0;
    RECOVERY_RANGE_LATENCY.observe(secs);
}

pub fn inc_decode_pkts() {
    DECODE_PKTS.inc();
}
pub fn inc_decode_msgs(n: u64) {
    DECODE_MSGS.inc_by(n);
}

pub fn inc_decode_event_vec_realloc() {
    DECODE_EVENT_VEC_REALLOCS.inc();
}

pub fn set_live_orders(n: usize) {
    BOOK_LIVE_ORDERS.set(n as i64);
}

pub fn inc_orderbook_slab_grow() {
    ORDERBOOK_SLAB_GROWS.inc();
}

pub fn inc_orderbook_depth_vec_grow() {
    ORDERBOOK_DEPTH_VEC_GROWS.inc();
}

pub fn inc_orderbook_export_vec_grow() {
    ORDERBOOK_EXPORT_VEC_GROWS.inc();
}

pub fn inc_snapshot_payload_vec_grow() {
    SNAPSHOT_PAYLOAD_VEC_GROWS.inc();
}

pub fn set_snapshot_payload_bytes(n: usize) {
    SNAPSHOT_PAYLOAD_BYTES.set(n as i64);
}

pub fn observe_latency_ns(ns: u64) {
    let secs = (ns as f64) / 1_000_000_000.0;
    E2E_LATENCY.observe(secs);
}

pub fn observe_latency_by_kind_ns(kind: crate::pool::TsKind, ns: u64) {
    let secs = (ns as f64) / 1_000_000_000.0;
    match kind {
        crate::pool::TsKind::Sw | crate::pool::TsKind::None => E2E_LATENCY_SW.observe(secs),
        crate::pool::TsKind::HwSys => E2E_LATENCY_SYS.observe(secs),
        crate::pool::TsKind::HwRaw => E2E_LATENCY_RAW.observe(secs),
    }
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

// removed

pub fn set_queue_len(queue: &'static str, len: usize) {
    QUEUE_LEN.with_label_values(&[queue]).set(len as i64);
    let mut hwm = HWM_TRACK.lock().unwrap();
    let e = hwm.entry(queue).or_insert(0);
    if *e < len as i64 {
        *e = len as i64;
        QUEUE_HWM.with_label_values(&[queue]).set(*e);
    }
}

// Outbound WebSocket feed -----

static WS_CLIENTS: Lazy<IntGauge> = Lazy::new(|| {
    let g =
        IntGauge::new("ws_clients", "Number of connected websocket clients").expect("ws_clients");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
});

static OUT_FRAMES: Lazy<IntCounter> = Lazy::new(|| {
    let c =
        IntCounter::new("out_frames_total", "Frames sent to clients").expect("out_frames_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static OUT_BYTES: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new("out_bytes_total", "Bytes sent to clients").expect("out_bytes_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

static DROPPED_CLIENTS: Lazy<IntCounter> = Lazy::new(|| {
    let c = IntCounter::new(
        "dropped_clients_total",
        "Clients dropped due to lag, gap, or write failure",
    )
    .expect("dropped_clients_total");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

pub fn inc_ws_clients(delta: i64) {
    WS_CLIENTS.add(delta);
}
pub fn inc_out_frames() {
    OUT_FRAMES.inc();
}
pub fn inc_out_bytes(n: usize) {
    OUT_BYTES.inc_by(n as u64);
}
pub fn inc_dropped_clients() {
    DROPPED_CLIENTS.inc();
}

pub fn spawn_http<A: ToSocketAddrs + Send + 'static>(
    addr: A,
    snapshot_trigger: Option<Sender<()>>,
) -> thread::JoinHandle<()> {
    let addr_string = addr
        .to_socket_addrs()
        .ok()
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
                        .with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"text/plain; version=0.0.4"[..],
                            )
                            .unwrap(),
                        );
                    let _ = req.respond(resp);
                } else if url == "/snapshot" {
                    let ok = snapshot_trigger
                        .as_ref()
                        .map(|tx| tx.try_send(()))
                        .is_some();
                    let status = if ok { 202 } else { 503 };
                    let _ = req.respond(tiny_http::Response::empty(status));
                } else if url == "/live" || url == "/healthz" {
                    let _ =
                        req.respond(tiny_http::Response::from_string("OK").with_status_code(200));
                } else if url == "/ready" {
                    // Minimal readiness: server up and metrics registry available
                    let _ = req
                        .respond(tiny_http::Response::from_string("READY").with_status_code(200));
                } else if url == "/shutdown" {
                    let _ =
                        req.respond(tiny_http::Response::from_string("BYE").with_status_code(200));
                    break;
                } else {
                    let _ = req.respond(tiny_http::Response::empty(404));
                }
            }
        }
    })
}
