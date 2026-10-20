#!/bin/bash
# 完整演练（a395，~/team-ops-eval/onboard 下执行）：5 台一批，注入 1 个故障（n4 密码错）+ 1 次崩溃（线路机批次 API 已加、未落盘）
# + 1 台 DD 人工闸门；证明零 Xray 重启、已有入口/分享链接不变；最后逐个新入口做 客户端 → 线路机入口 → 节点 → 出口（回显 + 公网）。
set -uo pipefail
cd "$(dirname "$0")"
say() { printf '\n\033[1m### %s\033[0m\n' "$*"; }
./lab.sh down; rm -rf lab/state lab/proof; ./lab.sh up >/dev/null && echo "实验环境已起：$(docker ps --filter name=onb- --format '{{.Names}}' | tr '\n' ' ')"
sleep 2
say "0. 现有入口基线（线路机L:20002 → 旧节点 11.99.0.30）+ 快照 + 经现有入口开 150 秒长连接"
./proof.sh e2e relay-l 20002; ./proof.sh snap before; ./proof.sh longconn-start relay-l 20002 150
say "1. plan"
./lab.sh fc plan batch.csv; H=$(./lab.sh fc plan batch.csv | grep -o 'confirm [0-9a-f]*' | cut -d' ' -f2)
say "2. run（注入崩溃：线路机批次 API 已加、落盘前进程退出）"
FLEET_ONB_CRASH=P13 ./lab.sh fc run batch.csv --confirm "$H" 2>&1 | tail -4; echo "fleetctl 退出码=${PIPESTATUS[0]}（134 = abort）"
docker exec onb-relay-l bash -c 'echo "崩溃现场：运行态 $(/opt/fleet/xray api lsi --server=127.0.0.1:62789 --isOnlyTags=true | tr -d " \n")；文件里 inbound-20003 出现 $(grep -c inbound-20003 /etc/fleet/xray/config.json) 次"'
./lab.sh fc status batch.csv
say "3. resume（等租约过期 6 秒后接管）"
sleep 7; ./lab.sh fc resume batch.csv 2>&1 | grep -vE '^[0-9:.]+Z 团队9-0[0-9] P(4|5|6|11|14) '
say "4. 处理注入的故障：给 团队9-04 补正确密码"
./lab.sh fc resume batch.csv --row 团队9-04 --password-file pw/n4.pw 2>&1 | grep -E '团队9-04 P(2|13|15|16)|第 . 批|"status"'
say "5. DD 人工闸门：在“供应商面板”重装 团队9-05 → resume（新 root 密码）"
./lab.sh reinstall-n5; sleep 2
./lab.sh fc resume batch.csv --row 团队9-05 --password-file pw/n5-new.pw 2>&1 | grep -E '团队9-05 P(4|5|6|13|15|16)|第 . 批|"status"'
say "6. 重跑同一份 CSV（应无任何变更）"
./lab.sh fc run batch.csv --confirm "$H" 2>&1 | tail -1
say "7. 证明：零 Xray 重启、已有入口与分享链接不变（配置文件 + 运行态）"
./proof.sh snap after; ./proof.sh diff before after; for r in relay-l relay-m; do echo "$r 配置文本 diff：删除 $(diff lab/proof/before/$r.config.json lab/proof/after/$r.config.json | grep -c '^<') 行 / 新增 $(diff lab/proof/before/$r.config.json lab/proof/after/$r.config.json | grep -c '^>') 行"; done
say "8. 长连接（接入全过程一直挂在现有入口上）"
sleep 5; ./proof.sh longconn-check
say "9. 端到端：客户端 → 新入口 → 节点 → 出口"
for x in "relay-l 20003 11.99.0.21" "relay-l 20004 11.99.0.22" "relay-m 20002 11.99.0.23" "relay-l 20005 11.99.0.24" "relay-l 20006 11.99.0.25"; do
  set -- $x; e=$(./proof.sh e2e $1 $2 | grep '^ip=' | cut -d= -f2); w=$(./proof.sh e2e $1 $2 https://www.cloudflare.com/cdn-cgi/trace | tr '\n' ' ')
  echo "$1:$2 → 回显出口 $e（期望 $3 $([ "$e" = "$3" ] && echo ✓ || echo ✗)）| 公网 cloudflare trace：$w"
done
say "10. 最终状态"
./lab.sh fc status batch.csv
