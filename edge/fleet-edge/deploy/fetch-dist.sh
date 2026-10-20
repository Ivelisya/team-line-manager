#!/bin/bash
# fetch-dist.sh — 从 a395 取回已构建的静态二进制到 fleet-edge/dist/<版本>/，并把 dist/current 指向它。
set -euo pipefail
A395=${A395:-kismet@10.172.17.213}
KEY=${KEY:-$HOME/.ssh/a395_kismet}
HERE=$(cd "$(dirname "$0")/.." && pwd)
VER=$(awk -F'"' '/^version/{print $2; exit}' "$HERE/Cargo.toml")
T=/home/kismet/team-ops-eval/fleet-edge-build/target/x86_64-unknown-linux-musl/release
TMP=$(mktemp -d)
for b in nodeagent relayd; do scp -q -i "$KEY" "$A395:$T/$b" "$TMP/$b"; done
ID=$(cat "$TMP/nodeagent" "$TMP/relayd" | shasum -a 256 | cut -c1-8)
D="$HERE/dist/$VER-$ID"
mkdir -p "$D"; mv "$TMP/nodeagent" "$TMP/relayd" "$D/"; rmdir "$TMP"
chmod 0755 "$D/nodeagent" "$D/relayd"
echo "$VER" > "$D/VERSION"
(cd "$D" && shasum -a 256 nodeagent relayd > SHA256SUMS)
ln -sfn "$(basename "$D")" "$HERE/dist/current"
ls -la "$D"; cat "$D/SHA256SUMS"
