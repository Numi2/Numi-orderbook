Darwin UDP receive path
=======================

Scope
-----
- This path is for macOS development and local performance work.
- It is not AF_XDP, PACKET_MMAP, hardware timestamping, or a production Linux
  venue latency proof.
- Use `config.darwin.toml` to avoid Linux-only options such as `SO_BUSY_POLL`
  and `mlockall`.

Receive strategy
----------------
- `src/rx_darwin_udp.rs` uses the Darwin `recvmsg_x` syscall when
  `general.rx_recvmmsg_batch > 1`.
- `recvmsg_x` receives several UDP datagrams into an array of `msghdr_x`
  descriptors. Each descriptor points directly at a checked-out packet-pool
  buffer, so the downstream packet ownership model stays identical to the Linux
  UDP path.
- If the kernel or protocol refuses `recvmsg_x`, the loop logs once by disabling
  the batch path for that socket and continues with nonblocking `recvmsg`.
- Truncated datagrams are dropped and counted through the RX drop metric; they
  are never forwarded with partial payloads.

Timestamp strategy
------------------
- macOS socket setup accepts `timestamping = "software"` and enables
  `SO_TIMESTAMP_MONOTONIC`.
- IPv4 and IPv6 input attach `SCM_TIMESTAMP_MONOTONIC` as a `uint64_t`
  `mach_absolute_time()` tick value.
- `util::now_nanos()` uses the same Mach absolute clock on macOS, converted
  through `mach_timebase_info`, so RX timestamps and userspace stage timestamps
  share one monotonic clock domain.
- `timestamping = "hardware"` and `timestamping = "hardware_raw"` fail fast on
  macOS. There is no silent downgrade.

Run
---

```bash
cargo run --release --locked -- config.darwin.toml
```

Validation
----------
- Darwin timestamp parser tests cover Mach tick conversion, control truncation,
  and the zeroed-control-buffer case required by older `recvmsg_x` behavior.
- `rx_probe` exercises the platform UDP receive loop over loopback and fails on
  missing packets, sequence gaps, duplicate/out-of-order delivery, or send
  errors:

```bash
cargo run --release --bin rx_probe -- 100000 64 32 software 0
```

- The regular Rust suite compiles and runs this path on macOS:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```
