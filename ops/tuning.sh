#!/usr/bin/env bash
set -euo pipefail

# Kernel and NIC tuning for low-latency market data on Linux.
# Run as root. Adjust IFACE and QUEUES to your environment.

IFACE=${IFACE:-eth0}
QUEUES=${QUEUES:-4}

echo "Applying sysctl for low latency..."
sysctl -w net.core.rmem_max=$((256*1024*1024))
sysctl -w net.core.rmem_default=$((64*1024*1024))
sysctl -w net.core.netdev_max_backlog=250000
sysctl -w net.ipv4.udp_rmem_min=$((4*1024*1024))
sysctl -w kernel.numa_balancing=0
sysctl -w kernel.sched_migration_cost_ns=5000000

echo "Disabling irqbalance and isolating CPUs is recommended (grub cmdline: isolcpus, nohz_full)."

echo "Configuring NIC RX/TX rings and offloads on ${IFACE}..."
ethtool -G ${IFACE} rx 4096 tx 4096 || true
ethtool -A ${IFACE} rx off tx off || true
ethtool -K ${IFACE} tso off gso off gro off lro off rxhash on || true

echo "Setting RSS queues to ${QUEUES} and pinning IRQs (best-effort)..."
if command -v set_irq_affinity >/dev/null 2>&1; then
  set_irq_affinity -x ${QUEUES} ${IFACE} || true
else
  echo "Install set_irq_affinity from kernel samples for precise IRQ pinning."
fi

echo "Consider enabling PTP and hardware timestamping: ptp4l + phc2sys for NIC PHC sync."
echo "Done."


