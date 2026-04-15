# Numi Orderbook

Numi Orderbook is a Rust market-data receiver and deterministic price-time
order book for low-latency L3 market data pipelines. It ingests dual A/B
multicast feeds, merges sequence-ordered packets, decodes venue-style binary
messages, applies them to an in-memory full-depth book, and distributes raw
order-by-order updates to clients.

The codebase is optimized for predictable hot-path behavior: preallocated packet
buffers, bounded queues, fail-fast socket tuning, explicit recovery state, and
regression tests for packet ownership and replay correctness.

## Capabilities

- Dual A/B multicast ingestion with strict sequencing and duplicate suppression.
- Linux UDP `recvmmsg` batching with software, hardware, and raw-hardware RX
  timestamp support.
- macOS UDP `recvmsg_x` batching with `SO_TIMESTAMP_MONOTONIC` for local
  development and performance work.
- Optional Linux PACKET_MMAP receive path for high-throughput AF_PACKET ingest.
- Bounded SPSC hot-path queues for RX, merge, and decode stages.
- Price-time order book with per-instrument tick configuration and stable state
  hashing.
- Generated Deutsche Boerse T7 14.1 EOBI, ITCH 5.0, and FAST/EMDI-like
  decoder surfaces.
- Snapshot load/save, framed journal replay verification, and deterministic
  restart checks.
- TCP recovery injector with retry, throttling, stale replay rejection, SLO
  accounting, and unrecoverable-gap policy.
- Raw-v1 WebSocket OBO feed with snapshot cursor semantics, heartbeat frames,
  connection caps, and slow-client eviction.
- Prometheus metrics, health endpoints, and synthetic soak/benchmark binaries.

## Architecture

```text
RX A / RX B
  -> sequence merge and gap detection
  -> decode and journal
  -> price-time order book
  -> snapshots, metrics, and OBO publication
  -> WebSocket clients
```

Recovery packets enter through a dedicated queue and are merged through the same
sequence path as live packets. PACKET_MMAP can own channel A when enabled;
AF_XDP is intentionally rejected by config until a real XSK/UMEM backend is
implemented.

## Platform

The production receive path targets Linux. Linux-specific features include:

- `recvmmsg` UDP batching.
- `SO_TIMESTAMPNS` and `SO_TIMESTAMPING` timestamp extraction.
- `SO_BUSY_POLL` socket tuning.
- `mlockall`, realtime scheduling, and PACKET_MMAP.

macOS has a dedicated local-performance receive path. It uses Darwin's
`recvmsg_x` batch syscall when available, parses `SCM_TIMESTAMP_MONOTONIC`, and
converts Mach absolute ticks into nanoseconds. It intentionally does not claim
hardware timestamping, PACKET_MMAP, AF_XDP, busy poll, or realtime Linux
scheduling support.

Production latency validation must run on the target Linux host with the target
NIC, driver, clock sync, kernel settings, and CPU isolation.

## Build

```bash
cargo build --release --locked
```

Optional allocator features:

```bash
cargo build --release --locked --features jemalloc
cargo build --release --locked --features mimalloc
```

`jemalloc` is Linux-only. If both allocator features are enabled, jemalloc is
used on Linux and mimalloc is used elsewhere.

## Run

```bash
cargo run --release --locked -- config.toml
```

For macOS local receive testing:

```bash
cargo run --release --locked -- config.darwin.toml
```

The process fails fast when requested production socket options cannot be
applied. This includes receive-buffer sizing, `SO_REUSEPORT`, busy poll, and RX
timestamping.

## Docker

Build the runtime image:

```bash
docker build -t numi-orderbook:local .
```

Run with host networking and the Linux capabilities required by the selected
configuration. For production timestamping, realtime scheduling, memory locking,
or PACKET_MMAP, grant only the capabilities needed by that deployment profile.

## Validation

Run the local verification suite before pushing changes:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --locked
```

Linux-only receive code is also checked explicitly:

```bash
RUSTFLAGS='' cargo check --target x86_64-unknown-linux-gnu --all-targets
RUSTFLAGS='' cargo clippy --target x86_64-unknown-linux-gnu --all-targets -- -D warnings
```

Lean benchmark smoke gates:

```bash
NUMI_BENCH_SMOKE=1 cargo bench --bench hot_paths -- --quiet
cargo run --release --bin pool_soak -- 65536 2048 10000 64
cargo run --release --bin rx_probe -- 100000 64 32 software 0
cargo run --release --bin bench_pipeline -- local-core
```

`pool_soak` must report `misses=0` and `return_drops=0` for production pool
sizing. `bench_pipeline -- local-core` reports a single machine-readable
`key=value` line and must show `status=ok`, `sequence_gaps=0`, `dup_or_ooo=0`,
`event_vec_reallocs=0`, and `pool_available=pool_size`. Local throughput and
latency samples are smoke signals only; production latency claims must come
from pinned target hardware with the target NIC, kernel, timestamp source, and
clock sync.

Additional benchmark profiles:

```bash
cargo run --release --bin bench_pipeline -- local-distribution
cargo run --release --bin bench_pipeline -- target-rx --config config.toml --duration-sec 60 --packets 892800000
cargo run --release --bin bench_pipeline -- target-failover-recovery
cargo run --release --bin bench_pipeline -- target-persistence --packets 1024
```

`target-rx` binds the configured multicast sockets and expects production-like
traffic to already be present; use `mcast_burst` or `pcap_replay` from another
process/host to drive the feed. `target-failover-recovery` uses a synthetic
1,000-message gap and fails unless merge output stays monotonic and the packet
pool returns to full availability. The Dockerfile is also part of the pre-push
gate:

```bash
docker build -t numi-orderbook:local .
```

## Configuration

Start from `config.toml`. Key production controls:

```toml
[general]
max_packet_size = 2048
pool_size = 65536
rx_queue_capacity = 65536
merge_queue_capacity = 65536
rx_recvmmsg_batch = 32
mlock_all = true

[sequence]
# T7 EOBI PacketHeader.ApplSeqNum
offset = 8
length = 4
endian = "le"

[parser]
kind = "eobi"
max_messages_per_packet = 128

[channels.a]
group = "239.10.10.1"
port = 5001
iface_addr = "10.0.0.11"
reuse_port = true
recv_buffer_bytes = 67108864
busy_poll_us = 50
timestamping = "hardware" # off | software | hardware | hardware_raw
workers = 1

[channels.b]
group = "239.10.10.2"
port = 5001
iface_addr = "10.0.0.12"
reuse_port = true
recv_buffer_bytes = 67108864
busy_poll_us = 50
timestamping = "hardware"
workers = 1

[book]
max_depth = 50
default_tick = 1
grid_span = 16384
order_slab_capacity = 1048576
order_index_capacity = 1048576
per_instrument_order_index_capacity = 1048576
# instrument_capacity = 1024
preallocate_instrument_books = false

[packet_mmap]
enable = false
ifname = "eth0"
queues = 1
frame_size = 2048
frames_per_block = 1024
block_count = 4
```

If a venue sends trades without subsequent modify/delete messages, set
`book.consume_trades = true`. Leave it `false` when the venue sends explicit
book updates after trades.

## Operations

Use `ops/README.md` and the scripts under `ops/` for Linux host tuning. The
main production checks are:

- Pin RX, merge, and decode threads to isolated cores.
- Keep hot threads and NIC queues on the same NUMA node.
- Move unrelated IRQs off hot cores.
- Disable GRO, LRO, GSO, and TSO on market-data queues.
- Run PTP/PHC sync before using hardware timestamp SLOs.
- Confirm requested socket options are applied at startup.
- Monitor packet-pool misses, packet-pool return drops, RX drops, merge gaps,
  recovery SLO violations, and client drops.

## Runtime Interfaces

- Prometheus metrics: `metrics.bind`
- Health endpoints: `/live`, `/ready`, `/healthz`
- Snapshot trigger: `GET /snapshot`
- Raw-v1 OBO WebSocket feed: configured under `[feeds]`

Snapshot files include the global live-feed replay cursor immediately following
the image. Clients requesting `snapshot=1` receive the image first, then live
frames from that cursor. Legacy snapshots without cursor metadata, or snapshots
whose cursor has fallen outside retained live replay, are rejected.

## Repository Layout

- `src/rx.rs`: UDP receive, batching, timestamp extraction, packet recycling.
- `src/rx_darwin_udp.rs`: macOS UDP `recvmsg_x` receive loop.
- `src/rx_packet_mmap.rs`: Linux PACKET_MMAP receive loop.
- `src/rx_udp.rs`: platform dispatcher for UDP receive loops.
- `src/merge.rs`: sequence merge, duplicate suppression, gap signaling.
- `src/decode.rs`: packet decode, book apply, snapshots, journaling, OBO publish.
- `src/orderbook.rs`: price-time book implementation.
- `src/recovery.rs`: recovery logging and TCP replay injection.
- `src/pubsub.rs`: retained raw-v1 publication bus.
- `src/ws_server.rs`: WebSocket feed serving, reconnect, snapshot handling.
- `src/metrics.rs`: Prometheus metrics and health endpoints.
- `benches/hot_paths.rs`: Criterion microbenchmarks for the highest-ROI hot paths.
- `src/bin/bench_pipeline.rs`: local and target-hardware macro benchmark runner.
- `src/bin/pool_soak.rs`: packet-pool allocation soak.
- `src/bin/rx_probe.rs`: loopback UDP receive integrity and timestamp probe.
- `docs/`: roadmap, timestamp strategy, SLOs, and raw-v1 wire format.
- `ops/`: Linux tuning and queue steering helpers.

## Documentation

- `docs/ROADMAP.md`: development roadmap and current completed slices.
- `docs/SLO.md`: validation gates and local verification record.
- `docs/timestamps.md`: timestamp source policy and calibration guidance.
- `docs/obo_raw_v1.md`: raw-v1 client wire format.
- `CHANGELOG.md`: notable changes.

## License

Apache-2.0.
