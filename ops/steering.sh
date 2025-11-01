#!/usr/bin/env bash
set -euo pipefail

# Deterministic multicast steering for mlx5 via tc flower (HW offload).
# Requirements: tc, mlx5 with flower offload, root privileges.
# Usage:
#   IFACE=eth0 ./ops/steering.sh add 239.10.10.1 5001 0   # feed A -> RXQ0
#   IFACE=eth0 ./ops/steering.sh add 239.10.10.2 5002 1   # feed B -> RXQ1
#   IFACE=eth0 ./ops/steering.sh clear

IFACE=${IFACE:-eth0}
CMD=${1:-}

ensure_clsact() {
  tc qdisc show dev "$IFACE" | grep -q clsact || tc qdisc add dev "$IFACE" clsact || true
}

if [[ "$CMD" == "add" ]]; then
  GROUP=$2
  PORT=$3
  QUEUE=$4
  ensure_clsact
  # Add HW offloaded rule: dst_ip + UDP dst_port -> RX queue
  tc filter add dev "$IFACE" ingress protocol ip prio 10 flower \
    ip_proto udp dst_ip "$GROUP" dst_port "$PORT" \
    action skbedit queue_mapping "$QUEUE" hw_stats delayed skip_sw
  echo "Added steering: $GROUP:$PORT -> queue $QUEUE on $IFACE"
elif [[ "$CMD" == "clear" ]]; then
  tc filter del dev "$IFACE" ingress || true
  echo "Cleared steering rules on $IFACE"
else
  echo "Usage: IFACE=eth0 $0 add <group> <port> <queue>|clear" >&2
  exit 2
fi


