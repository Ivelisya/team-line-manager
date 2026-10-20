#!/bin/bash
# 证据采集（a395 上跑）：
#   proof.sh snap <标签>          每台线路机：配置文件原文、运行态完整 lsi/lso/lsrules、xray PID + 启动时刻、重启日志行数
#   proof.sh diff <前> <后>       已有入口（配置 + 运行态）逐项比对；由配置推导的分享链接逐条比对；PID/启动时刻比对
#   proof.sh longconn-start <线路机> <端口> <秒>   经一个【已有】入口开一条慢速长连接（后台），用来证明接入期间老连接不断
#   proof.sh longconn-check        读长连接结果
#   proof.sh e2e <线路机> <端口> [url]   用该入口的参数做一次客户端请求（默认回显出口，返回 ip=）
set -euo pipefail
R=$HOME/team-ops-eval/onboard; L=$R/lab; P=$L/proof; X=$R/dist/xray-26.7.28-64d46afb
mkdir -p $P
relays="relay-l relay-m"

client_cfg() { # relay-json port relay-ip dest-host dest-port sock
  python3 - "$@" <<'PY'
import json, subprocess, sys
cfgp, port, rip, dh, dp, sock = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], int(sys.argv[5]), sys.argv[6]
c = json.load(open(cfgp))
ib = next(i for i in c["inbounds"] if i.get("port") == port)
rs = ib["streamSettings"]["realitySettings"]
pub = [l.split(":", 1)[1].strip() for l in subprocess.check_output(["%s" % "/home/kismet/team-ops-eval/onboard/dist/xray-26.7.28-64d46afb", "x25519", "-i", rs["privateKey"]]).decode().splitlines() if l.startswith(("Password", "Public"))][0]
cl = ib["settings"]["clients"][0]
print(json.dumps({"log": {"loglevel": "warning"},
  "inbounds": [{"listen": sock, "protocol": "dokodemo-door", "settings": {"address": dh, "port": dp, "network": "tcp"}, "tag": "in"}],
  "outbounds": [{"protocol": "vless", "settings": {"vnext": [{"address": rip, "port": port, "users": [{"id": cl["id"], "flow": cl.get("flow", ""), "encryption": "none"}]}]},
    "streamSettings": {"network": "tcp", "security": "reality", "realitySettings": {"fingerprint": "chrome", "serverName": rs["serverNames"][0], "publicKey": pub, "shortId": rs["shortIds"][0]}}}]}))
PY
}
relay_ip() { case $1 in relay-l) echo 11.99.0.10;; relay-m) echo 11.99.0.11;; esac; }

case "$1" in
snap)
  d=$P/$2; mkdir -p $d
  for r in $relays; do
    docker exec onb-$r cat /etc/fleet/xray/config.json > $d/$r.config.json
    docker exec onb-$r bash -c 'X=/opt/fleet/xray; A=--server=127.0.0.1:62789; $X api lsi $A > /tmp/lsi.json; $X api lso $A > /tmp/lso.json; $X api lsrules $A > /tmp/lsr.json; cat /tmp/lsi.json' > $d/$r.lsi.json
    docker exec onb-$r cat /tmp/lso.json > $d/$r.lso.json; docker exec onb-$r cat /tmp/lsr.json > $d/$r.lsrules.json
    docker exec onb-$r bash -c 'P=$(pgrep -o -f "run -c /etc/fleet/xray/config.json"); echo "pid=$P start=$(awk "{print \$22}" /proc/$P/stat) restarts=$(cat /var/log/xray-restarts.log 2>/dev/null | wc -l) uptime=$(/opt/fleet/xray api statssys --server=127.0.0.1:62789 | tr -d " \n" | grep -o "\"Uptime\":[0-9]*" | cut -d: -f2)"' > $d/$r.proc
    echo "$r: $(cat $d/$r.proc) sha=$(sha256sum < $d/$r.config.json | cut -c1-16)"
  done
  ;;
diff)
  python3 - $P/$2 $P/$3 $relays <<'PY'
import json, subprocess, sys
a, b, relays = sys.argv[1], sys.argv[2], sys.argv[3:]
X = "/home/kismet/team-ops-eval/onboard/dist/xray-26.7.28-64d46afb"
def pbk(priv):
    for l in subprocess.check_output([X, "x25519", "-i", priv]).decode().splitlines():
        if l.startswith(("Password", "Public")): return l.split(":", 1)[1].strip()
def links(cfg, host):
    out = {}
    for i in cfg["inbounds"]:
        if i.get("protocol") != "vless": continue
        rs = i["streamSettings"]["realitySettings"]; c = i["settings"]["clients"][0]
        out[i["tag"]] = "vless://%s@%s:%d?encryption=none&flow=%s&security=reality&sni=%s&fp=chrome&pbk=%s&sid=%s&type=tcp" % (c["id"], host, i["port"], c.get("flow", ""), rs["serverNames"][0], pbk(rs["privateKey"]), rs["shortIds"][0])
    return out
def rt(path):
    j = json.load(open(path)); return {x.get("tag"): x for x in j.get("inbounds", [])}
ok = True
for r in relays:
    host = {"relay-l": "11.99.0.10", "relay-m": "11.99.0.11"}[r]
    ca, cb = json.load(open(f"{a}/{r}.config.json")), json.load(open(f"{b}/{r}.config.json"))
    pa, pb = open(f"{a}/{r}.proc").read().split(), open(f"{b}/{r}.proc").read().split()
    pa, pb = dict(x.split("=") for x in pa), dict(x.split("=") for x in pb)
    print(f"== {r}")
    same_proc = pa["pid"] == pb["pid"] and pa["start"] == pb["start"] and pa["restarts"] == pb["restarts"] and int(pb["uptime"] or 0) >= int(pa["uptime"] or 0)
    print(f"  Xray 进程：pid {pa['pid']}→{pb['pid']}，启动时刻(jiffies) {pa['start']}→{pb['start']}，重启次数 {pa['restarts']}→{pb['restarts']}，uptime {pa['uptime']}s→{pb['uptime']}s  ⇒ {'未重启 ✓' if same_proc else '重启过 ✗'}")
    ok &= same_proc
    for k in ("inbounds", "outbounds"):
        ta = {x["tag"]: x for x in ca[k]}; tb = {x["tag"]: x for x in cb[k]}
        changed = [t for t in ta if tb.get(t) != ta[t]]
        prefix = cb[k][:len(ca[k])] == ca[k]
        print(f"  配置 {k}：前 {len(ta)} 个 → 后 {len(tb)} 个；已有对象改动 {changed or '无'}；已有部分逐项相同且顺序不变={prefix}；新增 {sorted(set(tb)-set(ta))}")
        ok &= (not changed) and prefix
    ra, rb = ca["routing"]["rules"], cb["routing"]["rules"]
    print(f"  配置 rules：{len(ra)} → {len(rb)}；已有规则前缀不变={rb[:len(ra)] == ra}；新增 {[x.get('ruleTag') for x in rb[len(ra):]]}")
    ok &= rb[:len(ra)] == ra
    sa = {k: v for k, v in ca.items() if k not in ("inbounds", "outbounds", "routing")}; sb = {k: v for k, v in cb.items() if k not in ("inbounds", "outbounds", "routing")}
    print(f"  静态段（api/log/policy/stats/metrics）不变={sa == sb}"); ok &= sa == sb
    ia, ib = rt(f"{a}/{r}.lsi.json"), rt(f"{b}/{r}.lsi.json")
    rchg = [t for t in ia if ib.get(t) != ia[t]]
    print(f"  运行态入口（xray api lsi 完整对象）：前 {len(ia)} → 后 {len(ib)}；已有入口运行态改动 {rchg or '无'}"); ok &= not rchg
    la, lb = links(ca, host), links(cb, host)
    lchg = [t for t in la if lb.get(t) != la[t]]
    print(f"  已有入口的分享链接（由配置推导，{len(la)} 条）：变化 {lchg or '无'} ✓" if not lchg else f"  分享链接变化 {lchg} ✗"); ok &= not lchg
print("结论：", "零重启、已有入口与分享链接全部不变 ✓" if ok else "有差异 ✗")
sys.exit(0 if ok else 1)
PY
  ;;
longconn-start)
  r=$2; port=$3; secs=$4
  docker exec onb-$r cat /etc/fleet/xray/config.json > $P/lc-src.json
  client_cfg $P/lc-src.json $port $(relay_ip $r) 11.99.0.200 80 /tmp/lc.sock > $P/lc-client.json
  docker cp $P/lc-client.json onb-central:/tmp/lc-client.json
  docker exec -d onb-central bash -c "rm -f /tmp/lc.sock /tmp/lc.out; (/opt/fleet/xray run -c /tmp/lc-client.json > /tmp/lc.log 2>&1 &); sleep 1; T0=\$(date +%s); curl -sS -m $((secs+30)) --unix-socket /tmp/lc.sock 'http://11.99.0.200/slow?s=$secs' > /tmp/lc.out 2>&1; echo \"rc=\$? elapsed=\$(( \$(date +%s)-T0 ))\" >> /tmp/lc.out"
  echo "长连接已开：$r:$port（已有入口）→ 旧节点 → 回显 /slow，持续 ${secs}s"
  ;;
longconn-check)
  docker exec onb-central bash -c 'tail -c 300 /tmp/lc.out; echo'
  ;;
e2e)
  r=$2; port=$3; url=${4:-http://11.99.0.200/}
  docker exec onb-$r cat /etc/fleet/xray/config.json > $P/e2e-src.json
  host=$(python3 -c "import sys,urllib.parse as u;p=u.urlparse(sys.argv[1]);print(p.hostname)" "$url")
  dport=$(python3 -c "import sys,urllib.parse as u;p=u.urlparse(sys.argv[1]);print(p.port or (443 if p.scheme=='https' else 80))" "$url")
  client_cfg $P/e2e-src.json $port $(relay_ip $r) $host $dport /tmp/e2e.sock > $P/e2e-client.json
  docker cp $P/e2e-client.json onb-central:/tmp/e2e-client.json
  docker exec onb-central bash -c "rm -f /tmp/e2e.sock; /opt/fleet/xray run -c /tmp/e2e-client.json > /tmp/e2e.log 2>&1 & XP=\$!; for i in \$(seq 50); do [ -S /tmp/e2e.sock ] && break; sleep 0.1; done; curl -sS -m 15 --unix-socket /tmp/e2e.sock '$url' | grep -E '^(ip|loc|colo)='; kill \$XP"
  ;;
esac
