#!/bin/bash
# 把 lab/seed/catalog.json（供应商、套餐、团队；不含任何 IP）经 API 导入实验室 fleet-server。幂等：已存在的按名字跳过。
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
scp -q -F "$HOME/.ssh/fleetlab_config" "$HERE/seed/catalog.json" fleet-dev:/root/catalog.json
ssh -F "$HOME/.ssh/fleetlab_config" fleet-dev 'bash -s' <<'REMOTE'
set -euo pipefail
B=http://127.0.0.1:8080/api/v1; J=/tmp/fleet-import.cookies; S=/root/catalog.json
trap 'rm -f "$J" "$S"' EXIT   # 登录 cookie 与传上来的清单，失败也删
jq -n --rawfile p /root/fleet-admin-password '{username:"admin",password:($p|rtrimstr("\n"))}' \
  | curl -fsS -c $J -H 'content-type: application/json' -d @- $B/auth/login >/dev/null
api() { curl -fsS -b $J -H 'content-type: application/json' -H 'x-fleet-request: 1' "$@"; }
# 团队
have=$(api $B/teams | jq -r '.data[].name')
i=0; for t in $(jq -r '.teams[]' $S); do i=$((i+1)); grep -qx "$t" <<<"$have" || api -d "$(jq -n --arg n "$t" --argjson o $i '{name:$n,sort_order:$o}')" $B/teams >/dev/null; done
# 供应商
have=$(api $B/providers | jq -r '.data[].name')
jq -c '.providers|to_entries[]' $S | while read -r e; do
  n=$(jq -r .key <<<"$e"); grep -qx "$n" <<<"$have" || api -d "$(jq -c '{name:.key,code:.value,timezone:"America/Los_Angeles"}' <<<"$e")" $B/providers >/dev/null
done
# 套餐
prov=$(api $B/providers | jq -c '[.data[]|{(.name):.id}]|add')
have=$(api $B/plans | jq -r '.data[].name')
jq -c '.plans[]' $S | while read -r p; do
  n=$(jq -r .name <<<"$p"); grep -qx "$n" <<<"$have" && continue
  api -d "$(jq -c --argjson prov "$prov" '{provider_id:$prov[.provider],name,cpu_cores,mem_mb,disk_gb,bandwidth_mbps,traffic_gb,price_cents,currency,cycle,notes}' <<<"$p")" $B/plans >/dev/null
done
echo "teams=$(api $B/teams | jq '.data|length') providers=$(api $B/providers | jq '.data|length') plans=$(api $B/plans | jq '.data|length')"
REMOTE
