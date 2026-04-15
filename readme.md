## Ultra‑low‑latency market‑data receiver and price–time order book

my experimeent in building an exchange‑style market‑data stack. inspired heavily by deutcheborse architecture

- **Dual A/B multicast ingestion** with strict sequencing and gap detection
- **Lock‑free merge** with bounded out‑of‑order buffering
- **Zero‑alloc decoders** (EOBI/SBE‑like and ITCH 5.0)
- **In‑memory full‑depth order book** with price–time semantics
- **Snapshots** (export/import) and **Prometheus metrics**
- **Recovery injector** (TCP) that feeds recovered sequences into the same pipeline
- **PACKET_MMAP RX fallback**: optional AF_PACKET/TPACKET_V2 receive path for high-throughput Linux ingest

### Architecture

1. RX A / RX B (UDP or PACKET_MMAP fallback)
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
mlock_all = true              # fail fast unless current+future pages are locked (Linux)
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
default_tick = 1
grid_span = 16384
order_slab_capacity = 1048576
instrument_ticks = []         # e.g. [{ instr = 1001, tick = 5 }]
# instrument_ticks_path = "/var/lib/t7_like/instrument_ticks.csv"

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

[journal]
path = "/var/lib/t7_like/book.journal"
enable_writer = false
record_state_hash = true

[recovery]
enable_injector = false
endpoint = "127.0.0.1:9000"  # venue‑specific replay endpoint (if enabled)
backlog_path = "/var/lib/t7_like/recovery.log"  # optional append-only gap log
retry_attempts = 3
retry_backoff_ms = 10
min_request_interval_ms = 0
slo_ms = 100
unrecoverable_policy = "log" # log, panic, or exit
request_timeout_ms = 250
replay_protocol = "len_seq_payload" # REPLAY request + [len, seq, payload] response frames

[afxdp]
enable = false                # disabled until a real AF_XDP/XSK backend is integrated
ifname = "eth0"
queues = 1

[packet_mmap]
enable = false                # if true, replaces channel A socket RX with PACKET_RX_RING
ifname = "eth0"
queues = 1
frame_size = 2048
frames_per_block = 1024
block_count = 4

[feeds]
enabled = false
pops = []

[feeds.obo]
enabled = false
client_write_timeout_ms = 250 # slow WS clients are evicted instead of blocking publisher threads
client_handshake_timeout_ms = 1000
client_heartbeat_interval_ms = 1000
client_max_connections = 1024
client_nodelay = true

[feeds.obo.buffers]
pub_queue = 65536
```

### Feed semantics: `consume_trades`

Some venues do not send explicit Mod/Del updates after a trade. If your feed has that behavior, set `book.consume_trades = true` to reduce maker orders directly on `Trade` events. Leave it `false` when your feed sends the normal Mod/Del updates.

### Snapshot feed semantics

Snapshots written by this process include the global live-feed replay cursor that
immediately follows the image. A WebSocket client using `snapshot=1` receives the
image first and then live frames from that cursor. Legacy snapshots without this
cursor, and snapshots whose cursor is older than retained live replay, are
rejected for client snapshot-on-connect.

### Performance tuning checklist (Linux)

- **CPU isolation & affinity**: pin threads to isolated cores; move IRQs off critical cores
- **Realtime scheduling**: set `cpu.rt_priority` (SCHED_FIFO) for RX/merge/decode
- **Page locking**: `general.mlock_all = true`
- **NIC offloads**: disable GRO/LRO/GSO/TSO for the RX queues used; enable hardware timestamping if required
- **Busy poll**: set `channels.*.busy_poll_us` and increase socket RCVBUF
- **IRQ/NAPI budget**: tune per driver; consider busy‑polling userspace receive loops
- **NUMA locality**: bind threads to the NIC’s NUMA node; keep queues and memory local


- `src/rx.rs` — UDP receive (timestamping, batching)
- `src/rx_packet_mmap.rs` — PACKET_MMAP receive loop; real AF_XDP/XSK is intentionally unavailable until a real backend is integrated
- `src/merge.rs` — sequence merge, gap detection, recovery signaling
- `src/decode.rs` — decode thread and event dispatch to the book
- `src/parser.rs` — `Event` model, sequence extractor, parser builder
- `src/decoder_eobi.rs` — EOBI/SBE‑like zero‑alloc decoder
- `src/decoder_itch.rs` — ITCH 5.0 decoder
- `src/orderbook.rs` — price–time order book
- `src/recovery.rs` — logger and TCP replay injector
- `src/snapshot.rs` — snapshot load/save
- `src/metrics.rs` — Prometheus exporter
- `src/bin/pool_soak.rs` — packet-pool allocation soak harness
- `src/net.rs` — socket setup and Linux socket tuning

### Notes

This code favors clarity on the cold path and extreme efficiency on the hot path. The hot loops avoid heap allocations, hold no locks beyond single‑writer state, and reuse pre‑sized buffers. Configure `max_messages_per_packet` to right‑size per‑packet event vectors.

See `docs/ROADMAP.md` for the staged path from the current implementation to a production-grade A/B multicast receiver and price-time order book.
