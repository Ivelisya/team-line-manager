// pages/nodes.js — 节点 #/nodes（SPEC §6.3，原版 /admin/nodes）。
// 区块顺序与原版一致：页头（节点 + 鲜度 + 宽松/紧凑 + 新增节点 + 导出 + ↻）→ 4 张 KPI → 一个面板（工具条 + 表格）。
// 精修点：状态分段与团队分段并进面板头（原版两种外观两个位置，dna-ops B2）；状态列拆成“状态 / 延迟”，每行最多两行文字；
// 全部数值列可排序（写进 URL）；搜索支持 team: relay: status: plan: due: 等 token 并自动补全；套餐只显示规范名，可筛选、可分组并带小计；
// 实时更新只补丁变化的格子（patchTable），行序按速率排序时 5 秒节流 + FLIP，离开筛选的行先淡出 5 秒再收起。
// 状态全部在 URL：status team relay plan q sort group（SPEC §5.6）；密度在 localStorage（外壳管）。

import { fmt, createTable, nodeTableConfig } from '../../../shared/index.js';
import {
  esc, num, txt, on,
  pageHead, watchStuck, freshness, segmented, onSegment, setSegmented, setSegmentCounts, hydrate,
  kpiTile, kpiValue, patchKpi, countDots, button, iconButton, empty,
  statusPill, neutralChip, removableChip, copyable, mark,
  openMenu, openPopover, closePopover, currentPopover, toast,
  dataTable, patchTable, tableMeter, quotaTone,
} from '../components.js';
import { searchBox, onSearch, setSearch } from '../ui/forms.js';
import { openEnrollWizard } from '../enroll-wizard.js';
import { allPlans, planOf, planById, planLine } from '../plans.js';
import * as M from '../node-model.js';
import { HEAD_TOOLS } from '../page-tools.js';
import { isLive } from '../live-mode.js';

const URL_KEYS = ['status', 'team', 'relay', 'region', 'provider', 'cap', 'plan', 'lifecycle', 'q', 'sort'];
const GROUPS = { '': '不分组', plan: '按套餐', relay: '按线路机', team: '按团队' };
const lc = (s) => String(s ?? '').toLowerCase();
const now = () => fmt.now();
// 11 列的列宽之和（含 32 的 › 列）= 1440 视口、侧栏展开时的面板内宽 1150；更宽时按比例分摊，更窄时容器内横向滚动
const TABLE_MIN = 1150;

export default {
  title: '节点',
  density: true,
  mount({ el, app, params }) {
    const store = app.store;

    // ─── 表格状态（shared/table.js）：套餐 facet 用规范套餐 id，词用规范名；线路机 facet 把“直连”改成“未绑定” ───
    const base = nodeTableConfig(store);
    const planIdx = new Map(allPlans(store).map((p, i) => [p.id, i]));
    const planName = (id) => planById(id)?.name ?? allPlans(store).find((p) => p.id === id)?.name ?? id;
    const teamIdx = new Map(store.teams().map((t, i) => [t.name, i]));
    const table = createTable({
      ...base,
      storageKey: null,                 // 分组写 URL，不写 localStorage 偏好
      resortThrottleMs: 5000,           // 按速率排序时行序最多 5 秒变一次（§5.3）
      columns: { ...base.columns, plan: { label: '套餐', type: 'string', get: (n) => planOf(n).name } },
      facets: {
        ...base.facets,
        team: { ...base.facets.team, order: (v) => teamIdx.get(v) ?? 99 },
        relay: { label: '线路机', get: (n) => n.relay ?? '未绑定', order: (v) => (v === '未绑定' ? 99 : Number(String(v).match(/\d+/)?.[0] ?? 50)) },
        plan: { label: '套餐', get: (n) => n.plan_id, word: planName, order: (v) => planIdx.get(v) ?? 99 },
      },
      tokens: {
        ...base.tokens,
        plan: { facet: 'plan', test: (n, v) => n.plan_id === v || lc(planOf(n).name).includes(lc(v)) },
        relay: { facet: 'relay', test: (n, v) => lc(n.relay).includes(lc(v)) || lc(n.relay) === lc(`线路机${v}`) || (v === '未绑定' && !n.relay) },
      },
    });
    let groupBy = '';
    let renderedKey = null;
    const readParams = (p) => {
      table.fromParams(p);
      groupBy = GROUPS[p.get('group') ?? ''] ? p.get('group') ?? '' : '';
      table.setGroupBy(groupBy || null);
    };
    const keyOf = (p) => [...URL_KEYS, 'group'].map((k) => `${k}=${p.get(k) ?? ''}`).join('&');
    readParams(params);
    renderedKey = keyOf(params);

    /** 把表格状态写回 URL（replaceState）→ 外壳调 update(params) → 重画。 */
    function commit() {
      const p = table.toParams(new URLSearchParams());
      const obj = Object.fromEntries(URL_KEYS.map((k) => [k, p.get(k)]));
      obj.group = groupBy || null;
      app.setParams(obj, { replace: true });
    }

    // ─── KPI（全机队口径；“需处理”“7 天内到期”可点，切换对应分段） ───
    function kpiModel() {
      const k = store.kpis();
      const c = k.normal;
      const att = store.attention();
      const attFail = att.filter((a) => a.tone === 'bad').length;
      const nonOk = c.fail + c.degraded + c.unknown + c.maintenance;
      const near = k.due_7d.nearest ? store.node(k.due_7d.nearest.id) : null;
      return {
        normal: {
          value: kpiValue(String(c.ok), `/ ${c.total}`),
          sub: nonOk ? countDots([['故障', c.fail, 'bad'], ['降级', c.degraded, 'warn'], ['未知', c.unknown, 'unknown'], ...(c.maintenance ? [['维护中', c.maintenance, 'info']] : [])]) : '全部正常',
        },
        attention: {
          value: kpiValue(String(k.attention)),
          sub: k.attention ? countDots([['故障', attFail, 'bad'], ['降级', k.attention - attFail, 'warn']]) : '当前没有需处理的节点',
        },
        due7: {
          value: kpiValue(String(k.due_7d.count)),
          sub: near ? `最近：${esc(near.name)} · ${M.dueHtml(near)}` : '没有登记到期日',
        },
        cost: { value: kpiValue(fmt.money(k.monthly_cost.cents)), sub: num(fmt.int(k.monthly_cost.services, '项服务')) },
      };
    }
    function kpisHtml() {
      const m = kpiModel();
      const seg = table.segment();
      return [
        kpiTile({ key: 'normal', label: '正常', info: '监控综合判定（连接 + 主机检查）为正常的节点数 / 全部节点。非正常按 故障、降级、未知、维护中 计数；数据超过 50 秒没有评估的节点记为未知。', ...m.normal }),
        kpiTile({ key: 'attention', label: '需处理', clickable: true, pressed: seg === 'attention', info: '未恢复的故障，加上持续降级 5 分钟以上的节点（服务端 monitor.attention）；与运营总览“需处理”同一口径。点卡片只看这些节点，再点取消。', ...m.attention }),
        kpiTile({ key: 'due7', label: '7 天内到期', clickable: true, pressed: seg === 'due7', info: '剩余 7 天以内（含今天到期和已过期）。天数按资产账期时区的日历日计算，非北京时区标 LA。副行是最近一台的到期。点卡片只看这些节点。', ...m.due7 }),
        // 在本页生效的 KPI 一律整卡可点（consistency #13）：每月费用 = 按月费从高到低排，再点取消
        kpiTile({ key: 'cost', label: '每月费用', clickable: true, pressed: app.params().get('sort') === '-cost', info: '折算成月付的合计，币种 USD；按节点实际登记的月费求和。季付、年付按月折算。点卡片按月费从高到低排。', ...m.cost }),
      ].join('');
    }

    // ─── 工具条 ───
    function statusItems() { return table.segmentCounts().map((s) => ({ value: s.key, label: s.label, count: s.count })); }
    function teamItems() {
      const counts = new Map(table.facetCounts('team').map((f) => [f.value, f.count]));
      const total = [...counts.values()].reduce((a, b) => a + b, 0);
      return [{ value: '', label: '全部', count: total }, ...store.teams().map((t) => ({ value: t.name, label: t.name, count: counts.get(t.name) ?? 0 }))];
    }
    function dropButtons() {
      const planSel = table.filterValues('plan'), relaySel = table.filterValues('relay');
      const lbl = (w, sel, word) => (sel.length === 1 ? `${w}：${word(sel[0])}` : sel.length ? `${w} · ${sel.length}` : w);
      return [
        button({ label: lbl('套餐', planSel, planName), caret: true, attrs: { 'data-act': 'plan-menu', 'aria-haspopup': 'menu', 'aria-expanded': 'false', 'aria-pressed': planSel.length ? 'true' : 'false' } }),
        button({ label: lbl('线路机', relaySel, (v) => v), caret: true, attrs: { 'data-act': 'relay-menu', 'aria-haspopup': 'menu', 'aria-expanded': 'false', 'aria-pressed': relaySel.length ? 'true' : 'false' } }),
        button({ label: groupBy ? GROUPS[groupBy] : '分组', icon: 'layers', caret: true, attrs: { 'data-act': 'group-menu', 'aria-haspopup': 'menu', 'aria-expanded': 'false', 'aria-pressed': groupBy ? 'true' : 'false' } }),
      ].join('');
    }
    function countText() {
      const n = table.count(), t = table.total();
      return n === t ? `${fmt.num(t)} 台` : `${fmt.num(n)} / ${fmt.num(t)} 台`;
    }
    function chipsHtml() {
      const chips = table.chips();
      if (!chips.length) return '';
      return `<span class="toolbar-label">已生效</span><div class="toolbar-chips">${chips.map((c, i) => removableChip(c.kind === 'token' ? c.label : c.label.replace('：直连', '：未绑定'), { 'data-chip': i })).join('')}</div>${button({ label: '清除全部', variant: 'quiet', size: 'sm', attrs: { 'data-act': 'clear-all' } })}`;
    }

    // ─── 表格列（SPEC §6.3：11 列，每行最多两行文字；数值列右对齐、全部可排序） ───
    const leaving = new Map(); // id → timer：离开当前筛选、正在淡出的行（§5.3 规则 4）
    const rateFresh = (n) => n.rate && n.rate.fresh !== false && n.rate.sample_at != null && now() - n.rate.sample_at <= 120;
    const dash = '<span class="t-muted">—</span>';
    const COLS = [
      { key: 'name', label: '节点', sortable: true, width: 113, cls: 'has-copy',
        cell: (n) => `<span class="strong">${esc(n.name)}</span>`,
        sub: (n) => (leaving.has(n.id) ? '<span class="t-muted">已不符合筛选</span>' : copyable(n.ip)) },
      { key: 'team', width: 110, sorts: [{ key: 'team', label: '团队' }, { key: 'relay', label: '入口' }],
        cell: (n) => esc(n.team ?? '—'),
        sub: (n) => (n.isRelay || n.entry_label ? esc(M.entryText(store, n)) : '<span class="t-muted">未绑定</span>') },
      { key: 'plan', label: '套餐', sortable: true, width: 155,
        cell: (n) => { const p = planOf(n); return `<span class="${p.unresolved ? 't-muted' : ''}" data-tip="${esc(`${p.name}\n${planLine(p)}`)}">${esc(p.name)}</span>${p.raw ? ' ' + mark('原名', { warn: true }) : ''}`; },
        sub: (n) => { const bw = M.bandwidthF(n); const u = M.utilOf(n); return `<span data-tip="${esc(`带宽 ${bw.text}${u ? ` · 业务利用率 ${u.text}（上下行分别计算）` : ''}`)}">${esc(bw.text)}${u && bw.kind === 'value' ? ` · ${esc(u.text)}` : ''}</span>`; } },
      { key: 'status', label: '状态', sortable: true, width: 131,
        cell: (n) => statusPill(M.healthOf(store, n)),
        sub: (n) => {
          const f = fmt.statusLine(n.status);
          if (fmt.healthCode(n.status?.code) === 'ok' && f.kind !== 'word') return `最近检查 ${esc(f.text)}`;
          return `<span data-tip="${esc([f.text, f.exact].filter(Boolean).join('\n'))}">${txt(f)}</span>`;
        } },
      { key: 'latency', align: 'right', width: 91, sorts: [{ key: 'latency', label: 'TCP' }, { key: 'egress', label: '出口' }],
        cell: (n) => {
          const ms = n.latency?.tcp_ms;
          const tf = M.probeFail(n, 'tcp');
          if (tf) return `<span class="t-secondary">TCP</span> <span class="v">${num(M.latencyF(n, 'tcp'))}</span>`;
          if (ms == null) return dash;
          const t = fmt.latencyTone(ms, 'tcp', n.limits ?? {});
          return `<span class="t-secondary">TCP</span> <span class="v">${num(fmt.latency(ms), { cls: t === 'warn' || t === 'bad' ? `t-${t}` : '' })}</span>`;
        },
        sub: (n) => {
          const L = n.latency ?? {};
          if (n.status?.reason === 'retrying' || L.egress_pending) return '出口 <span class="t-info">重试中</span>';
          if (M.probeFail(n, 'egress')) return `出口 ${num(M.latencyF(n, 'egress'))}`;
          if (L.egress_ms == null) return n.isRelay ? '' : '出口 <span class="t-muted">—</span>';
          const t = fmt.latencyTone(L.egress_ms, 'egress', { latency_ms: L.egress_limit_ms ?? n.limits?.latency_ms });
          return `出口 ${num(fmt.latency(L.egress_ms), { cls: t === 'warn' ? 't-warn' : '' })}`;
        } },
      { key: 'avail30', label: '近 30 天', sortable: true, align: 'right', width: 74,
        cell: (n) => { const w = n.availability?.windows?.['30']; return w?.availability_percent == null ? dash : `<span class="v">${num(fmt.pctAvail(w.availability_percent))}</span>`; },
        sub: (n) => esc(fmt.coverage(n.availability?.windows?.['30']?.coverage_percent).text) },
      { key: 'rate', label: '业务速率', sortable: true, align: 'right', width: 100,
        cell: (n) => (rateFresh(n) ? `<span class="v">${num(fmt.rate(n.rate.down_bps, { dir: 'down' }), { title: false })}</span>` : dash),
        sub: (n) => (rateFresh(n) ? num(fmt.rate(n.rate.up_bps, { dir: 'up' }), { title: false }) : '<span class="t-muted">未采样</span>') },
      { key: 'month', label: '本月', sortable: true, align: 'right', width: 102,
        cell: (n) => { const m = n.traffic?.month; return m ? `<span class="v">${num(fmt.bytes((m.down ?? 0) + (m.up ?? 0)))}</span>` : dash; },
        sub: (n) => {
          const q = n.traffic?.quota;
          if (q?.limit_bytes) {
            const pct = ((q.used_bytes ?? 0) / q.limit_bytes) * 100;
            const tone = quotaTone(pct);
            return `<span data-tip="${esc(`流量上限 ${fmt.bytes(q.used_bytes).text} / ${fmt.bytes(q.limit_bytes).text}${q.type_word ? ` · ${q.type_word}` : ''}`)}"><span class="meter-track"${tone ? ` data-tone="${tone}"` : ''}><span class="meter-fill" style="width:${Math.min(100, pct).toFixed(1)}%"></span></span>上限 ${num(fmt.pctResource(pct))}</span>`;
          }
          const t = n.traffic?.today;
          return t ? `今日 ${num(fmt.bytes((t.down ?? 0) + (t.up ?? 0)))}` : '';
        } },
      { key: 'cpu', label: '主机', sortable: true, width: 160,
        cell: (n) => {
          if (!n.hasHost) return neutralChip('无主机', n.isProxy ? '家宽代理产品，没有可装 agent 的主机' : '没有主机 agent');
          if (M.isStale(n)) return '<span class="t-muted">主机数据过期</span>';
          const h = n.host ?? {};
          const L = n.limits ?? {};
          return tableMeter([
            { label: 'CPU', value: fmt.pctResource(h.cpu_pct), pct: h.cpu_pct, tone: fmt.meterTone(h.cpu_pct, L.cpu_percent ?? 90), tip: `CPU ${fmt.pctResource(h.cpu_pct, { precise: true }).text} · ${h.cores ?? '—'} 核 · 阈值 ${L.cpu_percent ?? 90}%` },
            // 两行同一口径：主值都是百分比，用量放进提示（与首页卡片“内存 6%”一致，consistency #14、craft #31）
            { label: '内存', value: fmt.pctResource(h.mem_pct), pct: h.mem_pct, tone: fmt.meterTone(h.mem_pct, L.memory_percent ?? 90), tip: `内存 ${fmt.pctResource(h.mem_pct, { precise: true }).text} · ${fmt.bytes(h.mem_used).text} / ${fmt.bytes(h.mem_total).text} · 阈值 ${L.memory_percent ?? 90}%` },
          ], { width: 140 });
        },
        sub: (n) => (n.hasHost && M.isStale(n) ? esc(M.lastReport(n) ? `最后上报 ${fmt.ago(M.lastReport(n)).text}` : '没有上报') : '') },
      { key: 'cost', align: 'right', width: 82, sorts: [{ key: 'cost', label: '月费' }, { key: 'due', label: '到期' }],
        cell: (n) => `<span class="v">${num(fmt.monthlyFee(n.billing))}</span>`,
        sub: (n) => M.dueHtml(n) },
    ];

    // ─── 行集合：表格行 + 正在淡出的行（插回原位置） ───
    let order = [];          // 当前 DOM 里的行顺序（抽屉 [ ] 与 j/k 用）
    function groupsNow() {
      if (!groupBy) return null;
      return table.groups().map((g) => {
        const cents = g.rows.reduce((a, n) => a + (n.billing?.monthly_cents ?? 0), 0);
        const month = g.rows.reduce((a, n) => a + (n.traffic?.month?.down ?? 0) + (n.traffic?.month?.up ?? 0), 0);
        return { ...g, end: `${esc(fmt.money(cents, { perMonth: true }).text)} · 本月 ${esc(fmt.bytes(month).text)}` };
      });
    }
    function flatRows(groups) {
      const list = groups ? groups.flatMap((g) => (g.collapsed ? [] : g.rows)) : table.rows();
      if (!leaving.size || groups) return list;
      const out = list.slice();
      for (const id of leaving.keys()) {
        const n = store.node(id);
        if (!n || out.includes(n)) continue;
        out.splice(Math.min(Math.max(0, order.indexOf(id)), out.length), 0, n);
      }
      return out;
    }

    // ─── 首屏 ───
    el.innerHTML = `<div class="page" data-page="nodes">
      ${pageHead({ title: '节点', tools: HEAD_TOOLS.nodes({ density: app.density() }) })}
      <section class="kpi-grid" data-kpis aria-label="概览">${kpisHtml()}</section>
      <section class="panel" aria-label="节点列表">
        <div class="toolbar">
          <div class="toolbar-row">
            ${segmented({ name: 'status', items: statusItems(), value: table.segment(), label: '状态视图', disableZero: true })}
            <div class="toolbar-end">${segmented({ name: 'team', items: teamItems(), value: table.filterValues('team')[0] ?? '', label: '团队', disableZero: true })}</div>
          </div>
          <div class="toolbar-row">
            ${searchBox({ name: 'q', value: table.query(), placeholder: '搜索名称、IP、服务号、线路机', label: '搜索节点（支持 team: relay: status: plan: due:）' })}
            <span data-drops class="row">${dropButtons()}</span>
            <span class="toolbar-end toolbar-count" data-count aria-live="polite">${countText()}</span>
          </div>
          <div class="toolbar-row" data-chips${table.chips().length ? '' : ' hidden'}>${chipsHtml()}</div>
        </div>
        <div data-table style="--dt-min:${TABLE_MIN}px"></div>
      </section>
    </div>`;
    const tableBox = el.querySelector('[data-table]');
    const offStuck = watchStuck(el);

    function renderTable({ enter = [] } = {}) {
      const groups = groupsNow();
      const rows = flatRows(groups);
      const emptyHtml = store.nodes().length
        ? empty({ icon: 'inbox', text: '没有符合筛选条件的节点', action: { label: '清除筛选', attrs: { 'data-act': 'clear-all' } } })
        : empty({ text: '还没有节点', action: { label: '新增家宽节点', attrs: { 'data-act': 'add' } } });
      tableBox.innerHTML = dataTable({
        columns: COLS, rows: groups ? [] : rows, groups,
        sortOf: table.sortOf, current: app.drawerNodeId?.(), label: '节点', cls: 'is-sticky-first is-fixed is-tight', empty: emptyHtml,
      });
      order = rows.map((n) => n.id);
      syncRowState(enter);
      health = new Map(rows.map((n) => [n.id, M.healthOf(store, n).code]));
    }
    function syncRowState(enter = []) {
      for (const tr of tableBox.querySelectorAll('tr[data-row]')) {
        tr.classList.toggle('is-leaving', leaving.has(tr.dataset.row));
        if (enter.includes(tr.dataset.row)) tr.classList.add('is-entering');
      }
    }
    function renderChrome() {
      el.querySelector('[data-kpis]').innerHTML = kpisHtml();
      const st = el.querySelector('[data-seg="status"]');
      setSegmentCounts(st, Object.fromEntries(statusItems().map((s) => [s.value, s.count])));
      setSegmented(st, table.segment());
      const tm = el.querySelector('[data-seg="team"]');
      setSegmentCounts(tm, Object.fromEntries(teamItems().map((t) => [t.value, t.count])));
      setSegmented(tm, table.filterValues('team')[0] ?? '');
      setSearch(el, 'q', table.query());
      el.querySelector('[data-drops]').innerHTML = dropButtons();
      el.querySelector('[data-count]').textContent = countText();
      const chips = el.querySelector('[data-chips]');
      const ch = chipsHtml();
      chips.hidden = !ch;
      chips.innerHTML = ch;
    }
    let health = new Map();
    renderTable();
    hydrate(el);

    // ─── 交互 ───
    const offs = [offStuck];
    offs.push(onSegment(el, 'density', (v) => app.setDensity(v)));
    // ⌘⇧D 由外壳切换密度：跟着更新分段
    const mo = new MutationObserver(() => { const s = el.querySelector('[data-seg="density"]'); if (s) setSegmented(s, app.density()); });
    const mainEl = el.closest('main');
    if (mainEl) mo.observe(mainEl, { attributes: true, attributeFilter: ['data-density'] });
    offs.push(onSegment(el, 'status', (v) => { table.setSegment(v === 'all' ? null : v); commit(); }));
    offs.push(onSegment(el, 'team', (v) => { table.setFilter('team', v ? [v] : []); commit(); }));

    // 搜索：token 自动补全挂在 onSearch 之前（↑↓ Enter Tab Esc 先给建议列表）
    const searchSel = '[data-search="q"] input';
    offs.push(on(el, 'keydown', searchSel, (e, input) => {
      if (e.isComposing || !sugg) return;
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); moveSugg(e.key === 'ArrowDown' ? 1 : -1); e.stopImmediatePropagation(); }
      else if ((e.key === 'Enter' || e.key === 'Tab') && sugg.items.length) { e.preventDefault(); e.stopImmediatePropagation(); acceptSugg(input, sugg.active); }
      else if (e.key === 'Escape') { e.stopImmediatePropagation(); closeSugg(); }
    }));
    offs.push(on(el, 'input', searchSel, (e, input) => { if (!e.isComposing) showSugg(input); }));
    offs.push(on(el, 'focusout', searchSel, () => setTimeout(() => { if (!el.querySelector('[data-search="q"]')?.contains(document.activeElement)) closeSugg(); }, 120)));
    offs.push(onSearch(el, 'q', (v) => { table.setQuery(v); commit(); }, { delay: 150 }));

    offs.push(on(el, 'click', '[data-act]', (e, b) => {
      const act = b.dataset.act;
      if (act === 'add') openEnrollWizard(app); // 与运营总览、线路页同一个向导（js/enroll-wizard.js）
      else if (act === 'export') exportCsv();
      else if (act === 'refresh') refresh(b);
      else if (act === 'clear-all') { table.reset(); commit(); }
      else if (act === 'plan-menu') facetMenu(b, 'plan', '套餐');
      else if (act === 'relay-menu') facetMenu(b, 'relay', '线路机');
      else if (act === 'group-menu') groupMenu(b);
    }));
    offs.push(on(el, 'click', '[data-chip]', (e, b) => { const c = table.chips()[Number(b.dataset.chip)]; if (c) { c.remove(); commit(); } }));
    offs.push(on(el, 'click', '[data-kpi]', (e, k) => {
      if (e.target.closest('[data-info], .kpi-action')) return; // ⓘ 口径说明不当作点卡片
      const key = k.dataset.kpi;
      if (key === 'cost') { app.setParams({ sort: app.params().get('sort') === '-cost' ? null : '-cost' }); return; }
      if (key !== 'attention' && key !== 'due7') return;
      table.setSegment(table.segment() === key ? null : key);
      commit();
    }));
    offs.push(on(el, 'click', '[data-sort]', (e, b) => { table.setSort(b.dataset.sort, { multi: e.shiftKey }); commit(); }));
    offs.push(on(el, 'click', '[data-group-toggle]', (e, b) => { table.toggleGroup(b.dataset.groupToggle); renderTable(); }));
    offs.push(on(el, 'click', 'tr[data-row]', (e, tr) => {
      if (e.target.closest('button, a, input, [data-copy]')) return;
      if (String(getSelection?.() ?? '').trim()) return; // 在选文字，不当点击
      app.openNode(tr.dataset.row, 'overview');
    }));
    offs.push(on(el, 'keydown', 'tr[data-row]', (e, tr) => {
      if (e.target !== tr || e.isComposing) return;
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); app.openNode(tr.dataset.row, 'overview'); }
    }));
    // 鼠标或焦点在表里：暂停实时重排（§5.3 规则 3）
    let hovering = false, pendingReorder = false;
    offs.push(on(tableBox, 'pointerenter', null, () => { hovering = true; }));
    offs.push(on(tableBox, 'pointerleave', null, () => { hovering = false; if (pendingReorder) liveApply(); }));
    offs.push(on(tableBox, 'focusout', null, () => setTimeout(() => { if (pendingReorder && !tableBox.contains(document.activeElement)) liveApply(); }, 0)));

    function facetMenu(b, key, word) {
      const counts = table.facetCounts(key);
      const known = key === 'plan' ? allPlans(store).map((p) => p.id) : [...new Set(store.nodes().map((n) => n.relay ?? '未绑定'))].sort();
      const byVal = new Map(counts.map((c) => [c.value, c]));
      const sel = new Set(table.filterValues(key));
      const items = [
        { group: `${word}（可多选）` },
        ...known.map((v) => ({ value: v, label: key === 'plan' ? planName(v) : v, checked: sel.has(v), count: byVal.get(v)?.count ?? 0 })),
        ...(sel.size ? [{ sep: true }, { value: '__clear', label: `清除${word}筛选`, icon: 'x' }] : []),
      ];
      openMenu(b, {
        label: word, multi: true, minWidth: 240,
        items,
        onSelect: (v) => {
          if (v === '__clear') table.setFilter(key, []);
          else table.toggleFilter(key, v);
          commit(); // 菜单保持打开（多选），按钮文字与表格随 URL 更新
        },
      });
    }
    function groupMenu(b) {
      openMenu(b, {
        label: '分组', minWidth: 180,
        items: [{ group: '分组' }, ...Object.entries(GROUPS).map(([k, w]) => ({ value: k, label: w, checked: groupBy === k }))],
        onSelect: (v) => { groupBy = v; table.setGroupBy(v || null); commit(); },
      });
    }
    function refresh(b) {
      b.classList.add('is-busy');
      b.setAttribute('aria-busy', 'true');
      const done = () => { b.classList.remove('is-busy'); b.removeAttribute('aria-busy'); if (!el.isConnected) return; table.resort(); renderChrome(); renderTable(); };
      if (isLive() && app.live.refresh) app.live.refresh().then(done, done); // 实时：真的重读一次
      else setTimeout(done, 600);
    }

    // ─── 搜索建议（token 自动补全） ───
    let sugg = null;
    const TOKEN_KEYS = [
      ['team', '团队'], ['relay', '线路机'], ['status', '状态'], ['plan', '套餐'], ['due', '到期'],
      ['provider', '供应商'], ['region', '地区'], ['cap', '能力'], ['ip', 'IP 前缀或 *'],
    ];
    function tokenValues(key) {
      if (key === 'team') return store.teams().map((t) => ({ v: t.name, label: t.name }));
      if (key === 'relay') return [...new Set(store.nodes().map((n) => n.relay).filter(Boolean))].sort().map((r) => ({ v: r, label: r }));
      if (key === 'status') return [['attention', '需处理'], ['ok', '正常'], ['degraded', '降级'], ['fail', '故障'], ['maintenance', '维护中'], ['unknown', '未知'], ['stale', '数据过期']].map(([v, label]) => ({ v, label }));
      if (key === 'plan') return allPlans(store).map((p) => ({ v: /\s/.test(p.name) ? `"${p.name}"` : p.name, label: p.name, match: `${p.name} ${p.id}` }));
      if (key === 'due') return [['<7d', '7 天内（含已过期）'], ['expired', '已过期'], ['>30', '30 天以上']].map(([v, label]) => ({ v, label }));
      if (key === 'provider') return [...new Set(store.nodes().map((n) => n.provider).filter(Boolean))].map((v) => ({ v, label: v }));
      if (key === 'region') return [...new Set(store.nodes().map((n) => n.region?.code).filter(Boolean))].map((v) => ({ v, label: store.nodes().find((n) => n.region?.code === v)?.region?.name ?? v }));
      if (key === 'cap') return ['host', 'proxy', 'relay', 'no_host'].map((v) => ({ v, label: fmt.CAPABILITY?.[v] ?? v }));
      return [];
    }
    function fragmentOf(input) {
      const pos = input.selectionStart ?? input.value.length;
      const before = input.value.slice(0, pos);
      const start = before.search(/\S+$/);
      return start < 0 ? { text: '', start: pos, end: pos } : { text: before.slice(start), start, end: pos };
    }
    function suggestions(frag) {
      const t = frag.text;
      if (!t) return [];
      const i = t.indexOf(':');
      if (i < 0) {
        if (!/^[a-z]+$/i.test(t)) return [];
        return TOKEN_KEYS.filter(([k]) => k.startsWith(lc(t)) && k !== lc(t)).map(([k, w]) => ({ insert: `${k}:`, label: `${k}:`, hint: w, more: true }));
      }
      const key = lc(t.slice(0, i)), part = lc(t.slice(i + 1).split(',').pop());
      const prefix = t.slice(0, t.length - part.length);
      const bare = part.replace(/^"/, '');
      return tokenValues(key).filter((x) => !bare || lc(x.v).replace(/^"/, '').startsWith(bare) || lc(x.match ?? x.label).includes(bare)).slice(0, 8)
        .map((x) => ({ insert: `${prefix}${x.v} `, label: `${prefix}${x.v}`, hint: x.label !== x.v && `"${x.label}"` !== x.v ? x.label : '' }));
    }
    function suggHtml(items, active) {
      return `<div role="listbox" aria-label="搜索建议">${items.map((it, i) => `<button type="button" class="menu-item${i === active ? ' is-active' : ''}" role="option" aria-selected="${i === active}" data-i="${i}" tabindex="-1"><span>${esc(it.label)}</span>${it.hint ? `<span class="menu-item-end">${esc(it.hint)}</span>` : ''}</button>`).join('')}<div class="suggest-hint">↑↓ 选择 · ↵ 或 Tab 补全 · Esc 关闭</div></div>`;
    }
    function showSugg(input) {
      const items = suggestions(fragmentOf(input));
      if (!items.length) { closeSugg(); return; }
      const box = input.closest('[data-search]');
      if (sugg && currentPopover() === sugg.api) { sugg.items = items; sugg.active = 0; sugg.api.update(suggHtml(items, 0)); return; }
      const api = openPopover(box, { html: suggHtml(items, 0), placement: 'bottom-start', cls: 'menu', role: 'presentation', onClose: () => { sugg = null; } });
      api.el.style.minWidth = `${box.offsetWidth}px`;
      api.reposition();
      sugg = { api, items, active: 0 };
      api.el.addEventListener('pointerdown', (ev) => ev.preventDefault()); // 不抢输入框焦点
      api.el.addEventListener('click', (ev) => { const x = ev.target.closest('[data-i]'); if (x) acceptSugg(input, Number(x.dataset.i)); });
    }
    function moveSugg(d) {
      if (!sugg) return;
      sugg.active = (sugg.active + d + sugg.items.length) % sugg.items.length;
      sugg.api.update(suggHtml(sugg.items, sugg.active));
    }
    function acceptSugg(input, i) {
      const it = sugg?.items[i];
      if (!it) return;
      const f = fragmentOf(input);
      input.value = input.value.slice(0, f.start) + it.insert + input.value.slice(f.end);
      const caret = f.start + it.insert.length;
      input.setSelectionRange(caret, caret);
      input.dispatchEvent(new Event('input', { bubbles: true }));
      if (!it.more) closeSugg();
      input.focus();
    }
    function closeSugg() { if (sugg && currentPopover() === sugg.api) closePopover(); sugg = null; }

    // ─── 实时更新（§5.3）：KPI、分段计数、单元格补丁；成员变化时淡出 / 淡入；行序节流 + FLIP ───
    function liveApply() {
      table.invalidate();
      const groups = groupsNow();
      const next = table.rows();
      const cur = [...tableBox.querySelectorAll('tbody > tr[data-row]')].map((tr) => tr.dataset.row);
      const nextIds = new Set((groups ? groups.flatMap((g) => g.rows) : next).map((n) => n.id));
      // 离开筛选：先淡出 5 秒再收起
      for (const id of cur) {
        if (nextIds.has(id) || leaving.has(id)) continue;
        if (groups) continue; // 分组视图直接随重画收起
        leaving.set(id, setTimeout(() => { leaving.delete(id); renderTable(); }, 5000));
      }
      for (const id of [...leaving.keys()]) if (nextIds.has(id)) { clearTimeout(leaving.get(id)); leaving.delete(id); }
      const rows = flatRows(groups);
      const entered = rows.map((n) => n.id).filter((id) => !cur.includes(id));
      if (patchTable(tableBox, { columns: COLS, rows })) {
        pendingReorder = false;
        syncRowState();
        flashChanged(rows);
        return;
      }
      const sameSet = !entered.length && rows.length === cur.length;
      if (sameSet && !groups) {
        // 只是顺序变了（速率排序节流到点）：鼠标或焦点在表里就先不动
        if (hovering || tableBox.contains(document.activeElement) || currentPopover()) { pendingReorder = true; return; }
        pendingReorder = false;
        flipTo(rows);
        patchTable(tableBox, { columns: COLS, rows });
        syncRowState();
        flashChanged(rows);
        return;
      }
      pendingReorder = false;
      renderTable({ enter: entered });
    }
    function flipTo(rows) {
      const body = tableBox.querySelector('tbody');
      const trs = new Map([...body.querySelectorAll('tr[data-row]')].map((tr) => [tr.dataset.row, tr]));
      const first = new Map([...trs].map(([id, tr]) => [id, tr.getBoundingClientRect().top]));
      for (const n of rows) { const tr = trs.get(n.id); if (tr) body.appendChild(tr); }
      order = rows.map((n) => n.id);
      if (matchMedia('(prefers-reduced-motion: reduce)').matches) return;
      for (const [id, tr] of trs) {
        const dy = first.get(id) - tr.getBoundingClientRect().top;
        if (dy) tr.animate([{ transform: `translateY(${dy}px)` }, { transform: 'none' }], { duration: 220, easing: 'cubic-bezier(.4,0,.2,1)' });
      }
    }
    function flashChanged(rows) {
      for (const n of rows) {
        const code = M.healthOf(store, n).code;
        if (health.has(n.id) && health.get(n.id) !== code) {
          const pill = tableBox.querySelector(`tr[data-row="${CSS.escape(n.id)}"] .pill`);
          if (pill) { pill.classList.remove('is-flash'); void pill.offsetWidth; pill.classList.add('is-flash'); }
        }
        health.set(n.id, code);
      }
    }
    const patchChrome = () => {
      const m = kpiModel();
      const k = el.querySelector('[data-kpis]');
      for (const key of Object.keys(m)) patchKpi(k, key, m[key]);
      setSegmentCounts(el.querySelector('[data-seg="status"]'), Object.fromEntries(statusItems().map((s) => [s.value, s.count])));
      setSegmentCounts(el.querySelector('[data-seg="team"]'), Object.fromEntries(teamItems().map((t) => [t.value, t.count])));
      const c = el.querySelector('[data-count]');
      const t = countText();
      if (c.textContent !== t) c.textContent = t;
      for (const g of groupsNow() ?? []) {
        const end = tableBox.querySelector(`tr[data-group="${CSS.escape(g.key)}"] [data-group-end]`);
        if (end && end.innerHTML !== g.end) end.innerHTML = g.end;
      }
    };
    const offLive = store.on('change', () => {
      if (!el.isConnected) return;
      patchChrome();
      liveApply();
    });
    offs.push(offLive);
    // 相对时间（最近检查 N 秒前、最后上报）每 30 秒刷新
    const tick = setInterval(() => { if (el.isConnected) patchTable(tableBox, { columns: COLS, rows: flatRows(groupsNow()) }); }, 30000);

    // ─── 键盘：j/k 在行间移动焦点，Enter / o 打开抽屉（§5.4） ───
    const rowEls = () => [...tableBox.querySelectorAll('tr[data-row]')];
    function moveFocus(d) {
      const trs = rowEls();
      if (!trs.length) return;
      const i = trs.indexOf(document.activeElement);
      const next = trs[i < 0 ? (d > 0 ? 0 : trs.length - 1) : Math.max(0, Math.min(trs.length - 1, i + d))];
      next.focus({ preventScroll: true });
      next.scrollIntoView({ block: 'nearest', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' });
    }
    const inTable = () => tableBox.contains(document.activeElement);
    const unbind = app.keys.bindAll([
      { combo: 'j', run: () => moveFocus(1), desc: '下一行', group: '节点', when: () => !app.drawerOpen() },
      { combo: 'k', run: () => moveFocus(-1), desc: '上一行', group: '节点', when: () => !app.drawerOpen() },
      { combo: 'ArrowDown', run: () => moveFocus(1), hidden: true, when: () => !app.drawerOpen() && inTable() },
      { combo: 'ArrowUp', run: () => moveFocus(-1), hidden: true, when: () => !app.drawerOpen() && inTable() },
      { combo: 'o', run: () => { const tr = document.activeElement.closest?.('tr[data-row]'); if (tr) app.openNode(tr.dataset.row, 'overview'); }, desc: '打开焦点所在行的详情', group: '节点', when: () => !app.drawerOpen() && inTable() },
    ]);

    // ─── 导出 CSV（当前视图，列同表格；套餐列是规范名） ───
    app.__nodesExport = () => exportCsv({ download: false }); // 调试 / 测试：只取 CSV 文本，不下载
    function exportCsv({ download = true } = {}) {
      const rows = table.rows();
      const head = ['节点', 'IP', '团队', '入口', '套餐', '带宽', '状态', '状态说明', 'TCP 延迟 ms', '出口验证 ms', '近 30 天成功率 %', '采样覆盖 %', '下行 bps', '上行 bps', '本月业务流量 B', '今日业务流量 B', 'CPU %', '内存 %', '月费 USD', '到期日', '剩余天数', '账期时区'];
      // 以 = + - @ 开头的文本在表格软件里会被当成公式：前置 '（数字列是 number，不受影响）
      const cell = (v) => { let s = v == null ? '' : String(v); if (typeof v === 'string' && /^[=+\-@\t\r]/.test(s)) s = `'${s}`; return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s; };
      const lines = rows.map((n) => {
        const d = store.dueOf(n);
        const w = n.availability?.windows?.['30'] ?? {};
        return [n.name, n.ip, n.team, M.entryText(store, n), planOf(n).name, n.bandwidth?.text ?? '', M.healthOf(store, n).word, fmt.statusLine(n.status).text,
          n.latency?.tcp_ms ?? '', n.latency?.egress_ms ?? '', w.availability_percent ?? '', w.coverage_percent ?? '',
          rateFresh(n) ? n.rate.down_bps : '', rateFresh(n) ? n.rate.up_bps : '',
          n.traffic?.month ? (n.traffic.month.down ?? 0) + (n.traffic.month.up ?? 0) : '', n.traffic?.today ? (n.traffic.today.down ?? 0) + (n.traffic.today.up ?? 0) : '',
          n.host?.cpu_pct ?? '', n.host?.mem_pct ?? '', n.billing?.monthly_cents != null ? (n.billing.monthly_cents / 100).toFixed(2) : '',
          n.billing?.due_date ?? '', d.days ?? '', n.billing?.billing_timezone ?? ''].map(cell).join(',');
      });
      const csv = `\uFEFF${head.map(cell).join(',')}\n${lines.join('\n')}\n`;
      if (!download) return csv;
      const a = document.createElement('a');
      a.href = URL.createObjectURL(new Blob([csv], { type: 'text/csv;charset=utf-8' }));
      a.download = `节点-${fmt.date(now()).text}.csv`;
      document.body.appendChild(a);
      a.click();
      setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 1000);
      toast({ message: `已导出 ${rows.length} 行` });
      return csv;
    }

    return {
      nodeOrder: () => order,
      /** 抽屉打开 / 切换：高亮当前行（--selected + 首列 2 px 强调色），并把它滚进视野。 */
      onDrawer(id) {
        for (const tr of rowEls()) {
          const on_ = tr.dataset.row === id;
          tr.classList.toggle('is-current', on_);
          if (on_) tr.scrollIntoView({ block: 'nearest' });
        }
      },
      update(p) {
        const key = keyOf(p);
        if (key === renderedKey) return;
        renderedKey = key;
        readParams(p);
        for (const t of leaving.values()) clearTimeout(t);
        leaving.clear();
        renderChrome();
        renderTable();
        hydrate(el);
      },
      unmount() {
        offs.forEach((f) => f());
        mo.disconnect();
        clearInterval(tick);
        for (const t of leaving.values()) clearTimeout(t);
        closeSugg();
        unbind();
        delete app.__nodesExport;
      },
    };
  },
};
