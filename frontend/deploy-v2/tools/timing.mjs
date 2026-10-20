// timing.mjs — page-open times, cold (empty browser cache) and warm (reopen with the boot cache), per page:
//   first data = the page shows real rows / cards (not a skeleton);  fresh = the lists loaded in this visit are applied.
// Target (owner): first data < 300 ms from cache, fresh < 1 s.  Local / staging only (through the tunnel).
//   node deploy/v2/tools/timing.mjs [--base http://127.0.0.1:18398]
import { launch, sleep } from './cdp.mjs';

const arg = (k, d) => { const i = process.argv.indexOf(k); return i > 0 ? process.argv[i + 1] : d; };
const BASE = arg('--base', 'http://127.0.0.1:18398');
if (!/^http:\/\/127\.0\.0\.1/.test(BASE)) { console.error('local tunnel only'); process.exit(2); }
const PAGES = [
  { name: '首页', hash: '#/home', ready: '.ncard .ncard-head' },
  { name: '运营总览', hash: '#/dashboard', ready: '.kpi .kpi-value .num, .kpi-value' },
  { name: '节点', hash: '#/nodes', ready: '.dt tbody tr[data-row]' },
  { name: '线路', hash: '#/lines', ready: '.relay .dt tbody tr, [data-relay]' },
  { name: '节点行为', hash: '#/behavior', ready: '[data-table] tbody tr[data-row], [data-table] .empty' },
  { name: '事项与告警', hash: '#/alerts', ready: '[data-page="alerts"] table tbody tr, [data-page="alerts"] .empty' },
];
const noSkel = (sel) => `(() => { const c = document.getElementById('content'); return !!c && !!c.querySelector(${JSON.stringify(sel)}) && !c.querySelector('.page .skel, .page .tbl-skel'); })()`;
const fresh = "globalThis.TEAMOPS?.source && !globalThis.TEAMOPS.source.bootedFromCache && globalThis.TEAMOPS.source.fastAt != null";

async function measure(b, url, sel) {
  const t0 = Date.now();
  await b.send('Page.navigate', { url });
  let first = null, fr = null;
  for (let i = 0; i < 400 && (first == null || fr == null); i++) {
    await sleep(25);
    try {
      if (first == null && await b.eval(noSkel(sel))) first = Date.now() - t0;
      if (fr == null && first != null && await b.eval(fresh)) fr = Date.now() - t0;
    } catch { /* navigating */ }
  }
  const marks = await b.eval("Object.fromEntries(performance.getEntriesByType('mark').map((x) => [x.name.replace('teamops:', ''), Math.round(x.startTime)]))").catch(() => ({}));
  // in-app: from the app's first line running (modules loaded) to first data on screen
  return { first, fresh: fr, cache: await b.eval('!!globalThis.TEAMOPS?.fromCache').catch(() => null), app: first != null && marks.boot != null ? first - marks.boot : null, modules: marks.boot ?? null };
}

const b = await launch();
await b.send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
const rows = [];
for (const p of PAGES) {
  // cold: no boot cache for this origin
  await b.send('Page.navigate', { url: `${BASE}/#/home` }); await sleep(300);
  await b.eval("localStorage.removeItem('teamops:live:boot:v1'); localStorage.removeItem('teamops:live:system:v1'); 0").catch(() => {});
  await b.send('Page.navigate', { url: 'about:blank' }); await sleep(200);
  const cold = await measure(b, `${BASE}/${p.hash}`, p.ready);
  await sleep(4500); // let the debounced boot-cache save happen
  await b.send('Page.navigate', { url: 'about:blank' }); await sleep(200);
  const warm = await measure(b, `${BASE}/${p.hash}`, p.ready);
  rows.push({ page: p.name, cold, warm });
  console.log(`${p.name.padEnd(6)} cold: first ${cold.first ?? '—'} ms · fresh ${cold.fresh ?? '—'} ms   warm: first ${warm.first ?? '—'} ms (cache ${warm.cache}; modules ${warm.modules} ms, in-app ${warm.app} ms) · fresh ${warm.fresh ?? '—'} ms`);
}
await b.close();
