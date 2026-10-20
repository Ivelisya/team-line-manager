#!/usr/bin/env python3
import json, os, sqlite3, subprocess, time
H = os.path.expanduser("~/team-ops-eval"); OUT = H + "/fleet-edge-build/ops"
DB = OUT + "/state/probe.db"
PASS = FAIL = 0
def ok(c, what):
    global PASS, FAIL
    PASS += bool(c); FAIL += (not c)
    print(time.strftime("%H:%M:%S"), ("  ✔ " if c else "  ✘ ") + what, flush=True)
def rows(since):
    if not os.path.exists(DB):
        return []
    c = sqlite3.connect("file:%s?mode=ro" % DB, uri=True)
    return [json.loads(d) for (d,) in c.execute("SELECT doc FROM probe_raw WHERE check_kind='path' AND measured_at>? ORDER BY id", (since,))]
def clients():
    r = subprocess.run(["pgrep", "-f", "pathcheck-client.json"], capture_output=True, text=True)
    return len(r.stdout.split())
def e2e():
    out = OUT + "/e2e-path.json"
    subprocess.run(["python3", H + "/fleet-cutover/bin/e2e.py", "run", "--entries", OUT + "/entries.json", "--bin-dir", H + "/fleet-cutover/dist/linux",
                    "--clients", "xray", "--tries", "2", "--json", out], capture_output=True, timeout=300)
    return {r["port"]: r["ok"] for r in json.load(open(out))["results"]}
t0 = time.time()
base = e2e()
ok(all(base.values()), "副本 e2e 基线 %d/%d" % (sum(base.values()), len(base)))
# 第 1 个周期（启动后立即）
while len({r["path"]["entry_port"] for r in rows(t0)}) < 12 and time.time() - t0 < 90:
    time.sleep(1)
r1 = rows(t0)
byport = {r["path"]["entry_port"]: r for r in r1}
ok(len(byport) == 12, "一个周期覆盖全部 12 个 vless+reality 入口")
healthy = {p: r["path"]["healthy"] for p, r in byport.items()}
ok(healthy == base, "path 结论与客户视角 e2e 逐入口一致（%d/%d healthy）" % (sum(healthy.values()), len(healthy)))
s = byport.get(36002, {})
ok(s.get("status") == "ok" and s.get("legacy_rc") == 0 and s["path"]["rc"] == 0 and s["path"]["phase"] is None and s.get("actual_ip") == s.get("expected_ip")
   and s["path"]["attempts"][0]["rc"] == 0 and s.get("value_ms") and s.get("node") == "node-36002", "行字段：healthy/rc 0/phase null/actual_ip=expected_ip/ms/attempts/node 映射 %s" % json.dumps(s, ensure_ascii=False)[:300])
time.sleep(3)
ok(clients() == 1, "全链路客户端常驻：恰好 1 个 xray 客户端进程（跨周期复用）")
rp = subprocess.run(["systemctl", "--user", "show", "fe-relayd", "-p", "MainPID", "--value"], capture_output=True, text=True).stdout.strip()
kids = subprocess.run(["ps", "--ppid", rp, "-o", "stat=,comm="], capture_output=True, text=True).stdout.split("\n")
ok(not any(k.strip().startswith("Z") for k in kids), "relayd（pid %s）没有僵尸子进程：%s" % (rp, [k for k in kids if k.strip()]))
# 故障：停掉 SS 上游 → SS 类入口失败，直连入口 36001 照常
subprocess.run(["systemctl", "--user", "stop", "fe-ss"])
t1 = time.time()
while len({r["path"]["entry_port"] for r in rows(t1)}) < 12 and time.time() - t1 < 150:
    time.sleep(1)
r2 = {r["path"]["entry_port"]: r for r in rows(t1)}
fails = sorted(p for p, r in r2.items() if not r["path"]["healthy"])
ok(r2.get(36001, {}).get("path", {}).get("healthy") and len(fails) == 11 and all(r2[p]["path"]["rc"] != 0 and len(r2[p]["path"]["attempts"]) == 2 for p in fails),
   "SS 上游停掉：11 个 SS 入口失败（各 2 次尝试，rc=%s），直连 36001 正常" % sorted({r2[p]["path"]["rc"] for p in fails}))
print("  失败行示例：", json.dumps(r2[fails[0]], ensure_ascii=False)[:400] if fails else None)

lines = [l for l in open(OUT + "/log/xray-access.log") if "www.cloudflare.com:443" in l]
ok(lines and all(" from 127.0.0.1:" in l for l in lines), "访问日志里的检查流量 %d 行，来源全是 127.0.0.1" % len(lines))
t2 = time.time()
acc = []
while time.time() - t2 < 60:
    batches = [json.loads(l) for l in open(OUT + "/mock/batches.jsonl")] if os.path.exists(OUT + "/mock/batches.jsonl") else []
    acc = [b["access"] for b in batches if b.get("access")]
    if acc and acc[-1]["counters"]["loopback"] > 0:
        break
    time.sleep(2)
ok(acc and all(e["source_ip"] != "127.0.0.1" for a in acc for e in a.get("events", [])) and acc[-1]["counters"]["loopback"] > 0,
   "检查流量在访问日志里全是回环（loopback=%d），没有一条成为客户活动事件" % (acc[-1]["counters"]["loopback"] if acc else -1))
print("结果：%d 通过，%d 失败" % (PASS, FAIL))
raise SystemExit(0 if FAIL == 0 else 1)
