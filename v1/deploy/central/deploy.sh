#!/bin/bash
# 生产中控部署 / 升级。在编译主机上按当前提交编译 fleet-server（glibc）、fleet-agent 与 fleetctl（后两个 musl 静态），连同 dist 来源里
# 验证过的 xray、ssserver、geoip.dat（只带与生产不同的）、本机构建的前端、给中控上 agent 的说明模板与 fleet-ops 技能打成一个包，
# 传到中控机由 install.sh 安装并重启。
# 编译主机：默认家里实验室的 fleet-dev（rustup，dist 来源 fleet-dev:/opt/fleet/dist）；FLEET_BUILD=docker FLEET_BUILD_HOST=<主机>
# 在任意一台有 docker 的机器上编（dist 来源是那台的 FLEET_BUILD_DIST，没有就不带 dist，中控上现有的保持原样）。见 lab/build-lib.sh、docs/OPS.md §9。
# 幂等，升级同样用它。编译失败或 install.sh 的健康检查不过，整个部署以非 0 退出；回滚见 docs/OPS.md §9。
# 用法：deploy/central/deploy.sh [--dry-run] <中控 ssh 主机> <域名>      例：deploy/central/deploy.sh 185.13.159.131 fleet.kismet613.com
#   --dry-run（或 FLEET_DEPLOY_DRY_RUN=1）：照常编译、打包，不连中控（dist 按全部带上算），把包存到本机临时目录、列出内容就退出
# 首次部署后建登录账号（密码从标准输入读一行，至少 10 位）：
#   printf '%s\n' '<密码>' | ssh <主机> /opt/fleet/fleet-server user-set -c /etc/fleet/fleet.toml <用户名>
set -euo pipefail
DRY=${FLEET_DEPLOY_DRY_RUN:-}
[ "${1:-}" != --dry-run ] || { DRY=1; shift; }
[ "$DRY" != 0 ] || DRY=
HOST=${1:?用法：deploy.sh [--dry-run] <中控 ssh 主机> <域名>}
DOMAIN=${2:?用法：deploy.sh [--dry-run] <中控 ssh 主机> <域名>}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
. "$ROOT/lab/build-lib.sh"
# 进包的都要已提交：deploy/ 下的单元与 logrotate 由 scripts.rs 编进 fleet-server，web 下 src 以外的文件也影响前端构建；
# fleet-ops 技能装到中控 /root/.claude/skills（docs/OPS.md §14）
SKILL="$ROOT/../.claude/skills/fleet-ops/SKILL.md"
[ -z "$(git -C "$ROOT" status --porcelain -- Cargo.toml Cargo.lock crates migrations deploy web ../.claude/skills/fleet-ops)" ] \
  || { echo "Cargo.toml / Cargo.lock / crates / migrations / deploy / web / fleet-ops 技能有未提交的改动，先提交再部署"; exit 1; }
# 版本号取最后一次改动编译输入的提交：纯前端、纯文档的提交不换哈希，编译主机上不用全量重编
GIT_HASH=$(git -C "$ROOT" log -1 --format=%h -- Cargo.toml Cargo.lock crates migrations deploy)

echo "== 编译 ${GIT_HASH}（${FB_DESC}）"
fb_sync
fb_build_release "$GIT_HASH"

# xray、ssserver、geoip.dat 只带生产上没有或与 dist 来源不同的（比对 sha256），没变的不重传（共约 22.5 MB）。
# fleet-dev 上必须有；docker 编译主机上不全就不带，install.sh 保留中控现有的
if [ "$FB_MODE" = lab ]; then
  want=$(fb_ssh "cd $FB_DIST_DIR && sha256sum xray ssserver geoip.dat")
else
  want=$(fb_ssh "cd $FB_DIST_DIR 2>/dev/null && sha256sum xray ssserver geoip.dat 2>/dev/null" || true)
  [ "$(printf '%s' "$want" | grep -c .)" = 3 ] || want=
fi
if [ -z "$want" ]; then
  DIST=
  echo "== dist 不随包：$FB_DESC 的 $FB_DIST_DIR 里没有齐 xray、ssserver、geoip.dat，中控上现有的保持原样"
else
  if [ -n "$DRY" ]; then have=
  else have=$(ssh "$HOST" 'cd /opt/fleet/dist 2>/dev/null && sha256sum xray ssserver geoip.dat 2>/dev/null' || true)
  fi
  DIST=$(comm -13 <(sort <<<"$have") <(sort <<<"$want") | awk '{print $2}' | xargs)
  echo "== dist 随包：${DIST:-无（xray、ssserver、geoip.dat 与生产相同）}${DRY:+（演练不连中控，按全部带上算）}"
fi

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/pkg/bin" "$STAGE/pkg/web"
fb_bins_tar fleet-server fleet-agent fleetctl | tar --no-same-owner -xzf - -C "$STAGE/pkg/bin"
[ -z "$DIST" ] || fb_ssh "tar -czf - -C $FB_DIST_DIR $DIST" | tar --no-same-owner -xzf - -C "$STAGE/pkg/bin"

echo "== 构建前端"
(cd "$ROOT/web" && pnpm build >/dev/null)
cp -r "$ROOT/web/dist/." "$STAGE/pkg/web/"
cp "$HERE/install.sh" "$HERE/central-agents.md" "$ROOT/deploy/ipintel/fetch-dbs.sh" "$ROOT/deploy/ipintel/fleet-ipintel-fetch.service" \
  "$ROOT/deploy/ipintel/fleet-ipintel-fetch.timer" "$ROOT/deploy/backup/fleet-backup.sh" "$ROOT/deploy/backup/fleet-backup-latest.sh" \
  "$ROOT/deploy/backup/fleet-backup.service" "$ROOT/deploy/backup/fleet-backup.timer" \
  "$HERE/tunnel-setup.sh" "$HERE/tunnel-keys" "$HERE/sshd-fleet-tunnel.conf" "$STAGE/pkg/"
cp "$SKILL" "$STAGE/pkg/fleet-ops-SKILL.md"

if [ -n "$DRY" ]; then
  OUT=$(mktemp -d)
  fb_tar -C "$STAGE/pkg" -czf "$OUT/fleet-pkg.tgz" .
  echo "== 演练：不上传、不安装。包在 $OUT/fleet-pkg.tgz，内容（前端只列个数）："
  tar -tvzf "$OUT/fleet-pkg.tgz" | grep -v ' \./web/assets/.'
  echo "web/assets 下 $(tar -tzf "$OUT/fleet-pkg.tgz" | grep -c '^\./web/assets/.') 个文件"
  exit 0
fi

echo "== 上传并安装到 $HOST"
fb_tar -C "$STAGE/pkg" -czf - . | ssh "$HOST" "set -e; trap 'rm -rf /root/fleet-pkg' EXIT; rm -rf /root/fleet-pkg; mkdir -p /root/fleet-pkg
tar --no-same-owner -xzf - -C /root/fleet-pkg
find /root/fleet-pkg -name '._*' -type f -delete
bash /root/fleet-pkg/install.sh '$DOMAIN'"
