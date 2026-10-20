#!/usr/bin/env python3
"""relayd 管理模式端到端验收（a395，line3 副本，端口平移）。
每个操作：下发 spec（或命令）→ 等回执 → 检查回执里的差异只含预期对象 → 核对磁盘配置与 Xray 运行态 → 经受影响的入口做客户视角 e2e。
结果写到 ops/report.json，并打印逐项 ✔/✘。"""
import copy, hashlib, json, os, subprocess, sys, time, uuid

H = os.path.expanduser("~/team-ops-eval")
OUT = H + "/fleet-edge-build/ops"
M = OUT + "/mock"
CFG = OUT + "/etc/config.json"
XRAY = H + "/fleet-cutover/dist/xray-26.7.28-64d46afb"
E2E = H + "/fleet-cutover/bin/e2e.py"
BIN = H + "/fleet-cutover/dist/linux"
ENTRIES = OUT + "/entries.json"
API = "127.0.0.1:36789"
REPORT = {"steps": []}
PASS = FAIL = 0


def say(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def ok(cond, what, **ev):
    global PASS, FAIL
    if cond:
        PASS += 1
    else:
        FAIL += 1
    say(("  ✔ " if cond else "  ✘ ") + what)
    REPORT["steps"].append({"ok": bool(cond), "what": what, **ev})


def sha_file(p):
    return hashlib.sha256(open(p, "rb").read()).hexdigest()


def cfg():
    return json.load(open(CFG))


def xapi(*a):
    r = subprocess.run([XRAY, "api", a[0], "--server=" + API, *a[1:]], capture_output=True, text=True, timeout=20)
    return json.loads(r.stdout) if r.stdout.strip() else {}


def runtime():
    return {"inbounds": sorted(i["tag"] for i in xapi("lsi").get("inbounds", [])),
            "rules": [r.get("tag") for r in xapi("lsrules").get("rules", [])]}


def stats():
    return {s["name"]: int(s.get("value", 0)) for s in xapi("statsquery").get("stat", [])}


GEN = [0]


def spec(managed, commands=(), base=None, gen=None):
    GEN[0] = gen or GEN[0] + 1
    s = {"schema": "fleet.relayspec.v1", "relay": "ops-replica", "generation": GEN[0],
         "spec_sha256": hashlib.sha256(json.dumps(managed, sort_keys=True).encode()).hexdigest(),
         "issued_at": time.time(), "base_config_sha256": base or sha_file(CFG), "managed": managed, "commands": list(commands)}
    tmp = M + "/spec.json.tmp"
    json.dump(s, open(tmp, "w"))
    os.replace(tmp, M + "/spec.json")
    return s


def acks():
    p = M + "/acks.jsonl"
    return [json.loads(l) for l in open(p)] if os.path.exists(p) else []


def wait_ack(gen, timeout=90, cmd=None):
    t0 = time.time()
    while time.time() - t0 < timeout:
        for a in reversed(acks()):
            if a.get("generation") == gen and (cmd is None or any(c.get("id") == cmd for c in a.get("commands", []))):
                return a
        time.sleep(0.5)
    return None


def e2e(ports, entries=ENTRIES, udp=False):
    out = OUT + "/e2e-last.json"
    args = ["python3", E2E, "run", "--entries", entries, "--bin-dir", BIN, "--clients", "xray", "--tries", "2", "--json", out]
    for p in ports:
        args += ["--port", str(p)]
    if udp:
        args.append("--udp")
    subprocess.run(args, capture_output=True, text=True, timeout=300)
    res = json.load(open(out))["results"]
    return {(r["port"], r["check"]): r["ok"] for r in res}


def managed_now():
    return json.load(open(M + "/observed.json"))["managed"]


def main():
    ents = json.load(open(ENTRIES))["entries"]
    ports = [e["port"] for e in ents]
    say("基线 e2e（全部入口，xray 客户端，TCP）")
    base = e2e(ports)
    ok(all(base.values()), "副本基线：%d/%d 个入口通过" % (sum(base.values()), len(base)), baseline=str(base))
    say("等 relayd 上报观测（spec 尚未启用 → 404，只观测）")
    t0 = time.time()
    while not os.path.exists(M + "/observed.json") and time.time() - t0 < 60:
        time.sleep(0.5)
    ok(os.path.exists(M + "/observed.json"), "relayd 上报了受管对象观测（fleet.relayobs.v1）")
    obs = json.load(open(M + "/observed.json"))
    ok(obs["config_sha256"] == sha_file(CFG), "观测里的 config_sha256 = 配置文件 sha256")
    m1 = obs["managed"]
    ok(len(m1["inbounds"]) == 12 and all(r["ruleTag"].startswith("fleet-") for r in m1["routing_rules"]),
       "受管集合：%d 入站 / %d 出站 / %d 规则（只含 fleet-*/policy-*）" % (len(m1["inbounds"]), len(m1["outbounds"]), len(m1["routing_rules"])))

    # 1. 收编（no-op）
    sha0 = sha_file(CFG)
    s = spec(m1)
    a = wait_ack(s["generation"])
    ok(a and a["status"] == "applied" and a.get("diff", {}).get("noop") and sha_file(CFG) == sha0, "gen1 收编：applied，无变化，文件未改写", ack=a)

    # 2. 停用 36003
    rt0 = runtime()
    m2 = copy.deepcopy(m1)
    m2["inbounds"] = [i for i in m2["inbounds"] if i["tag"] != "inbound-36003"]
    s = spec(m2)
    a = wait_ack(s["generation"])
    d = (a or {}).get("diff", {})
    ok(a and a["status"] == "applied", "gen2 停用 inbound-36003：applied", ack=a)
    ok(d.get("inbounds", {}).get("removed") == ["inbound-36003"] and d["inbounds"]["changed"] == [] and d["inbounds"]["added"] == []
       and d["unmanaged_inbounds_identical"] and d["unmanaged_outbounds_identical"] and d["unmanaged_rules_identical"] and d["r1_default_outbound_identical"],
       "差异只有 inbound-36003 被移除，其他 %s 个入站逐字相同，R1 默认出站与非受管规则不变" % d.get("inbounds", {}).get("identical"), diff=d)
    rt = runtime()
    ok("inbound-36003" not in rt["inbounds"] and set(rt0["inbounds"]) - set(rt["inbounds"]) == {"inbound-36003"} and rt["rules"] == rt0["rules"],
       "运行态：只少了 inbound-36003，规则顺序不变")
    ok(all(i["tag"] != "inbound-36003" for i in cfg()["inbounds"]), "磁盘配置：inbound-36003 已移除")
    r = e2e([36002, 36003, 36004])
    ok(not r[(36003, "tcp")] and r[(36002, "tcp")] and r[(36004, "tcp")], "e2e：36003 不通（已停用），相邻 36002/36004 照常", e2e=str(r))

    # 3. 重新启用（参数原样）
    s = spec(m1)
    a = wait_ack(s["generation"])
    d = (a or {}).get("diff", {})
    ok(a and a["status"] == "applied" and d.get("inbounds", {}).get("added") == ["inbound-36003"], "gen3 启用 inbound-36003：applied，差异只有它被加回", diff=d)
    back = next(i for i in cfg()["inbounds"] if i["tag"] == "inbound-36003")
    orig = next(i for i in m1["inbounds"] if i["tag"] == "inbound-36003")
    ok(back == orig, "重新启用的入站参数与停用前逐字相同（订阅不变）")
    r = e2e([36003])
    ok(r[(36003, "tcp")], "e2e：36003 恢复", e2e=str(r))

    # 4. 轮换 36004 的 UUID
    new_id = str(uuid.uuid4())
    m4 = copy.deepcopy(m1)
    old_id = None
    for i in m4["inbounds"]:
        if i["tag"] == "inbound-36004":
            old_id = i["settings"]["clients"][0]["id"]
            i["settings"]["clients"][0]["id"] = new_id
    s = spec(m4)
    a = wait_ack(s["generation"])
    d = (a or {}).get("diff", {})
    ok(a and a["status"] == "applied" and d.get("inbounds", {}).get("changed") == ["inbound-36004"] and not d["inbounds"]["removed"] and not d["inbounds"]["added"],
       "gen4 轮换 inbound-36004 UUID：applied，差异只有这一个入站", diff=d, results=(a or {}).get("results"))
    r_old = e2e([36004])
    e_new = json.load(open(ENTRIES))
    for e in e_new["entries"]:
        if e["port"] == 36004:
            e["uuid"] = new_id
    json.dump(e_new, open(OUT + "/entries-rotated.json", "w"))
    r_new = e2e([36004, 36005], entries=OUT + "/entries-rotated.json")
    ok(not r_old[(36004, "tcp")] and r_new[(36004, "tcp")] and r_new[(36005, "tcp")], "e2e：旧 UUID 被拒，新 UUID 通过，36005 不受影响")

    # 5. 重置 36002 的流量计数
    e2e([36002])
    time.sleep(3)
    st0 = stats()
    before_dn = st0.get("inbound>>>inbound-36002>>>traffic>>>downlink", 0)
    cid = "cmd-reset-" + uuid.uuid4().hex[:8]
    s = spec(m4, commands=[{"id": cid, "kind": "reset_traffic", "args": {"tag": "inbound-36002", "users": True}, "expires_at": time.time() + 300}], gen=GEN[0])
    a = wait_ack(s["generation"], cmd=cid)
    c = next((x for x in (a or {}).get("commands", []) if x["id"] == cid), {})
    cnt = {x["name"]: x for x in (c.get("data") or {}).get("counters", [])}
    dn = cnt.get("inbound>>>inbound-36002>>>traffic>>>downlink", {})
    ok(c.get("ok") and dn.get("value_at_reset", 0) >= before_dn > 0, "reset_traffic：ok，清零前 downlink=%s（≥ 测试前读到的 %d）" % (dn.get("value_at_reset"), before_dn), command=c)
    st1 = stats()
    others_same = all(st1.get(k, 0) >= v for k, v in st0.items() if "36002" not in k and "node-26" not in k and ">>>api>>>" not in k)
    ok(st1.get("inbound>>>inbound-36002>>>traffic>>>downlink", 0) < before_dn and others_same, "Xray 里只有 36002（及其用户）的计数被清零，其他计数未回退")
    xs = json.load(open(OUT + "/state/xray.json"))
    ok(xs["cum"].get("inbound>>>inbound-36002>>>traffic>>>downlink", 0) >= dn.get("value_at_reset", 1), "relayd 累计值未清零（cum=%s ≥ 清零前的值）" % xs["cum"].get("inbound>>>inbound-36002>>>traffic>>>downlink"))

    # 6. 访问规则：36002 白名单只放 cloudflare.com，36005 黑名单拒 cloudflare.com
    route = lambda t: next(r["outboundTag"] for r in cfg()["routing"]["rules"] if r.get("inboundTag") == [t] and "ruleTag" not in r)
    m6 = copy.deepcopy(m4)
    m6["routing_rules"] = [
        {"type": "field", "inboundTag": ["inbound-36002"], "ruleTag": "policy-u36002-match", "domain": ["full:www.cloudflare.com"], "outboundTag": route("inbound-36002")},
        {"type": "field", "inboundTag": ["inbound-36002"], "ruleTag": "policy-u36002-default", "outboundTag": "blocked"},
        {"type": "field", "inboundTag": ["inbound-36005"], "ruleTag": "policy-u36005-match", "domain": ["domain:cloudflare.com"], "outboundTag": "blocked"},
    ] + m6["routing_rules"]
    s = spec(m6)
    a = wait_ack(s["generation"])
    d = (a or {}).get("diff", {})
    ok(a and a["status"] == "applied" and sorted(d.get("policy_rules", {}).get("added", [])) == ["policy-u36002-default", "policy-u36002-match", "policy-u36005-match"]
       and d["inbounds"]["changed"] == [] and d["unmanaged_rules_identical"] and d["explicit_rules"] == {"added": [], "removed": [], "changed": []},
       "gen5 访问规则：applied，只新增 3 条 policy-* 规则，入站、非受管规则、fleet-explicit 直连规则都不变", diff=d)
    rules = cfg()["routing"]["rules"]
    tags = [r.get("ruleTag") or r.get("outboundTag") for r in rules]
    first_entry = next(i for i, r in enumerate(rules) if any(t.startswith("inbound-") for t in r.get("inboundTag", [])) and not r.get("ruleTag", "").startswith("policy-"))
    ok(tags.index("policy-u36002-match") < first_entry and tags[-1].startswith("fleet-explicit-") and cfg()["outbounds"][0]["tag"] == "fleet-default-deny",
       "规则位置：policy-* 在全局保护之后、各入口转发之前；fleet-explicit 仍在末尾；outbounds[0] 仍是 fleet-default-deny")
    ok(runtime()["rules"] == [r.get("outboundTag") for r in rules], "运行态 lsrules 的出站顺序与配置一致")
    r = e2e([36002, 36005, 36006], entries=OUT + "/entries-rotated.json", udp=True)
    ok(r[(36002, "tcp")] and not r[(36002, "udp")] and not r[(36005, "tcp")] and r[(36006, "tcp")],
       "e2e：36002 白名单（只放 www.cloudflare.com）内的 trace 通、stun.cloudflare.com（名单外）被拒；36005 黑名单拒 cloudflare；36006 不受影响", e2e=str(r))

    # 7. 守卫：去掉 36001 的直连显式规则（会让它落进 R1 blackhole）→ 必须拒绝
    sha_b, rt_b = sha_file(CFG), runtime()
    m7 = copy.deepcopy(m6)
    m7["routing_rules"] = [x for x in m7["routing_rules"] if x["ruleTag"] != "fleet-explicit-inbound-36001"]
    s = spec(m7)
    a = wait_ack(s["generation"])
    ok(a and a["status"] == "rejected" and "guard" in (a.get("error") or "") and sha_file(CFG) == sha_b and runtime() == rt_b,
       "gen6 去掉直连显式规则：rejected（guard），配置与运行态均未改变", ack_error=(a or {}).get("error"))
    r = e2e([36001], entries=OUT + "/entries-rotated.json")
    ok(r[(36001, "tcp")], "e2e：36001（直连）仍通")

    # 8. 运行态失败 → 撤回：把 36006 的端口换成已被占用的 25920（-test 不查端口，adi 会失败）
    m8 = copy.deepcopy(m6)
    for i in m8["inbounds"]:
        if i["tag"] == "inbound-36006":
            i["port"] = 25920
    s = spec(m8)
    a = wait_ack(s["generation"])
    rt = runtime()
    ok(a and a["status"] == "rejected" and a.get("runtime", {}).get("rolled_back_to_before") and sha_file(CFG) == sha_b and rt == rt_b,
       "gen7 运行态应用失败（端口被占）：rejected，已撤回，运行态与配置回到应用前", ack_error=(a or {}).get("error"), results=(a or {}).get("results"))
    r = e2e([36006], entries=OUT + "/entries-rotated.json")
    ok(r[(36006, "tcp")], "e2e：36006 恢复可用")

    # 9. base_changed：外部写入者（同一把锁下）改了文件
    stale = sha_file(CFG)
    raw = open(CFG, "rb").read()
    c = json.loads(raw)
    c["inbounds"][-1].setdefault("sniffing", {})["enabled"] = not c["inbounds"][-1].get("sniffing", {}).get("enabled", False)
    subprocess.run(["flock", "-w", "10", OUT + "/lock", "python3", "-c", "import json,sys; json.dump(json.loads(sys.argv[1]), open(sys.argv[2],'w'), indent=2)", json.dumps(c), CFG], check=True)
    m9 = copy.deepcopy(m6)
    for i in m9["inbounds"]:
        if i["tag"] == "inbound-36007":
            i.setdefault("sniffing", {})["routeOnly"] = True
    s = spec(m9, base=stale)
    a = wait_ack(s["generation"])
    ok(a and a["status"] == "rejected" and a.get("error") == "base_changed", "gen8 基于旧文件的 spec：rejected base_changed", ack_error=(a or {}).get("error"))
    subprocess.run(["flock", "-w", "10", OUT + "/lock", "sh", "-c", "cat > " + CFG], input=raw, check=True)
    time.sleep(1)

    # 10. restart_xray（带当前期望），之后所有入口照常
    cid = "cmd-restart-" + uuid.uuid4().hex[:8]
    s = spec(m6, commands=[{"id": cid, "kind": "restart_xray", "args": {}, "expires_at": time.time() + 300}])
    a = wait_ack(s["generation"], cmd=cid, timeout=150)
    c = next((x for x in (a or {}).get("commands", []) if x["id"] == cid), {})
    ok(a and a["status"] == "applied" and c.get("ok"), "gen9 restart_xray：ok（%s ms 后端口/API/规则就绪）" % (c.get("data") or {}).get("restart_ms"), command=c)
    r = e2e([p for p in ports if p != 36005], entries=OUT + "/entries-rotated.json")
    ok(all(r.values()), "重启后 e2e：%d/%d 通过" % (sum(r.values()), len(r)), e2e=str(r))

    # 11. 恢复到最初（去掉策略、UUID 换回）
    s = spec(m1)
    a = wait_ack(s["generation"])
    ok(a and a["status"] == "applied", "gen10 恢复初始期望：applied", diff=(a or {}).get("diff"))
    fin = e2e(ports)
    ok(fin == base, "最终 e2e 与基线逐项一致（%d/%d）" % (sum(fin.values()), len(fin)))
    norm = lambda m: {**m, "inbounds": sorted(m["inbounds"], key=lambda i: i["tag"])}
    ok(norm(managed_of_file()) == norm(m1), "磁盘上的受管集合与最初逐字相同（入站顺序除外：重新启用的入站排在末尾）")

    # 12. 访问日志：非回环来源的行（模拟客户）→ fleet.access.v1 事件
    log = cfg()["log"]["access"]
    lines = ["%s from 198.51.100.%d:5%03d accepted tcp:e2e-%d.example.com:443 [inbound-36002 -> %s] email: node-26\n" % (time.strftime("%Y/%m/%d %H:%M:%S"), n, n, n, route("inbound-36002")) for n in (7, 8)]
    lines.append("%s from 198.51.100.9:5009 accepted tcp:blocked.example.com:443 [inbound-36005 -> blocked] email: node-29\n" % time.strftime("%Y/%m/%d %H:%M:%S"))
    with open(log, "a") as f:
        f.writelines(lines)
    t0 = time.time()
    got = []
    while time.time() - t0 < 30 and len(got) < 3:
        got = [e for l in open(M + "/batches.jsonl") for e in ((json.loads(l).get("access") or {}).get("events") or []) if e["host"].endswith("example.com")]
        time.sleep(1)
    ok(len(got) == 3 and got[2]["action"] == "blocked" and got[0]["client"] == "node-26" and got[0]["source_port"] == 5007,
       "访问日志：3 条模拟客户事件按 fleet.access.v1 上报（含 blocked、client、source_port）", events=got)
    acc = [json.loads(l).get("access") for l in open(M + "/batches.jsonl")]
    acc = [x for x in acc if x]
    ok(acc and acc[-1]["counters"]["loopback"] > 0 and acc[-1]["counters"]["dropped_lines"] == 0,
       "e2e 的本机流量按旧口径记为 loopback（%d 行）不上报；dropped_lines=0" % acc[-1]["counters"]["loopback"])
    REPORT["pass"], REPORT["fail"] = PASS, FAIL
    json.dump(REPORT, open(OUT + "/report.json", "w"), ensure_ascii=False, indent=1)
    say("结果：%d 通过，%d 失败" % (PASS, FAIL))
    return 0 if FAIL == 0 else 1


def managed_of_file():
    c = cfg()
    m = {"inbounds": [i for i in c["inbounds"] if i["tag"].startswith("inbound-")],
         "outbounds": [o for o in c["outbounds"] if o["tag"].split("-")[0] in ("ss", "relay", "socks")],
         "routing_rules": [r for r in c["routing"]["rules"] if r.get("ruleTag", "").startswith(("fleet-", "policy-"))]}
    return m


if __name__ == "__main__":
    sys.exit(main())
