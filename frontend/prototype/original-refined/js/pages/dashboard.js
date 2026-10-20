// pages/dashboard.js — 运营总览 #/dashboard（SPEC §6.2）。版面与原版 /admin/dashboard 一致（renders/ops-overview-full.png）：
//   页头（运营总览 + 鲜度 + 宽松/紧凑 + 新增节点 + ↻）→ 4 张 KPI → 面板“团队”（表）
//   → 两栏：面板“业务速率排行”（原“实时传输排行”，前 5）│ 面板“需处理”（原“需要关注”，+ 小节“未知”）
//   → 面板“24 小时业务流量”（原“24 小时流量”，uPlot：不画采样点、↓ 靛蓝带填充 / ↑ 橙色只画线）。
// 精修：KPI 外形照首页、主值不染色（状态写副行）；团队表去掉与“N 台”重复的“节点”列、状态列只列非零 pill、加“本月业务流量”；
// 排行可切“速率 │ 今日”；图表可切 1 / 6 / 24 小时、表格视图；速率一律比特每秒（§4.1 F-01）。
// 状态：排行方式、图表时间范围存 URL（rank=today、range=6h）；密度存 localStorage（外壳负责）。

import { fmt, charts } from '../../../shared/index.js';
import {
  icon, esc, num, on,
  pageHead, watchStuck, freshness, segmented, onSegment, setSegmented, hydrate,
  kpiTile, kpiValue, patchKpi, countDots, button, iconButton, statusPill, tagChip, copyable, mark, infoButton,
  panel, empty, note, skel, dataTable, patchTable, mountChart, chartLegend, chartTable, currentPopover, toast,
} from '../components.js';
import { openEnrollWizard } from '../enroll-wizard.js';
import { HEAD_TOOLS } from '../page-tools.js';
import { isLive } from '../live-mode.js';

const RANGES = [{ value: '1h', label: '1 小时', min: 60 }, { value: '6h', label: '6 小时', min: 360 }, { value: '24h', label: '24 小时', min: 1440 }];
const RANKS = [{ value: 'rate', label: '速率' }, { value: 'today', label: '今日' }];
const STATUS_ORDER = ['ok', 'degraded', 'fail', 'maintenance', 'unknown'];
const RESORT_MS = 5000; // 按速率排序：行序最多 5 秒变一次（§5.3 规则 3）

const readState = (p) => ({
  rank: RANKS.some((r) => r.value === p.get('rank')) ? p.get('rank') : 'rate',
  range: RANGES.some((r) => r.value === p.get('range')) ? p.get('range') : '24h',
});

// 成对数值：两行，先 ↓ 后 ↑（§3.10、§4.3 D-03）
const rateDown = (d) => `<span class="v">${num(fmt.ratePair(d, 0).down)}</span>`;
const rateUp = (u) => num(fmt.ratePair(0, u).up);
// 一行里的成对值：“↓ 值” 与 “↑ 值” 各自不折行（窄屏 KPI 副行折两行时不把箭头和数值拆开）
const pairLine = (d, u, f) => `<span class="val">↓ ${num(f(d))}</span> · <span class="val">↑ ${num(f(u))}</span>`;
const bytesPairLine = (d, u) => pairLine(d, u, fmt.bytes);
const dash = '<span class="t-muted">—</span>';

export default {
  title: '运营总览',
  density: true,
  mount({ el, app, params }) {
    const store = app.store;
    let st = readState(params);
    let rankIds = [];                 // 排行当前行序
    let lastResort = 0, resortTimer = null, rankHover = false;
    let attnKey = '';                 // 需处理 + 未知 的成员（变化时整块重画）
    let chart = null, chartTableOn = false;
    const markers = [];               // 图表峰值标记（原地改，mountChart 共享这个数组）
    let lastList = [];                // 抽屉 [ ] 用：最近点开的那张表的节点顺序

    // ─── KPI（§6.2 表） ───
    function kpiModel() {
      const k = store.kpis();
      const c = k.normal;
      const allOk = c.ok === c.total;
      const br = k.business_rate;
      const partial = br.sampled_entries < br.expected_entries;
      const T = k.today_business;
      return {
        normal: {
          value: kpiValue(String(c.ok), `/ ${c.total}`),
          sub: allOk ? '全部正常' : countDots([['故障', c.fail, 'bad'], ['降级', c.degraded, 'warn'], ...(c.maintenance ? [['维护中', c.maintenance, 'info']] : []), ['未知', c.unknown, 'unknown']]),
        },
        rate: {
          value: kpiValue(fmt.rate(br.down_bps + br.up_bps)) + (partial ? ` ${tagChip('部分', `只含已采样的 ${br.sampled_entries} 个入口`)}` : '') /* consistency #7：状态说明用 tagChip，不用 .mark */,
          sub: `${partial ? `覆盖 <span class="num">${br.sampled_entries}/${br.expected_entries}</span> · ` : ''}${pairLine(br.down_bps, br.up_bps, fmt.rate)}`,
        },
        today: { value: kpiValue(fmt.bytes(T.total)), sub: bytesPairLine(T.down, T.up) },
        cost: { value: kpiValue(fmt.money(k.monthly_cost.cents)), sub: num(fmt.int(k.monthly_cost.services, '项服务')) },
      };
    }
    function kpisHtml() {
      const m = kpiModel();
      return [
        kpiTile({ key: 'normal', label: '正常', info: '监控综合判定为“正常”的节点数（连接检查 + 主机检查，原名“状态正常”）。其余按 故障 · 降级 · 维护中 · 未知 列在副行。', ...m.normal }),
        kpiTile({ key: 'rate', label: '业务速率', infoAlways: true, info: '全部 Xray 入口的业务速率之和（原名“实时传输”），按入口去重，含线路机主节点入口与监控探测流量。↓ 下行 = 流向用户客户端。采样不全时主值只含已采样部分并标“部分”。', ...m.rate }),
        kpiTile({ key: 'today', label: '今日业务流量', infoAlways: true, info: '北京时间 0 点起，全部 Xray 入口的业务流量（原名“今日流量”）。', ...m.today }),
        kpiTile({ key: 'cost', label: '每月费用', info: '折算成月付的合计，币种 USD；按节点实际登记的月费求和。', action: { icon: 'circle-dollar-sign', label: '按月费看节点', href: '#/nodes?sort=-cost' }, ...m.cost }),
      ].join('');
    }

    // ─── 团队表（§6.2；整行点击：团队 → 节点页 ?team=，线路机 → 线路页） ───
    const teamCols = [
      { key: 'team', label: '团队', width: 'auto', cell: (g) => `<span class="strong">${esc(g.team)}</span>`, sub: (g) => `<span class="num">${g.count}</span> 台 · 有活动 <span class="num">${g.active}</span>` },
      {
        key: 'status', label: '状态', cls: 'dash-status', width: 300,
        cell: (g) => `<span class="chip-wrap">${STATUS_ORDER.filter((c) => g.status[c]).map((c) => statusPill(fmt.health(c), { count: g.status[c] })).join('')}</span>`,
      },
      {
        key: 'rate', label: '业务速率', align: 'right', width: 132,
        cell: (g) => rateDown(g.rate_down_bps) + (g.rate_complete ? '' : ` ${tagChip('部分', `采样覆盖 ${g.rate_sampled}/${g.rate_expected}`)}`),
        sub: (g) => rateUp(g.rate_up_bps),
      },
      { key: 'today', label: '今日业务流量', align: 'right', width: 176, cell: (g) => `<span class="v">${num(fmt.bytes(g.today_bytes))}</span>`, sub: (g) => bytesPairLine(g.today.down, g.today.up) },
      { key: 'month', label: '本月业务流量', align: 'right', width: 176, cell: (g) => `<span class="v">${num(fmt.bytes(g.month.down + g.month.up))}</span>`, sub: (g) => bytesPairLine(g.month.down, g.month.up) },
      { key: 'cost', label: '每月费用', align: 'right', width: 104, cell: (g) => `<span class="v">${num(fmt.money(g.monthly_cents))}</span>` },
    ];
    const teamGetId = (g) => g.team;
    // 固定表格布局 + 列宽：实时数值变长不再推动别的列（状态列、速率列每次刷新左右跳，craft #5）；窄屏首列吊住
    const teamsTable = () => dataTable({ columns: teamCols, rows: store.groups(), getId: teamGetId, label: '团队', cls: 'is-fixed is-sticky-first' });

    // ─── 业务速率排行（前 5，§6.2） ───
    // 速率排行按粗体显示的 ↓ 下行排序（原来按 ↓+↑ 合计排，粗体只显示 ↓，看起来顺序是乱的，craft #20）；“今日”按合计排、主值也是合计
    const rankValue = (n) => (st.rank === 'today'
      ? (n.traffic?.today?.down ?? 0) + (n.traffic?.today?.up ?? 0)
      : n.rate?.fresh === false ? null : (n.rate?.down_bps ?? 0));
    // 始终 5 行（零布局位移）：有速率的不足 5 台时用 0 速率的节点补齐（按团队、编号）；只有没有任何新鲜数据时才是空状态
    function rankRows() {
      return store.nodes().map((n) => ({ n, v: rankValue(n) })).filter((x) => x.v != null && x.v >= 0)
        .sort((a, b) => b.v - a.v || a.n.teamOrder - b.n.teamOrder || a.n.number - b.n.number).slice(0, 5).map((x) => x.n);
    }
    const nodeCell = { key: 'node', label: '节点', cell: (n) => `<span class="strong">${esc(n.name)}</span>`, sub: (n) => copyable(n.ip, { display: fmt.ip(n.ip).text }) };
    const rankCols = () => [
      nodeCell,
      { key: 'team', label: '团队', cls: 'hide-narrow', cell: (n) => tagChip(n.team) },
      st.rank === 'today'
        ? { key: 'v', label: '今日业务流量', align: 'right', cell: (n) => `<span class="v">${num(fmt.bytes((n.traffic?.today?.down ?? 0) + (n.traffic?.today?.up ?? 0)))}</span>`, sub: (n) => bytesPairLine(n.traffic?.today?.down, n.traffic?.today?.up) }
        : { key: 'v', label: '业务速率', align: 'right', cell: (n) => rateDown(n.rate?.down_bps), sub: (n) => rateUp(n.rate?.up_bps) },
    ];
    function rankTable(rows = rankRows()) {
      rankIds = rows.map((n) => n.id);
      return dataTable({
        columns: rankCols(), rows, head: false, label: '业务速率排行', current: app.drawerNodeId?.(),
        empty: empty({ icon: 'activity', text: st.rank === 'today' ? '今日没有业务流量' : '现在没有实时数据' }),
      });
    }

    // ─── 需处理（+ 小节“未知”，§6.2；口径只用 store.attention()，T-04） ───
    const attnCols = [
      nodeCell,
      { key: 'check', label: '检查项', cls: 'hide-narrow', cell: (a) => (a.check_title ? tagChip(a.check_title) : dash) },
      { key: 'status', label: '状态', cell: (a) => statusPill(fmt.health(a.status, { stale: store.staleOf(a.node) })) },
      { key: 'reason', label: '原因', cell: (a) => (a.reason_word ? `<span class="nowrap">${esc(a.reason_word)}</span>` : dash) },
      { key: 'dur', label: '持续', align: 'right', cell: (a) => (a.duration_s != null ? `<span class="nowrap">${num(fmt.duration(a.duration_s))}</span>` : dash) },
    ];
    // 行对象：attention 项 + 节点字段（nodeCell 读 n.name / n.ip）
    const attnRows = (list) => list.map((a) => ({ ...a, name: a.node.name, ip: a.node.ip }));
    function attnModel() {
      const a = attnRows(store.attention()), u = attnRows(store.unknown());
      return { a, u, key: `${a.map((x) => x.id).join()}|${u.map((x) => x.id).join()}` };
    }
    function attnBody(m = attnModel()) {
      attnKey = m.key;
      const cur = app.drawerNodeId?.();
      // 需处理与“未知”小节放在同一张表里，列宽上下对齐；需处理为空时先给空状态，再接“未知”
      const table = (sections) => `<div data-list="attn">${dataTable({ columns: attnCols, sections, head: false, label: '需处理', current: cur })}</div>`;
      const none = (cls) => empty({ icon: 'circle-check', text: '当前没有需处理的节点', cls });
      if (!m.a.length && !m.u.length) return none('is-ok');
      if (!m.a.length) return none('is-ok is-short') + table([{ title: '未知', count: m.u.length, rows: m.u }]);
      return table([{ rows: m.a }, { title: '未知', count: m.u.length, rows: m.u }]);
    }
    const attnTools = (m) => (m.a.length || m.u.length
      ? button({ label: '在节点页查看', variant: 'link', size: 'sm', attrs: { 'data-act': 'attn-all' } })
      : '');

    // ─── 24 小时业务流量（§3.15） ───
    const rangeOf = () => RANGES.find((r) => r.value === st.range);
    function chartData() {
      const f = store.fleetTraffic();
      if (!f?.down?.length) return null;
      const [xs, down, up] = charts.columnar(f, ['down', 'up'], { toRate: true });
      const n = Math.min(xs.length, rangeOf().min);
      return [xs.slice(-n), down.slice(-n), up.slice(-n)];
    }
    function chartNote(data) {
      if (!data) return '';
      const [xs, down, up] = data;
      const pd = charts.peakOf(xs, down), pu = charts.peakOf(xs, up);
      const gaps = down.filter((v) => v == null).length; // 1 分钟桶：一个空桶 = 缺测 1 分钟
      return [
        `${fmt.stamp(xs[0]).text} 至 ${fmt.stamp(xs[xs.length - 1]).text}`,
        pd ? `↓ 峰值 ${num(fmt.rate(pd.y))}（${esc(fmt.stamp(pd.x).text)}）` : null,
        pu ? `↑ 峰值 ${num(fmt.rate(pu.y))}（${esc(fmt.stamp(pu.x).text)}）` : null,
        gaps ? `缺测 <span class="num">${gaps}</span> 分钟` : null,
        '1 分钟桶',
      ].filter(Boolean).join(' · ');
    }
    function setPeakMarker(data) {
      markers.length = 0;
      const pd = data && charts.peakOf(data[0], data[1]);
      if (pd) markers.push({ x: pd.x, y: pd.y, label: `峰值 ${fmt.rate(pd.y).text}` });
    }
    const chartTitle = () => `${rangeOf().label}业务流量`;
    const chartTools = () => chartLegend([{ label: '↓ 下行', color: 'var(--chart-1)' }, { label: '↑ 上行', color: 'var(--chart-2)' }])
      + segmented({ name: 'range', size: 'sm', items: RANGES, value: st.range, label: '时间范围' })
      + iconButton({ icon: 'table-2', label: '表格视图', size: 'sm', attrs: { 'data-act': 'chart-table', 'aria-pressed': 'false' } });

    async function mountFleetChart() {
      const box = el.querySelector('[data-chart]');
      const data = chartData();
      el.querySelector('[data-chart-note]').innerHTML = data ? chartNote(data) : '&nbsp;';
      if (!data) { box.innerHTML = empty({ icon: 'chart-column', text: '这段时间没有流量数据' }); return; }
      setPeakMarker(data);
      try {
        const ctl = await mountChart(box, {
          data, kind: 'rate', height: 190, markers,
          series: [{ label: '↓ 下行', color: 'var(--chart-1)', fill: true }, { label: '↑ 上行', color: 'var(--chart-2)' }],
        });
        if (!el.isConnected) { ctl.destroy(); return; }
        box.querySelector(':scope > .skel')?.remove();
        chart = ctl;
      } catch (e) {
        console.error(e);
        box.innerHTML = note({ tone: 'bad', title: '图表加载失败', body: esc(e.message ?? String(e)), action: button({ label: '重试', variant: 'link', size: 'sm', attrs: { 'data-act': 'chart-retry' } }) });
      }
    }
    function refreshChart() {
      const data = chartData();
      el.querySelector('[data-chart-note]').innerHTML = data ? chartNote(data) : '&nbsp;';
      el.querySelector('[data-chart-title]').textContent = chartTitle();
      if (!chart || !data) return;
      setPeakMarker(data);
      chart.setData(data);
      if (chartTableOn) renderChartTable();
    }
    function renderChartTable() {
      const box = el.querySelector('[data-chart]');
      box.nextElementSibling?.matches('.chart-table') && box.nextElementSibling.remove();
      if (chartTableOn && chart) box.insertAdjacentHTML('afterend', chartTable(chart.toTable()));
      box.hidden = chartTableOn && !!chart;
    }

    // ─── 首屏 ───
    const am = attnModel();
    el.innerHTML = `<div class="page" data-page="dashboard">
      ${pageHead({ title: '运营总览', tools: HEAD_TOOLS.dashboard({ density: app.density() }) })}
      <section class="kpi-grid" data-kpis aria-label="概览">${kpisHtml()}</section>
      ${panel({ title: '团队', flush: true, body: `<div data-list="teams" style="--dt-min:900px">${teamsTable()}</div>`, attrs: ' data-panel="teams"' })}
      <div class="cols-2 dash-pair">
        ${panel({ title: '业务速率排行', sub: infoButton('按 Xray 入口的 ↓ 下行速率从高到低，取前 5；次行是 ↑ 上行。含监控探测流量。切到“今日”按北京时间 0 点起的业务流量（↓ + ↑）排。', '业务速率排行：口径说明'), flush: true, tools: segmented({ name: 'rank', size: 'sm', items: RANKS, value: st.rank, label: '排行方式' }), body: `<div class="dash-rank" data-list="rank">${rankTable()}</div>`, attrs: ' data-panel="rank"' })}
        ${panel({ title: '需处理', count: am.a.length || null, flush: true, tools: `<span data-attn-tools>${attnTools(am)}</span>`, body: `<div class="dash-attn" data-attn>${attnBody(am)}</div>`, attrs: ' data-panel="attn"' })}
      </div>
      ${panel({ title: chartTitle(), tools: chartTools(), body: `<div class="dash-chart" data-chart>${skel('100%', 190)}</div><div class="chart-note" data-chart-note>&nbsp;</div>`, attrs: ' data-panel="chart"' })}
    </div>`;
    el.querySelector('[data-panel="chart"] .panel-title').dataset.chartTitle = '';
    hydrate(el);
    lastResort = Date.now(); // 首绘也算一次排序：之后按 5 秒节流
    const offs = [watchStuck(el)];
    mountFleetChart();

    // ─── 交互 ───
    offs.push(onSegment(el, 'density', (v) => app.setDensity(v)));
    // 只写 URL；update(params) 统一应用（刷新、前进后退、分享链接走同一条路）
    offs.push(onSegment(el, 'rank', (v) => app.setParams({ rank: v === 'rate' ? null : v })));
    offs.push(onSegment(el, 'range', (v) => app.setParams({ range: v === '24h' ? null : v })));
    offs.push(on(el, 'click', '[data-act]', (e, b) => {
      const act = b.dataset.act;
      if (act === 'enroll') openEnrollWizard(app);
      else if (act === 'refresh') manualRefresh(b);
      else if (act === 'attn-all') app.navigate('#/nodes?status=attention');
      else if (act === 'chart-table') { chartTableOn = !chartTableOn; b.setAttribute('aria-pressed', String(chartTableOn)); renderChartTable(); }
      else if (act === 'chart-retry') { el.querySelector('[data-chart]').innerHTML = skel('100%', 190); mountFleetChart(); }
    }));
    // 行：点击 / Enter / Space。团队行换页（列表语境改变），节点行在当前页开抽屉（§5.5）
    function activateRow(tr) {
      const list = tr.closest('[data-list]')?.dataset.list;
      const id = tr.dataset.row;
      if (list === 'teams') { app.navigate(id === '线路机' ? '#/lines' : `#/nodes?team=${encodeURIComponent(id)}`); return; }
      lastList = [...tr.closest('tbody').querySelectorAll('tr[data-row]')].map((x) => x.dataset.row);
      app.openNode(id);
    }
    offs.push(on(el, 'click', 'tr[data-row]', (e, tr) => {
      if (e.target.closest('button, a, [data-copy]')) return; // 行内的复制、链接自己处理
      activateRow(tr);
    }));
    offs.push(on(el, 'keydown', 'tr[data-row]', (e, tr) => {
      if (e.target !== tr || (e.key !== 'Enter' && e.key !== ' ')) return;
      e.preventDefault();
      activateRow(tr);
    }));
    // 按速率排序的实时重排：鼠标在排行面板上、或键盘焦点在里面时暂停（§5.3 规则 3）
    const rankPanel = el.querySelector('[data-panel="rank"]');
    offs.push(on(rankPanel, 'pointerenter', null, () => { rankHover = true; }));
    offs.push(on(rankPanel, 'pointerleave', null, () => { rankHover = false; if (resortTimer == null) scheduleResort(); }));

    async function manualRefresh(b) {
      if (b.classList.contains('is-busy')) return;
      b.classList.add('is-busy'); b.setAttribute('aria-busy', 'true');
      try {
        if (isLive()) await app.live.refresh(); // 实时：真的重读 /overview + /health
        else { await new Promise((r) => setTimeout(r, 450)); app.live?.tick(0); } // 原型：模拟一次往返
        repaintAll();
      } catch (e) {
        toast({ message: `刷新失败：${e.message ?? e}`, tone: 'bad' });
      } finally {
        b.classList.remove('is-busy'); b.removeAttribute('aria-busy');
      }
    }

    // ─── 实时更新：只补丁变化的格子（§5.3） ───
    function renderRank() {
      const box = el.querySelector('[data-list="rank"]');
      const first = new Map([...box.querySelectorAll('tr[data-row]')].map((tr) => [tr.dataset.row, tr.getBoundingClientRect().top]));
      box.innerHTML = rankTable();
      lastResort = Date.now();
      if (matchMedia('(prefers-reduced-motion: reduce)').matches) return;
      // FLIP：留在榜上的行从旧位置滑到新位置，新上榜的行淡入（transform / opacity 不计入布局位移）
      for (const tr of box.querySelectorAll('tr[data-row]')) {
        const y0 = first.get(tr.dataset.row);
        const dy = y0 == null ? null : y0 - tr.getBoundingClientRect().top;
        if (dy == null) tr.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 180, easing: 'cubic-bezier(.2,.8,.2,1)' });
        else if (dy) tr.animate([{ transform: `translateY(${dy}px)` }, { transform: 'none' }], { duration: 220, easing: 'cubic-bezier(.4,0,.2,1)' });
      }
    }
    function scheduleResort() {
      clearTimeout(resortTimer);
      resortTimer = setTimeout(() => {
        resortTimer = null;
        if (rankHover || rankPanel.contains(document.activeElement) || currentPopover()) { scheduleResort(); return; }
        const rows = rankRows();
        if (rows.map((n) => n.id).join() !== rankIds.join()) renderRank();
      }, Math.max(0, RESORT_MS - (Date.now() - lastResort)));
    }
    function patchRank() {
      const rows = rankRows();
      const same = rows.map((n) => n.id).join() === rankIds.join();
      // 行序没变：只改数值；变了：保持旧行序补丁数值，等节流窗口到了再重排
      const keep = same ? rows : rankIds.map((id) => store.node(id)).filter(Boolean);
      if (!patchTable(el.querySelector('[data-list="rank"]'), { columns: rankCols(), rows: keep })) renderRank();
      else if (!same && resortTimer == null) scheduleResort();
    }
    function patchAttn() {
      const m = attnModel();
      const box = el.querySelector('[data-attn]');
      if (m.key !== attnKey) {
        const before = new Set([...box.querySelectorAll('tr[data-row]')].map((tr) => tr.dataset.row));
        box.innerHTML = attnBody(m);
        el.querySelector('[data-panel="attn"] .panel-count')?.remove();
        if (m.a.length) el.querySelector('[data-panel="attn"] .panel-title').insertAdjacentHTML('afterend', `<span class="panel-count">${m.a.length}</span>`);
        el.querySelector('[data-attn-tools]').innerHTML = attnTools(m);
        // 新进入的行淡入（§5.3 规则 6）
        for (const tr of box.querySelectorAll('tr[data-row]')) if (!before.has(tr.dataset.row)) tr.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 180 });
        return;
      }
      patchTable(box.querySelector('[data-list="attn"]'), { columns: attnCols, rows: [...m.a, ...m.u] });
    }
    function patchKpis() {
      const m = kpiModel();
      const k = el.querySelector('[data-kpis]');
      for (const key of Object.keys(m)) patchKpi(k, key, m[key]);
    }
    function patchTeams() {
      const box = el.querySelector('[data-list="teams"]');
      if (!patchTable(box, { columns: teamCols, rows: store.groups(), getId: teamGetId })) box.innerHTML = teamsTable();
    }
    function repaintAll() {
      patchKpis(); patchTeams(); patchRank(); patchAttn(); refreshChart();
    }
    offs.push(store.on('change', (diff) => {
      if (!el.isConnected) return;
      patchKpis(); patchTeams(); patchRank(); patchAttn();
      if (diff?.fleet?.['traffic.fleet_24h']) refreshChart();
    }));
    // “持续 12 分钟”这类时长在两次推送之间也要走（外壳每 2 秒推送一次，这里兜底 30 秒）
    const tick = setInterval(() => { if (el.isConnected) patchAttn(); }, 30000);

    // ─── 键盘：j / k 在各表的行之间移动，Enter 打开（§5.4） ───
    const rowsInPage = () => [...el.querySelectorAll('tr[data-row]')];
    function moveFocus(d) {
      const rows = rowsInPage();
      if (!rows.length) return;
      const i = rows.indexOf(document.activeElement);
      const next = rows[i < 0 ? (d > 0 ? 0 : rows.length - 1) : Math.max(0, Math.min(rows.length - 1, i + d))];
      next.focus();
      next.scrollIntoView({ block: 'nearest', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' });
    }
    const unbind = app.keys.bindAll([
      { combo: 'j', run: () => moveFocus(1), desc: '下一行', group: '运营总览', when: () => !app.drawerOpen() },
      { combo: 'k', run: () => moveFocus(-1), desc: '上一行', group: '运营总览', when: () => !app.drawerOpen() },
      { combo: 'o', run: () => { const tr = document.activeElement?.closest?.('tr[data-row]'); if (tr && el.contains(tr)) activateRow(tr); }, desc: '打开焦点所在行', group: '运营总览', when: () => !app.drawerOpen() },
      { combo: 'ArrowDown', run: () => moveFocus(1), hidden: true, when: () => !app.drawerOpen() && document.activeElement?.matches?.('tr[data-row]') && el.contains(document.activeElement) },
      { combo: 'ArrowUp', run: () => moveFocus(-1), hidden: true, when: () => !app.drawerOpen() && document.activeElement?.matches?.('tr[data-row]') && el.contains(document.activeElement) },
    ]);

    return {
      nodeOrder: () => {
        const cur = app.drawerNodeId?.();
        if (cur && lastList.includes(cur)) return lastList;
        return store.nodes().map((n) => n.id);
      },
      /** 抽屉打开 / 切换：高亮对应的行（底 --selected + 首列 2 px 强调色）。 */
      onDrawer(id) { for (const tr of el.querySelectorAll('tr[data-row]')) tr.classList.toggle('is-current', tr.dataset.row === id && !tr.closest('[data-list="teams"]')); },
      onDensity(v) { setSegmented(el.querySelector('[data-seg="density"]'), v); },
      update(p) {
        const next = readState(p);
        if (next.rank !== st.rank) {
          st.rank = next.rank;
          setSegmented(el.querySelector('[data-seg="rank"]'), st.rank);
          renderRank();
        }
        if (next.range !== st.range) {
          st.range = next.range;
          setSegmented(el.querySelector('[data-seg="range"]'), st.range);
          refreshChart();
        }
      },
      unmount() { offs.forEach((f) => f()); clearTimeout(resortTimer); clearInterval(tick); unbind(); chart?.destroy(); },
    };
  },
};
