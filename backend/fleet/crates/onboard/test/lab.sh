#!/bin/bash
# a395 演练环境（只用自己的一次性容器 onb-*，自己的网络 onb-net；不碰 Leap dev 栈）。
#   lab.sh up        起：中控 onb-central、线路机 onb-relay-l（Xray 26.7.28）与 onb-relay-m（26.6.27）、
#                        旧节点 onb-old（已在线路机上的现有入口）、回显 onb-echo、5 台“新机” onb-n1..n5
#   lab.sh down      全删
#   lab.sh fc ...    在中控容器里跑 fleetctl
#   lab.sh reinstall-n5   模拟在供应商面板重装 n5（同 IP 同 MAC、新主机密钥、新 root 密码）
# 网段用 11.99.0.0/24：不在 geoip:private 里，这样线路机的「私网→blocked」规则不会挡住演练流量（与生产同一套规则）。
set -euo pipefail
R=$HOME/team-ops-eval/onboard
L=$R/lab; D=$R/dist; BIN=$R/target/x86_64-unknown-linux-musl/release/fleetctl
IMG=onb-base:1; NET=onb-net
X28=$D/xray-26.7.28-64d46afb; X27=$D/xray-26.6.27-8ef87ac0
NODES="n1:11.99.0.21 n2:11.99.0.22 n3:11.99.0.23 n4:11.99.0.24 n5:11.99.0.25"

node_run() { # name ip rootpw authkey nat mac
  local n=$1 ip=$2 pw=$3 ak=$4 nat=$5 mac=${6:-}
  docker run -d --name onb-$n --hostname $n --network $NET --ip $ip ${mac:+--mac-address $mac} --cap-add NET_ADMIN \
    -e ROOT_PW="$pw" -e AUTH_KEY="$ak" -e NAT="$nat" -v $L/lab-bin:/lab-bin:ro $IMG bash /lab-bin/node-entry.sh >/dev/null
}

relay_cfg() { # xray out [old_ip]
  python3 - "$1" "$2" "${3:-}" <<'PY'
import json, subprocess, sys, uuid, secrets
X, out, old = sys.argv[1], sys.argv[2], sys.argv[3]
def priv():
    d = {}
    for l in subprocess.check_output([X, "x25519"]).decode().splitlines():
        k, _, v = l.partition(":"); d[k.strip()] = v.strip()
    return d.get("PrivateKey") or d.get("Private key")
def inbound(port, email):
    return {"listen": "0.0.0.0", "port": port, "protocol": "vless",
            "settings": {"clients": [{"email": email, "flow": "xtls-rprx-vision", "id": str(uuid.uuid4())}], "decryption": "none", "fallbacks": []},
            "sniffing": {"destOverride": ["http", "tls", "quic"], "enabled": True, "metadataOnly": False, "routeOnly": False},
            "streamSettings": {"network": "tcp", "realitySettings": {"maxClientVer": "", "maxTimeDiff": 0, "minClientVer": "1.8.2", "privateKey": priv(),
                "serverNames": ["www.cloudflare.com"], "shortIds": [secrets.token_hex(8)], "show": False, "target": "www.cloudflare.com:443", "xver": 0},
                "security": "reality", "tcpSettings": {"acceptProxyProtocol": False, "header": {"type": "none"}}},
            "tag": "inbound-%d" % port}
cfg = {"api": {"services": ["HandlerService", "LoggerService", "StatsService", "RoutingService"], "tag": "api"},
       "inbounds": [{"listen": "127.0.0.1", "port": 62789, "protocol": "dokodemo-door", "settings": {"address": "127.0.0.1"}, "tag": "api"}, inbound(20001, "node-00")],
       "log": {"access": "none", "dnsLog": False, "error": "", "loglevel": "warning", "maskAddress": ""},
       "metrics": {"listen": "127.0.0.1:11111", "tag": "metrics_out"},
       "outbounds": [{"protocol": "freedom", "settings": {"domainStrategy": "AsIs"}, "tag": "direct"}, {"protocol": "blackhole", "settings": {}, "tag": "blocked"}],
       "policy": {"levels": {"0": {"statsUserDownlink": True, "statsUserOnline": True, "statsUserUplink": True}},
                  "system": {"statsInboundDownlink": True, "statsInboundUplink": True, "statsOutboundDownlink": False, "statsOutboundUplink": False}},
       "routing": {"domainStrategy": "AsIs", "rules": [{"inboundTag": ["api"], "outboundTag": "api", "type": "field"},
                   {"ip": ["geoip:private"], "outboundTag": "blocked", "type": "field"},
                   {"outboundTag": "blocked", "protocol": ["bittorrent"], "type": "field"}]},
       "stats": {}}
if old:
    cfg["inbounds"].append(inbound(20002, "node-01"))
    tag = "ss-" + old.replace(".", "-")
    cfg["outbounds"].append({"protocol": "shadowsocks", "settings": {"servers": [{"address": old, "method": "aes-256-gcm", "password": "old-node-pass-0001", "port": 8388}]}, "tag": tag})
    cfg["routing"]["rules"].append({"inboundTag": ["inbound-20002"], "outboundTag": tag, "type": "field"})
open(out, "w").write(json.dumps(cfg, ensure_ascii=False, indent=2) + "\n")
PY
}

relay_run() { # name ip xray cfgfile
  docker run -d --name onb-$1 --hostname $1 --network $NET --ip $2 -e AUTH_KEY="$(cat $L/keys/relay_ed25519.pub)" \
    -v $L/lab-bin:/lab-bin:ro -v $3:/opt/fleet/xray:ro -v $D/geoip.dat:/opt/fleet/geoip.dat:ro -v $D/geosite.dat:/opt/fleet/geosite.dat:ro \
    $IMG bash -c "mkdir -p /etc/fleet/xray && cp /lab-bin/$1.json /etc/fleet/xray/config.json && chmod 600 /etc/fleet/xray/config.json && exec bash /lab-bin/relay-entry.sh" >/dev/null
}

case "${1:-}" in
up)
  mkdir -p $L/keys $L/lab-bin $L/csv/pw $L/state
  [ -f $L/keys/onboard_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -C fleet-onboard-lab -f $L/keys/onboard_ed25519
  [ -f $L/keys/relay_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -C fleet-relay-lab -f $L/keys/relay_ed25519
  cat > $L/lab-bin/node-entry.sh <<'EOF'
#!/bin/bash
ssh-keygen -A >/dev/null 2>&1
[ -n "$ROOT_PW" ] && echo "root:$ROOT_PW" | chpasswd
mkdir -p /root/.ssh && chmod 700 /root/.ssh
[ -n "$AUTH_KEY" ] && echo "$AUTH_KEY" >> /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys
# 模拟供应商给的新机：root 密码登录开着
sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin yes/; s/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
if [ -n "$NAT" ]; then  # 模拟 NAT 机：供应商把公网 49762→22、49763→8388
  nft -f - <<'NFT'
table ip provider_nat {
  chain prerouting { type nat hook prerouting priority dstnat; policy accept;
    tcp dport 49762 redirect to :22
    tcp dport 49763 redirect to :8388
    udp dport 49763 redirect to :8388
  }
}
NFT
fi
exec /usr/sbin/sshd -D -e
EOF
  cat > $L/lab-bin/relay-entry.sh <<'EOF'
#!/bin/bash
ssh-keygen -A >/dev/null 2>&1
mkdir -p /root/.ssh && chmod 700 /root/.ssh && echo "$AUTH_KEY" > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys
cat > /usr/local/bin/xray-sup.sh <<'SUP'
#!/bin/bash
# 演练用的“systemd”：xray 退出就拉起（每次拉起都会换 PID，所以 PID 不变 = 没重启）
while :; do XRAY_LOCATION_ASSET=/opt/fleet /opt/fleet/xray run -c /etc/fleet/xray/config.json >> /var/log/xray.log 2>&1; echo "$(date) xray exited" >> /var/log/xray-restarts.log; sleep 1; done
SUP
chmod +x /usr/local/bin/xray-sup.sh
setsid /usr/local/bin/xray-sup.sh < /dev/null > /dev/null 2>&1 &
exec /usr/sbin/sshd -D -e
EOF
  cat > $L/lab-bin/echo.py <<'EOF'
# 回显出口：/ → "ip=<来源 IP>"（cloudflare trace 格式）；/slow?s=N → 每 0.2 秒 1 字节，持续 N 秒（长连接不断的证据）
import http.server, socketserver, time, urllib.parse
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        if u.path == "/slow":
            s = float(urllib.parse.parse_qs(u.query).get("s", ["30"])[0])
            self.send_response(200); self.send_header("Content-Type", "text/plain"); self.end_headers()
            t0 = time.time(); n = 0
            while time.time() - t0 < s:
                self.wfile.write(b"."); self.wfile.flush(); n += 1; time.sleep(0.2)
            self.wfile.write(("\nbytes=%d ip=%s\n" % (n, self.client_address[0])).encode()); return
        b = ("fl=lab\nip=%s\nloc=LAB\n" % self.client_address[0]).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
class S(socketserver.ThreadingMixIn, http.server.HTTPServer): daemon_threads = True
S(("0.0.0.0", 80), H).serve_forever()
EOF
  docker network inspect $NET >/dev/null 2>&1 || docker network create --subnet 11.99.0.0/24 --gateway 11.99.0.1 $NET >/dev/null
  relay_cfg $X28 $L/lab-bin/relay-l.json 11.99.0.30
  relay_cfg $X27 $L/lab-bin/relay-m.json
  relay_run relay-l 11.99.0.10 $X28
  relay_run relay-m 11.99.0.11 $X27
  # 现有节点（旧流程上的，SS 直接跑）
  docker run -d --name onb-old --hostname old --network $NET --ip 11.99.0.30 -v $D:/dist:ro $IMG bash -c \
    'mkdir -p /tmp/s && tar -xJf /dist/shadowsocks-v1.25.0.x86_64-unknown-linux-musl.tar.xz -C /tmp/s ssserver && exec /tmp/s/ssserver -s 0.0.0.0:8388 -m aes-256-gcm -k old-node-pass-0001 -U' >/dev/null
  docker run -d --name onb-echo --hostname echo --network $NET --ip 11.99.0.200 -v $L/lab-bin:/lab-bin:ro $IMG python3 /lab-bin/echo.py >/dev/null
  # 新机：n1 已注入上架公钥（auth=key），n2/n3/n5 给密码，n4 的 CSV 故意给错密码；n3 是 NAT；n5 走 DD 人工闸门（固定 MAC）
  AK="$(cat $L/keys/onboard_ed25519.pub)"
  for x in n2 n3 n4 n5; do [ -f $L/csv/pw/$x.pw ] || head -c 12 /dev/urandom | base64 | tr -d '/+=' > $L/csv/pw/$x.pw; chmod 600 $L/csv/pw/$x.pw; done
  echo -n 'definitely-wrong-password' > $L/csv/pw/n4-wrong.pw; chmod 600 $L/csv/pw/n4-wrong.pw
  node_run n1 11.99.0.21 "$(head -c 9 /dev/urandom | base64)" "$AK" ""
  node_run n2 11.99.0.22 "$(cat $L/csv/pw/n2.pw)" "" ""
  node_run n3 11.99.0.23 "$(cat $L/csv/pw/n3.pw)" "" "1"
  node_run n4 11.99.0.24 "$(cat $L/csv/pw/n4.pw)" "" ""
  node_run n5 11.99.0.25 "$(cat $L/csv/pw/n5.pw)" "" "" 02:42:0b:63:00:19
  # 中控：fleetctl + xray 客户端 + ssserver 包 + 套餐目录
  docker run -d --name onb-central --hostname central --network $NET --ip 11.99.0.5 \
    -v $L:/lab -v $(dirname $BIN):/opt/onb-bin:ro -v $X28:/opt/fleet/xray:ro -v $D:/dist:ro $IMG sleep infinity >/dev/null
  sleep 2
  docker exec onb-central bash -c 'for h in 11.99.0.10 11.99.0.11; do ssh-keyscan -t ed25519 $h 2>/dev/null; done > /lab/keys/relays.known_hosts; wc -l < /lab/keys/relays.known_hosts'
  cat > $L/onboard.toml <<'EOF'
db = "/lab/state/onboard.db"
catalog = "/dist/catalog.json"
work_dir = "/lab/state/work"
ssh_key = "/lab/keys/onboard_ed25519"
ssh_pubkey = "/lab/keys/onboard_ed25519.pub"
client_xray = "/opt/fleet/xray"
concurrency = 5
canary = true
canary_gate = "P16"
relay_window_s = 5
relay_batch_max = 20
retry_max = 3
retry_base_ms = 1000
keyscan_samples = 3
keyscan_interval_ms = 500
lease_ttl_s = 6
egress_check_url = "http://11.99.0.200/"
require_bbr = false          # 容器里 /proc/sys 只读，BBR 设不上；生产必须 true
dd_wait_timeout_s = 120

[ss]
tarball = "/dist/shadowsocks-v1.25.0.x86_64-unknown-linux-musl.tar.xz"

[[relays]]
name = "线路机L"
host = "11.99.0.10"
ssh_key = "/lab/keys/relay_ed25519"
known_hosts = "/lab/keys/relays.known_hosts"
public_host = "11.99.0.10"
allow_ips = ["11.99.0.10"]

[[relays]]
name = "线路机M"
host = "11.99.0.11"
ssh_key = "/lab/keys/relay_ed25519"
known_hosts = "/lab/keys/relays.known_hosts"
public_host = "11.99.0.11"
allow_ips = ["11.99.0.11"]
EOF
  cat > $L/csv/batch.csv <<'EOF'
provider,ip,ssh_port,auth,plan,team,number,relay,ss_port,dd,mac,service_id,note
ATTVPS,11.99.0.21,22,key,基础版,团队9,1,线路机L,,,,90001,金丝雀（已注入上架公钥）
ATTVPS,11.99.0.22,22,password:pw/n2.pw,ATTVPS 基础版,团队9,2,线路机L,,,,90002,
OhnoICU,11.99.0.23,49762,password:pw/n3.pw,ohnoicu.resi-nat-aca.small,团队9,3,线路机M,49763,,,90003,NAT 映射端口（Xray 26.6.27 线路机）
ATTVPS,11.99.0.24,22,password:pw/n4-wrong.pw,基础版,团队9,4,线路机L,,,,90004,注入故障：密码错
ATTVPS,11.99.0.25,22,password:pw/n5.pw,标准版,团队9,5,线路机L,,manual,02:42:0b:63:00:19,90005,DD 人工闸门
EOF
  docker ps --filter name=onb- --format '{{.Names}}\t{{.Status}}'
  ;;
down)
  docker rm -f $(docker ps -aq --filter name=onb-) >/dev/null 2>&1 || true
  docker network rm $NET >/dev/null 2>&1 || true
  ;;
fc)
  shift; docker exec -e FLEET_ONBOARD_CONFIG=/lab/onboard.toml -e FLEET_OPERATOR=lab -e FLEET_ONB_CRASH="${FLEET_ONB_CRASH:-}" -w /lab/csv onb-central /opt/onb-bin/fleetctl onboard "$@"
  ;;
reinstall-n5)
  # 在“供应商面板”重装：同 IP、同 MAC（同一台 VM），系统盘清空 → 新主机密钥、新 root 密码、没有我们的公钥
  head -c 12 /dev/urandom | base64 | tr -d '/+=' > $L/csv/pw/n5-new.pw; chmod 600 $L/csv/pw/n5-new.pw
  docker rm -f onb-n5 >/dev/null
  node_run n5 11.99.0.25 "$(cat $L/csv/pw/n5-new.pw)" "" "" 02:42:0b:63:00:19
  echo "n5 已重装（新 root 密码在 csv/pw/n5-new.pw）"
  ;;
*) echo "用法：lab.sh up|down|fc <args>|reinstall-n5"; exit 2 ;;
esac
