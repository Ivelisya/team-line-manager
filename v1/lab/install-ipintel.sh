#!/bin/bash
# 在 fleet-dev 上安装 IP 情报离线库的下载脚本和每月定时器，并立即下载一次（约 190 MB，落盘约 230 MB，需要几分钟）。
# 不动 fleet-server.service；库下载完 5 分钟内 fleet-server 自动重新加载（比对文件 mtime），不用重启。
# 幂等：重复运行会再下载一次（已是最新的库会跳过）。
# 用法：lab/install-ipintel.sh
# 之后检查：fleetctl ipintel-status（loaded 应为 5），或 GET /api/v1/ipintel/status。
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SSH="ssh -F $HOME/.ssh/fleetlab_config fleet-dev"
tar -C "$ROOT/deploy/ipintel" -czf - fetch-dbs.sh fleet-ipintel-fetch.service fleet-ipintel-fetch.timer \
  | $SSH 'rm -rf /root/ipintel-install && mkdir -p /root/ipintel-install && tar --no-same-owner -xzf - -C /root/ipintel-install'
$SSH 'bash -s' <<'REMOTE'
set -euo pipefail
cd /root/ipintel-install
# fetch-dbs.sh 需要的工具（Debian/Ubuntu 自带，缺了才装）
for t in curl gzip sha1sum sha256sum od awk; do command -v "$t" >/dev/null || { apt-get install -y -qq curl coreutils gzip gawk >/dev/null; break; }; done
id fleet >/dev/null 2>&1 || { echo "没有 fleet 用户：先跑 lab/deploy-server.sh"; exit 1; }
install -d -m 755 /opt/fleet/ipintel
install -m 644 fetch-dbs.sh /opt/fleet/ipintel/fetch-dbs.sh
install -d -m 750 -o fleet -g fleet /var/lib/fleet /var/lib/fleet/ipintel
install -m 644 fleet-ipintel-fetch.service fleet-ipintel-fetch.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now fleet-ipintel-fetch.timer
echo "--- 立即下载一次 ---"
if systemctl start fleet-ipintel-fetch.service; then
  echo "下载完成"
else
  echo "下载失败，最近日志："
  journalctl -u fleet-ipintel-fetch -n 40 --no-pager || true
  exit 1
fi
ls -la /var/lib/fleet/ipintel
cat /var/lib/fleet/ipintel/MANIFEST 2>/dev/null || true
systemctl list-timers fleet-ipintel-fetch.timer --no-pager
REMOTE
