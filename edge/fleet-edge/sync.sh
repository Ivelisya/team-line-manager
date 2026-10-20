#!/bin/bash
# sync.sh — 把 fleet-edge/ 同步到 a395 的 ~/team-ops-eval/fleet-edge-build/src（不含 target、dist），
# 并把 path 依赖 fleet/crates/probe 同步到 ~/team-ops-eval/fleet-edge-build/fleet/crates/probe。
set -euo pipefail
A395=${A395:-kismet@10.172.17.213}
KEY=${KEY:-$HOME/.ssh/a395_kismet}
HERE=$(cd "$(dirname "$0")" && pwd)
export COPYFILE_DISABLE=1
ssh -i "$KEY" "$A395" 'mkdir -p ~/team-ops-eval/fleet-edge-build/src ~/team-ops-eval/fleet-edge-build/fleet/crates/probe'
tar --no-xattrs -C "$HERE" --exclude=target --exclude=dist --exclude='*.local' -czf - . | ssh -i "$KEY" "$A395" 'cd ~/team-ops-eval/fleet-edge-build/src && tar -xzf -'
tar --no-xattrs -C "$HERE/../fleet/crates/probe" --exclude=target -czf - . | ssh -i "$KEY" "$A395" 'cd ~/team-ops-eval/fleet-edge-build/fleet/crates/probe && tar -xzf -'
echo synced
