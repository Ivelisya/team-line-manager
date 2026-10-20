#!/bin/bash
# 实验室端到端测试：加入口 → 真实客户端走通 → 删入口 → 运行态/文件无残留，且 Xray 全程没重启。
# 在开发机（有 ~/.ssh/fleetlab_config）上运行，需要环境变量：
#   SS_PASSWORD           lab-node1 上 ssserver 的密码
#   REALITY_PRIVATE_KEY   线路机用的 x25519 私钥（xray x25519 生成）
#   REALITY_PUBLIC_KEY    对应公钥（客户端用）
# 前提：lab-relay1 已装 fleet-agent、xray、单元文件；fleet-dev 上有 /root/xclient/xray（客户端）；fleet-agent 服务已停止（本脚本用 apply 子命令）。
set -euo pipefail
: "${SS_PASSWORD:?}" "${REALITY_PRIVATE_KEY:?}" "${REALITY_PUBLIC_KEY:?}"
SSH="ssh -F $HOME/.ssh/fleetlab_config"
HERE="$(cd "$(dirname "$0")" && pwd)"
ok()   { echo "  [通过] $*"; }
fail() { echo "  [失败] $*"; exit 1; }

echo "== 准备"
sed "s#__REALITY_PRIVATE_KEY__#$REALITY_PRIVATE_KEY#; s#__SS_PASSWORD__#$SS_PASSWORD#" "$HERE/desired-add.json.tmpl" | $SSH lab-relay1 'mkdir -p /root/t && cat > /root/t/add.json && chmod 600 /root/t/add.json'
sed "s#__REALITY_PRIVATE_KEY__#$REALITY_PRIVATE_KEY#" "$HERE/desired-remove.json.tmpl" | $SSH lab-relay1 'cat > /root/t/remove.json && chmod 600 /root/t/remove.json'
$SSH fleet-dev 'mkdir -p /root/xclient'
$SSH fleet-dev '[ -x /root/xclient/xray ]' || $SSH lab-relay1 'cat /usr/local/bin/xray' | $SSH fleet-dev 'cat > /root/xclient/xray && chmod +x /root/xclient/xray'
cat <<EOF | $SSH fleet-dev 'cat > /root/xclient/client.json'
{"log":{"loglevel":"warning"},
 "inbounds":[{"tag":"s","listen":"127.0.0.1","port":1080,"protocol":"socks","settings":{"udp":false}}],
 "outbounds":[{"tag":"p","protocol":"vless","settings":{"vnext":[{"address":"192.168.31.61","port":20001,"users":[{"id":"6b0c1c52-1f0e-4c53-9c0b-2d7a4f6f1a01","encryption":"none","flow":"xtls-rprx-vision"}]}]},
 "streamSettings":{"network":"tcp","security":"reality","realitySettings":{"serverName":"www.cloudflare.com","fingerprint":"chrome","publicKey":"$REALITY_PUBLIC_KEY","shortId":"a1b2c3d4e5f60718"}}}]}
EOF
$SSH fleet-dev 'pkill -x xray || true; cd /root/xclient && (nohup ./xray run -c client.json >x.log 2>&1 &); sleep 1'

relay() { $SSH lab-relay1 "export XRAY_LOCATION_ASSET=/usr/local/share/xray; $*"; }
tags() { relay "S=127.0.0.1:62789; echo inbounds: \$(xray api lsi --server=\$S | grep '\"tag\"' | tr -d ' \n'); echo outbounds: \$(xray api lso --server=\$S | grep '\"tag\"' | tr -d ' \n'); echo rules: \$(xray api lsrules --server=\$S | grep ruleTag | tr -d ' \n')"; }
xpid() { relay 'p=$(systemctl show -p MainPID --value fleet-xray); echo $p:$(awk "{print \$22}" /proc/$p/stat)'; }

echo "== 0. 空骨架（Xray 未运行时 apply 会写配置并启动它；已运行则为空操作）"
relay 'cd /root/t && fleet-agent apply --desired remove.json >/dev/null 2>&1'
P0=$(xpid); echo "  xray pid:starttime = $P0"
EMPTY=$(tags)

echo "== 1. 加入口（generation 1）"
relay 'cd /root/t && fleet-agent apply --desired add.json 2>&1 | grep -E "INFO   |应用完成"'
tags
sleep 1
IP=$($SSH fleet-dev 'curl -s -m 20 --socks5-hostname 127.0.0.1:1080 http://api.ipify.org')
[ "$IP" = "13.212.203.115" ] && ok "客户端经 线路机→lab-node1 访问外网，出口 IP=$IP" || fail "客户端未走通：'$IP'"
$SSH lab-node1 'systemctl stop ssserver'
BAD=$($SSH fleet-dev 'curl -s -m 8 --socks5-hostname 127.0.0.1:1080 http://api.ipify.org' || true)
$SSH lab-node1 'systemctl start ssserver'
[ -z "$BAD" ] && ok "停掉 lab-node1 的 ssserver 后客户端失败 —— 流量确实经过节点" || fail "ssserver 停了仍有响应：$BAD"
relay 'ss -ltn | grep -E ":(20001|30001) "' >/dev/null && ok "20001（入口）与 30001（探测）在监听" || fail "端口未监听"

echo "== 2. 幂等：同一份再应用一次不产生任何步骤"
N=$(relay 'cd /root/t && fleet-agent apply --desired add.json 2>&1 | grep -c "INFO   "' || true)
[ "$N" = "0" ] && ok "无变更步骤" || fail "重复应用产生了 $N 个步骤"

echo "== 3. 删入口（generation 2）"
relay 'cd /root/t && fleet-agent apply --desired remove.json 2>&1 | grep -E "INFO   |应用完成"'
NOW=$(tags)
[ "$NOW" = "$EMPTY" ] && ok "运行态（lsi/lso/lsrules）回到空骨架，无残留" || { echo "$NOW"; fail "运行态有残留"; }
relay 'grep -c -E "0198e5a0|20001|30001" /etc/fleet-agent/xray.json || true' | grep -qx 0 && ok "xray.json 无残留" || fail "xray.json 有残留"
relay 'ss -ltn | grep -E ":(20001|30001) "' && fail "端口仍在监听" || ok "端口已释放"
relay 'ls /var/lib/fleet-agent/tmp | wc -l' | grep -qx 0 && ok "临时文件已清理" || fail "tmp 有残留"
BAD=$($SSH fleet-dev 'curl -s -m 8 --socks5-hostname 127.0.0.1:1080 http://api.ipify.org' || true)
[ -z "$BAD" ] && ok "客户端不再能连通" || fail "删除后客户端仍可用"

echo "== 4. Xray 全程没有重启"
P1=$(xpid); [ "$P0" = "$P1" ] && ok "pid:starttime 未变（$P1）" || fail "Xray 被重启：$P0 → $P1"
$SSH fleet-dev 'pkill -x xray || true'
$SSH lab-relay1 'rm -rf /root/t'
echo "== 全部通过"
