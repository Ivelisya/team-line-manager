// interact-dashboard.mjs — 运营总览 #/dashboard 的键盘与交互冒烟测试（SPEC §6.2、§5.3–5.8）：真实按键经 CDP 派发。
// 用法：~/.local/bin/node tools/interact-dashboard.mjs
import { launch } from './shoot.mjs';

const BASE = 'http://127.0.0.1:5199/original-refined/index.html';
const b = await launch({ port: Number(process.env.CDP_PORT ?? 9391) });
const key = async (k, { meta = false, shift = false, code, text } = {}) => {
  const mods = (meta ? 4 : 0) | (shift ? 8 : 0);
  const params = { key: k, code: code ?? (k.length === 1 ? `Key${k.toUpperCase()}` : k), modifiers: mods, windowsVirtualKeyCode: k.length === 1 ? k.toUpperCase().charCodeAt(0) : { Escape: 27, Enter: 13, ArrowDown: 40, ArrowUp: 38 }[k] ?? 0 };
  await b.send('Input.dispatchKeyEvent', { type: 'keyDown', ...params, text: text ?? (k.length === 1 && !meta ? k : undefined) });
  await b.send('Input.dispatchKeyEvent', { type: 'keyUp', ...params });
  await b.sleep(250);
};
const results = [];
const check = async (name, expr) => { const v = await b.eval(expr); results.push(`${v ? 'PASS' : 'FAIL'}  ${name}${v && v !== true ? ` → ${v}` : ''}`); };
const open = async (hash) => { await b.go(`${BASE}${hash}`); await b.until('document.documentElement.dataset.ready === "1"', 20000); await b.sleep(1200); };

await b.viewport(1440, 900);
await b.media({ dark: false, reduce: true });
await b.go(`${BASE}#/dashboard`);
await b.until('document.readyState === "complete"');
await b.eval(`localStorage.removeItem('refined:density'); 1`);
await open('#/dashboard');

await check('路由挂载运营总览、侧栏当前项', `document.querySelector('.page-title').textContent + ' / ' + document.querySelector('[aria-current="page"]').textContent`);
await check('4 张 KPI 标签', `[...document.querySelectorAll('.kpi-label > span:first-child')].map((e) => e.textContent).join(' · ')`);
await check('团队表 3 行、6 列 + ›', `document.querySelectorAll('[data-list="teams"] tbody tr').length === 3 && document.querySelectorAll('[data-list="teams"] thead th').length === 7`);
await check('排行前 5', `document.querySelectorAll('[data-list="rank"] tr[data-row]').length === 5 && [...document.querySelectorAll('[data-list="rank"] tr[data-row]')].map((t) => t.dataset.row).join(',')`);
await check('需处理空状态', `document.querySelector('[data-attn] .empty')?.textContent.trim()`);
await check('图表已挂载（uPlot）', `!!document.querySelector('[data-chart] .u-over') && document.querySelector('[data-chart-note]').textContent`);
// 密度
await b.eval(`document.querySelector('[data-seg="density"] [data-value="compact"]').click()`); await b.sleep(300);
await check('宽松/紧凑 → main[data-density] 与 localStorage', `document.getElementById('main').dataset.density + ' ' + localStorage.getItem('refined:density')`);
await key('d', { meta: true, shift: true });
await check('⌘⇧D → 切回宽松，分段同步', `document.getElementById('main').dataset.density + ' ' + document.querySelector('[data-seg="density"] [aria-checked="true"]').dataset.value`);
// 排行方式、时间范围写 URL
await b.eval(`document.querySelector('[data-seg="rank"] [data-value="today"]').click()`); await b.sleep(400);
await check('排行“今日”写进 URL 并重排', `location.hash + ' ' + [...document.querySelectorAll('[data-list="rank"] tr[data-row]')].map((t) => t.dataset.row).join(',')`);
await b.eval(`document.querySelector('[data-seg="range"] [data-value="1h"]').click()`); await b.sleep(500);
await check('图表“1 小时”写进 URL、标题跟着变', `location.hash + ' ' + document.querySelector('[data-chart-title]').textContent`);
await b.eval(`document.querySelector('[data-act="chart-table"]').click()`); await b.sleep(300);
await check('表格视图：图表隐藏、出现 60 行表', `document.querySelector('[data-chart]').hidden && document.querySelectorAll('.chart-table tbody tr').length`);
await b.eval(`document.querySelector('[data-act="chart-table"]').click()`); await b.sleep(300);
await check('再点回到图表', `!document.querySelector('[data-chart]').hidden && !document.querySelector('.chart-table')`);
await open('#/dashboard?rank=today&range=6h');
await check('刷新后从 URL 还原排行与范围', `document.querySelector('[data-seg="rank"] [aria-checked="true"]').dataset.value + ' ' + document.querySelector('[data-seg="range"] [aria-checked="true"]').dataset.value`);
await open('#/dashboard');
// 键盘：j/k、Enter、[ ]、Esc、后退
await key('j'); await key('j'); await key('j'); await key('j');
await check('j ×4 → 焦点在排行第一行', `document.activeElement.closest('[data-list]')?.dataset.list + ':' + document.activeElement.dataset.row`);
await key('Enter', { text: '\r' }); await b.sleep(500);
await check('Enter → 打开抽屉（pushState）', `!!document.querySelector('.drawer') && location.hash`);
await check('当前行高亮', `document.querySelector('[data-list="rank"] tr.is-current')?.dataset.row`);
await key(']'); await b.sleep(400);
await check('] → 排行里的下一个节点', `location.hash`);
await key('Escape'); await b.sleep(400);
await check('Esc → 关抽屉回到原 URL', `!document.querySelector('.drawer:not(.is-leaving)') && location.hash`);
await b.eval(`document.querySelector('[data-list="rank"] tr[data-row]').click()`); await b.sleep(500);
await b.eval(`history.back()`); await b.sleep(600);
await check('浏览器后退 = 关抽屉', `!document.querySelector('.drawer:not(.is-leaving)') && location.hash`);
// 复制 IP 不打开抽屉
await b.eval(`document.querySelector('[data-list="rank"] [data-copy]').click()`); await b.sleep(300);
await check('点 IP 复制，不打开抽屉', `!document.querySelector('.drawer:not(.is-leaving)')`);
// 刷新按钮
await b.eval(`document.querySelector('[data-act="refresh"]').click()`); await b.sleep(120);
await check('↻ 刷新中：图标旋转、aria-busy', `document.querySelector('[data-act="refresh"]').classList.contains('is-busy') && document.querySelector('[data-act="refresh"]').getAttribute('aria-busy')`);
await b.sleep(700);
await check('↻ 刷新完成，不弹成功 toast', `!document.querySelector('[data-act="refresh"]').classList.contains('is-busy') && !document.querySelector('.toast')`);
// 需处理：回放一个故障
await b.eval(`__app.store.applyReplay(0); 1`); await b.sleep(500);
await check('回放故障 → 需处理出现一行、KPI 副行带圆点', `[...document.querySelectorAll('[data-attn] tr[data-row]')].map((t) => t.dataset.row).join(',') + ' | ' + document.querySelector('[data-kpi="normal"] [data-k="sub"]').textContent`);
await check('团队表状态列出现故障 pill', `[...document.querySelectorAll('[data-list="teams"] tbody tr:first-child .pill')].map((p) => p.textContent.trim()).join(' ')`);
await b.eval(`__app.store.applyReplay(4); 1`); await b.sleep(500);
await check('回放 16 台未知 → “未知”小节', `document.querySelector('[data-attn] .section-tr')?.textContent.trim() + ' / 行数 ' + document.querySelectorAll('[data-attn] tr[data-row]').length`);
await b.eval(`__app.store.clearReplay(); 1`); await b.sleep(500);
await check('恢复 → 空状态', `document.querySelector('[data-attn] .empty')?.textContent.trim()`);
// 新增节点 → 向导
await b.eval(`document.querySelector('[data-act="enroll"]').click()`); await b.sleep(400);
await check('新增节点 → 向导第 1 步', `document.querySelector('.dialog .dialog-title')?.textContent + ' / ' + document.querySelector('.dialog [aria-current="step"]')?.textContent`);
await key('Escape'); await b.sleep(300);
await check('Esc 关闭向导', `!document.querySelector('.dialog:not(.is-leaving)')`);
// 团队行 → 节点页
await b.eval(`document.querySelector('[data-list="teams"] tr[data-row="团队2"]').click()`); await b.sleep(800);
await check('团队行 → 节点页 ?team=', `decodeURIComponent(location.hash)`);
await open('#/dashboard');
await b.eval(`document.querySelector('[data-list="teams"] tr[data-row="线路机"]').click()`); await b.sleep(800);
await check('线路机行 → 线路页', `location.hash`);
await open('#/dashboard');
await key('g'); await key('o'); await b.sleep(300);
await check('g o → 运营总览', `location.hash`);
console.log(results.join('\n'));
console.log(b.logs.length ? `控制台：\n${b.logs.join('\n')}` : '控制台：无报错');
await b.close();
