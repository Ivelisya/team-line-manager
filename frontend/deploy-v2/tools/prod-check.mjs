// prod-check.mjs — the owner's issues, checked on the NEW build against production data (via tools/prod-mirror.mjs;
// read-only: the browser blocks every non-GET, the mirror refuses them too).  Boot time, sidebar, relay cards,
// node-drawer source cards on nodes with traffic, alert labels.  Screenshots to --out.
import fs from 'node:fs';
import path from 'node:path';
import { launch, sleep, js } from './cdp.mjs';
const arg = (k, d) => { const i = process.argv.indexOf(k); return i > 0 ? process.argv[i + 1] : d; };
const BASE = arg('--base', 'http://127.0.0.1:18470');
const OUT = arg('--out', 'shots/prod-check');
fs.mkdirSync(OUT, { recursive: true });
const b = await launch({ readOnly: true });
await b.send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
const t0 = Date.now();
await b.send('Page.navigate', { url: `${BASE}/#/home` });
const ready = await b.until("document.documentElement.dataset.ready === '1'", 30000);
const out = { readyMs: ready ? Date.now() - t0 : null };
await sleep(3000);
out.home = await b.eval(js(() => ({ fresh: document.querySelector('[data-fresh]')?.innerText, cards: document.querySelectorAll('.ncard').length, skel: document.querySelectorAll('.ncard .skel').length,
  nav: [...document.querySelectorAll('.nav-item')].map((x) => x.innerText.trim()).filter(Boolean), sys: globalThis.TEAMOPS?.fleet?.system?.version, decom: globalThis.TEAMOPS?.decommissioned })));
await b.shot(path.join(OUT, 'home.png'));
// relay cards
await b.eval("location.hash = '#/home?team=%E7%BA%BF%E8%B7%AF%E6%9C%BA'; 0");
await sleep(2500);
out.relays = await b.eval(js(() => [...document.querySelectorAll('.ncard')].map((c) => ({ name: c.querySelector('.ncard-head')?.innerText.trim().split('\n')[0], quota: c.querySelector('[data-s="fquota"]')?.innerText, quse: c.querySelector('[data-s="quse"]')?.innerText, beat: c.querySelector('[data-b="cap"]')?.innerText }))));
await b.shot(path.join(OUT, 'relays.png'));
// node drawer → access tab on 3 busy nodes
const ids = await b.eval("__app.store.nodes().filter((n) => !n.isRelay && (n.rate?.down_bps ?? 0) > 0).sort((a, b) => (b.rate?.down_bps ?? 0) - (a.rate?.down_bps ?? 0)).slice(0, 3).map((n) => n.id)");
out.sources = [];
for (const id of ids) {
  await b.eval(`__app.closeDrawerParams?.(); location.hash = '#/home'; 0`); await sleep(800);
  await b.eval(`__app.openNode(${JSON.stringify(id)}, 'access'); 0`);
  await b.until("!!document.querySelector('.drawer [data-src-table] table, .drawer [data-src-table] ~ .empty, .drawer .empty')", 25000);
  await sleep(2500);
  out.sources.push(await b.eval(js(() => ({
    id: new URLSearchParams(location.hash.split('?')[1]).get('node'),
    summary: document.querySelector('.drawer [data-src-card] [data-src-body]')?.innerText.replace(/\s+/g, ' ').slice(0, 160),
    rows: document.querySelectorAll('.drawer [data-src-table] tbody tr, .drawer table[aria-label="来源 IP"] tbody tr').length,
    count: document.querySelector('.drawer [data-src-count]')?.innerText,
    empties: [...document.querySelectorAll('.drawer .empty')].map((e) => e.innerText.trim()),
  }))));
  await b.shot(path.join(OUT, `drawer-access-${id}.png`));
}
// alerts: no raw codes
await b.eval(`__app.closeDrawerParams?.(); location.hash = '#/alerts?state=all'; 0`);
await b.until("!!document.querySelector('[data-page=\"alerts\"] table')", 15000);
await sleep(1500);
out.alertsRaw = await b.eval("[...document.querySelectorAll('[data-page=\"alerts\"] table td')].map((t) => t.innerText).filter((t) => /\\b[a-z]+_[a-z_]+\\b/.test(t)).slice(0, 8)");
await b.shot(path.join(OUT, 'alerts.png'));
out.failed = b.ev.failed; out.console = b.ev.console; out.exceptions = b.ev.exceptions; out.blocked = b.ev.blocked; out.csp = b.ev.csp;
console.log(JSON.stringify(out, null, 1));
await b.close();
