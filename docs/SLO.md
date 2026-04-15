SLO and validation gates
========================

Cold start
----------
- No page faults after 5s. Verify `minflt`/`majflt` stable at zero while replaying.
- Packet pool is fully preallocated and touched during startup. During soak,
  `packet_pool_misses_total` and `packet_pool_return_drops_total` must remain
  zero.

10GbE small packet (64B UDP payload class)
------------------------------------------
- 14.88 Mpps for 60s, zero app-level drops
- p50 < 9 µs, p99 < 40 µs, p99.9 < 60 µs (decode entry, ts_rx_hw or calibrated ts_sw)

25GbE
-----
- 37.5 Mpps for 30s, zero drops, p99 < 80 µs

Failover
--------
- Hard cut of feed A for 200 ms. Switch to B within dwell. No reordering beyond window. Zero duplicates.

Recovery
--------
- Inject 1,000 message gap. Replay fills within 100 ms. No duplicate events. Sequence strictly monotonic after merge.

Packet ownership
----------------
- RX queue-full drops must recycle packet buffers before incrementing drop
  counters.
- Batched UDP receive must recycle every prepared buffer when `recvmmsg` makes
  no progress or returns a fatal error.
- Timestamped UDP receive must stay on the batched `recvmmsg` path when
  `rx_recvmmsg_batch > 1` and must classify `SCM_TIMESTAMPING` slots by the
  actual timestamp returned by the kernel.
- PACKET_MMAP drops must recycle the copied packet buffer before releasing the
  kernel frame.

Local development gates
-----------------------
- 2026-04-15: `cargo clippy --all-targets --all-features -- -D warnings`
  passed.
- 2026-04-15: `RUSTFLAGS='' cargo check --target x86_64-unknown-linux-gnu
  --all-targets` passed.
- 2026-04-15: `RUSTFLAGS='' cargo clippy --target x86_64-unknown-linux-gnu
  --all-targets -- -D warnings` passed.
- 2026-04-15: `cargo test --all-features` passed with 52 library tests plus all
  binary and doc-test targets.
- 2026-04-15: `cargo build --release` passed.
- 2026-04-15: `cargo run --release --bin pool_soak -- 65536 2048 10000 64`
  completed 640,000 operations with `misses=0` and `return_drops=0`.
- 2026-04-15: `cargo run --release --bin bench_orderbook -- 64 10000 64`
  processed 1,173,376 synthetic events successfully on the local development
  host. This is a smoke benchmark, not a substitute for the hardware SLO runs
  above, and local throughput samples are treated as host-noise-sensitive.
- 2026-04-15: `docker build -t numi-orderbook:local .` passed.
