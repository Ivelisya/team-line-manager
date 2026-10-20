// 本机复刻原版控制台：原生 Komari 外壳（komari-admin-live）+ 运营 iframe（plugin/public）+ 首页 iframe（plugin/home）。
// 只读本地快照文件，只监听 127.0.0.1，不访问任何线上地址。
// 用法：node server.mjs [port]      环境变量 REPLAY=0 关闭“真实历史故障回放”。
import http from 'node:http';
import https from 'node:https';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as M from './mock.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const SNAP = path.resolve(here, '../../../upstream-snapshot');
const NATIVE = path.join(SNAP, 'komari-admin-live');
const PUB = path.join(SNAP, 'release/plugin/public');
const HOME = path.join(SNAP, 'release/plugin/home');
const PORT = Number(process.argv[2] || 5198);

const state = { loggedIn: true };
const misses = new Map();

const types = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.mjs': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8', '.json': 'application/json; charset=utf-8', '.svg': 'image/svg+xml', '.png': 'image/png', '.webp': 'image/webp', '.ico': 'image/x-icon', '.woff2': 'font/woff2', '.woff': 'font/woff', '.ttf': 'font/ttf', '.mp4': 'video/mp4', '.webmanifest': 'application/manifest+json' };

function file(res, base, rel) {
  const p = path.normalize(path.join(base, rel));
  if (!p.startsWith(base) || !fs.existsSync(p) || fs.statSync(p).isDirectory()) return false;
  res.writeHead(200, { 'Content-Type': types[path.extname(p)] || 'application/octet-stream', 'Cache-Control': 'no-store' });
  fs.createReadStream(p).pipe(res);
  return true;
}
function json(res, body, status = 200) { res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8', 'Cache-Control': 'no-store' }); res.end(JSON.stringify(body)); }
async function body(req) { let b = ''; for await (const c of req) b += c; try { return JSON.parse(b || '{}'); } catch { return {}; } }

const settings = {
  sitename: M.site.name, description: M.site.description, allow_cors: false, theme: 'LuminaPlus', private_site: true, api_key: '', auto_discovery_key: '', script_domain: '', send_ip_addr_to_guest: false, eula_accepted: true,
  geo_ip_enabled: true, geo_ip_provider: 'ipinfo', nezha_compat_enabled: false, o_auth_enabled: false, o_auth_provider: 'github', disable_password_login: false,
  custom_head: '', custom_body: '', notification_enabled: true, notification_method: 'Javascript', notification_template: '{{emoji}}{{emoji}}{{emoji}}\nEvent: {{event}}\nClients: {{client}}\nMessage: {{message}}\nTime: {{time}}',
  expire_notification_enabled: true, expire_notification_lead_days: 7, login_notification: true, traffic_limit_percentage: 80, record_enabled: true, record_preserve_time: 720, ping_record_preserve_time: 24,
};

async function rpc(method, params) {
  switch (method) {
    case 'common:getVersion': return { version: '1.5.0-review20260917.4', hash: 'f18ad72c0b1e' };
    case 'common:getMe': return { logged_in: state.loggedIn, username: 'admin', uuid: 'demo-admin', '2fa_enabled': false };
    case 'common:getPublicInfo': case 'public:getPublicSettings': return publicInfo();
    case 'admin:listPlugins': return [{ short: 'team-ops', name: 'team-ops', version: '1.2.0', enabled: true, running: true, pages: [] }];
    case 'admin:getSettings': return settings;
    case 'admin:listClients': return M.komariClients();
    case 'common:getNodes': { const o = {}; for (const c of M.komariClients()) o[c.uuid] = c; return o; }
    case 'common:getNodesLatestStatus': return M.komariLatest();
    case 'public:getPublicPingTasks': return M.pingTasks();
    case 'public:queryMetrics': return { start: new Date(Date.now() - 864e5).toISOString(), end: new Date().toISOString(), series: [] };
    case 'public:getPingMetricStats': return { stats: [] };
    case 'common:getRecords': return { count: 0, records: [] };
    default: return null;
  }
}
function publicInfo() { return { sitename: M.site.name, description: M.site.description, theme: 'LuminaPlus', private_site: true, disable_password_login: false, oauth_enable: false, oauth_provider: '', allow_cors: false, custom_head: '', custom_body: '', theme_settings: {}, version: '1.5.0' }; }

// 原生页的演示数据：只为渲染外观；数量级参照 inventory/native.md（244 个会话、1015 条日志）。
function sessions() {
  const now = Date.now();
  const iso = ms => new Date(ms).toISOString();
  const ua = ['Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36', 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 Mobile/15E148 Safari/604.1', 'python-requests/2.32.3'];
  const data = Array.from({ length: 12 }, (_, k) => ({ uuid: 'demo-admin', session: 'sess' + (1000 + k).toString(16) + 'a1b2c3d4e5f6', user_agent: ua[k % 3], ip: '203.0.113.' + (60 + (k % 5)), login_method: 'password', latest_online: iso(now - k * 3600e3), latest_ip: '203.0.113.' + (60 + (k % 5)), latest_user_agent: ua[k % 3], expires: iso(now + (20 - k / 4) * 86400e3), created_at: iso(now - (8 - k / 3) * 86400e3) }));
  return { current: data[0].session, data };
}
function logs(q) {
  const limit = Number(q.get('limit') || 10);
  const types = ['error', 'login', 'info', 'error', 'warn', 'login', 'info', 'logout', 'error', 'terminal'];
  const msg = { error: 'Failed to send event message: JavaScript script is empty', login: 'login succeeded: admin', info: 'client edited: 团队1-21 · ATT · 198.51.100.31', warn: 'client deleted: 待接入-7f2a', logout: 'logout: admin', terminal: 'terminal session opened: 线路机2' };
  const now = Date.now();
  const rows = Array.from({ length: limit }, (_, k) => ({ id: 1015 - k, ip: '203.0.113.' + (60 + (k % 4)), uuid: k % 3 ? 'demo-admin' : '', message: msg[types[k % types.length]], msg_type: types[k % types.length], time: new Date(now - k * 1800e3).toISOString() }));
  return { logs: rows, total: 1015 };
}

const handler = async (req, res) => {
  const u = new URL(req.url, 'http://x');
  const p = decodeURIComponent(u.pathname);
  try {
    if (p === '/__mock/login') { state.loggedIn = u.searchParams.get('state') !== '0'; return json(res, { loggedIn: state.loggedIn }); }
    if (p === '/__mock/misses') return json(res, Object.fromEntries(misses));
    if (p.startsWith('/api/team-ops/')) {
      const r = M.route(p.slice('/api/team-ops'.length), u.searchParams);
      if (r === undefined || r === null) return json(res, { error: '接口不存在（本地复刻未实现）' }, 404);
      return json(res, { data: r });
    }
    if (p.startsWith('/ops/assets/')) return file(res, PUB, p.slice('/ops/assets/'.length)) || json(res, { error: 'nf' }, 404);
    if (p.startsWith('/api/admin/plugin/team-ops/public/')) { const rel = p.slice('/api/admin/plugin/team-ops/public/'.length) || 'index.html'; return file(res, PUB, rel) || json(res, { error: 'nf' }, 404); }
    if (p.startsWith('/api/admin/plugin/team-ops/home/')) { const rel = p.slice('/api/admin/plugin/team-ops/home/'.length) || 'index.html'; return file(res, HOME, rel) || json(res, { error: 'nf' }, 404); }
    if (p === '/api/me') return json(res, state.loggedIn ? { logged_in: true, username: 'admin', uuid: 'demo-admin', '2fa_enabled': false, sso_type: '', sso_id: '' } : { logged_in: false });
    if (p === '/api/public') return json(res, { status: 'success', message: '', data: publicInfo() });
    if (p === '/api/admin/settings' || p === '/api/admin/settings/') return json(res, { status: 'success', message: '', data: settings });
    if (p === '/api/rpc2') {
      if (req.method !== 'POST') { res.writeHead(400); return res.end(); }
      const b = await body(req);
      const one = async x => { try { return { jsonrpc: '2.0', id: x.id, result: await rpc(x.method, x.params) }; } catch (e) { return { jsonrpc: '2.0', id: x.id, error: { code: e.code || -32000, message: e.message } }; } };
      return json(res, Array.isArray(b) ? await Promise.all(b.map(one)) : await one(b));
    }
    if (p === '/api/admin/client/list') return json(res, M.komariClients());
    if (p === '/api/admin/session/get') { const ss = sessions(); return json(res, { status: 'success', message: '', current: ss.current, data: ss.data }); }
    if (p === '/api/admin/database/size') return json(res, { status: 'success', data: { main: { size: 593920 }, monitoring: { size: 111779840 } } });
    if (p.startsWith('/assets/flags/')) return file(res, path.join(NATIVE, 'assets/flags'), p.slice('/assets/flags/'.length)) || json(res, { error: 'nf' }, 404);
    if (p === '/api/admin/logs') return json(res, { status: 'success', message: '', data: logs(u.searchParams) });
    if (p === '/api/admin/ping') return json(res, { status: 'success', message: '', data: M.pingTasks() });
    if (p === '/api/records/ping') return json(res, { status: 'success', message: '', data: M.pingRecords(u.searchParams) });
    if (p.startsWith('/themes/') || p === '/assets/pwa-icon.webp') { res.writeHead(404); return res.end(); }
    if (p === '/api/nodes') return json(res, { status: 'success', data: M.komariClients() });
    if (p.startsWith('/komari-admin/')) return file(res, NATIVE, p.slice('/komari-admin/'.length)) || json(res, { error: 'nf' }, 404);
    if (p === '/favicon.ico') return file(res, NATIVE, 'favicon.ico');
    if (p === '/' || p === '/admin' || p.startsWith('/admin/') || p === '/terminal' || p === '/instance') return file(res, NATIVE, 'index.html');
    misses.set(req.method + ' ' + p, (misses.get(req.method + ' ' + p) || 0) + 1);
    if (p.startsWith('/api/')) return json(res, { status: 'error', message: 'not mocked', data: null }, 404);
    res.writeHead(404); res.end('not found');
  } catch (e) {
    json(res, { error: String(e?.stack || e) }, 500);
  }
};
// 原版外壳在非 HTTPS 下会显示红色“当前连接不是 HTTPS”提示，所以默认用本机自签证书走 HTTPS（Chrome 以 --ignore-certificate-errors 打开）。
const key = path.join(here, 'local-key.pem'), cert = path.join(here, 'local-cert.pem');
const server = fs.existsSync(key) && process.env.PLAIN !== '1' ? https.createServer({ key: fs.readFileSync(key), cert: fs.readFileSync(cert) }, handler) : http.createServer(handler);
server.listen(PORT, '127.0.0.1', () => console.log('original-console replica on ' + (server instanceof https.Server ? 'https' : 'http') + '://127.0.0.1:' + PORT));
