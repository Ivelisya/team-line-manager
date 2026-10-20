// shoot.mjs — 无头 Chrome（CDP）截图与运行时检查，只访问 127.0.0.1:5199 的本地原型。
// 用法：~/.local/bin/node tools/shoot.mjs [jobs.json]    （不给文件时跑内置的首页验收截图）
// job：{ name, url, width, height, dark, full, dpr, wait, before: "页面里执行的 JS（可 await）", clip: {x,y,width,height} }
// 输出：shots/<name>.png；控制台报错汇总打印到 stdout。

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const ROOT = path.dirname(path.dirname(new URL(import.meta.url).pathname));
const BASE = 'http://127.0.0.1:5199/original-refined/index.html';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export async function launch({ port = Number(process.env.CDP_PORT ?? 9361) } = {}) {
  const dir = fs.mkdtempSync(path.join(process.env.TMPDIR || os.tmpdir(), 'refined-chrome-'));
  const proc = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${port}`, `--user-data-dir=${dir}`, '--lang=zh-CN', '--accept-lang=zh-CN,zh', '--no-first-run', '--no-default-browser-check', '--hide-scrollbars', '--force-color-profile=srgb', '--window-size=1440,900', 'about:blank'], { stdio: 'ignore' });
  let targets = [];
  for (let i = 0; i < 100; i++) {
    try { targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json(); if (targets.some((t) => t.type === 'page')) break; } catch {}
    await sleep(150);
  }
  const page = targets.find((t) => t.type === 'page');
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const pending = new Map();
  const logs = [];
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) { const { res, rej } = pending.get(m.id); pending.delete(m.id); m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result); return; }
    if (m.method === 'Runtime.exceptionThrown') logs.push('EXC ' + (m.params.exceptionDetails?.exception?.description || m.params.exceptionDetails?.text));
    if (m.method === 'Runtime.consoleAPICalled' && (m.params.type === 'error' || m.params.type === 'warning')) logs.push(`${m.params.type} ` + m.params.args.map((a) => a.value ?? a.description).join(' '));
    if (m.method === 'Log.entryAdded' && m.params.entry.level === 'error') logs.push('log ' + m.params.entry.text + ' ' + (m.params.entry.url ?? ''));
    if (m.method === 'Fetch.requestPaused') api.onPaused?.(m.params);
  };
  const send = (method, params = {}) => new Promise((res, rej) => { const i = ++id; pending.set(i, { res, rej }); ws.send(JSON.stringify({ id: i, method, params })); });
  await send('Page.enable'); await send('Runtime.enable'); await send('Log.enable');
  const api = {
    send, logs, sleep,
    async viewport(w, h, dpr = 1) { await send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: dpr, mobile: w < 600 }); },
    async media({ dark = false, reduce = true } = {}) { await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: dark ? 'dark' : 'light' }, { name: 'prefers-reduced-motion', value: reduce ? 'reduce' : 'no-preference' }] }); },
    async go(url) { await send('Page.navigate', { url }); },
    async eval(expr) { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text); return r.result.value; },
    async until(expr, timeout = 15000) { const t = Date.now(); while (Date.now() - t < timeout) { try { if (await api.eval(expr)) return true; } catch {} await sleep(150); } return false; },
    async shot(file, { full = false, clip } = {}) {
      const opts = { format: 'png' };
      if (full) {
        const h = await api.eval('Math.max(document.documentElement.scrollHeight, document.body.scrollHeight)');
        const w = await api.eval('document.documentElement.clientWidth');
        opts.clip = { x: 0, y: 0, width: w, height: h, scale: 1 };
        opts.captureBeyondViewport = true;
      } else if (clip) { opts.clip = { scale: 1, ...clip }; opts.captureBeyondViewport = true; }
      const r = await send('Page.captureScreenshot', opts);
      fs.writeFileSync(file, Buffer.from(r.data, 'base64'));
    },
    async close() { try { ws.close(); } catch {} proc.kill('SIGTERM'); },
  };
  return api;
}

export const DEFAULT_JOBS = [
  { name: 'home-large-light-1440', url: '#/home', width: 1440, height: 900 },
  { name: 'home-large-dark-1440', url: '#/home', width: 1440, height: 900, dark: true },
  { name: 'home-large-light-1440-full', url: '#/home', width: 1440, height: 900, full: true },
  { name: 'home-small-light-1440', url: '#/home', width: 1440, height: 900, view: 'small' },
  { name: 'home-compact-light-1440', url: '#/home', width: 1440, height: 900, view: 'compact' },
  { name: 'home-large-light-375', url: '#/home', width: 375, height: 812 },
  { name: 'home-large-dark-375', url: '#/home', width: 375, height: 812, dark: true },
];

async function run(jobs) {
  const b = await launch({ port: Number(process.env.SHOOT_PORT) || 9361 }); // SHOOT_PORT：多个构建者同时截图时各用各的调试端口
  const outDir = path.join(ROOT, 'shots');
  fs.mkdirSync(outDir, { recursive: true });
  const report = [];
  for (const j of jobs) {
    b.logs.length = 0;
    await b.viewport(j.width ?? 1440, j.height ?? 900, j.dpr ?? 1);
    await b.media({ dark: !!j.dark, reduce: j.reduce !== false });
    await b.go('about:blank');
    if (j.page) {
      // 非原型页面（例如 tools/compare.html 并排对照）：等图片加载完再截
      await b.go(j.page);
      await b.until('document.readyState === "complete" && [...document.images].every((i) => i.complete)', 20000);
      await b.sleep(j.wait ?? 300);
      await b.shot(path.join(outDir, `${j.name}.png`), { full: !!j.full });
      console.log(`${j.name}: 对照图`);
      continue;
    }
    await b.go(`${BASE}${j.url ?? '#/home'}`);
    await b.until('document.readyState === "complete"');
    await b.eval(`(() => { try { localStorage.setItem('refined:theme', JSON.stringify(${JSON.stringify(j.theme ?? 'system')})); ${j.view ? `localStorage.setItem('refined:home:view', JSON.stringify('${j.view}'));` : `localStorage.removeItem('refined:home:view');`} localStorage.removeItem('refined:nav'); localStorage.removeItem('refined:density'); } catch (e) {} return 1; })()`);
    if (j.delaySnapshot) {
      // 首次加载骨架：把 snapshot.json 的响应拖住，先截骨架
      await b.send('Fetch.enable', { patterns: [{ urlPattern: '*snapshot.json*' }] });
      b.onPaused = (p) => setTimeout(() => b.send('Fetch.continueRequest', { requestId: p.requestId }).catch(() => {}), j.delaySnapshot);
      await b.send('Page.reload', { ignoreCache: true });
      await b.sleep(900);
      const file = path.join(outDir, `${j.name}.png`);
      await b.shot(file);
      console.log(`${j.name}: 骨架截图`);
      await b.sleep(j.delaySnapshot);
      await b.send('Fetch.disable');
      b.onPaused = null;
      continue;
    }
    await b.send('Page.reload', { ignoreCache: false });
    await b.sleep(300);
    await b.until('document.documentElement.dataset.ready === "1"', 20000);
    await b.sleep(j.wait ?? 900);
    if (j.before) { await b.eval(`(async () => { ${j.before} })()`); await b.sleep(j.afterWait ?? 700); }
    const file = path.join(outDir, `${j.name}.png`);
    await b.shot(file, { full: !!j.full, clip: j.clip });
    const metrics = await b.eval(`({ sw: document.documentElement.scrollWidth, cw: document.documentElement.clientWidth, h: document.documentElement.scrollHeight })`);
    report.push({ name: j.name, ...metrics, logs: [...b.logs] });
    console.log(`${j.name}: scrollWidth ${metrics.sw} / ${metrics.cw}${b.logs.length ? `\n  ${b.logs.join('\n  ')}` : ''}`);
  }
  await b.close();
  return report;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const jobs = process.argv[2] ? JSON.parse(fs.readFileSync(process.argv[2], 'utf8')) : DEFAULT_JOBS;
  run(jobs).then(() => process.exit(0), (e) => { console.error(e); process.exit(1); });
}
export { run };
