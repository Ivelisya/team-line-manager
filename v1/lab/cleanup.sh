#!/bin/bash
# 轮末清理 fleet-dev：调试构建产物、worktree 的源码副本、测试遗留的临时文件与测试库。幂等，可反复跑。
# 有 cargo 在跑（还有人在编译或测试）就什么都不动，退出码 1。
# 不动：/root/src 的源码与 release 产物（部署要用）、fleet 库（实验室在用）、fleet_test 库（默认测试库）、/opt/fleet、fleet-sim。
# worktree 副本 /root/src-wt-<名字> 与它的测试库 fleet_test_<名字>：给了名字只删这几个；不给名字只删 6 小时没同步过的
# （同步会刷新目录的修改时间），并列出留下的——并行会话正在用的不会被误删。
# 用法：lab/cleanup.sh [名字...]
set -euo pipefail
ssh -F "$HOME/.ssh/fleetlab_config" fleet-dev "bash -s -- $*" <<'REMOTE'
set -euo pipefail
if pgrep -x cargo >/dev/null; then
  echo "有 cargo 在跑，这次不清理："
  pgrep -a -x cargo
  exit 1
fi
df -h / | awk 'NR == 2 {print "清理前：已用 " $3 "，可用 " $4}'
# 调试 / 测试构建产物（下次测试重新编）、install-ipintel 传上来的文件
rm -rf /root/src/target/debug /root/ipintel-install
# worktree 副本与它的测试库
if [ $# -gt 0 ]; then names="$*"
else names=$(find /root -maxdepth 1 -name 'src-wt-*' -type d -mmin +360 -printf '%f\n' | sed 's/^src-wt-//')
fi
for n in $names; do rm -rf "/root/src-wt-$n"; echo "已删 worktree 副本 src-wt-$n"; done
left=$(find /root -maxdepth 1 -name 'src-wt-*' -type d -printf '%f ' )
[ -z "$left" ] || echo "留下（还在用）：$left"
# 副本已经不在的 worktree 测试库 fleet_test_<名字>；fleet 与默认测试库 fleet_test 不匹配，保留
for db in $(runuser -u postgres -- psql -tAc "select datname from pg_database where datname ~ '^fleet_test_.+' order by 1"); do
  [ -d "/root/src-wt-${db#fleet_test_}" ] && continue
  runuser -u postgres -- psql -q -c "drop database \"$db\" with (force)"
  echo "已删测试库 $db"
done
# agent 单元测试的临时目录、e2e 的客户端配置与日志；mktemp 建的临时目录和导出的 SQL 只删 60 分钟以前的
rm -rf /tmp/fa-* /tmp/e2e-*
find /tmp -maxdepth 1 \( -name 'tmp.*' -o -name '*.sql' \) -mmin +60 -exec rm -rf {} +
df -h / | awk 'NR == 2 {print "清理后：已用 " $3 "，可用 " $4}'
REMOTE
