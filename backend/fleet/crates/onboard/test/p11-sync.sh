#!/bin/bash
# P11 演练专用：同步 fleet/ + fleet-edge/（全量，不含 target/dist）到 a395:~/team-ops-eval/fleet-build-p11/，并放上 nodeagent 发布目录与演练脚本。
# 不碰 ~/team-ops-eval/fleet-build/src、fleet-stage。之后在 a395：cd ~/team-ops-eval/fleet-build-p11 && ./p11-lab.sh up && ./p11-e2e.sh
set -euo pipefail
A395=kismet@10.172.17.213; KEY=$HOME/.ssh/a395_kismet
T=$HOME/Desktop/team-ops-v2
export COPYFILE_DISABLE=1
ssh -i "$KEY" "$A395" 'mkdir -p ~/team-ops-eval/fleet-build-p11/src ~/team-ops-eval/fleet-build-p11/fleet-edge ~/team-ops-eval/fleet-build-p11/architecture/plan-catalog'
tar --no-xattrs -C "$T/fleet" --exclude=target --exclude='*.local' --exclude='fixtures' --exclude=dist -czf - . | ssh -i "$KEY" "$A395" 'cd ~/team-ops-eval/fleet-build-p11/src && tar -xzf -'
tar --no-xattrs -C "$T/fleet-edge" --exclude=target --exclude=dist --exclude=runs -czf - . | ssh -i "$KEY" "$A395" 'cd ~/team-ops-eval/fleet-build-p11/fleet-edge && tar -xzf -'
tar --no-xattrs -C "$T/architecture/plan-catalog" -czf - catalog.json | ssh -i "$KEY" "$A395" 'tar -xzf - -C ~/team-ops-eval/fleet-build-p11/architecture/plan-catalog'
tar --no-xattrs -C "$T/fleet-edge/dist/current/" -czf - nodeagent VERSION SHA256SUMS | ssh -i "$KEY" "$A395" 'mkdir -p ~/team-ops-eval/fleet-build-p11/edge-dist && tar -xzf - -C ~/team-ops-eval/fleet-build-p11/edge-dist'

scp -q -i "$KEY" "$T/fleet/crates/onboard/test/p11-lab.sh" "$T/fleet/crates/onboard/test/p11-e2e.sh" "$A395:team-ops-eval/fleet-build-p11/"
[ "${1:-}" = "--no-build" ] && exit 0
ssh -i "$KEY" "$A395" 'source ~/team-ops-eval/tools/env.sh && cd ~/team-ops-eval/fleet-build-p11/src && CARGO_TARGET_DIR=~/team-ops-eval/fleet-build-p11/target cargo build --release -p fleet-onboard -p fleetd 2>&1 | grep -E "^error|-->|Finished"'
