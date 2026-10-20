// interact-onboarding.mjs — 上架流水线的交互与验收检查（真实按键经 CDP 派发；只访问 127.0.0.1:5199）。
// 用法：~/.local/bin/node tools/interact-onboarding.mjs
// 覆盖：筛选写 URL、KPI 点击筛选、j/k/Enter 开抽屉、[ ] 换步骤、Esc 与后退关抽屉、提交密码、重试、再等、放弃（L2）、
//      改 IP、合批窗口到点合并、新建批次向导全流程、V3 字号/字重/圆角/控件高、V4 术语、V5 套餐原文、V6 布局位移、控制台。
import { launch } from './shoot.mjs';

const BASE = 'http://127.0.0.1:5199/original-refined/index.html';
async function freePort() {
  for (let i = 0; i < 40; i++) {
    const port = 9700 + Math.floor(Math.random() * 100);
    try { await fetch(`http://127.0.0.1:${port}/json/version`, { signal: AbortSignal.timeout(300) }); } catch { return port; }
  }
  throw new Error('没有空闲的 CDP 端口');
}
const b = await launch({ port: await freePort() });
const key = async (k, { meta = false, shift = false, code, text } = {}) => {
  const mods = (meta ? 4 : 0) | (shift ? 8 : 0);
  const params = { key: k, code: code ?? (k.length === 1 ? (/[a-z]/i.test(k) ? `Key${k.toUpperCase()}` : k === '[' ? 'BracketLeft' : k === ']' ? 'BracketRight' : k) : k), modifiers: mods, windowsVirtualKeyCode: k.length === 1 ? ({ '[': 219, ']': 221 }[k] ?? k.toUpperCase().charCodeAt(0)) : { Escape: 27, Enter: 13, ArrowDown: 40, ArrowUp: 38, Tab: 9 }[k] ?? 0 };
  await b.send('Input.dispatchKeyEvent', { type: 'keyDown', ...params, text: text ?? (k.length === 1 && !meta ? k : undefined) });
  await b.send('Input.dispatchKeyEvent', { type: 'keyUp', ...params });
  await b.sleep(250);
};
const results = [];
let fails = 0;
const check = async (name, expr) => { let v; try { v = await b.eval(expr); } catch (e) { v = false; results.push(`ERR   ${name}: ${e.message.slice(0, 160)}`); } if (!v) fails++; results.push(`${v ? 'PASS' : 'FAIL'}  ${name}${v && v !== true ? ` → ${String(v).slice(0, 200)}` : ''}`); };
const click = (sel) => b.eval(`(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return false; e.click(); return true; })()`);
const load = async (hash, { w = 1440, h = 900, dark = false } = {}) => {
  await b.viewport(w, h); await b.media({ dark, reduce: true });
  await b.go('about:blank'); await b.go(`${BASE}${hash}`);
  await b.until('document.documentElement.dataset.ready === "1"', 20000);
  await b.sleep(1200);
};

await load('#/onboarding');
await check('页面标题', `document.querySelector('.page-title').textContent`);
await check('矩阵 8 行 + 3 条常驻说明行', `document.querySelectorAll('[data-slot=matrix] tr[data-row]').length === 8 && document.querySelectorAll('[data-slot=matrix] tr.note-tr').length === 3`);
await check('失败行 团队1-42 有 bad 色线', `document.querySelector('tr[data-row="团队1-42"]').dataset.attn === 'bad'`);
await check('等待输入行 团队1-41 有 warn 色线', `document.querySelector('tr[data-row="团队1-41"]').dataset.attn === 'warn'`);
await check('7 个阶段列表头 + 汇总', `[...document.querySelectorAll('[data-slot=matrix] thead .th-2l')].map(e => e.textContent).join(' | ')`);
await check('单元格状态齐全（成功/进行中/等待输入/失败/已停止/跳过/排队）', `(() => { const s = new Set([...document.querySelectorAll('[data-slot=matrix] .step')].map(e => e.dataset.step)); return ['succeeded','running','waiting_input','failed','blocked','skipped','queued'].every(x => s.has(x)) && [...s].join(','); })()`);
await check('KPI 4 张（成功 / 进行中 / 待介入 / 预计剩余，fidelity #10）', `[...document.querySelectorAll('[data-slot=kpis] .kpi-label > span:first-child')].map(e => e.textContent).join(' ')`);
await check('KPI 主值', `[...document.querySelectorAll('[data-slot=kpis] .kpi-value')].map(e => e.textContent.trim()).join(' | ')`);

// 筛选：“待介入”含等待输入、风险停止、失败（consistency #4）
await click('[data-kpi="attention"] .kpi-hit'); await b.sleep(500);
await check('KPI“待介入”点击 → status=attention，剩 3 行（41 等待输入 · 42 失败 · 43 风险）', `location.hash.includes('status=attention') && [...document.querySelectorAll('[data-slot=matrix] tr[data-row]')].map(t => t.dataset.row).join(',')`);
await click('[data-kpi="attention"] .kpi-hit'); await b.sleep(400);
await check('再点一次取消筛选', `!location.hash.includes('status=') && document.querySelectorAll('[data-slot=matrix] tr[data-row]').length === 8`);
await click('[data-seg="ob-status"] [data-value=""]'); await b.sleep(300);
await b.eval(`(() => { const i = document.querySelector('[data-search="q"] input'); i.focus(); i.value = '1-44'; i.dispatchEvent(new Event('input', { bubbles: true })); return 1; })()`); await b.sleep(700);
await check('搜索 1-44 写进 URL 并过滤', `decodeURIComponent(location.hash).includes('q=1-44') && document.querySelectorAll('[data-slot=matrix] tr[data-row]').length === 1`);
await b.eval(`(() => { const i = document.querySelector('[data-search="q"] input'); i.value = ''; i.dispatchEvent(new Event('input', { bubbles: true })); i.blur(); return 1; })()`); await b.sleep(700);
await click('[data-seg="ob-cols"] [data-value="steps"]'); await b.sleep(400);
await check('步骤 16 列（首列固定）', `location.hash.includes('cols=steps') && document.querySelectorAll('[data-slot=matrix] thead .th-2l').length === 16 && document.querySelector('.ob-matrix').classList.contains('is-pin-first')`);
await click('[data-seg="ob-cols"] [data-value="phases"]'); await b.sleep(400);

// 键盘与抽屉
await b.eval('document.activeElement?.blur(); document.body.focus(); 1');
await key('j'); await key('j'); await key('j');
await check('j j j → 焦点在第 3 行', `document.activeElement.closest('tr[data-row]')?.dataset.row`);
await key('Enter', { text: '\r' }); await b.sleep(600);
await check('Enter → 步骤抽屉（当前步）+ URL', `document.querySelector('.drawer .drawer-title')?.textContent + ' ' + decodeURIComponent(location.hash)`);
await key(']'); await b.sleep(400);
await check('] → 下一步', `document.querySelector('.drawer .drawer-title')?.textContent + ' ' + decodeURIComponent(location.hash)`);
await key('['); await key('['); await b.sleep(400);
await check('[ [ → 上一步', `document.querySelector('.drawer .drawer-title')?.textContent`);
await click('.drawer .tab[data-value="timeline"]'); await b.sleep(400);
await check('时间线页签 16 项', `document.querySelectorAll('.drawer .tl-item').length === 16 && decodeURIComponent(location.hash).includes('stab=timeline')`);
await click('.drawer [data-tl="P6"]'); await b.sleep(400);
await check('点时间线 P6 → 切到 P6 日志', `document.querySelector('.drawer .drawer-title')?.textContent + ' ' + document.querySelector('.drawer .tab[aria-selected=true]').textContent`);
await key('Escape'); await b.sleep(500);
await check('Esc → 关抽屉并回到原 URL', `!document.querySelector('.drawer:not(.is-leaving)') && !location.hash.includes('row=')`);
await click('[data-slot=matrix] tr[data-row="团队1-40"] .step[data-step="running"]'); await b.sleep(600);
await check('点进行中的格子 → 该步抽屉', `document.querySelector('.drawer .drawer-title')?.textContent`);
const n1 = await b.eval(`document.querySelectorAll('.drawer .log-line').length`);
await b.eval('__app.live.tick(60); 1'); await b.sleep(1600);
await check('进行中的日志随时间追加（跟随末尾）', `document.querySelectorAll('.drawer .log-line').length > ${n1} && document.querySelectorAll('.drawer .log-line').length`);
await b.eval('history.back()'); await b.sleep(600);
await check('浏览器后退 = 关抽屉', `!document.querySelector('.drawer:not(.is-leaving)') && !location.hash.includes('row=')`);

// 需处理的行：提交密码
await b.eval(`(() => { const i = document.querySelector('[data-secret-for="团队1-41"]'); i.value = 'not-a-real-password'; return 1; })()`);
await click('[data-ract="secret"][data-rowid="团队1-41"]'); await b.sleep(700);
await check('提交密码 → 团队1-41 转为进行中、说明行消失', `document.querySelector('tr[data-row="团队1-41"]').dataset.attn === undefined && !document.querySelector('tr[data-after="团队1-41"]') && document.querySelector('tr[data-row="团队1-41"] .pill').textContent`);
await check('密码不出现在页面文字里', `!document.body.innerText.includes('not-a-real-password')`);
// 失败行：再等 10 分钟
await click('[data-ract="wait"][data-rowid="团队1-42"]'); await b.sleep(600);
await check('再等 10 分钟 → 团队1-42 重装格回到进行中', `document.querySelector('tr[data-row="团队1-42"] .step[data-step="running"]') && document.querySelector('.toast')?.textContent`);
// 风险行：改 IP
await click('[data-ract="ip"][data-rowid="团队1-43"]'); await b.sleep(500);
await b.eval(`(() => { const i = document.querySelector('[data-newip]'); i.value = '198.51.100.31'; return 1; })()`);
await click('[data-ipok]'); await b.sleep(300);
await check('改 IP：重复 IP 被拦下', `document.querySelector('[data-iperr]')?.textContent`);
await b.eval(`(() => { const i = document.querySelector('[data-newip]'); i.value = '198.51.100.221'; return 1; })()`);
await click('[data-ipok]'); await b.sleep(700);
await check('改 IP → 团队1-43 从预检重新开始', `document.querySelector('tr[data-row="团队1-43"] .step[data-step="running"]') && document.querySelector('tr[data-row="团队1-43"] .sub').textContent`);
// 放弃此行（L2）：打开一个还在跑的行的菜单
await click('[data-act="row-menu"][data-rowid="团队1-40"]'); await b.sleep(300);
await b.eval(`[...document.querySelectorAll('.menu-item')].find(e => e.textContent.includes('放弃此行')).click(); 1`); await b.sleep(400);
await check('放弃此行 → L2 确认，未输入名称时按钮不可用', `document.querySelector('.dialog [data-dlg="ok"]').disabled`);
await b.eval(`(() => { const i = document.querySelector('[data-type-confirm]'); i.value = '团队1-40'; i.dispatchEvent(new Event('input', { bubbles: true })); document.querySelector('.dialog [data-dlg="ok"]').click(); return 1; })()`); await b.sleep(700);
await check('放弃后 团队1-40 为已取消', `document.querySelector('tr[data-row="团队1-40"] .pill').textContent`);

// 合批窗口到点：快进 50 秒
await b.eval('__app.live.tick(50); 1'); await b.sleep(1500);
await check('窗口结束 → 合并中', `[...document.querySelectorAll('[data-slot=relay] tbody tr')].map(t => t.innerText.replace(/\\s+/g, ' ')).join(' / ')`);
await b.eval('__app.live.tick(6); 1'); await b.sleep(1600);
await check('合并完成 → 团队1-39 进入探针与监测，线路机2 Xray 重载 1 次', `document.querySelector('tr[data-row="团队1-39"] .sub')?.textContent + ' | ' + [...document.querySelectorAll('[data-slot=relay] tbody tr')].map(t => t.innerText.replace(/\\s+/g, ' ')).join(' / ')`);

// 新建批次向导
await load('#/onboarding');
await key('n'); await b.sleep(500);
await check('n → 新建批次对话框', `document.querySelector('.dialog .dialog-title')?.textContent`);
await click('[data-nb="sample"]'); await b.sleep(300);
await check('填入示例：识别计数', `document.querySelector('[data-nb-count]').textContent`);
await check('粘贴的密码已遮住', `!document.querySelector('[data-nb-text]').value.includes('demo-root') && document.querySelector('[data-nb-text]').value.includes('••••')`);
await click('[data-nb="next"]'); await b.sleep(300);
await check('预览：重复 IP 报错且不能继续', `document.querySelector('[data-nb-row] .field-error')?.textContent + ' | next disabled=' + document.querySelector('[data-nb="next"]').disabled`);
await check('预览套餐下拉只有规范名', `[...document.querySelector('[data-nb-f="plan_id"]').options].map(o => o.textContent).join(' / ')`);
await b.eval(`[...document.querySelectorAll('[data-nb-row]')].find(t => t.querySelector('[data-nb-f=ip]').value === '198.51.100.31').querySelector('[data-nb-del]').click(); 1`); await b.sleep(300);
await check('删掉重复行后可以继续', `!document.querySelector('[data-nb="next"]').disabled && document.querySelectorAll('[data-nb-row]').length`);
await click('[data-nb="next"]'); await b.sleep(300);
await check('选项：线路机分配', `document.querySelector('[data-nb-alloc]').textContent`);
await click('[data-nb="next"]'); await b.sleep(300);
await check('确认页主按钮', `document.querySelector('[data-nb="run"]').textContent`);
await click('[data-nb="run"]'); await b.sleep(1500);
await check('开始 → 新批次成为当前批次、金丝雀在跑、其余等闸门', `document.querySelector('[data-slot=batch] .panel-title').textContent + ' | ' + document.querySelectorAll('[data-slot=matrix] .step[data-step="running"]').length + ' running, ' + document.querySelectorAll('[data-slot=matrix] .step[data-step="waiting_gate"]').length + ' gate'`);
await check('批次列表多了一条', `document.querySelector('[data-slot=list] .panel-count').textContent`);
await click('[data-slot=list] tr[data-row^="batch:b-0923-1509"]'); await b.sleep(600);
await check('点列表里的演示批次 → 切回演示批次', `decodeURIComponent(location.hash) + ' | ' + document.querySelector('[data-slot=batch] .panel-title').textContent`);
await click('[data-slot=list] tr[data-row="task:2162d60c"]'); await b.sleep(600);
await check('点旧任务 → 任务抽屉（已回滚）', `document.querySelector('.drawer .drawer-title')?.textContent + ' ' + document.querySelector('.drawer .pill')?.textContent`);
await key('Escape'); await b.sleep(400);

// V3 / V4 / V5 / V8 与控制台
const RUNTIME = `(() => {
  const FS = new Set(['11px','12px','13px','15px','16px','24px','27px']);
  const FW = new Set(['400','500','600']);
  const RADII = new Set(['0px','4px','8px','10px','14px','999px','50%']);
  const bad = { fs: new Map(), fw: new Map(), radius: new Map(), height: [] };
  const add = (m, k, el) => { if (!m.has(k)) m.set(k, (el.className?.baseVal ?? el.className ?? el.tagName) + ''); };
  for (const el of document.querySelectorAll('body *')) {
    if (el.closest('svg')) continue;
    const r = el.getBoundingClientRect();
    if (!r.width && !r.height) continue;
    const cs = getComputedStyle(el);
    const hasText = [...el.childNodes].some((n) => n.nodeType === 3 && n.textContent.trim());
    if (hasText && !FS.has(cs.fontSize)) add(bad.fs, cs.fontSize, el);
    if (hasText && !FW.has(cs.fontWeight)) add(bad.fw, cs.fontWeight, el);
    for (const c of ['borderTopLeftRadius','borderTopRightRadius','borderBottomLeftRadius','borderBottomRightRadius']) { const v = cs[c]; if (v && !RADII.has(v) && !(parseFloat(v) >= 999) && !v.endsWith('%')) add(bad.radius, v, el); }
    if (el.matches('.btn:not(.btn-link), .icon-btn, .input, .select, .seg, .fchip, .top-btn, .search-trigger')) { const hh = Math.round(r.height); if (![20,28,32,36].includes(hh)) bad.height.push((el.className?.baseVal ?? el.className) + ' ' + hh); }
  }
  return JSON.stringify({ fontSizes: [...bad.fs], fontWeights: [...bad.fw], radii: [...bad.radius], heights: bad.height.slice(0, 10) });
})()`;
const TERMS = `(async () => {
  const { fmt } = await import('/shared/index.js');
  const text = document.body.innerText;
  const extra = ['异常', '节点可用', '连接不稳定', '上传', '下载', '出站', '入站', 'KiB/s', 'MiB/s'];
  const rawPlans = ['基础版 VPS', '标准版 VPS', 'LAX.Pulse', 'Acapulco', '2000 GB 出站', '1000 GB 出站', '高速代理 ·', '原文'];
  const bad = [...fmt.lintTerms(text), ...extra.filter((w) => text.includes(w)), ...rawPlans.filter((w) => text.includes(w))];
  return bad.length ? 'BAD ' + bad.join(',') : 'ok';
})()`;
for (const [hash, w, dark, extra] of [
  ['#/onboarding', 1440, false, ''], ['#/onboarding', 1440, true, ''], ['#/onboarding', 390, false, ''], ['#/onboarding?cols=steps', 1440, false, ''],
  ['#/onboarding?row=团队1-42&step=P6', 1440, false, ''], ['#/onboarding?row=团队1-43&step=P1&stab=timeline', 1440, true, ''], ['#/onboarding?row=团队1-37&step=P13&stab=io', 1440, false, ''],
  ['#/onboarding?task=c753f43f', 1440, false, ''], ['#/onboarding?task=2162d60c&ttab=log', 1440, false, ''],
  ['#/onboarding', 1440, false, "document.querySelector('[data-act=new]').click(); await new Promise(r=>setTimeout(r,300)); document.querySelector('[data-nb=sample]').click(); document.querySelector('[data-nb=next]').click();"],
]) {
  b.logs.length = 0;
  await load(hash, { w, h: 900, dark });
  if (extra) { await b.eval(`(async () => { ${extra} return 1; })()`); await b.sleep(500); }
  const rt = await b.eval(RUNTIME);
  const terms = await b.eval(TERMS);
  const ov = await b.eval('document.documentElement.scrollWidth - document.documentElement.clientWidth');
  const okRt = rt === JSON.stringify({ fontSizes: [], fontWeights: [], radii: [], heights: [] });
  if (!okRt || terms !== 'ok' || ov || b.logs.length) fails++;
  results.push(`${okRt && terms === 'ok' && !ov && !b.logs.length ? 'PASS' : 'FAIL'}  V3/V4/V5/V8/V13 ${decodeURIComponent(hash)} ${w}${dark ? ' dark' : ''}${extra ? ' +对话框' : ''} → 运行时 ${okRt ? 'ok' : rt} · 术语 ${terms} · 横向溢出 ${ov} · 控制台 ${b.logs.length ? b.logs.join(' | ') : '无'}`);
}
// V6：实时更新 30 秒的累计布局位移（含合批窗口到点、计时文字每秒变化）
await load('#/onboarding');
await b.eval(`(() => { window.__cls = 0; new PerformanceObserver((l) => { for (const e of l.getEntries()) if (!e.hadRecentInput) window.__cls += e.value; }).observe({ type: 'layout-shift', buffered: false }); return 1; })()`);
await b.sleep(30000);
await check('V6 30 秒累计布局位移 < 0.01', `window.__cls < 0.01 && window.__cls.toFixed(4)`);
results.push(`console: ${b.logs.length ? b.logs.join(' | ') : '无报错'}`);
results.push(`${fails ? `${fails} 项未通过` : '全部通过'}`);
console.log(results.join('\n'));
await b.close();
process.exit(0);
