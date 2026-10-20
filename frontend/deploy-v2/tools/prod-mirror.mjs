// prod-mirror.mjs — serve the local dist at http://127.0.0.1:<port>/ and forward GET /v2/api/* to production with a
// session cookie read from a file, so a new build can be checked against real production data WITHOUT deploying it.
// READ-ONLY: every non-GET request is answered 405 here and never reaches production.  Binds 127.0.0.1 only.
//   PROD_SESSION_FILE=~/.config/teamops/prod-session node deploy/v2/tools/prod-mirror.mjs [--port 18470] [--origin https://38.58.57.22]
import http from 'node:http';
import https from 'node:https';
import httpc from 'node:http';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
const arg = (k, d) => { const i = process.argv.indexOf(k); return i > 0 ? process.argv[i + 1] : d; };
const PORT = Number(arg('--port', 18470));
const ORIGIN = new URL(arg('--origin', 'https://38.58.57.22'));
const DIST = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..', 'dist');
const TOKEN = process.env.NO_SESSION ? '' : fs.readFileSync(process.env.PROD_SESSION_FILE ?? path.join(os.homedir(), '.config/teamops/prod-session'), 'utf8').trim();
const CSP = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; worker-src 'self' blob:; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'self'";
const TYPES = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.mjs': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8', '.json': 'application/json', '.woff2': 'font/woff2', '.svg': 'image/svg+xml', '.png': 'image/png' };
http.createServer((req, res) => {
  const u = new URL(req.url, 'http://x');
  if (u.pathname.startsWith('/v2/api/')) {
    if (req.method !== 'GET' && req.method !== 'HEAD') { res.writeHead(405, { 'Content-Type': 'application/json' }); res.end('{"error":"prod-mirror: read-only"}'); return; }
    const h = { Accept: req.headers.accept ?? 'application/json', ...(TOKEN ? { Cookie: `fleet_session=${TOKEN}` } : {}) };
    if (req.headers['if-none-match']) h['If-None-Match'] = req.headers['if-none-match'];
    const lib = ORIGIN.protocol === 'http:' ? httpc : https;
    const up = lib.request({ host: ORIGIN.hostname, port: ORIGIN.port || (ORIGIN.protocol === 'http:' ? 80 : 443), path: req.url, method: 'GET', headers: h, rejectUnauthorized: false }, (r) => {
      const out = { ...r.headers }; delete out['set-cookie']; delete out['transfer-encoding']; delete out.connection;
      res.writeHead(r.statusCode, out); r.pipe(res);
    });
    up.on('error', (e) => { if (!res.headersSent) res.writeHead(502); res.end(); void e; });
    req.on('close', () => up.destroy());
    up.end();
    return;
  }
  let p = u.pathname === '/' ? '/index.html' : u.pathname;
  const f = path.join(DIST, path.normalize(p));
  if (!f.startsWith(DIST) || !fs.existsSync(f) || fs.statSync(f).isDirectory()) { res.writeHead(404); res.end(); return; }
  res.writeHead(200, { 'Content-Type': TYPES[path.extname(f)] ?? 'application/octet-stream', 'Content-Security-Policy': CSP, 'Cache-Control': 'no-cache' });
  fs.createReadStream(f).pipe(res);
}).listen(PORT, '127.0.0.1', () => console.log(`prod-mirror on http://127.0.0.1:${PORT}/ (GET-only → ${ORIGIN.origin})`));
