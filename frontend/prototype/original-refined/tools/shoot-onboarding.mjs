// shoot-onboarding.mjs — 上架流水线的截图与运行时检查（无头 Chrome，只访问 127.0.0.1:5199）。
// 用法：~/.local/bin/node tools/shoot-onboarding.mjs [jobs.json] [--only name,name]
// CDP 端口在 9600–9699 里随机挑一个没人监听的（别的构建者也在跑无头 Chrome，固定端口会连到别人的浏览器）。job 字段同 shoot.mjs：
//   { name, url, width, height, dark, full, wait, before(页面内 JS，可 await), afterWait, clip, density }
import fs from 'node:fs';
import path from 'node:path';
import { launch } from './shoot.mjs';

const ROOT = path.dirname(path.dirname(new URL(import.meta.url).pathname));
const BASE = 'http://127.0.0.1:5199/original-refined/index.html';
const args = process.argv.slice(2);
const file = args.find((a) => a.endsWith('.json')) ?? path.join(ROOT, 'tools/jobs-onboarding.json');
const only = (args[args.indexOf('--only') + 1] ?? '').split(',').filter(Boolean);
let jobs = JSON.parse(fs.readFileSync(file, 'utf8'));
if (args.includes('--only')) jobs = jobs.filter((j) => only.includes(j.name));

async function freePort() {
  for (let i = 0; i < 40; i++) {
    const port = 9600 + Math.floor(Math.random() * 100);
    try { await fetch(`http://127.0.0.1:${port}/json/version`, { signal: AbortSignal.timeout(300) }); } catch { return port; }
  }
  throw new Error('没有空闲的 CDP 端口');
}
export { freePort };
const b = await launch({ port: await freePort() });
const outDir = path.join(ROOT, 'shots');
for (const j of jobs) {
  b.logs.length = 0;
  await b.viewport(j.width ?? 1440, j.height ?? 900, j.dpr ?? 1);
  await b.media({ dark: !!j.dark, reduce: j.reduce !== false });
  await b.go('about:blank');
  await b.go(`${BASE}${j.url ?? '#/onboarding'}`);
  await b.until('document.readyState === "complete"');
  await b.eval(`(() => { try { localStorage.setItem('refined:theme', JSON.stringify(${JSON.stringify(j.theme ?? 'system')})); localStorage.setItem('refined:density', JSON.stringify(${JSON.stringify(j.density ?? 'comfortable')})); localStorage.removeItem('refined:nav'); } catch (e) {} return 1; })()`);
  if (j.delaySnapshot) {
    // 首屏骨架：拖住 snapshot.json 的响应，先截骨架
    await b.send('Fetch.enable', { patterns: [{ urlPattern: '*snapshot.json*' }] });
    b.onPaused = (p) => setTimeout(() => b.send('Fetch.continueRequest', { requestId: p.requestId }).catch(() => {}), j.delaySnapshot);
    await b.send('Page.reload', { ignoreCache: true });
    await b.sleep(900);
    await b.shot(path.join(outDir, `${j.name}.png`));
    console.log(`${j.name}: 骨架截图`);
    await b.sleep(j.delaySnapshot);
    await b.send('Fetch.disable');
    b.onPaused = null;
    continue;
  }
  await b.send('Page.reload', { ignoreCache: false });
  await b.sleep(300);
  await b.until('document.documentElement.dataset.ready === "1"', 20000);
  await b.sleep(j.wait ?? 1200);
  if (j.before) { await b.eval(`(async () => { ${j.before} })()`); await b.sleep(j.afterWait ?? 800); }
  const out = path.join(outDir, `${j.name}.png`);
  await b.shot(out, { full: !!j.full, clip: j.clip });
  const m = await b.eval('({ sw: document.documentElement.scrollWidth, cw: document.documentElement.clientWidth, h: document.documentElement.scrollHeight })');
  console.log(`${j.name}: scrollWidth ${m.sw}/${m.cw} height ${m.h}${b.logs.length ? `\n  ${b.logs.join('\n  ')}` : ''}`);
}
await b.close();
process.exit(0);
