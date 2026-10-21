#!/bin/bash
# 轮末清理 fleet-dev：调试构建产物、各 worktree 的源码副本、测试遗留的临时文件与测试库。幂等，可反复跑。
# 有 cargo 在跑（还有人在编译或测试）就什么都不动，退出码 1。
# 不动：/root/src 的源码与 release 产物（部署要用）、fleet 库（实验室在用）、fleet_test 库（默认测试库）、/opt/fleet、fleet-sim。
# 用法：lab/cleanup.sh
set -euo pipefail
ssh -F "$HOME/.ssh/fleetlab_config" fleet-dev 'bash -s' <<'REMOTE'
set -euo pipefail
if pgrep -x cargo >/dev/null; then
  echo "有 cargo 在跑，这次不清理："
  pgrep -a -x cargo
  exit 1
fi
df -h / | awk 'NR == 2 {print "清理前：已用 " $3 "，可用 " $4}'
# 调试 / 测试构建产物（下次测试重新编）、各 worktree 的源码副本、install-ipintel 传上来的文件
rm -rf /root/src/target/debug /root/src-wt-* /root/ipintel-install
# agent 单元测试的临时目录、e2e 的客户端配置与日志；mktemp 建的临时目录和导出的 SQL 只删 60 分钟以前的
rm -rf /tmp/fa-* /tmp/e2e-*
find /tmp -maxdepth 1 \( -name 'tmp.*' -o -name '*.sql' \) -mmin +60 -exec rm -rf {} +
# 各 worktree 自己的测试库 fleet_test_*；fleet 与默认测试库 fleet_test 不匹配，保留
dbs=$(runuser -u postgres -- psql -tAc "select datname from pg_database where datname ~ '^fleet_test_.+' order by 1")
for db in $dbs; do
  runuser -u postgres -- psql -q -c "drop database \"$db\" with (force)"
  echo "已删测试库 $db"
done
df -h / | awk 'NR == 2 {print "清理后：已用 " $3 "，可用 " $4}'
REMOTE
