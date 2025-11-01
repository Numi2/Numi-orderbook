SLO and validation gates
========================

Cold start
----------
- No page faults after 5s. Verify `minflt`/`majflt` stable at zero while replaying.

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


