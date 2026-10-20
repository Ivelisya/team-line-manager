#!/usr/bin/env python3
"""fleet-agent 实验室联调用的假中控（仅标准库）。

  fake_server.py --port 8080 --token TOKEN --dir /tmp/fake [--hold 5]

- POST /agent/v1/report   记录到 DIR/reports.jsonl，回 ReportAck（accepted_seq = 见过的最大 seq）
- GET  /agent/v1/desired  读 DIR/desired.json，generation > after 就返回；否则最多挂起 --hold 秒后 204
- POST /agent/v1/applied  记录到 DIR/applied.jsonl
故障注入：DIR/fail 里写状态码（如 503/400）→ report 一律返回该码；DIR/revoke 存在 → 全部 401。
"""
import argparse, json, os, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

A = None
LOCK = threading.Lock()
MAX_SEQ = 0


def append(name, obj):
    with LOCK, open(os.path.join(A.dir, name), "a") as f:
        f.write(json.dumps(obj, ensure_ascii=False) + "\n")


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, code, body=None):
        data = json.dumps(body).encode() if body is not None else b""
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def authed(self):
        if os.path.exists(os.path.join(A.dir, "revoke")):
            return False
        return self.headers.get("authorization") == "Bearer " + A.token

    def do_POST(self):
        global MAX_SEQ
        n = int(self.headers.get("content-length", 0))
        raw = self.rfile.read(n)
        path = urlparse(self.path).path
        if not self.authed():
            return self.reply(401, {"error": "bad token"})
        if path == "/agent/v1/report":
            fp = os.path.join(A.dir, "fail")
            if os.path.exists(fp):
                code = int(open(fp).read().strip() or 503)
                return self.reply(code, {"error": "injected"})
            r = json.loads(raw)
            with LOCK:
                dup = r["seq"] <= MAX_SEQ
                MAX_SEQ = max(MAX_SEQ, r["seq"])
                acc = MAX_SEQ
            if not dup:
                append("reports.jsonl", {
                    "seq": r["seq"], "boot_id": r["boot_id"], "agent": r["agent_version"],
                    "sent_at_ms": r["sent_at_ms"], "recv_ms": int(time.time() * 1000),
                    "info": r.get("info"), "host": len(r.get("host", [])),
                    "entries": r.get("entries", []), "probes": r.get("probes", []),
                    "access": r.get("access", []), "xray": r.get("xray"),
                    "last_host": (r.get("host") or [None])[-1],
                })
            return self.reply(200, {"accepted_seq": acc, "interval_s": 10})
        if path == "/agent/v1/applied":
            append("applied.jsonl", json.loads(raw))
            return self.reply(200, {})
        self.reply(404, {})

    def do_GET(self):
        u = urlparse(self.path)
        if not self.authed():
            return self.reply(401, {"error": "bad token"})
        if u.path != "/agent/v1/desired":
            return self.reply(404, {})
        after = int(parse_qs(u.query).get("after", ["0"])[0])
        deadline = time.time() + A.hold
        fp = os.path.join(A.dir, "desired.json")
        while True:
            try:
                d = json.load(open(fp))
                if d["generation"] > after:
                    return self.reply(200, d)
            except (OSError, ValueError):
                pass
            if time.time() >= deadline:
                return self.reply(204)
            time.sleep(0.5)


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=8080)
    p.add_argument("--token", required=True)
    p.add_argument("--dir", default="/tmp/fake")
    p.add_argument("--hold", type=float, default=5)
    A = p.parse_args()
    os.makedirs(A.dir, exist_ok=True)
    ThreadingHTTPServer(("0.0.0.0", A.port), H).serve_forever()
