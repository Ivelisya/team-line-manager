#!/bin/bash
# 本机构建前端并替换 fleet-dev:/opt/fleet/web（先放到 web.new 再整体换，避免半新半旧）。fleet-server 不用重启。
# 用法：lab/deploy-web.sh
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
(cd "$ROOT/web" && pnpm build >/dev/null)
tar -C "$ROOT/web/dist" -czf - . | ssh -F "$HOME/.ssh/fleetlab_config" fleet-dev '
set -e
rm -rf /opt/fleet/web.new && mkdir -p /opt/fleet/web.new && tar --no-same-owner -xzf - -C /opt/fleet/web.new
rm -rf /opt/fleet/web.old && { [ -d /opt/fleet/web ] && mv /opt/fleet/web /opt/fleet/web.old || true; }
mv /opt/fleet/web.new /opt/fleet/web && rm -rf /opt/fleet/web.old
ls /opt/fleet/web/assets | wc -l | xargs echo "assets:"'
