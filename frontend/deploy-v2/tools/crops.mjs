// crops.mjs — 3× element crops for type/alignment review (headless Chrome, local or staging only).
//   node tools/crops.mjs <base-url-of-index.html> <out-prefix>   e.g. http://127.0.0.1:5199/original-refined/index.html shots/align/before
import { launch } from '../../../prototype/original-refined/tools/shoot.mjs';
import path from 'node:path';
const [, , base, prefix] = process.argv;
if (!/^http:\/\/127\.0\.0\.1/.test(base)) { console.error('local URLs only'); process.exit(2); }
const ONLY = process.argv[4] ? new Set(process.argv[4].split(',')) : null;
const JOBS = [
  // 名称 + 计数（分段、页签、筛选 chip）：基线、字体、间距
  { name: 'seg-home', hash: '#/home', sel: '[data-seg="team"], .seg' },
  { name: 'fchips', hash: '#/home', sel: '.fbar-chips' },
  { name: 'seg-nodes', hash: '#/nodes', sel: '.toolbar .seg' },
  { name: 'tabs-behavior', hash: '#/behavior', sel: '.tabs' },
  { name: 'seg-alerts', hash: '#/alerts', sel: '[data-page="alerts"] .seg' },
  { name: 'relay-card', hash: '#/home?team=%E7%BA%BF%E8%B7%AF%E6%9C%BA', sel: '.ncard', h: 760 },
  { name: 'card-title', hash: '#/home', sel: '.ncard .ncard-head' },
  { name: 'card', hash: '#/home', sel: '.ncard', h: 760 },
  { name: 'kpi', hash: '#/home', sel: '.kpi-grid' },
  { name: 'table', hash: '#/nodes', sel: '.dt tbody tr:nth-child(2)' },
  { name: 'lines-head', hash: '#/lines', sel: '.relay-head' },
  { name: 'drawer', hash: '#/home', drawer: true, sel: '.drawer .drawer-head' },
  { name: 'drawer-body', hash: '#/home', drawer: true, sel: '.drawer .dz-card' },
];
const b = await launch({ port: 9480 + Math.floor(Math.random() * 100) });
for (const dark of [false, true]) {
  await b.viewport(1440, 1600, 3);
  await b.media({ dark });
  await b.go(base + '#/home');
  await b.until('document.documentElement.dataset.ready === "1"', 30000);
  for (const j of JOBS.filter((x) => !ONLY || ONLY.has(x.name))) {
    await b.eval(`__app.closeDrawerParams?.(); location.hash = ${JSON.stringify(j.hash)}; 0`);
    await b.sleep(1200);
    if (j.drawer) { await b.eval(`__app.openNode(__app.store.nodes().find((n) => n.hasHost && !n.isRelay).id); 0`); await b.sleep(1500); }
    const r = await b.eval(`(() => { const e = ${JSON.stringify(j.sel)}.split(', ').map((s) => document.querySelector(s)).find(Boolean); if (!e) return null; const q0 = e.getBoundingClientRect(); if (q0.bottom > innerHeight || q0.top < 0) e.scrollIntoView({ block: 'center' }); const q = e.getBoundingClientRect(); return { x: q.x, y: q.y, width: Math.min(q.width, 720), height: Math.min(q.height, 400) }; })()`);
    if (!r) { console.log('missing', j.name); continue; }
    const q = await b.eval(`(() => { const e = ${JSON.stringify(j.sel)}.split(', ').map((s) => document.querySelector(s)).find(Boolean); const q = e.getBoundingClientRect(); return { x: q.x, y: q.y, width: Math.min(q.width, 720), height: Math.min(q.height, ${j.h ?? 400}) }; })()`);
    await b.shot(`${prefix}-${j.name}-${dark ? 'dark' : 'light'}.png`, { clip: q });
  }
}
await b.close();
process.exit(0);
