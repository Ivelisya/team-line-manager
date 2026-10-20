// 无头浏览器截图 + 卡片分段对齐检查。用法：先 VITE_MOCK=1 pnpm dev，再 node tools/shoot.mjs [场景名过滤]
// （TRACE_WAIT=35000 让首页大卡片场景多等几轮数据更新，截到有内容的实时迹线；BASE 换端口）
// 输出到 shots/；对齐检查把每个 .cards 网格里同一行的卡片逐槽位比较“槽位顶边相对卡片顶边的偏移”，必须完全一致。
import puppeteer from 'puppeteer-core';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const out = path.join(here, '..', 'shots');
fs.mkdirSync(out, { recursive: true });
const BASE = process.env.BASE ?? 'http://127.0.0.1:5173';
const EXE = process.env.CHROME ?? ['C:/Program Files/Google/Chrome/Application/chrome.exe', 'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe'].find((p) => fs.existsSync(p));
const filter = process.argv[2];

// layout：首页视图（cards / table / ring）；view：卡片密度。wait：多等几轮 mock 的 15 秒抖动，让实时迹线攒出几个点
const TRACE = Number(process.env.TRACE_WAIT ?? 0);
const scenes = [
  { name: 'home-large-1440', url: '/', w: 1440, view: 'large', align: true, wait: TRACE },
  { name: 'home-small-1440', url: '/', w: 1440, view: 'small', align: true },
  { name: 'home-compact-1440', url: '/', w: 1440, view: 'compact', align: true },
  { name: 'home-large-1440-dark', url: '/', w: 1440, view: 'large', theme: 'dark', align: true, wait: TRACE },
  { name: 'home-table-1440', url: '/', w: 1440, layout: 'table' },
  { name: 'home-table-1440-dark', url: '/', w: 1440, layout: 'table', theme: 'dark' },
  { name: 'home-ring-1440', url: '/', w: 1440, layout: 'ring' },
  { name: 'home-ring-1440-dark', url: '/', w: 1440, layout: 'ring', theme: 'dark' },
  { name: 'home-ring-1650', url: '/', w: 1650, layout: 'ring' },
  { name: 'home-large-390', url: '/', w: 390, view: 'large', align: true },
  { name: 'home-large-390-dark', url: '/', w: 390, view: 'large', theme: 'dark' },
  { name: 'home-small-390', url: '/', w: 390, view: 'small' },
  { name: 'home-compact-390', url: '/', w: 390, view: 'compact' },
  { name: 'home-table-390', url: '/', w: 390, layout: 'table' },
  { name: 'home-ring-390', url: '/', w: 390, layout: 'ring' },
  { name: 'home-ring-390-dark', url: '/', w: 390, layout: 'ring', theme: 'dark' },
  { name: 'nodes-1440', url: '/nodes', w: 1440 },
  { name: 'nodes-drawer-1440', url: '/nodes?m=m-t1-05', w: 1440 },
  { name: 'nodes-drawer-manage-1440', url: '/nodes?m=m-t1-03&tab=manage', w: 1440 },
  { name: 'nodes-drawer-billing-1440', url: '/nodes?m=m-t1-03&tab=billing', w: 1440 },
  { name: 'nodes-drawer-host-1440', url: '/nodes?m=m-t1-02&tab=host', w: 1440 },
  { name: 'nodes-drawer-host-lxc-1440', url: '/nodes?m=m-t2-06&tab=host', w: 1440 },
  { name: 'nodes-390', url: '/nodes', w: 390 },
  { name: 'nodes-retired-1440', url: '/nodes?scope=retired', w: 1440, retired: true },
  { name: 'nodes-drawer-retired-1440', url: '/nodes?m=m-t1-3r', w: 1440 },
  { name: 'jobs-done-1440', url: '/jobs/j-done2', w: 1440 },
  { name: 'nodes-drawer-390', url: '/nodes?m=m-t1-05', w: 390 },
  { name: 'relays-1440', url: '/relays', w: 1440 },
  { name: 'provision-1440', url: '/provision', w: 1440 },
  { name: 'provision-batch-1440', url: '/provision', w: 1440, batch: true },
  { name: 'provision-390', url: '/provision', w: 390 },
  { name: 'jobs-1440', url: '/jobs/j-run', w: 1440 },
  { name: 'jobs-390', url: '/jobs/j-run', w: 390 },
  { name: 'access-1440', url: '/access', w: 1440 },
  { name: 'access-events-1440', url: '/access?view=events', w: 1440 },
  { name: 'access-drawer-1440', url: '/access?m=m-t1-05&tab=access', w: 1440 },
  { name: 'access-390', url: '/access', w: 390 },
  { name: 'catalog-1440', url: '/catalog', w: 1440 },
  { name: 'catalog-providers-1440', url: '/catalog?section=providers', w: 1440 },
  { name: 'catalog-plans-1440', url: '/catalog?section=plans', w: 1440 },
  { name: 'incidents-1440', url: '/incidents', w: 1440 },
  { name: 'login-1440', url: '/login?logout=1', w: 1440 },
];

// 实时模式：SESSION 环境变量给会话 Cookie（不经页面登录），场景换成真实数据能打开的页面
const SESSION = process.env.SESSION;
const liveScenes = [
  { name: 'live-home-large-1440', url: '/', w: 1440, view: 'large', align: true },
  { name: 'live-home-small-1440', url: '/', w: 1440, view: 'small', align: true },
  { name: 'live-home-compact-1440', url: '/', w: 1440, view: 'compact', align: true },
  { name: 'live-home-large-1440-dark', url: '/', w: 1440, view: 'large', theme: 'dark', align: true },
  { name: 'live-home-large-390', url: '/', w: 390, view: 'large', align: true },
  { name: 'live-nodes-1440', url: '/nodes', w: 1440 },
  { name: 'live-nodes-retired-1440', url: '/nodes?scope=retired', w: 1440 },
  { name: 'live-drawer-1440', url: '/nodes?m=__FIRST__', w: 1440 },
  { name: 'live-drawer-manage-1440', url: '/nodes?m=__FIRST__&tab=manage', w: 1440 },
  { name: 'live-relays-1440', url: '/relays', w: 1440 },
  { name: 'live-jobs-1440', url: '/jobs', w: 1440 },
  { name: 'live-catalog-1440', url: '/catalog', w: 1440 },
  { name: 'live-provision-1440', url: '/provision', w: 1440 },
];
const run = SESSION ? liveScenes : scenes;
const browser = await puppeteer.launch({ executablePath: EXE, headless: true, args: ['--no-sandbox', '--hide-scrollbars'] });
let firstNode = null;
if (SESSION) {
  const p = await browser.newPage();
  await p.setCookie({ name: 'fleet_session', value: SESSION, url: BASE });
  await p.goto(BASE + '/api/v1/machines');
  firstNode = await p.evaluate(() => JSON.parse(document.body.innerText).data.find((m) => m.kind !== 'relay')?.id ?? null);
  await p.close();
}
const report = [];
for (const s0 of run) {
  const s = { ...s0, url: s0.url.replace('__FIRST__', firstNode ?? '') };
  if (filter && !s.name.includes(filter)) continue;
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  await page.setViewport({ width: s.w, height: 1000, deviceScaleFactor: 1 });
  if (SESSION) await page.setCookie({ name: 'fleet_session', value: SESSION, url: BASE });
  await page.evaluateOnNewDocument((view, theme, layout) => {
    try { localStorage.setItem('fleet:view', view ?? 'large'); localStorage.setItem('fleet:theme', theme ?? 'light'); localStorage.setItem('fleet:layout', layout ?? 'cards'); } catch { /* 忽略 */ }
  }, s.view, s.theme, s.layout);
  await page.goto(BASE + s.url, { waitUntil: SESSION ? 'networkidle2' : 'networkidle0', timeout: 30000 });
  if (SESSION) await new Promise((r) => setTimeout(r, 1500));
  if (s.wait) await new Promise((r) => setTimeout(r, s.wait));
  await new Promise((r) => setTimeout(r, 700));
  if (s.retired) { await page.evaluate(() => { [...document.querySelectorAll('.seg-item')].find((b) => b.textContent?.includes('已退役'))?.click(); }); await new Promise((r) => setTimeout(r, 500)); }
  if (s.batch) {
    await page.evaluate(() => { [...document.querySelectorAll('.seg-item')].find((b) => b.textContent?.includes('批量'))?.click(); });
    await new Promise((r) => setTimeout(r, 400));
  }
  // 撑高视口让懒加载的心跳条都进入视口，再截全页
  const h = await page.evaluate(() => document.documentElement.scrollHeight);
  await page.setViewport({ width: s.w, height: Math.min(h, 6000), deviceScaleFactor: 1 });
  await new Promise((r) => setTimeout(r, 900));
  const file = path.join(out, `${s.name}.png`);
  const clip = process.env.CLIP ? (([x, y, width, height]) => ({ x, y, width, height }))(process.env.CLIP.split(',').map(Number)) : undefined;
  await page.screenshot({ path: clip ? file.replace('.png', '-clip.png') : file, fullPage: !clip && !s.url.includes('m='), clip, captureBeyondViewport: true });
  const entry = { name: s.name, errors };
  if (s.align) {
    entry.align = await page.evaluate(() => {
      const res = { rows: 0, bad: [] };
      for (const grid of document.querySelectorAll('.cards')) {
        const cards = [...grid.querySelectorAll(':scope > .ncard')];
        const byRow = new Map();
        for (const c of cards) { const t = Math.round(c.getBoundingClientRect().top + scrollY); (byRow.get(t) ?? byRow.set(t, []).get(t)).push(c); }
        for (const [top, row] of byRow) {
          if (row.length < 2) continue;
          res.rows++;
          const n = Math.max(...row.map((c) => c.children.length));
          for (let i = 0; i < n; i++) {
            const offs = row.map((c) => (c.children[i] ? Math.round((c.children[i].getBoundingClientRect().top - c.getBoundingClientRect().top) * 10) / 10 : null));
            if (new Set(offs).size > 1) res.bad.push({ rowTop: top, slot: i, offs });
          }
          const hs = new Set(row.map((c) => Math.round(c.getBoundingClientRect().height)));
          if (hs.size > 1) res.bad.push({ rowTop: top, slot: 'height', offs: [...hs] });
        }
      }
      return res;
    });
  }
  entry.overflowX = await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth + 1);
  report.push(entry);
  await page.close();
  console.log(s.name, entry.errors.length ? 'ERR ' + entry.errors.slice(0, 2).join(' | ') : 'ok', entry.align ? `rows=${entry.align.rows} misaligned=${entry.align.bad.length}` : '', entry.overflowX ? 'OVERFLOW-X' : '');
}
await browser.close();
fs.writeFileSync(path.join(out, 'report.json'), JSON.stringify(report, null, 1));
