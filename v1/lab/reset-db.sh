#!/bin/bash
# 清空实验室数据库（保留密钥、管理员密码文件），重启服务后重建管理员并导入套餐目录。
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ssh -F "$HOME/.ssh/fleetlab_config" fleet-dev 'bash -s' <<'REMOTE'
set -euo pipefail
systemctl stop fleet-server
runuser -u postgres -- psql -q fleet -c 'drop schema public cascade; create schema public authorization fleet;'
: > /var/lib/fleet/known_hosts
systemctl start fleet-server; sleep 3
/opt/fleet/fleet-server user-set -c /etc/fleet/fleet.toml admin < /root/fleet-admin-password
REMOTE
bash "$HERE/import-catalog.sh"
