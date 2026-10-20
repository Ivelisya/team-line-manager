// interact-lines.mjs — 线路页交互冒烟测试（SPEC §6.4、§5.4、§3.17）：真实按键与点击经 CDP 派发，只访问本地原型。
// 用法：SHOOT_PORT=9455 ~/.local/bin/node tools/interact-lines.mjs
import { launch } from './shoot.mjs';

const BASE = 'http://127.0.0.1:5199/original-refined/index.html';
const b = await launch({ port: Number(process.env.SHOOT_PORT) || 9395 });
const key = async (k, { meta = false, shift = false, code, text } = {}) => {
  const mods = (meta ? 4 : 0) | (shift ? 8 : 0);
  const params = { key: k, code: code ?? (k.length === 1 ? `Key${k.toUpperCase()}` : k), modifiers: mods, windowsVirtualKeyCode: k.length === 1 ? k.toUpperCase().charCodeAt(0) : { Escape: 27, Enter: 13, ArrowDown: 40, ArrowUp: 38, ArrowRight: 39, ArrowLeft: 37, Tab: 9, ' ': 32 }[k] ?? 0 };
  await b.send('Input.dispatchKeyEvent', { type: 'keyDown', ...params, text: text ?? (k.length === 1 && !meta ? k : undefined) });
  await b.send('Input.dispatchKeyEvent', { type: 'keyUp', ...params });
  await b.sleep(250);
};
const click = async (sel) => { await b.eval(`(() => { const x = document.querySelector(${JSON.stringify(sel)}); if (!x) throw new Error('没找到 ' + ${JSON.stringify(sel)}); x.click(); return 1; })()`); await b.sleep(350); };
const results = [];
const check = async (name, expr) => { let v; try { v = await b.eval(expr); } catch (e) { v = false; } results.push(`${v ? 'PASS' : 'FAIL'}  ${name}${v && v !== true ? ` → ${v}` : ''}`); };
const go = async (hash) => { await b.go(`${BASE}${hash}`); await b.until('document.documentElement.dataset.ready === "1"', 20000); await b.sleep(1200); };

await b.viewport(1440, 900);
await b.media({ dark: false, reduce: true });
await go('#/lines');

await check('三块线路机面板 + 接入任务 + 管理操作记录', `document.querySelectorAll('.relay').length === 3 && !!document.querySelector('[data-jobs]') && !!document.querySelector('[data-audit]') && 'ok'`);
await check('入口表 41 行、没有“连接入口”旧词', `document.querySelectorAll('[data-table] tr[data-row]').length === 41 && !document.body.innerText.includes('连接入口') && 41`);
// 展开行
await click('tr[data-row="线路机1:20001"] td');
await check('点行 → 展开（aria-expanded + 明细）', `document.querySelector('tr[data-row="线路机1:20001"]').getAttribute('aria-expanded') === 'true' && document.querySelector('tr[data-expand="线路机1:20001"] .dl-grid')?.innerText.includes('入口标签') && 'ok'`);
await b.sleep(900);
await check('展开行的来源 IP（懒加载 extra.json）', `document.querySelector('tr[data-expand="线路机1:20001"]').innerText.match(/203\\.0\\.113\\.\\d+/)?.[0]`);
await key('Enter'); await check('Enter → 收起', `document.querySelector('tr[data-row="线路机1:20001"]').getAttribute('aria-expanded') === 'false'`);
await key('j'); await check('j → 焦点移到下一行', `document.activeElement.dataset.row`);
await key('k'); await check('k → 焦点回到上一行', `document.activeElement.dataset.row`);
await key('ArrowRight'); await check('→ 展开焦点行', `document.activeElement.getAttribute('aria-expanded')`);
await key('ArrowLeft'); await check('← 收起焦点行', `document.activeElement.getAttribute('aria-expanded')`);
// 排序
await click('[data-relay="r1"] [data-sort="rate"]');
await check('业务速率表头 → URL sort=-rate，箭头向下', `decodeURIComponent(location.hash).includes('sort=-rate') && document.querySelector('[data-relay="r1"] [data-sort="rate"]').getAttribute('aria-sort')`);
await click('[data-relay="r1"] [data-sort="port"]');
await check('端口表头 → 默认排序（URL 去掉 sort）', `!location.hash.includes('sort=') && 'ok'`);
// 每台线路机的筛选
await click('[data-relay="r1"] [data-act="filter"][data-k="act"]');
await check('活动筛选菜单（带计数）', `[...document.querySelectorAll('.menu .menu-item')].map(x => x.innerText.replace(/\\s+/g, ' ')).join(' | ')`);
await b.eval(`[...document.querySelectorAll('.menu .menu-item')].find(x => x.innerText.includes('空闲')).click()`); await b.sleep(400);
await check('选“空闲” → URL r1.act=idle，只剩空闲行，按钮变软靛蓝', `decodeURIComponent(location.hash).includes('r1.act=idle') && document.querySelectorAll('[data-relay="r1"] tr[data-row]').length + ' 行 · pressed=' + document.querySelector('[data-relay="r1"] [data-k="act"]').getAttribute('aria-pressed')`);
await b.eval(`__app.setParams({ 'r1.act': null })`); await b.sleep(300);
// 跨线路机搜索
await b.eval(`document.querySelector('[data-page-search]').focus()`);
await b.send('Input.insertText', { text: '团队1-24' }); await b.sleep(700);
await check('搜索 团队1-24 → URL q，线路机2 只剩 1 行，其余折叠成一行', `decodeURIComponent(location.hash).includes('q=团队1-24') && document.querySelectorAll('[data-relay="r2"] tr[data-row]').length + ' 行 · ' + [...document.querySelectorAll('.relay.is-nomatch')].map(x => x.innerText.replace(/\\s+/g, ' ')).join(' / ')`);
await key('Escape'); await b.sleep(500);
await check('搜索框里 Esc → 清空搜索', `!location.hash.includes('q=') && document.querySelectorAll('[data-table] tr[data-row]').length`);
// 线路机分段
await click('[data-seg="relay"] [data-value="r2"]');
await check('分段选 线路机2 → 只显示一块面板，URL relay=r2', `document.querySelectorAll('.relay').length === 1 && location.hash.includes('relay=r2') && 'ok'`);
await click('[data-seg="relay"] [data-value=""]');
// 命令面板选入口 → 定位并展开
await b.eval(`location.hash = '#/lines?entry=' + encodeURIComponent('线路机2:20015')`); await b.sleep(900);
await check('entry=线路机2:20015 → 那一行展开、URL 去掉 entry', `document.querySelector('tr[data-row="线路机2:20015"]')?.getAttribute('aria-expanded') === 'true' && !location.hash.includes('entry=') && 'ok'`);
// 绑定节点 → 抽屉
await click('tr[data-row="线路机2:20015"] [data-act="node"]'); await b.sleep(500);
await check('点绑定节点 → 抽屉（当前行高亮）', `location.hash.includes('node=t1-24') && !!document.querySelector('.drawer') && document.querySelector('tr[data-row="线路机2:20015"]').classList.contains('is-current') && 'ok'`);
await key('Escape'); await b.sleep(500);
// 停用（L1）
await click('tr[data-row="线路机1:20001"] [data-act="disable"]');
await check('停用 → L1 确认框（红色按钮、影响范围）', `[document.querySelector('.dialog .dialog-title')?.textContent, document.querySelector('.dialog [data-dlg="ok"]')?.className.includes('btn-danger')].join(' · ')`);
await click('.dialog [data-dlg="ok"]'); await b.sleep(400);
await check('确认 → 行变成 停用 + ▷ 启用，计数 停用 1', `document.querySelector('tr[data-row="线路机1:20001"]').innerText.includes('停用') && !!document.querySelector('tr[data-row="线路机1:20001"] [data-act="enable"]') && document.querySelector('[data-relay="r1"] [data-s="counts"]').innerText.match(/停用 \\d+/)?.[0]`);
await click('tr[data-row="线路机1:20001"] [data-act="enable"]');
await check('启用 → L0 直接执行 + toast 撤销', `document.querySelector('.toast:last-child')?.innerText.replace(/\\s+/g, ' ')`);
// 重置用量（L2）
await click('tr[data-row="线路机1:20002"] [data-act="reset"]');
await check('重置用量 → L2（输入名称前按钮禁用）', `document.querySelector('.dialog [data-dlg="ok"]')?.disabled && document.querySelector('.dialog .field-label')?.innerText`);
await key('Escape'); await b.sleep(400);
// 编辑备注
await click('tr[data-row="线路机1:20003"] td'); await click('tr[data-expand="线路机1:20003"] [data-act="remark"]');
await check('编辑备注 → 行内输入框获得焦点', `document.activeElement?.dataset.remark`);
await b.eval(`document.activeElement.value = '团队1-14 · 测试备注'`);
await key('Enter'); await b.sleep(300);
await check('Enter 保存备注', `document.querySelector('tr[data-expand="线路机1:20003"]').innerText.includes('测试备注') && 'ok'`);
// 日志
await click('[data-relay="r1"] [data-act="logs"]'); await b.sleep(300);
await check('Xray 日志对话框（日志块）', `document.querySelector('.dialog .dialog-title')?.textContent + ' · ' + document.querySelectorAll('.dialog .log-line').length + ' 行'`);
await key('Escape'); await b.sleep(400);
// 重启 Xray
await click('[data-relay="r3"] [data-act="restart"]');
await check('重启 Xray → L1（写明影响入口数）', `document.querySelector('.dialog .dialog-text')?.textContent`);
await key('Escape'); await b.sleep(400);
// 更新详情折叠
await click('[data-relay="r1"] .relay-foot .disclosure');
await check('软件与数据更新详情展开', `!document.querySelector('#relay-more-r1').hidden && document.querySelector('#relay-more-r1').innerText.includes('模板版本') && 'ok'`);
// 新增家宽节点向导
await click('[data-relay="r2"] [data-act="add-node"]'); await b.sleep(300);
await check('向导：预选线路机2，步骤条 1 填写', `document.querySelector('.dialog select[name="relay"]').value + ' · ' + document.querySelector('.wiz-step[data-state="current"]')?.innerText`);
await click('.dialog [data-wz="next"]');
await check('空表单下一步 → 字段错误', `[...document.querySelectorAll('.dialog .field-error')].map(x => x.textContent).slice(0, 3).join(' / ')`);
await b.eval(`(() => { const set = (n, v) => { const x = document.querySelector('.dialog [name="' + n + '"]'); x.value = v; x.dispatchEvent(new Event('input', { bubbles: true })); }; set('egress', '198.51.100.31'); set('host', '198.51.100.99'); set('password', 'x'); set('due', '2026-10-23'); return 1; })()`);
await click('.dialog [data-wz="next"]');
await check('重复出口 IP → “已被 团队1-21 使用”', `document.querySelector('.dialog [data-field="egress"] .field-error')?.textContent`);
await b.eval(`(() => { const x = document.querySelector('.dialog [name="egress"]'); x.value = '198.51.100.209'; x.dispatchEvent(new Event('input', { bubbles: true })); return 1; })()`);
await click('.dialog [data-wz="next"]');
await check('确认页：名称预览 + 入口 + 套餐规范名 + 风险提示条', `document.querySelector('.dialog .dialog-body').innerText.replace(/\\s+/g, ' ').slice(0, 220)`);
await click('.dialog [data-wz="start"]'); await b.sleep(5500);
await check('执行：步骤全部成功 + 接入任务面板出现演示任务', `document.querySelectorAll('.dialog .tl-icon[data-step="succeeded"]').length + ' 步成功 · ' + document.querySelector('[data-jobs] .job-title')?.innerText`);
await key('Escape'); await b.sleep(400);
// 新增线路机向导
await click('[data-act="add-relay"]'); await b.sleep(300);
await check('新增线路机向导（名称预填 线路机4，套餐只有规范名）', `document.querySelector('.dialog [name="name"]').value + ' · ' + [...document.querySelectorAll('.dialog select[name="plan"] option')].map(o => o.textContent).join(' / ')`);
await b.eval(`[...document.querySelectorAll('.dialog [name="auth"]')].find(x => x.value === 'key').click()`); await b.sleep(300);
await check('认证方式选“已装公钥” → 出现公钥块', `!!document.querySelector('.dialog .codeblock') && 'ok'`);
await key('Escape'); await b.sleep(400);
// 接入任务展开
await click('[data-jobs] .job:nth-child(2) .job-row');
await check('接入任务展开 → 结果提示条 + 步骤（建入口 / 转发与路由）', `document.querySelector('[data-jobs] .job:nth-child(2) .job-body').innerText.replace(/\\s+/g, ' ').slice(0, 120)`);
// 审计
await check('审计默认 10 条、隐藏自动记录', `document.querySelectorAll('[data-audit] tbody tr[data-row]').length + ' · ' + document.querySelector('[data-audit] .table-foot').innerText.replace(/\\s+/g, ' ')`);
await b.eval(`document.querySelector('[data-act="audit-auto"]').click()`); await b.sleep(300);
await click('[data-audit] [data-act="audit-all"]');
await check('显示自动记录 + 查看全部', `document.querySelectorAll('[data-audit] tbody tr[data-row]').length`);
await click('[data-audit] [data-act="audit-row"]');
await check('审计详情展开 → 等宽 JSON（套餐原文已换成规范名）', `(() => { const p = document.querySelector('[data-audit] .codeblock:not([hidden])'); return p && !/基础版 VPS|LAX\\.Pulse|Acapulco/.test(p.textContent) && p.textContent.slice(0, 40).replace(/\\s+/g, ' '); })()`);
await check('全部审计 JSON 里没有套餐原文', `![...document.querySelectorAll('[data-audit] .codeblock')].some(p => /基础版（美国住宅|基础版 VPS|标准版 VPS|LAX\\.Pulse|Acapulco/.test(p.textContent)) && 'ok'`);
// 密度
await click('[data-seg="density"] [data-value="compact"]');
await check('紧凑密度 → main[data-density=compact]，行高变矮', `document.querySelector('#main').dataset.density + ' · ' + Math.round(document.querySelector('tr[data-row]').getBoundingClientRect().height)`);
await click('[data-seg="density"] [data-value="comfortable"]');
// 演示：面板不可达
await go('#/lines?demo=1');
await click('[data-demo="panel"]');
await check('演示：线路机2 面板不可达 → 提示条 + 操作禁用 + 健康降级', `[document.querySelector('[data-relay="r2"] .entry-note')?.innerText.replace(/\\s+/g, ' ').slice(0, 40), document.querySelector('[data-relay="r2"] [data-act="disable"]')?.disabled, document.querySelector('[data-relay="r2"] [data-s="pill"]').innerText].join(' · ')`);
await click('[data-demo="clear"]');
results.push(`console: ${b.logs.length ? b.logs.join(' | ') : '无报错'}`);
console.log(results.join('\n'));
await b.close();
process.exit(0);
