World-Class Market Data Roadmap
===============================

This roadmap tracks the path from the current Rust prototype to a production
ultra-low-latency A/B multicast receiver and price-time order book.

Status key:
- Done: implemented in the working tree.
- Next: immediate development focus.
- Planned: not started.

M1: Correctness Baseline
------------------------

Status: Done

Goals:
- Deterministic book state for every accepted event.
- No corrupt outbound OBO frames.
- Config behavior matches operator intent.
- Regression tests cover known correctness failures.

Completed in current working tree:
- Resolve OBO instrument id for `Mod` and `Del` before mutating book state.
- Skip OBO publish when an event cannot be tied to an instrument instead of
  publishing under instrument `0`.
- Honor `general.mlock_all = false`.
- Fail fast for `general.mlock_all = true` when RLIMIT or `mlockall` cannot be
  applied.
- Rename the current packet-ring receiver as PACKET_MMAP and reserve AF_XDP for
  a future real XSK/UMEM implementation.
- Rename the receive module to `rx_packet_mmap` so the fallback is not presented
  as AF_XDP anywhere in the code path.
- Use `packet_mmap.queues` as the PACKET_RX_RING worker-count source of truth.
- Replace duplicate `Add` order ids atomically instead of orphaning the old node.
- Add duplicate-add regression tests for single and batched apply.
- Add order-book invariant validation for index/slab/level consistency, FIFO
  links, aggregate quantities, cached BBO, and positive live quantities.
- Add deterministic order-book event-sequence regression with snapshot roundtrip.
- Correct cold-path `top_n` depth ordering across grid and overflow levels.
- Prevent duplicate grid/overflow levels for the same side and price after grid
  recentering.
- Add deterministic merge fixtures for A/B duplicate handling, feed-A cutoff
  with B continuation, and recovery-queue gap fill.
- Add merge regression for recovery gap notification.
- Fix adaptive merge ring sizing so the preallocated buffer covers the configured
  adaptive maximum window.
- Keep merge pending-count accounting stable when a stale ring slot is replaced.
- Fix recovery gap coalescing so every drained non-overlapping range is fetched,
  not only the first merged range.
- Add recovery retry policy, requested/fetched/failed backlog statuses, stale
  replay rejection, dropped-request visibility, and recovery lifecycle metrics.
- Clean known default clippy findings that were already identified in static
  review.
- Split the project into a reusable library plus binaries, so tools and tests
  use the same production modules instead of path-based module copies.
- Remove dead-code allowances and obsolete private helpers instead of hiding
  unused code.

Remaining:
- No open baseline gate in the current working tree. Continue adding
  venue-specific fixtures as production schemas and replay samples become
  available.

M2: A/B Merge And Recovery
--------------------------

Status: Next

Goals:
- Strict monotonic output under duplicate, reordered, and failed feed conditions.
- Explicit gap lifecycle: detected, requested, filled, or unrecoverable.
- Venue-specific replay client protocol, throttling, and recovery SLO alerts.

Completed in current working tree:
- Coalesced recovery range fetching.
- Configurable replay retry attempts and linear backoff.
- Requested/fetched/failed recovery backlog status lines.
- Stale replay packet rejection by requested sequence range.
- Replay fetch throttling via `recovery.min_request_interval_ms`.
- Recovery range latency and SLO-violation tracking via `recovery.slo_ms`.
- Unrecoverable gap escalation via `recovery.unrecoverable_policy`.
- Configurable replay protocol adapter surface via `recovery.replay_protocol`,
  with the current `len_seq_payload` framing isolated and regression-covered.
- TCP read/write timeout control via `recovery.request_timeout_ms`.
- Recovery request, dropped-request, retry, failure, fetched-range,
  unrecoverable-gap, injected-packet, stale-packet, range-latency, and
  SLO-violation metrics.

Remaining:
- Add concrete venue replay adapter once the target venue protocol is selected.

Acceptance gates:
- Hard cut feed A for 200 ms, continue on B with zero duplicate output.
- Inject a 1,000-message gap, fill via replay within the SLO, and preserve
  sequence monotonicity after merge.

M3: Latency Baseline
--------------------

Status: Next

Goals:
- UDP `recvmmsg` plus hardware timestamping remains the canonical measured path.
- No steady-state hot-path allocations in RX, merge, decode, and book apply.
- No page faults after warmup.

Completed in current working tree:
- Packet pool buffers are preallocated and page-touched during startup.
- Runtime packet-pool fallback allocations are counted with
  `packet_pool_misses_total`.
- Failed packet-buffer returns are counted with
  `packet_pool_return_drops_total`.
- Startup pool footprint is exposed as `packet_pool_preallocated_bytes`.
- Decode event-vector capacity misses are counted with
  `decode_event_vec_reallocs_total`.
- Order-book slab capacity growth is counted with `orderbook_slab_grows_total`.
- `pool_soak` synthetic harness verifies packet-pool sizing and fails on
  fallback allocation or return-drop counters by default.
- Cold-path depth assembly vector growth is counted with
  `orderbook_depth_vec_grows_total`.
- Snapshot export vector growth is counted with `orderbook_export_vec_grows_total`.
- Snapshot writer payload growth and latest payload size are counted with
  `snapshot_payload_vec_grows_total` and `snapshot_payload_bytes`.
- UDP RX recycles packet buffers when the output queue rejects a packet instead
  of leaking the backing allocation under backpressure.
- UDP `recvmmsg` no-progress and fatal-error paths recycle every prepared batch
  buffer before sleeping or returning an error.
- UDP `recvmmsg` now preserves Linux timestamp ancillary data with per-message
  control buffers, so timestamped channels keep the batched receive path.
- Timestamp parsing distinguishes actual `SCM_TIMESTAMPING` slots: software,
  system-hardware, and raw-hardware timestamps are labeled by the timestamp that
  was actually present, not only by requested mode.
- UDP `recv`/`recvmsg` transient and fatal-error paths return the checked-out
  packet buffer before retrying or exiting.
- PACKET_MMAP queue-full drops recycle the copied packet buffer before
  releasing the kernel frame.
- Socket setup now fails fast when requested production options cannot be
  applied: `SO_REUSEADDR`, `SO_REUSEPORT`, receive-buffer sizing, Linux busy
  poll, and Linux RX timestamping.
- Packet-pool ownership regression tests cover rejected queue pushes and the
  Linux batched-receive no-progress recycle path.
- Linux-only timestamp parser regressions cover software timestamping,
  hardware-slot selection, and software fallback when hardware slots are empty.
- Linux target checks now cover the production-only receive code:
  `RUSTFLAGS='' cargo check --target x86_64-unknown-linux-gnu --all-targets`
  and `RUSTFLAGS='' cargo clippy --target x86_64-unknown-linux-gnu --all-targets
  -- -D warnings`.
- Allocator feature switches now map to real optional dependencies instead of
  empty feature flags.
- Local development gates pass on 2026-04-15: `cargo fmt`, strict
  `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test --all-features`, `cargo build --release`, and `pool_soak` with
  zero misses and zero return drops.

Remaining:
- Run target-hardware benchmark and latency SLO measurements on pinned,
  isolated production NIC hosts.
- Continue hot-path allocation audits for journaling, snapshot export, and
  client distribution under production load.

Acceptance gates:
- 10GbE 64-byte payload class: 14.88 Mpps for 60 seconds, zero app drops.
- p50 < 9 us, p99 < 40 us, p99.9 < 60 us to decode entry using hardware or
  calibrated timestamps.

M4: Venue-Grade Book And Decoders
---------------------------------

Status: Next

Goals:
- Venue tick tables per instrument.
- Certified binary decoders from real schemas where possible.
- Stable book hashes and full-session deterministic replay.
- Snapshot plus journal restart with sequence/session continuity.

Completed in current working tree:
- Snapshot export and depth export traverse instruments in sorted order, not
  `HashMap` iteration order.
- Stable `OrderBook::state_hash()` hashes deterministic per-order book state.
- Snapshot roundtrip and insertion-order-independent hash regressions cover
  deterministic state identity.
- Config-driven per-instrument tick table wires into newly-created instrument
  books, with validation and regression coverage.
- Add framed journal records and replay verification that compare final
  `state_hash()` against recorded state and flag non-monotonic sequences.
- Add optional live journal writing from the decode thread with per-packet
  sequence plus per-event index for multi-message packets.
- Add streaming replay verification from framed journal readers, so full-session
  checks do not require loading all records into memory.
- Add snapshot+journal restart continuity verification by anchoring a snapshot
  hash to a recorded post-event journal hash and replaying only continuation
  records.
- Add CSV venue reference-data tick-table loading with header aliases for common
  instrument-id and tick-size column names; inline config entries remain
  available as overrides.
- Add static EOBI/SBE message descriptors for the selected binary decoder and
  dispatch through schema/version/template metadata before field decoding.

Remaining:
- Replace static EOBI descriptors with generated descriptors from official venue
  XML when those schema files are available.

M5: Kernel-Bypass Receive Path
------------------------------

Status: Next

Goals:
- Real AF_XDP/XSK implementation, not PACKET_MMAP.
- UMEM fill/completion ring ownership.
- No payload copy into pooled buffers.
- Queue-specific steering and NUMA-local memory.
- Timestamp calibration or explicit timestamp limitation.

Completed in current working tree:
- Keep AF_XDP unavailable unless a real XSK backend is integrated; config
  rejects `afxdp.enable = true` instead of routing to an incomplete receive path.
- Reject simultaneous `afxdp.enable` and `packet_mmap.enable` configuration.
- Document AF_XDP timestamp limitations and the calibration gate required before
  treating XSK latency metrics as canonical.
- Add AF_XDP queue-steering and NUMA-locality runbook guidance.
- Harden PACKET_MMAP resource handling with RAII fd/mmap cleanup, fatal fanout
  setup errors, stable per-channel fanout groups, and packet-ring bounds checks.
- Add configurable PACKET_MMAP ring geometry and length-safe IPv4/UDP payload
  extraction using IP total length and UDP length.

Remaining:
- Bind a real XSK implementation via libxdp/libbpf or an equivalent Rust XDP
  crate.
- Implement UMEM frame ownership, fill/completion ring replenishment, RX ring
  polling, and zero-copy packet handoff as part of the real XSK backend.

M6: Client Distribution
-----------------------

Status: Next

Goals:
- Low-latency binary client feed with versioned schema.
- Snapshot plus exact global replay-cursor protocol.
- Per-client backpressure isolation and slow-client eviction.
- Compatibility tests for client reconnect and gap handling.

Completed in current working tree:
- Removed the experimental HTTP/3 endpoint and unused TLS/QUIC config surface;
  WebSocket raw-v1 is the production client transport until another transport is
  implemented with matching semantics, auth, liveness, and observability.
- WebSocket clients subscribe to the live bus before snapshot serialization, so
  frames produced while a snapshot is being sent are streamed after the snapshot
  instead of being skipped.
- WebSocket feed sockets enable TCP_NODELAY by default.
- WebSocket handshakes and feed writes use configurable per-client timeouts; slow
  or stalled clients are dropped and counted instead of blocking a publisher
  thread.
- Each WebSocket A/B endpoint pair enforces a configurable connection cap before
  spawning client handler threads, covering both handshakes and established
  sessions.
- Idle WebSocket sessions now emit configurable `HEARTBEAT` control frames using
  timeout-capable pubsub receives, so liveness does not depend on market activity.
- Outbound frame and byte counters are recorded in the common WebSocket send path
  so snapshot, live, gap, and heartbeat frames share one accounting path.
- `ws_clients` now counts only established, authorized sessions; rejected
  handshakes no longer inflate the live client gauge.
- Snapshot-on-connect now fails the connection when snapshot data is unavailable
  instead of silently sending live-only data to a client that requested a full
  image.
- Snapshot files now carry the global replay cursor that immediately follows the
  image; snapshot-on-connect streams from that cursor and rejects legacy snapshots
  without cursor metadata.
- Snapshot-on-connect validates that the embedded replay cursor is still retained
  by the live bus before sending the image.
- Pubsub cursor regression tests cover snapshot-before-live delivery, evicted
  cursor gap reporting, timeout receive behavior, and per-instrument sequence
  monotonicity.
- WebSocket query parsing regressions cover reconnect cursors and snapshot flags.
- The example WebSocket client now reads raw-v1 header offsets correctly,
  validates payload length, and applies dedupe only to live OBO event frames.
- Added the implemented `HEARTBEAT` message-type constant and removed an
  undocumented control type from the wire-format notes.
- WebSocket request parsing now rejects invalid `from_seq`, invalid snapshot
  flags, unsupported channel/codec values, unsupported symbol filters, and
  unknown or duplicate query parameters instead of silently serving a different
  stream than requested.
- WebSocket request parsing rejects `snapshot=1` combined with `from_seq`,
  because snapshot replay uses the cursor embedded in the snapshot image.
- Raw-v1 frames now carry `global_sequence` in the header for live OBO events, so
  clients can persist an exact bus replay cursor and reconnect with
  `from_seq = last_global_sequence + 1`.
- Feed config parsing rejects unknown feed/POP/OBO buffer fields, so removed H3
  or TLS keys and misspelled live-feed settings fail fast instead of being
  accepted silently.
- Pubsub frame assembly now performs allocation and payload copy before taking
  the ring mutex; the lock only assigns the global cursor, writes the fixed
  header, and pushes into the ring.

Remaining:
- Add durable client replay beyond the in-memory pubsub retention window, backed
  by venue replay or the local journal.
