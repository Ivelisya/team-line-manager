#!/usr/bin/env python3
# 核对 a395-e2e 的结果（只读 ingest.db 和 agent 状态文件）
import json, sqlite3, os, sys, glob
R = os.path.expanduser("~/team-ops-eval/fleet-edge-build/run")
db = sqlite3.connect(f"file:{R}/ingest.db?mode=ro", uri=True)
ok = True
for host, sd in (("a395-node", "node-state"), ("a395-relay", "relay-state")):
    rows = db.execute("SELECT boot_id, seq FROM ingest_seen WHERE host_id=? ORDER BY seq", (host,)).fetchall()
    seqs = [r[1] for r in rows]
    gaps = [(a, b) for a, b in zip(seqs, seqs[1:]) if b != a + 1]
    st = json.load(open(f"{R}/{sd}/status.json"))
    n1m = db.execute("SELECT SUM(cpu_n), SUM(mem_n), COUNT(*) FROM host_1m WHERE host_id=?", (host,)).fetchone()
    lat = db.execute("SELECT samples, batches FROM host_latest WHERE host_id=?", (host,)).fetchone()
    print(f"{host}: batches_accepted={len(seqs)} seq {seqs[0]}..{seqs[-1]} gaps={gaps} agent_last_seq={st['seq']} pending={st['pending']} dropped={st['dropped']} rejected={st['rejected']}")
    print(f"   samples: host_latest={lat[0]} host_1m(mem_n)={n1m[1]} minutes={n1m[2]}")
    if gaps or st['pending'] or st['dropped'] or st['rejected'] or seqs[-1] != st['seq'] or lat[0] != n1m[1]:
        ok = False; print("   !! MISMATCH")
dups = sum(open(f).read().count("push_duplicate") for f in glob.glob(f"{R}/logs/*agent*.log") + glob.glob(f"{R}/logs/relayd.log"))
print(f"agent-side duplicate acks: {dups} (server dedupe; not double-counted)")
# Xray 累计值：种子 + 重启前最终原始值 + 重启后原始值
b = json.load(open(f"{R}/raw-before-restart.json")); a = json.load(open(f"{R}/raw-after-restart.json"))
seed = {("inbound","inbound-25921"): (1000, 2000), ("user","client-1"): (10, 20)}
for (k, t) in [("inbound","inbound-25921"), ("inbound","inbound-25922"), ("user","client-1")]:
    su, sdn = seed.get((k, t), (0, 0))
    exp_u = su + b.get(f"{k}>>>{t}>>>traffic>>>uplink", 0) + a.get(f"{k}>>>{t}>>>traffic>>>uplink", 0)
    exp_d = sdn + b.get(f"{k}>>>{t}>>>traffic>>>downlink", 0) + a.get(f"{k}>>>{t}>>>traffic>>>downlink", 0)
    row = db.execute("SELECT cum_up, cum_down, raw_up, raw_down FROM xray_latest WHERE host_id='a395-relay' AND kind=? AND tag=?", (k, t)).fetchone()
    w = db.execute("SELECT SUM(up), SUM(down) FROM xray_1m WHERE host_id='a395-relay' AND kind=? AND tag=?", (k, t)).fetchone()
    good = row and row[0] == exp_u and row[1] == exp_d and (w[0] or 0) + su == exp_u and (w[1] or 0) + sdn == exp_d
    ok &= bool(good)
    print(f"xray {k}/{t}: cum=({row[0]},{row[1]}) expected=({exp_u},{exp_d}) sum(xray_1m)+seed=({(w[0] or 0)+su},{(w[1] or 0)+sdn}) {'OK' if good else '!! MISMATCH'}")
print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
