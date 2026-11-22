// 交互流畅度测量（pnpm perf / pnpm perf:check）：mock 模式的生产构建（vite build --mode mock → dist-mock/）+ vite preview，Playwright 的 Chromium 跑固定的几个场景，
// 每个场景记长任务（个数、合计、最长）、交互延迟（Event Timing，按 interactionId 取最长的一次，< 16ms 的不上报记 0）、布局偏移、最长的一帧、JS 堆（先 GC）、就绪耗时、
// 接口 GET 次数与其中同一路径重复取的次数。
// 两种规模（mock 的机器台数，见 src/mock/scale.ts；实时流握手按生产的几趟往返算 350 ms）× 两档 CPU（不降速 / 4 倍降速），每组跑 RUNS 遍取中位数（另有一遍热身不计）。
// 结果打印成表格，并写进 perf-results/（git 忽略）：latest.json，带 --save <名字> 时另存 <名字>.json（如 baseline）。
// 用法：node tools/perf.mjs [--no-build] [--runs 3] [--scales 18,150] [--cpu 1,4] [--save baseline] [--compare baseline] [--ab <对照 web 目录>] [--check] [--only 场景名]
//   --check：与 tools/perf-budget.json 比较，任一数超出预算就退出码 1（pnpm perf:check）。
// 浏览器：Playwright 自带的 Chromium（pnpm exec playwright install chromium），CHROME=<路径> 可换成本机的 Chrome。
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { build, preview } from 'vite';
import { chromium } from '@playwright/test';

const web = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const outDir = path.join(web, 'perf-results');
const arg = (k, d) => { const i = process.argv.indexOf(`--${k}`); return i > 0 ? process.argv[i + 1] : d; };
const flag = (k) => process.argv.includes(`--${k}`);
const RUNS = Number(arg('runs', 3));
const SCALES = arg('scales', '18,150').split(',').map(Number);
const CPUS = arg('cpu', '1,4').split(',').map(Number);
const ONLY = arg('only', '');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------- 页面里的采集（文档开始前注入） ----------
function collector() {
  const P = (window.__perf = { lt: [], ev: [], ls: [], frames: [], lcp: 0, down: 0 });
  const obs = (type, f, o = {}) => { try { new PerformanceObserver((l) => l.getEntries().forEach(f)).observe({ type, buffered: true, ...o }); } catch { /* 不支持的类型跳过 */ } };
  obs('longtask', (e) => P.lt.push([e.startTime, e.duration]));
  obs('event', (e) => { if (e.interactionId) P.ev.push([e.startTime, e.duration, e.interactionId, e.name]); }, { durationThreshold: 16 });
  obs('layout-shift', (e) => { if (!e.hadRecentInput) P.ls.push([e.startTime, e.value]); });
  obs('largest-contentful-paint', (e) => { P.lcp = e.startTime; });
  // 每一帧的间隔：只在场景进行时记（rAF 一直跑会让页面一直出帧，但页面本来就有实时推送，影响可以忽略）
  let last = 0;
  const tick = (t) => { if (last) P.frames.push([last, t - last]); last = t; requestAnimationFrame(tick); };
  requestAnimationFrame(tick);
  // 接口请求（mock 在页面里顶替 fetch，网络面板看不到）：数 /api/v1/ 的 GET，同一路径在场景里取了几次。fetch 被谁换掉都经过这里
  P.api = [];
  let inner = window.fetch;
  const counted = (input, init) => {
    const u = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
    if (u.includes('/api/v1/') && (init?.method ?? 'GET').toUpperCase() === 'GET') P.api.push([performance.now(), new URL(u, location.href).pathname + new URL(u, location.href).search]);
    return inner(input, init);
  };
  Object.defineProperty(window, 'fetch', { configurable: true, get: () => counted, set: (v) => { inner = v; } });
  addEventListener('pointerdown', () => { P.down = performance.now(); }, true);
  addEventListener('keydown', () => { P.down = performance.now(); }, true);
}

/** 场景窗口 [t0, t1] 里的各项 */
function summarize(P, t0, t1) {
  const lt = P.lt.filter(([s]) => s >= t0 && s < t1).map(([, d]) => d);
  const byId = new Map();
  for (const [s, d, id] of P.ev) if (s >= t0 && s < t1) byId.set(id, Math.max(byId.get(id) ?? 0, d));
  const fr = P.frames.filter(([s]) => s >= t0 && s < t1).map(([, d]) => d);
  const api = P.api.filter(([s]) => s >= t0 && s < t1).map(([, p]) => p);
  return {
    ltCount: lt.length, ltTotal: Math.round(lt.reduce((a, b) => a + b, 0)), ltMax: Math.round(Math.max(0, ...lt)),
    inp: Math.round(Math.max(0, ...byId.values())), interactions: byId.size,
    cls: +P.ls.filter(([s]) => s >= t0 && s < t1).reduce((a, [, v]) => a + v, 0).toFixed(4),
    maxFrame: Math.round(Math.max(0, ...fr)),
    api: api.length, apiDup: api.length - new Set(api).size,
  };
}

// ---------- 场景 ----------
const READY = { home: '.ncard:not(.is-skel)', nodes: 'main table tbody tr', traffic: 'main .stat-grid .num, main table tbody tr' };

async function now(page) { return page.evaluate(() => performance.now()); }
/** 等到页面「安静」：至少 min ms，且最近 quiet ms 没有长任务（最多 max ms） */
async function settle(page, min, quiet = 800, max = 15000) {
  const start = Date.now();
  await sleep(min);
  while (Date.now() - start < max) {
    const idle = await page.evaluate((q) => { const P = window.__perf; const l = P.lt[P.lt.length - 1]; return !l || performance.now() - (l[0] + l[1]) > q; }, quiet);
    if (idle) return;
    await sleep(200);
  }
}
async function heapMB(cdp) {
  await cdp.send('HeapProfiler.collectGarbage');
  const h = await cdp.send('Runtime.getHeapUsage');
  return +(h.usedSize / 1048576).toFixed(1);
}
/** 点击（或按键）到 sel 出现在页面上的毫秒数：从页面里记的最近一次 pointerdown / keydown 算起 */
async function readyAfter(page, sel, act) {
  const wait = page.evaluate((s) => new Promise((res) => {
    const t = () => (document.querySelector(s) ? res(performance.now() - window.__perf.down) : requestAnimationFrame(t));
    requestAnimationFrame(t);
  }), sel);
  await act();
  return Math.round(await wait);
}
async function navTo(page, label, ready) {
  const link = page.locator('aside.sidebar a.nav-item', { hasText: new RegExp(`^${label}$`) }).first();
  // 像人一样先移上去停一下再点（约 150ms）；就绪时间从按下算起
  await link.hover();
  await sleep(150);
  return readyAfter(page, ready, () => link.click());
}

const SCENES = [
  { name: 'cold-home', run: async (page, cdp, base) => {
    await page.goto(base + '/', { waitUntil: 'commit' });
    await page.waitForSelector(READY.home, { timeout: 60000 });
    const ready = Math.round(await page.evaluate(() => performance.now()));
    await settle(page, 4000); // 外壳 3 s 后预取抽屉、命令面板的代码：算进首屏
    return { t0: 0, ready, lcp: Math.round(await page.evaluate(() => window.__perf.lcp)) };
  } },
  { name: 'route-change', run: async (page) => {
    const t0 = await now(page);
    const a = await navTo(page, '机器', READY.nodes); await settle(page, 1200);
    const b = await navTo(page, '流量', READY.traffic); await settle(page, 1200);
    const c = await navTo(page, '首页', READY.home); await settle(page, 1500);
    return { t0, ready: Math.max(a, b, c), readyNodes: a, readyTraffic: b, readyHome: c };
  } },
  { name: 'drawer', run: async (page) => {
    const t0 = await now(page);
    let ready = 0;
    for (let i = 0; i < 2; i++) {
      ready = Math.max(ready, await readyAfter(page, '.m-drawer-head', () => page.locator('.ncard .nc-name').nth(i).click()));
      await settle(page, 600);
      await page.keyboard.press('Escape');
      await page.waitForSelector('.m-drawer-head', { state: 'detached', timeout: 10000 });
      await settle(page, 500);
    }
    return { t0, ready };
  } },
  { name: 'palette', run: async (page) => {
    const t0 = await now(page);
    const ready = await readyAfter(page, '[cmdk-input]', () => page.keyboard.press('Control+k'));
    await page.locator('[cmdk-input]').pressSequentially('198.51.100', { delay: 90 });
    await settle(page, 400);
    await page.keyboard.press('Escape');
    await settle(page, 400);
    return { t0, ready };
  } },
  { name: 'scroll-home', run: async (page) => {
    await page.evaluate(() => scrollTo(0, 0));
    await sleep(300);
    const t0 = await now(page);
    await page.mouse.move(700, 600);
    for (let i = 0; i < 16; i++) { await page.mouse.wheel(0, 360); await sleep(70); }
    for (let i = 0; i < 16; i++) { await page.mouse.wheel(0, -360); await sleep(70); }
    await settle(page, 500);
    return { t0 };
  } },
];

async function session(browser, base, scale, cpu) {
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
  await ctx.addInitScript((n) => {
    try { sessionStorage.setItem('mock:machines', String(n)); sessionStorage.setItem('mock:ws-delay', '350'); localStorage.setItem('fleet:theme', 'light'); localStorage.setItem('fleet:view', 'large'); localStorage.setItem('fleet:layout', 'cards'); } catch { /* 忽略 */ }
  }, scale);
  await ctx.addInitScript(collector);
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  const cdp = await ctx.newCDPSession(page);
  await cdp.send('Emulation.setCPUThrottlingRate', { rate: cpu });
  const out = {};
  for (const s of SCENES) {
    if (ONLY && s.name !== 'cold-home' && !ONLY.split(',').includes(s.name)) continue;
    const extra = await s.run(page, cdp, base);
    const t1 = await now(page);
    const P = await page.evaluate(() => window.__perf);
    out[s.name] = { ...summarize(P, extra.t0, t1), ...extra, heap: await heapMB(cdp) };
    delete out[s.name].t0;
  }
  await ctx.close();
  if (errors.length) throw new Error(`页面报错：${errors.join('\n')}`);
  return out;
}

const median = (xs) => { const s = [...xs].sort((a, b) => a - b); return s.length % 2 ? s[(s.length - 1) / 2] : +((s[s.length / 2 - 1] + s[s.length / 2]) / 2).toFixed(4); };
function aggregate(runs) {
  const out = {};
  for (const scene of Object.keys(runs[0])) {
    out[scene] = {};
    for (const k of Object.keys(runs[0][scene])) out[scene][k] = median(runs.map((r) => r[scene][k]));
  }
  return out;
}

const COLS = [['ltCount', '长任务'], ['ltTotal', '合计ms'], ['ltMax', '最长ms'], ['inp', '交互ms'], ['cls', 'CLS'], ['maxFrame', '最长帧'], ['ready', '就绪ms'], ['heap', '堆MB'], ['api', '请求'], ['apiDup', '重复']];
function table(res, cmp) {
  const lines = [];
  const head = ['规模', 'CPU', '场景'.padEnd(13), ...COLS.map(([, l]) => l)];
  lines.push(head.map((h) => String(h).padStart(7)).join(' '));
  for (const [key, scenes] of Object.entries(res)) {
    const [scale, cpu] = key.split('/');
    for (const [scene, v] of Object.entries(scenes)) {
      const old = cmp?.[key]?.[scene];
      const cell = (k) => { const x = v[k]; if (x == null) return '—'; return old?.[k] != null && old[k] !== x ? `${old[k]}→${x}` : String(x); };
      lines.push([scale, `${cpu}x`, scene.padEnd(13), ...COLS.map(([k]) => cell(k))].map((h) => String(h).padStart(7)).join(' '));
    }
  }
  return lines.join('\n');
}

// ---------- 预算 ----------
function check(res) {
  const budget = JSON.parse(fs.readFileSync(path.join(web, 'tools/perf-budget.json'), 'utf8'));
  const bad = [];
  for (const [key, scenes] of Object.entries(budget.budgets)) for (const [scene, lim] of Object.entries(scenes)) for (const [k, max] of Object.entries(lim)) {
    const v = res[key]?.[scene]?.[k];
    if (v == null) bad.push(`${key} ${scene} ${k}：没有测到`);
    else if (v > max) bad.push(`${key} ${scene} ${k} = ${v}，超出预算 ${max}`);
  }
  if (bad.length) { for (const b of bad) console.error(b); process.exitCode = 1; } else console.log('全部在预算内');
}

// ---------- 主流程 ----------
// --ab <目录>：对照组（另一份 web 目录，用它自己的 dist-mock/，须已构建），与本目录的构建逐个会话交替着跑，结果并排比。
// 机器上别的程序抢 CPU 时绝对值会整体漂（实测同一份构建前后差几倍），交替跑两边受的影响一样，比两次分开跑的结果靠得住
if (!flag('no-build')) {
  console.log('构建 mock 模式的生产包（dist-mock/）…');
  await build({ root: web, mode: 'mock', logLevel: 'warn' });
}
const abDir = arg('ab');
const variants = [...(abDir ? [{ name: 'ab', root: path.resolve(abDir) }] : []), { name: 'cur', root: web }];
const servers = [];
for (const v of variants) {
  const srv = await preview({ root: v.root, mode: 'mock', logLevel: 'warn', preview: { port: 4317, strictPort: false, open: false } });
  servers.push(srv);
  v.base = srv.resolvedUrls.local[0].replace(/\/$/, '');
}
const browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME || undefined });
const all = Object.fromEntries(variants.map((v) => [v.name, {}]));
try {
  for (const v of variants) await session(browser, v.base, SCALES[0], 1); // 热身：浏览器与预览服务器的第一次不计
  for (const scale of SCALES) for (const cpu of CPUS) {
    const runs = Object.fromEntries(variants.map((v) => [v.name, []]));
    for (let i = 0; i < RUNS; i++) for (const v of (i % 2 ? [...variants].reverse() : variants)) runs[v.name].push(await session(browser, v.base, scale, cpu));
    for (const v of variants) all[v.name][`${scale}/${cpu}`] = aggregate(runs[v.name]);
    console.log(`· ${scale} 台 / CPU ${cpu}x 完成`);
  }
} finally {
  await browser.close();
  for (const srv of servers) await srv.close();
}
const res = all.cur;
fs.mkdirSync(outDir, { recursive: true });
const doc = { at: new Date().toISOString(), runs: RUNS, results: res, ...(abDir && { ab: { dir: abDir, results: all.ab } }) };
fs.writeFileSync(path.join(outDir, 'latest.json'), JSON.stringify(doc, null, 2));
const save = arg('save');
if (save) fs.writeFileSync(path.join(outDir, `${save}.json`), JSON.stringify(doc, null, 2));
const cmpName = arg('compare');
const cmp = abDir ? all.ab : cmpName ? JSON.parse(fs.readFileSync(path.join(outDir, `${cmpName}.json`), 'utf8')).results : null;
if (cmp) console.log(`\n对照：${abDir ? `${abDir}（交替跑）` : cmpName} → 本目录`);
console.log('\n' + table(res, cmp));
if (flag('check')) check(res);
