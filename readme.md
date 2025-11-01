## Ultra‑low‑latency market‑data receiver and price–time order book

my experimeent in building an exchange‑style market‑data stack. inspired heavily by deutcheborse architecture

- **Dual A/B multicast ingestion** with strict sequencing and gap detection
- **Lock‑free merge** with bounded out‑of‑order buffering
- **Zero‑alloc decoders** (EOBI/SBE‑like and ITCH 5.0)
- **In‑memory full‑depth order book** with price–time semantics
- **Snapshots** (export/import) and **Prometheus metrics**
- **Recovery injector** (TCP) that feeds recovered sequences into the same pipeline
- **Kernel-bypass style RX**: optional AF_XDP path with high-performance PACKET_MMAP ring fallback

### Architecture

1. RX A / RX B (UDP or AF_XDP)
2. Merge (sequence order, windowed buffering, gap notification)
3. Decode (payload → `Event` vector; zero‑copy slices; pre‑sized buffers)
4. Order book apply (price–time, per‑instrument)
5. Metrics + periodic snapshots
6. Recovery injector (optional) injects recovered ranges back into the merge/decoder path

### Protocols

- **EOBI/SBE‑like**: default when `parser.kind = "fixed_binary"`. Frames are parsed with minimal copies and mapped to `Event`s.
- **ITCH 5.0**: `parser.kind = "itch50"`. Includes stateful handling of add/modify/execute/cancel/replace and trades.
- **FAST/EMDI‑like**: `parser.kind = "fast_like"`. Minimal, production‑ready subset decoder using stop‑bit integers and presence maps sufficient for Add/Mod/Del/Trade.

### Build

```bash
cargo build --release
```

### Run

```bash
cargo run --release -- config.toml
```

### Configuration (key fields)

```toml
[general]
max_packet_size = 2048
pool_size = 65536
rx_queue_capacity = 65536
merge_queue_capacity = 65536
spin_loops_per_yield = 64
rx_recvmmsg_batch = 32        # repeated recv/recvmsg per loop (>=1)
mlock_all = true              # mlockall current+future pages (Linux)
json_logs = false             # structured JSON logs to stdout

[sequence]
offset = 0
length = 8
endian = "be"

[parser]
kind = "fixed_binary"         # fixed_binary | fast_like | itch50
max_messages_per_packet = 128

[channels.a]
group = "239.10.10.1"
port = 5001
iface_addr = "10.0.0.11"
reuse_port = true
recv_buffer_bytes = 67108864
busy_poll_us = 50
nonblocking = true
timestamping = "hardware"     # off | software | hardware | hardware_raw
workers = 1                    # number of UDP RX sockets/threads (requires reuse_port)

[channels.b]
group = "239.10.10.2"
port = 5001
iface_addr = "10.0.0.12"
reuse_port = true
recv_buffer_bytes = 67108864
busy_poll_us = 50
nonblocking = true
timestamping = "hardware"
workers = 1

[merge]
initial_expected_seq = 1
reorder_window = 512
max_pending_packets = 131072

[book]
max_depth = 50
snapshot_interval_ms = 1000
consume_trades = false        # set true if your feed omits Mod/Del after trades

[cpu]
a_rx_core = 2
b_rx_core = 4
merge_core = 6
decode_core = 8
rt_priority = 80              # SCHED_FIFO priority (Linux)

[metrics]
bind = "0.0.0.0:9100"

[snapshot]
path = "/var/lib/t7_like/book.snap"
load_on_start = true
enable_writer = true

[recovery]
enable_injector = false
endpoint = "127.0.0.1:9000"  # venue‑specific replay endpoint (if enabled)
backlog_path = "/var/lib/t7_like/recovery.log"  # optional append-only gap log

[afxdp]
enable = false                # if true, replaces channel A socket RX with AF_XDP
ifname = "eth0"
queue_id = 0
```

### Feed semantics: `consume_trades`

Some venues do not send explicit Mod/Del updates after a trade. If your feed has that behavior, set `book.consume_trades = true` to reduce maker orders directly on `Trade` events. Leave it `false` when your feed sends the normal Mod/Del updates.

### Performance tuning checklist (Linux)

- **CPU isolation & affinity**: pin threads to isolated cores; move IRQs off critical cores
- **Realtime scheduling**: set `cpu.rt_priority` (SCHED_FIFO) for RX/merge/decode
- **Page locking**: `general.mlock_all = true`
- **NIC offloads**: disable GRO/LRO/GSO/TSO for the RX queues used; enable hardware timestamping if required
- **Busy poll**: set `channels.*.busy_poll_us` and increase socket RCVBUF
- **IRQ/NAPI budget**: tune per driver; consider busy‑polling userspace receive loops
- **NUMA locality**: bind threads to the NIC’s NUMA node; keep queues and memory local

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

### Directory layout

- `src/rx.rs` — UDP receive (timestamping, batching)
- `src/rx_afxdp.rs` — AF_XDP receive loop
- `src/merge.rs` — sequence merge, gap detection, recovery signaling
- `src/decode.rs` — decode thread and event dispatch to the book
- `src/parser.rs` — `Event` model, sequence extractor, parser builder
- `src/decoder_eobi.rs` — EOBI/SBE‑like zero‑alloc decoder
- `src/decoder_itch.rs` — ITCH 5.0 decoder
- `src/orderbook.rs` — price–time order book
- `src/recovery.rs` — logger and TCP replay injector
- `src/snapshot.rs` — snapshot load/save
- `src/metrics.rs` — Prometheus exporter
- `src/net.rs` — socket setup and Linux socket tuning

### Notes

This code favors clarity on the cold path and extreme efficiency on the hot path. The hot loops avoid heap allocations, hold no locks beyond single‑writer state, and reuse pre‑sized buffers. Configure `max_messages_per_packet` to right‑size per‑packet event vectors.

### Binary OBO feed (raw v1)

This build exposes a binary WebSocket OBO feed (Order‑by‑Order L3 events) with two endpoints per POP for first‑arrival dedupe by sequence. See `docs/obo_raw_v1.md` for wire format and API.