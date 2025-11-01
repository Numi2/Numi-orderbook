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

NUMA
----

Ensure NIC, UMEM, and threads are on the same NUMA node. Check NIC node:

```bash
cat /sys/class/net/eth0/device/numa_node
cat /sys/devices/system/node/node${N}/cpulist
```

Power/governor
--------------

Set `performance` governor; limit C-states to C1 for hot cores. Disable turbo fluctuations if detrimental to tail latency.

Ring sizing cheatsheet
----------------------

- AF_XDP per RXQ UMEM: 256 MB with 2 KB frames (~128K frames)
- Rings per queue: RX 2048, FILL 8192, CQ 4096, TX 512
- Target batch: 32–64 frames/poll cycle




