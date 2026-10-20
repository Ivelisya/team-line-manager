// verify-skeleton.mjs — 骨架 → 真实页面的位移（craft #6）：拖住 snapshot.json 1.5 秒，量骨架里“更新于”、KPI、第一块内容的位置，
// 数据到达后再量一次。用法：SHOOT_PORT=9735 ~/.local/bin/node tools/fix/verify-skeleton.mjs
import path from 'node:path';
import { launch } from '../shoot.mjs';

const ROOT = path.dirname(path.dirname(path.dirname(new URL(import.meta.url).pathname)));
const b = await launch({ port: Number(process.env.SHOOT_PORT) || 9735 });
const BASE = 'http://127.0.0.1:5199/original-refined/index.html';
const probe = `(() => {
  const r = (el) => el ? Math.round(el.getBoundingClientRect().left) + ',' + Math.round(el.getBoundingClientRect().top) : '-';
  const page = document.querySelector('#content .page');
  const kids = [...page.children].filter((x) => !x.classList.contains('page-sentinel') && !x.classList.contains('page-head') && x.getBoundingClientRect().height > 0);
  const panels = [...document.querySelectorAll('#content .panel')];
  return { fresh: r(document.querySelector('.page-head .fresh')), tools: r(document.querySelector('.page-head .page-tools')), first: r(kids[0]), second: r(kids[1]), kpi: r(document.querySelector('#content .kpi-grid')), panel1: r(panels[0]), panel2: r(panels[1]), panel3: r(panels[2]) };
})()`;
const out = [];
for (const page of ['home', 'dashboard', 'nodes', 'lines', 'behavior', 'onboarding']) {
  await b.viewport(1440, 900);
  await b.media({ reduce: true });
  await b.go('about:blank');
  await b.go(`${BASE}#/${page}`);
  await b.until('document.readyState === "complete"');
  await b.eval(`(() => { try { localStorage.clear(); } catch {} return 1; })()`);
  await b.send('Fetch.enable', { patterns: [{ urlPattern: '*snapshot.json*' }] });
  b.onPaused = (p) => setTimeout(() => b.send('Fetch.continueRequest', { requestId: p.requestId }).catch(() => {}), 1500);
  await b.send('Page.reload', { ignoreCache: true });
  await b.sleep(700);
  const skel = await b.eval(probe);
  await b.shot(path.join(ROOT, 'shots', 'fix', `skeleton-${page}.png`));
  await b.until('document.documentElement.dataset.ready === "1"', 20000);
  await b.sleep(1200);
  await b.send('Fetch.disable'); b.onPaused = null;
  const real = await b.eval(probe);
  const diff = Object.keys(skel).filter((k) => skel[k] !== real[k] && skel[k] !== '-').map((k) => `${k} ${skel[k]} → ${real[k]}`);
  out.push(`${diff.length ? 'MOVED' : 'SAME '}  ${page}: ${diff.join(' · ') || `更新于 ${real.fresh}，KPI ${real.kpi}，面板 ${real.panel1} / ${real.panel2} / ${real.panel3}`}`);
}
console.log(out.join('\n'));
await b.close();
