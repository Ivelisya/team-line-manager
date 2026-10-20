#!/usr/bin/env python3
"""原版首页（LuminaPlus 补丁版 review20260917v4）的本地复现服务器。

只用于提取设计 DNA 与截图：原样加载线上首页构建产物，接口全部用
prototype/data 里的脱敏快照伪造，不连接生产。

  python3 mock_server.py [port]      默认 127.0.0.1:5187
  http://127.0.0.1:5187/admin/home            父页面（仿原生壳）+ 首页 iframe
  http://127.0.0.1:5187/admin/home?theme=dark 深色
  http://127.0.0.1:5187/admin/home?view=compact|mini   视图（写 sessionStorage）
  http://127.0.0.1:5187/admin/home/instance/t1-21      节点详情
"""
import json, os, sys, time, math, re
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from urllib.parse import urlparse, parse_qs

ROOT = os.path.expanduser("~/Desktop/team-ops-v2")
HOME = os.path.join(ROOT, "upstream-snapshot/release/plugin/home")
FLAGS = os.path.join(ROOT, "upstream-snapshot/komari-admin-live/assets/flags")
DATA = os.path.join(ROOT, "prototype/data")
HERE = os.path.dirname(os.path.abspath(__file__))

snap = json.load(open(os.path.join(DATA, "snapshot.json")))
host24 = json.load(open(os.path.join(DATA, "series/host-24h.json")))
lat24 = json.load(open(os.path.join(DATA, "series/latency-24h.json")))
AS_OF = snap["as_of"]
def S():
    return time.time() - AS_OF        # 快照时刻平移到“现在”（每次请求重算）
CFG = {"slow": 0, "fail": "", "offline": set(), "health": {}, "cpu": {}, "due": {}}   # 状态演示开关，见 /__cfg
UNBOUND = {f"t1-{i:02d}" for i in list(range(21, 26)) + list(range(32, 37))} | {"r3"}


def now():
    return time.time()


def shift(ts):
    return None if ts is None else ts + S()


def iso(ts):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))


NODES = snap["nodes"]
BY_ID = {n["id"]: n for n in NODES}


def komari_node(n, index):
    h = n.get("host") or {}
    relay = n["team"] == "线路机"
    bw = n["bandwidth"]["text"]
    plan = n["billing"]["plan"]
    if relay:
        tags = f'{n["provider"]};线路机'
        remark = f'{n["provider"]} · {plan}'
    else:
        tags = f'{n["provider"]};家宽;{n["relay"]};连通性;{bw}'
        proto = (n.get("outbound") or {}).get("protocol") or "shadowsocks"
        remark = f'{n["relay"]}:{n["relay_port"]} → {proto} · {plan} · 仅监测连接，不采集主机负载'
    quota = (n.get("traffic") or {}).get("quota")
    return {
        "uuid": n["id"], "name": n["full_name"], "group": n["team"], "region": n["region"]["flag"],
        "hidden": False, "cpu_name": h.get("cpu_model") or "", "cpu_cores": h.get("cores") or 0,
        "arch": h.get("arch") or "", "virtualization": h.get("virtualization") or "",
        "os": h.get("os") or ("连通性探针" if "proxy" in n["capabilities"] else "Debian GNU/Linux 12 (bookworm)"),
        "kernel_version": h.get("kernel") or "", "gpu_name": "",
        "mem_total": h.get("mem_total") or 0, "swap_total": h.get("swap_total") or 0, "disk_total": h.get("disk_total") or 0,
        "weight": index, "price": float(n["billing"]["price"]), "billing_cycle": 30, "auto_renewal": False,
        "currency": n["billing"]["currency"], "expired_at": n["billing"]["expired_at"],
        "tags": tags, "public_remark": remark,
        "traffic_limit": quota["limit_bytes"] if quota else 0,
        "traffic_limit_type": (quota or {}).get("type") or "max",
        "ipv4": n["ip"] if n.get("ip_version") == "v4" or n.get("ip") else "", "ipv6": n.get("ipv6") or "",
        "created_at": iso(AS_OF - 30 * 86400), "updated_at": iso(now()),
    }


def latest(n):
    h = n.get("host")
    if n["id"] in CFG["offline"]:
        return {"online": False, "updated_at": int((now() - 600) * 1000)}
    if not h:
        return {"online": bool(n["agent"]["online"]), "updated_at": int(now() * 1000)}
    jitter = 1 + 0.04 * math.sin(now() / 7 + len(n["id"]))
    return {
        "online": True, "cpu": CFG["cpu"].get(n["id"], h["cpu_pct"]), "ram": h["mem_used"], "ram_total": h["mem_total"],
        "swap": h["swap_used"], "swap_total": h["swap_total"], "load": h["load1"], "load5": h["load5"], "load15": h["load15"],
        "disk": h["disk_used"], "disk_total": h["disk_total"],
        "net_out": h["net_tx_bps"] / 8 * jitter, "net_in": h["net_rx_bps"] / 8 * jitter,
        "net_total_up": h["net_total_tx"], "net_total_down": h["net_total_rx"],
        "uptime": h["uptime_s"] + int(S()), "process": h["processes"],
        "connections": h.get("tcp_conns") or 0, "connections_udp": h.get("udp_conns") or 0,
        "updated_at": int(now() * 1000),
    }


def asset(n):
    lat = n.get("latency") or {}
    rate = n.get("rate") or {}
    tr = n.get("traffic") or {}
    relay = n["team"] == "线路机"
    probe = None
    if not relay:
        probe = {"egress_ms": lat.get("egress_ms"), "egress_at": shift(lat.get("egress_at")), "egress_rc": 0, "egress_error": None,
                 "actual_ip": lat.get("egress_actual_ip"), "tcp_ms": lat.get("tcp_ms"), "tcp_at": shift(lat.get("tcp_at")),
                 "tcp_rc": 0, "tcp_error": None}
    cap = n["bandwidth"]
    a = {
        "uuid": n["id"], "name": n["full_name"], "team": n["team"], "relay": n["relay"], "provider": n["provider"],
        "plan": n["billing"]["plan"], "ip": n["ip"], "price_cents": n["billing"]["price_cents"],
        "due_date": CFG["due"].get(n["id"], n["billing"]["due_date"]), "billing_timezone": n["billing"]["billing_timezone"],
        "region": n["region"]["flag"],
        "capacity": {"bps": cap["capacity_bps"], "source": cap["source"]} if cap.get("capacity_bps") else None,
        "channel": {"rate_up": rate.get("up_bps", 0) / 8, "rate_down": rate.get("down_bps", 0) / 8, "fresh": True,
                    "stats_valid": True, "sample_at": now() - 3, "counter_reset": False, "sample_gap": False},
        "daily": tr.get("today") or {"up": 0, "down": 0}, "monthly": tr.get("month") or {"up": 0, "down": 0},
        "probe": probe, "health": {"status": CFG["health"].get(n["id"], n["status"]["code"]), "reason": None, "since": shift(n["status"].get("since"))},
        "host_measured": bool(n.get("host")), "tcp_task_id": (lat.get("strip_tcp") or {}).get("task_id"),
    }
    return a


def overview():
    assets = [asset(n) for n in NODES]
    return {"now": now(), "started_at": AS_OF - 8 * 86400 + S(), "assets": assets, "groups": {}, "relays": {},
            "direct": [], "monthly_cents": sum(a["price_cents"] for a in assets), "events_today": 0}


def bindings():
    out = {}
    for n in NODES:
        if n["id"] in UNBOUND:
            continue
        st = (n.get("latency") or {}).get("strip_tcp")
        tid = st["task_id"] if st else (900 + NODES.index(n))
        out[str(tid)] = [n["id"]]
    return out


def ping_tasks():
    tasks = []
    for n in NODES:
        eg = (n.get("latency") or {}).get("strip_egress")
        if eg:
            tasks.append({"id": eg["task_id"], "name": f'{n["name"]} 出口验证 · {n["relay"]}', "interval": 300, "loss": 0,
                          "clients": [n["id"]], "type": "http", "weight": eg["task_id"]})
    for tid, uuids in bindings().items():
        n = BY_ID[uuids[0]]
        tasks.append({"id": int(tid), "name": f'{n["name"]} TCP · {n["relay"]}', "interval": 30, "loss": 0,
                      "clients": uuids, "type": "tcp", "weight": int(tid)})
    return tasks


def public():
    return {
        "sitename": "团队线路管理", "description": "团队线路管理节点概览", "theme": "LuminaPlus",
        "allow_cors": False, "disable_password_login": False, "oauth_enable": False, "private_site": True,
        "record_enabled": True, "record_preserve_time": 720, "ping_record_preserve_time": 720, "metric_retention_days": 30,
        "custom_head": "", "custom_body": "",
        "theme_settings": {
            "fakePingForUnbound": False, "homeGroupOrder": ["团队1", "团队2", "线路机"],
            "homepagePingBindings": bindings(), "mobileNodeViewMode": "large",
            "showOverviewRatings": False, "showTrafficRating": False, "showBandwidthRating": False, "showAssetRating": False,
        },
    }


def ping_series(params):
    ids = params.get("entity_ids") or []
    tid = str((params.get("tags") or {}).get("task_id") or params.get("task_id") or "")
    keys = [k for k in (params.get("metric_keys") or []) if k.startswith("ping.")]
    hours = params.get("hours")
    long_range = (hours and hours > 1.5) or (not tid)
    series = []
    for uid in ids:
        n = BY_ID.get(uid)
        if not n:
            continue
        lat = n.get("latency") or {}
        for kind in ("tcp", "egress"):
            st = lat.get("strip_" + kind)
            if not st or (tid and str(st["task_id"]) != tid):
                continue
            pts_lat, pts_loss = [], []
            if long_range:   # 详情页：用 24h 5 分钟桶
                ln = (lat24["nodes"].get(uid) or {}).get(kind) or {}
                t0 = lat24["nodes"].get(uid, {}).get("t0", AS_OF - 86400)
                losses = ln.get("loss_pct") or []
                for k, v in enumerate(ln.get("avg_ms") or []):
                    if v is None:
                        continue
                    t = t0 + k * 300 + S()
                    pts_lat.append({"time": iso(t), "value": v, "count": 10})
                    pts_loss.append({"time": iso(t), "value": ((losses[k] if k < len(losses) else 0) or 0) / 100, "count": 10})
            else:
                for dt, ms, lost in zip(st["dt"], st["ms"], st["lost"]):
                    t = st["t_end"] + S() - dt
                    pts_lat.append({"time": iso(t), "value": None if lost else ms, "count": 1})
                    pts_loss.append({"time": iso(t), "value": 1 if lost else 0, "count": 1})
            for key in keys:
                series.append({"metric_key": key, "entity_id": uid, "tags": {"task_id": str(st["task_id"])},
                               "interval_seconds": st.get("interval_s", 30) if not long_range else 300,
                               "points": pts_lat if key == "ping.latency_ms" else pts_loss})
    t_end = now()
    return {"start": iso(t_end - 86400), "end": iso(t_end), "series": series, "count": len(series)}


LOAD_FIELDS = {
    "cpu.usage": ("cpu_pct", None), "memory.used": ("mem_pct", "mem_total"), "swap.used": ("swap_used", None),
    "load.average": ("load1", None), "disk.used": ("disk_pct", "disk_total"),
    "net.in.rate": ("net_rx_bps", 0.125), "net.out.rate": ("net_tx_bps", 0.125),
    "process.count": ("processes", None), "connections.tcp": ("tcp", None), "connections.udp": ("udp", None),
}


def metric_series(params):
    keys = params.get("metric_keys") or []
    if any(k.startswith("ping.") for k in keys):
        return ping_series(params)
    ids = params.get("entity_ids") or []
    series = []
    for uid in ids:
        n = BY_ID.get(uid)
        hs = host24["nodes"].get(uid)
        if not n or not hs or not n.get("host"):
            continue
        h = n["host"]
        for key in keys:
            pts = []
            if key in ("traffic.up", "traffic.down"):
                field = "net_tx_bps" if key == "traffic.up" else "net_rx_bps"
                for k, v in enumerate(hs.get(field) or []):
                    if v is not None:
                        pts.append({"time": iso(hs["t0"] + k * 300 + S()), "value": v / 8 * 300, "count": 1})
            elif key in ("net.total.up", "net.total.down"):
                tot = h["net_total_tx"] if key.endswith("up") else h["net_total_rx"]
                for k in range(288):
                    pts.append({"time": iso(hs["t0"] + k * 300 + S()), "value": tot - (287 - k) * 2e7, "count": 1})
            elif key in LOAD_FIELDS:
                field, scale = LOAD_FIELDS[key]
                for k, v in enumerate(hs.get(field) or []):
                    if v is None:
                        continue
                    if scale == "mem_total":
                        v = v / 100 * h["mem_total"]
                    elif scale == "disk_total":
                        v = v / 100 * h["disk_total"]
                    elif isinstance(scale, float):
                        v = v * scale
                    pts.append({"time": iso(hs["t0"] + k * 300 + S()), "value": v, "count": 1})
            if not pts:
                continue
            series.append({"metric_key": key, "entity_id": uid, "interval_seconds": 300, "points": pts})
    t = now()
    return {"start": iso(t - 86400), "end": iso(t), "series": series, "count": len(series)}


def rpc(payload):
    method, params, rid = payload.get("method"), payload.get("params") or {}, payload.get("id")
    ok = lambda r: {"jsonrpc": "2.0", "id": rid, "result": r}
    if method == "common:getNodes":
        return ok({n["id"]: komari_node(n, i) for i, n in enumerate(NODES)})
    if method == "common:getNodesLatestStatus":
        uu = params.get("uuids") or [n["id"] for n in NODES]
        return ok({u: latest(BY_ID[u]) for u in uu if u in BY_ID})
    if method == "public:getPublicPingTasks":
        return ok(ping_tasks())
    if method == "public:queryMetrics":
        return ok(metric_series(params))
    return {"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": f"Method not found: {method}"}}


PARENT = open(os.path.join(HERE, "parent.html"), encoding="utf-8").read()
TYPES = {".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml", ".woff2": "font/woff2", ".json": "application/json",
         ".html": "text/html; charset=utf-8", ".png": "image/png", ".ico": "image/x-icon", ".mp4": "video/mp4", ".webp": "image/webp"}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, code, body, ctype="application/json"):
        if isinstance(body, (dict, list)):
            body = json.dumps(body, ensure_ascii=False).encode()
        elif isinstance(body, str):
            body = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def file(self, path):
        if not os.path.isfile(path):
            return self.send(404, {"message": "not found"})
        self.send(200, open(path, "rb").read(), TYPES.get(os.path.splitext(path)[1], "application/octet-stream"))

    def gate(self, kind):
        if CFG["slow"]:
            time.sleep(CFG["slow"] / 1000)
        return kind in CFG["fail"].split(",") or CFG["fail"] == "all"

    def do_GET(self):
        u = urlparse(self.path)
        p = u.path
        if p == "/__cfg":
            q = parse_qs(u.query)
            CFG["slow"] = int(q.get("slow", ["0"])[0])
            CFG["fail"] = q.get("fail", [""])[0]
            CFG["offline"] = set(filter(None, q.get("offline", [""])[0].split(",")))
            pair = lambda key: dict(x.split(":", 1) for x in filter(None, q.get(key, [""])[0].split(",")))
            CFG["health"] = pair("health")
            CFG["cpu"] = {k: float(v) for k, v in pair("cpu").items()}
            CFG["due"] = pair("due")
            return self.send(200, {k: (sorted(v) if isinstance(v, set) else v) for k, v in CFG.items()})
        if p == "/" or p.startswith("/admin"):
            return self.send(200, PARENT, "text/html; charset=utf-8")
        pre = "/api/admin/plugin/team-ops/home/"
        if p.startswith(pre):
            rel = p[len(pre):] or "index.html"
            return self.file(os.path.join(HOME, rel))
        if p.startswith("/assets/flags/"):
            return self.file(os.path.join(FLAGS, os.path.basename(p)))
        if p.startswith("/images/"):
            return self.file(os.path.join(HOME, p.lstrip("/")))
        if p == "/api/me":
            return self.send(200, {"logged_in": True, "username": "admin", "uuid": "admin"})
        if p == "/api/public":
            return self.send(200, {"status": "success", "message": "", "data": public()})
        if p == "/api/team-ops/overview":
            if self.gate("overview"):
                return self.send(500, {"status": "error", "message": "mock failure"})
            return self.send(200, {"status": "success", "data": overview()})
        if p == "/api/admin/ping":
            return self.send(200, ping_tasks())
        if p == "/api/nodes":
            return self.send(200, [komari_node(n, i) for i, n in enumerate(NODES)])
        if p == "/api/rpc2":
            return self.send(400, {"message": "websocket not supported in mock"})
        return self.send(404, {"message": f"no mock for {p}"})

    def do_POST(self):
        u = urlparse(self.path)
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        if u.path == "/api/rpc2":
            if self.gate("rpc"):
                return self.send(500, {"message": "mock failure"})
            payload = json.loads(body or b"{}")
            if isinstance(payload, list):
                return self.send(200, [rpc(x) for x in payload])
            return self.send(200, rpc(payload))
        return self.send(404, {"message": "no mock"})


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 5187
    print(f"mock on http://127.0.0.1:{port}/admin/home  (shift {S():.0f}s)")
    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
