#!/bin/bash
# 生产中控部署 / 升级。在 fleet-dev 上按当前提交编译 fleet-server 与 fleet-agent（musl），连同 fleet-dev:/opt/fleet/dist 里
# 实验室验证过的 xray、ssserver、geoip.dat（只带与生产不同的），以及本机构建的前端打成一个包，传到中控机由 install.sh 安装并重启。
# 幂等，升级同样用它。编译失败或 install.sh 的健康检查不过，整个部署以非 0 退出；回滚见 docs/OPS.md §9。
# 用法：deploy/central/deploy.sh <中控 ssh 主机> <域名>      例：deploy/central/deploy.sh debian13 fleet.kismet613.com
# 首次部署后建登录账号（密码从标准输入读一行，至少 10 位）：
#   printf '%s\n' '<密码>' | ssh <主机> /opt/fleet/fleet-server user-set -c /etc/fleet/fleet.toml <用户名>
set -euo pipefail
HOST=${1:?用法：deploy.sh <中控 ssh 主机> <域名>}
DOMAIN=${2:?用法：deploy.sh <中控 ssh 主机> <域名>}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
# 进包的都要已提交：deploy/ 下的单元与 logrotate 由 scripts.rs 编进 fleet-server，web 下 src 以外的文件也影响前端构建
[ -z "$(git -C "$ROOT" status --porcelain -- Cargo.toml Cargo.lock crates migrations deploy web)" ] \
  || { echo "Cargo.toml / Cargo.lock / crates / migrations / deploy / web 有未提交的改动，先提交再部署"; exit 1; }
# 版本号取最后一次改动编译输入的提交：纯前端、纯文档的提交不换哈希，fleet-dev 上不用全量重编
GIT_HASH=$(git -C "$ROOT" log -1 --format=%h -- Cargo.toml Cargo.lock crates migrations deploy)
LAB="ssh -F $HOME/.ssh/fleetlab_config fleet-dev"

echo "== 编译 $GIT_HASH（fleet-dev）"
bash "$ROOT/lab/sync-server.sh"
$LAB "FLEET_GIT_HASH=$GIT_HASH bash -s" <<'REMOTE'
set -euo pipefail
export PATH=$HOME/.cargo/bin:$PATH
cd /root/src
# 成功只打最后一行（用时），失败打全部输出：编译错误在前面
build() { local out; out=$(cargo build "$@" 2>&1) || { printf '%s\n' "$out"; return 1; }; printf '%s\n' "$out" | tail -1; }
build -p fleet-server --release
build -p fleet-agent --release --target x86_64-unknown-linux-musl
REMOTE

# xray、ssserver、geoip.dat 只带生产上没有或与 fleet-dev 不同的（比对 sha256），没变的不重传（共约 22.5 MB）
want=$($LAB 'cd /opt/fleet/dist && sha256sum xray ssserver geoip.dat')
have=$(ssh "$HOST" 'cd /opt/fleet/dist 2>/dev/null && sha256sum xray ssserver geoip.dat 2>/dev/null' || true)
DIST=$(comm -13 <(sort <<<"$have") <(sort <<<"$want") | awk '{print $2}' | xargs)
echo "== dist 随包：${DIST:-无（xray、ssserver、geoip.dat 与生产相同）}"

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/pkg/bin" "$STAGE/pkg/web"
$LAB "tar -czf - -C /root/src/target/release fleet-server -C /root/src/target/x86_64-unknown-linux-musl/release fleet-agent ${DIST:+-C /opt/fleet/dist $DIST}" \
  | tar --no-same-owner -xzf - -C "$STAGE/pkg/bin"

echo "== 构建前端"
(cd "$ROOT/web" && pnpm build >/dev/null)
cp -r "$ROOT/web/dist/." "$STAGE/pkg/web/"
cp "$HERE/install.sh" "$ROOT/deploy/ipintel/fetch-dbs.sh" "$ROOT/deploy/ipintel/fleet-ipintel-fetch.service" \
  "$ROOT/deploy/ipintel/fleet-ipintel-fetch.timer" "$STAGE/pkg/"

echo "== 上传并安装到 $HOST"
tar -C "$STAGE/pkg" -czf - . | ssh "$HOST" "set -e; trap 'rm -rf /root/fleet-pkg' EXIT; rm -rf /root/fleet-pkg; mkdir -p /root/fleet-pkg
tar --no-same-owner -xzf - -C /root/fleet-pkg
bash /root/fleet-pkg/install.sh '$DOMAIN'"
