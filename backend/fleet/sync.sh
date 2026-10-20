#!/bin/bash
# sync.sh — 把 fleet/ 工作区（+ fleet-edge/fleetd-ingest）同步到 a395 的 ~/team-ops-eval/fleet-build/（不含 target、真实夹具）。
set -euo pipefail
A395=${A395:-kismet@10.172.17.213}
KEY=${KEY:-$HOME/.ssh/a395_kismet}
HERE=$(cd "$(dirname "$0")" && pwd)
export COPYFILE_DISABLE=1
ssh -i "$KEY" "$A395" 'mkdir -p ~/team-ops-eval/fleet-build/src ~/team-ops-eval/fleet-build/fleet-edge'
tar --no-xattrs -C "$HERE" --exclude=target --exclude='*.local' --exclude='fixtures' --exclude=dist -czf - . | ssh -i "$KEY" "$A395" 'cd ~/team-ops-eval/fleet-build/src && tar -xzf -'
tar --no-xattrs -C "$HERE/../fleet-edge" --exclude=target -czf - fleetd-ingest | ssh -i "$KEY" "$A395" 'cd ~/team-ops-eval/fleet-build/fleet-edge && tar -xzf -'
tar --no-xattrs -C "$HERE/../architecture/plan-catalog" -czf - catalog.json | ssh -i "$KEY" "$A395" 'mkdir -p ~/team-ops-eval/fleet-build/architecture/plan-catalog && tar -xzf - -C ~/team-ops-eval/fleet-build/architecture/plan-catalog'
echo synced
