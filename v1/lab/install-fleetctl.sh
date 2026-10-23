#!/bin/bash
# 编译 fleetctl（静态 musl 版），放到 fleet-dev:/opt/fleet/dist/fleetctl 供下载。
# 前提：编译主机上已是要发布的代码（先跑 lab/sync-server.sh）。不动 fleet-server.service。
# 编译主机：默认在 fleet-dev 上编；FLEET_BUILD=docker 时在 docker 编译主机的 musl 容器里编，经本机传到 fleet-dev（lab/build-lib.sh）。
# 用法：lab/install-fleetctl.sh
# 取用：从 fleet-dev 上 scp /opt/fleet/dist/fleetctl 到需要的机器（Linux x86_64 静态二进制）。
#   Windows 版在本机 `cargo build -p fleetctl --release` 即得 target\release\fleetctl.exe。
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/build-lib.sh"
LAB="ssh -F $HOME/.ssh/fleetlab_config fleet-dev"
fb_build_fleetctl
if [ "$FB_MODE" = lab ]; then
  $LAB 'install -d -m 755 /opt/fleet/dist && install -m 755 /root/src/target/x86_64-unknown-linux-musl/release/fleetctl /opt/fleet/dist/fleetctl'
else
  # 先放到 .new 再改名：正在被下载的旧文件不会被写到一半
  fb_bins_tar fleetctl | $LAB 'install -d -m 755 /opt/fleet/dist && tar --no-same-owner -xzf - -O fleetctl > /opt/fleet/dist/fleetctl.new && chmod 755 /opt/fleet/dist/fleetctl.new && mv -f /opt/fleet/dist/fleetctl.new /opt/fleet/dist/fleetctl'
fi
$LAB 'sha256sum /opt/fleet/dist/fleetctl; ls -la /opt/fleet/dist/fleetctl; /opt/fleet/dist/fleetctl --version'
