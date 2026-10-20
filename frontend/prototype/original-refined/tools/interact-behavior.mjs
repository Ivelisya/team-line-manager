// interact-behavior.mjs — 节点行为页（#/behavior）的交互冒烟测试 + 状态截图（真实按键经 CDP 派发，只访问本地 5199）。
// 用法：~/.local/bin/node tools/interact-behavior.mjs     截图写到 shots/behavior-*.png
import path from 'node:path';
import { launch } from './shoot.mjs';

const BASE = 'http://127.0.0.1:5199/original-refined/index.html';
const SHOTS = path.join(path.dirname(path.dirname(new URL(import.meta.url).pathname)), 'shots');
const b = await launch({ port: Number(process.env.SHOOT_PORT) || 9393 });
const key = async (k, { meta = false, shift = false, code, text } = {}) => {
  const mods = (meta ? 4 : 0) | (shift ? 8 : 0);
  const params = { key: k, code: code ?? (k.length === 1 ? `Key${k.toUpperCase()}` : k), modifiers: mods, windowsVirtualKeyCode: k.length === 1 ? k.toUpperCase().charCodeAt(0) : { Escape: 27, Enter: 13, ArrowDown: 40, ArrowUp: 38, ArrowRight: 39, ArrowLeft: 37, Tab: 9 }[k] ?? 0 };
  await b.send('Input.dispatchKeyEvent', { type: 'keyDown', ...params, text: text ?? (k.length === 1 && !meta ? k : undefined) });
  await b.send('Input.dispatchKeyEvent', { type: 'keyUp', ...params });
  await b.sleep(250);
};
const results = [];
const check = async (name, expr) => { let v; try { v = await b.eval(expr); } catch (e) { v = false; results.push(`ERR   ${name}: ${e.message.slice(0, 160)}`); return; } results.push(`${v ? 'PASS' : 'FAIL'}  ${name}${v && v !== true ? ` → ${v}` : ''}`); };
const click = (sel) => b.eval(`(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return false; e.click(); return true; })()`);
const shot = (name) => b.shot(path.join(SHOTS, `${name}.png`));
const hash = 'decodeURIComponent(location.hash)';
const open = async (h, { w = 1440, hgt = 900, dark = false } = {}) => {
  await b.viewport(w, hgt);
  await b.media({ dark, reduce: true });
  await b.go('about:blank');
  await b.go(`${BASE}${h}`);
  await b.until('document.readyState === "complete" && document.documentElement.dataset.ready === "1"', 20000);
  await b.until(`!!document.querySelector('[data-table] .dt tbody tr[data-row]') || !!document.querySelector('[data-table] .empty') || !!document.querySelector('.rules-grid')`, 15000);
  await b.sleep(600);
};

await b.viewport(1440, 900);
await b.go(`${BASE}#/home`);
await b.until('document.readyState === "complete"');
await b.eval(`(() => { try { localStorage.removeItem('refined:nav'); localStorage.setItem('refined:theme', '"system"'); localStorage.removeItem('refined:density'); } catch (e) {} return 1; })()`);

// 1. 首屏：页签计数、KPI、表
await open('#/behavior');
await check('页签带计数（域名与 IP · 来源地址 · 节点概览）', `[...document.querySelectorAll('[data-tabs="view"] .tab')].map(t => t.textContent).join(' | ')`);
await check('KPI 四张：有活动的节点 · 连接记录 · 访问目标 · 已阻止的连接', `[...document.querySelectorAll('[data-kpis] .kpi-label > span:first-child')].map(e => e.textContent).join(' · ')`);
await check('表格首行 = chatgpt.com · 团队1-01', `document.querySelector('[data-table] tbody tr').innerText.replace(/\\s+/g, ' ').slice(0, 60)`);
await check('行为变化页签有未读圆点', `!!document.querySelector('[data-tabs="view"] .tab[data-value="events"] .tab-unread')`);

// 2. 行 → 证据对话框
await b.eval(`document.querySelector('[data-table] tbody tr td:nth-child(3)').click()`); await b.sleep(500);
await check('点行 → 证据对话框（标题 = 目标，副标题 = 节点）', `document.querySelector('.dialog .dialog-title')?.textContent + ' / ' + document.querySelector('.dialog .dialog-sub')?.textContent`);
await check('证据对话框有逐条记录与来源画像', `document.querySelectorAll('.dialog .dt tbody tr').length + ' 行 · ' + (document.querySelector('.dialog .src-line')?.textContent ?? '')`);
await shot('behavior-evidence-light-1440');
await key('Escape'); await b.sleep(300);
await check('Esc 关闭对话框', `!document.querySelector('.dialog:not(.is-leaving)')`);

// 3. 搜索：防抖写 URL、Esc 清空
await b.eval(`document.querySelector('[data-search="q"] input').focus()`);
await b.send('Input.insertText', { text: 'apple' }); await b.sleep(700);
await check('搜索 apple（防抖 400 ms）写进 URL，只剩 apple 的行', `${hash}.includes('q=apple') && [...document.querySelectorAll('[data-table] tbody tr[data-row]')].every(r => r.dataset.row.includes('apple')) && document.querySelectorAll('[data-table] tbody tr[data-row]').length`);
await check('搜索时焦点仍在输入框', `document.activeElement?.matches('[data-search="q"] input')`);
await key('Escape'); await b.sleep(300);
await check('Esc 清空搜索', `!${hash}.includes('q=') && document.querySelector('[data-search="q"] input').value === ''`);

// 4. 团队 / 节点下拉联动
// 筛选控件是“按钮 + 菜单”（与节点页同一种，consistency #1）
await b.eval(`document.querySelector('[data-act="ffilter"][data-k="team"]').click()`); await b.sleep(300);
await b.eval(`[...document.querySelectorAll('.pop.menu .menu-item')].find((x) => x.textContent.includes('团队2')).click()`); await b.sleep(500);
await b.eval(`document.querySelector('[data-act="ffilter"][data-k="asset"]').click()`); await b.sleep(300);
await check('团队菜单写 URL，节点菜单只剩团队2', `${hash}.includes('team=团队2') && [...document.querySelectorAll('.pop.menu .menu-item')].map(o => o.textContent.trim()).join(',')`);
await key('Escape');
await check('KPI 带“· 已筛选”', `document.querySelectorAll('[data-kpis] .kpi-filtered').length`);
await click('[data-act="clear"]'); await b.sleep(400);
await check('清除 → 回到默认 URL', `${hash}`);

// 5. 名称 ◎ → 节点筛选 + 选择上下文条
await b.eval(`document.querySelector('[data-table] [data-focus="t1-01"]').click()`); await b.sleep(900);
await check('点节点名 → asset=t1-01，出现上下文条', `${hash}.includes('asset=t1-01') && document.querySelector('.ctxbar')?.innerText.replace(/\\s+/g, ' ').slice(0, 70)`);
await check('KPI 变成这个节点的数', `document.querySelector('[data-kpi="conn"] .kpi-value').textContent.trim()`);
await click('[data-act="ctx-open"]'); await b.sleep(600);
await check('上下文条“节点详情” → 抽屉访问记录页签', `!!document.querySelector('.drawer') && ${hash}`);
await check('抽屉访问记录页签的链接指向 asset=', `document.querySelector('.drawer a[href*="#/behavior"]')?.getAttribute('href')`);
await key('Escape'); await b.sleep(500);
await click('[data-act="ctx-clear"]'); await b.sleep(400);

// 6. 行为变化：类型分段、标记已查看（L0 + 撤销）
await b.eval(`document.querySelector('[data-tabs="view"] .tab[data-value="events"]').click()`); await b.sleep(500);
await check('切到行为变化，写 view=events', `${hash}.includes('view=events') && document.querySelectorAll('[data-table] tbody tr[data-row]').length`);
await b.eval(`document.querySelector('[data-seg="ek"] [data-value="new_source"]').click()`); await b.sleep(400);
await check('类型分段：只看新来源', `${hash}.includes('ek=new_source') && [...document.querySelectorAll('[data-table] tbody tr')].every(r => r.innerText.includes('新来源'))`);
await b.eval(`document.querySelector('[data-act="ack"]').click()`); await b.sleep(400);
await check('标记已查看 → 单元格变“已查看” + toast 带撤销', `document.querySelector('[data-table] [data-ack]')?.textContent + ' / ' + document.querySelector('.toast .toast-msg')?.textContent`);
await shot('behavior-events-ack-light-1440');
await b.eval(`document.querySelector('.toast [data-t="act"]').click()`); await b.sleep(400);
await check('撤销 → 按钮回来', `!!document.querySelector('[data-table] [data-act="ack"]')`);
await b.eval(`document.querySelector('[data-seg="ek"] [data-value="new_domain"]').click()`); await b.sleep(400);
await check('新域名：说明没有逐条明细', `document.querySelector('[data-table] .empty')?.innerText.replace(/\\s+/g, ' ')`);
await b.eval(`document.querySelector('[data-seg="ek"] [data-value=""]').click()`); await b.sleep(300);
await b.eval(`document.querySelector('[data-table] tbody tr[data-row] td:nth-child(3)').click()`); await b.sleep(500);
await check('连接突增行 → 节点最近连接证据', `document.querySelector('.dialog .dialog-title')?.textContent + ' / ' + document.querySelector('.dialog .dialog-sub')?.textContent`);
await key('Escape'); await b.sleep(300);

// 7. 来源地址：画像次行
await b.eval(`document.querySelector('[data-tabs="view"] .tab[data-value="sources"]').click()`); await b.sleep(500);
await check('来源地址：运营商 · 城市 · 接入方式', `[...document.querySelectorAll('[data-table] .src-line')].slice(0, 3).map(e => e.textContent).join(' | ')`);
await check('来源标签（新来源 / 机房 / 线路机自身 是标签 chip，境外是 warn pill；consistency #7）', `[...document.querySelectorAll('[data-table] .src .chip, [data-table] .src .pill')].map(e => e.textContent).join(',')`);
await b.eval(`document.querySelector('[data-table] tbody tr[data-row] td:nth-child(3)').click()`); await b.sleep(500);
await check('来源行 → 来源证据', `document.querySelector('.dialog .dialog-title')?.textContent + ' / ' + document.querySelector('.dialog .dialog-sub')?.textContent`);
await key('Escape'); await b.sleep(300);

// 8. KPI 已阻止 → 筛选 + 空态说明
await click('[data-kpi="blocked"] .kpi-hit'); await b.sleep(500); // KPI 整卡可点（consistency #13）
await check('“只看已阻止” → result=blocked、回到域名与 IP', `${hash}`);
await check('空态说明已阻止在哪个节点', `document.querySelector('[data-table] .empty')?.innerText.replace(/\\s+/g, ' ')`);
await click('[data-act="clear"]'); await b.sleep(400);

// 9. 排序
await b.eval(`document.querySelector('[data-sort="sources"]').click()`); await b.sleep(400);
await check('点表头排序写 URL', `${hash}.includes('sort=-sources') && document.querySelector('th[aria-sort="descending"]')?.textContent`);
await b.eval(`document.querySelector('[data-sort="sources"]').click()`); await b.sleep(400);
await check('再点切换方向', `${hash}.includes('sort=sources') && document.querySelector('th[aria-sort="ascending"]')?.textContent`);
await click('[data-act="clear"]'); await b.eval(`__app.setParams({ sort: null })`); await b.sleep(300);

// 10. 键盘 j / k / Enter
await b.eval(`document.activeElement?.blur()`);
await key('j'); await key('j');
await check('j j → 焦点在第二行', `document.activeElement?.closest?.('tr[data-row]')?.dataset.row`);
await key('Enter', { text: '\r' }); await b.sleep(500);
await check('Enter → 打开证据', `document.querySelector('.dialog .dialog-title')?.textContent`);
await key('Escape'); await b.sleep(300);

// 11. 节点概览 → 抽屉（后退关闭）
await b.eval(`document.querySelector('[data-tabs="view"] .tab[data-value="nodes"]').click()`); await b.sleep(500);
await b.eval(`document.querySelector('[data-table] tbody tr[data-row] td:nth-child(2)').click()`); await b.sleep(700);
await check('节点概览行 → 抽屉（pushState，tab=access）', `!!document.querySelector('.drawer') && ${hash}`);
await check('当前行高亮', `document.querySelector('[data-table] tr.is-current')?.dataset.row`);
await b.eval('history.back()'); await b.sleep(600);
await check('后退 = 关抽屉，筛选仍在', `!document.querySelector('.drawer:not(.is-leaving)') && ${hash}`);

// 12. 时间范围：快照只有 24 小时 → 提示条
await b.eval(`document.querySelector('[data-seg="range"] [data-value="7d"]').click()`); await b.sleep(400);
await check('时间范围 7 天 → 提示条', `${hash}.includes('range=7d') && document.querySelector('[data-note] .note')?.innerText.replace(/\\s+/g, ' ').slice(0, 40)`);
await click('[data-act="clear"]'); await b.sleep(300);

// 13. 访问规则
await b.eval(`document.querySelector('[data-seg="mode"] [data-value="rules"]').click()`); await b.sleep(500);
await check('访问规则：两栏 + 发布记录', `!!document.querySelector('.rules-grid') && ${hash} + ' · ' + document.querySelectorAll('.pick').length + ' 个范围'`);
await b.eval(`document.querySelector('[data-scope="node:t1-01"]').click()`); await b.sleep(400);
await check('选范围写 URL', `${hash}.includes('scope=node:t1-01') && document.querySelector('.rules-editor .panel-title').textContent`);
await b.eval(`(() => { const s = document.querySelector('[data-select="rule-mode"]'); s.value = 'deny'; s.dispatchEvent(new Event('change', { bubbles: true })); document.querySelector('.rules-domains').value = '198.51.100.9'; return 1; })()`);
await click('[data-act="rule-save"]'); await b.sleep(300);
await check('域名列表里有 IP → 字段报错', `document.querySelector('[data-rule-error]:not([hidden])')?.textContent`);
await b.eval(`document.querySelector('.rules-domains').value = 'example.com\\n=login.example.org'`);
await click('[data-act="rule-save"]'); await b.sleep(400);
await check('保存草稿 → 待应用 + 范围标“草稿”', `document.querySelector('.rules-editor .pill')?.textContent + ' / ' + document.querySelector('[data-scope="node:t1-01"] .mark')?.textContent`);
await shot('behavior-rules-draft-light-1440');
await click('[data-act="rule-preview"]'); await b.sleep(500);
await check('预览影响对话框', `document.querySelector('.dialog .dialog-sub')?.textContent`);
await b.eval(`document.querySelector('[data-preview-ok]').click()`); await b.sleep(200);
await check('勾选确认后才能应用', `!document.querySelector('[data-dlg-act="apply"]').disabled`);
await shot('behavior-rules-preview-light-1440');
await key('Escape'); await b.sleep(300);
await click('[data-act="rule-discard"]'); await b.sleep(400);
await check('丢弃全部草稿 → 红色确认', `document.querySelector('.dialog .btn-danger')?.textContent`);
await b.eval(`document.querySelector('.dialog [data-dlg="ok"]').click()`); await b.sleep(400);
await check('丢弃后回到“已同步”', `document.querySelector('.rules-editor .pill')?.textContent`);
await b.eval(`document.querySelector('[data-seg="mode"] [data-value="behavior"]').click()`); await b.sleep(500);
await check('回到节点行为', `!!document.querySelector('[data-fform]') && ${hash}`);

// 13b. 键盘：页签 ← → 切换后焦点留在页签上；表头排序后焦点留在表头
await b.eval(`document.querySelector('[data-tabs="view"] .tab[aria-selected="true"]').focus()`);
await key('ArrowRight'); await b.sleep(400);
await check('页签 → 键切换，焦点仍在页签', `${hash} + ' · 焦点 ' + document.activeElement?.textContent`);
await key('ArrowLeft'); await b.sleep(400);
await b.eval(`document.querySelector('[data-sort="connections"]').focus()`);
await key('Enter', { text: '\r' }); await b.sleep(400);
await check('表头 Enter 排序，焦点仍在表头', `${hash} + ' · 焦点 ' + document.activeElement?.dataset.sort`);
await b.eval(`__app.setParams({ sort: null })`); await b.sleep(300);

// 14. 密度：⌘⇧D
await key('d', { meta: true, shift: true }); await b.sleep(300);
await check('⌘⇧D → 紧凑，密度分段同步', `document.querySelector('main').dataset.density + ' / ' + document.querySelector('[data-seg="density"] [aria-checked="true"]')?.textContent`);
await shot('behavior-compact-light-1440');
await key('d', { meta: true, shift: true }); await b.sleep(300);

// 15. 窄屏与深色的补充截图
await open('#/behavior?view=events', { w: 375, hgt: 812 });
await shot('behavior-events-light-375');
await open('#/behavior?view=sources', { w: 375, hgt: 812, dark: true });
await shot('behavior-sources-dark-375');
await open('#/behavior', { w: 375, hgt: 812, dark: true });
await shot('behavior-dark-375');
await open('#/behavior?asset=t1-01', { w: 1440, hgt: 900, dark: true });
await shot('behavior-asset-dark-1440');
await open('#/behavior?view=events', { w: 1440, hgt: 900, dark: true });
await shot('behavior-events-dark-1440');
await open('#/behavior?mode=rules', { w: 1440, hgt: 900, dark: true });
await shot('behavior-rules-dark-1440');
await open('#/behavior?mode=rules', { w: 375, hgt: 812 });
await shot('behavior-rules-light-375');
await open('#/behavior', { w: 1440, hgt: 900 });
await b.eval(`document.querySelector('[data-table] tbody tr td:nth-child(3)').click()`); await b.sleep(400);
await b.viewport(375, 812); await b.sleep(400);
await shot('behavior-evidence-light-375');

results.push(`console: ${b.logs.length ? b.logs.join(' | ') : '无报错'}`);
console.log(results.join('\n'));
await b.close();
process.exit(0);
