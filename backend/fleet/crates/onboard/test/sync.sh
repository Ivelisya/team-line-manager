#!/bin/bash
# 把 crates/onboard 同步到 a395:~/team-ops-eval/onboard/src（连同 fleet/.cargo 的 musl 构建设置）并构建。
set -euo pipefail
A395=${A395:-kismet@10.172.17.213}; KEY=${KEY:-$HOME/.ssh/a395_kismet}
HERE=$(cd "$(dirname "$0")/.." && pwd)
export COPYFILE_DISABLE=1
tar --no-xattrs -C "$HERE" --exclude=target -czf - . | ssh -i "$KEY" "$A395" 'mkdir -p ~/team-ops-eval/onboard/src && cd ~/team-ops-eval/onboard/src && tar -xzf -'
tar --no-xattrs -C "$HERE/../.." -czf - .cargo | ssh -i "$KEY" "$A395" 'cd ~/team-ops-eval/onboard/src && tar -xzf -'
tar --no-xattrs -C "$HERE/../../../architecture/plan-catalog" -czf - catalog.json | ssh -i "$KEY" "$A395" 'mkdir -p ~/team-ops-eval/onboard/dist && tar -xzf - -C ~/team-ops-eval/onboard/dist'
[ "${1:-}" = "--no-build" ] && exit 0
ssh -i "$KEY" "$A395" 'source ~/team-ops-eval/tools/env.sh && cd ~/team-ops-eval/onboard/src && CARGO_TARGET_DIR=~/team-ops-eval/onboard/target cargo build --release 2>&1 | grep -E "^(error|warning: unused)|-->|^\s+\||Finished|error\[" | head -80; ls -la ~/team-ops-eval/onboard/target/x86_64-unknown-linux-musl/release/fleetctl 2>/dev/null'
