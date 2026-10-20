#!/usr/bin/env python3
"""verify_stage.py — 在 a395 宿主侧核对 fleetd 测试服。

  verify_stage.py equal      /v2/api/legacy/* 与旧后端副本逐字节比较（测试服用 pinned 时钟才有意义）
  verify_stage.py sse        SSE：hello、首帧延迟、Last-Event-ID 补发、心跳
  verify_stage.py auth       401 / 会话缓存 / CSRF
  verify_stage.py scenario   注入故障 → 告警 → 影子通知 → 确认 → 恢复（keepalive 时钟）
输出 JSON 摘要。"""
import json
import os
import socket
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = os.path.expanduser("~/team-ops-eval/fleet-stage")
TOKEN = open(os.path.join(BASE, "session.token")).read().strip()
F = "http://127.0.0.1:18396"
L = "http://127.0.0.1:18397"


def get(url, cookie=True, headers=None, method="GET", body=None, timeout=60):
    h = dict(headers or {})
    if cookie:
        h["Cookie"] = "session_token=" + (TOKEN if cookie is True else cookie)
    data = json.dumps(body).encode() if body is not None else None
    if data is not None:
        h["Content-Type"] = "application/json"
    req = urllib.request.Request(url, headers=h, method=method, data=data)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read(), dict(e.headers)


def jget(url, **kw):
    st, b, _ = get(url, **kw)
    return st, json.loads(b) if b else None


def num(v):
    if isinstance(v, bool) or v is None:
        return v
    if isinstance(v, (int, float)):
        return float(v)
    if isinstance(v, list):
        return [num(x) for x in v]
    if isinstance(v, dict):
        return {k: num(x) for k, x in v.items()}
    return v


def paths():
    _, ov = jget(L + "/api/team-ops/overview", cookie=False)
    assets = ov["data"]["assets"]
    _, jobs = jget(L + "/api/team-ops/jobs?limit=50", cookie=False)
    p = ["/overview", "/health", "/history?hours=24", "/incidents", "/renewals", "/audit", "/jobs?limit=50", "/behavior",
         "/activity?limit=500", "/panel/overview", "/provision/defaults", "/fleet", "/public-status", "/policies", "/export",
         "/schema", "/api-keys", "/api-keys/usage", "/komari/hosts", "/komari/clients", "/komari/pings", "/incidents?status=open"]
    for a in assets:
        p.append("/history?asset=%s&hours=24" % a["uuid"])
    for j in (jobs["data"].get("jobs") or [])[:25]:
        p.append("/jobs/" + j["id"])
    sample = [a for a in assets if a.get("host_measured")][:2] + [a for a in assets if a["team"] == "线路机"][:1]
    for a in sample:
        u = a["uuid"]
        p += ["/node?asset=" + u, "/behavior?asset=%s&seconds=1209600" % u, "/activity?asset=%s&limit=10&seconds=1209600" % u,
              "/komari/hosts/%s/metrics?range=4h" % u, "/komari/hosts/%s/metrics?range=1d" % u]
        for k in ("tcp_task_id", "egress_task_id"):
            if a.get(k):
                p.append("/komari/pings/%s/results?hours=24&uuid=%s" % (a[k], u))
    p += ["/komari/hosts/x/metrics?range=2h", "/jobs/does-not-exist", "/node?asset=nope"]
    return p


def cmd_equal():
    res = {"paths": 0, "byte_equal": 0, "json_equal": 0, "status_equal": 0, "mismatch": [], "bytes_total": 0}
    for p in paths():
        st1, b1, h1 = get(F + "/v2/api/legacy" + p)
        st2, b2, _ = get(L + "/api/team-ops" + p, cookie=False)
        res["paths"] += 1
        res["bytes_total"] += len(b2)
        if st1 == st2:
            res["status_equal"] += 1
        if st1 == st2 and b1 == b2:
            res["byte_equal"] += 1
            res["json_equal"] += 1
            continue
        try:
            same = st1 == st2 and num(json.loads(b1)) == num(json.loads(b2))
        except Exception:
            same = False
        if same:
            res["json_equal"] += 1
        else:
            res["mismatch"].append({"path": p, "fleetd": [st1, b1[:160].decode("utf-8", "replace")], "legacy": [st2, b2[:160].decode("utf-8", "replace")]})
    print(json.dumps(res, ensure_ascii=False, indent=1))


def sse_read(sock, until, deadline):
    buf = b""
    sock.settimeout(1)
    while until not in buf and time.time() < deadline:
        try:
            chunk = sock.recv(65536)
            if not chunk:
                break
            buf += chunk
        except socket.timeout:
            pass
    return buf.decode("utf-8", "replace")


def sse_open(last_id=None):
    s = socket.create_connection(("127.0.0.1", 18396))
    h = "GET /v2/api/stream HTTP/1.1\r\nHost: x\r\nCookie: session_token=%s\r\nAccept: text/event-stream\r\n" % TOKEN
    if last_id:
        h += "Last-Event-ID: %s\r\n" % last_id
    s.sendall((h + "\r\n").encode())
    return s


def cmd_sse():
    out = {}
    t0 = time.time()
    s = sse_open()
    txt = sse_read(s, b"event: change", time.time() + 30)
    out["first_change_after_s"] = round(time.time() - t0, 2)
    out["hello"] = "event: hello" in txt
    txt += sse_read(s, b"event: heartbeat", time.time() + 20)
    out["heartbeat_within_s"] = round(time.time() - t0, 2)
    ids = [l[4:] for l in txt.splitlines() if l.startswith("id: ")]
    res = [json.loads(l[6:])["resource"] for l in txt.splitlines() if l.startswith("data: {\"resource\"")]
    out["events"] = len(ids)
    out["resources"] = sorted(set(res))
    s.close()
    time.sleep(1)
    s2 = sse_open(ids[0])
    t2 = sse_read(s2, b"event: change", time.time() + 10)
    replay = [l[4:] for l in t2.splitlines() if l.startswith("id: ")]
    out["replay_from"] = ids[0]
    out["replayed_ids"] = replay[:5]
    out["replay_ok"] = bool(replay) and ids[0] not in replay
    s2.close()
    s3 = sse_open("1-1")
    out["stale_id_reset"] = "event: reset" in sse_read(s3, b"event: reset", time.time() + 5)
    s3.close()
    _, sysj = jget(F + "/v2/api/v1/system")
    out["cadence_after_disconnect"] = sysj["data"]["cadence"]
    print(json.dumps(out, ensure_ascii=False, indent=1))


def cmd_auth():
    out = {}
    out["no_cookie"] = get(F + "/v2/api/legacy/overview", cookie=False)[0]
    out["bad_cookie"] = get(F + "/v2/api/legacy/overview", cookie="nope")[0]
    out["good_cookie"] = get(F + "/v2/api/legacy/overview")[0]
    out["post_legacy"] = get(F + "/v2/api/legacy/assets/edit", method="POST", body={}, headers={"X-Fleet-Request": "1"})[0]
    out["post_without_header"] = get(F + "/v2/api/v1/silences", method="POST", body={"matchers": {"node": "t1-01"}, "duration_minutes": 5})[0]
    out["post_cross_site"] = get(F + "/v2/api/v1/silences", method="POST", body={"matchers": {"node": "t1-01"}, "duration_minutes": 5},
                                 headers={"X-Fleet-Request": "1", "Sec-Fetch-Site": "cross-site"})[0]
    _, before = jget(L + "/__fleet/status", cookie=False)
    for _ in range(20):
        get(F + "/v2/api/legacy/health")
    _, after = jget(L + "/__fleet/status", cookie=False)
    out["komari_me_calls_for_20_requests"] = after["counts"]["me"] - before["counts"]["me"]
    out["legacy_calls_for_20_requests"] = after["counts"]["legacy"] - before["counts"]["legacy"]
    print(json.dumps(out, ensure_ascii=False, indent=1))


def inject(spec):
    return jget(L + "/__fleet/inject", cookie=False, method="POST", body=spec)


def alerts(state="open"):
    _, j = jget(F + "/v2/api/v1/alerts?state=" + state)
    return j["data"]["alerts"]


def wait_for(pred, timeout, step=5):
    end = time.time() + timeout
    while time.time() < end:
        v = pred()
        if v:
            return v
        time.sleep(step)
    return None


def cmd_scenario():
    out = {"steps": []}
    t0 = time.time()

    def note(**kw):
        kw["t"] = round(time.time() - t0, 1)
        out["steps"].append(kw)
        print(json.dumps(kw, ensure_ascii=False), flush=True)

    inject({"op": "clear"})
    # 1) 团队1-31 认证失败（真实 Monitor 判定）→ critical，影子通知
    inject({"op": "node_fail", "node": "t1-31", "name": "t1-31"})
    a = wait_for(lambda: [x for x in alerts() if x["rule"] == "node_health_fail" and x["labels"].get("node") == "t1-31" and x["state"] == "firing"], 120)
    note(step="node_fail t1-31", alert=a and {k: a[0][k] for k in ("id", "severity", "summary", "group_key")})
    n = wait_for(lambda: [x for x in jget(F + "/v2/api/v1/notify-log?limit=20")[1]["data"]["notifications"] if "团队1-31" in x["body"] and x["status"] == "shadow"], 60, 2)
    note(step="shadow notification t1-31", notify=n and {k: n[0][k] for k in ("id", "status", "title", "severity")}, delay_after_fire_s=n and round(n[0]["ts"] - a[0]["fired_at"], 1))
    # 2) 出口 IP 不一致
    inject({"op": "egress_mismatch", "node": "t1-05", "name": "t1-05"})
    a = wait_for(lambda: [x for x in alerts() if x["rule"] == "egress_mismatch"], 90)
    note(step="egress_mismatch t1-05", alert=a and {k: a[0][k] for k in ("id", "severity", "summary")})
    # 3) 同一线路机名下 3 个节点同时故障 → 关联故障（一条通知）
    _, ov = jget(L + "/api/team-ops/overview", cookie=False)
    names = [a["name"] for a in ov["data"]["assets"] if a.get("relay") == "线路机1" and a.get("probe")][:3]
    shorts = []
    for nm in names:
        head = nm.split(" · ")[0]
        s = "t" + head[2:] if head.startswith("团队") else head
        shorts.append(s)
        inject({"op": "node_fail", "node": s, "name": s})
    a = wait_for(lambda: [x for x in alerts() if x["rule"] == "correlated_node_failure"], 120)
    note(step="correlated 线路机1 " + ",".join(shorts), alert=a and {k: a[0][k] for k in ("id", "summary")})
    time.sleep(15)
    members = [x for x in alerts() if x["labels"].get("relay") == "线路机1" and x["rule"] == "node_health_fail"]
    note(step="members aggregated", members=[(m["labels"]["node"], m["suppressed"]) for m in members])
    # 4) 线路机3 宕机（管理通道 + 主机探针）→ relay_down，抑制它名下节点（t1-31 在线路机3）
    inject({"op": "relay_down", "relay": "线路机3", "name": "r3"})
    a = wait_for(lambda: [x for x in alerts() if x["rule"] == "relay_down" and x["state"] == "firing"], 180)
    note(step="relay_down 线路机3", alert=a and {k: a[0][k] for k in ("id", "severity", "summary")})
    time.sleep(12)
    t131 = [x for x in alerts() if x["labels"].get("node") == "t1-31"]
    note(step="t1-31 under relay_down", alerts=[(x["rule"], x["state"], x["suppressed"]) for x in t131])
    _, pl = jget(F + "/v2/api/v1/probes/latest")
    n = pl["data"]["nodes"].get("t1-31", {})
    note(step="probe history t1-31", **{k: [(r["status"], r["ms"]) for r in n.get(k, {}).get("recent", [])][-6:] for k in ("tcp", "egress", "path")})
    # 5) 线路机2 出站配额 85%
    inject({"op": "quota", "relay": "线路机2", "bytes": 1.71e12, "name": "q"})
    a = wait_for(lambda: [x for x in alerts() if x["rule"] == "quota"], 90)
    note(step="quota 线路机2", alert=a and {k: a[0][k] for k in ("id", "severity", "summary")})
    # 6) 确认 + 维护窗口（写操作，带 CSRF 头）
    crit = [x for x in alerts("firing") if x["severity"] == "critical" and x["rule"] == "egress_mismatch"]
    if crit:
        st, j = jget(F + "/v2/api/v1/alerts/%d/ack" % crit[0]["id"], method="POST", body={"note": "测试服确认"},
                     headers={"X-Fleet-Request": "1", "Sec-Fetch-Site": "same-origin"})
        note(step="ack", status=st, ack_by=j and j.get("data", {}).get("ack_by"))
    st, j = jget(F + "/v2/api/v1/maintenance", method="POST", body={"matchers": {"relay": "线路机2"}, "duration_minutes": 30, "comment": "测试"},
                 headers={"X-Fleet-Request": "1"})
    note(step="maintenance create", status=st, id=j and j.get("data", {}).get("id"))
    # 7) 全部恢复 → 恢复通知（group_interval 5 min 对齐）
    inject({"op": "clear"})
    ok = wait_for(lambda: not [x for x in alerts() if x["rule"] in ("node_health_fail", "relay_down", "egress_mismatch", "correlated_node_failure")], 180)
    note(step="cleared → resolved", resolved=bool(ok))
    res = wait_for(lambda: [x for x in jget(F + "/v2/api/v1/notify-log?limit=50")[1]["data"]["notifications"] if x["title"].startswith("已恢复")], 420, 10)
    note(step="resolution notifications", titles=[x["title"] for x in (res or [])][:6])
    _, nl = jget(F + "/v2/api/v1/notify-log?limit=200")
    out["notify_log"] = [{k: x[k] for k in ("id", "status", "kind", "severity", "title")} for x in nl["data"]["notifications"]]
    _, au = jget(F + "/v2/api/v1/audit")
    out["audit"] = [{k: x[k] for k in ("action", "actor", "target")} for x in au["data"]["audit"]][:10]
    out["audit_chain_ok"] = au["data"]["chain_ok"]
    print(json.dumps(out, ensure_ascii=False, indent=1))


if __name__ == "__main__":
    {"equal": cmd_equal, "sse": cmd_sse, "auth": cmd_auth, "scenario": cmd_scenario}[sys.argv[1]]()
