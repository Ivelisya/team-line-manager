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
  async 'ipintel'() {
    // 访问监控「来源地址」页签（运营商 · 城市 · 接入方式）与节点抽屉「访问记录」里的来源画像卡
    const { page, errors } = await open('/access?view=sources', { h: 900 });
    await shot(page, 'ui-access-sources-intel');
    const d = await open('/access?m=m-t1-01&tab=access', { h: 1500 });
    await sleep(600);
    await shot(d.page, 'ui-drawer-source-profile');
    await d.page.close();
    return { page, errors: errors.concat(d.errors) };
  },
  // 生命周期：换线路机、搬空线路机、恢复已退役的机器
  async 'move-dialog'() {
    const { page, errors } = await open('/?m=m-t1-02&tab=manage', { h: 1000 });
    await clickText(page, 'button', '换线路机'); await sleep(700);
    await page.click('.dialog .select-trigger'); await sleep(300);
    await clickText(page, '[role="option"]', '线路机2'); await sleep(400);
    await shot(page, 'ui-move-dialog');
    return { page, errors };
  },
  async 'evacuate-dialog'() {
    const { page, errors } = await open('/relays', { h: 1000 });
    await clickText(page, 'button', '搬空到'); await sleep(700);
    await page.click('.dialog .select-trigger'); await sleep(300);
    await clickText(page, '[role="option"]', '线路机2'); await sleep(400);
    await shot(page, 'ui-evacuate-dialog');
    return { page, errors };
  },
  async 'restore-dialog'() {
    const { page, errors } = await open('/?m=m-t2-5r&tab=manage', { h: 1100 });
    await shot(page, 'ui-restore-drawer');
    await clickText(page, 'button', '恢复…'); await sleep(1500);
    await shot(page, 'ui-restore-dialog');
    return { page, errors };
  },
  async 'restore-dialog-password'() {
    const { page, errors } = await open('/?m=m-t1-3r&tab=manage', { h: 1100 });
    await clickText(page, 'button', '恢复…'); await sleep(1500);
    await page.type('.dialog input[type="password"]', 'secret'); await sleep(300);
    await shot(page, 'ui-restore-dialog-password');
    return { page, errors };
  },
  async 'move-menu'() {
    const { page, errors } = await open('/', { h: 1000 });
    await page.click('.ncard:nth-child(3) .nc-actions button[aria-label="更多操作"]'); await sleep(500);
    await shot(page, 'ui-move-menu');
  },
  async 'policy-light'() { const { page, errors } = await open('/access/policy', { h: 1500 }); await shot(page, 'ui-policy-light'); return { page, errors }; },
  async 'policy-dark'() { const { page, errors } = await open('/access/policy', { h: 1500, theme: 'dark' }); await shot(page, 'ui-policy-dark'); return { page, errors }; },
  async 'policy-invalid'() {
    const { page, errors } = await open('/access/policy', { h: 1300 });
    await page.click('.pol-ta'); await page.keyboard.down('Control'); await page.keyboard.press('End'); await page.keyboard.up('Control');
    await page.keyboard.type('\nhttp://bad.example.com/x\n1.2.3.4, ok-one.example.org\nbad_label.com');
    await sleep(500);
    await shot(page, 'ui-policy-invalid');
    return { page, errors };
  },
  async 'policy-preview'() {
    const { page, errors } = await open('/access/policy', { h: 1000 });
    await clickText(page, 'button', '预览并发布'); await sleep(1200);
    await shot(page, 'ui-policy-preview');
    await clickText(page, '.dialog button', '发布…'); await sleep(600);
    await shot(page, 'ui-policy-publish-confirm');
    return { page, errors };
  },
  async 'policy-flow'() {
    // 走一遍：改草稿 → 保存 → 预览 → 发布 → 记录里出现「进行中」→ 回滚；断言靠页面文字
    const { page, errors } = await open('/access/policy', { h: 1300 });
    const text = () => page.evaluate(() => document.body.innerText);
    const expect = async (s) => { for (let i = 0; i < 30; i++) { if ((await text()).includes(s)) return; await sleep(200); } throw new Error('页面上没有出现：' + s); };
    await page.click('.pol-ta'); await page.keyboard.down('Control'); await page.keyboard.press('End'); await page.keyboard.up('Control');
    await page.keyboard.type('\nnew.example.com');
    await clickText(page, 'button', '保存草稿'); await expect('草稿已保存');
    await clickText(page, 'button', '预览并发布'); await expect('预览：发布后会怎样'); await sleep(600);
    await clickText(page, '.dialog button', '发布…'); await expect('发布 2 份草稿？');
    await clickText(page, '.dialog button', '确认发布'); await expect('已发布为版本 4');
    await sleep(500); await shot(page, 'ui-policy-published');
    await expect('进行中'); await expect('已应用');
    await clickText(page, 'tbody button', '回滚到此版本'); await expect('回滚到版本');
    await clickText(page, '.dialog button', '回滚'); await expect('已回滚，新版本 v5');
    await sleep(500); await shot(page, 'ui-policy-rolledback');
    return { page, errors };
  },
  async 'policy-preview-dark'() {
    const { page, errors } = await open('/access/policy', { h: 1000, theme: 'dark' });
    await clickText(page, 'button', '预览并发布'); await sleep(1200);
    await shot(page, 'ui-policy-preview-dark');
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
