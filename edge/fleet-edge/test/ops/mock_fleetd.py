#!/usr/bin/env python3
"""测试用 fleetd：实现 API.md §10（observed / spec / spec/ack）和 /v2/ingest/{node,relay}，状态落在 --dir 下的文件里。
spec.json 由测试驱动器写；不存在时 /spec 回 404（模拟未启用管理模式）。只监听 127.0.0.1。"""
import json, os, sys, threading
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from urllib.parse import urlparse, parse_qs
D = sys.argv[1]
PORT = int(sys.argv[2])
lock = threading.Lock()

def jl(name, obj):
    with lock, open(os.path.join(D, name), "a") as f:
        f.write(json.dumps(obj, ensure_ascii=False) + "\n")

def done_cmds():
    ids = set()
    p = os.path.join(D, "acks.jsonl")
    if os.path.exists(p):
        for l in open(p):
            for c in json.loads(l).get("commands", []):
                ids.add(c.get("id"))
    return ids

class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, code, obj=None):
        b = b"" if obj is None else json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def body(self):
        return json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")

    def do_POST(self):
        u = urlparse(self.path).path
        b = self.body()
        if u in ("/v2/ingest/relay", "/v2/ingest/node"):
            keep = {k: b.get(k) for k in ("seq", "host_id", "access")}
            keep["xray"] = {k: (b.get("xray") or {}).get(k) for k in ("spec_generation", "manage", "ledger_notes")}
            jl("batches.jsonl", keep)
            return self.reply(200, {"status": "ok"})
        if u == "/v2/ingest/relay/observed":
            with lock:
                json.dump(b, open(os.path.join(D, "observed.json"), "w"))
            jl("observed.jsonl", {"config_sha256": b["config_sha256"], "generation": b["generation"]})
            return self.reply(200, {"ok": True, "generation": b.get("generation")})
        if u == "/v2/ingest/relay/spec/ack":
            jl("acks.jsonl", b)
            return self.reply(200, {"ok": True})
        self.reply(404, {"error": "not found"})

    def do_GET(self):
        u = urlparse(self.path)
        if u.path == "/v2/ingest/relay/spec":
            p = os.path.join(D, "spec.json")
            if not os.path.exists(p):
                return self.reply(404, {"error": "not enabled"})
            spec = json.load(open(p))
            have = int(parse_qs(u.query).get("have", ["0"])[0])
            pend = [c for c in spec.get("commands", []) if c["id"] not in done_cmds()]
            if spec["generation"] == have and not pend:
                return self.reply(304)
            spec["commands"] = pend
            return self.reply(200, spec)
        self.reply(404, {"error": "not found"})

ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
