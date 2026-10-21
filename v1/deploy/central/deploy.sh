#!/bin/bash
# 生产中控部署 / 升级。在 fleet-dev 上按当前提交编译 fleet-server 与 fleet-agent（musl），连同 fleet-dev:/opt/fleet/dist 里
# 实验室验证过的 xray、ssserver、geoip.dat，以及本机构建的前端打成一个包，传到中控机由 install.sh 安装并重启。幂等，升级同样用它。
# 用法：deploy/central/deploy.sh <中控 ssh 主机> <域名>      例：deploy/central/deploy.sh debian13 fleet.kismet613.com
# 首次部署后建登录账号（密码从标准输入读一行，至少 10 位）：
#   printf '%s\n' '<密码>' | ssh <主机> /opt/fleet/fleet-server user-set -c /etc/fleet/fleet.toml <用户名>
set -euo pipefail
HOST=${1:?用法：deploy.sh <中控 ssh 主机> <域名>}
DOMAIN=${2:?用法：deploy.sh <中控 ssh 主机> <域名>}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
[ -z "$(git -C "$ROOT" status --porcelain -- crates migrations web/src)" ] || { echo "crates / migrations / web 有未提交的改动，先提交再部署"; exit 1; }
GIT_HASH=$(git -C "$ROOT" rev-parse --short HEAD)
LAB="ssh -F $HOME/.ssh/fleetlab_config fleet-dev"

echo "== 编译 $GIT_HASH（fleet-dev）"
bash "$ROOT/lab/sync-server.sh"
$LAB "cd /root/src && export PATH=\$HOME/.cargo/bin:\$PATH FLEET_GIT_HASH=$GIT_HASH
cargo build -p fleet-server --release 2>&1 | tail -1
cargo build -p fleet-agent --release --target x86_64-unknown-linux-musl 2>&1 | tail -1"

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/pkg/bin" "$STAGE/pkg/web"
$LAB 'tar -czf - -C /root/src/target/release fleet-server -C /root/src/target/x86_64-unknown-linux-musl/release fleet-agent -C /opt/fleet/dist xray ssserver geoip.dat' \
  | tar --no-same-owner -xzf - -C "$STAGE/pkg/bin"

echo "== 构建前端"
(cd "$ROOT/web" && pnpm build >/dev/null)
cp -r "$ROOT/web/dist/." "$STAGE/pkg/web/"
cp "$HERE/install.sh" "$ROOT/deploy/ipintel/fetch-dbs.sh" "$ROOT/deploy/ipintel/fleet-ipintel-fetch.service" \
  "$ROOT/deploy/ipintel/fleet-ipintel-fetch.timer" "$STAGE/pkg/"

echo "== 上传并安装到 $HOST"
tar -C "$STAGE/pkg" -czf - . | ssh "$HOST" "set -e; rm -rf /root/fleet-pkg; mkdir -p /root/fleet-pkg
tar --no-same-owner -xzf - -C /root/fleet-pkg
bash /root/fleet-pkg/install.sh '$DOMAIN'
rm -rf /root/fleet-pkg"
