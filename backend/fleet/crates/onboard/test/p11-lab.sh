#!/bin/bash
# P11 演练环境（a395，只用自己的 p11-* 容器和 p11-net 11.98.0.0/24；不碰 onb-*、Leap dev 栈、fleet-build/src、fleet-stage）。
#   p11-lab.sh up            线路机 L/M（Xray，复用 onboard/dist 的二进制，只读挂载）、回显、中控 p11-central（fleetd + fleetctl + xray 客户端）、
#                        systemd 节点 n1..n5（onb-sd:1）
#   p11-lab.sh fleetd [ENV=…] 在中控里起 fleetd（127.0.0.1:25911，dev-allow-local，自己的 data_dir）
#   p11-lab.sh api METHOD PATH [JSON]   经 fleetd HTTP 调 /v2/api/v1/onboard/*（本机直连 = dev-local 管理员）
#   p11-lab.sh down
# 生产的上报地址是 https://38.58.57.22；这里 nodeagent 只允许 http://127.0.0.1 的明文，所以节点上起一个转发
# 127.0.0.1:25911 → 中控 11.98.0.5:25912 → 中控 127.0.0.1:25911（fleetd）。转发只是演练管道，不在产品里。
set -euo pipefail
R=$HOME/team-ops-eval/fleet-build-p11
L=$R/lab; D=$HOME/team-ops-eval/onboard/dist; ED=$R/edge-dist
BIN=$R/target/x86_64-unknown-linux-musl/release
IMG=onb-base:1; SD=onb-sd:1; NET=p11-net
X28=$D/xray-26.7.28-64d46afb; X27=$D/xray-26.6.27-8ef87ac0

relay_cfg() { # xray out
  python3 - "$1" "$2" <<'PY'
import json, subprocess, sys, uuid, secrets
X, out = sys.argv[1], sys.argv[2]
def priv():
    d = {}
    for l in subprocess.check_output([X, "x25519"]).decode().splitlines():
        k, _, v = l.partition(":"); d[k.strip()] = v.strip()
    return d.get("PrivateKey") or d.get("Private key")
def inbound(port, email):
    return {"listen": "0.0.0.0", "port": port, "protocol": "vless",
            "settings": {"clients": [{"email": email, "flow": "xtls-rprx-vision", "id": str(uuid.uuid4())}], "decryption": "none", "fallbacks": []},
            "streamSettings": {"network": "tcp", "realitySettings": {"privateKey": priv(), "serverNames": ["www.cloudflare.com"], "shortIds": [secrets.token_hex(8)],
                "show": False, "target": "www.cloudflare.com:443", "xver": 0}, "security": "reality", "tcpSettings": {"acceptProxyProtocol": False, "header": {"type": "none"}}},
            "tag": "inbound-%d" % port}
cfg = {"api": {"services": ["HandlerService", "LoggerService", "StatsService", "RoutingService"], "tag": "api"},
       "inbounds": [{"listen": "127.0.0.1", "port": 62789, "protocol": "dokodemo-door", "settings": {"address": "127.0.0.1"}, "tag": "api"}, inbound(20001, "node-00")],
       "log": {"access": "none", "error": "", "loglevel": "warning"},
       "outbounds": [{"protocol": "freedom", "settings": {"domainStrategy": "AsIs"}, "tag": "direct"}, {"protocol": "blackhole", "settings": {}, "tag": "blocked"}],
       "policy": {"levels": {"0": {"statsUserDownlink": True, "statsUserUplink": True}}, "system": {"statsInboundDownlink": True, "statsInboundUplink": True}},
       "routing": {"domainStrategy": "AsIs", "rules": [{"inboundTag": ["api"], "outboundTag": "api", "type": "field"},
                   {"ip": ["geoip:private"], "outboundTag": "blocked", "type": "field"}, {"outboundTag": "blocked", "protocol": ["bittorrent"], "type": "field"}]},
       "stats": {}}
open(out, "w").write(json.dumps(cfg, ensure_ascii=False, indent=2) + "\n")
PY
}

relay_run() { # name ip xray
  docker run -d --name p11-$1 --hostname $1 --network $NET --ip $2 -e AUTH_KEY="$(cat $L/keys/relay_ed25519.pub)" \
    -v $L/lab-bin:/lab-bin:ro -v $3:/opt/fleet/xray:ro -v $D/geoip.dat:/opt/fleet/geoip.dat:ro -v $D/geosite.dat:/opt/fleet/geosite.dat:ro \
    $IMG bash -c "mkdir -p /etc/fleet/xray && cp /lab-bin/$1.json /etc/fleet/xray/config.json && chmod 600 /etc/fleet/xray/config.json && exec bash /lab-bin/relay-entry.sh" >/dev/null
}

node_run() { # name ip rootpw authkey
  local n=$1 ip=$2 pw=$3 ak=$4
  docker run -d --name p11-$n --hostname $n --network $NET --ip $ip --privileged --cgroupns=private -v $L/lab-bin:/lab-bin:ro $SD >/dev/null
  for i in $(seq 30); do docker exec p11-$n systemctl is-system-running 2>/dev/null | grep -qE 'running|degraded' && break; sleep 0.5; done
  docker exec p11-$n bash -c "ssh-keygen -A >/dev/null 2>&1; echo 'root:$pw' | chpasswd; mkdir -p /root/.ssh; chmod 700 /root/.ssh; : > /root/.ssh/authorized_keys; chmod 600 /root/.ssh/authorized_keys
    [ -n '$ak' ] && echo '$ak' >> /root/.ssh/authorized_keys
    sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin yes/; s/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
    systemctl reset-failed ssh; systemctl restart ssh"
  # 演练管道：节点回环 25911 → 中控（模拟到 38.58.57.22:443 的出站）
  docker exec -d p11-$n bash -c 'exec python3 /lab-bin/fwd.py 127.0.0.1:25911 11.98.0.5:25912 >> /var/log/p11-fwd.log 2>&1'
}

case "${1:-}" in
up)
  mkdir -p $L/keys $L/lab-bin $L/state/work/secrets $L/fleetd-data $L/cli-state
  [ -f $L/keys/onboard_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -C fleet-onboard-p11lab -f $L/keys/onboard_ed25519
  [ -f $L/keys/relay_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -C fleet-relay-p11lab -f $L/keys/relay_ed25519
  cat > $L/lab-bin/relay-entry.sh <<'EOF'
#!/bin/bash
ssh-keygen -A >/dev/null 2>&1
mkdir -p /root/.ssh && chmod 700 /root/.ssh && echo "$AUTH_KEY" > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys
cat > /usr/local/bin/xray-sup.sh <<'SUP'
#!/bin/bash
while :; do XRAY_LOCATION_ASSET=/opt/fleet /opt/fleet/xray run -c /etc/fleet/xray/config.json >> /var/log/xray.log 2>&1; echo "$(date) xray exited" >> /var/log/xray-restarts.log; sleep 1; done
SUP
chmod +x /usr/local/bin/xray-sup.sh
setsid /usr/local/bin/xray-sup.sh < /dev/null > /dev/null 2>&1 &
exec /usr/sbin/sshd -D -e
EOF
  cat > $L/lab-bin/echo.py <<'EOF'
import http.server, socketserver
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        b = ("fl=lab\nip=%s\nloc=LAB\n" % self.client_address[0]).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
class S(socketserver.ThreadingMixIn, http.server.HTTPServer): daemon_threads = True
S(("0.0.0.0", 80), H).serve_forever()
EOF
  cat > $L/lab-bin/fwd.py <<'EOF'
# 最小 TCP 转发：fwd.py LISTEN_HOST:PORT TARGET_HOST:PORT（演练管道）
import socket, sys, threading
lh, lp = sys.argv[1].rsplit(":", 1); th, tp = sys.argv[2].rsplit(":", 1)
def pipe(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d: break
            b.sendall(d)
    except OSError: pass
    finally:
        for s in (a, b):
            try: s.shutdown(socket.SHUT_RDWR)
            except OSError: pass
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); srv.bind((lh, int(lp))); srv.listen(64)
while True:
    c, _ = srv.accept()
    try: u = socket.create_connection((th, int(tp)), timeout=5); u.settimeout(None)
    except OSError: c.close(); continue
    threading.Thread(target=pipe, args=(c, u), daemon=True).start(); threading.Thread(target=pipe, args=(u, c), daemon=True).start()
EOF
  docker network inspect $NET >/dev/null 2>&1 || docker network create --subnet 11.98.0.0/24 --gateway 11.98.0.1 $NET >/dev/null
  relay_cfg $X28 $L/lab-bin/relay-l.json
  relay_cfg $X27 $L/lab-bin/relay-m.json
  relay_run relay-l 11.98.0.10 $X28
  relay_run relay-m 11.98.0.11 $X27
  docker run -d --name p11-echo --hostname echo --network $NET --ip 11.98.0.200 -v $L/lab-bin:/lab-bin:ro $IMG python3 /lab-bin/echo.py >/dev/null
  AK="$(cat $L/keys/onboard_ed25519.pub)"
  [ -f $L/state/work/secrets/n2.pw ] || { head -c 12 /dev/urandom | base64 | tr -d '/+=' > $L/state/work/secrets/n2.pw; chmod 600 $L/state/work/secrets/n2.pw; }
  # 中控：fleetd + fleetctl（本次构建，只读挂载）、xray 客户端、ssserver 包、nodeagent 发布目录
  docker run -d --name p11-central --hostname central --network $NET --ip 11.98.0.5 \
    -v $L:/lab -v $BIN:/opt/p11-bin:ro -v $X28:/opt/fleet/xray:ro -v $D:/dist:ro -v $ED:/edge-dist:ro -v $R/fleet-edge/deploy:/fleet-edge-deploy:ro $IMG sleep infinity >/dev/null
  docker exec -d p11-central bash -c 'exec python3 /lab/lab-bin/fwd.py 0.0.0.0:25912 127.0.0.1:25911 >> /lab/central-fwd.log 2>&1'
  node_run n1 11.98.0.21 "$(head -c 9 /dev/urandom | base64)" "$AK"
  node_run n2 11.98.0.22 "$(cat $L/state/work/secrets/n2.pw)" ""
  node_run n3 11.98.0.23 "$(head -c 9 /dev/urandom | base64)" "$AK"
  node_run n4 11.98.0.24 "$(head -c 9 /dev/urandom | base64)" "$AK"
  node_run n5 11.98.0.25 "$(head -c 9 /dev/urandom | base64)" "$AK"
  sleep 1
  docker exec p11-central bash -c 'for h in 11.98.0.10 11.98.0.11; do ssh-keyscan -t ed25519 $h 2>/dev/null; done > /lab/keys/relays.known_hosts; wc -l < /lab/keys/relays.known_hosts'
  cat > $L/fleetd.toml <<'EOF'
# P11 演练用 fleetd：自己的端口、自己的 data_dir；旧后端指向不存在的端口（只要 ingest + onboard）
listen = "127.0.0.1:25911"
data_dir = "/lab/fleetd-data"
onboard_config = "/lab/onboard.toml"
[legacy]
base_url = "http://127.0.0.1:9/api/team-ops"
[komari]
enabled = false
[auth]
mode = "dev-allow-local"
[notify]
shadow = true
EOF
  write_onb() { # file db work extra
    cat > $1 <<EOF
db = "$2"
catalog = "/dist/catalog.json"
work_dir = "$3"
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
egress_check_url = "http://11.98.0.200/"
require_bbr = false          # 容器里 /proc/sys 只读
dd_wait_timeout_s = 120

[ss]
tarball = "/dist/shadowsocks-v1.25.0.x86_64-unknown-linux-musl.tar.xz"

[agent]
dist = "/edge-dist"
endpoint = "http://127.0.0.1:25911"   # 生产 https://38.58.57.22（缺省值）
first_batch_timeout_s = 90
$4

[[relays]]
name = "线路机L"
host = "11.98.0.10"
ssh_key = "/lab/keys/relay_ed25519"
known_hosts = "/lab/keys/relays.known_hosts"
public_host = "11.98.0.10"
allow_ips = ["11.98.0.10"]

[[relays]]
name = "线路机M"
host = "11.98.0.11"
ssh_key = "/lab/keys/relay_ed25519"
known_hosts = "/lab/keys/relays.known_hosts"
public_host = "11.98.0.11"
allow_ips = ["11.98.0.11"]
EOF
  }
  write_onb $L/onboard.toml /lab/state/onboard.db /lab/state/work ""
  # fleetctl 独立运行（命令模式令牌后端）：自己的库，令牌经 fleetd agent-token 命令签发到同一个 ingest.db
  write_onb $L/onboard-cli.toml /lab/cli-state/onboard.db /lab/cli-state/work \
    "token_cmd = \"/opt/p11-bin/fleetd agent-token issue {host_id} --name onboard --config /lab/fleetd.toml\"
revoke_cmd = \"/opt/p11-bin/fleetd agent-token revoke {host_id} --config /lab/fleetd.toml\""
  docker ps --filter name=p11- --format '{{.Names}}\t{{.Status}}'
  ;;
fleetd)
  shift
  docker exec p11-central bash -c 'pkill -f "^/opt/p11-bin/fleetd serve" ; sleep 0.5; true'
  docker exec -d "$@" p11-central bash -c 'cd /lab && exec /opt/p11-bin/fleetd serve --config /lab/fleetd.toml >> /lab/fleetd.log 2>&1'
  for i in $(seq 40); do docker exec p11-central curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:25911/v2/ingest/node 2>/dev/null | grep -q 401 && { echo "fleetd 已起（/v2/ingest/node → 401）"; exit 0; }; sleep 0.5; done
  echo "fleetd 没起来"; docker exec p11-central tail -20 /lab/fleetd.log; exit 1
  ;;
api)
  shift; M=$1 P=$2 B=${3:-}
  docker exec p11-central curl -sS -X "$M" -H 'X-Fleet-Request: 1' -H 'Content-Type: application/json' ${B:+--data-binary "$B"} "http://127.0.0.1:25911/v2/api/v1/onboard$P"; echo
  ;;
fc)
  shift; docker exec -e FLEET_ONBOARD_CONFIG=/lab/onboard-cli.toml -e FLEET_OPERATOR=lab -w /lab/cli-state p11-central /opt/p11-bin/fleetctl onboard "$@"
  ;;
tokens)
  docker exec p11-central /opt/p11-bin/fleetd agent-token list --config /lab/fleetd.toml
  ;;
down)
  docker rm -f $(docker ps -aq --filter name=p11-) >/dev/null 2>&1 || true
  docker network rm $NET >/dev/null 2>&1 || true
  ;;
*) echo "用法：p11-lab.sh up|down|fleetd [docker exec 选项]|api M P [JSON]|fc …|tokens"; exit 2 ;;
esac
