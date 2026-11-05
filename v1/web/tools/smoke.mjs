// 交互冒烟：退役确认流程（默认「移入备用」预览改名，再切到「立即清理」输入显示名）、批量上架（粘贴 → 检查 → 唯一确认 → 作业）、对话框里提示开着时 Esc 的先后、数据保留页的并排表单项。需先 VITE_MOCK=1 pnpm dev
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
// 选择器按角色 / 标签写，不依赖 .dialog、.seg-item 这类样式类名（组件换底层后类名会变）
const clickText = (sel, text) => page.evaluate((s, t) => { const el = [...document.querySelectorAll(s)].find((e) => e.textContent?.trim().includes(t)); if (!el) throw new Error('找不到 ' + t); el.click(); }, sel, text);
// 最上层的对话框（页面上可能还有一个非模态的抽屉也是 role=dialog，后挂载的在最上面）：取到元素句柄，fn(对话框, ...args) 在页面里执行
const inDialog = async (fn, ...args) => {
  const d = (await page.evaluateHandle(() => { const ds = document.querySelectorAll('[role="dialog"]'); return ds[ds.length - 1] ?? null; })).asElement();
  if (!d) throw new Error('没有对话框');
  return d.evaluate(fn, ...args);
};
const clickInDialog = (sel, text) => inDialog((d, s, t) => { const el = [...d.querySelectorAll(s)].find((e) => e.textContent?.trim().includes(t)); if (!el) throw new Error('对话框里找不到 ' + t); el.click(); }, sel, text);
// 某一步结果是 false、或页面有报错，进程以非 0 退出（方便脚本里串联）
const step = (m) => { console.log('·', m); if (m.endsWith('false')) process.exitCode = 1; };

// 1. 退役
await page.goto(`${BASE}/nodes?m=m-t1-03&tab=manage`, { waitUntil: 'networkidle0' });
await clickText('button', '退役…'); await sleep(500);
await page.screenshot({ path: path.join(out, 'smoke-retire-dialog.png') });
const preview = await inDialog((d) => d.innerText);
step('退役预览含改名：' + (preview.includes('团队1-04') && preview.includes('团队1-03')));
step('默认去向是移入备用（不要求输入）：' + (await inDialog((d) => ![...d.querySelectorAll('label')].some((l) => l.textContent.includes('请输入')))));
// 切到「立即清理并移入回收站」：这时要输入显示名才能确认
await clickInDialog('label', '立即清理并移入回收站'); await sleep(300);
await inDialog((d) => { const l = [...d.querySelectorAll('label')].find((e) => e.textContent.includes('请输入显示名')); if (!l?.control) throw new Error('找不到确认输入框'); l.control.focus(); });
await page.keyboard.type('团队1-03');
await clickInDialog('button', '清理并回收'); await sleep(900);
step('退役已提交：' + (await page.evaluate(() => document.body.innerText.includes('已提交清理作业'))));
// 注意：mock 数据在内存里，整页跳转会重置，所以不在这里跨页验证作业列表

// 2. 批量上架
await page.goto(`${BASE}/provision`, { waitUntil: 'networkidle0' });
await clickText('[role="radio"], [role="tab"], button', '批量'); await sleep(300);
await page.evaluate(() => {
  const input = document.querySelector('[aria-label="类型"]');
  input.focus();
  const dt = new DataTransfer();
  dt.setData('text', 'VPS 节点\t198.51.100.150\t\t团队2\t\t\t线路机2\nVPS 节点\t198.51.100.151\t\t团队2\t\t\t线路机2\n线路机\t203.0.113.60\t\t\t\t线路机3');
  input.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
});
await sleep(300);
await clickText('button', '检查'); await sleep(900);
await page.screenshot({ path: path.join(out, 'smoke-provision-checked.png') });
step('检查通过提示：' + (await page.evaluate(() => document.body.innerText.includes('检查通过'))));
await clickText('button', '开始上架'); await sleep(400);
await page.screenshot({ path: path.join(out, 'smoke-provision-confirm.png') });
await clickInDialog('button', '确认上架'); await sleep(900);
await page.screenshot({ path: path.join(out, 'smoke-provision-created.png') });
step('已创建作业：' + (await page.evaluate(() => document.body.innerText.includes('已开始上架'))));

// 3. 对话框里提示开着时按 Esc：第一下只收起提示，对话框和已填的内容还在；第二下才关对话框
await page.goto(`${BASE}/routers`, { waitUntil: 'networkidle0' });
await clickText('button', '接入软路由'); await sleep(500);
await page.keyboard.type('smoke-esc');
await inDialog((d) => d.querySelector('[role="img"][tabindex="0"]')?.focus()); await sleep(600);
const tipOpen = () => page.evaluate(() => !!document.querySelector('[data-slot="tooltip-content"][data-open]'));
const dialogs = () => page.evaluate(() => document.querySelectorAll('[role="dialog"]').length);
step('键盘聚焦「(i)」出现提示：' + (await tipOpen()));
await page.keyboard.press('Escape'); await sleep(500);
step('第一下 Esc 只收起提示：' + (!(await tipOpen()) && (await dialogs()) === 1 && (await inDialog((d) => d.querySelector('input')?.value)) === 'smoke-esc'));
await page.keyboard.press('Escape'); await sleep(500);
step('第二下 Esc 关掉对话框：' + ((await dialogs()) === 0));

// 4. 数据保留：两个天数字段在同一行（表单项不撑满整行）
await page.goto(`${BASE}/settings?section=retention`, { waitUntil: 'networkidle0' });
const tops = await page.evaluate(() => [...document.querySelectorAll('[data-slot="field"]')].slice(0, 2).map((f) => Math.round(f.getBoundingClientRect().top)));
step('数据保留的两个天数字段并排：' + (tops.length === 2 && tops[0] === tops[1]));
console.log('页面错误：', errors.length ? errors : '无');
if (errors.length) process.exitCode = 1;
await browser.close();
