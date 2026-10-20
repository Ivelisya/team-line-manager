// palette-shots.mjs — before (indigo) / after (粉紫) screenshots of the same build, light + dark, 1440 and 390, plus one
// side-by-side overview.  node deploy/v2/tools/palette-shots.mjs [--base http://127.0.0.1:18471] [--out shots/palette]
import fs from 'node:fs';
import path from 'node:path';
import { launch, sleep } from './cdp.mjs';
const arg = (k, d) => { const i = process.argv.indexOf(k); return i > 0 ? process.argv[i + 1] : d; };
const BASE = arg('--base', 'http://127.0.0.1:18471');
const OUT = path.resolve(arg('--out', 'shots/palette'));
fs.mkdirSync(OUT, { recursive: true });
const VIEWS = [
  { name: 'home', hash: '#/home', ready: '.ncard .ncard-head' },
  { name: 'dashboard', hash: '#/dashboard', ready: '.kpi' },
  { name: 'nodes', hash: '#/nodes', ready: '.dt tbody tr[data-row]' },
  { name: 'drawer', hash: '#/nodes', ready: '.drawer .dz-card', drawer: true },
  { name: 'lines', hash: '#/lines', ready: '.relay, [data-relay]' },
  { name: 'alerts', hash: '#/alerts', ready: '[data-page="alerts"] .panel' },
];
const MODES = [{ w: 1440, h: 900, tag: '1440' }, { w: 390, h: 844, tag: '390', mobile: true }];
const shots = [];
for (const pal of ['indigo', 'plum']) {
  for (const dark of [false, true]) {
    for (const m of MODES) {
      const b = await launch();
      await b.send('Emulation.setDeviceMetricsOverride', { width: m.w, height: m.h, deviceScaleFactor: m.mobile ? 2 : 1, mobile: !!m.mobile });
      await b.send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: dark ? 'dark' : 'light' }, { name: 'prefers-reduced-motion', value: 'reduce' }] });
      await b.send('Page.navigate', { url: `${BASE}/#/home` });
      await b.until("document.documentElement.dataset.ready === '1'", 30000);
      await b.eval(`localStorage.setItem('refined:palette', JSON.stringify('${pal}')); localStorage.setItem('refined:theme', JSON.stringify('${dark ? 'dark' : 'light'}')); 0`);
      for (const v of VIEWS) {
        await b.send('Page.navigate', { url: `${BASE}/${v.hash}` });
        await b.until("document.documentElement.dataset.ready === '1'", 30000);
        await b.until(`!!document.querySelector(${JSON.stringify(v.drawer ? '.dt tbody tr[data-row]' : v.ready)})`, 15000);
        if (v.drawer) { await b.eval("__app.openNode(__app.store.nodes().find((n) => n.hasHost && !n.isRelay).id, 'overview'); 0"); await b.until(`!!document.querySelector('.drawer .dz-card')`, 10000); }
        await sleep(2500);
        const file = path.join(OUT, `${v.name}-${pal}-${dark ? 'dark' : 'light'}-${m.tag}.png`);
        await b.shot(file);
        shots.push({ view: v.name, pal, dark, m: m.tag, file });
      }
      await b.close();
    }
  }
}
// side-by-side overview: indigo | 粉紫 for each view at 1440, light and dark
const rows = [];
for (const dark of [false, true]) for (const v of VIEWS) {
  const a = shots.find((s) => s.view === v.name && s.pal === 'indigo' && s.dark === dark && s.m === '1440');
  const z = shots.find((s) => s.view === v.name && s.pal === 'plum' && s.dark === dark && s.m === '1440');
  rows.push(`<tr><th>${v.name} · ${dark ? '暗' : '亮'}</th><td><img src="file://${a.file}"></td><td><img src="file://${z.file}"></td></tr>`);
}
const html = `<!doctype html><meta charset=utf-8><style>body{margin:0;background:#fff;font:14px -apple-system,sans-serif}table{border-collapse:collapse}th{writing-mode:vertical-rl;padding:4px;color:#444}td{padding:6px}img{width:720px;display:block;border:1px solid #ddd}caption{font-size:18px;padding:10px;text-align:left}</style><table><caption>靛蓝（原版）｜ 粉紫（新默认）</caption>${rows.join('')}</table>`;
const hf = path.join(OUT, 'overview.html');
fs.writeFileSync(hf, html);
const b = await launch();
await b.send('Emulation.setDeviceMetricsOverride', { width: 1500, height: 900, deviceScaleFactor: 1, mobile: false });
await b.send('Page.navigate', { url: `file://${hf}` });
await sleep(3000);
const h = await b.eval('document.body.scrollHeight');
await b.send('Emulation.setDeviceMetricsOverride', { width: 1500, height: h, deviceScaleFactor: 1, mobile: false });
await sleep(800);
await b.shot(path.join(OUT, 'overview.png'));
await b.close();
console.log(`${shots.length} screenshots + overview.png in ${OUT}`);
