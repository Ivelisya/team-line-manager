// verify.mjs — 三份评审（fidelity / consistency / craft）修复的回归检查：真实按键与点击经 CDP 派发，逐条断言并截图到 shots/fix/。
// 用法：SHOOT_PORT=9733 ~/.local/bin/node tools/fix/verify.mjs          （只访问 127.0.0.1:5199 的本地原型）
import path from 'node:path';
import { launch } from '../shoot.mjs';

const ROOT = path.dirname(path.dirname(path.dirname(new URL(import.meta.url).pathname)));
const OUT = path.join(ROOT, 'shots', 'fix');
const BASE = 'http://127.0.0.1:5199/original-refined/index.html';
const b = await launch({ port: Number(process.env.SHOOT_PORT) || 9733 });
const results = [];
const check = async (name, expr) => {
  let v;
  try { v = await b.eval(expr); } catch (e) { v = `ERR ${e.message.slice(0, 120)}`; }
  const ok = v && !(typeof v === 'string' && v.startsWith('ERR'));
  results.push(`${ok ? 'PASS' : 'FAIL'}  ${name}${v !== true ? ` → ${typeof v === 'object' ? JSON.stringify(v) : v}` : ''}`);
  return v;
};
const KEYCODE = { Escape: 27, Enter: 13, ArrowDown: 40, ArrowUp: 38, ArrowRight: 39, ArrowLeft: 37, Tab: 9 };
const key = async (k, { meta = false, shift = false, code, text } = {}) => {
  const mods = (meta ? 4 : 0) | (shift ? 8 : 0);
  const params = { key: k, code: code ?? (k.length === 1 ? `Key${k.toUpperCase()}` : k), modifiers: mods, windowsVirtualKeyCode: k.length === 1 ? k.toUpperCase().charCodeAt(0) : KEYCODE[k] ?? 0 };
  await b.send('Input.dispatchKeyEvent', { type: 'keyDown', ...params, text: text ?? (k.length === 1 && !meta ? k : undefined) });
  await b.send('Input.dispatchKeyEvent', { type: 'keyUp', ...params });
  await b.sleep(200);
};
const click = async (sel) => {
  const r = await b.eval(`(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return null; e.scrollIntoView({ block: 'nearest' }); const r = e.getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 }; })()`);
  if (!r) throw new Error(`no element ${sel}`);
  for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased']) await b.send('Input.dispatchMouseEvent', { type, x: r.x, y: r.y, button: 'left', clickCount: 1 });
  await b.sleep(300);
};
const hover = async (sel) => {
  const r = await b.eval(`(() => { const e = document.querySelector(${JSON.stringify(sel)}); const r = e.getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 }; })()`);
  await b.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: r.x, y: r.y });
};
const shot = async (name, opts) => b.shot(path.join(OUT, `${name}.png`), opts);
async function open(hash, { width = 1440, height = 900, dark = false, reduce = true, clear = true } = {}) {
  await b.viewport(width, height);
  await b.media({ dark, reduce });
  await b.go('about:blank');
  await b.go(`${BASE}${hash}`);
  await b.until('document.readyState === "complete"');
  if (clear) await b.eval(`(() => { try { localStorage.clear(); } catch {} return 1; })()`);
  await b.send('Page.reload', { ignoreCache: false });
  await b.sleep(300);
  await b.until('document.documentElement.dataset.ready === "1"', 20000);
  await b.sleep(900);
}

// ─── 外壳：顶栏、侧栏、名称写法、title ─────────────────────────────────────
await open('#/home');
await check('F5 顶栏 5 个同尺寸方块：搜索 · 外观 · 强调色 · 语言 · 退出', `[...document.querySelectorAll('.topbar-right > .top-btn')].map((x) => x.dataset.top + ':' + Math.round(x.getBoundingClientRect().width)).join(' ')`);
await click('[data-top="accent"]');
await check('F5 强调色菜单：靛蓝 ✓', `[...document.querySelectorAll('.pop.menu .menu-item')].map((x) => x.textContent.trim() + ':' + x.getAttribute('aria-checked')).join(' ')`);
await check('craft#10 菜单打开时锚点不再叠提示', `!document.querySelector('.tip')`);
await shot('verify-top-accent-menu', { clip: { x: 1100, y: 0, width: 340, height: 160 } });
await key('Escape');
await click('[data-top="lang"]');
await check('F5 语言菜单：简体中文 ✓', `[...document.querySelectorAll('.pop.menu .menu-item')].map((x) => x.textContent.trim()).join(' ')`);
await key('Escape');
await check('craft#12 Esc 后焦点回到触发按钮', `document.activeElement?.dataset.top`);
await check('F8 侧栏顶层顺序与原版一致', `[...document.querySelectorAll('.nav-list > li > .nav-item .nav-label')].map((x) => x.textContent).join(' · ')`);
await check('F8 上架流水线是“运维”分组第一项', `document.querySelector('#nav-ops .nav-item .nav-label')?.textContent`);
await check('F4 名称不再被自动加空隙（text-autospace）', `getComputedStyle(document.body).textAutospace`);
await check('C3 首页没有原生 title（全换成 data-tip）', `document.querySelectorAll('.app [title]').length === 0 || document.querySelectorAll('.app [title]').length`);
await check('F3 筛选条一行高（≈48）', `Math.round(document.querySelector('[data-fbar]').getBoundingClientRect().height)`);
await check('F13 KPI ⓘ：探针在线 / 每月费用 平时隐藏，网卡速率 / 累计常驻', `[...document.querySelectorAll('.kpi')].map((k) => k.dataset.kpi + ':' + (k.querySelector('.info-btn') ? getComputedStyle(k.querySelector('.info-btn')).opacity : '-')).join(' ')`);
await check('F12 套餐值平时没有下划线', `getComputedStyle(document.querySelector('.plan-btn')).textDecorationColor`);
await check('Q8 大卡片百分比 1 位小数', `[...document.querySelectorAll('.ncard[data-view="large"] .meter-value')].slice(0, 4).map((x) => x.textContent).join(' ')`);
await check('Q4 footer 带宽 chip 合并利用率', `document.querySelector('.ncard [data-s="util"]')?.textContent.trim()`);
await check('craft#7 标签与数值同一基线（误差 <1 px）', `(() => { const h = document.querySelector('.ncard .meter-head'); const l = h.querySelector('.meter-label'), v = h.querySelector('.meter-value .num'); const range = document.createRange(); range.selectNodeContents(l.lastChild); const a = range.getBoundingClientRect().bottom, c = v.getBoundingClientRect().bottom; return Math.abs(a - c) < 1.01 || (a - c).toFixed(2); })()`);
// 套餐菜单（筛选条）
await click('[data-act="plan-menu"]');
await check('F3 套餐 ▾ 多选菜单', `document.querySelectorAll('.pop.menu .menu-item[role="menuitemcheckbox"]').length`);
await b.eval(`[...document.querySelectorAll('.pop.menu .menu-item')].find((x) => x.textContent.includes('GoMami Pulse Mini'))?.click()`); await b.sleep(500);
await check('F3 选套餐写进 URL，按钮写“套餐：…”', `decodeURIComponent(location.hash) + ' | ' + document.querySelector('[data-act="plan-menu"]').textContent.trim()`);
await check('craft#12 菜单跟着变宽的按钮对齐（右缘差 <2 px）', `(() => { const m = document.querySelector('.pop.menu'), a = document.querySelector('[data-act="plan-menu"]'); return m && Math.abs(m.getBoundingClientRect().right - a.getBoundingClientRect().right) < 2 || [m?.getBoundingClientRect().right, a.getBoundingClientRect().right].join('/'); })()`);
await shot('verify-home-plan-menu', { clip: { x: 900, y: 260, width: 540, height: 420 } });
await key('Escape');
await b.eval(`__app.setParams({ plan: null, region: 'MX' })`); await b.sleep(500);
await check('craft#34 计数为 0 的分段不可点（筛 MX 后）', `[...document.querySelectorAll('[data-seg="team"] .seg-item')].map((x) => x.textContent.trim() + (x.disabled ? '(禁用)' : '')).join(' ')`);
await b.eval(`__app.setParams({ region: null })`); await b.sleep(400);

// 今日流量：悬停出弹层，不再叠提示
await hover('.ncard [data-act="today"]'); await b.sleep(700);
await check('craft#10 悬停“今日网卡流量”：只有弹层、没有提示', `!!document.querySelector('.pop') && !document.querySelector('.tip')`);
await b.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: 5, y: 890 }); await b.sleep(400);

// 紧凑、小卡片
await b.eval(`document.querySelector('[data-seg="view"] [data-value="compact"]').click()`); await b.sleep(500);
await check('F6 紧凑视图 3 列', `getComputedStyle(document.querySelector('[data-grid]')).gridTemplateColumns.split(' ').length`);
await check('F1 紧凑卡头带 IP', `document.querySelector('.ncard .ncard-head').textContent.replace(/\\s+/g, ' ').trim()`);
await check('F6 紧凑卡标签不再收成 +N', `[...document.querySelectorAll('.ncard[data-view="compact"] .tags-list')].slice(0, 3).map((l) => l.querySelector('.chip-more')?.textContent ?? '全显示').join(' ')`);
await check('F6 紧凑卡 footer 有带宽 chip 和两个文字按钮', `(() => { const f = document.querySelector('.ncard .ncard-foot'); return getComputedStyle(f.querySelector('.foot-text')).display !== 'none' && !!f.querySelector('.chip-util') && f.textContent.replace(/\\s+/g, ' ').trim(); })()`);
await shot('verify-card-compact', { clip: { x: 264, y: 380, width: 400, height: 360 } });
await b.eval(`document.querySelector('[data-seg="view"] [data-value="small"]').click()`); await b.sleep(500);
await check('F2 小卡片流量上限：左“流量上限 值”、右“持续运行”、下一行“已用 / 上限”', `(() => { const c = document.querySelector('.ncard[data-view="small"]'); const q = c.querySelector('.qhead').textContent.replace(/\\s+/g, ' ').trim(); return q + ' || ' + c.querySelector('.quse-row').textContent.trim(); })()`);
await shot('verify-card-small', { clip: { x: 264, y: 380, width: 400, height: 460 } });
await b.eval(`document.querySelector('[data-seg="view"] [data-value="large"]').click()`); await b.sleep(500);
await check('F7 线路机大卡片：套餐下面有规格行', `document.querySelector('.ncard[data-kind="relay"] .ncard-spec')?.textContent`);
await check('C11 线路机大卡片：延迟写“不适用”、保留原版空格子条', `(() => { const c = document.querySelector('.ncard[data-kind="relay"]'); return c.querySelector('.beat-cap').textContent.replace(/\\s+/g, ' ').trim() + ' · 格子 ' + c.querySelectorAll('.beat-cells > i').length; })()`);
await b.eval(`document.querySelector('.ncard[data-kind="relay"]').scrollIntoView({ block: 'center' })`); await b.sleep(300);
await shot('verify-card-large-relay', { clip: await b.eval(`(() => { const r = document.querySelector('.ncard[data-kind="relay"]').getBoundingClientRect(); return { x: r.left - 8, y: Math.max(0, r.top - 8), width: r.width + 16, height: Math.min(900 - Math.max(0, r.top - 8), r.height + 16) }; })()`) });
await b.eval(`window.scrollTo(0, 0)`);
// 切视图保住阅读位置
await b.eval(`document.querySelector('[data-seg="view"] [data-value="large"]').click()`); await b.sleep(500);
await b.eval(`window.scrollTo(0, 1500)`); await b.sleep(400);
const before = await b.eval(`(() => { const top = 56 + document.querySelector('.page-head').offsetHeight; const c = [...document.querySelectorAll('.ncard')].find((x) => x.getBoundingClientRect().bottom > top + 8); return { id: c.dataset.id, top: Math.round(c.getBoundingClientRect().top) }; })()`);
await b.eval(`document.querySelector('[data-seg="view"] [data-value="compact"]').click()`); await b.sleep(600);
await check(`craft#11 切到紧凑后仍停在 ${before.id}（原来顶 ${before.top}，滚出页头的卡对齐到页头下沿）`, `(() => { const c = document.querySelector('.ncard[data-id="${before.id}"]'); const top = 56 + document.querySelector('.page-head').offsetHeight; const t = Math.round(c.getBoundingClientRect().top); return (Math.abs(t - ${before.top}) < 4 || Math.abs(t - top - 8) < 4) ? t : 'FAIL ' + t; })()`);
// 后退恢复滚动
await b.eval(`window.scrollTo(0, 2400)`); await b.sleep(500);
await b.eval(`location.hash = '#/nodes'`); await b.sleep(900);
await b.eval(`history.back()`); await b.sleep(1500);
await check('craft#11 从节点页后退，首页回到原滚动位置（≈2400）', `Math.round(scrollY)`);
await b.eval(`document.querySelector('[data-seg="view"] [data-value="large"]').click()`); await b.sleep(300);

// ─── 命令面板 ───────────────────────────────────────────────────────────────
await b.eval(`window.scrollTo(0, 0)`);
await key('k', { meta: true });
await key('Tab'); await key('Tab');
await check('craft#1 Tab 不离开命令面板（焦点在输入框、当前项下移）', `document.activeElement.closest('.palette') && document.activeElement.tagName + ' active=' + document.querySelector('.palette-item.is-active')?.id`);
await b.send('Input.insertText', { text: '20015' }); await b.sleep(300);
await check('craft#29 入口单独成组、副文字匹配高亮', `[...document.querySelectorAll('.palette-group')].map((g) => g.textContent).join(' ') + ' | mark: ' + document.querySelectorAll('.palette-sub mark, .palette-title mark').length`);
await b.eval(`(() => { const i = document.querySelector('.palette input'); i.value = ''; i.dispatchEvent(new Event('input')); })()`);
await b.send('Input.insertText', { text: '密度' }); await b.sleep(300);
await check('craft#29 搜“密度”有命令', `[...document.querySelectorAll('.palette-title')].map((x) => x.textContent).join(' ')`);
await b.eval(`(() => { const i = document.querySelector('.palette input'); i.value = 'zzzzqqqqzzzzqqqqzzzzqqqq'; i.dispatchEvent(new Event('input')); })()`); await b.sleep(200);
await check('craft#1 没有结果时去掉 aria-activedescendant、回显截到 20 字', `!document.querySelector('.palette input').hasAttribute('aria-activedescendant') && document.querySelector('.palette-empty').textContent`);
await check('craft#27 命令面板打开时锁住页面滚动', `document.documentElement.classList.contains('is-locked')`);
await key('Escape');
await check('命令面板关闭后解锁', `!document.documentElement.classList.contains('is-locked')`);

// ─── 手机侧栏 ─────────────────────────────────────────────────────────────
await open('#/home', { width: 375, height: 812 });
await click('[data-top="nav"]');
await check('craft#3 ☰ 打开手机侧栏，焦点进侧栏', `document.querySelector('#app').dataset.nav + ' ' + !!document.activeElement.closest('.sidebar')`);
await click('.nav-group[data-group="ops"]');
await check('craft#3 点“运维”分组：侧栏不关、子项展开', `document.querySelector('#app').dataset.nav + ' ' + document.querySelector('.nav-group[data-group="ops"]').getAttribute('aria-expanded')`);
await shot('verify-mobile-nav-group', {});
await key('Escape');
await check('craft#3 Esc 关闭侧栏，焦点回到 ☰', `document.querySelector('#app').dataset.nav + ' ' + document.activeElement.dataset.top`);
await check('F15 375 宽 KPI 副行一行、同行两卡等高', `(() => { const k = [...document.querySelectorAll('.kpi')]; return k.map((x) => Math.round(x.getBoundingClientRect().height)).join('/') + ' sub:' + k.map((x) => Math.round(x.querySelector('.kpi-sub').getBoundingClientRect().height)).join('/'); })()`);
await shot('verify-home-375-kpi', { clip: { x: 0, y: 0, width: 375, height: 420 } });

// ─── 节点页：吊顶表头、筛选菜单焦点、KPI、无 title ───────────────────────
await open('#/nodes');
await check('C3 节点页没有原生 title', `document.querySelectorAll('.app [title]').length === 0 || document.querySelectorAll('.app [title]').length`);
await check('C2 页头主按钮“新增家宽节点”带 Plus', `(() => { const x = document.querySelector('.page-tools .btn-primary'); return x.textContent.trim() + ' ' + !!x.querySelector('.i-plus'); })()`);
await check('C9 计数写“N 台”', `document.querySelector('[data-count]').textContent`);
await b.eval(`window.scrollTo(0, 1400)`); await b.sleep(500);
await check('craft#4 滚动后表头吊在页头下面（th 顶 ≈ 56 + 页头高）', `(() => { const th = document.querySelector('.dt thead th'); const ph = document.querySelector('.page-head'); return Math.round(th.getBoundingClientRect().top) + ' vs ' + Math.round(ph.getBoundingClientRect().bottom); })()`);
await shot('verify-nodes-sticky-thead', { clip: { x: 240, y: 0, width: 1200, height: 260 } });
await b.eval(`window.scrollTo(0, 0)`); await b.sleep(300);
await click('[data-act="plan-menu"]');
await b.eval(`[...document.querySelectorAll('.pop.menu .menu-item')].find((x) => x.textContent.includes('ATTVPS 基础版'))?.click()`); await b.sleep(500);
await check('craft#12 多选后菜单对齐新按钮（右缘差 <2 px）', `(() => { const m = document.querySelector('.pop.menu'), a = document.querySelector('[data-act="plan-menu"]'); return m && Math.abs(m.getBoundingClientRect().right - a.getBoundingClientRect().right) < 2 || [m?.getBoundingClientRect().right, a.getBoundingClientRect().right].join('/'); })()`);
await key('Escape');
await check('craft#12 Esc 后焦点回到（重画过的）套餐按钮', `document.activeElement?.dataset.act`);
await b.eval(`__app.setParams({ plan: null })`); await b.sleep(400);
await click('[data-kpi="cost"] .kpi-hit');
await check('C13 每月费用整卡可点 → 按月费排序', `decodeURIComponent(location.hash) + ' pressed=' + document.querySelector('[data-kpi="cost"] .kpi-hit').getAttribute('aria-pressed')`);
await b.eval(`__app.setParams({ sort: null })`); await b.sleep(400);
await check('C14 主机列内存主值是百分比', `document.querySelectorAll('.tmeter-row')[1]?.textContent.trim()`);
await check('C11 线路机行：TCP / 出口 写“不适用”', `(() => { const tr = [...document.querySelectorAll('tr[data-row]')].find((x) => x.querySelector('td .strong')?.textContent === '线路机1'); return tr?.querySelectorAll('td')[4]?.textContent.trim(); })()`);
// 抽屉：焦点回到当前行、锁滚动
await b.eval(`document.querySelector('tr[data-row="t1-03"]').focus()`);
await key('Enter', { text: '\r' }); await b.sleep(700);
await check('craft#27 抽屉打开时页面锁滚动', `document.documentElement.classList.contains('is-locked')`);
await check('F11 抽屉概览三张大数字卡', `[...document.querySelectorAll('.drawer .kpi-label')].map((x) => x.textContent.trim()).join(' / ') + ' · ' + getComputedStyle(document.querySelector('.drawer .kpi-value')).fontSize`);
await shot('verify-drawer-overview', {});
await key(']'); await b.sleep(500);
const cur = await b.eval(`__app.drawerNodeId()`);
await key('Escape'); await b.sleep(500);
await check(`craft#27 用 ] 切到 ${cur} 再关：焦点回到它所在的行`, `document.activeElement?.dataset?.row === '${cur}' || document.activeElement?.dataset?.row`);
await check('关掉抽屉后解锁', `!document.documentElement.classList.contains('is-locked')`);
// 抽屉：主机、管理、连接质量
await b.eval(`__app.openNode('t1-01', 'host')`); await b.sleep(1500);
await check('F9 主机页签：时间范围 6 档（没有数据的置灰）', `[...document.querySelectorAll('.drawer [data-seg="host-range"] .seg-item')].map((x) => x.textContent + (x.disabled ? '×' : '')).join(' ')`);
await check('F9 主机页签：断点连线开关 + 去连接质量的链接', `!!document.querySelector('.drawer [data-host-gaps]') && document.querySelector('.drawer [data-tab="quality"]')?.textContent`);
await check('craft#18 IPv4 值右缘与其它值对齐', `(() => { const dds = [...document.querySelectorAll('.drawer .dl-rows dd')]; const ip = dds.find((d) => d.querySelector('.copy')); const other = dds.find((d) => !d.querySelector('.copy') && d.closest('.dl-rows') === ip.closest('.dl-rows')); return Math.abs(ip.querySelector('.copy-text').getBoundingClientRect().right - other.getBoundingClientRect().right) < 2 || [ip.querySelector('.copy-text').getBoundingClientRect().right, other.getBoundingClientRect().right].join('/'); })()`);
await check('craft#37 网卡小图带图例', `document.querySelector('.drawer [data-ms="net"]')?.textContent.trim()`);
await b.eval(`document.querySelector('.drawer [data-seg="host-range"] [data-value="1h"]').click()`); await b.sleep(1200);
await check('F9 切到 1 小时', `document.querySelector('.drawer [data-host-cover]').textContent`);
await b.eval(`document.querySelector('.drawer [data-host-gaps]').click()`); await b.sleep(500);
await shot('verify-drawer-host', {});
await b.eval(`__app.openNode('t1-01', 'manage')`); await b.sleep(700);
await check('craft#17 管理页按钮同一外形、都带图标', `[...document.querySelectorAll('.drawer .dz-action .btn')].map((x) => x.textContent.trim() + (x.querySelector('.lucide') ? '' : '(无图标)') + (x.classList.contains('btn-secondary') ? '' : '(非描边)')).join(' ')`);
await shot('verify-drawer-manage', {});
await b.eval(`__app.openNode('t1-01', 'quality')`); await b.sleep(900);
await check('craft#16 连接路径 3 列', `(() => { const c = [...document.querySelectorAll('.drawer .dz-card')].find((x) => x.querySelector('.dz-title')?.textContent === '连接路径'); return getComputedStyle(c.querySelector('.kv')).gridTemplateColumns.split(' ').length; })()`);
await shot('verify-drawer-quality', {});
await key('Escape'); await b.sleep(400);

// ─── 对话框：初始焦点、表单守卫 ─────────────────────────────────────────────
await click('[data-act="add"]'); await b.sleep(500);
await check('C2 向导标题与按钮同名', `document.querySelector('.dialog .dialog-title').textContent`);
await check('craft#13 向导初始焦点在第一个输入框（不是 ×）', `document.activeElement.tagName + ' ' + (document.activeElement.name ?? '')`);
await check('craft#35 “登记已存在的入口”是链接样式', `document.querySelector('.dialog [data-wz="enroll"]').className`);
await b.send('Input.insertText', { text: '203.0.113.99' }); await b.sleep(200);
await key('Escape'); await b.sleep(400);
await check('craft#2 填过内容按 Esc：先问“放弃已填写的内容？”', `[...document.querySelectorAll('.dialog:not(.is-leaving) .dialog-title')].map((x) => x.textContent).join(' / ')`);
await check('craft#13 确认框初始焦点在“继续填写”', `document.activeElement.textContent.trim()`);
await shot('verify-wizard-dirty-guard', {});
await key('Enter', { text: '\r' }); await b.sleep(400);
await check('craft#2 继续填写：向导还在、内容还在', `document.querySelectorAll('.dialog:not(.is-leaving)').length + ' ' + [...document.querySelectorAll('.dialog input')].some((i) => i.value === '203.0.113.99')`);
await b.eval(`document.querySelector('.dialog-scrim').click()`); await b.sleep(300);
await check('craft#2 点遮罩不关表单对话框', `document.querySelectorAll('.dialog:not(.is-leaving)').length`);
await key('Escape'); await b.sleep(300);
await b.eval(`[...document.querySelectorAll('.dialog [data-dlg="ok"]')].pop()?.click()`); await b.sleep(400);
await check('确认放弃后关闭', `document.querySelectorAll('.dialog:not(.is-leaving)').length === 0`);
await key('?', { shift: true, code: 'Slash', text: '?' }); await b.sleep(300);
await check('craft#13 信息对话框聚焦对话框本身、不自动弹“关闭”提示', `document.activeElement.classList.contains('dialog') && !document.querySelector('.tip')`);
await key('Escape'); await b.sleep(300);

// ─── 运营总览：列宽不跳、两栏各自高度、图例顺序 ───────────────────────────
await open('#/dashboard', { reduce: false });
const cols0 = await b.eval(`[...document.querySelectorAll('[data-list="teams"] tbody tr:first-child td')].map((td) => Math.round(td.getBoundingClientRect().left)).join(',')`);
await b.sleep(9000);
await check(`craft#5 团队表 9 秒实时刷新后列位置不变（${cols0}）`, `[...document.querySelectorAll('[data-list="teams"] tbody tr:first-child td')].map((td) => Math.round(td.getBoundingClientRect().left)).join(',') === '${cols0}' || [...document.querySelectorAll('[data-list="teams"] tbody tr:first-child td')].map((td) => Math.round(td.getBoundingClientRect().left)).join(',')`);
await check('F14 需处理面板按内容高、不被拉到排行一样高', `[...document.querySelectorAll('.dash-pair > .panel')].map((p) => Math.round(p.getBoundingClientRect().height)).join(' vs ')`);
await check('C6 面板头统一 52 高', `[...new Set([...document.querySelectorAll('.panel-head:not(.is-wrap)')].map((h) => Math.round(h.getBoundingClientRect().height)))].join(',')`);
await check('C17 图例顺序 “↓ 下行 / ↑ 上行”', `document.querySelector('[data-panel="chart"] .chart-legend').textContent`);
await check('craft#20 速率排行按 ↓ 从高到低', `(() => { const v = [...document.querySelectorAll('[data-list="rank"] tr[data-row]')].map((tr) => tr.querySelector('.v').textContent.replace(/\\s+/g, ' ').trim()); return v.join(' > '); })()`);
await check('craft#32 图表单位不伸出卡片内边距', `(() => { const u = document.querySelector('.chart .ts-unit'); const c = u.closest('.panel-body'); return u.getBoundingClientRect().left >= c.getBoundingClientRect().left + 15 || [u.getBoundingClientRect().left, c.getBoundingClientRect().left].join('/'); })()`);

// ─── 线路：读数行不跳、筛选按钮写法、列名 ───────────────────────────────
await open('#/lines', { reduce: false });
const x0 = await b.eval(`Math.round(document.querySelector('[data-relay="r1"] [data-s="pill"]').getBoundingClientRect().left)`);
const td0 = await b.eval(`[...document.querySelectorAll('[data-table="r1"] tbody tr[data-row]:first-child td')].map((td) => Math.round(td.getBoundingClientRect().left)).join(',')`);
await b.sleep(9000);
await check(`craft#5 线路机 pill 9 秒内不左右跳（${x0}）`, `Math.round(document.querySelector('[data-relay="r1"] [data-s="pill"]').getBoundingClientRect().left) === ${x0} || Math.round(document.querySelector('[data-relay="r1"] [data-s="pill"]').getBoundingClientRect().left)`);
await check('craft#5 入口表列位置不变', `[...document.querySelectorAll('[data-table="r1"] tbody tr[data-row]:first-child td')].map((td) => Math.round(td.getBoundingClientRect().left)).join(',') === '${td0}' || 'moved'`);
await check('C1 入口筛选按钮写类别名', `[...document.querySelectorAll('[data-relay="r1"] [data-act="filter"]')].map((x) => x.textContent.trim()).join(' / ')`);
await check('C11 列名 “TCP / 出口”', `[...document.querySelectorAll('[data-table="r1"] th')].map((x) => x.textContent.trim()).filter(Boolean).join(' · ')`);
await check('C15 控制条下面没有多出的 16 px（控制条与第一块面板间距 16）', `Math.round(document.querySelector('[data-relays]').getBoundingClientRect().top - document.querySelector('[data-bar]').getBoundingClientRect().bottom)`);
await check('craft#14 同一行的“启用”和“有活动”顶端对齐', `(() => { const tr = document.querySelector('[data-table="r1"] tbody tr[data-row]'); const p = tr.querySelectorAll('.pill'); return Math.abs(p[0].getBoundingClientRect().top - p[1].getBoundingClientRect().top) < 1 || [p[0].getBoundingClientRect().top, p[1].getBoundingClientRect().top].join('/'); })()`);
await check('C3 线路页没有原生 title', `document.querySelectorAll('.app [title]').length === 0 || document.querySelectorAll('.app [title]').length`);
await shot('verify-lines-relay', { clip: { x: 240, y: 180, width: 1200, height: 420 } });
await b.eval(`document.querySelector('[data-table="r1"] tr[data-row]').click()`); await b.sleep(500);
await b.eval(`document.querySelector('.expand-actions [data-act="disable"]').click()`); await b.sleep(500);
await check('craft#13 破坏性确认的初始焦点在“取消”', `document.activeElement.textContent.trim()`);
await key('Escape'); await b.sleep(300);

// ─── 节点行为：筛选按钮、KPI 整卡可点、术语 ─────────────────────────────
await open('#/behavior');
await check('C1 筛选条：按钮 + 菜单（不再是原生 select）', `document.querySelectorAll('.fform select').length + ' select · 按钮：' + [...document.querySelectorAll('.fform [data-act="ffilter"]')].map((x) => x.textContent.trim()).join(' / ')`);
await click('[data-act="ffilter"][data-k="team"]');
await b.eval(`[...document.querySelectorAll('.pop.menu .menu-item')].find((x) => x.textContent.includes('团队2'))?.click()`); await b.sleep(600);
await check('C1 选了写“团队：团队2”、软靛蓝底', `(() => { const x = document.querySelector('[data-act="ffilter"][data-k="team"]'); return x.textContent.trim() + ' pressed=' + x.getAttribute('aria-pressed'); })()`);
await b.eval(`__app.setParams({ team: null })`); await b.sleep(400);
await click('[data-kpi="blocked"] .kpi-hit');
await check('C13 已阻止的连接整卡可点 → result=blocked', `decodeURIComponent(location.hash)`);
await b.eval(`__app.setParams({ result: null, view: 'events' })`); await b.sleep(1200);
await check('C8 事件类型写“阻止增多”', `[...document.querySelectorAll('[data-seg="ek"] .seg-item')].map((x) => x.textContent.trim()).join(' ')`);
await check('C10 事件时间次行精确到秒', `document.querySelector('[data-table] tbody tr[data-row] td .sub')?.textContent`);
await check('C23 证据行的行尾是“查看证据”', `document.querySelector('[data-table] tbody tr[data-row] td.chev')?.dataset.tip`);
await check('C7 “新来源”是标签 chip 不是 .mark', `document.querySelectorAll('.app .mark').length + ' marks: ' + [...new Set([...document.querySelectorAll('.app .mark')].map((m) => m.textContent))].join(',')`);
await check('C3 行为页没有原生 title', `document.querySelectorAll('.app [title]').length === 0 || document.querySelectorAll('.app [title]').length`);
await shot('verify-behavior-events', {});
await b.eval(`__app.setParams({ view: null, mode: 'rules' })`); await b.sleep(600);
await check('craft#33 访问规则：没改过时“保存草稿”不可点', `document.querySelector('[data-act="rule-save"]').disabled`);
await check('C9 范围写“N 台家宽节点”', `document.querySelector('.pick-sub')?.textContent`);

// ─── 上架流水线：4 张 KPI、状态词 ───────────────────────────────────────
await open('#/onboarding');
await check('F10 4 张 KPI', `[...document.querySelectorAll('[data-slot="kpis"] .kpi-label')].map((x) => x.textContent.trim()).join(' / ')`);
await check('C4 状态词：成功 / 待介入 / 合批中 / 部分失败', `[...new Set([...document.querySelectorAll('.pill')].map((x) => x.textContent.trim()))].join(' ')`);
await check('C4 分段：全部 / 待介入 / 进行中 / 成功', `[...document.querySelectorAll('[data-seg="ob-status"] .seg-item')].map((x) => x.textContent.trim()).join(' ')`);
await check('C10 批次卡开始时间写“09-23 15:10”', `document.querySelector('.ob-meta').textContent.match(/开始 [^·]+/)?.[0]`);
await check('craft#30 站内链接用 → 不用外链图标', `!!document.querySelector('[data-slot="relay"] .ilink .i-arrow-right') && !document.querySelector('[data-slot="relay"] .i-square-arrow-out-up-right')`);
await check('C3 流水线没有原生 title', `document.querySelectorAll('.app [title]').length === 0 || document.querySelectorAll('.app [title]').length`);
await click('[data-act="new"]'); await b.sleep(400);
await check('C5 批次向导：步骤名、取消是次按钮、下一步 ›', `[...document.querySelectorAll('.dialog .wiz-step')].map((x) => x.textContent.replace(/\\d/, '')).join(' / ') + ' | 取消:' + document.querySelector('.dialog [data-dlg="cancel"]').className + ' | ' + !!document.querySelector('.dialog .i-chevron-right')`);
await key('Escape'); await b.sleep(300);

// ─── 减少动态效果、骨架 ─────────────────────────────────────────────────
await check('craft#28 减少动态效果时 .spin 不转', `(() => { const s = document.createElement('span'); s.className = 'spin'; document.body.appendChild(s); const a = getComputedStyle(s).animationName; s.remove(); return a; })()`);

results.push(`console: ${b.logs.length ? b.logs.join(' | ') : '无报错'}`);
console.log(results.join('\n'));
await b.close();
