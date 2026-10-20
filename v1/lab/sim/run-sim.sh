#!/bin/bash
# 从本机（Git Bash）驱动 fleet-dev 上的模拟器 lab/sim/fleet-sim.py：先把脚本拷到 fleet-dev:/root/fleet-sim/，再执行。
# 模拟器造假机器（note='fleet-sim'、IP 在 10.77.0.0/16）并像真 agent 一样上报，用来看界面/图表/告警和服务端负载；只在实验室用。
#
# 用法：
#   lab/sim/run-sim.sh seed [--relays 6 --nodes 150 ...]   插入假机器 + 回放 24 小时历史（令牌存 fleet-dev:/root/fleet-sim/tokens.json）
#   lab/sim/run-sim.sh start [秒数，默认 60]                 以临时 systemd 单元 fleet-sim 常驻上报（崩溃自动重启，CPU 限 25%）
#   lab/sim/run-sim.sh stop                                  停止上报（机器随即会因为 120 秒没上报变成离线告警）
#   lab/sim/run-sim.sh purge                                 先 stop，再在一个事务里删除全部模拟数据
#   lab/sim/run-sim.sh status                                各表数量、在线数、未恢复告警 + 单元状态 + 最近日志
#   lab/sim/run-sim.sh logs [n]                              看 journald 里的单元日志
#   lab/sim/run-sim.sh backfill [--hours 24]                 单独补历史（一般不用）
# 其余参数原样传给 fleet-sim.py，见该文件头注释。
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
F="$HOME/.ssh/fleetlab_config"
R() { ssh -F "$F" fleet-dev "$@"; }

push() {
  R 'mkdir -p /root/fleet-sim && chmod 700 /root/fleet-sim'
  scp -q -F "$F" "$HERE/fleet-sim.py" fleet-dev:/root/fleet-sim/fleet-sim.py
}
# 把参数安全地拼成远端命令行
remote_py() { R "python3 -u /root/fleet-sim/fleet-sim.py $(printf '%q ' "$@")"; }

cmd=${1:-}; shift || true
case "$cmd" in
  seed)     push; remote_py seed "$@" ;;
  backfill) push; remote_py backfill "$@" ;;
  start)
    push
    interval=${1:-60}
    R "systemctl reset-failed fleet-sim 2>/dev/null || true
       if systemctl is-active --quiet fleet-sim; then echo 'fleet-sim 已在运行（先 stop 再 start 才会换成新脚本）'; exit 0; fi
       systemd-run --unit fleet-sim --description='fleet 实验室模拟器' --property=Restart=on-failure --property=RestartSec=10 \
         --property=CPUQuota=25% --property=MemoryMax=400M python3 -u /root/fleet-sim/fleet-sim.py run --interval $interval
       sleep 2; systemctl is-active fleet-sim; journalctl -u fleet-sim -n 5 --no-pager -o cat" ;;
  stop)     R 'systemctl stop fleet-sim 2>/dev/null || true; systemctl reset-failed fleet-sim 2>/dev/null || true; echo stopped' ;;
  purge)    push; R 'systemctl stop fleet-sim 2>/dev/null || true; systemctl reset-failed fleet-sim 2>/dev/null || true'
            remote_py purge "$@" ;;
  status)   push; remote_py status "$@"
            R 'echo "--- 单元状态：$(systemctl is-active fleet-sim 2>&1 || true)"; journalctl -u fleet-sim -n 8 --no-pager -o cat 2>/dev/null || true' ;;
  logs)     R "journalctl -u fleet-sim -n ${1:-50} --no-pager -o cat" ;;
  *) sed -n 2,14p "$0"; exit 2 ;;
esac
