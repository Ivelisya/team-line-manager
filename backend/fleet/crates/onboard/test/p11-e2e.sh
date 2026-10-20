#!/bin/bash
# P11 演练（a395，~/team-ops-eval/fleet-build-p11 下执行）：真实 fleetd（ingest + 内嵌上架流水线）驱动 P1–P16，
# P11 装 nodeagent 到 systemd 节点，并核对：第一批被 fleetd-ingest 接收、agent_tokens 有令牌、布局与 rollout.sh 装出来的逐项一致；
# 崩溃后重跑 P11（沿用令牌、不重复）；P11 失败 → 重试；取消 → 补偿（停服务、删文件、撤销令牌）；fleetctl 独立运行（命令模式）。
set -uo pipefail
cd "$(dirname "$0")"
source ~/team-ops-eval/tools/env.sh
say() { printf '\n\033[1m### %s\033[0m\n' "$*"; }
J() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }
api() { ./p11-lab.sh api "$@"; }
csv() { # 行… → JSON body
  python3 - "$@" <<'PY'
import json, sys
hdr = "provider,ip,ssh_port,auth,plan,team,number,relay,ss_port,dd,mac,service_id,note"
print(json.dumps({"csv": hdr + "\n" + "\n".join(sys.argv[1:]) + "\n", "title": "P11 演练"}, ensure_ascii=False))
PY
}
new_batch() { # 行… → 批次 id（并 start）
  local body out id hash
  body=$(csv "$@")
  out=$(api POST /batches "$body")
  id=$(echo "$out" | J 'd["data"]["batch_id"]'); hash=$(echo "$out" | J 'd["data"]["plan_hash"]')
  echo "$out" | J '"计划：" + str(len(d["data"]["rows"])) + " 行，问题 " + str(d["data"]["issues"]) + "，计划哈希 " + d["data"]["plan_hash"]' >&2
  api POST /batches/$id/start "{\"plan_hash\":\"$hash\"}" >&2
  echo "$id"
}
wait_batch() { # id 超时秒 → 打印最终状态
  local id=$1 t=${2:-400} st
  for i in $(seq $t); do
    st=$(api GET /batches/$id | J 'd["data"]["status"]')
    case "$st" in succeeded|attention|cancelled) break ;; esac
    sleep 1
  done
  api GET /batches/$id | J '"批次 %s：%s；" % (d["data"]["id"], d["data"]["status_word"]) + "；".join("%s %s %s" % (r["name"], r["row_state"], r["current_step"]) for r in d["data"]["rows"])'
}
cell() { # id 行名 步骤 → 格子
  api GET /batches/$1 | python3 -c "import json,sys; d=json.load(sys.stdin)['data']; r=[x for x in d['rows'] if x['name']=='$2'][0]; c=[x for x in r['cells'] if x['step']=='$3'][0]; print(json.dumps(c, ensure_ascii=False))"
}
p11_out() { # 行名 → P11 的 outputs（直接读 onboard.db）
  docker exec p11-central python3 -c "
import sqlite3, json
c = sqlite3.connect('/lab/state/onboard.db')
r = c.execute(\"SELECT s.status, s.attempt, s.outputs, s.detail FROM steps s JOIN rows r ON r.id=s.row_id WHERE r.name LIKE '$1%' AND s.step='P11' ORDER BY r.created_at DESC LIMIT 1\").fetchone()
print(json.dumps({'status': r[0], 'attempt': r[1], 'outputs': json.loads(r[2] or 'null'), 'detail': r[3]}, ensure_ascii=False))"
}
tokens() { # host → 该 host 的令牌（名字、是否撤销、最后接收）
  docker exec p11-central /opt/p11-bin/fleetd agent-token list --config /lab/fleetd.toml 2>/dev/null | grep "\"$1\"" | python3 -c "
import json, sys
for l in sys.stdin:
    t = json.loads(l); print('  令牌 name=%s sha=%s… revoked=%s last_received=%s' % (t['name'], t['sha256_prefix'], 'yes' if t['revoked_at'] else 'no', t['last_received']))"
}
layout() { # 节点 → 安装布局（去掉 host_id 与令牌内容）
  docker exec p11-$1 bash -c '
    for p in /opt/fleet-edge /opt/fleet-edge/releases /opt/fleet-edge/releases/* /opt/fleet-edge/releases/*/nodeagent /etc/fleet-edge /etc/fleet-edge/* \
             /etc/systemd/system/fleet-nodeagent.service /etc/systemd/system/fleet-nodeagent.service.d /etc/systemd/system/fleet-nodeagent.service.d/* /var/lib/fleet-nodeagent; do
      [ -e "$p" ] && stat -c "%A %U:%G %n" "$p"; done
    echo "current -> $(readlink /opt/fleet-edge/current)"
    echo "agent.toml sha(去 host_id)=$(grep -v ^host_id /etc/fleet-edge/agent.toml | sha256sum | cut -c1-16)"
    echo "unit sha=$(sha256sum < /etc/systemd/system/fleet-nodeagent.service | cut -c1-16) dropin sha=$(sha256sum < /etc/systemd/system/fleet-nodeagent.service.d/10-local.conf | cut -c1-16)"
    echo "token 字节=$(wc -c < /etc/fleet-edge/token 2>/dev/null)"
    echo "user: $(getent passwd fleet-edge | cut -d: -f1,3,4,6,7) group: $(getent group fleet-edge | cut -d: -f1)"
    echo "unit: enabled=$(systemctl is-enabled fleet-nodeagent) active=$(systemctl is-active fleet-nodeagent) $(systemctl show fleet-nodeagent -p User -p NRestarts | tr "\n" " ")"' 2>/dev/null
}

./p11-lab.sh fleetd >/dev/null || exit 1

say "A. fleetd 模式全链路：团队9-01（金丝雀，auth=key）P1–P16"
A=$(new_batch "ATTVPS,11.98.0.21,22,key,基础版,团队9,1,线路机L,,,,91001,P11 演练")
wait_batch $A 300
echo "P11 格子：$(cell $A 团队9-01 P11)"
echo "P11 输出：$(p11_out 团队9-01)"
tokens 团队9-01
echo "fleetd /v2/api/v1/hosts：$(docker exec p11-central curl -s http://127.0.0.1:25911/v2/api/v1/hosts | python3 -c "import json,sys; d=json.load(sys.stdin); d=d.get('data',d); h=[x for x in (d if isinstance(d,list) else d.get('hosts',[])) if x.get('host_id')=='团队9-01']; print(json.dumps({k: h[0].get(k) for k in ('host_id','kind','online','age_s','agent','batches')} if h else '无', ensure_ascii=False))")"
echo "节点 n1 布局："; layout n1 | sed 's/^/  /'

say "B. 参照：用 fleet-edge/deploy/rollout.sh 原样给 n5 装一遍（令牌用 fleetd agent-token 签发），与 P11 装出来的逐项对比"
docker exec p11-central bash -c '
  mkdir -p /root/.ssh && cp /lab/keys/onboard_ed25519 /root/.ssh/id_ed25519 && chmod 600 /root/.ssh/id_ed25519
  ssh-keyscan -t ed25519 11.98.0.25 2>/dev/null > /root/.ssh/known_hosts
  printf "#!/bin/sh\nshift 2; exec sha256sum \"\$@\"\n" > /usr/local/bin/shasum; chmod +x /usr/local/bin/shasum
  /opt/p11-bin/fleetd agent-token issue 团队9-05 --name default --config /lab/fleetd.toml > /tmp/t5 2>/dev/null
  DIST=/edge-dist bash /fleet-edge-deploy/rollout.sh 团队9-05 11.98.0.25 direct --endpoint http://127.0.0.1:25911 --token-file /tmp/t5 --force 2>&1 | tail -4; rm -f /tmp/t5'
layout n1 > lab/p11-n1.layout; layout n5 > lab/p11-n5.layout
# 系统用户的 uid/gid 数值不比（n1 上 P9 先建了 fleet-ss，编号往后挪）
norm() { sed -e 's/ [0-9]*$//' -e 's/^user: fleet-edge:[0-9]*:[0-9]*:/user: fleet-edge:UID:GID:/' "$1" | grep -v NRestarts; }
if diff <(norm lab/p11-n1.layout) <(norm lab/p11-n5.layout) >lab/p11-layout.diff; then echo "✓ 布局与 rollout.sh 装出来的完全一致（权限、属主、版本目录、current、agent.toml（除 host_id）、单元、drop-in、令牌长度、用户、enable/active；uid/gid 数值除外）"; else echo "✗ 有差异："; cat lab/p11-layout.diff; fi
docker exec p11-n1 cat /etc/fleet-edge/agent.toml | sed 's/^/  n1 agent.toml: /'

say "C. 崩溃后重跑 P11（团队9-02，auth=password）：P11 做完、未记账时 fleetd abort → 重启 fleetd → resume"
./p11-lab.sh fleetd -e FLEET_ONB_CRASH=P11@团队9-02 >/dev/null
C=$(new_batch "ATTVPS,11.98.0.22,22,password:n2.pw,基础版,团队9,2,线路机L,,,,91002,P11 崩溃重跑")
for i in $(seq 200); do docker exec p11-central pgrep -f '^/opt/p11-bin/fleetd serve' >/dev/null || break; sleep 1; done
echo "fleetd 已退出（崩溃注入）：$(docker exec p11-central grep -c '崩溃注入' /lab/fleetd.log) 条崩溃日志；此时 P11 格子：$(cell $C 团队9-02 P11 2>/dev/null || echo '（fleetd 不在，读库）') $(p11_out 团队9-02 | J 'd["status"]')"
tokens 团队9-02
echo "节点上：$(docker exec p11-n2 bash -c 'echo active=$(systemctl is-active fleet-nodeagent) pid=$(systemctl show -p MainPID --value fleet-nodeagent) token_sha=$(tr -d "[:space:]" < /etc/fleet-edge/token | sha256sum | cut -c1-12)')"
PID_BEFORE=$(docker exec p11-n2 systemctl show -p MainPID --value fleet-nodeagent)
sleep 7   # 租约 6 s 过期
./p11-lab.sh fleetd >/dev/null
api POST /batches/$C/resume >/dev/null
wait_batch $C 300
echo "重跑后 P11 输出：$(p11_out 团队9-02 | J 'json.dumps({k: d["outputs"].get(k) for k in ("token","token_sha256","binary_uploaded","service","verified_by","elapsed_s")}, ensure_ascii=False) + " attempt=" + str(d["attempt"])')"
echo "节点上：$(docker exec p11-n2 bash -c 'echo active=$(systemctl is-active fleet-nodeagent) pid=$(systemctl show -p MainPID --value fleet-nodeagent) token_sha=$(tr -d "[:space:]" < /etc/fleet-edge/token | sha256sum | cut -c1-12) releases=$(ls /opt/fleet-edge/releases | wc -l)') （崩溃前 pid=$PID_BEFORE）"
tokens 团队9-02

say "D. P11 失败 → 重试 → 之后取消该行 → 补偿（团队9-03）"
docker exec p11-n3 pkill -f 'fwd.py 127.0.0.1:25911' ; sleep 0.5
D=$(new_batch "ATTVPS,11.98.0.23,22,key,基础版,团队9,3,线路机M,,,,91003,P11 失败与补偿")
wait_batch $D 300
echo "P11 格子（到中控的出站被断开）：$(cell $D 团队9-03 P11)"
tokens 团队9-03; echo "  （预检失败在签发之前：应没有令牌）"
docker exec -d p11-n3 bash -c 'exec python3 /lab-bin/fwd.py 127.0.0.1:25911 11.98.0.5:25912 >> /var/log/p11-fwd.log 2>&1'; sleep 0.5
api POST /batches/$D/rows/团队9-03/actions '{"action":"retry"}'
api POST /batches/$D/resume >/dev/null
for i in $(seq 300); do s=$(api GET /batches/$D | J '[r for r in d["data"]["rows"]][0]["current_step"]'); case $s in P12|P13|P14|P15) break ;; esac; sleep 0.3; done
echo "重试后 P11 格子：$(cell $D 团队9-03 P11)（现在在 $s）"
tokens 团队9-03
echo "发取消："; api POST /batches/$D/cancel '{"row":"团队9-03"}'
wait_batch $D 120
api GET /batches/$D | python3 -c "import json,sys; d=json.load(sys.stdin)['data']; r=d['rows'][0]; print('格子：' + ' '.join('%s=%s' % (c['step'], c['status']) for c in r['cells']))"
echo "P11 补偿说明：$(p11_out 团队9-03 | J 'd["detail"]')"
echo "节点上：$(docker exec p11-n3 bash -c 'echo unit_files=$(systemctl list-unit-files fleet-nodeagent.service --no-legend | wc -l) paths=$(ls -d /opt/fleet-edge /etc/fleet-edge /var/lib/fleet-nodeagent 2>/dev/null | wc -l) user=$(getent passwd fleet-edge >/dev/null && echo 有 || echo 无) ssserver=$(ls /usr/local/bin/ssserver 2>/dev/null | wc -l)')"
tokens 团队9-03

say "E. fleetctl 独立运行（命令模式：[agent].token_cmd = fleetd agent-token issue …；首批以节点 status.json 为准）：团队9-04"
cat > lab/cli-state/batch.csv <<'EOF'
provider,ip,ssh_port,auth,plan,team,number,relay,ss_port,dd,mac,service_id,note
ATTVPS,11.98.0.24,22,key,基础版,团队9,4,线路机M,,,,91004,fleetctl 命令模式
EOF
H=$(./p11-lab.sh fc plan batch.csv | grep -o 'confirm [0-9a-f]*' | cut -d' ' -f2)
./p11-lab.sh fc run batch.csv --confirm "$H" 2>&1 | grep -E '团队9-04 P1[01] |✓|"status"' | tail -4
docker exec p11-central python3 -c "
import sqlite3, json
c = sqlite3.connect('/lab/cli-state/onboard.db')
r = c.execute(\"SELECT s.status, s.outputs FROM steps s WHERE s.step='P11'\").fetchone(); o = json.loads(r[1] or '{}')
print('P11', r[0], json.dumps({k: o.get(k) for k in ('token','token_source','verified_by','service','elapsed_s')}, ensure_ascii=False))"
tokens 团队9-04
echo "重跑同一份 CSV（行已上线，不应有任何动作）："; ./p11-lab.sh fc run batch.csv --confirm "$H" 2>&1 | tail -1
tokens 团队9-04

say "F. 汇总：所有令牌"
./p11-lab.sh tokens
echo "fleetd 日志里的 P11："; grep -E 'P11 ' lab/fleetd.log | sed 's/^/  /' | tail -12
