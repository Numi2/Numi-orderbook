Timestamp strategy
==================

Canonical path
--------------
- Use UDP `recvmmsg` + `SO_TIMESTAMPING` (SYS/HW/RAW) as the canonical
  timestamped RX path.
- The PACKET_MMAP fallback is used for throughput experiments and is stamped from the packet-ring timestamp.
- AF_XDP/XSK is a separate receive path. Until NIC/driver timestamp support is
  explicitly wired and calibrated for that path, treat AF_XDP timestamps as
  ingress-observation timestamps, not exchange-to-decode latency truth.

Current implementation
----------------------
- On Linux, UDP channels use `recvmmsg` when `rx_recvmmsg_batch > 1`, including
  software, hardware, and raw-hardware timestamping modes.
- The batched receive path preallocates one aligned control buffer per message
  and parses `SCM_TIMESTAMPNS` / `SCM_TIMESTAMPING` from each returned
  `mmsghdr`, preserving timestamp ancillary data without hot-loop allocation.
- `SCM_TIMESTAMPING` slot selection is based on the timestamp that is actually
  present: slot 2 is `HwRaw`, slot 1 is `HwSys`, and slot 0 remains `Sw`.
- Socket setup fails fast if requested timestamping cannot be enabled on the
  target platform. Non-Linux software or hardware timestamping is rejected
  instead of silently falling back to unstamped packets.
- Per-packet `recvmsg` remains the Linux fallback only when the configured batch
  size is one.

Unification
-----------
- Decode computes `ts_e2e` once at entry. Metrics record:
  - `e2e_latency_seconds` (all)
  - `e2e_latency_seconds_sw`, `_hw_sys`, `_hw_raw` (by source)
- Monotonicity is validated per-queue; violations increment `ts_monotonic_violations{queue}`.

Calibration
-----------
- If AF_XDP is used for e2e timing, calibrate TSC->PHC offset via
  `ptp4l`/`phc2sys` and record drift as a gauge.
- Record the timestamp source with every packet: `HwRaw` only when the NIC
  timestamp is available on the actual RX path, `HwSys` only after PHC-to-system
  conversion, and `Sw` for userspace observation time.
- Do not compare UDP hardware timestamp SLOs against AF_XDP software timestamps;
  report them as separate latency classes.

Next steps for mlx5/XSK
-----------------------
- If your kernel+mlx5 support hardware timestamps to AF_XDP, wire timestamp
  extraction into the XSK descriptor side channel and switch the canonical path
  only after drift and monotonicity checks pass.
- If hardware timestamps are unavailable for XSK, keep UDP hardware timestamping
  as the canonical latency measurement path while using AF_XDP for throughput.
