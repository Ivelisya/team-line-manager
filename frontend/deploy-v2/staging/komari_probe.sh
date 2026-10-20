#!/bin/bash
# komari_probe.sh — 在 a395 上用真实 Komari 二进制读一次 RPC 结果，只为确认返回格式（stage_server 的 komari/* 镜像按它写）。
#
# 安全：
#   - 只用 ~/team-ops-eval/data 下 komari.db / metrics.db 的新副本（cp --reflink），原文件不动；
#   - 副本里关掉通知、GeoIP、登录通知、到期通知，删掉消息发送渠道，客户端 token 换成假值，探测目标改成本机 9 号端口；
#   - Komari 跑在 unshare -Urn 的独立网络命名空间里，只有 lo，连不到任何外部地址（生产机、线路机都连不到）；
#   - 跑完即停，不留常驻进程。
# 用法：bash komari_probe.sh [uuid] [task_id]
set -euo pipefail
B=~/team-ops-eval/v2-stage/komari-probe
BIN=~/team-ops-eval/bin/komari
PY=~/team-ops-eval/teamops/runtime/python/bin/python3.12
umask 077
rm -rf "$B"; mkdir -p "$B/data" "$B/out"
cp --reflink=auto ~/team-ops-eval/data/komari.db ~/team-ops-eval/data/metrics.db "$B/data/"
TOKEN="v2probe$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
echo "$TOKEN" > "$B/session.txt"
sqlite3 "$B/data/komari.db" <<SQL
UPDATE configs SET value='false' WHERE key IN ('notification_enabled','geo_ip_enabled','login_notification','expire_notification_enabled');
DELETE FROM message_sender_providers;
DELETE FROM sessions;
UPDATE clients SET token = 'probe-' || substr(uuid,1,8) || '-' || rowid;
UPDATE ping_tasks SET target='127.0.0.1:9' WHERE type='tcp';
UPDATE ping_tasks SET target='http://127.0.0.1:9/' WHERE type='http';
INSERT INTO sessions(uuid, session, user_agent, ip, login_method, latest_online, expires, created_at)
  SELECT uuid, '$TOKEN', 'v2probe', '127.0.0.1', 'password', datetime('now'), datetime('now','+1 days'), datetime('now') FROM users LIMIT 1;
SQL
if [ "${SHIFT:-0}" = 1 ]; then
  # 把副本里的指标时间整体平移到“现在”附近（整小时平移，桶对齐不变），用来看 1h / 4h 查询的真实分辨率
  END=$(sqlite3 "$B/data/metrics.db" "select max(bucket_milli) from metric_rollups where resolution_id=1")
  D=$(( ( ($(date +%s)*1000 - 600000 - END) / 3600000 ) * 3600000 ))
  sqlite3 "$B/data/metrics.db" "UPDATE metric_rollups SET bucket_milli=bucket_milli+$D+1000000000000000; UPDATE metric_rollups SET bucket_milli=bucket_milli-1000000000000000, first_ts_milli=first_ts_milli+$D, last_ts_milli=last_ts_milli+$D;"
  echo "shifted metrics by $D ms"
fi
UUID=${1:-$(sqlite3 "$B/data/komari.db" "select uuid from clients where cpu_cores>0 order by name limit 1")}
TASK=${2:-$(sqlite3 "$B/data/komari.db" "select id from ping_tasks order by id limit 1")}
cat > "$B/capture.py" <<'PY'
import json, sys, time, urllib.request
token, uuid, task, out = sys.argv[1:5]
def rpc(method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request("http://127.0.0.1:25774/api/rpc2", data=body, method="POST",
                                 headers={"Content-Type": "application/json", "Cookie": "session_token=" + token})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read())
for i in range(60):
    try:
        urllib.request.urlopen("http://127.0.0.1:25774/api/version", timeout=2); break
    except Exception:
        time.sleep(0.5)
calls = {
    "records_all_24": ("public:getRecordsByUUID", {"uuid": uuid, "load_type": "all", "hours": "24"}),
    "records_all_4": ("public:getRecordsByUUID", {"uuid": uuid, "load_type": "all", "hours": "4"}),
    "records_all_1": ("public:getRecordsByUUID", {"uuid": uuid, "load_type": "all", "hours": "1"}),
    "ping_records_task_uuid_1": ("public:getPingRecords", {"task_id": str(task), "hours": "1", "uuid": uuid}),
    "ping_records_task_uuid_4": ("public:getPingRecords", {"task_id": str(task), "hours": "4", "uuid": uuid}),
    "ping_records_task_uuid": ("public:getPingRecords", {"task_id": str(task), "hours": "24", "uuid": uuid}),
    "ping_records_task": ("public:getPingRecords", {"task_id": str(task), "hours": "24"}),
    "ping_tasks": ("admin:getAllPingTasks", {}),
    "clients": ("admin:listClients", {}),
    "latest": ("common:getNodesLatestStatus", {}),
    "public_settings": ("public:getPublicSettings", {}),
}
for name, (m, p) in calls.items():
    try:
        res = rpc(m, p)
    except Exception as e:
        res = {"probe_error": repr(e)}
    with open(f"{out}/{name}.json", "w") as f:
        json.dump(res, f, ensure_ascii=False)
    print(name, len(json.dumps(res)))
print("now", time.time())
PY
cd "$B"
unshare -Urn bash -c "ip link set lo up; $BIN server -d ./data/komari.db -l 127.0.0.1:25774 > komari.log 2>&1 & KP=\$!; sleep 3; $PY capture.py '$TOKEN' '$UUID' '$TASK' '$B/out'; kill \$KP; wait \$KP 2>/dev/null || true"
echo "uuid=$UUID task=$TASK"
ls -la "$B/out"
