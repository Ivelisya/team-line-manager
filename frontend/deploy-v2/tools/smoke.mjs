// smoke.mjs — headless Chrome smoke of the dist build on the a395 staging server (through the SSH tunnel).
//
//   ssh -i ~/.ssh/a395_kismet -N -L 18395:127.0.0.1:18395 kismet@10.172.17.213 &
//   ~/.local/bin/node deploy/v2/tools/smoke.mjs [--base http://127.0.0.1:18395] [--out deploy/v2/shots/phase1]
//
// For each mode (1440×900 light, 1440×900 dark, 390×844 light) it loads / fresh (the app lives at the site
// root), walks every route by hash, opens one node drawer, and records: console errors and
// warnings, uncaught exceptions, CSP violations (securitypolicyviolation events + console “Content Security
// Policy” messages), failed requests (HTTP ≥ 400 or network errors), API request counts and the peak number
// of API requests in flight, and whether the page shows live data.  Screenshots: <out>/<route>-<mode>.png.
// Only talks to the given base URL (127.0.0.1); never production.
import fs from 'node:fs';
import path from 'node:path';
import { launch, sleep, js } from './cdp.mjs';

const HERE = path.dirname(new URL(import.meta.url).pathname);
const arg = (k, d) => { const i = process.argv.indexOf(k); return i > 0 ? process.argv[i + 1] : d; };
const BASE = arg('--base', 'http://127.0.0.1:18395');
const OUT = path.resolve(arg('--out', path.join(HERE, '..', 'shots', 'phase2')));
const ONLY = arg('--only', null);
if (!/^http:\/\/(127\.0\.0\.1|localhost)(:\d+)?$/.test(BASE)) { console.error('smoke only runs against a local tunnel (127.0.0.1)'); process.exit(2); }

const ROUTES = [
  { name: 'home', hash: '#/home', ready: '.ncard, .cards article, [data-card]' },
  { name: 'dashboard', hash: '#/dashboard', ready: '.kpi' },
  { name: 'nodes', hash: '#/nodes', ready: '.dt, table' },
  { name: 'lines', hash: '#/lines', ready: '.relay, .panel' },
  { name: 'behavior', hash: '#/behavior', ready: '.kpi' },
  { name: 'onboarding', hash: '#/onboarding', ready: '.panel, .empty' },
  { name: 'drawer', hash: null, drawer: true, ready: '.drawer' },
  { name: 'alerts', hash: '#/alerts', ready: '[data-page="alerts"] .panel' },
  { name: 'notify', hash: '#/notify', ready: '[data-page="notify"] .panel' },
  { name: 'plans', hash: '#/plans', ready: '[data-page="plans"] .panel' },
  { name: 'api-keys', hash: '#/api-keys', ready: '[data-page="api-keys"] .panel' },
  { name: 'account', hash: '#/account', ready: '[data-page="account"] .panel' },
  { name: 'native', hash: '#/native/servers', ready: '.placeholder, .empty' },
];
const MODES = [
  { name: 'light-1440', width: 1440, height: 900, dark: false },
  { name: 'dark-1440', width: 1440, height: 900, dark: true },
  { name: 'light-390', width: 390, height: 844, dark: false, mobile: true },
];


// Write actions to click (in live mode each must show the read-only notice, open no dialog, send no request).
// prep: JS run first (navigate, open a drawer tab …); sel: the button to click.
const WRITES = [
  // 新增家宽节点：新后台开放上架流水线（WRITES=1）时跳到上架流水线并打开新建批次（不发请求），否则提示
  { key: 'dashboard.enroll', prep: "location.hash = '#/dashboard'", sel: '[data-act="enroll"]', wired: 'onboard' },
  { key: 'nodes.add', prep: "location.hash = '#/nodes'", sel: '[data-act="add"]', wired: 'onboard' },
  { key: 'lines.add-relay', prep: "location.hash = '#/lines'", sel: '[data-act="add-relay"]' },
  { key: 'lines.entry.disable', prep: "location.hash = '#/lines'", sel: '[data-act="disable"]' },
  { key: 'lines.entry.reset', prep: "location.hash = '#/lines'", sel: '[data-act="reset"]' },
  { key: 'lines.relay.restart', prep: "location.hash = '#/lines'", sel: '[data-act="restart"]' },
  { key: 'lines.relay.logs', prep: "location.hash = '#/lines'", sel: '[data-act="logs"]' },
  { key: 'drawer.rotate', prep: "location.hash = '#/home'; setTimeout(() => __app.openNode(__app.store.nodes().find((n) => !n.isRelay && n.relay).id, 'manage'), 300)", sel: '.drawer [data-dz="rotate"]' },
  { key: 'drawer.verify', prep: '', sel: '.drawer [data-dz="verify"]' },
  { key: 'drawer.renew', prep: "__app.setDrawerParams(__app.drawerNodeId(), 'billing', { replace: true })", sel: '.drawer [data-dz="renew"]', wired: true },
  // 行为变化列表可能为空（最近没有未查看的事件）：找不到按钮就跳过，不算失败
  { key: 'behavior.event.ack', prep: "location.hash = '#/behavior?view=events'", sel: '[data-act="ack"]', optional: true, wired: 'direct' },
  { key: 'onboarding.new', prep: "location.hash = '#/onboarding'", sel: '[data-act="new"]', wired: 'onboard' },
];
async function writeChecks(b, shotFile) {
  const out = [];
  for (const w of WRITES) {
    await b.eval(`document.querySelectorAll('.toasts .toast').forEach((t) => t.remove()); 0`);
    if (w.prep) { await b.eval(`${w.prep}; 0`); await sleep(1500); }
    const found = await b.until(`!!document.querySelector(${JSON.stringify(w.sel)})`, 6000);
    if (!found && w.optional) { out.push({ key: w.key, ok: true, skipped: 'list empty' }); continue; }
    if (!found) { out.push({ key: w.key, ok: false, why: 'button not found' }); continue; }
    const apiBefore = b.ev.api.length;
    await b.eval(`document.querySelector(${JSON.stringify(w.sel)}).click(); 0`);
    await sleep(500);
    const r = await b.eval(js(() => ({
      toast: [...document.querySelectorAll('.toasts .toast .toast-msg')].map((t) => t.textContent).pop() ?? null,
      link: !!document.querySelector('.toasts .toast [data-t="act"]'),
      dialog: !!document.querySelector('.dialog'),
    })));
    const writes = b.ev.api.slice(apiBefore).filter((x) => x.method !== 'GET').length;
    if (w.wired === 'onboard' && (await b.eval("globalThis.TEAMOPS_WRITES === true && !!globalThis.TEAMOPS?.fleet?.has?.('onboard')"))) {
      // the live pipeline's new-batch dialog opens on #/onboarding; nothing is sent until “生成计划”
      await b.until("!!document.querySelector('.dialog')", 4000);
      const at = await b.eval('location.hash');
      const dlg = await b.eval("document.querySelector('.dialog .dialog-title')?.textContent ?? null");
      out.push({ key: w.key, ok: dlg === '新建批次' && /^#\/onboarding/.test(at) && writes === 0, dialog: dlg, at, writes, wired: true });
      await b.eval(`document.querySelector('.dialog [data-dlg="cancel"]')?.click(); 0`); await sleep(400);
      continue;
    }
    // WRITES=1 builds: wired class (a) actions open their real form instead of a notice (still nothing sent until submit)
    if (w.wired === 'direct' && (await b.eval('globalThis.TEAMOPS_WRITES === true'))) {
      // WRITES=1: an L0 class (a) action (标记已查看) really writes: exactly one POST, no error toast
      out.push({ key: w.key, ok: writes === 1 && !/^操作失败/.test(r.toast ?? '') && !r.dialog, toast: r.toast, writes, wired: true });
      continue;
    }
    if (w.wired === true && (await b.eval('globalThis.TEAMOPS_WRITES === true'))) {
      out.push({ key: w.key, ok: r.dialog && writes === 0, dialog: r.dialog, writes, wired: true });
      await b.eval(`document.querySelector('.dialog [data-dlg="cancel"]')?.click(); 0`); await sleep(300);
      await b.eval(`document.querySelector('.dialog [data-dlg="cancel"], .dialog .btn-danger')?.click(); 0`); await sleep(300);
      continue;
    }
    // class a (not wired / writes off): read-only notice + 打开原版; class b (relay ops): “新后台尚未开放” or why there is
    // no such operation any more (verify / logs / add relay), no link back
    const moved = /新后台尚未开放|这个构建没有开启写操作|新后台每分钟自动验证|新后台暂不提供|3x-ui 面板已退役|不走网页/;
    const noticeOk = (/^只读预览：请在原版后台操作/.test(r.toast ?? '') && r.link) || (moved.test(r.toast ?? '') && !r.link);
    const ok = noticeOk && !r.dialog && writes === 0;
    out.push({ key: w.key, ok, toast: r.toast, dialog: r.dialog, writes });
    if (w.key === 'lines.entry.disable' && shotFile) await b.shot(shotFile);
    if (r.dialog) await b.eval(`document.querySelector('.dialog [data-dlg="cancel"]')?.click(); 0`);
  }
  await b.eval(`__app.closeDrawerParams?.(); document.querySelectorAll('.toasts .toast').forEach((t) => t.remove()); 0`);
  return out;
}
const probe = js(() => {
  const a = globalThis.__app;
  const s = a?.store;
  const T = globalThis.TEAMOPS;
  const txt = document.getElementById('content')?.innerText ?? '';
  const fresh = document.querySelector('[data-fresh]');
  return {
    ready: document.documentElement.dataset.ready === '1',
    live: globalThis.TEAMOPS_LIVE === true && !!T?.poller,
    source: s?.raw?.provenance?.source ?? null,
    nodes: s?.nodes?.().length ?? null,
    asOf: s?.raw?.as_of_text ?? null,
    kpi: [...document.querySelectorAll('.kpi')].slice(0, 4).map((k) => k.innerText.replace(/\s+/g, ' ').trim().slice(0, 60)),
    fresh: fresh ? `${fresh.dataset.state} · ${fresh.innerText.trim()}` : null,
    status: T?.poller?.status?.() ?? null,
    csp: globalThis.__csp ?? [],
    hasDemoWord: /原型|演示/.test(txt),
    badge: document.querySelector('.topbar .live-badge')?.innerText.trim() ?? null,
    writes: globalThis.TEAMOPS_WRITES === true,
    overflowX: document.documentElement.scrollWidth > document.documentElement.clientWidth + 1 || (() => { const t = document.querySelector('.topbar'); return !!t && t.scrollWidth > t.clientWidth + 1; })(),
    title: document.title,
  };
});

async function run() {
  fs.mkdirSync(OUT, { recursive: true });
  const report = [];
  const firstVisit = {};
  for (const mode of MODES) {
    const b = await launch();
    await b.send('Emulation.setDeviceMetricsOverride', { width: mode.width, height: mode.height, deviceScaleFactor: 1, mobile: !!mode.mobile });
    await b.send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: mode.dark ? 'dark' : 'light' }, { name: 'prefers-reduced-motion', value: 'reduce' }] });
    await b.send('Page.navigate', { url: `${BASE}/` });
    const t0 = Date.now();
    const ok = await b.until('document.documentElement.dataset.ready === "1"', 30000);
    const readyMs = Date.now() - t0;
    // first visit budget: API requests until the first poll tick (10 s)
    await sleep(Math.max(0, 9000 - (Date.now() - t0)));
    firstVisit[mode.name] = { readyMs: ok ? readyMs : null, api9s: b.ev.api.length, peak: b.ev.peak, byPath: countPaths(b.ev.api) };
    for (const r of ROUTES) {
      if (ONLY && !ONLY.split(',').includes(r.name)) continue;
      b.reset();
      if (r.drawer) {
        const id = await b.eval('__app.store.nodes().find((n) => n.hasHost)?.id');
        await b.eval(`location.hash = '#/home'`); await sleep(800);
        await b.eval(`__app.openNode(${JSON.stringify(id)}, 'overview')`);
      } else {
        await b.eval(`location.hash = ${JSON.stringify(r.hash)}`);
      }
      await b.until(`!!document.querySelector(${JSON.stringify(r.ready)})`, 10000);
      await sleep(r.drawer ? 3500 : 2500);
      const p = await b.eval(probe);
      const file = path.join(OUT, `${r.name}-${mode.name}.png`);
      await b.shot(file);
      report.push({ mode: mode.name, route: r.name, ...p, console: [...b.ev.console], exceptions: [...b.ev.exceptions], cspLog: [...b.ev.csp], failed: [...b.ev.failed], shot: path.relative(process.cwd(), file) });
    }
    // write actions → read-only notice (every mode)
    b.reset();
    const writes = await writeChecks(b, path.join(OUT, `notice-${mode.name}.png`));
    firstVisit[mode.name].writeChecks = writes;
    firstVisit[mode.name].writeErrors = [...b.ev.console, ...b.ev.exceptions, ...b.ev.failed, ...b.ev.csp];
    // one poll tick later: freshness and status on the last page
    b.reset();
    await b.eval(`location.hash = '#/dashboard'`);
    const rankH = [];
    for (let i = 0; i < 12; i++) { await sleep(1000); rankH.push(await b.eval(`Math.round(document.querySelector('[data-panel="rank"]')?.getBoundingClientRect().height ?? -1)`)); }
    // 模拟“有速率的节点从 5 台变成 2 台”（生产上 3↔5 来回变）：面板高度必须不变
    await b.eval(`(() => { const s = __app.store; const d = {}; for (const n of s.nodes().filter((x) => (x.rate?.down_bps ?? 0) > 0).slice(2)) { n.rate.down_bps = 0; d[n.id] = { 'rate.down_bps': 0 }; } s.emit('change', { nodes: d, relays: {}, entries: {}, fleet: {}, events: [] }); return Object.keys(d).length; })()`);
    for (let i = 0; i < 7; i++) { await sleep(1000); rankH.push(await b.eval(`Math.round(document.querySelector('[data-panel="rank"]')?.getBoundingClientRect().height ?? -1)`)); }
    const rankRows = await b.eval(`document.querySelectorAll('[data-list="rank"] tr[data-row]').length`);
    const after = await b.eval(probe);
    firstVisit[mode.name].rank = { rows: rankRows, heights: [...new Set(rankH)] };
    firstVisit[mode.name].after = { fresh: after.fresh, status: after.status, apiTotal: b.ev.api.length, peak: b.ev.peak, errors: b.ev.console.length + b.ev.exceptions.length + b.ev.failed.length + b.ev.csp.length };
    // non-GET outside the deliberate write clicks (WRITES=1 builds send exactly those) must be zero
    firstVisit[mode.name].writes = b.ev.api.filter((x) => x.method !== 'GET').length - writes.filter((x) => x.wired && x.writes).reduce((n, x) => n + x.writes, 0);
    await b.close();
  }
  return { report, firstVisit };
}

function countPaths(api) {
  const out = {};
  for (const a of api) {
    const u = new URL(a.url);
    const k = u.pathname.replace('/api/team-ops', '').replace('/v2/api/legacy', '').replace('/v2/api/v1', 'v1:').replace(/\/[0-9a-f-]{8,}(?=\/|$)/g, '/:id') + (u.searchParams.has('asset') ? '?asset=…' + (u.searchParams.get('hours') === '24' ? '&hours=24' : u.searchParams.has('hours') ? '&hours=month' : '') : '');
    out[k] = (out[k] ?? 0) + 1;
  }
  return out;
}

const { report, firstVisit } = await run();
let bad = 0;
for (const r of report) {
  const problems = [...r.exceptions.map((x) => 'EXC ' + x), ...r.console, ...r.failed.map((x) => 'FAIL ' + x), ...r.cspLog.map((x) => 'CSP ' + x), ...r.csp.map((x) => 'CSP ' + x)];
  if (r.overflowX) problems.push('horizontal overflow');
  bad += problems.length;
  if (r.hasDemoWord) problems.push('prototype / demo wording visible');
  const okBadge = r.writes ? ['实时数据', '实时'] : ['只读预览 · 实时数据', '只读'];
  if (!(r.badge === okBadge[0] || (r.mode.endsWith('390') && r.badge === okBadge[1]))) problems.push(`badge: ${r.badge}`);
  bad += r.hasDemoWord || problems.some((x) => x.startsWith('badge')) ? 1 : 0;
  console.log(`${r.mode.padEnd(11)} ${r.route.padEnd(11)} nodes=${r.nodes} source=${r.source} live=${r.live} fresh="${r.fresh}" ${problems.length ? '✗ ' + problems.length : '✓'}`);
  for (const x of problems.slice(0, 8)) console.log('      ' + x.slice(0, 220));
}
console.log('\nfirst visit (fresh profile, until the first poll tick):');
for (const [m, v] of Object.entries(firstVisit)) {
  console.log(`  ${m}: ready ${v.readyMs} ms · ${v.api9s} API requests · peak ${v.peak} in flight · non-GET ${v.writes} · after 1 tick: ${JSON.stringify(v.after)}\n    ${JSON.stringify(v.byPath)}`);
  const failedW = v.writeChecks.filter((x) => !x.ok);
  bad += failedW.length + v.writeErrors.length + (v.writes ? 1 : 0) + (v.after.errors ? 1 : 0);
  bad += v.rank.heights.length !== 1 ? 1 : 0;
  console.log(`    rank panel: ${v.rank.rows} rows · heights over 12 s ${JSON.stringify(v.rank.heights)}`);
  console.log(`    write actions → notice: ${v.writeChecks.length - failedW.length}/${v.writeChecks.length}${v.writeChecks.some((x) => x.skipped) ? ` (skipped: ${v.writeChecks.filter((x) => x.skipped).map((x) => x.key).join(', ')})` : ''}${failedW.length ? ' ✗ ' + JSON.stringify(failedW) : ''}${v.writeErrors.length ? ' errors: ' + v.writeErrors.join(' | ').slice(0, 400) : ''}`);
}
const cspTotal = report.reduce((a, r) => a + r.csp.length + r.cspLog.length, 0);
console.log(`\nCSP violations: ${cspTotal} · problems: ${bad} · screenshots: ${OUT}`);
fs.writeFileSync(path.join(OUT, 'smoke.json'), JSON.stringify({ base: BASE, report, firstVisit }, null, 2));
process.exit(bad ? 1 : 0);
