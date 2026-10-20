// interact.mjs — 键盘与交互冒烟测试（SPEC §8.2 V10 的自动部分）：真实按键事件经 CDP 派发。
// 用法：~/.local/bin/node tools/interact.mjs
import { launch } from './shoot.mjs';

const BASE = 'http://127.0.0.1:5199/original-refined/index.html';
const b = await launch({ port: 9381 });
const key = async (k, { meta = false, shift = false, code, text } = {}) => {
  const mods = (meta ? 4 : 0) | (shift ? 8 : 0);
  const params = { key: k, code: code ?? (k.length === 1 ? `Key${k.toUpperCase()}` : k), modifiers: mods, windowsVirtualKeyCode: k.length === 1 ? k.toUpperCase().charCodeAt(0) : { Escape: 27, Enter: 13, ArrowDown: 40, ArrowUp: 38, ArrowRight: 39, ArrowLeft: 37, Tab: 9 }[k] ?? 0 };
  await b.send('Input.dispatchKeyEvent', { type: 'keyDown', ...params, text: text ?? (k.length === 1 && !meta ? k : undefined) });
  await b.send('Input.dispatchKeyEvent', { type: 'keyUp', ...params });
  await b.sleep(250);
};
const results = [];
const check = async (name, expr) => { const v = await b.eval(expr); results.push(`${v ? 'PASS' : 'FAIL'}  ${name}${v && v !== true ? ` → ${v}` : ''}`); };

await b.viewport(1440, 900);
await b.media({ dark: false, reduce: true });
await b.go(`${BASE}#/home`);
await b.until('document.documentElement.dataset.ready === "1"', 20000);
await b.sleep(1200);

await key('2'); await check('2 → 小卡片', `document.querySelector('[data-grid]').dataset.view === 'small'`);
await key('3'); await check('3 → 紧凑', `document.querySelector('[data-grid]').dataset.view === 'compact'`);
await key('1'); await check('1 → 大卡片', `document.querySelector('[data-grid]').dataset.view === 'large'`);
await key('j'); await key('j'); await check('j j → 焦点在第二张卡', `document.activeElement.closest('.ncard')?.dataset.id`);
await key('Enter', { text: '\r' }); await b.sleep(400); await check('Enter → 打开抽屉', `!!document.querySelector('.drawer') && location.hash`);
await key(']'); await b.sleep(300); await check('] → 下一个节点', `location.hash`);
await key('Escape'); await b.sleep(400); await check('Esc → 关闭抽屉并回到原 URL', `!document.querySelector('.drawer:not(.is-leaving)') && location.hash`);
await key('k', { meta: true }); await check('⌘K → 命令面板', `!!document.querySelector('.palette')`);
await b.send('Input.insertText', { text: 'Pulse' }); await b.sleep(300);
await check('命令面板搜 Pulse 命中套餐', `[...document.querySelectorAll('.palette-title')].map(e=>e.textContent).filter(t=>t.includes('Pulse')).join(' | ')`);
await key('Escape'); await check('Esc → 关闭命令面板', `!document.querySelector('.palette')`);
await key('?', { shift: true, code: 'Slash', text: '?' }); await check('? → 快捷键帮助', `document.querySelector('.dialog .dialog-title')?.textContent`);
await key('Escape'); await b.sleep(300); await check('Esc → 关闭帮助', `!document.querySelector('.dialog:not(.is-leaving)')`);
await key('g'); await check('g → 左下角提示条', `document.querySelector('.keyhint')?.textContent.slice(0, 30)`);
await key('n'); await b.sleep(600); await check('g n → 节点页（占位或实现）', `location.hash + ' ' + document.querySelector('.page-title')?.textContent`);
await key('g'); await key('h'); await b.sleep(900); await check('g h → 首页', `location.hash`);
await key('l', { meta: true, shift: true }); await check('⌘⇧L → 深色', `document.documentElement.dataset.theme`);
await key('l', { meta: true, shift: true }); await check('⌘⇧L → 浅色', `document.documentElement.dataset.theme`);
await check('主题写入 localStorage', `localStorage.getItem('refined:theme')`);
// 筛选写进 URL
// 套餐筛选在“套餐 ▾”多选菜单里（原筛选条上的套餐 chip 已收进菜单，fidelity #3）
await b.eval(`document.querySelector('[data-act="plan-menu"]').click()`); await b.sleep(300);
await b.eval(`[...document.querySelectorAll('.pop.menu .menu-item')].find((x) => x.textContent.includes('GoMami Pulse Mini'))?.click()`); await b.sleep(400);
await key('Escape');
await check('套餐筛选写进 URL', `location.hash.includes('plan=gomami.lax-pulse.mini') && document.querySelectorAll('.ncard').length`);
await b.eval(`document.querySelector('[data-seg="team"] [data-value="线路机"]').click()`); await b.sleep(400);
await check('分组分段写进 URL', `decodeURIComponent(location.hash)`);
await b.eval(`document.querySelector('[data-act="clear"]')?.click()`); await b.sleep(400);
await check('清除筛选', `decodeURIComponent(location.hash)`);
// 后退 = 关抽屉
await b.eval(`__app.openNode('t1-13', 'traffic')`); await b.sleep(600);
await check('抽屉 pushState', `location.hash`);
await b.eval('history.back()'); await b.sleep(600);
await check('浏览器后退 = 关抽屉', `!document.querySelector('.drawer:not(.is-leaving)') && location.hash`);
// 代理节点没有“主机”页签
await b.eval(`__app.openNode('t1-13')`); await b.sleep(500);
await check('代理抽屉无“主机”页签', `[...document.querySelectorAll('.drawer .tab')].map(t=>t.textContent).join(' ')`);
await key('Escape'); await b.sleep(300);
results.push(`console: ${b.logs.length ? b.logs.join(' | ') : '无报错'}`);
console.log(results.join('\n'));
await b.close();
process.exit(0);
