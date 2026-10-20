#!/usr/bin/env python3
"""a395 管理模式验收用的 line3 副本（端口整体平移，互不干扰）：
入站 200xx → 360xx、api 62789 → 36789、SS 上游 410xx → 460xx；按 fleet-relay.py 的 hardened() 做 R1 加固
（outbounds[0] = fleet-default-deny blackhole，靠默认出站出网的入口补 fleet-explicit-* 显式规则）。"""
import json, os, stat
H = os.path.expanduser("~/team-ops-eval")
SRC = H + "/fleet-cutover/replica/line3"
OUT = H + "/fleet-edge-build/ops"
IN_OFF, SS_OFF = 16000, 5000
os.makedirs(OUT + "/etc", exist_ok=True)
os.makedirs(OUT + "/log", exist_ok=True)
c = json.load(open(SRC + "/etc/fleet/xray/config.json"))
ren = lambda t: "inbound-%d" % (int(t.split("-")[1]) + IN_OFF) if t.startswith("inbound-") else t
for i in c["inbounds"]:
    if i["tag"] == "api":
        i["port"] = 36789
    else:
        i["port"] += IN_OFF
        i["tag"] = ren(i["tag"])
for r in c["routing"]["rules"]:
    if "inboundTag" in r:
        r["inboundTag"] = [ren(t) for t in r["inboundTag"]]
for o in c["outbounds"]:
    if o["protocol"] == "shadowsocks":
        for s in o["settings"].get("servers", []):
            s["port"] += SS_OFF
c["log"]["access"] = OUT + "/log/xray-access.log"
MATCH = ("domain", "ip", "source", "port", "network", "protocol", "user", "attrs")
default = c["outbounds"][0]["tag"]
explicit = []
for i in c["inbounds"]:
    t = i["tag"]
    if t == "api":
        continue
    if not any(t in r.get("inboundTag", []) and not any(r.get(k) for k in MATCH) for r in c["routing"]["rules"]):
        explicit.append({"type": "field", "inboundTag": [t], "outboundTag": default, "ruleTag": "fleet-explicit-" + t})
c["routing"]["rules"] += explicit
c["outbounds"] = [{"tag": "fleet-default-deny", "protocol": "blackhole", "settings": {}}] + c["outbounds"]
p = OUT + "/etc/config.json"
with open(p, "w") as f:
    json.dump(c, f, ensure_ascii=False, indent=2)
os.chmod(p, 0o600)
ss = json.load(open(SRC + "/ss-upstreams.json"))
for s in ss["servers"]:
    s["server_port"] += SS_OFF
json.dump(ss, open(OUT + "/ss-upstreams.json", "w"), indent=1)
e = json.load(open(SRC + "/entries.json"))
for x in e["entries"]:
    x["port"] += IN_OFF
    x["relay"] = "ops-replica"
json.dump(e, open(OUT + "/entries.json", "w"), ensure_ascii=False, indent=1)
os.chmod(OUT + "/entries.json", 0o600)
print("inbounds", [i["tag"] for i in c["inbounds"]])
print("explicit", [r["ruleTag"] for r in explicit], "default→", default)
