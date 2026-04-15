Low‑latency ops tuning (Linux)
================================

Summary of recommended settings for co‑lo/low‑latency deployments:

- CPU: Isolate dedicated cores for RX/merge/decode (kernel cmdline: `isolcpus=`, `nohz_full=`, disable SMT on hot cores).
- Scheduling: Use `SCHED_FIFO` for hot threads (already supported via `config.cpu.rt_priority`).
- Memory: Enable `mlockall` (`general.mlock_all=true`) and HugeTLB if using custom allocators. Pre‑warm packet pool (enabled).
- NIC: Enable RSS with N queues, set large RX ring, disable GRO/LRO/TSO.
- IRQ affinity: Pin NIC RX IRQs to the same NUMA cores as the app threads.
- Time sync: Run `ptp4l` and `phc2sys` to sync NIC PHC to system clock; enable hardware RX timestamping in config.
- Network: Set high UDP rmem, grow `netdev_max_backlog`.

Script
------

Use `ops/tuning.sh` as a starting point (run as root):

```bash
sudo IFACE=eth0 QUEUES=4 ./ops/tuning.sh
```

Systemd
-------

Consider adding CPUAffinity and MemoryDenyWriteExecute to `systemd-orderbook.service`, and disable `irqbalance` for precise IRQ pinning.

Multicast steering
------------------

Default RSS may not shard multicast. Use `ops/steering.sh` to program HW steering per (VLAN?, dst_ip, dst_port) into dedicated RX queues. Example:

```bash
sudo IFACE=eth0 ./ops/steering.sh add 239.10.10.1 5001 0  # A -> RXQ0
sudo IFACE=eth0 ./ops/steering.sh add 239.10.10.2 5002 1  # B -> RXQ1
```

PACKET_MMAP queue plan
----------------------

`[packet_mmap] enable = true` replaces channel A socket receive with
PACKET_RX_RING workers. Channel A spawns one worker per `packet_mmap.queues`.
Each worker joins the same PACKET_FANOUT group for the configured interface and
channel, so NIC steering should send feed A traffic only to the queues intended
for those workers.

Before enabling PACKET_MMAP:

- Program NIC steering so feed A multicast packets land only on the queues used
  by the PACKET_MMAP workers.
- Pin `cpu.a_rx_core + N` to the same NUMA node as queue `N`.
- Size `packet_mmap.frame_size`, `packet_mmap.frames_per_block`, and
  `packet_mmap.block_count` so the ring can absorb scheduler jitter without
  adding unnecessary cache and TLB pressure.
- Do not share an RX queue between UDP socket receive and PACKET_MMAP consumers.
- Keep `afxdp.enable = false`; config validation rejects AF_XDP until a real
  XSK/UMEM backend is integrated.

Example queue split:

```bash
sudo IFACE=eth0 ./ops/steering.sh add 239.10.10.1 5001 0
sudo IFACE=eth0 ./ops/steering.sh add 239.10.10.1 5001 1
```

AF_XDP/XSK readiness
--------------------

AF_XDP is intentionally unavailable in the current binary. Do not enable
`afxdp.enable` in production configs; validation fails by design. A real backend
must add XSK socket binding, UMEM fill/completion ownership, zero-copy packet
handoff, queue-local memory, and timestamp calibration before it can replace the
canonical UDP timestamped path.

NUMA
----

Ensure NIC queues, packet rings, and hot threads are on the same NUMA node. Check
NIC node:

```bash
cat /sys/class/net/eth0/device/numa_node
cat /sys/devices/system/node/node${N}/cpulist
```

Power/governor
--------------

Set `performance` governor; limit C-states to C1 for hot cores. Disable turbo fluctuations if detrimental to tail latency.

Ring sizing cheatsheet
----------------------

- PACKET_MMAP default per worker: 4 blocks * 1024 frames/block * 2048 bytes =
  8 MiB mapped ring.
- Increase `packet_mmap.block_count` before increasing frame size when handling
  burst absorption for normal Ethernet MTU traffic.
- Future AF_XDP/XSK target: per-RXQ UMEM around 256 MiB with 2 KiB frames
  (~128K frames), RX ring 2048, fill ring 8192, completion ring 4096.
- Target receive batch: 32-64 frames/poll cycle.

