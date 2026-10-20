#!/usr/bin/env python3
"""fleet_stage.py — a395 上的 fleetd 测试服。

在一个隔离的用户 + 网络命名空间（unshare -Urn，只有 lo）里同时跑：
  * 旧后端副本：直接复用 deploy/v2/staging/stage_server.py 的 load_backend（真实 teamops 代码 + 新的库副本；
    采集 / bridge / 远程命令全部禁用），在 127.0.0.1:18397 回答 /api/team-ops/*（不要 X-Ops-Key）；
  * Komari 替身（同一端口）：GET /api/me（Cookie session_token）与 POST /api/rpc2 的只读方法，数据来自
    komari_mirror（与生产 Komari 1.5 二进制返回形状一致）和副本里插件同步的最新主机快照；
  * fleetd：127.0.0.1:18396，auth.mode = komari，指向上面两个替身；影子模式；
  * 故障注入：POST /__fleet/inject 修改“副本的 overview 视图”，真实的 Monitor.evaluate 在其上判定健康，
    fleetd 再从旧接口推导告警（与生产同一路径）。
宿主侧只有两个转发：127.0.0.1:18396 → fleetd、127.0.0.1:18397 → 旧后端副本 / Komari 替身（经 unix socket）。
deploy/v2/staging 下的文件只读使用（启动前拷一份到 ~/team-ops-eval/fleet-stage/vendor），不修改。

用法（a395，python3.12 运行时）：
  fleet_stage.py serve [--clock keepalive|pinned] [--no-fleetd]
  （serve 前台运行；stage.sh 用 nohup 起它）
"""
import argparse
import contextlib
import datetime as dt
import json
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
import urllib.parse
from http.server import ThreadingHTTPServer

HOME = Path(os.path.expanduser("~/team-ops-eval"))
BASE = HOME / "fleet-stage"
VENDOR = BASE / "vendor"
sys.path.insert(0, str(VENDOR))
import stage_server as ss  # noqa: E402

STAGE_PORT = 18397
FLEETD_PORT = 18396
INJECT = {}
LOCK = threading.Lock()
COUNTS = {"legacy": 0, "rpc2": 0, "me": 0, "by_path": {}}
FLEETD = {"proc": None}


def log(*a):
    ss.log("[fleet-stage]", *a)


def short_id(name):
    head = name.split(" · ")[0].strip()
    m = re.fullmatch(r"团队(\d+)-(\d+)", head)
    if m:
        return "t%s-%s" % (m[1], m[2])
    m = re.fullmatch(r"线路机(\d+)", head)
    if m:
        return "r" + m[1]
    return head


def iso(t):
    return dt.datetime.fromtimestamp(t, dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.000000000Z")


def find(ov, short):
    return next((a for a in ov["assets"] if short_id(a["name"]) == short), None)


def apply_injections(ov, now):
    """在 overview 视图上叠加注入的故障（Monitor.evaluate 与 /overview 都走这里）。"""
    with LOCK:
        specs = list(INJECT.values())
    for sp in specs:
        op = sp["op"]
        if op == "node_fail":
            a = find(ov, sp["node"])
            if not a:
                continue
            p = dict(a.get("probe") or {})
            p.update(egress_error="curl: (35) OpenSSL SSL_connect: SSL_ERROR_SYSCALL", egress_rc=35, egress_phase="tls",
                     egress_attempt=3, actual_ip=None, healthy=False)
            a["probe"] = p
            path = dict(a.get("path") or {})
            path.update(healthy=False, rc=35, phase="tls", actual_ip=None)
            a["path"] = path
        elif op == "egress_mismatch":
            a = find(ov, sp["node"])
            if not a:
                continue
            p = dict(a.get("probe") or {})
            p.update(actual_ip=sp.get("ip", "203.0.113.9"), egress_error=None, egress_rc=0, egress_attempt=1)
            a["probe"] = p
        elif op == "relay_down":
            r = ov["relays"].get(sp["relay"])
            if r is not None:
                r["fresh"] = False
                r["received_at"] = sp["since"]
            a = find(ov, short_id(sp["relay"]))
            if a and a.get("host"):
                a["host"] = dict(a["host"], online=False, time=iso(sp["since"]))
                a["host_fresh"] = False
        elif op == "quota":
            a = find(ov, short_id(sp["relay"]))
            if a and a.get("host"):
                a["host"] = dict(a["host"], net_total_up=sp["bytes"])
    return ov


def inject_hosts(hosts):
    with LOCK:
        specs = list(INJECT.values())
    app = ss.StageState.app
    for sp in specs:
        if sp["op"] == "relay_down":
            uuid = next((json.loads(r[0])["uuid"] for r in app.store.ro.execute("SELECT data FROM assets")
                         if json.loads(r[0])["name"] == sp["relay"]), None)
            if uuid and uuid in hosts:
                hosts[uuid] = dict(hosts[uuid], online=False, time=iso(sp["since"]))
    return hosts


class H(ss.Handler):
    def cookie_ok(self):
        tok = (BASE / "session.token").read_text().strip()
        cookies = dict(c.strip().split("=", 1) for c in self.headers.get("Cookie", "").split(";") if "=" in c)
        return cookies.get("session_token") == tok

    def bearer_ok(self):
        key = (BASE / "komari.apikey").read_text().strip()
        return self.headers.get("Authorization", "") == "Bearer " + key

    def do_GET(self):
        path = urllib.parse.urlsplit(self.path).path
        if path.startswith("/api/team-ops/"):
            with LOCK:
                COUNTS["legacy"] += 1
                k = path[len("/api/team-ops"):]
                k = re.sub(r"/[0-9a-f-]{20,}", "/<id>", k)
                COUNTS["by_path"][k] = COUNTS["by_path"].get(k, 0) + 1
            return super().do_GET()
        if path == "/api/me":
            with LOCK:
                COUNTS["me"] += 1
            ok = self.cookie_ok()
            return ss.reply(self, 200, {"logged_in": ok, "username": "admin" if ok else "", "uuid": "stage-admin" if ok else "",
                                        "sso_id": "", "sso_type": "", "2fa_enabled": False})
        if path == "/__fleet/status":
            with LOCK:
                return ss.reply(self, 200, {"inject": INJECT, "counts": COUNTS, "now": ss.StageState.clock(),
                                            "fleetd_pid": FLEETD["proc"].pid if FLEETD["proc"] else None, **ss.StageState.info})
        return super().do_GET()

    def body(self):
        n = int(self.headers.get("Content-Length", "0") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def do_POST(self):
        path = urllib.parse.urlsplit(self.path).path
        if path == "/api/rpc2":
            with LOCK:
                COUNTS["rpc2"] += 1
            req = self.body()
            if not (self.cookie_ok() or self.bearer_ok()):
                return ss.reply(self, 200, {"jsonrpc": "2.0", "id": req.get("id"), "error": {"code": 401, "message": "Unauthorized"}})
            method, params = req.get("method"), req.get("params") or {}
            try:
                if method == "common:getNodesLatestStatus":
                    result = inject_hosts(ss.StageState.app.store.get_meta("hosts", {}))
                elif method in ("admin:listClients", "admin:getAllPingTasks", "public:getRecordsByUUID", "public:getPingRecords"):
                    result = ss.StageState.app.provision.komari(method, params)
                else:
                    return ss.reply(self, 200, {"jsonrpc": "2.0", "id": req.get("id"), "error": {"code": -32601, "message": "method not found"}})
                return ss.reply(self, 200, {"jsonrpc": "2.0", "id": req.get("id"), "result": result})
            except Exception as e:  # noqa: BLE001
                return ss.reply(self, 200, {"jsonrpc": "2.0", "id": req.get("id"), "error": {"code": -32000, "message": str(e)}})
        if path == "/__fleet/inject":
            spec = self.body()
            now = ss.StageState.clock()
            with LOCK:
                if spec.get("op") == "clear":
                    if spec.get("name"):
                        INJECT.pop(spec["name"], None)
                    else:
                        INJECT.clear()
                else:
                    spec.setdefault("since", now)
                    INJECT[spec.get("name") or spec["op"]] = spec
            log("inject", json.dumps(spec, ensure_ascii=False))
            return ss.reply(self, 200, {"inject": INJECT})
        if path == "/__fleet/fleetd":
            action = self.body().get("action")
            if action == "stop":
                stop_fleetd()
            elif action == "start":
                start_fleetd()
            return ss.reply(self, 200, {"fleetd_pid": FLEETD["proc"].pid if FLEETD["proc"] else None})
        return self.refuse()


def start_fleetd():
    if FLEETD["proc"] and FLEETD["proc"].poll() is None:
        return
    logf = open(BASE / "fleetd.log", "ab")
    FLEETD["proc"] = subprocess.Popen([str(BASE / "bin" / "fleetd"), "serve", "--config", str(BASE / "fleetd.toml")],
                                      stdout=logf, stderr=logf, env={**os.environ, "FLEETD_LOG": "info"})
    (BASE / "fleetd.pid").write_text(str(FLEETD["proc"].pid))
    log("fleetd started pid", FLEETD["proc"].pid)


def stop_fleetd():
    p = FLEETD["proc"]
    if p and p.poll() is None:
        p.terminate()
        with contextlib.suppress(Exception):
            p.wait(10)
    FLEETD["proc"] = None
    with contextlib.suppress(FileNotFoundError):
        (BASE / "fleetd.pid").unlink()
    log("fleetd stopped")


def unix_relay(sock_path, port):
    """命名空间内：unix socket → 127.0.0.1:port"""
    class P(socketserver.BaseRequestHandler):
        def handle(self):
            try:
                up = socket.create_connection(("127.0.0.1", port), timeout=10)
                up.settimeout(None)
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
            t.join(3600)
            up.close()
    with contextlib.suppress(FileNotFoundError):
        os.unlink(sock_path)
    srv = socketserver.ThreadingUnixStreamServer(str(sock_path), P)
    srv.daemon_threads = True
    os.chmod(sock_path, 0o600)
    threading.Thread(target=srv.serve_forever, daemon=True).start()


def child(args):
    db_dir = BASE / "db"
    as_of = ss.read_as_of(db_dir / "operations.db")
    real_now = _time.time()
    clock = ss.VClock(real_now, args.clock)
    app = ss.load_backend(db_dir, args.release, clock)
    orig = app.store.overview
    app.store.overview = lambda: apply_injections(orig(), clock())
    # 把副本整体搬到“现在”：时间戳前移 (now - as_of)，年龄保持 as_of 时的样子，并跑一次真实 Monitor.evaluate
    ss.keepalive_step(app, clock, {"stamped_to": as_of, "behavior_at": clock()})
    ss.StageState.app, ss.StageState.clock = app, clock
    ss.StageState.routes = ss.route_patterns(ss.load_allowlist(args.release))
    ss.StageState.static = None
    ss.StageState.auth_token = None
    ss.StageState.info = {"as_of": as_of, "shifted_to": real_now, "clock": args.clock, "network_isolated": ss.network_isolated(),
                          "db_copy": str(db_dir)}
    log("backend ready as_of=%s shifted_to=%s clock=%s isolated=%s" % (
        dt.datetime.fromtimestamp(as_of, ss.BJ).isoformat(), dt.datetime.fromtimestamp(real_now, ss.BJ).isoformat(),
        args.clock, ss.StageState.info["network_isolated"]))
    stop = threading.Event()
    if args.clock == "keepalive":
        threading.Thread(target=ss.keepalive_loop, args=(app, clock, stop), daemon=True).start()
    srv = ThreadingHTTPServer(("127.0.0.1", STAGE_PORT), H)
    srv.daemon_threads = True
    unix_relay(BASE / "stage.sock", STAGE_PORT)
    unix_relay(BASE / "fleetd.sock", FLEETD_PORT)
    (BASE / "child.pid").write_text(str(os.getpid()))
    if not args.no_fleetd:
        start_fleetd()

    def term(*_):
        stop_fleetd()
        threading.Thread(target=srv.shutdown, daemon=True).start()
    signal.signal(signal.SIGTERM, term)
    ppid = os.getppid()

    def orphan():
        while not stop.wait(2):
            if os.getppid() != ppid:
                term()
                return
    threading.Thread(target=orphan, daemon=True).start()
    srv.serve_forever()
    stop.set()


def serve(args):
    # 新副本（原文件只经 cp --reflink 读取）
    db = BASE / "db"
    if db.exists():
        shutil.rmtree(db)
    db.mkdir(parents=True, mode=0o700)
    for name in ("operations.db", "komari.db", "metrics.db"):
        subprocess.run(["cp", "--reflink=auto", str(HOME / "data" / name), str(db / name)], check=True)
        os.chmod(db / name, 0o600)
    for f, n in (("session.token", 24), ("komari.apikey", 24)):
        p = BASE / f
        if not p.exists():
            p.write_text(secrets.token_hex(n))
            os.chmod(p, 0o600)
    cmd = ["unshare", "-Urn", "sh", "-c", 'ip link set lo up && exec "$@"', "sh", sys.executable, str(Path(__file__).resolve()),
           "child", "--clock", args.clock, "--release", args.release] + (["--no-fleetd"] if args.no_fleetd else [])
    p = subprocess.Popen(cmd)
    for _ in range(600):
        if (BASE / "stage.sock").exists() and (BASE / "fleetd.sock").exists() or p.poll() is not None:
            break
        _time.sleep(0.2)
    if p.poll() is not None:
        log("child failed")
        sys.exit(1)
    stop = threading.Event()

    def term(*_):
        stop.set()
        with contextlib.suppress(Exception):
            p.terminate()
    signal.signal(signal.SIGTERM, term)
    signal.signal(signal.SIGINT, term)
    threading.Thread(target=lambda: (p.wait(), stop.set()), daemon=True).start()
    threading.Thread(target=ss.forwarder, args=(STAGE_PORT, str(BASE / "stage.sock"), stop), daemon=True).start()
    threading.Thread(target=ss.forwarder, args=(FLEETD_PORT, str(BASE / "fleetd.sock"), stop), daemon=True).start()
    stop.wait()
    with contextlib.suppress(Exception):
        p.terminate()
        p.wait(15)


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("serve", "child"):
        s = sub.add_parser(name)
        s.add_argument("--clock", choices=("keepalive", "pinned"), default="keepalive")
        s.add_argument("--release", default=str(HOME / "teamops-release"))
        s.add_argument("--no-fleetd", action="store_true")
    args = ap.parse_args()
    (serve if args.cmd == "serve" else child)(args)


if __name__ == "__main__":
    main()
