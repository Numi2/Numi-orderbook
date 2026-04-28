# Numi Orderbook

Numi Orderbook is a Rust market-data engine for low-latency L3 pipelines. It
receives redundant multicast feeds, merges packets by sequence, decodes venue
wire formats, applies events to a deterministic price-time order book, and
publishes raw order-by-order updates to downstream clients.

The production path is built around predictable hot-path behavior: preallocated
packet buffers, bounded queues, explicit packet ownership, fail-fast socket
configuration, deterministic replay checks, and Prometheus-visible operational
state.

## Contents

- [Capabilities](#capabilities)
- [Architecture](#architecture)
- [Platform support](#platform-support)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Runtime interfaces](#runtime-interfaces)
- [Participant insight signals](#participant-insight-signals)
- [Validation](#validation)
- [Operations](#operations)
- [Repository layout](#repository-layout)
- [Documentation](#documentation)
- [License](#license)

## Capabilities

- Dual A/B multicast ingestion with sequence merge, duplicate suppression, gap
  detection, and recovery injection.
- Linux UDP receive with `recvmmsg` batching, `SO_TIMESTAMPNS`,
  `SO_TIMESTAMPING`, busy polling, realtime scheduling hooks, and optional
  PACKET_MMAP ingest.
- macOS local receive profile using Darwin `recvmsg_x` batching and
  `SO_TIMESTAMP_MONOTONIC` software receive timestamps.
- Deterministic full-depth price-time order book with per-instrument tick
  configuration, stable state hashing, snapshots, and journal replay
  verification.
- Generated Deutsche Boerse T7 14.1 EOBI layout descriptors plus ITCH 5.0 and
  FAST-like decoder surfaces.
- Raw-v1 WebSocket order-by-order feed with snapshot-on-connect, reconnect
  cursors, heartbeat frames, connection caps, TCP_NODELAY, and slow-client
  eviction.
- Prometheus metrics, health endpoints, packet-pool accounting, recovery SLO
  accounting, and local/target benchmark binaries.
- Sidecar and deterministic replay tooling for absorption,
  iceberg/replenishment-candidate, and liquidity-pull participant insight
  signals.

## Architecture

```text
Multicast A/B
    -> RX workers
    -> sequence merge and gap detection
    -> decoder and journal
    -> price-time order book
    -> snapshots, metrics, and raw-v1 publication
    -> WebSocket clients
```

Recovery packets enter through a dedicated queue and pass through the same merge
path as live packets. PACKET_MMAP can own channel A when enabled. AF_XDP is
reserved for a future real XSK/UMEM implementation and is rejected by config
validation today.

## Platform Support

Production receive performance must be validated on the target Linux host with
the target NIC, driver, kernel, timestamp source, CPU isolation, and clock-sync
configuration.

Linux production features:

- UDP `recvmmsg` batching.
- Software, hardware, and raw-hardware RX timestamp parsing.
- `SO_BUSY_POLL`, `mlockall`, realtime scheduling, and CPU pinning.
- PACKET_MMAP / `PACKET_RX_RING` receive path.
- Host-network Docker deployment with explicit Linux capabilities.

macOS is supported for local development and smoke testing. It does not provide
Linux hardware timestamping, busy poll, realtime scheduling, PACKET_MMAP, or
AF_XDP behavior.

## Quick Start

### Prerequisites

- Rust 1.94 or newer.
- Linux for production receive validation.
- macOS or Linux for local build, replay, and smoke-test workflows.
- Docker, only if building the runtime container.

### Build

```bash
cargo build --release --locked
```

Optional allocator features:

```bash
cargo build --release --locked --features jemalloc
cargo build --release --locked --features mimalloc
```

`jemalloc` is Linux-only. If both allocator features are enabled, jemalloc is
used on Linux and mimalloc is used on other platforms.

### Run The Engine

Linux production-style config:

```bash
cargo run --release --locked -- config.toml
```

macOS local receive profile:

```bash
cargo run --release --locked -- config.darwin.toml
```

The engine exits early when requested production socket or scheduling options
cannot be applied. This includes receive-buffer sizing, `SO_REUSEPORT`, busy
poll, memory locking, realtime scheduling, and RX timestamping.

### Docker

```bash
docker build -t numi-orderbook:local .
```

Run the container with host networking, a deployment-specific config, writable
snapshot/journal paths, and only the Linux capabilities required by that
profile. PACKET_MMAP and host tuning require additional privileges; see
[`ops/README.md`](ops/README.md).

## Configuration

Start from [`config.toml`](config.toml) for Linux deployments and
[`config.darwin.toml`](config.darwin.toml) for local macOS receive testing.

Important sections:

- `[general]`: packet size, pool sizing, queue capacities, batch size, memory
  locking, and log format.
- `[sequence]`: packet sequence offset, width, and byte order.
- `[parser]`: decoder selection (`eobi`, `itch50`, or `fast_like`).
- `[channels.a]` / `[channels.b]`: multicast group, port, interface address,
  socket options, timestamping, and worker count.
- `[merge]`: initial sequence, reorder window, pending-packet cap, and adaptive
  merge controls.
- `[book]`: reporting depth, tick sizes, order-slab sizing, reference data, and
  trade-consumption policy.
- `[cpu]`: RX, merge, and decode thread pinning plus optional realtime priority.
- `[metrics]`: Prometheus and health endpoint bind address.
- `[snapshot]` / `[journal]`: persistence and deterministic replay settings.
- `[recovery]`: TCP replay injector, retries, throttling, stale replay rejection,
  timeouts, and unrecoverable-gap policy.
- `[packet_mmap]`: optional Linux PACKET_RX_RING receive path.
- `[feeds]`: raw-v1 WebSocket feed settings.

Leave `[afxdp].enable = false`. The config validator rejects AF_XDP until a real
XSK/UMEM backend is implemented.

## Runtime Interfaces

When `[metrics]` is configured, the HTTP server exposes:

- `GET /metrics`: Prometheus metrics.
- `GET /live`: process liveness.
- `GET /ready`: readiness.
- `GET /healthz`: combined health probe.
- `GET /snapshot`: on-demand snapshot trigger.

The raw-v1 WebSocket feed is configured under `[feeds]`. Clients should use the
wire contract in [`docs/obo_raw_v1.md`](docs/obo_raw_v1.md), including snapshot
cursor and reconnect semantics.

## Participant Insight Signals

The participant insight layer consumes raw-v1 order-by-order WebSocket frames
outside the engine hot path. It gives users explainable market-participant
behavior signals without changing ingest, merge, decode, or book-apply latency.

Current signals:

- Absorption: aggressive flow trades into a price while passive liquidity holds
  or replenishes.
- Iceberg/replenishment candidate: repeated same-price replenishment under
  execution pressure, with executed quantity exceeding observed display size.
- Liquidity pull: displayed size is withdrawn by cancels or quantity-reducing
  modifies before it trades.

Run the sidecar against one or more raw-v1 OBO WebSocket feeds:

```bash
cargo run --release --bin absorption_sidecar -- \
  --url 'ws://127.0.0.1:7001/ws?channel=obo&codec=raw-v1&snapshot=1' \
  --listen 127.0.0.1:9201
```

The sidecar emits each signal as tagged JSON and exposes:

- `GET /stats`: frames, duplicates, parse errors, connection attempts, and
  per-signal counters, including a diagnostics snapshot.
- `GET /diagnostics`: session signal diagnostics: counts, max/average score,
  top scoring components, first/last signal timestamps, and a conservative
  regime label (`balance`, `absorption`, `spoof_risk`, or `initiative_flow`).
- `GET /features`: latest z-scored microstructure feature snapshot per
  instrument when the sidecar is started with `--enable-features`, including
  book imbalance, OFI, touch activity, volume, momentum, slope, and day-of-week
  context.
- `GET /signals`: recent retained participant signals.
- `GET /signals/absorption`: recent absorption signals.
- `GET /signals/iceberg`: recent iceberg/replenishment candidates.
- `GET /signals/liquidity_pull`: recent liquidity-pull candidates.

Record raw frames for deterministic replay:

```bash
cargo run --release --bin absorption_sidecar -- \
  --url 'ws://127.0.0.1:7001/ws?channel=obo&codec=raw-v1&snapshot=1' \
  --record-frames /tmp/numi-obo.frames
```

Validate each signal family against the same recording:

```bash
cargo run --release --bin absorption_replay -- /tmp/numi-obo.frames
cargo run --release --bin iceberg_replay -- /tmp/numi-obo.frames
cargo run --release --bin liquidity_pull_replay -- /tmp/numi-obo.frames
cargo run --release --bin participant_replay -- /tmp/numi-obo.frames
```

`participant_replay` runs all participant detectors together and is the
backtest-oriented path for the combined session diagnostics and regime label.

Feature collection is disabled by default on the sidecar hot path. Start with
`--enable-features` to populate `/features`. Feature z-score baselines are
per-instrument EWMA normalizers in the sidecar; use `--feature-depth-levels`,
`--feature-z-alpha`, `--feature-z-min-samples`, and `--feature-z-clip` to tune
the sampled depth and normalization behavior.

The signal definitions, evidence fields, thresholds, and interpretation limits
are documented in [`docs/participant_insights.md`](docs/participant_insights.md).

## Validation

Run the standard local gate before pushing changes:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --locked
```

Check Linux-only receive code explicitly when working away from the target host:

```bash
RUSTFLAGS='' cargo check --target x86_64-unknown-linux-gnu --all-targets
RUSTFLAGS='' cargo clippy --target x86_64-unknown-linux-gnu --all-targets -- -D warnings
```

Recommended smoke benchmarks:

```bash
NUMI_BENCH_SMOKE=1 cargo bench --bench hot_paths -- --quiet
cargo run --release --bin pool_soak -- 65536 2048 10000 64
cargo run --release --bin rx_probe -- 100000 64 32 software 0
cargo run --release --bin bench_pipeline -- local-core
cargo run --release --bin bench_pipeline -- rx-proof --packets 512 --artifact-dir target/bench-artifacts
```

Expected high-level results:

- `pool_soak`: `misses=0` and `return_drops=0`.
- `local-core`: `status=ok`, `sequence_gaps=0`, `dup_or_ooo=0`,
  `event_vec_reallocs=0`, and `pool_available=pool_size`.
- `rx-proof`: `status=ok`, matching expected and journal hashes, no decoder
  sequence gaps, and non-zero OBO frames.

Target-hardware SLOs and benchmark profiles are documented in
[`docs/SLO.md`](docs/SLO.md). Local benchmark numbers are smoke signals only;
production latency claims must come from pinned target hardware.

## Operations

Use [`ops/README.md`](ops/README.md), [`ops/tuning.sh`](ops/tuning.sh), and
[`ops/steering.sh`](ops/steering.sh) as the starting point for Linux host
tuning.

Operational checklist:

- Pin RX, merge, and decode threads to isolated cores.
- Keep NIC queues, packet rings, and hot threads on the same NUMA node.
- Move unrelated IRQs off hot cores.
- Disable GRO, LRO, GSO, and TSO on market-data queues.
- Run PTP/PHC sync before relying on hardware timestamp SLOs.
- Confirm startup logs show requested socket options were applied.
- Monitor packet-pool misses, return drops, RX drops, merge gaps, recovery SLO
  violations, journal errors, snapshot failures, and dropped clients.

A sample systemd unit is provided at
[`systemd-orderbook.service`](systemd-orderbook.service). Review paths,
capabilities, CPU affinity, and hardening options before using it on a
production host.

## Repository Layout

```text
src/main.rs                 Main engine binary
src/lib.rs                  Shared library surface for tools and tests
src/rx*.rs                  UDP, Darwin, and PACKET_MMAP receive paths
src/merge.rs                Sequence merge, duplicate suppression, gap handling
src/decode.rs               Decode loop, book apply, journal, snapshots, publish
src/orderbook.rs            Deterministic price-time order book
src/decoder_*.rs            Venue decoder surfaces and generated schemas
src/recovery.rs             Gap recovery logging and TCP replay injection
src/pubsub.rs               Retained raw-v1 publication bus
src/ws_server.rs            WebSocket feed serving and reconnect handling
src/metrics.rs              Prometheus metrics and health endpoints
src/bin/                    Replay, probe, benchmark, capture, and sidecar tools
benches/                    Criterion hot-path benchmarks
config*.toml                Linux and macOS example configurations
docs/                       Protocol, SLO, timestamp, roadmap, and venue notes
ops/                        Linux host tuning and multicast steering helpers
```

## Documentation

- [`CHANGELOG.md`](CHANGELOG.md): notable project changes.
- [`docs/SLO.md`](docs/SLO.md): validation gates and verification records.
- [`docs/obo_raw_v1.md`](docs/obo_raw_v1.md): raw-v1 WebSocket wire format.
- [`docs/timestamps.md`](docs/timestamps.md): timestamp source policy.
- [`docs/darwin_rx.md`](docs/darwin_rx.md): macOS receive implementation notes.
- [`docs/eobi_t7_14_1.md`](docs/eobi_t7_14_1.md): EOBI decoder notes.
- [`docs/participant_insights.md`](docs/participant_insights.md): signal sidecar
  behavior.
- [`docs/ROADMAP.md`](docs/ROADMAP.md): current roadmap and acceptance gates.

## License

Apache-2.0. See [`Cargo.toml`](Cargo.toml) for the package license metadata.
