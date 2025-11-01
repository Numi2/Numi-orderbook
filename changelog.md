# Changelog

All notable changes to this project will be documented in this file.
This project loosely follows the Keep a Changelog format.

- 2025-11-01

### Added
- Public, binary WebSocket OBO feed (Order-by-Order L3) using raw structs + zerocopy
  - New modules: `src/codec_raw.rs` (wire header/payloads), `src/obo.rs` (event mapping)
- High-throughput publish/subscribe bus with per-instrument sequencing
  - New module: `src/pubsub.rs`
- Two independent WebSocket endpoints per POP (A/B) with identical payloads
  - New module: `src/ws_server.rs`
  - Snapshot-on-connect support (SNAPSHOT_START/HDR/END)
  - Optional Bearer token auth
- HTTP/3 (QUIC) bytestream A/B endpoints with identical frames
  - New module: `src/h3_server.rs` (feature-gated; not enabled by default)
- Config extensions for feeds/POPs/TLS/auth and OBO buffers
  - Schema: `feeds`, `feeds.pops[*].ws_endpoints`, `feeds.pops[*].h3_endpoints`, `feeds.tls`, `feeds.auth_token`, `feeds.obo.buffers`
- Outbound metrics
  - `ws_clients`, `out_frames_total`, `out_bytes_total`, `dropped_clients_total`
- Example clients
  - `src/bin/ws_client.rs` (dual-endpoint first-arrival dedupe demo)
- Documentation
  - `docs/obo_raw_v1.md` (wire format + API)
  - `readme.md` updated with feed overview

### Added (performance & tooling)
- OrderBook batch APIs: `apply_many(&[Event])` and `apply_many_for_instr(instr, &[Event])` to amortize lookups and reuse hot structures.
- Micro-benchmark binary: `src/bin/bench_orderbook.rs` to measure OrderBook adds/mods/dels throughput.
- Minimal ingest runner: `src/bin/ingest_min.rs` (RX → merge → metrics) to facilitate end-to-end latency testing without publishers.

### Changed
- `decode.rs`: maps internal `Event`s to OBO events and publishes frames via the bus
  - Correct instrument resolution for Mod/Del/Trade using `OrderBook::instrument_for_order`
- `orderbook.rs`: added `instrument_for_order(order_id)` accessor
- `pubsub.rs`: GAP reporting now includes `from/to` range
- `ws_server.rs`: sends GAP control with range payload; improved Authorization parsing
- `metrics.rs`: added outbound counters/gauges
- `main.rs`: wired publishers and A/B endpoints; HTTP/3 endpoints are feature-gated
- `cargo.toml`: added dependencies (`tungstenite`, `url`, `http`, optional `quinn`/`rustls`/`h3(-quinn)`), introduced feature flags (`ws`, `obo`, optional `h3`)

### Changed (low-latency optimizations)
- RX path switched to single-producer/single-consumer queues: migrated from `crossbeam::ArrayQueue` to an internal `SpscQueue` for strictly 1P/1C paths; updated `main.rs` wiring to per-worker SPSC lists.
- OrderBook hot path:
  - Node pointers use `Option<NonZeroUsize>` for tighter layout and fewer cache misses.
  - BBO made O(1) by caching best bid/ask price and aggregate qty; avoided `BTreeMap` traversal.
  - Safe level removal in `cancel` (no long-lived mutable borrows; remove after borrow ends).
  - Preallocation via `InstrumentBook::with_capacity` and default per-instrument slab capacity in `OrderBook`.
- `cargo.toml`: gated HTTP/3 stack under `h3` feature and fixed optional `rustls` gating to unblock lean builds/tests.

### Security
- Optional Bearer token authentication for WebSocket endpoints (if `feeds.auth_token` configured)

### Performance
- OrderBook micro-benchmark (32 instruments × 5k orders/instr, batch=64) on release build achieved ~9.68 M events/sec total across adds/mods/dels.
- Stage metrics exposed for RX→merge and merge→decode latencies; Prometheus histograms for e2e latency.

### Notes
- HTTP/3 server is behind the `h3` feature and not enabled by default.
- Clients should connect to both endpoints per POP and keep the first-arriving frame per `(instrument_id, sequence)`.



### Metrics

- Prometheus endpoint at `/metrics` on `metrics.bind`
- Trigger on-demand snapshot with `GET /snapshot` (returns 202 on success)
- Health endpoints: `/live`, `/ready`, `/healthz`
- Examples: `rx_packets{chan="A"}`, `rx_bytes{chan="B"}`, `merge_gaps`, `decode_messages`, `book_live_orders`, `e2e_latency_seconds`

### Snapshots

- Periodic writer saves atomically to the configured path
- On startup, the snapshot is loaded if present and `load_on_start = true`

### AF_XDP / PACKET_MMAP

On Linux, enabling `[afxdp] enable = true` replaces channel A’s socket RX with a high‑performance mmap’ed packet ring. The code attempts AF_XDP if available and falls back to PACKET_RX_RING (TPACKET_V2) for broad compatibility. Packets are parsed to UDP payload and fed into the pipeline with a single copy into the pool buffer.

