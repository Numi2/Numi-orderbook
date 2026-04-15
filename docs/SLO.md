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
- Merge-stage duplicate, stale, and out-of-window gap drops must recycle packet
  buffers when the packet pool is available to merge.

Local development gates
-----------------------
- `NUMI_BENCH_SMOKE=1 cargo bench --bench hot_paths -- --quiet` must complete.
- `cargo run --release --bin bench_pipeline -- local-core` must report
  `status=ok`, `sequence_gaps=0`, `dup_or_ooo=0`, `event_vec_reallocs=0`, and
  `pool_available=pool_size`.
- `cargo run --release --bin pool_soak -- 65536 2048 10000 64` must report
  `misses=0` and `return_drops=0`.
- `cargo run --release --bin rx_probe -- 100000 64 32 software 0` must report
  `missing=0`, `seq_gaps=0`, and `dup_or_ooo=0`.
- Local benchmark throughput is host-noise-sensitive and is not a production
  SLO claim.

Target-hardware benchmark gates
-------------------------------
- `cargo run --release --bin bench_pipeline -- target-rx --config config.toml
  --duration-sec 60 --packets 892800000` while production-like multicast
  traffic is present. For 10GbE 64-byte payload class, require 14.88 Mpps for
  60 seconds, zero app drops, and latency buckets by timestamp source.
- `cargo run --release --bin bench_pipeline -- target-rx --config config.toml
  --duration-sec 30 --packets 1125000000` for the 25GbE profile. Require 37.5
  Mpps for 30 seconds and zero drops.
- `cargo run --release --bin bench_pipeline -- target-failover-recovery` must
  report `status=ok`, no sequence gaps, no duplicate/out-of-order output, and
  recovery under 100 ms for the injected gap.
- `cargo run --release --bin bench_pipeline -- target-persistence --packets
  1024` must report `status=ok`, `anchored=true`, no non-monotonic journal
  records, and matching final/restored state hashes.
- Target results must record git SHA, rustc, allocator, OS/kernel, CPU, NIC,
  timestamp source, config path, and config hash from the benchmark output.

Verification record
-------------------
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
- 2026-04-15: `NUMI_BENCH_SMOKE=1 cargo bench --bench hot_paths -- --quiet`
  completed the Criterion smoke microbenchmarks.
- 2026-04-15: `cargo run --release --bin bench_pipeline -- local-core --packets
  512` reported `status=ok`, `sequence_gaps=0`, `dup_or_ooo=0`,
  `event_vec_reallocs=0`, and `pool_available=pool_size`.
- 2026-04-15: `cargo run --release --bin bench_pipeline --
  target-failover-recovery` reported `status=ok` for a 1,000-message gap with
  `pool_available=pool_size`.
- 2026-04-15: `docker build -t numi-orderbook:local .` passed.
