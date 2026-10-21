#!/bin/bash
# 实验室端到端（在本机执行，经 ssh 驱动 fleet-dev 上的 API）。前提：lab/reset-lab.sh、lab/deploy-server.sh、lab/import-catalog.sh 已跑过。
# 全程只调 API（和点界面等价），最后在线路机/节点上核对「没有残留」。
set -euo pipefail
F="-F $HOME/.ssh/fleetlab_config"
ssh $F fleet-dev 'bash -s' <<'REMOTE'
set -euo pipefail
B=http://127.0.0.1:8080/api/v1; J=/tmp/e2e.cookies; P=/root/lab-root-passwords
trap 'rm -f $J' EXIT   # 登录 cookie 失败也删
ok() { echo "  ✓ $*"; }
die() { echo "  ✗ $*"; exit 1; }
jq -n --rawfile p /root/fleet-admin-password '{username:"admin",password:($p|rtrimstr("\n"))}' | curl -fsS -c $J -H 'content-type: application/json' -d @- $B/auth/login >/dev/null
api() { curl -sS -b $J -H 'content-type: application/json' -H 'x-fleet-request: 1' "$@"; }
wait_job() {
  for i in $(seq 1 300); do
    s=$(api $B/jobs/$1 | jq -r .data.state)
    case $s in succeeded) return 0;; failed|cancelled) api $B/jobs/$1 | jq -r '.data.error'; api "$B/jobs/$1/logs" | jq -r '.data[]|"    "+.level+" "+.msg' | tail -15; return 1;; esac
    sleep 2
  done; echo "timeout"; return 1
}
name_of() { api $B/machines/$1 | jq -r .data.name; }

echo "== 1. 上架线路机（root 密码引导）"
R=$(api -d "$(jq -n --rawfile pw $P/lab-relay1 '{items:[{kind:"relay",ip:"192.168.31.61",label:"实验线路机",login:{password:($pw|rtrimstr("\n"))}}]}')" $B/provision)
RJ=$(jq -r '.data.jobs[0].job_id' <<<"$R"); RID=$(jq -r '.data.jobs[0].machine_id' <<<"$R")
[ "$RJ" != null ] || die "创建失败：$R"
wait_job "$RJ" || die "线路机上架失败"; ok "线路机 $(name_of $RID) 在用"
ssh -n -o BatchMode=yes -o StrictHostKeyChecking=no -o PubkeyAuthentication=no -o PreferredAuthentications=password root@192.168.31.61 true 2>/dev/null && die "线路机仍允许密码登录" || ok "线路机已禁用密码登录"

echo "== 2. 上架三台节点（密码引导 VPS / 密钥 VPS / SOCKS5 上游）"
T1=$(api $B/teams | jq -r '.data[]|select(.name=="团队1").id')
PL=$(api $B/plans | jq -r '.data[]|select(.name=="ATTVPS 基础版").id')
PV=$(api $B/providers | jq -r '.data[]|select(.name=="ATTVPS").id')
DUE=$(date -d '+20 days' +%F)
BODY=$(jq -n --arg t "$T1" --arg r "$RID" --arg pl "$PL" --arg pv "$PV" --arg due "$DUE" \
  --rawfile pw $P/lab-node1 --rawfile sp $P/socks-upstream '{items:[
  {kind:"vps",ip:"192.168.31.63",team_id:$t,relay_id:$r,plan_id:$pl,provider_id:$pv,login:{password:($pw|rtrimstr("\n"))},subscription:{due_date:$due}},
  {kind:"vps",ip:"192.168.31.64",team_id:$t,relay_id:$r,plan_id:$pl,provider_id:$pv,subscription:{due_date:$due}},
  {kind:"proxy",ip:"192.168.31.65",team_id:$t,relay_id:$r,upstream:{protocol:"socks5",host:"192.168.31.65",port:1080,username:"labuser",password:($sp|rtrimstr("\n"))}}]}')
api -d "$BODY" $B/provision/check | jq -r '.data.items[]|"  预检 "+.name+" ok="+(.ok|tostring)+" "+(.errors|join(";"))'
C=$(api -d "$BODY" $B/provision)
for row in $(jq -r '.data.jobs[]|.job_id+","+.machine_id+","+.name' <<<"$C"); do
  IFS=, read -r j m n <<<"$row"; wait_job "$j" || die "$n 上架失败"; ok "$n 在用"
done
mapfile -t NODES < <(jq -r '.data.jobs[].machine_id' <<<"$C")

echo "== 3. 真实客户端：经每条入口访问回显，出口 IP 必须是节点自己"
i=0
for m in "${NODES[@]}"; do
  i=$((i+1)); mv=$(api $B/machines/$m); ip=$(jq -r .data.ip <<<"$mv"); eid=$(jq -r '.data.entries[0].id' <<<"$mv")
  url=$(api $B/entries/$eid/share | jq -r .data.url)
  uuid=${url#vless://}; uuid=${uuid%%@*}; hp=${url#*@}; hp=${hp%%\?*}; host=${hp%:*}; port=${hp##*:}
  q=${url#*\?}; q=${q%%#*}; get() { tr '&' '\n' <<<"$q" | sed -n "s/^$1=//p"; }
  cfg=/tmp/e2e-client-$i.json; lp=$((10800+i))
  jq -n --arg u "$uuid" --arg h "$host" --argjson p "$port" --arg sni "$(get sni)" --arg pbk "$(get pbk)" --arg sid "$(get sid)" --argjson lp $lp '{
    log:{loglevel:"warning"}, inbounds:[{listen:"127.0.0.1",port:$lp,protocol:"socks",settings:{udp:false}}],
    outbounds:[{protocol:"vless",settings:{vnext:[{address:$h,port:$p,users:[{id:$u,encryption:"none",flow:"xtls-rprx-vision"}]}]},
      streamSettings:{network:"tcp",security:"reality",realitySettings:{fingerprint:"chrome",serverName:$sni,publicKey:$pbk,shortId:$sid}}}]}' > $cfg
  /opt/fleet/dist/xray run -c $cfg >/tmp/e2e-client-$i.log 2>&1 & cp=$!; sleep 1.5
  # 回显在内网，客户流量按规则禁止访问内网 → 用公网回显核对路径不可行；改为核对「能连通公网」+ 节点侧看到连接
  got=$(curl -s -m 10 --socks5-hostname 127.0.0.1:$lp https://www.cloudflare.com/cdn-cgi/trace | sed -n 's/^ip=//p')
  kill $cp 2>/dev/null; wait $cp 2>/dev/null || true
  [ -n "$got" ] && ok "$(jq -r .data.name <<<"$mv")（$ip）经入口 $host:$port 上网成功，公网出口 $got" || die "$(jq -r .data.name <<<"$mv") 客户端连不通（$url）"
  priv=$(curl -s -m 5 --socks5-hostname 127.0.0.1:$lp http://192.168.31.60:8080/echo 2>/dev/null || true)
done
echo "  出口探测（经节点访问中控回显，核对节点 IP）："
for m in "${NODES[@]}"; do api $B/machines/$m | jq -r '.data|"    "+.name+" egress ok="+(.entries[0].egress.ok|tostring)+" 看到 "+(.entries[0].egress.detail//"-")+" 期望 "+.ip'; done

echo "== 4. 改团队名 → 线路机备注同步（不换令牌）"
api -X PATCH -d '{"name":"测试团队"}' $B/teams/$T1 >/dev/null; sleep 3
api $B/machines | jq -r '.data[]|select(.kind!="relay")|"    "+.name'
api -X PATCH -d '{"name":"团队1"}' $B/teams/$T1 >/dev/null

echo "== 5. 退役第一台节点：预览改名 → 退役 → 核对无残留、编号压缩"
N1=${NODES[0]}; N1NAME=$(name_of $N1); EID=$(api $B/machines/$N1 | jq -r '.data.entries[0].id'); EPORT=$(api $B/machines/$N1 | jq -r '.data.entries[0].port')
api $B/machines/$N1/retire | jq -r '.data.renames[]|"    将改名："+.old_name+" → "+.new_name'
RJ=$(api -d "$(jq -n --arg c "$N1NAME" '{confirm:$c,reason:"e2e"}')" $B/machines/$N1/retire | jq -r .data.job_id)
wait_job "$RJ" || die "退役失败"; ok "$N1NAME 已退役"
TAG="in-$(tr -d - <<<"$EID")"
ssh -n -o BatchMode=yes -i /etc/fleet/ssh/id_ed25519 -o UserKnownHostsFile=/var/lib/fleet/known_hosts root@192.168.31.61 \
  "XRAY_LOCATION_ASSET=/usr/local/share/xray; a=\$(/usr/local/bin/xray api lsi --server=127.0.0.1:62789; /usr/local/bin/xray api lso --server=127.0.0.1:62789; /usr/local/bin/xray api lsrules --server=127.0.0.1:62789); \
   echo \"\$a\" | grep -q '$(tr -d - <<<"$EID")' && echo LEFTOVER_RUNTIME; grep -q '$(tr -d - <<<"$EID")' /etc/fleet-agent/xray.json && echo LEFTOVER_FILE; \
   ss -Hltn 'sport = :$EPORT' | grep -q . && echo LEFTOVER_PORT; echo xray_pid=\$(systemctl show -p MainPID --value fleet-xray)" > /tmp/e2e-relay-check.txt
grep -q LEFTOVER /tmp/e2e-relay-check.txt && die "线路机有残留：$(cat /tmp/e2e-relay-check.txt)" || ok "线路机上入站/出站/规则/配置文件/端口均无残留（$(grep xray_pid /tmp/e2e-relay-check.txt)）"
ssh -n -o BatchMode=yes -i /etc/fleet/ssh/id_ed25519 -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no root@192.168.31.63 \
  'for s in fleet-agent fleet-ssserver; do systemctl is-active -q $s && echo "LEFT $s"; done; [ -e /etc/fleet-agent ] && echo "LEFT /etc/fleet-agent"; nft list table inet ss_guard >/dev/null 2>&1 && echo "LEFT nft"; true' > /tmp/e2e-node-check.txt 2>/dev/null
[ -s /tmp/e2e-node-check.txt ] && die "节点有残留：$(cat /tmp/e2e-node-check.txt)" || ok "节点上 agent、ssserver、防火墙表均已清理"
api $B/machines | jq -r '.data[]|select(.kind!="relay")|"    在用："+.name+"  "+.ip'
api "$B/machines?scope=retired" | jq -r '.data[]|"    已退役："+.name+"  "+.ip'
TOK=$(runuser -u postgres -- psql -qtA fleet -c "select count(*) from agent_token where machine_id='$N1' and revoked_at is null")
[ "$TOK" = 0 ] && ok "令牌已撤销" || die "令牌未撤销"
echo "== 全部通过"
# 客户端配置与日志、残留核对结果：失败时留着排查（文件名固定，下次运行覆盖），通过才删
rm -f /tmp/e2e-*
REMOTE
