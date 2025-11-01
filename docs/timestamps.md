Timestamp strategy
==================

Canonical path
--------------
- Use UDP `recvmmsg` + `SO_TIMESTAMPING` (SYS/HW/RAW) as the canonical timestamped RX path.
- AF_XDP is used for throughput; until mlx5 exposes RX timestamps to XSK in your kernel, AF_XDP frames are stamped with local TSC.

Unification
-----------
- Decode computes `ts_e2e` once at entry. Metrics record:
  - `e2e_latency_seconds` (all)
  - `e2e_latency_seconds_sw`, `_hw_sys`, `_hw_raw` (by source)
- Monotonicity is validated per-queue; violations increment `ts_monotonic_violations{queue}`.

Calibration
-----------
- If AF_XDP is used for e2e timing, calibrate TSC->PHC offset via `ptp4l`/`phc2sys` and record drift as a gauge (future work).

Next steps for mlx5/XSK
-----------------------
- If your kernel+mlx5 support HW timestamps to AF_XDP, wire it and switch canonical path to XDP. Otherwise keep UDP canonical.


