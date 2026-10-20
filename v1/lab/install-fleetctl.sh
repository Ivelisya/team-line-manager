#!/bin/bash
# 在 fleet-dev 上编译 fleetctl（静态 musl 版），放到 /opt/fleet/dist/fleetctl 供下载。
# 前提：/root/src 已是要发布的代码（先跑 lab/sync-server.sh）。不动 fleet-server.service。
# 用法：lab/install-fleetctl.sh
# 取用：从 fleet-dev 上 scp /opt/fleet/dist/fleetctl 到需要的机器（Linux x86_64 静态二进制）。
#   Windows 版在本机 `cargo build -p fleetctl --release` 即得 target\release\fleetctl.exe。
set -euo pipefail
ssh -F "$HOME/.ssh/fleetlab_config" fleet-dev 'bash -s' <<'REMOTE'
set -euo pipefail
export PATH=$HOME/.cargo/bin:$PATH
cd /root/src
rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1 || true
cargo build -p fleetctl --release --target x86_64-unknown-linux-musl 2>&1 | tail -2
install -d -m 755 /opt/fleet/dist
install -m 755 target/x86_64-unknown-linux-musl/release/fleetctl /opt/fleet/dist/fleetctl
sha256sum /opt/fleet/dist/fleetctl
ls -la /opt/fleet/dist/fleetctl
/opt/fleet/dist/fleetctl --version
REMOTE
