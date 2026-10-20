#!/usr/bin/env python3
import json, os, sqlite3, subprocess, time
OUT = os.path.expanduser("~/team-ops-eval/fleet-edge-build/ops")
c = sqlite3.connect("file:%s/state/probe.db?mode=ro" % OUT, uri=True)
rows = [json.loads(d) for (d,) in c.execute("SELECT doc FROM probe_raw ORDER BY id")]
eg = [r for r in rows if r["node"] == "relay-self-test" and r["check"] == "egress"]
pt = [r for r in rows if r["check"] == "tcp" and r["node"].startswith("peer-")]
F = 0
def ok(c, w):
    global F
    F += (not c); print(("  ✔ " if c else "  ✘ ") + w, flush=True)
ok(eg and all(r["target"] == "127.0.0.1:36001" and r["expected_ip"] for r in eg), "线路机自检 egress 行：%d 条，target=127.0.0.1:36001" % len(eg))
fin = [r for r in eg if r.get("final")]
print("   自检 final 行：", [(r["status"], r.get("actual_ip"), r.get("value_ms"), r.get("reason_code")) for r in fin][:6])
# SS 上游在 pathcheck_verify 里被停掉过，但主节点入口直连不受影响
ok(fin and fin[-1]["status"] == "ok" and fin[-1]["actual_ip"] == fin[-1]["expected_ip"], "最新一次自检 ok 且出口 = 本机公网 IP")
up = [r for r in pt if r["node"] == "peer-up"]; dn = [r for r in pt if r["node"] == "peer-down"]
ok(up and all(r["status"] == "ok" and r["target"] == "127.0.0.1:36001" and r["value_ms"] >= 1 for r in up), "互探 peer-up：%d 条 ok" % len(up))
ok(dn and all(r["status"] == "fail" and r["reason_code"] == "upstream_down" and r["legacy_rc"] == 7 for r in dn), "互探 peer-down：%d 条 fail/upstream_down/7" % len(dn))
log = open(OUT + "/relayd.log").read() if os.path.exists(OUT + "/relayd.log") else ""
starts = log.count("path_client_started")
ok(starts == 1, "常驻客户端：整个测试期间只启动了 %d 次（跨周期复用）" % starts)
raise SystemExit(1 if F else 0)
