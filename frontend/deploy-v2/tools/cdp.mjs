// cdp.mjs — headless Chrome over the DevTools protocol for the smoke tools (smoke.mjs, flow-smoke.mjs): launch a fresh
// profile, record console errors / exceptions / CSP / failed requests / API requests, evaluate, wait, screenshot.
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import net from 'node:net';

export const CHROME = process.env.CHROME ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const js = (f) => `(${f.toString()})()`;

function freePort() {
  return new Promise((res) => { const s = net.createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => res(p)); }); });
}

/** launch({ readOnly }) — readOnly: every non-GET/HEAD request is failed at the network layer (production probes). */
export async function launch({ readOnly = false } = {}) {
  const port = await freePort();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'v2-smoke-'));
  const proc = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${port}`, `--user-data-dir=${dir}`, '--lang=zh-CN', '--accept-lang=zh-CN,zh',
    '--no-first-run', '--no-default-browser-check', '--hide-scrollbars', '--force-color-profile=srgb', '--window-size=1440,900', 'about:blank'], { stdio: 'ignore' });
  let targets = [];
  for (let i = 0; i < 100; i++) {
    try { targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json(); if (targets.some((t) => t.type === 'page')) break; } catch { /* starting */ }
    await sleep(150);
  }
  const ws = new WebSocket(targets.find((t) => t.type === 'page').webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const pending = new Map();
  const ev = { console: [], exceptions: [], csp: [], failed: [], api: [], blocked: [], requests: new Map(), inflight: 0, peak: 0 };
  ws.onmessage = (msg) => {
    const m = JSON.parse(msg.data);
    if (m.id && pending.has(m.id)) { const { res, rej } = pending.get(m.id); pending.delete(m.id); if (m.error) rej(new Error(JSON.stringify(m.error))); else res(m.result); return; }
    const p = m.params;
    if (m.method === 'Fetch.requestPaused') {
      const meth = p.request.method;
      if (meth === 'GET' || meth === 'HEAD') send('Fetch.continueRequest', { requestId: p.requestId }).catch(() => {});
      else { ev.blocked.push(`${meth} ${p.request.url}`); send('Fetch.failRequest', { requestId: p.requestId, errorReason: 'BlockedByClient' }).catch(() => {}); }
      return;
    }
    switch (m.method) {
      case 'Runtime.exceptionThrown': ev.exceptions.push(p.exceptionDetails?.exception?.description || p.exceptionDetails?.text); break;
      case 'Runtime.consoleAPICalled':
        if (p.type === 'error' || p.type === 'warning' || p.type === 'assert') ev.console.push(`${p.type}: ${p.args.map((a) => a.value ?? a.description).join(' ')}`);
        break;
      case 'Log.entryAdded': {
        const t = `${p.entry.source}/${p.entry.level}: ${p.entry.text} ${p.entry.url ?? ''}`;
        if (/Content Security Policy/i.test(p.entry.text)) ev.csp.push(t);
        else if ((p.entry.level === 'error' || p.entry.level === 'warning') && !/\/v2\/api\/v1\/system/.test(p.entry.url ?? '')) ev.console.push(t);
        break;
      }
      case 'Network.requestWillBeSent': {
        const u = p.request.url;
        ev.requests.set(p.requestId, { url: u, method: p.request.method });
        if (u.includes('/api/team-ops/') || u.includes('/v2/api/')) {
          ev.api.push({ url: u, method: p.request.method, t: Date.now() });
          // the SSE stream stays open by design: counted, but not as “in flight”
          if (!u.includes('/v2/api/stream')) { ev.inflight++; ev.peak = Math.max(ev.peak, ev.inflight); ev.requests.get(p.requestId).api = true; }
        }
        break;
      }
      case 'Network.responseReceived': {
        const r = ev.requests.get(p.requestId);
        if (r) r.status = p.response.status;
        // the fleetd probe at load (/v2/api/v1/system) 404s by design while fleetd is not deployed: not a failure
        if (p.response.status >= 400 && !(p.response.status === 404 && p.response.url.endsWith('/v2/api/v1/system'))) ev.failed.push(`${p.response.status} ${p.response.url}`);
        break;
      }
      case 'Network.loadingFinished': { const r = ev.requests.get(p.requestId); if (r?.api && !r.done) { r.done = true; ev.inflight--; } break; }
      case 'Network.loadingFailed': {
        const r = ev.requests.get(p.requestId);
        if (r?.api && !r.done) { r.done = true; ev.inflight--; }
        if (!p.canceled) ev.failed.push(`${p.errorText} ${r?.url ?? ''}`);
        break;
      }
      default:
    }
  };
  const send = (method, params = {}) => new Promise((res, rej) => { const i = ++id; pending.set(i, { res, rej }); ws.send(JSON.stringify({ id: i, method, params })); });
  await send('Page.enable'); await send('Runtime.enable'); await send('Log.enable'); await send('Network.enable');
  if (readOnly) await send('Fetch.enable', { patterns: [{ urlPattern: '*', requestStage: 'Request' }] });
  await send('Network.setCacheDisabled', { cacheDisabled: false });
  await send('Page.addScriptToEvaluateOnNewDocument', { source: 'window.__csp=[];document.addEventListener("securitypolicyviolation",e=>window.__csp.push(e.violatedDirective+" "+e.blockedURI),true);' });
  const b = {
    send, ev,
    async eval(expr) { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text); return r.result.value; },
    async until(expr, timeout = 20000) { const t = Date.now(); while (Date.now() - t < timeout) { try { if (await b.eval(expr)) return true; } catch { /* navigating */ } await sleep(150); } return false; },
    async shot(file) { const r = await send('Page.captureScreenshot', { format: 'png' }); fs.writeFileSync(file, Buffer.from(r.data, 'base64')); },
    reset() { ev.console.length = 0; ev.exceptions.length = 0; ev.csp.length = 0; ev.failed.length = 0; },
    async close() {
      try { ws.close(); } catch { /* closed */ }
      const exited = new Promise((r) => proc.once('exit', r));
      proc.kill('SIGTERM');
      await Promise.race([exited, sleep(5000)]);
      try { fs.rmSync(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }); } catch { /* temp dir, best effort */ }
    },
  };
  return b;
}

