// 交互态截图：命令面板、悬浮卡片、右键菜单、下拉菜单、作业进度、设置页、通知、快捷键帮助、深色模式。
// 用法：VITE_MOCK=1 pnpm dev（或 pnpm dev:mock）后，BASE=http://127.0.0.1:5173 OUT=<目录> node tools/shoot-ui.mjs [场景名过滤]
import puppeteer from 'puppeteer-core';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const out = process.env.OUT ?? path.join(here, '..', 'shots');
fs.mkdirSync(out, { recursive: true });
const BASE = process.env.BASE ?? 'http://127.0.0.1:5173';
const EXE = process.env.CHROME ?? ['C:/Program Files/Google/Chrome/Application/chrome.exe', 'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe'].find((p) => fs.existsSync(p));
const filter = process.argv[2];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const browser = await puppeteer.launch({ executablePath: EXE, headless: true, args: ['--no-sandbox', '--hide-scrollbars'] });
const report = [];

async function open(url, { w = 1440, h = 900, theme = 'light', view = 'large' } = {}) {
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  await page.setViewport({ width: w, height: h, deviceScaleFactor: 1 });
  await page.evaluateOnNewDocument((t, v) => { try { localStorage.setItem('fleet:theme', t); localStorage.setItem('fleet:view', v); } catch { /* 忽略 */ } }, theme, view);
  await page.goto(BASE + url, { waitUntil: 'networkidle0', timeout: 30000 });
  await sleep(700);
  return { page, errors };
}
const clickText = (page, sel, text) => page.evaluate((s, t) => { const el = [...document.querySelectorAll(s)].find((e) => e.textContent?.trim().includes(t)); if (!el) throw new Error('找不到 ' + t); el.click(); }, sel, text);
const shot = (page, name, clip) => page.screenshot({ path: path.join(out, `${name}.png`), clip });

const scenes = {
  async 'home-light'() { const { page, errors } = await open('/', { h: 1100 }); await shot(page, 'ui-home-light'); return { page, errors }; },
  async 'palette'() {
    const { page, errors } = await open('/');
    await page.keyboard.down('Control'); await page.keyboard.press('k'); await page.keyboard.up('Control'); await sleep(500);
    await shot(page, 'ui-palette-empty');
    await page.keyboard.type('团队2-0'); await sleep(400);
    await shot(page, 'ui-palette-search');
    return { page, errors };
  },
  async 'palette-dark'() {
    const { page, errors } = await open('/', { theme: 'dark' });
    await page.keyboard.down('Control'); await page.keyboard.press('k'); await page.keyboard.up('Control'); await sleep(500);
    await page.keyboard.type('线路'); await sleep(400);
    await shot(page, 'ui-palette-dark');
    return { page, errors };
  },
  async 'hover'() {
    const { page, errors } = await open('/nodes');
    const el = await page.$('tbody tr:nth-child(3) .cell-main.strong');
    await el.hover(); await sleep(1000);
    await shot(page, 'ui-hovercard-nodes');
    return { page, errors };
  },
  async 'hover-incidents'() {
    const { page, errors } = await open('/incidents');
    const el = await page.$('tbody tr:nth-child(1) td:nth-child(2) button');
    await el.hover(); await sleep(1000);
    await shot(page, 'ui-hovercard-incidents');
    return { page, errors };
  },
  async 'health-tip'() {
    const { page, errors } = await open('/nodes');
    const el = await page.$('tbody tr:nth-child(7) .pill');
    await el.hover(); await sleep(900);
    await shot(page, 'ui-tooltip-health');
    return { page, errors };
  },
  async 'context-card'() {
    const { page, errors } = await open('/', { h: 1000 });
    const el = await page.$('.ncard:nth-child(2) .nc-name');
    const b = await el.boundingBox();
    await page.mouse.click(b.x + 20, b.y + 80, { button: 'right' }); await sleep(500);
    await shot(page, 'ui-contextmenu-card');
    return { page, errors };
  },
  async 'context-row'() {
    const { page, errors } = await open('/nodes');
    const el = await page.$('tbody tr:nth-child(4)');
    const b = await el.boundingBox();
    await page.mouse.click(b.x + 400, b.y + 20, { button: 'right' }); await sleep(500);
    await shot(page, 'ui-contextmenu-row');
    return { page, errors };
  },
  async 'dropdown-card'() {
    const { page, errors } = await open('/', { h: 1000 });
    await page.click('.ncard:nth-child(3) .nc-actions button[aria-label="更多操作"]'); await sleep(500);
    await shot(page, 'ui-dropdown-card');
    return { page, errors };
  },
  async 'retire-dialog'() {
    const { page, errors } = await open('/', { h: 1000 });
    await page.click('.ncard:nth-child(3) .nc-actions button[aria-label="更多操作"]'); await sleep(400);
    await clickText(page, '[role="menuitem"]', '退役'); await sleep(900);
    await shot(page, 'ui-retire-dialog');
    return { page, errors };
  },
  async 'jobs-run'() { const { page, errors } = await open('/jobs/j-run', { h: 1500 }); await shot(page, 'ui-jobs-running'); return { page, errors }; },
  async 'jobs-fail'() { const { page, errors } = await open('/jobs/j-fail', { h: 1300 }); await shot(page, 'ui-jobs-failed'); return { page, errors }; },
  async 'jobs-cancel'() { const { page, errors } = await open('/jobs/j-cancel', { h: 1300 }); await shot(page, 'ui-jobs-cancelled'); return { page, errors }; },
  async 'jobs-dark'() { const { page, errors } = await open('/jobs/j-run', { h: 1500, theme: 'dark' }); await shot(page, 'ui-jobs-running-dark'); return { page, errors }; },
  async 'jobs-popover'() {
    const { page, errors } = await open('/');
    await page.click('.jobs-ind'); await sleep(500);
    await shot(page, 'ui-jobs-popover', { x: 840, y: 0, width: 600, height: 420 });
    return { page, errors };
  },
  async 'settings-keys'() { const { page, errors } = await open('/settings', { h: 800 }); await shot(page, 'ui-settings-keys'); return { page, errors }; },
  async 'settings-key-create'() {
    const { page, errors } = await open('/settings', { h: 900 });
    await clickText(page, 'button', '新建密钥'); await sleep(500);
    await page.type('.dialog input', '部署脚本');
    await clickText(page, '.scope-row', '远程执行'); await sleep(300);
    await shot(page, 'ui-settings-key-create');
    await clickText(page, '.dialog button[type="submit"]', '创建密钥'); await sleep(900);
    await shot(page, 'ui-settings-key-token');
    return { page, errors };
  },
  async 'settings-notify'() { const { page, errors } = await open('/settings?tab=notify', { h: 1000 }); await shot(page, 'ui-settings-notify'); return { page, errors }; },
  async 'settings-notify-dialog'() {
    const { page, errors } = await open('/settings?tab=notify', { h: 900 });
    await clickText(page, 'button', '新建渠道'); await sleep(500);
    await shot(page, 'ui-settings-notify-dialog');
    return { page, errors };
  },
  async 'settings-notify-test'() {
    const { page, errors } = await open('/settings?tab=notify', { h: 1000 });
    await clickText(page, 'button', '发送测试'); await sleep(1000);
    await shot(page, 'ui-settings-notify-test');
    return { page, errors };
  },
  async 'settings-dark'() { const { page, errors } = await open('/settings?tab=notify', { h: 1000, theme: 'dark' }); await shot(page, 'ui-settings-notify-dark'); return { page, errors }; },
  async 'toast'() {
    const { page, errors } = await open('/', { h: 900 });
    for (let i = 0; i < 2; i++) {
      await page.click(`.ncard:nth-child(${i + 2}) .nc-actions button[aria-label="更多操作"]`); await sleep(300);
      await clickText(page, '[role="menuitem"]', '复制 IP'); await sleep(300);
    }
    await page.click('.ncard:nth-child(4) .nc-actions button[aria-label="更多操作"]'); await sleep(300);
    await clickText(page, '[role="menuitem"]', '升级 agent'); await sleep(600);
    await shot(page, 'ui-toasts');
    return { page, errors };
  },
  async 'help'() {
    const { page, errors } = await open('/');
    await page.keyboard.press('?'); await sleep(500);
    await shot(page, 'ui-shortcuts-help');
    return { page, errors };
  },
  async 'drawer'() {
    const { page, errors } = await open('/?m=m-t1-05&tab=quality', { h: 1000 });
    await shot(page, 'ui-drawer-quality');
    await page.keyboard.press(']'); await sleep(900);
    await shot(page, 'ui-drawer-next');
    return { page, errors };
  },
  async 'mobile'() {
    const { page, errors } = await open('/', { w: 390, h: 800 });
    await page.keyboard.down('Control'); await page.keyboard.press('k'); await page.keyboard.up('Control'); await sleep(500);
    await shot(page, 'ui-palette-mobile');
    return { page, errors };
  },
};

for (const [name, fn] of Object.entries(scenes)) {
  if (filter && !name.includes(filter)) continue;
  let page; let errors = [];
  try { ({ page, errors } = await fn()); } catch (e) { errors.push('SCRIPT: ' + e.message); }
  console.log(name, errors.length ? 'ERR ' + errors.slice(0, 3).join(' | ') : 'ok');
  report.push({ name, errors });
  if (page) await page.close();
}
await browser.close();
fs.writeFileSync(path.join(out, 'report-ui.json'), JSON.stringify(report, null, 1));
