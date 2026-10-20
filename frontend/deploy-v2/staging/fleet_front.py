#!/usr/bin/env python3
"""fleet_front.py — one origin for the v2 UI + fleetd on a395 (what Caddy does in production).

  127.0.0.1:18398  /v2/api/*  → fleetd test instance 127.0.0.1:18396 (fleet/staging), with the test session cookie
                   everything else → the v2 staging server 127.0.0.1:18395 (/admin/v2/*, /api/team-ops/*)

Streams responses (SSE /v2/api/stream stays open).  Binds 127.0.0.1 only; standard library only.
  python3 fleet_front.py [--port 18398] [--ui 18395] [--fleet 18396] [--token-file ~/team-ops-eval/fleet-stage/session.token]
"""
import argparse
import http.client
import http.server
import os
import socketserver

HOP = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailers", "transfer-encoding", "upgrade"}


class Front(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    ui = 18395
    fleet = 18396
    token = ""
    cookie = "session_token"

    def log_message(self, fmt, *args):
        pass

    def _proxy(self):
        to_fleet = self.path.startswith("/v2/api/")
        port = self.fleet if to_fleet else self.ui
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else None
        headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP and k.lower() != "host"}
        headers["Host"] = f"127.0.0.1:{port}"
        if to_fleet:  # like Caddy: fleetd must not treat proxied browser requests as local diagnostics
            headers["X-Forwarded-For"] = self.client_address[0]
        if to_fleet and self.token:
            cookies = [c for c in headers.get("Cookie", "").split(";") if c.strip() and not c.strip().startswith(self.cookie + "=")]
            headers["Cookie"] = "; ".join([*cookies, f"{self.cookie}={self.token}"])
        sse = to_fleet and self.path.startswith("/v2/api/stream")
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=None if sse else 60)
        try:
            conn.request(self.command, self.path, body=body, headers=headers)
            r = conn.getresponse()
        except OSError as e:
            data = ('{"error":"upstream unavailable: %s"}' % type(e).__name__).encode()
            self.send_response(502); self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)
            return
        self.send_response(r.status, r.reason)
        if r.status in (204, 304) or self.command == "HEAD":  # no body: no Transfer-Encoding either (else the browser rejects it)
            for k, v in r.getheaders():
                if k.lower() not in HOP:
                    self.send_header(k, v)
            self.end_headers()
            r.read()
            conn.close()
            return
        chunked = r.getheader("Content-Length") is None
        for k, v in r.getheaders():
            if k.lower() not in HOP:
                self.send_header(k, v)
        if chunked:
            self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        try:
            while True:
                try:
                    chunk = r.read1(65536) if hasattr(r, "read1") else r.read(65536)
                except (http.client.HTTPException, OSError):
                    chunk = b""  # upstream went away mid-stream (fleetd restart): end the response cleanly, the client reconnects
                if not chunk:
                    break
                if chunked:
                    self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                else:
                    self.wfile.write(chunk)
                self.wfile.flush()
            if chunked:
                self.wfile.write(b"0\r\n\r\n")
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            conn.close()

    do_GET = do_POST = do_DELETE = do_PUT = do_PATCH = do_HEAD = _proxy


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=18398)
    ap.add_argument("--ui", type=int, default=18395)
    ap.add_argument("--fleet", type=int, default=18396)
    ap.add_argument("--token-file", default=os.path.expanduser("~/team-ops-eval/fleet-stage/session.token"))
    ap.add_argument("--cookie-name", default="session_token")
    a = ap.parse_args()
    Front.ui, Front.fleet, Front.cookie = a.ui, a.fleet, a.cookie_name
    try:
        Front.token = open(a.token_file).read().strip()
    except OSError:
        Front.token = ""
    Server(("127.0.0.1", a.port), Front).serve_forever()


if __name__ == "__main__":
    main()
