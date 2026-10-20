#!/bin/bash
# Create fleet lab containers on PVE. Idempotent: skips existing IDs.
set -euo pipefail
T=local:vztmpl/debian-13-standard_13.6-1_amd64.tar.zst
mk() { # id host ip cores mem disk
  local id=$1 host=$2 ip=$3 cores=$4 mem=$5 disk=$6
  if pct status "$id" >/dev/null 2>&1; then echo "skip $id (exists)"; return; fi
  pct create "$id" "$T" --hostname "$host" --cores "$cores" --memory "$mem" --swap 512 \
    --rootfs "local-lvm:$disk" --net0 "name=eth0,bridge=vmbr0,ip=192.168.31.$ip/24,gw=192.168.31.2" \
    --nameserver 192.168.31.2 --unprivileged 1 --features nesting=1 --onboot 0 \
    --ssh-public-keys /root/fleetlab.pub --tags fleetlab --description "fleet v3 lab (Claude)"
  pct start "$id"
  echo "created $id $host 192.168.31.$ip"
}
mk 160 fleet-dev    60 6 4096 40
mk 161 lab-relay1   61 1 512  8
mk 162 lab-relay2   62 1 512  8
mk 163 lab-node1    63 1 256  4
mk 164 lab-node2    64 1 256  4
mk 165 lab-node3    65 1 256  4
sleep 5
pct list | grep -E '^16[0-9]'
