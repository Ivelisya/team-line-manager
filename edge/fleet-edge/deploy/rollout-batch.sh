#!/bin/bash
# rollout-batch.sh <名字正则> [rollout.sh 的其他选项]
# 按清单（jump ip name keyfile）逐台执行 rollout.sh，串行，遇到失败就停（先看清楚再继续）。
#   deploy/rollout-batch.sh '^团队1-2[1-5]$'
#   deploy/rollout-batch.sh '^团队1-(2[6-9]|3[0-6])$'
#   deploy/rollout-batch.sh '.' --remove
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
INVENTORY=${INVENTORY:-/private/tmp/claude-501/-Users-kismet-Desktop-leap/342586b4-73dc-433e-a4bc-cb91efbdbfe7/scratchpad/hosts-keys.txt}
PAT=${1:?用法：rollout-batch.sh <名字正则> [选项]}; shift
ok=(); fail=()
while read -r jump ip name key; do
  [ -z "$jump" ] || [[ "$jump" == \#* ]] && continue
  [[ "$name" =~ $PAT ]] || continue
  if "$HERE/rollout.sh" --key "$key" "$@" "$name" "$ip" "$jump" < /dev/null; then ok+=("$name"); else fail+=("$name"); echo "== $name 失败，停止（修好后重跑同一命令，已完成的主机会被跳过为 unchanged）"; break; fi
done < "$INVENTORY"
echo "成功 ${#ok[@]}：${ok[*]:-}"
echo "失败 ${#fail[@]}：${fail[*]:-}"
[ ${#fail[@]} -eq 0 ]
