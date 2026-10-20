#!/usr/bin/env python3
"""a395 staging server for the refined team-ops UI (团队线路管理 v2).

Serves the production team-ops read API from a FRESH COPY of the operations / Komari / metrics databases,
by dispatching in-process into the real teamops backend code (teamops.server.Application.dispatch), plus
the v2 static build at the site root (/, /app, /shared, /original, /data, /live).

Staging rules (enforced here, not just promised):
  * read-only collection: collect_loop / monitor_loop / cache_loop are never started; Application.bridge,
    Provision.remote and provision.komari are replaced, so no SSH, no bridge, no Komari RPC pump;
  * nothing leaves the machine: by default the backend runs in its own user+network namespace
    (unshare -Urn) that has no interface but an unconfigured loopback; the host side only forwards
    127.0.0.1:<port> to a unix socket.  Production (central / line machines) is unreachable by design;
  * no notifications, no backups, no line changes: every non-GET request is refused with 403;
  * the source databases under ~/team-ops-eval/data are only ever read by `cp --reflink=auto`.

Clock.  The copy is a frozen moment (as_of = meta monitor.checked_at).  The backend runs on a virtual
clock that starts at as_of:
  pinned     time stands still at as_of (fixture capture; identical to what build.py sees)
  shifted    as_of + real elapsed time; data ages naturally (collection "stopped" at as_of)
  keepalive  (default) like shifted, but every 10 s the world is re-stamped: relay snapshot, probe, path,
             panel, Komari host and collector timestamps move forward by the elapsed time (their ages stay
             what they were at as_of) and the real Monitor.evaluate() runs on the copy.  Values stay the
             real values at as_of; only freshness moves.  Traffic counters do not grow.

Komari.  /komari/* goes through the real dispatch code; provision.komari is answered by KomariMirror
(read-only SQL over the komari.db / metrics.db copies on the same virtual clock; shapes verified
against the production Komari binary, see komari_mirror.py).

Usage (on a395, Python 3.12 runtime):
  stage_server.py serve   --static DIR [--port 18395] [--clock keepalive|shifted|pinned] [--now EPOCH]
                          [--no-isolate] [--auth-token TOKEN]
  stage_server.py capture --out DIR [--tick2 10]        # in-process fixture capture at as_of (pinned)
"""
import argparse
import contextlib
import datetime as dt
import json
import mimetypes
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import socket
import socketserver
import subprocess
import sys
import threading
import time as _time
import traceback
import types
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from zoneinfo import ZoneInfo

HOME = Path(os.path.expanduser("~/team-ops-eval"))
HERE = Path(__file__).resolve().parent
BJ = ZoneInfo("Asia/Shanghai")
DEFAULT_PORT = 18395
# Mirrors plugin-live/script.js allowedGet (+ the session route public-status).  Parsed from the release's
# plugin script at start-up when available, so the staging allowlist cannot drift from production.
ALLOWED_GET = ["overview", "activity", "history", "policies", "policies/preview", "audit", "renewals", "export", "health",
               "fleet", "node", "incidents", "behavior", "panel/overview", "schema", "api-keys", "api-keys/usage",
               "provision/defaults", "jobs", "jobs/:id", "schema.md", "komari/hosts", "komari/hosts/:uuid/metrics",
               "komari/pings", "komari/pings/:task_id/results", "komari/clients", "public-status"]
STAMP_KEYS = {"ts", "last_active"}
EPOCH_LO, EPOCH_HI = 1.6e9, 2.2e9


def log(*parts):
    sys.stdout.write(dt.datetime.now(BJ).strftime("%m-%d %H:%M:%S ") + " ".join(str(p) for p in parts) + "\n")
    sys.stdout.flush()


def load_allowlist(release):
    script = Path(release) / "plugin" / "script.js"
    try:
        m = re.search(r"const allowedGet = (\[.*?\]);", script.read_text())
        return json.loads(m[1]) + ["public-status"]
    except Exception:
        return list(ALLOWED_GET)


def route_patterns(allowed):
    out = []
    for p in allowed:
        rx = "^/" + re.sub(r":[a-z_]+", "[^/]+", re.escape(p).replace("\\:", ":")) + "$"
        out.append(re.compile(rx))
    return out


# ─── fresh copy ──────────────────────────────────────────────────────────────────────────────────────

def fresh_copy(target):
    """cp --reflink=auto of the three databases into `target` (wiped first)."""
    target = Path(target)
    if target.exists():
        shutil.rmtree(target)
    target.mkdir(parents=True, mode=0o700)
    for name in ("operations.db", "komari.db", "metrics.db"):
        src = HOME / "data" / name
        subprocess.run(["cp", "--reflink=auto", str(src), str(target / name)], check=True)
        os.chmod(target / name, 0o600)
    return target


def read_as_of(db):
    import sqlite3
    c = sqlite3.connect("file:%s?mode=ro" % db, uri=True)
    try:
        return float(json.loads(c.execute("SELECT value FROM meta WHERE key='monitor'").fetchone()[0])["checked_at"])
    finally:
        c.close()


# ─── virtual clock and the patched backend ───────────────────────────────────────────────────────────

class VClock:
    def __init__(self, base, mode):
        self.base, self.mode, self.t0 = float(base), mode, _time.monotonic()

    def __call__(self):
        return self.base if self.mode == "pinned" else self.base + (_time.monotonic() - self.t0)


class TimeShim(types.ModuleType):
    """Stands in for the `time` module inside teamops: time() is virtual, everything else is real."""
    def __init__(self, clock):
        super().__init__("time")
        self._clock = clock

    def time(self):
        return self._clock()

    def __getattr__(self, name):
        return getattr(_time, name)


def load_backend(db_dir, release, clock):
    sys.path.insert(0, str(release))
    import teamops.server as tserver
    import teamops.store as tstore
    shim = TimeShim(clock)
    for name in ("server", "store", "monitor", "behavior", "jobs", "apikeys", "fleet", "provision", "panel", "policy"):
        mod = sys.modules.get("teamops." + name) or __import__("teamops." + name, fromlist=["x"])
        if hasattr(mod, "time"):
            mod.time = shim

    def start_times():
        now = dt.datetime.fromtimestamp(clock(), BJ)
        day = now.replace(hour=0, minute=0, second=0, microsecond=0)
        return day.timestamp(), day.replace(day=1).timestamp()
    tstore.start_times = start_times

    db = Path(db_dir) / "operations.db"
    (Path(db_dir) / "inventory-empty.json").write_text("[]")
    # Provision.defaults() reads the provision public key next to the database; production's key is not
    # part of the copy, so say so instead of inventing one.
    (Path(db_dir) / "provision.key.pub").write_text("（测试服副本不含 provision 公钥）")
    import sqlite3
    c = sqlite3.connect(str(db))
    relay_names = [r[0] for r in c.execute("SELECT relay FROM relays")]
    fleet = {r[0]: json.loads(r[1]) for r in c.execute("SELECT relay, data FROM fleet")} if c.execute(
        "SELECT 1 FROM sqlite_master WHERE name='fleet'").fetchone() else {}
    c.close()
    # Relays not enrolled through the fleet table come from backend.json in production; the command is
    # never run here (bridge is disabled below).
    config = {"database": str(db), "inventory": str(Path(db_dir) / "inventory-empty.json"), "key": secrets.token_hex(16),
              "public_ip": "127.0.0.1",
              "relays": {name: {"command": ["/bin/false"]} for name in relay_names if not (fleet.get(name) or {}).get("active")}}
    app = tserver.Application(config)

    def no_bridge(*_a, **_k):
        raise RuntimeError("测试服不连接线路管理通道")
    app.bridge = no_bridge

    class NoRemote:
        def __getattr__(self, name):
            raise RuntimeError("测试服不执行远程命令")
    app.provision.remote = NoRemote()
    from komari_mirror import KomariMirror
    mirror = KomariMirror(str(Path(db_dir) / "komari.db"), str(Path(db_dir) / "metrics.db"), clock)
    app.provision.komari = mirror.call
    return app


# ─── keepalive: move freshness forward, keep values ──────────────────────────────────────────────────

def shift_stamps(obj, delta):
    if isinstance(obj, dict):
        for k, v in obj.items():
            if isinstance(v, (int, float)) and not isinstance(v, bool) and (k in STAMP_KEYS or k.endswith("_at")) and EPOCH_LO <= v <= EPOCH_HI:
                obj[k] = v + delta
            elif isinstance(v, (dict, list)):
                shift_stamps(v, delta)
    elif isinstance(obj, list):
        for v in obj:
            shift_stamps(v, delta)


ISO_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(\.\d+)?(Z|[+-]\d{2}:\d{2})$")


def shift_iso(value, delta):
    m = ISO_RE.match(str(value or ""))
    if not m:
        return value
    frac = m[2] or ""
    base = dt.datetime.fromisoformat(m[1] + (m[3].replace("Z", "+00:00")))
    t = base.timestamp() + (float(frac) if frac else 0) + delta
    out = dt.datetime.fromtimestamp(t, dt.timezone.utc)
    return out.strftime("%Y-%m-%dT%H:%M:%S") + ("." + ("%09d" % round((t % 1) * 1e9))[:max(1, len(frac) - 1)] if frac else "") + "Z"


def keepalive_step(app, clock, state):
    now = clock()
    delta = now - state["stamped_to"]
    store = app.store
    with store.lock, store.db:
        if delta > 0:
            for relay, ts, data in store.db.execute("SELECT relay, ts, data FROM relays").fetchall():
                d = json.loads(data)
                shift_stamps(d, delta)
                store.db.execute("UPDATE relays SET ts=?, data=? WHERE relay=?", ((ts or 0) + delta if ts else ts, json.dumps(d, ensure_ascii=False, separators=(",", ":")), relay))
            hosts = store.get_meta("hosts", {})
            for h in hosts.values():
                if isinstance(h, dict) and h.get("time"):
                    h["time"] = shift_iso(h["time"], delta)
            store.meta("hosts", hosts)
            store.meta("hosts_at", (store.get_meta("hosts_at", 0) or 0) + delta)
            coll = store.get_meta("collector", {})
            if coll.get("checked_at"):
                coll["checked_at"] += delta
                store.meta("collector", coll)
            state["stamped_to"] = now
    app.monitor.evaluate(now=now)
    app.store.behavior.detect()
    if now - state.get("behavior_at", 0) >= 30:
        app.store.behavior.refresh()
        state["behavior_at"] = now


def keepalive_loop(app, clock, stop):
    state = {"stamped_to": clock()}
    while not stop.wait(10):
        try:
            keepalive_step(app, clock, state)
        except Exception:
            log("keepalive failed", traceback.format_exc(limit=3))


# ─── HTTP ────────────────────────────────────────────────────────────────────────────────────────────

class StageState:
    app = None
    clock = None
    routes = []
    static = None
    auth_token = None
    writes = False
    info = {}


# --writes: the class (a) write routes (team-ops database only; LIVE-MODE.md §13) are dispatched on the DB copy,
# with the same request rules as the production plugin proxy (plugin-live/script.js: X-Ops-Request: 1 and
# Sec-Fetch-Site same-origin).  Everything else that is not GET stays 403.
WRITE_ROUTES = {"/assets/edit", "/node/profile", "/incidents/ack", "/behavior/ack"}


def reply(h, status, body, ctype="application/json; charset=utf-8", extra=None):
    data = body if isinstance(body, bytes) else (json.dumps(body, ensure_ascii=False, separators=(",", ":")).encode() if not isinstance(body, str) else body.encode())
    extra = dict(extra or {})
    h.send_response(status)
    h.send_header("Content-Type", ctype)
    h.send_header("Cache-Control", extra.pop("Cache-Control", "no-store"))
    h.send_header("Content-Length", str(len(data)))
    for k, v in extra.items():
        h.send_header(k, v)
    h.end_headers()
    if h.command != "HEAD":
        h.wfile.write(data)


# Same headers production Caddy sends for /admin/v2/* (keep in sync with deploy/prod/deploy.sh), so the
# staging smoke catches CSP violations before production does.
STATIC_CSP = ("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; "
              "font-src 'self' data:; connect-src 'self'; worker-src 'self' blob:; object-src 'none'; base-uri 'self'; "
              "form-action 'self'; frame-ancestors 'self'")
STATIC_HEADERS = {"Cache-Control": "no-cache", "X-Frame-Options": "SAMEORIGIN", "Content-Security-Policy": STATIC_CSP}


class Handler(BaseHTTPRequestHandler):
    server_version = "TeamOpsStage/1.0"
    protocol_version = "HTTP/1.1"

    def address_string(self):
        return "local"

    def log_message(self, fmt, *args):
        log(self.command, self.path[:160], *(a for a in args[1:2]))

    def do_GET(self):
        parts = urllib.parse.urlsplit(self.path)
        path = parts.path
        try:
            if path.startswith("/api/team-ops/"):
                return self.api(path[len("/api/team-ops"):], parts.query)
            # production (root layout, LIVE-MODE.md §19): the app is at /, its files at /app /shared /original /data /live;
            # the old /admin/v2/… and /admin… addresses 301 to / (the browser keeps the #/… fragment across the redirect)
            if path in ("/", "/index.html"):
                return self.static("index.html")
            if path.split("/")[1] in ("app", "shared", "original", "data", "live") and path != "/app/":
                return self.static(path[1:])
            if path == "/app/" or path == "/admin" or path.startswith("/admin/") or path.startswith("/admin?"):
                return reply(self, 301, b"", "text/plain", {"Location": "/"})
            if path == "/__stage/status":
                return reply(self, 200, {**StageState.info, "now": StageState.clock(), "now_text": dt.datetime.fromtimestamp(StageState.clock(), BJ).isoformat()})
            return reply(self, 404, {"error": "接口不存在"})
        except BrokenPipeError:
            pass

    do_HEAD = do_GET

    def refuse(self):
        reply(self, 403, {"error": "测试服只读：已拒绝写操作"})
        self.close_connection = True
    do_PUT = do_PATCH = do_DELETE = refuse

    def do_POST(self):
        path = urllib.parse.urlsplit(self.path).path
        sub = path[len("/api/team-ops"):] if path.startswith("/api/team-ops/") else None
        if not StageState.writes or sub not in WRITE_ROUTES:
            return self.refuse()
        if self.headers.get("X-Ops-Request") != "1" or self.headers.get("Sec-Fetch-Site", "same-origin") != "same-origin":
            return reply(self, 403, {"error": "请求被拒绝"})
        if StageState.auth_token:
            cookies = dict(c.strip().split("=", 1) for c in self.headers.get("Cookie", "").split(";") if "=" in c)
            if cookies.get("stage_session") != StageState.auth_token:
                return reply(self, 401, {"error": "请先登录管理员账户"})
        try:
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        except ValueError:
            return reply(self, 400, {"error": "请求格式错误"})
        log("WRITE", sub, json.dumps(body, ensure_ascii=False)[:300])
        status, out = dispatch(StageState.app, sub, body, method="POST")
        reply(self, status, out)

    def api(self, path, query):
        if StageState.auth_token:
            cookies = dict(c.strip().split("=", 1) for c in self.headers.get("Cookie", "").split(";") if "=" in c)
            if cookies.get("stage_session") != StageState.auth_token:
                return reply(self, 401, {"error": "请先登录管理员账户"})
        if not any(rx.match(path) for rx in StageState.routes):
            return reply(self, 404, {"error": "接口不存在"})
        params = {k: v[-1] for k, v in urllib.parse.parse_qs(query).items()}
        status, body = dispatch(StageState.app, path, params)
        if path == "/schema.md" and status == 200:
            return reply(self, 200, body["data"]["markdown"], "text/markdown; charset=utf-8")
        reply(self, status, body)

    def static(self, rel):
        root = StageState.static
        if not root:
            return reply(self, 404, {"error": "未配置静态目录"})
        target = (root / urllib.parse.unquote(rel)).resolve()
        if root not in target.parents and target != root:
            return reply(self, 404, {"error": "接口不存在"})
        if target.is_dir():
            if rel and not rel.endswith("/"):
                return reply(self, 404, {"error": "文件不存在"}, extra=STATIC_HEADERS)
            target = target / "index.html"
        if not target.is_file():
            return reply(self, 404, {"error": "文件不存在"}, extra=STATIC_HEADERS)
        ctype = mimetypes.guess_type(str(target))[0] or "application/octet-stream"
        if target.suffix in (".js", ".mjs"):
            ctype = "text/javascript"
        if ctype.startswith("text/") or ctype.endswith("json") or ctype.endswith("javascript"):
            ctype += "; charset=utf-8"
        reply(self, 200, target.read_bytes(), ctype, STATIC_HEADERS)


def dispatch(app, path, params, method="GET"):
    """Same status mapping as teamops.server.Handler.handle_request."""
    from teamops.provision import Conflict
    try:
        return 200, {"data": app.dispatch(method, path, params, "stage")}
    except Conflict:
        return 409, {"error": "已有任务在执行"}
    except (ValueError, TypeError) as e:
        return 400, {"error": str(e)}
    except KeyError:
        return 404, {"error": "接口不存在"}
    except Exception as e:
        log("request failed", path, type(e).__name__, e)
        return 500, {"error": "操作未完成，请查看系统状态后重试"}


class UnixHTTPServer(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True
    allow_reuse_address = True


def network_isolated():
    try:
        with socket.create_connection(("1.1.1.1", 443), timeout=1):
            return False
    except OSError:
        return True


# ─── serve: host-side forwarder + (isolated) backend child ───────────────────────────────────────────

def forwarder(port, sock_path, stop):
    class Pipe(socketserver.BaseRequestHandler):
        def handle(self):
            try:
                up = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                up.connect(sock_path)
            except OSError:
                return
            def pump(src, dst):
                try:
                    while True:
                        b = src.recv(65536)
                        if not b:
                            break
                        dst.sendall(b)
                except OSError:
                    pass
                finally:
                    with contextlib.suppress(OSError):
                        dst.shutdown(socket.SHUT_WR)
            t = threading.Thread(target=pump, args=(up, self.request), daemon=True)
            t.start()
            pump(self.request, up)
            t.join(60)
            up.close()

    class TCP(socketserver.ThreadingTCPServer):
        daemon_threads = True
        allow_reuse_address = True
    srv = TCP(("127.0.0.1", port), Pipe)
    threading.Thread(target=lambda: (stop.wait(), srv.shutdown()), daemon=True).start()
    log("forwarding 127.0.0.1:%d -> %s" % (port, sock_path))
    srv.serve_forever()


def run_backend(args, db_dir, sock_path=None):
    as_of = read_as_of(db_dir / "operations.db")
    clock = VClock(args.now if args.now else as_of, args.clock)
    app = load_backend(db_dir, args.release, clock)
    StageState.app, StageState.clock = app, clock
    StageState.routes = route_patterns(load_allowlist(args.release))
    StageState.static = Path(args.static).resolve() if args.static else None
    StageState.auth_token = args.auth_token or None
    StageState.writes = bool(args.writes)
    isolated = network_isolated()
    StageState.info = {"as_of": as_of, "clock": args.clock, "db_copy": str(db_dir), "network_isolated": isolated, "writes": bool(args.writes),
                       "started_at": _time.time(), "static": str(StageState.static)}
    log("backend ready as_of=%s clock=%s isolated=%s" % (dt.datetime.fromtimestamp(as_of, BJ).isoformat(), args.clock, isolated))
    stop = threading.Event()
    if args.clock == "keepalive":
        threading.Thread(target=keepalive_loop, args=(app, clock, stop), daemon=True).start()
    if sock_path:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(sock_path)
        srv = UnixHTTPServer(sock_path, Handler)
        os.chmod(sock_path, 0o600)
        ppid = os.getppid()
        def orphan_watch():
            while not stop.wait(2):
                if os.getppid() != ppid:
                    log("parent gone, stopping")
                    srv.shutdown()
                    return
        threading.Thread(target=orphan_watch, daemon=True).start()
    else:
        srv = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
        srv.daemon_threads = True
    signal.signal(signal.SIGTERM, lambda *_: threading.Thread(target=srv.shutdown, daemon=True).start())
    try:
        srv.serve_forever()
    finally:
        stop.set()


def cmd_serve(args):
    stage = HOME / "v2-stage"
    db_dir = Path(args.db_dir) if args.child else fresh_copy(stage / "db")
    if args.child:
        return run_backend(args, db_dir, args.sock)
    log("fresh copy in", db_dir)
    if args.no_isolate:
        return run_backend(args, db_dir)
    sock = str(stage / "stage.sock")
    with contextlib.suppress(FileNotFoundError):
        os.unlink(sock)
    child_cmd = ["unshare", "-Urn", sys.executable, str(Path(__file__).resolve()), "serve", "--child", "--db-dir", str(db_dir),
                 "--sock", sock, "--clock", args.clock, "--release", str(args.release)]
    if args.static:
        child_cmd += ["--static", str(args.static)]
    if args.now:
        child_cmd += ["--now", str(args.now)]
    if args.auth_token:
        child_cmd += ["--auth-token", args.auth_token]
    if args.writes:
        child_cmd += ["--writes"]
    child = subprocess.Popen(child_cmd)
    for _ in range(600):
        if os.path.exists(sock) or child.poll() is not None:
            break
        _time.sleep(0.1)
    if child.poll() is not None or not os.path.exists(sock):
        log("backend child failed to start")
        sys.exit(1)
    stop = threading.Event()
    def term(*_):
        stop.set()
        with contextlib.suppress(Exception):
            child.terminate()
    signal.signal(signal.SIGTERM, term)
    signal.signal(signal.SIGINT, term)
    threading.Thread(target=lambda: (child.wait(), stop.set()), daemon=True).start()
    try:
        forwarder(args.port, sock, stop)
    finally:
        with contextlib.suppress(Exception):
            child.terminate()
            child.wait(10)


# ─── capture: fixtures for deploy/v2/live/tests ──────────────────────────────────────────────────────

def cmd_capture(args):
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    db_dir = fresh_copy(HOME / "v2-stage" / "db-capture")
    as_of = read_as_of(db_dir / "operations.db")
    clock = VClock(as_of, "pinned")
    app = load_backend(db_dir, args.release, clock)
    index = {}

    def get(path, query=None, sub=""):
        q = urllib.parse.urlencode(sorted((query or {}).items()))
        params = {k: str(v) for k, v in (query or {}).items()}
        status, body = dispatch(app, path, params)
        name = (sub + path.strip("/").replace("/", "_") + ("__" + re.sub(r"[^A-Za-z0-9=_.-]+", "_", q) if q else ""))[:180] + ".json"
        (out / "api").mkdir(exist_ok=True)
        (out / "api" / name).write_text(json.dumps({"status": status, "body": body}, ensure_ascii=False, separators=(",", ":")))
        index[(sub + "|" if sub else "") + "/api/team-ops" + path + ("?" + q if q else "")] = "api/" + name
        return body.get("data") if status == 200 else None

    overview = get("/overview")
    health = get("/health")
    get("/history", {"hours": 24})
    # adapter trafficLane: 24 h per node + first hour of the month per node (hours = whole hours since BJ month start + 2)
    now_bj = dt.datetime.fromtimestamp(as_of, BJ)
    month0 = now_bj.replace(day=1, hour=0, minute=0, second=0, microsecond=0).timestamp()
    month_hours = min(840, int((as_of - month0) // 3600) + 2)
    for a in overview["assets"]:
        get("/history", {"asset": a["uuid"], "hours": 24})
        get("/history", {"asset": a["uuid"], "hours": month_hours})
    get("/incidents")
    get("/renewals")
    get("/audit")
    jobs = get("/jobs", {"limit": 50})
    for j in (jobs or {}).get("jobs", []):
        get("/jobs/" + j["id"])
    get("/behavior")
    get("/activity", {"limit": 500})
    get("/panel/overview")
    get("/provision/defaults")
    get("/fleet")
    get("/public-status")
    get("/policies")
    get("/komari/hosts")
    get("/komari/clients")
    get("/komari/pings")
    sample = [a for a in overview["assets"] if a.get("host_measured")][:2] + [a for a in overview["assets"] if a["team"] == "线路机"][:1]
    for a in sample:
        get("/node", {"asset": a["uuid"]})
        get("/behavior", {"asset": a["uuid"], "seconds": 1209600})
        get("/activity", {"asset": a["uuid"], "limit": 10, "seconds": 1209600})
        for rng in ("4h", "1d"):
            get("/komari/hosts/%s/metrics" % a["uuid"], {"range": rng})
        relay_uuid = next((x["uuid"] for x in overview["assets"] if x["team"] == "线路机" and x["name"] == a.get("relay")), None)
        for key in ("tcp_task_id", "egress_task_id"):
            if a.get(key):
                for who in dict.fromkeys(filter(None, (a["uuid"], relay_uuid))):
                    get("/komari/pings/%s/results" % a[key], {"hours": 24, "uuid": who})
    # Second poll 10 s later: the keepalive world step (stamps move, real Monitor.evaluate runs).
    if args.tick2:
        clock.base = as_of + args.tick2
        keepalive_step(app, clock, {"stamped_to": as_of, "behavior_at": clock()})
        get("/overview", sub="tick2")
        get("/health", sub="tick2")
    (out / "index.json").write_text(json.dumps({"as_of": as_of, "captured_by": "stage_server.py capture (pinned clock)",
                                                "routes": index}, ensure_ascii=False, indent=1))
    log("captured %d responses into %s" % (len(index), out))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("serve")
    s.add_argument("--static")
    s.add_argument("--port", type=int, default=DEFAULT_PORT)
    s.add_argument("--clock", choices=("keepalive", "shifted", "pinned"), default="keepalive")
    s.add_argument("--now", type=float)
    s.add_argument("--no-isolate", action="store_true")
    s.add_argument("--auth-token")
    s.add_argument("--writes", action="store_true", help="allow the class (a) team-ops DB write routes on the DB copy")
    s.add_argument("--release", default=str(HOME / "teamops-release"))
    s.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    s.add_argument("--sock", help=argparse.SUPPRESS)
    s.add_argument("--db-dir", help=argparse.SUPPRESS)
    c = sub.add_parser("capture")
    c.add_argument("--out", required=True)
    c.add_argument("--tick2", type=float, default=10)
    c.add_argument("--release", default=str(HOME / "teamops-release"))
    args = ap.parse_args()
    sys.path.insert(0, str(HERE))
    (cmd_serve if args.cmd == "serve" else cmd_capture)(args)


if __name__ == "__main__":
    main()
