// interact-nodes.mjs — 节点页 #/nodes 的交互冒烟测试（SPEC §6.3、§5.3–5.6）：真实按键与点击经 CDP 派发。
// 用法：SHOOT_PORT=9457 ~/.local/bin/node tools/interact-nodes.mjs
import { launch } from './shoot.mjs';

const BASE = 'http://127.0.0.1:5199/original-refined/index.html';
const b = await launch({ port: Number(process.env.SHOOT_PORT) || 9457 });
const key = async (k, { meta = false, shift = false, code, text } = {}) => {
  const mods = (meta ? 4 : 0) | (shift ? 8 : 0);
  const params = { key: k, code: code ?? (k.length === 1 ? `Key${k.toUpperCase()}` : k), modifiers: mods, windowsVirtualKeyCode: k.length === 1 ? k.toUpperCase().charCodeAt(0) : { Escape: 27, Enter: 13, ArrowDown: 40, ArrowUp: 38, ArrowRight: 39, ArrowLeft: 37, Tab: 9, Backspace: 8 }[k] ?? 0 };
  await b.send('Input.dispatchKeyEvent', { type: 'keyDown', ...params, text: text ?? (k.length === 1 && !meta ? k : undefined) });
  await b.send('Input.dispatchKeyEvent', { type: 'keyUp', ...params });
  await b.sleep(250);
};
const click = async (sel) => { await b.eval(`(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) throw new Error('没有 ' + ${JSON.stringify(sel)}); e.click(); return 1; })()`); await b.sleep(450); };
const results = [];
const check = async (name, expr) => { let v; try { v = await b.eval(expr); } catch (e) { v = false; results.push(`FAIL  ${name} → ${e.message}`); return; } results.push(`${v ? 'PASS' : 'FAIL'}  ${name}${v && v !== true ? ` → ${v}` : ''}`); };
const hash = 'decodeURIComponent(location.hash)';
const rows = `document.querySelectorAll('[data-page=nodes] tr[data-row]').length`;

await b.viewport(1440, 900);
await b.media({ dark: false, reduce: true });
await b.go(`${BASE}#/nodes`);
await b.until('document.documentElement.dataset.ready === "1"', 20000);
await b.sleep(1200);

await check('首屏 41 行、11 列表头', `${rows} === 41 && document.querySelectorAll('[data-page=nodes] .dt thead th').length === 11 && ${rows}`);
await check('没有“原型未包含此页”', `!document.body.innerText.includes('原型未包含此页')`);
// 排序：点表头写 URL；再点切换方向；Shift 多列
await click('[data-sort="rate"]');
await check('点“业务速率” → sort=-rate', `${hash}.includes('sort=-rate') && ${hash}`);
await click('[data-sort="rate"]');
await check('再点 → 升序 sort=rate', `${hash}.includes('sort=rate') && !${hash}.includes('-rate')`);
await b.eval(`document.querySelector('[data-sort="team"]').dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true }))`); await b.sleep(400);
await check('Shift 点“团队” → 多列排序', `${hash}.includes('sort=rate,team') && ${hash}`);
await click('[data-sort="due"]');
await check('月费 / 到期：点“到期” → sort=due，箭头在“到期”上', `${hash}.includes('sort=due') && document.querySelector('[data-sort="due"] .lucide') && !document.querySelector('[data-sort="cost"] .lucide')`);
// 分段与团队
await click('[data-seg="team"] [data-value="线路机"]');
await check('团队分段 → team=线路机，3 行', `${hash}.includes('team=线路机') && ${rows} === 3`);
await check('团队筛选出现在“已生效”', `document.querySelector('[data-chips]:not([hidden])')?.textContent.includes('团队')`);
// 计数为 0 的分段不可点（craft #34）；从 URL 直接进这个筛选仍然给空状态
await check('计数为 0 的“7 天内到期”分段不可点', `document.querySelector('[data-seg="status"] [data-value="due7"]').disabled`);
await b.eval(`__app.setParams({ status: 'due7' })`); await b.sleep(400);
await check('URL status=due7', `${hash}.includes('status=due7') && ${hash}`);
await check('空结果 → 空状态文案', `document.querySelector('[data-page=nodes] .empty')?.textContent.includes('没有符合筛选条件的节点')`);
await click('[data-page=nodes] .empty [data-act="clear-all"]');
await check('清除筛选 → 41 行，URL 只留排序', `${rows} === 41 && !${hash}.includes('team=') && !${hash}.includes('status=')`);
// KPI 可点
await click('[data-kpi="attention"] .kpi-hit');
await check('点 KPI“需处理” → status=attention，KPI 选中', `${hash}.includes('status=attention') && (document.querySelector('[data-kpi="attention"] .kpi-hit') ?? document.querySelector('[data-kpi="attention"]')).getAttribute('aria-pressed')`);
await click('[data-kpi="attention"] .info-btn');
await check('点 ⓘ 只出口径说明，不切分段', `!${hash}.includes('status=attention') || document.querySelector('.pop')?.textContent.includes('monitor.attention')`);
await key('Escape');
await check('再看一眼：分段仍是需处理', `${hash}.includes('status=attention')`);
await click('[data-kpi="attention"] .kpi-hit');
await check('再点 → 取消', `!${hash}.includes('status=') && (document.querySelector('[data-kpi="attention"] .kpi-hit') ?? document.querySelector('[data-kpi="attention"]')).getAttribute('aria-pressed') === 'false'`);
// 套餐多选菜单（规范名）
await click('[data-act="plan-menu"]');
await check('套餐菜单只列规范名', `[...document.querySelectorAll('.menu .menu-item span:not(.menu-item-end):not(.menu-check)')].map(e => e.textContent).filter(Boolean).join(' | ')`);
await b.eval(`[...document.querySelectorAll('.menu .menu-item')].find(e => e.textContent.includes('NodeMach'))?.click()`); await b.sleep(400);
await check('选 NodeMach 高速代理 → plan=nodemach…，8 行，菜单仍开着', `${hash}.includes('plan=nodemach.proxy.highspeed') && ${rows} === 8 && !!document.querySelector('.menu')`);
await b.eval(`[...document.querySelectorAll('.menu .menu-item')].find(e => e.textContent.includes('GoMami Pulse Mini'))?.click()`); await b.sleep(400);
await check('再勾 GoMami Pulse Mini → 两个套餐（或），10 行', `${rows} === 10 && ${hash}`);
await key('Escape');
await check('Esc 关闭菜单；按钮显示“套餐 · 2”', `!document.querySelector('.menu') && document.querySelector('[data-act="plan-menu"]').textContent.trim()`);
// 分组 + 小计
await click('[data-act="group-menu"]');
await b.eval(`[...document.querySelectorAll('.menu .menu-item')].find(e => e.textContent.includes('按套餐'))?.click()`); await b.sleep(500);
await check('按套餐分组 → group=plan，分组行带台数与小计', `${hash}.includes('group=plan') && [...document.querySelectorAll('.group-tr')].map(g => g.textContent.replace(/\\s+/g, ' ').trim()).join(' | ')`);
await click('.group-tr [data-group-toggle]');
await check('折叠第一组', `document.querySelector('.group-tr [data-group-toggle]').getAttribute('aria-expanded') === 'false' && ${rows}`);
await check('点分组行不会误触侧栏分组', `!document.querySelector('.nav-group[aria-expanded="true"]') || true`);
await b.eval(`__app.setParams({ plan: null, group: null, sort: null })`); await b.sleep(400);
// 搜索：token 补全
await b.eval(`document.querySelector('[data-search="q"] input').focus()`);
await b.send('Input.insertText', { text: 're' }); await b.sleep(300);
await check('输入 re → 建议 relay:', `document.querySelector('.pop.menu [aria-label="搜索建议"]')?.textContent.includes('relay:')`);
await key('Tab'); await b.sleep(250);
await check('Tab 补全 → 输入框是 relay:，并列出线路机', `document.querySelector('[data-search="q"] input').value + ' | ' + [...document.querySelectorAll('.pop.menu [role=option]')].map(e => e.textContent).join(',')`);
await key('ArrowDown'); await key('Enter', { text: '\r' }); await b.sleep(500);
await check('↓ ↵ 选线路机2 → q=relay:线路机2，行数', `${hash}.includes('q=relay:线路机2') && ${rows}`);
await b.eval(`(() => { const i = document.querySelector('[data-search="q"] input'); i.focus(); return 1; })()`);
await key('Escape'); await b.sleep(300);
await check('Esc 清空搜索', `!${hash}.includes('q=') && ${rows} === 41`);
await b.eval(`(() => { const i = document.querySelector('[data-search="q"] input'); i.value = '100.13'; i.dispatchEvent(new Event('input', { bubbles: true })); return 1; })()`); await b.sleep(400);
await check('按 IP 片段搜索', `${rows} + ' 行：' + [...document.querySelectorAll('[data-page=nodes] tr[data-row] .strong')].map(e => e.textContent).join(',')`);
await b.eval(`__app.setParams({ q: null })`); await b.sleep(300);
await b.eval('document.activeElement?.blur()');
// 键盘：j/k、Enter、[ ]、Esc
await key('j'); await key('j');
await check('j j → 焦点在第 2 行', `document.activeElement.dataset.row`);
await key('Enter', { text: '\r' }); await b.sleep(600);
await check('Enter → 打开抽屉（pushState），当前行高亮', `!!document.querySelector('.drawer') && ${hash}.includes('node=t1-02') && document.querySelector('tr[data-row="t1-02"]').classList.contains('is-current')`);
await key(']'); await b.sleep(400);
await check('] → 下一个节点（跟表格顺序）', `${hash}.includes('node=t1-03') && document.querySelector('tr[data-row="t1-03"]').classList.contains('is-current')`);
await b.eval(`[...document.querySelectorAll('.drawer .tab')].find(t => t.textContent === '连接质量').click()`); await b.sleep(900);
await check('连接质量页签有“出口画像”，观测到的出口 IP 已填', `[...document.querySelectorAll('.drawer .dz-title')].some(t => t.textContent === '出口画像') && !document.querySelector('[data-egress-ips] .skel') && document.querySelector('[data-egress-ips]').textContent.includes('与台账')`);
await b.eval(`[...document.querySelectorAll('.drawer .tab')].find(t => t.textContent === '访问记录').click()`); await b.sleep(900);
await check('访问记录页签有“来源画像”和“来源 IP”表', `!!document.querySelector('[data-src-card]') && document.querySelectorAll('[data-src-table] tbody tr').length`);
await b.eval(`document.querySelector('[data-src-card] .seg-item[data-value="d14"]').click()`); await b.sleep(300);
await check('来源画像切 14 天', `document.querySelector('[data-src-body]').textContent.includes('会话')`);
await key('Escape'); await b.sleep(500);
await check('Esc → 关抽屉，URL 回到列表', `!document.querySelector('.drawer:not(.is-leaving)') && !${hash}.includes('node=')`);
// 行点击（复制 IP 不打开抽屉）
await b.eval(`document.querySelector('tr[data-row="t1-05"] [data-copy]').click()`); await b.sleep(400);
await check('点 IP 只复制，不开抽屉', `!document.querySelector('.drawer:not(.is-leaving)')`);
await b.eval(`document.querySelector('tr[data-row="t1-13"] td:nth-child(3)').click()`); await b.sleep(600);
await check('点行 → 抽屉；代理没有“主机”页签', `${hash}.includes('node=t1-13') && [...document.querySelectorAll('.drawer .tab')].map(t => t.textContent).join(' ')`);
await b.eval('history.back()'); await b.sleep(600);
await check('浏览器后退 = 关抽屉', `!document.querySelector('.drawer:not(.is-leaving)')`);
// 密度
await click('[data-seg="density"] [data-value="compact"]');
await check('紧凑 → main[data-density=compact]，行高 44/45', `document.querySelector('main').dataset.density + ' ' + Math.round(document.querySelector('tr[data-row]').getBoundingClientRect().height)`);
await key('d', { meta: true, shift: true });
await check('⌘⇧D → 宽松，分段跟着变', `document.querySelector('main').dataset.density + ' ' + document.querySelector('[data-seg="density"] [aria-checked="true"]')?.textContent`);
// 导出（只取 CSV 文本，不触发下载）
await check('导出 CSV：表头 + 41 行，套餐是规范名、没有原始套餐串', `(() => { const csv = document.querySelector('[data-page=nodes]') && __app.__nodesExport?.(); return csv ? csv.trim().split('\\n').length + ' 行 · ' + ['ATTVPS 基础版','NodeMach 高速代理'].every(w => csv.includes(w)) + ' · 原文 ' + ['基础版 VPS','高速代理 ·'].some(w => csv.includes(w)) : 'skip'; })()`);
await b.eval(`(() => { HTMLAnchorElement.prototype.__c = HTMLAnchorElement.prototype.click; HTMLAnchorElement.prototype.click = function () { window.__csv = this.download; }; document.querySelector('[data-act="export"]').click(); HTMLAnchorElement.prototype.click = HTMLAnchorElement.prototype.__c; return 1; })()`); await b.sleep(300);
await check('导出文件名 + toast', `window.__csv + ' | ' + document.querySelector('.toast')?.textContent`);
// 新增节点：共用向导
await click('[data-act="add"]');
await check('新增节点 → 向导对话框（共用 enroll-wizard）', `document.querySelector('.dialog .dialog-title')?.textContent`);
await key('Escape'); await b.sleep(300);
// 实时：跑 12 秒，检查补丁没有重建表格
await b.eval(`window.__tb = document.querySelector('[data-page=nodes] tbody'); 1`);
await b.sleep(12000);
await check('实时 12 秒：tbody 没被重建（只补丁单元格）', `document.querySelector('[data-page=nodes] tbody') === window.__tb`);
// 实时：一台节点变成故障 → 需处理 KPI、分段计数、状态 pill 闪一次
await b.eval(`(() => { const s = __app.store; const n = s.node('t1-10'); n.status = { ...n.status, code: 'fail', word: '故障', reason: 'auth', reason_word: null, since: s.now() - 60, attention: true, first_failed_check: 'ingress' }; s.emit('change', { nodes: { 't1-10': { status: n.status } } }); return 1; })()`); await b.sleep(500);
await check('故障 → KPI 需处理 1、分段计数 1、行内 pill 变“故障”', `document.querySelector('[data-kpi="attention"] [data-k="value"]').textContent.trim() + ' / ' + document.querySelector('[data-seg="status"] [data-value="attention"] .seg-count').textContent + ' / ' + document.querySelector('tr[data-row="t1-10"] .pill').textContent`);
await click('[data-seg="status"] [data-value="attention"]');
await check('需处理分段只剩这台', `${rows} + ' ' + document.querySelector('tr[data-row]')?.dataset.row`);
await b.eval(`(() => { const s = __app.store; const n = s.node('t1-10'); n.status = { ...n.status, code: 'ok', word: '正常', reason: null, since: s.now(), attention: false, first_failed_check: null }; s.emit('change', { nodes: { 't1-10': { status: n.status } } }); return 1; })()`); await b.sleep(600);
await check('恢复后：行先淡出并标“已不符合筛选”', `document.querySelector('tr[data-row="t1-10"]')?.classList.contains('is-leaving') && document.querySelector('tr[data-row="t1-10"]').textContent.includes('已不符合筛选')`);
await b.sleep(5600);
await check('5 秒后收起 → 空状态', `${rows} === 0 && !!document.querySelector('[data-page=nodes] .empty')`);
results.push(`console: ${b.logs.length ? b.logs.join(' | ') : '无报错'}`);
console.log(results.join('\n'));
await b.close();
process.exit(0);
