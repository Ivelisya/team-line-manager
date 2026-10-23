#!/bin/bash
# 本机构建前端并替换 fleet-dev:/opt/fleet/web（先放到 web.new 再整体换，避免半新半旧）。fleet-server 不用重启。
# 前端总在本机构建（pnpm），不经编译主机，FLEET_BUILD 对它不起作用（lab/build-lib.sh 只用来打 tar）；部署目标总是 fleet-dev。
# 用法：lab/deploy-web.sh
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
. "$ROOT/lab/build-lib.sh"   # fb_tar：macOS 上打 tar 不带 ._* 与扩展属性
(cd "$ROOT/web" && pnpm build >/dev/null)
fb_tar -C "$ROOT/web/dist" -czf - . | ssh -F "$HOME/.ssh/fleetlab_config" fleet-dev '
set -e
rm -rf /opt/fleet/web.new && mkdir -p /opt/fleet/web.new && tar --no-same-owner -xzf - -C /opt/fleet/web.new
find /opt/fleet/web.new -name "._*" -type f -delete
rm -rf /opt/fleet/web.old && { [ -d /opt/fleet/web ] && mv /opt/fleet/web /opt/fleet/web.old || true; }
mv /opt/fleet/web.new /opt/fleet/web && rm -rf /opt/fleet/web.old
ls /opt/fleet/web/assets | wc -l | xargs echo "assets:"'
