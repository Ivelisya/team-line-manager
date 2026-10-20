// 交互冒烟：退役确认流程（预览改名 + 输入显示名）与批量上架（粘贴 → 检查 → 唯一确认 → 作业）。需先 VITE_MOCK=1 pnpm dev
import puppeteer from 'puppeteer-core';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const out = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'shots');
fs.mkdirSync(out, { recursive: true });
const BASE = process.env.BASE ?? 'http://127.0.0.1:5173';
const EXE = ['C:/Program Files/Google/Chrome/Application/chrome.exe', 'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe'].find((p) => fs.existsSync(p));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const browser = await puppeteer.launch({ executablePath: EXE, headless: true, args: ['--no-sandbox'] });
const page = await browser.newPage();
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));
page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
await page.setViewport({ width: 1440, height: 1000 });
const clickText = (sel, text) => page.evaluate((s, t) => { const el = [...document.querySelectorAll(s)].find((e) => e.textContent?.trim().includes(t)); if (!el) throw new Error('找不到 ' + t); el.click(); }, sel, text);
const step = (m) => console.log('·', m);

// 1. 退役
await page.goto(`${BASE}/nodes?m=m-t1-03&tab=manage`, { waitUntil: 'networkidle0' });
await clickText('button', '退役…'); await sleep(500);
await page.screenshot({ path: path.join(out, 'smoke-retire-dialog.png') });
const preview = await page.evaluate(() => document.querySelector('.dialog')?.innerText ?? '');
step('退役预览含改名：' + (preview.includes('团队1-04') && preview.includes('团队1-03')));
await page.type('#typed-confirm', '团队1-03');
await clickText('.dialog button', '退役'); await sleep(900);
// 注意：mock 数据在内存里，整页跳转会重置，所以不在这里跨页验证作业列表

// 2. 批量上架
await page.goto(`${BASE}/provision`, { waitUntil: 'networkidle0' });
await clickText('.seg-item', '批量'); await sleep(300);
await page.evaluate(() => {
  const input = document.querySelector('.dt-grid select[aria-label="类型"]');
  input.focus();
  const dt = new DataTransfer();
  dt.setData('text', '落地机\t198.51.100.150\t\t团队2\t\t\t线路机2\n落地机\t198.51.100.151\t\t团队2\t\t\t线路机2\n线路机\t203.0.113.60\t\t\t\t线路机3');
  input.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
});
await sleep(300);
await clickText('button', '检查'); await sleep(900);
await page.screenshot({ path: path.join(out, 'smoke-provision-checked.png') });
step('检查通过提示：' + (await page.evaluate(() => document.body.innerText.includes('检查通过'))));
await clickText('button', '开始上架'); await sleep(400);
await page.screenshot({ path: path.join(out, 'smoke-provision-confirm.png') });
await clickText('.dialog button', '确认上架'); await sleep(900);
await page.screenshot({ path: path.join(out, 'smoke-provision-created.png') });
step('已创建作业：' + (await page.evaluate(() => document.body.innerText.includes('已开始上架'))));
console.log('页面错误：', errors.length ? errors : '无');
await browser.close();
