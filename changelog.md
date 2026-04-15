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
- Config extensions for feeds/POPs/auth and OBO buffers
  - Schema: `feeds`, `feeds.pops[*].ws_endpoints`, `feeds.auth_token`, `feeds.obo.buffers`
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
- `main.rs`: wired publishers and A/B WebSocket endpoints
- `cargo.toml`: added dependencies (`tungstenite`, `url`), introduced feature flags (`ws`, `obo`)

### Changed (low-latency optimizations)
- RX path switched to single-producer/single-consumer queues: migrated from `crossbeam::ArrayQueue` to an internal `SpscQueue` for strictly 1P/1C paths; updated `main.rs` wiring to per-worker SPSC lists.
- OrderBook hot path:
  - Node pointers use `Option<NonZeroUsize>` for tighter layout and fewer cache misses.
  - BBO made O(1) by caching best bid/ask price and aggregate qty; avoided `BTreeMap` traversal.
  - Safe level removal in `cancel` (no long-lived mutable borrows; remove after borrow ends).
  - Preallocation via `InstrumentBook::with_capacity` and default per-instrument slab capacity in `OrderBook`.

### Security
- Optional Bearer token authentication for WebSocket endpoints (if `feeds.auth_token` configured)

### Performance
- OrderBook micro-benchmark (32 instruments × 5k orders/instr, batch=64) on release build achieved ~9.68 M events/sec total across adds/mods/dels.
- Stage metrics exposed for RX→merge and merge→decode latencies; Prometheus histograms for e2e latency.

### Notes
- Clients should connect to both WebSocket endpoints per POP and keep the first-arriving live event frame per `(instrument_id, sequence)`.



### Metrics

- Prometheus endpoint at `/metrics` on `metrics.bind`
- Trigger on-demand snapshot with `GET /snapshot` (returns 202 on success)
- Health endpoints: `/live`, `/ready`, `/healthz`
- Examples: `rx_packets{chan="A"}`, `rx_bytes{chan="B"}`, `merge_gaps`, `decode_messages`, `book_live_orders`, `e2e_latency_seconds`

### Snapshots

- Periodic writer saves atomically to the configured path
- On startup, the snapshot is loaded if present and `load_on_start = true`

### PACKET_MMAP

On Linux, enabling `[packet_mmap] enable = true` replaces channel A’s socket RX with a high-performance mmap'ed packet ring using PACKET_RX_RING (TPACKET_V2). Packets are parsed to UDP payload and fed into the pipeline with a single copy into the pool buffer. `[afxdp]` is reserved for a future real AF_XDP/XSK implementation.
The receive module is named `rx_packet_mmap` to keep this fallback separate from the future AF_XDP/XSK path.

### Recovery Lifecycle

The TCP replay injector now coalesces drained gap requests, persists requested/fetched/failed/SLO/unrecoverable status lines, retries failed fetches using `recovery.retry_attempts` and `recovery.retry_backoff_ms`, throttles replay fetch attempts with `recovery.min_request_interval_ms`, applies TCP request timeouts with `recovery.request_timeout_ms`, routes wire handling through `recovery.replay_protocol`, escalates exhausted ranges through `recovery.unrecoverable_policy`, rejects stale replay packets outside the requested sequence range, and exports recovery request, dropped-request, retry, failure, unrecoverable-gap, fetched-range, injected-packet, stale-packet, range-latency, and SLO-violation metrics.

### Latency Baseline

Packet pool startup now page-touches every preallocated packet buffer and exports `packet_pool_preallocated_bytes`, `packet_pool_misses_total`, and `packet_pool_return_drops_total` so hot-path allocation regressions are visible. Decode event-vector growth, order-book slab growth, cold-path depth assembly growth, snapshot export growth, and snapshot payload growth are also counted. The `pool_soak` utility exercises packet-pool sizing and fails by default if runtime allocation or return drops occur.

### Deterministic Book State

Snapshot and depth exports now traverse instruments in sorted order, and `OrderBook::state_hash()` provides a deterministic hash of per-order book state for replay verification.
The order book can now be constructed with a per-instrument tick table, and config exposes `book.default_tick`, `book.grid_span`, `book.order_slab_capacity`, and `book.instrument_ticks`.
Framed journal records now capture packet sequence, per-packet event index, event payload, and optional post-event state hash; replay verification rebuilds an `OrderBook`, compares the final `state_hash()`, and flags non-monotonic sequence records. Config can enable append-only live journal writing from the decode thread, with flushing at snapshot cadence and on shutdown, and framed replay can stream from a reader without loading the full session into memory. Restart verification can now anchor a loaded snapshot to a recorded journal hash and replay only the continuation records.
Venue reference-data tick tables can now be loaded from CSV via `book.instrument_ticks_path`, with aliases for common instrument-id and tick-size column names. Inline `book.instrument_ticks` entries are applied after file loading so operators can override individual instruments.
The EOBI/SBE-style decoder now dispatches through static schema descriptors that define supported template ids, minimum block lengths, and expected schema/version. Wrong schema/version and undersized blocks are rejected before field decoding.

### AF_XDP Guardrail

`afxdp.enable` now fails validation unless a real AF_XDP/XSK backend is integrated. Config rejects enabling AF_XDP and PACKET_MMAP at the same time.
Operations docs now include AF_XDP queue steering, NUMA locality, and queue ownership guidance.

### PACKET_MMAP Hardening

The PACKET_MMAP receiver now owns sockets and mmap rings with RAII cleanup, treats PACKET_FANOUT setup failure as fatal, uses a stable fanout group per interface/channel so workers join the same fanout set, and validates TPACKET frame offsets before reading packet bytes.
Removed the unused UMEM packet-buffer variant so the hot-path packet type contains only implemented buffer ownership modes.
PACKET_MMAP ring geometry is now configurable and validated, channel-A UDP sockets are no longer opened when PACKET_MMAP owns channel A, and UDP payload extraction now honors IPv4 total length plus UDP length instead of returning packet padding.
Fragmented IPv4 UDP frames are rejected in the PACKET_MMAP parser because market-data UDP payloads must arrive complete.

### Client Distribution Hardening

Removed the experimental feature-gated HTTP/3 endpoint and its unused TLS/QUIC config surface; WebSocket raw-v1 is the production client transport until another distribution path is implemented to the same semantics and observability bar.
WebSocket clients now subscribe to the live bus before optional snapshot serialization, preserving frames produced while the snapshot is being sent.
Snapshot-on-connect now fails the connection when snapshot data is unavailable instead of silently downgrading to live-only delivery.
Snapshot files now carry the global replay cursor that immediately follows the image; snapshot-on-connect streams from that cursor and rejects legacy snapshots without cursor metadata.
Snapshot-on-connect rejects snapshot files whose embedded replay cursor has already fallen outside the retained live bus window, avoiding an unreplayable image followed by an immediate gap.
Snapshot writes now surface temporary-file and directory sync failures instead of ignoring durability errors.
`general.mlock_all = true` now fails fast when RLIMIT or `mlockall` cannot be applied, while `false` remains a no-op.
WebSocket feed sockets enable TCP_NODELAY by default and use configurable per-client handshake and write timeouts so slow or stalled clients are dropped instead of blocking a feed thread.
Each WebSocket A/B endpoint pair now enforces a configurable connection cap before spawning client handler threads.
Idle WebSocket sessions now emit configurable `HEARTBEAT` control frames using timeout-capable pubsub receives.
Outbound WebSocket frame and byte counters now share the common send path, so snapshot, live, gap, and heartbeat frames are accounted consistently after successful writes.
`ws_clients` now counts established authenticated sessions only; rejected handshakes are counted as drops instead of live clients.
`dropped_clients_total` now covers handshake failures, authorization failures, and write failures in addition to replay gaps.
Pubsub cursor regression tests cover snapshot-before-live delivery, evicted cursor gap reporting, timeout receive behavior, and per-instrument sequence monotonicity.
WebSocket query parsing regressions cover reconnect cursors and snapshot flags.
The example WebSocket client now reads raw-v1 header offsets correctly, validates frame payload length, and applies dedupe only to live OBO event frames. The wire-format notes now list only implemented control message types.
WebSocket request parsing now rejects invalid `from_seq`, invalid snapshot flags, unsupported channel/codec values, unsupported symbol filters, and unknown or duplicate query parameters instead of silently serving a different stream than requested.
WebSocket request parsing rejects `snapshot=1` combined with `from_seq`; snapshot replay uses the cursor embedded in the snapshot image.
Raw-v1 frames now carry `global_sequence` in the header for live OBO events, giving clients an exact bus replay cursor for reconnects via `from_seq = last_global_sequence + 1`.
Feed config parsing now rejects unknown feed, POP, OBO, and OBO buffer fields so removed H3/TLS keys and misspelled live-feed settings fail fast.
Pubsub frame assembly now performs allocation and payload copy before taking the ring mutex; the lock only assigns the global cursor, writes the fixed header, and pushes into the ring.
