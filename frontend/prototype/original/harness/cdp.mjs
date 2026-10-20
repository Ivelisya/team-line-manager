// 极简 CDP 驱动：无头 Chrome 打开本地页面，截图 / 执行脚本。只访问 127.0.0.1。
// 用法：node cdp.mjs <jobs.json>
// jobs.json: [{url, width, height, wait, shot?: "out.png", fullPage?: true, eval?: "file.js", out?: "result.json", actions?: [...]}]
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const PORT = 9334;
const profile = fs.mkdtempSync('/private/tmp/claude-501/cdp-profile-');
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, `--user-data-dir=${profile}`,
  '--no-first-run', '--no-default-browser-check', '--hide-scrollbars', '--force-device-scale-factor=2', 'about:blank'], { stdio: 'ignore' });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function wsUrl() {
  for (let i = 0; i < 50; i++) {
    try { const r = await fetch(`http://127.0.0.1:${PORT}/json/list`); const l = await r.json(); const p = l.find((t) => t.type === 'page'); if (p) return p.webSocketDebuggerUrl; } catch {}
    await sleep(200);
  }
  throw new Error('chrome not up');
}

const ws = new WebSocket(await wsUrl());
await new Promise((r) => ws.addEventListener('open', r, { once: true }));
let id = 0; const pending = new Map();
ws.addEventListener('message', (e) => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const { res, rej } = pending.get(m.id); pending.delete(m.id); m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result); } });
const send = (method, params = {}) => new Promise((res, rej) => { const i = ++id; pending.set(i, { res, rej }); ws.send(JSON.stringify({ id: i, method, params })); });

await send('Page.enable'); await send('Runtime.enable');
const jobs = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const base = path.dirname(path.resolve(process.argv[2]));
const evalIn = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 800)); return r.result.value; };

for (const job of jobs) {
  const dark = job.dark === true;
  await send('Emulation.setDeviceMetricsOverride', { width: job.width || 1440, height: job.height || 900, deviceScaleFactor: job.scale || 2, mobile: false });
  await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: dark ? 'dark' : 'light' }, { name: 'prefers-reduced-motion', value: 'reduce' }] });
  if (job.pre) await fetch(job.pre);
  await send('Page.navigate', { url: job.url });
  await sleep(job.wait || 6000);
  const act = async (a) => {
    if (a.js) await evalIn(a.js);
    if (a.hoverJs || a.clickJs) {
      const pt = await evalIn(a.hoverJs || a.clickJs);
      if (pt) {
        await send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: pt.x, y: pt.y });
        if (a.clickJs) { await send('Input.dispatchMouseEvent', { type: 'mousePressed', x: pt.x, y: pt.y, button: 'left', clickCount: 1 }); await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: pt.x, y: pt.y, button: 'left', clickCount: 1 }); }
      } else console.log('no point for action');
    }
    if (a.key) { await send('Input.dispatchKeyEvent', { type: 'keyDown', key: a.key, code: a.key, windowsVirtualKeyCode: a.keyCode || 9 }); await send('Input.dispatchKeyEvent', { type: 'keyUp', key: a.key, code: a.key, windowsVirtualKeyCode: a.keyCode || 9 }); }
    if (a.wait) await sleep(a.wait);
  };
  for (const a of job.actions || []) await act(a);
  if (job.eval) {
    const code = fs.readFileSync(path.resolve(base, job.eval), 'utf8');
    const v = await evalIn(code);
    fs.writeFileSync(path.resolve(base, job.out), JSON.stringify(v, null, 1));
    console.log('eval ->', job.out);
  }
  const shots = job.shots || (job.shot ? [{ shot: job.shot, clipJs: job.clipJs }] : []);
  for (const s of shots) {
    if (s.js) { await evalIn(s.js); await sleep(s.wait || 400); }
    for (const a of s.actions || []) await act(a);
    let clip;
    if (s.clipJs) { const c = await evalIn(s.clipJs); if (!c) { console.log('no clip for', s.shot); continue; } clip = { ...c, scale: 1 }; }
    const r = await send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: false, ...(clip ? { clip } : {}) });
    fs.writeFileSync(path.resolve(base, s.shot), Buffer.from(r.data, 'base64'));
    console.log('shot ->', s.shot);
  }
}
ws.close(); chrome.kill('SIGKILL');
try { fs.rmSync(profile, { recursive: true, force: true }); } catch {}
process.exit(0);
