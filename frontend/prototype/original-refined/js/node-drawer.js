// node-drawer.js — 节点抽屉（SPEC §3.16、§6.3）。任何页面都用 app.openNode(id, tab) 打开它，不自己实现详情。
// 页签：概览 · 连接质量 · 流量 · 主机（有主机才有，收纳原首页详情页 /instance/<id>）· 费用 · 访问记录 · 管理。
// 外形就是原运营抽屉：右侧 760、灰底里放白卡、顶部页签（左对齐）；覆盖整个视口。

import { fmt, charts, createPatcher } from '../../shared/index.js';
import { icon } from './icons.js';
import { esc, num, txt } from './ui/dom.js';
import { openDrawer, confirmAction, openDialog, toast } from './ui/layers.js';
import { tabs as tabsHtml, statusPill, tonePill, button, iconButton, copyable, segmented, onSegment, disclosure, mark, tagChip, infoButton, setSegmented, hydrate } from './ui/controls.js';
import { latencyStrip, patchBeat, dayTracker, dayLegend, mountChart, chartLegend, chartTable, kpiTile } from './ui/viz.js';
import { kv, dlRows, note, empty, skel } from './ui/layout.js';
import * as M from './node-model.js';
import { planOf, membersOf } from './plans.js';
import { egressCard, fillEgressIps, sourcesCard, sourcesTableCard, fillSourcesTable, bindIpintel, sourcesTimeout } from './node-ipintel.js';
import { ensureIntel, toRecord } from './live-intel.js';
import { intelOf, sourceCell, sourceLineHtml } from './ui/intel.js';
import { isLive, guard, isWired } from './live-mode.js';
import { drawerAction } from './live-actions.js';

// 只读预览里被拦下的抽屉操作：data-dz → live-mode.js ACTIONS 的 key
const DRAWER_WRITES = {
  disable: 'drawer.disable', enable: 'drawer.enable', reset: 'drawer.reset', rotate: 'drawer.rotate', verify: 'drawer.verify', remark: 'drawer.remark',
  monitor: 'drawer.monitor', edit: 'drawer.editAsset', renew: 'drawer.renew', ack: 'drawer.ack', 'ack-all': 'drawer.ackAll',
};

export const DRAWER_TABS = [
  { value: 'overview', label: '概览' },
  { value: 'quality', label: '连接质量' },
  { value: 'traffic', label: '流量' },
  { value: 'host', label: '主机' },
  { value: 'billing', label: '费用' },
  { value: 'access', label: '访问记录' },
  { value: 'manage', label: '管理' },
];

const patcher = createPatcher();
let current = null;

/**
 * openNodeDrawer(app, id, tab) — 已打开时切换节点 / 页签，不重建外壳。
 * app 需要：store、nodeOrder()（当前页的节点顺序，给 [ ] 切换）、setDrawerParams(id, tab)、closeDrawerParams()、navigate(hash)。
 */
export function openNodeDrawer(app, id, tab = 'overview') {
  const store = app.store;
  const n = store.node(id);
  if (!n) return null;
  const tabList = DRAWER_TABS.filter((t) => t.value !== 'host' || n.hasHost);
  if (!tabList.some((t) => t.value === tab)) tab = 'overview';
  if (current && current.el.isConnected) {
    const same = current.nodeId === id;
    current.nodeId = id;
    current.setTabs(tabsHtml({ name: 'drawer', items: tabList, value: tab, label: '节点详情页签' }));
    current.setHead(head(store, n));
    if (!same || current.tab !== tab) current.setTab(tab, { silent: true });
    return current;
  }
  const charts_ = [];
  const state = { nodeId: id };
  const api = openDrawer({
    label: `节点详情 ${n.name}`,
    ...head(store, n),
    tabsHtml: tabsHtml({ name: 'drawer', items: tabList, value: tab, label: '节点详情页签' }),
    tab,
    onNav: (dir) => {
      const order = app.nodeOrder?.() ?? store.nodes().map((x) => x.id);
      const i = order.indexOf(state.nodeId);
      const next = order[(i + dir + order.length) % order.length];
      if (next) app.setDrawerParams(next, api?.tab);
    },
    onRefresh: (btn) => {
      btn.classList.add('is-busy');
      if (isLive() && app.live?.refresh) { const done = () => { btn.classList.remove('is-busy'); api.refresh(); }; app.live.refresh().then(done, done); return; }
      setTimeout(() => { btn.classList.remove('is-busy'); api.refresh(); }, 500);
    },
    onTab: (t) => app.setDrawerParams(state.nodeId, t, { replace: true }),
    onClose: () => { off(); destroyCharts(); current = null; app.closeDrawerParams(); app.onDrawerChange?.(null); },
    returnFocus: () => app.focusTargetFor?.(state.nodeId),
    render: (t, body) => {
      destroyCharts();
      const node = store.node(state.nodeId);
      body.innerHTML = renderTab(app, node, t, charts_);
      afterRender(app, node, t, body, charts_);
      app.onDrawerChange?.(state.nodeId);
    },
  });
  function destroyCharts() { while (charts_.length) charts_.pop()?.then?.((c) => c?.destroy()); }
  Object.defineProperty(api, 'nodeId', { get: () => state.nodeId, set: (v) => { state.nodeId = v; } });
  const off = store.on('change', (diff) => {
    const node = store.node(state.nodeId);
    if (!node || !api.el.isConnected) return;
    if (!diff?.nodes?.[node.id] && !diff?.replay) return;
    api.setHead(head(store, node));
    // 流量页签的读数（近 24 小时峰值、本月覆盖）在逐节点历史到达后才有：到了就重画这一页签（每个节点只发生一次）
    const d = diff?.nodes?.[node.id] ?? {};
    if (api.tab === 'traffic' && Object.keys(d).some((k) => /^traffic\.(peak_24h|last24h|coverage_start|month_partial)/.test(k))) { api.refresh(); return; }
    // 访问页签：新访问记录（activity 增量）只重画“最近连接”卡
    if (api.tab === 'access' && diff?.fleet?.activity?.nodes?.includes(node.id)) patchRecent(app, node);
    // 访问页签：节点详情（来源画像 / 来源 IP）在抽屉打开后才补全，到了就重画这一页签（每个节点一次）
    if (api.tab === 'access' && d.detail && !state.accessFilled?.has(node.id)) { (state.accessFilled ??= new Set()).add(node.id); api.refresh(); return; }
    patchLive(api.body, node, api.tab, store);
  });
  current = api;
  app.onDrawerChange?.(id);
  return api;
}
export function closeNodeDrawer() { current?.close(); }
export const drawerNodeId = () => current?.nodeId ?? null;

function head(store, n) {
  const h = M.healthOf(store, n);
  const meta = [n.ip, n.team, M.entryText(store, n), planOf(n).name].filter(Boolean).map(esc).join(' · ');
  return { title: n.name, pill: statusPill(h), meta };
}

// ─── 页签内容 ───────────────────────────────────────────────────────────────

function renderTab(app, n, tab, charts_) {
  switch (tab) {
    case 'quality': return quality(app, n);
    case 'traffic': return traffic(app, n);
    case 'host': return host(app, n);
    case 'billing': return billing(app, n);
    case 'access': return access(app, n);
    case 'manage': return manage(app, n);
    default: return overview(app, n);
  }
}
const card = (title, body, tools = '') => `<section class="dz-card">${title ? `<div class="dz-head"><h3 class="dz-title">${esc(title)}</h3><div class="dz-tools">${tools}</div></div>` : ''}${body}</section>`;
const cardFlush = (title, body, tools = '', sub = '') => `<section class="dz-card-flush"><div class="panel-head"><h3 class="panel-title">${esc(title)}</h3>${sub ? `<span class="panel-sub">${sub}</span>` : ''}<div class="panel-tools">${tools}</div></div>${body}</section>`;
const S = (k, html, cls = '') => `<span data-s="${k}"${cls ? ` class="${cls}"` : ''}>${html}</span>`;
const bigVal = (f) => num(f, { title: false });

/** 出口（fleetd 0.13.3 起只看连通）：拿到出口 IP = 通，IP 只作信息；没拿到 = 超时 / 失败；从不写“不符”。 */
function egressPill(n) {
  const ip = n.latency?.egress_actual_ip;
  if (ip) return tonePill('连通', 'ok', { title: `出口 IP ${ip}` });
  const f = M.probeFail(n, 'egress');
  if (f) return tonePill(f === 'timeout' ? '超时' : '失败', 'warn', { title: `出口检查${f === 'timeout' ? '超时' : '失败'}，没有拿到出口 IP` });
  return tonePill('未知', 'neutral', { dot: 'ring' });
}
function probeWord(n) {
  if (!n.hasHost && !n.isProxy) return '<span class="t-muted">无主机</span>';
  const s = M.probeState(n);
  return s === 'on' ? '在线' : s === 'off' ? `<span class="t-bad">${esc(M.offlineText(n))}</span>` : '<span class="t-muted">未知</span>';
}

function overviewSlots(store, n) {
  const L = n.latency ?? {};
  const T = n.traffic ?? {};
  const r = n.rate ?? {};
  const st = fmt.statusLine(n.status);
  return {
    checked: esc(`最近检查 ${st.kind === 'word' ? fmt.ago(n.status?.checked_at).text : st.text}`),
    health: statusPill(M.healthOf(store, n)),
    egress: egressPill(n),
    probe: probeWord(n),
    eg: bigVal(M.latencyF(n, 'egress')),
    tcp: bigVal(M.latencyF(n, 'tcp')),
    path: bigVal(M.latencyF(n, 'path')),
    month: bigVal(fmt.bytes((T.month?.down ?? 0) + (T.month?.up ?? 0))),
    monthSub: `↓ ${num(fmt.bytes(T.month?.down))} · ↑ ${num(fmt.bytes(T.month?.up))}`,
    // 实时数据里逐节点 24 小时流量是打开抽屉后才取的：没到之前显示“—”+ 骨架，不显示 0 B
    d24: T.last24h ? bigVal(fmt.bytes((T.last24h.down ?? 0) + (T.last24h.up ?? 0))) : bigVal(fmt.missing('读取中')),
    d24Sub: T.last24h ? `↓ ${num(fmt.bytes(T.last24h.down))} · ↑ ${num(fmt.bytes(T.last24h.up))}` : skel(120, 12),
    rate: `<span class="t-secondary">↓</span> ${bigVal(fmt.rate(r.down_bps))}`,
    rateSub: `↑ ${num(fmt.rate(r.up_bps))}`,
  };
}

function overview(app, n) {
  const store = app.store;
  const s = overviewSlots(store, n);
  const h = M.healthOf(store, n);
  const stNote = h.code !== 'ok' ? note({
    tone: h.tone === 'bad' ? 'bad' : h.tone === 'info' ? 'info' : 'warn',
    title: h.word,
    body: esc([fmt.statusLine(n.status).text, n.status?.first_failed_check ? `首个失败检查：${fmt.checkTitle(n.status.first_failed_check)}` : null].filter(Boolean).join(' · ')),
  }) : '';
  const statusRow = `<div class="dz-statusrow">
    <span class="dz-kv">综合状态 ${S('health', s.health)}</span>
    <span class="dz-kv">出口 ${S('egress', s.egress)}</span>
    <span class="dz-kv">探针 <b>${S('probe', s.probe)}</b></span>
    <span class="dz-kv">团队 <b>${esc(n.team)}</b></span>
    <span class="dz-kv">供应商 <b>${esc(n.provider)}</b></span>
  </div>`;
  const readings = `<div class="card-readings">
    ${kv([{ label: '出口验证', value: S('eg', s.eg) }], { cols: 1, big: true })}
    ${kv([{ label: '上游 TCP', value: S('tcp', s.tcp) }], { cols: 1, big: true })}
    ${kv([{ label: '入口到出口', value: S('path', s.path) }], { cols: 1, big: true })}
  </div>`;
  // 流量三项按原版抽屉概览写成三张大数字卡（27 px，外形同首页 KPI，fidelity #11）
  return `${stNote}
    ${card('当前状态', `<div class="dz">${statusRow}<div class="segment">${latencyStrip(n, { slot: 'beat' })}</div><div class="segment">${readings}</div><div class="segment" data-checks>${disclosure({ title: '查看检查详情', meta: `${n.status?.summary_counts ? Object.entries(n.status.summary_counts).map(([k, v]) => `${fmt.health(k).word} ${v}`).join(' · ') : ''}`, body: `<div data-checks-body>${skel('100%', 12)}</div>` })}</div></div>`, S('checked', s.checked))}
    <section class="kpi-grid is-3" aria-label="流量">
      ${kpiTile({ key: 'd-month', label: '本月业务流量', value: S('month', s.month), sub: S('monthSub', s.monthSub), action: { icon: 'chart-column', label: '流量明细', attrs: { 'data-dz': 'tab', 'data-tab': 'traffic' } } })}
      ${kpiTile({ key: 'd-24h', label: '近 24 小时', value: S('d24', s.d24), sub: S('d24Sub', s.d24Sub) })}
      ${kpiTile({ key: 'd-rate', label: '业务速率', value: S('rate', s.rate), sub: S('rateSub', s.rateSub) })}
    </section>`;
}

function checksTable(checks) {
  if (!checks?.length) return empty({ text: '没有检查项' });
  const rank = { fail: 0, degraded: 1, unknown: 2, na: 5, ok: 4 };
  const rows = checks.slice().sort((a, b) => (rank[a.status] ?? 3) - (rank[b.status] ?? 3));
  return `<div class="table-wrap"><table class="dt"><thead><tr><th>名称</th><th>状态</th><th>原因</th><th class="r">数值</th><th class="r">测量时间</th></tr></thead><tbody>${rows.map((c) => {
    const h = fmt.health(c.status);
    const val = c.value == null ? '<span class="t-muted">—</span>' : esc(typeof c.value === 'number' ? fmt.num(c.value, c.value % 1 ? 1 : 0) : String(c.value));
    return `<tr><td>${esc(c.title ?? fmt.checkTitle(c.key))}${c.critical ? ' ' + tagChip('关键') : ''}</td><td>${statusPill(h)}</td><td>${esc(fmt.reasonWord(c.reason) ?? '—')}</td><td class="r v">${val}</td><td class="r nowrap">${txt(fmt.ago(c.measured_at))}</td></tr>`;
  }).join('')}</tbody></table></div>`;
}

function quality(app, n) {
  const A = n.availability ?? {};
  const W = A.windows ?? {};
  const w = (k) => W[k] ?? {};
  const c = n.connection ?? {};
  const ob = n.outbound ?? {};
  const path = kv([
    { label: '入口', value: esc(M.entryText(app.store, n)) },
    { label: '协议', value: esc([c.inbound_protocol, c.security].filter(Boolean).join(' · ') || '—') },
    { label: 'Reality 目标 / SNI', value: esc(c.reality ? `${c.reality.dest} · ${c.reality.sni}` : '—') },
    { label: '转发到', value: `${esc(ob.protocol ? `${ob.protocol} ${ob.target ?? ''}${ob.target_port ? ':' + ob.target_port : ''}` : '—')}${ob.outbound_tag ? `<span class="kv-sub">${esc(ob.outbound_tag)}</span>` : ''}` },
    { label: '入口标签', value: esc(c.inbound_tag ?? '—') },
    { label: '客户端标识', value: esc(c.client_label ?? '—') },
    { label: '连接凭据', value: c.credential_prefix ? copyable(c.credential_prefix, { display: `${c.credential_prefix}…` }) : '<span class="t-muted">—</span>', tip: c.credential_prefix_note },
    { label: '来源地址', value: esc(c.source_ip_count != null ? `${c.source_ip_count} 个` : '—') },
  ], { cols: 3 }); // 抽屉里的明细统一 3 列，与上下两张卡的读数列对齐（craft #16）
  const winReading = (k, label) => kv([{ label, value: `${bigVal(fmt.pctAvail(w(k).availability_percent))}<span class="kv-sub">${esc(fmt.coverage(w(k).coverage_percent).text)}</span>` }], { cols: 1, big: true });
  return `${card('近 30 天成功率', `<div class="dz">
      <div class="dz-days">${dayTracker(A.tracker30)}<span class="dz-days-val">${bigVal(fmt.pctAvail(w('30').availability_percent))}</span></div>
      ${dayLegend()}
      <div class="segment"><div class="card-readings">${winReading('1', '1 天成功率')}${winReading('7', '7 天成功率')}${winReading('30', '30 天成功率')}</div></div>
    </div>`, infoButton('每天按监控评估（连接 + 主机检查）的成功采样占比；未监控的日子是灰格。监控从 09-15 开始。'))}
    ${card('连接路径', path)}
    ${egressCard(app.store, n)}
    ${card('延迟（24 小时）', `<div class="minis is-2">
      <div class="mini"><div class="mini-head"><span class="mini-title">${icon('radar', 13)}上游 TCP</span><span class="mini-value">${num(M.latencyF(n, 'tcp'))}</span></div><div class="mini-sub">线路机 → 落地 TCP 握手，5 分钟均值</div><div data-chart="lat-tcp"></div></div>
      <div class="mini"><div class="mini-head"><span class="mini-title">${icon('globe', 13)}出口验证</span><span class="mini-value">${num(M.latencyF(n, 'egress'))}</span></div><div class="mini-sub">经入口访问验证地址的 HTTP 耗时</div><div data-chart="lat-egress"></div></div>
    </div>`, '<span>TCP 与出口验证分两张图、两根轴</span>')}
    ${cardFlush('故障记录', `<div data-incidents>${`<div class="panel-body">${skel('100%', 12)}</div>`}</div>`)}`;
}

function incidentsTable(list) {
  if (!list?.length) return empty({ icon: 'circle-check', text: '近 30 天没有故障记录' });
  return `<div class="table-wrap"><table class="dt is-two"><thead><tr><th>开始 / 恢复</th><th>检查</th><th>状态</th><th>证据</th><th class="r">处理</th></tr></thead><tbody>${list.slice(0, 20).map((i) => {
    const w = fmt.incident(i);
    const ev = i.evidence ?? {};
    const evText = [i.reason_word ?? fmt.reasonWord(i.reason), ev.value != null ? `数值 ${ev.value}` : null, ev.actual_ip ? `实际 ${ev.actual_ip} · 期望 ${ev.expected_ip}` : null].filter(Boolean).join(' · ') || '—';
    return `<tr><td class="nowrap">${txt(fmt.stamp(i.opened_at))}<span class="sub">${i.resolved_at ? `${fmt.stamp(i.resolved_at).text} · ${fmt.duration(i.duration_s ?? (i.resolved_at - i.opened_at)).text}` : '未恢复'}</span></td><td>${esc(i.title ?? fmt.checkTitle(i.check))}</td><td>${tonePill(w.word, w.tone)}</td><td class="clip" data-tip="${esc(evText)}">${esc(evText)}</td><td class="r">${w.acked || i.ack_at ? '<span class="muted">已知悉</span>' : i.open ? button({ label: '标记已知悉', size: 'sm', attrs: { 'data-dz': 'ack', 'data-inc': i.id } }) : '<span class="t-muted">—</span>'}</td></tr>`;
  }).join('')}</tbody></table></div>`;
}

function traffic(app, n) {
  const T = n.traffic ?? {};
  const p = T.peak_24h;
  const peakRate = p ? fmt.rate(((p.down ?? 0) * 8) / (p.step ?? 300)) : fmt.missing();
  const readings = `<div class="card-readings">
    ${kv([{ label: '今日', value: `${bigVal(fmt.bytes((T.today?.down ?? 0) + (T.today?.up ?? 0)))}<span class="kv-sub">↓ ${num(fmt.bytes(T.today?.down))} · ↑ ${num(fmt.bytes(T.today?.up))}</span>` }], { cols: 1, big: true })}
    ${kv([{ label: '本月', value: `${bigVal(fmt.bytes((T.month?.down ?? 0) + (T.month?.up ?? 0)))}<span class="kv-sub">${T.month_partial ? `监控 ${esc(T.coverage_start ? fmt.date(T.coverage_start).text.slice(5) : '月中')} 起，本月不完整` : `↓ ${num(fmt.bytes(T.month?.down))} · ↑ ${num(fmt.bytes(T.month?.up))}`}</span>` }], { cols: 1, big: true })}
    ${kv([{ label: '近 24 小时峰值', value: `<span class="t-secondary">↓</span> ${bigVal(peakRate)}<span class="kv-sub">${p ? `${fmt.stamp(p.t).text} · 5 分钟均值` : '—'}</span>` }], { cols: 1, big: true })}
  </div>`;
  const legend = chartLegend([{ label: '↓ 下行', color: 'var(--chart-1)' }, { label: '↑ 上行', color: 'var(--chart-2)' }]);
  const chartCard = `<section class="dz-card"><div class="dz-head"><h3 class="dz-title">24 小时业务流量</h3><div class="dz-tools">${legend}${iconButton({ icon: 'table-2', label: '表格视图', size: 'sm', attrs: { 'data-dz': 'table', 'aria-pressed': 'false' } })}</div></div><div data-chart="traffic"></div><div class="chart-note" data-chart-note>&nbsp;</div></section>`;
  const nicCard = n.hasHost ? card('今日网卡', `<div data-today-nic>${skel('60%', 12)}</div>`, '<span>按 5 分钟采样</span>') : '';
  return `${card('', readings)}${chartCard}${nicCard}`;
}

function hostSlots(n) {
  const h = n.host ?? {};
  const x = M.nic(n);
  const pair = (u, t) => `${num(fmt.bytes(u))} / ${num(fmt.bytes(t))}`;
  return {
    state: M.probeState(n) === 'on' ? '在线' : `<span class="t-bad">${esc(M.offlineText(n) || '离线')}</span>`,
    mem: `${pair(h.mem_used, h.mem_total)} · ${num(fmt.pctResource(h.mem_pct, { precise: true }))}`,
    disk: `${pair(h.disk_used, h.disk_total)} · ${num(fmt.pctResource(h.disk_pct, { precise: true }))}`,
    load: esc(`${fmt.loadTriple(h.load1, h.load5, h.load15).text}（每核 ${fmt.pctLoad(h.load5_per_core_pct).text}）`),
    procs: num(fmt.int(h.processes)),
    conns: `${num(fmt.int(h.tcp_conns))} / ${num(fmt.int(h.udp_conns))}`,
    rx: num(x.rx), tx: num(x.tx), rxT: num(x.rxT), txT: num(x.txT),
    up: num(M.uptimeF(n)),
    rep: txt(fmt.ago(h.reported_at)),
  };
}

function host(app, n) {
  const h = n.host ?? {};
  const s = hostSlots(n);
  const sys = dlRows([
    { label: '状态', value: S('state', s.state) },
    { label: '系统', value: esc(h.os ?? '—'), tip: h.os, cls: 'is-long' },
    { label: '内核', value: esc(h.kernel ?? '—') },
    { label: '架构', value: esc(h.arch ?? '—') },
    { label: '虚拟化', value: esc(h.virtualization ?? '—') },
    { label: 'CPU', value: esc(h.cpu_model ? `${h.cpu_model} ×${h.cores}` : '—'), tip: h.cpu_model, cls: 'is-long' },
    { label: '显卡', value: esc(h.gpu ?? '—') },
    { label: 'agent', value: esc(n.agent?.version ?? '—') },
  ]);
  const res = dlRows([
    { label: '核数', value: num(fmt.int(h.cores)) },
    { label: '内存', value: S('mem', s.mem) },
    { label: 'Swap', value: h.swap_total ? `${num(fmt.bytes(h.swap_used))} / ${num(fmt.bytes(h.swap_total))}` : '<span class="t-muted">无</span>' },
    { label: '磁盘', value: S('disk', s.disk) },
    { label: '负载', value: S('load', s.load) },
    { label: '进程', value: S('procs', s.procs) },
    { label: 'TCP / UDP', value: S('conns', s.conns) },
  ]);
  const net = dlRows([
    { label: 'IPv4', value: n.ip && !n.ip.includes(':') ? copyable(n.ip) : '<span class="t-muted">—</span>' },
    { label: 'IPv6', value: n.ipv6 ? copyable(n.ipv6, { display: fmt.ip(n.ipv6).text }) : '<span class="t-muted">—</span>' },
    { label: '接收速率', value: S('rx', s.rx) },
    { label: '发送速率', value: S('tx', s.tx) },
    { label: '累计接收', value: S('rxT', s.rxT) },
    { label: '累计发送', value: S('txT', s.txT) },
    { label: '持续运行', value: S('up', s.up) },
    { label: '最近上报', value: S('rep', s.rep) },
  ]);
  const minis = [
    ['cpu', 'cpu', 'CPU'], ['mem', 'memory-stick', '内存'], ['disk', 'hard-drive', '磁盘'],
    ['load', 'gauge', '负载'], ['net', 'network', '网卡'], ['tcp', 'activity', 'TCP 连接'],
  ].map(([k, ic, label]) => `<div class="mini" data-mini="${k}"><div class="mini-head"><span class="mini-title">${icon(ic, 13)}${label}</span><span class="mini-value" data-mv="${k}">—</span></div><div class="mini-sub" data-ms="${k}">&nbsp;</div><div data-chart="h-${k}"><div class="skel" style="height:96px"></div></div></div>`).join('');
  // 时间范围与原版详情页同样 6 档；原型快照只有 24 小时的 5 分钟均值，没有数据的几档置灰并说明（fidelity #9）
  const range = segmented({ name: 'host-range', size: 'sm', items: HOST_RANGES, value: '4h', label: '时间范围' });
  const gaps = '<label class="switch-label" data-tip="缺测的时段直接连线（关闭时断开）"><input type="checkbox" class="switch" data-host-gaps>断点连线</label>';
  const ping = '<button type="button" class="btn btn-link btn-sm" data-dz="tab" data-tab="quality">延迟与丢包见“连接质量”</button>';
  return `${card(`${n.name} 主机信息`, `<div class="dz-groups">
      <div><div class="dz-group-title">系统</div>${sys}</div>
      <div><div class="dz-group-title">资源</div>${res}</div>
      <div><div class="dz-group-title">网络</div>${net}</div>
    </div>`, M.probeState(n) === 'off' ? '<span class="t-bad">节点当前离线，以下为最近一次上报的数据</span>' : '')}
    <section class="dz-card"><div class="dz-head"><h3 class="dz-title">负载图表</h3><div class="dz-tools">${gaps}${range}</div></div><div class="row-between host-cover"><span class="panel-sub" data-host-cover>&nbsp;</span>${ping}</div><div class="minis">${minis}</div></section>`;
}

const SRC = () => (isLive() ? '接口' : '原型快照');
const HOST_RANGES = [
  { value: 'live', label: '实时', disabled: true, get tip() { return `${SRC()}没有秒级实时数据`; } },
  { value: '1h', label: '1 小时' },
  { value: '4h', label: '4 小时' },
  { value: '24h', label: '1 天' },
  { value: '7d', label: '7 天', disabled: true, get tip() { return `${SRC()}只有 24 小时`; } },
  { value: '30d', label: '30 天', disabled: true, get tip() { return `${SRC()}只有 24 小时`; } },
];

function billing(app, n) {
  const b = n.billing ?? {};
  const p = planOf(n);
  const due = M.dueF(n);
  const same = membersOf(app.store, p.id).length;
  const planVal = `<b>${esc(p.name)}</b>${p.raw ? ' ' + mark('原名', { warn: true }) : ''}<span class="kv-sub">${esc(p.specs?.summary ?? '')}${same ? ` · 同套餐 ${same} 台` : ''}</span>`;
  const tzWord = (tz) => (!tz || tz === 'Asia/Shanghai' ? '北京' : fmt.tzBadge(tz));
  const src = b.source && /^https?:/.test(b.source) ? `<a class="btn btn-link btn-sm" href="${esc(b.source)}" target="_blank" rel="noopener noreferrer">打开供应商面板 ${icon('external-link', 14)}</a>` : esc(b.source ?? '—');
  const detail = `<dl class="kv" style="--kv-cols:3">
    <div class="kv-item is-wide"><dt>套餐</dt><dd>${planVal}</dd></div>
    <div class="kv-item"><dt>每月费用</dt><dd>${num(M.feeF(n))}</dd></div>
    <div class="kv-item"><dt>账期</dt><dd>${esc(b.cycle_word ?? '—')}</dd></div>
    <div class="kv-item"><dt>到期</dt><dd>${esc(b.due_date ?? '—')} · ${num(due, { title: false })}</dd></div>
    <div class="kv-item"><dt>账期时区</dt><dd>${mark(tzWord(b.billing_timezone), { title: b.billing_timezone })}</dd></div>
    <div class="kv-item"><dt>供应商时区</dt><dd>${esc(b.supplier_timezone ?? '—')}</dd></div>
    <div class="kv-item"><dt>服务号</dt><dd>${b.service_id ? copyable(String(b.service_id)) : '<span class="t-muted">—</span>'}</dd></div>
    <div class="kv-item"><dt>供应商后台</dt><dd>${src}</dd></div>
    <div class="kv-item"><dt>自动续费</dt><dd>${b.auto_renewal_supplier == null ? '<span class="t-muted">未设置</span>' : b.auto_renewal_supplier ? '已开启' : '未开启'}</dd></div>
    <div class="kv-item"><dt>提醒天数</dt><dd>${b.renewal_reminder_days != null ? `提前 ${esc(b.renewal_reminder_days)} 天` : '<span class="t-muted">未设置</span>'}</dd></div>
  </dl>`;
  return `${card('费用明细', detail)}
    ${cardFlush('续费记录', `<div data-renewals><div class="panel-body">${skel('100%', 12)}</div></div>`)}
    <div class="row row-end">${button({ label: '编辑资产', attrs: { 'data-dz': 'edit' } })}${button({ label: '登记续费', variant: 'primary', attrs: { 'data-dz': 'renew' } })}</div>`;
}

/** 最近连接卡：新访问记录（fleetd activity 增量）或情报到了只重画这一张卡，页签其余部分不动。 */
function patchRecent(app, n) {
  const box = document.querySelector('.drawer [data-recent]');
  if (box && box.dataset.recent === n.id) { box.innerHTML = recentCard(app, n, { noIntel: true }); hydrate(box); }
}
function recentCard(app, n, { noIntel = false } = {}) {
  const list = app.store.access(n.id).slice(0, 10);
  // 实时：线路机自身地址当场写成“线路机N（内部）”，其余 IP 的情报去新后台取（到了只重画这张卡）
  if (isLive() && !noIntel) ensureIntel(app.store, list.map((r) => r.source_ip), () => patchRecent(app, n));
  const table = list.length ? `<div class="table-wrap"><table class="dt is-two"><thead><tr><th>时间</th><th>来源 IP</th><th>目标</th><th>结果 ${infoButton('“已转发”表示 Xray 已把连接转给出站，不代表目标网站已响应。')}</th></tr></thead><tbody>${list.map((a) => {
    const w = fmt.accessAction(a.action);
    const x = intelOf(app.store, a.source_ip) ?? (a.source_intel ? toRecord(a.source_ip, a.source_intel) : a); // 来源画像与节点行为页同一种写法（ui/intel.js）
    return `<tr><td class="nowrap">${txt(fmt.ago(a.ts))}<span class="sub">${esc(fmt.stamp(a.ts, { seconds: true }).text)}</span></td><td>${sourceCell(a.source_ip, x, { relayName: (id) => app.store.node(id)?.name })}<span class="sub">${sourceLineHtml(x, { ip: a.source_ip }) || '—'}</span></td><td class="clip">${esc(a.host)}<span class="sub">${esc(`${String(a.protocol ?? '').toUpperCase()} :${a.port} · ${a.kind === 'ip' ? '仅 IP' : '域名'}`)}</span></td><td>${tonePill(w.word, w.tone ?? 'ok')}</td></tr>`;
  }).join('')}</tbody></table></div>` : empty({ text: '最近没有访问记录' });
  // 最近连接（原版内容）→ 来源画像（IP 情报汇总，24 小时 / 14 天）→ 来源 IP（逐个来源的情报与新来源确认状态）
  return cardFlush('最近连接', table, `<a class="btn btn-link btn-sm" href="#/behavior?asset=${esc(n.id)}">查看访问记录</a>`, `最近 ${list.length} 条`);
}
function access(app, n) {
  return `<div data-recent="${esc(n.id)}">${recentCard(app, n)}</div>${sourcesCard(app.store, n)}${sourcesTableCard(n)}`;
}

function manage(app, n) {
  const c = n.connection ?? {};
  const open = app.store.incidents({ open: true, node: n.id });
  const row = (name, desc, btn) => `<div class="dz-action"><div class="dz-action-body"><div class="dz-action-name">${esc(name)}</div><div class="dz-action-desc">${esc(desc)}</div></div>${btn}</div>`;
  const rows = [
    c.enable === false
      ? row('启用入口', `${M.entryText(app.store, n)} 当前已停用`, button({ label: '启用', size: 'sm', icon: 'play', attrs: { 'data-dz': 'enable' } }))
      // 管理页按钮同一种外形：描边小按钮、都带图标；危险操作只把字和描边换成红色（craft #17）
      : row('停用入口', `停用 ${M.entryText(app.store, n)}；该节点的用户会立即断开`, button({ label: '停用', size: 'sm', cls: 'is-danger', icon: 'power', attrs: { 'data-dz': 'disable' } })),
    row('重置累计用量', '把入口的累计上下行清零（不可撤销）', button({ label: '重置', size: 'sm', cls: 'is-danger', icon: 'rotate-ccw', attrs: { 'data-dz': 'reset' } })),
    row('编辑备注', '入口备注只给运维看，不影响连接', button({ label: '编辑', size: 'sm', icon: 'pencil', attrs: { 'data-dz': 'remark' } })),
    row('更换连接凭据', '生成新的 UUID；旧凭据立即失效，需要把新链接发给用户', button({ label: '更换', size: 'sm', cls: 'is-danger', icon: 'key-round', attrs: { 'data-dz': 'rotate' } })),
    row('重新验证链路', '立即跑一次 TCP 与出口验证', button({ label: '重新验证', size: 'sm', icon: 'refresh-cw', attrs: { 'data-dz': 'verify' } })),
    row('监控与维护', '四个阈值、维护截止时间', button({ label: '设置', size: 'sm', icon: 'settings-2', attrs: { 'data-dz': 'monitor' } })),
    open.length ? row('确认故障', `有 ${open.length} 条未确认的故障`, button({ label: '确认', size: 'sm', icon: 'check', attrs: { 'data-dz': 'ack-all' } })) : '',
  ];
  return card('管理', `<div class="dz-actions">${rows.join('')}</div>`);
}

// ─── 渲染后：懒加载数据、图表、交互 ─────────────────────────────────────────

function afterRender(app, n, tab, body, charts_) {
  const store = app.store;
  body.onclick = (e) => onAction(app, n, e);
  if (tab === 'overview') {
    store.detail(n.id).then((d) => { const el = body.querySelector('[data-checks-body]'); if (el) el.innerHTML = checksTable(d?.checks); })
      .catch(() => { const el = body.querySelector('[data-checks-body]'); if (el) el.innerHTML = note({ tone: 'bad', title: '检查详情不可用', body: 'detail.json 读取失败' }); });
  }
  if (tab === 'quality') {
    store.detail(n.id).then((d) => { const el = body.querySelector('[data-incidents]'); if (el) el.innerHTML = incidentsTable(d?.incidents); fillEgressIps(body, d, n.id); }).catch(() => {});
    store.series(n.id, 'latency_24h').then((s) => {
      if (!s || !body.isConnected) return;
      const xs = charts.xsOf(s.t0, s.step, s.tcp?.avg_ms?.length ?? 0);
      const mk = (key, ys, color) => { const el = body.querySelector(`[data-chart="${key}"]`); if (el) charts_.push(mountChart(el, { data: [xs, ys], series: [{ label: key === 'lat-tcp' ? '上游 TCP' : '出口验证', color, fill: true }], kind: 'ms', height: 120, syncKey: `lat-${n.id}` })); };
      mk('lat-tcp', s.tcp?.avg_ms ?? [], 'var(--chart-1)');
      mk('lat-egress', s.egress?.avg_ms ?? [], 'var(--chart-1)');
    }).catch(() => {});
  }
  if (tab === 'traffic') {
    store.series(n.id, 'traffic_5m').then((s) => {
      const el = body.querySelector('[data-chart="traffic"]');
      if (!s || !el) return;
      const data = charts.columnar(s, ['down', 'up'], { toRate: true });
      const peak = charts.peakOf(data[0], data[1]);
      const ctl = mountChart(el, { data, series: [{ label: '↓ 下行', color: 'var(--chart-1)', fill: true }, { label: '↑ 上行', color: 'var(--chart-2)' }], kind: 'rate', height: 160, markers: peak ? [{ x: peak.x, y: peak.y, label: `峰值 ${fmt.rate(peak.y).text}` }] : [] });
      charts_.push(ctl);
      const noteEl = body.querySelector('[data-chart-note]');
      const gaps = data[1].filter((v) => v == null).length * 5;
      if (noteEl) noteEl.textContent = `${fmt.stamp(data[0][0]).text} 起 · ↓ 峰值 ${peak ? `${fmt.rate(peak.y).text}（${fmt.stamp(peak.x).text}）` : '—'}${gaps ? ` · 缺测 ${gaps} 分钟` : ''}`;
      body.querySelector('[data-dz="table"]')?.addEventListener('click', async (e) => {
        const b = e.currentTarget; const c = await ctl;
        const on_ = b.getAttribute('aria-pressed') !== 'true';
        b.setAttribute('aria-pressed', String(on_));
        let t = el.nextElementSibling?.matches('.chart-table') ? el.nextElementSibling : null;
        if (on_ && !t) el.insertAdjacentHTML('afterend', chartTable(c.toTable()));
        else t?.remove();
        el.hidden = on_;
      });
    }).catch(() => { const el = body.querySelector('[data-chart="traffic"]'); if (el) el.innerHTML = note({ tone: 'bad', title: '流量序列不可用', body: '读取失败' }); });
    if (n.hasHost) todayNic(store, n).then((m) => { const el = body.querySelector('[data-today-nic]'); if (el) el.innerHTML = todayNicHtml(m); }).catch(() => {});
  }
  if (tab === 'host') mountHostCharts(app, n, body, charts_, '4h');
  if (tab === 'access') {
    bindIpintel(body, store, n);
    // 8 秒还没有来源数据（新后台慢或失败）：退回这个节点自己的访问记录，写明原因，可重试——不留无尽的骨架
    const t8 = setTimeout(() => { if (body.isConnected) sourcesTimeout(body, store, n, () => store.detail(n.id).then((d) => { if (body.isConnected) fillSourcesTable(body, store, n, d); })); }, 8000);
    store.detail(n.id).then((d) => { if (body.isConnected) { fillSourcesTable(body, store, n, d); if (d?.ipintel || n.ipintel) clearTimeout(t8); } })
      .catch(() => { const el = body.querySelector('[data-src-table] .panel-body'); if (el) el.innerHTML = note({ tone: 'bad', title: '来源 IP 不可用', body: 'detail.json 读取失败' }); });
  }
  if (tab === 'billing') {
    store.extra().then((x) => {
      const el = body.querySelector('[data-renewals]');
      if (!el) return;
      const list = (x.renewals ?? []).filter((r) => r.node === n.id);
      el.innerHTML = list.length ? `<div class="table-wrap"><table class="dt"><thead><tr><th>登记时间</th><th class="r">金额</th><th>原到期</th><th>新到期</th><th>备注</th></tr></thead><tbody>${list.map((r) => `<tr><td class="nowrap">${txt(fmt.stamp(r.ts))}</td><td class="r v">${esc(fmt.money(r.amount_cents, { currency: r.currency }).text)}</td><td>${esc(r.old_due ?? '—')}</td><td>${esc(r.new_due ?? '—')}</td><td class="clip" data-tip="${esc(r.note ?? '')}">${esc(r.note || '—')}</td></tr>`).join('')}</tbody></table></div>` : empty({ text: '没有续费记录' });
    }).catch(() => {});
  }
}

async function todayNic(store, n) {
  const s = await store.series(n.id, 'host_24h');
  if (!s) return null;
  const day0 = n.traffic?.day_start ?? (store.now() - (store.now() + 8 * 3600) % 86400);
  let rx = 0, tx = 0, peak = null, samples = 0;
  for (let i = 0; i < s.net_rx_bps.length; i++) {
    const t = s.t0 + i * s.step;
    if (t < day0) continue;
    const r = s.net_rx_bps[i], w = s.net_tx_bps[i];
    if (r == null && w == null) continue;
    samples++;
    rx += ((r ?? 0) * s.step) / 8; tx += ((w ?? 0) * s.step) / 8;
    const m = Math.max(s.net_rx_bps_max?.[i] ?? r ?? 0, s.net_tx_bps_max?.[i] ?? w ?? 0);
    if (!peak || m > peak.v) peak = { v: m, t };
  }
  return { rx, tx, peak, samples, last: s.t0 + (s.net_rx_bps.length - 1) * s.step };
}
function todayNicHtml(m) {
  if (!m || !m.samples) return empty({ text: '今日没有采样' });
  return `<div class="card-readings">
    ${kv([{ label: '接收', value: bigVal(fmt.bytes(m.rx)) }], { cols: 1, big: true })}
    ${kv([{ label: '发送', value: bigVal(fmt.bytes(m.tx)) }], { cols: 1, big: true })}
    ${kv([{ label: '采样峰值', value: `${bigVal(fmt.rate(m.peak?.v))}<span class="kv-sub">${m.peak ? fmt.clockTime(m.peak.t) : '—'}</span>` }], { cols: 1, big: true })}
  </div>`;
}
export { todayNic, todayNicHtml };

async function mountHostCharts(app, n, body, charts_, range) {
  const store = app.store;
  const off = onSegment(body, 'host-range', (v) => { off(); destroy(); mountHostCharts(app, n, body, charts_, v); });
  const destroy = () => { while (charts_.length) charts_.pop()?.then?.((c) => c?.destroy()); };
  let s;
  try { s = await store.series(n.id, 'host_24h'); } catch { s = null; }
  if (!body.isConnected) return;
  const cover = body.querySelector('[data-host-cover]');
  if (!s) {
    body.querySelectorAll('[data-chart^="h-"]').forEach((el) => { el.innerHTML = empty({ text: '24 小时没有负载数据' }); });
    return;
  }
  const N = s.cpu_pct.length;
  const from = range === '1h' ? Math.max(0, N - 12) : range === '4h' ? Math.max(0, N - 48) : 0;
  const sl = (a) => (a ?? []).slice(from);
  const xs = charts.xsOf(s.t0 + from * s.step, s.step, N - from);
  const lastOf = (a) => { for (let i = a.length - 1; i >= 0; i--) if (a[i] != null) return a[i]; return null; };
  const h = n.host ?? {};
  if (cover) cover.textContent = `${fmt.stamp(xs[0]).text} – ${fmt.clockTime(xs[xs.length - 1])} · ${xs.length} 个点 · 5 分钟均值`;
  const set = (k, v, sub) => { const a = body.querySelector(`[data-mv="${k}"]`), b = body.querySelector(`[data-ms="${k}"]`); if (a) a.innerHTML = v; if (b) b.innerHTML = sub || '&nbsp;'; };
  set('cpu', num(fmt.pctResource(h.cpu_pct, { precise: true })), `${esc(h.cores ?? '—')} 核`);
  set('mem', num(fmt.pctResource(h.mem_pct, { precise: true })), `${fmt.bytes(h.mem_used).text} / ${fmt.bytes(h.mem_total).text}`);
  set('disk', num(fmt.pctResource(h.disk_pct, { precise: true })), `${fmt.bytes(h.disk_used).text} / ${fmt.bytes(h.disk_total).text}`);
  set('load', esc(fmt.loadTriple(h.load1, h.load5, h.load15).text), `每核 ${fmt.pctLoad(h.load5_per_core_pct).text}`);
  // 网卡小图两条线：副行就是图例（色块 + 当前值，craft #37）；先画发送、再画接收，接收线在上面不被盖住
  set('net', `${num(fmt.rate(h.net_rx_bps))}`, `<span class="chart-legend"><span><i style="background:var(--chart-1)"></i>接收 ${esc(fmt.rate(h.net_rx_bps).text)}</span><span><i style="background:var(--chart-2)"></i>发送 ${esc(fmt.rate(h.net_tx_bps).text)}</span></span>`);
  set('tcp', num(fmt.int(h.tcp_conns)), `UDP ${fmt.int(h.udp_conns).text}`);
  const key = `host-${n.id}`;
  const mk = (k, spec) => { const el = body.querySelector(`[data-chart="h-${k}"]`); if (!el) return; el.innerHTML = ''; charts_.push(mountChart(el, { height: 120, syncKey: key, xTickSpace: 70, ...spec })); };
  mk('cpu', { data: [xs, sl(s.cpu_pct)], series: [{ label: 'CPU', color: 'var(--chart-1)', fill: true }], kind: 'pct' });
  mk('mem', { data: [xs, sl(s.mem_pct)], series: [{ label: '内存', color: 'var(--chart-1)', fill: true }], kind: 'pct' });
  mk('disk', { data: [xs, sl(s.disk_pct)], series: [{ label: '磁盘', color: 'var(--chart-1)', fill: true }], kind: 'pct' });
  mk('load', { data: [xs, sl(s.load1)], series: [{ label: '负载（1 分钟）', color: 'var(--chart-1)', fill: true, fmt: (v) => fmt.num(v, 2) }], kind: 'count' });
  mk('net', { data: [xs, sl(s.net_tx_bps), sl(s.net_rx_bps)], series: [{ label: '发送', color: 'var(--chart-2)' }, { label: '接收', color: 'var(--chart-1)', fill: true }], kind: 'rate' });
  mk('tcp', { data: [xs, sl(s.tcp)], series: [{ label: 'TCP 连接', color: 'var(--chart-1)', fill: true }], kind: 'count' });
  void lastOf;
  // 断点连线（原版详情页的开关）：缺测时段连起来还是断开
  const gapsBox = body.querySelector('[data-host-gaps]');
  const applyGaps = () => { for (const p of charts_) p?.then?.((c) => { if (!c?.uplot) return; c.uplot.series.forEach((x, i) => { if (i) x.spanGaps = !!gapsBox?.checked; }); c.uplot.redraw(true); }); };
  if (gapsBox) { gapsBox.onchange = applyGaps; if (gapsBox.checked) applyGaps(); }
}

// ─── 管理动作（原型：只演示确认分级与反馈，不改数据） ─────────────────────

async function onAction(app, n, e) {
  const b = e.target.closest('[data-dz]');
  if (!b) return;
  const k = b.dataset.dz;
  if (DRAWER_WRITES[k] && guard(DRAWER_WRITES[k], { node: n })) return;
  // 已接上旧后台的操作（写开关打开时）：真实请求，表单与文案照旧运营页（live-actions.js）
  if (DRAWER_WRITES[k] && isWired(DRAWER_WRITES[k])) {
    b.setAttribute('aria-busy', 'true');
    try { await drawerAction(app, n, k, b); } finally { if (b.isConnected) b.removeAttribute('aria-busy'); }
    return;
  }
  const entry = M.entryText(app.store, n);
  if (k === 'tab') { app.setDrawerParams(n.id, b.dataset.tab); return; }
  if (k === 'disable') {
    const ok = await confirmAction({ title: `停用入口 ${entry}`, sub: n.name, body: '该节点的用户会立即断开，出口 IP 不变。可以随时重新启用。', impact: `影响 1 个入口；当前 ${n.rate?.tcp_connections ?? 0} 个连接会断开。`, confirm: '停用', danger: true });
    if (ok) toast({ message: `已停用 ${entry}（原型演示，未改数据）`, action: { label: '撤销', run: () => toast({ message: '已撤销', tone: 'info' }) } });
  } else if (k === 'enable') {
    toast({ message: `已启用 ${entry}（原型演示）`, action: { label: '撤销', run: () => {} } });
  } else if (k === 'reset') {
    const ok = await confirmAction({ title: `重置累计用量 ${entry}`, sub: n.name, body: '入口累计上下行会清零，无法恢复。本月业务流量统计不受影响。', confirm: '重置', danger: true, typeToConfirm: n.name });
    if (ok) toast({ message: `已重置 ${entry} 的累计用量（原型演示）` });
  } else if (k === 'rotate') {
    const ok = await confirmAction({ title: '更换连接凭据', sub: `${n.name} · ${entry} · UUID`, body: '旧凭据立即失效，用户需要换成新链接才能连上。', impact: '更换后请把新链接发给这个节点的用户。', confirm: '更换', danger: true, typeToConfirm: n.name });
    if (ok) toast({ message: '已更换连接凭据 · 已记录审计（原型演示）' });
  } else if (k === 'verify') {
    b.setAttribute('aria-busy', 'true');
    const label = b.querySelector('span'); const prev = label.textContent; label.textContent = '进行中';
    b.querySelector('.lucide')?.classList.add('spin');
    setTimeout(() => { b.removeAttribute('aria-busy'); label.textContent = prev; b.querySelector('.lucide')?.classList.remove('spin'); toast({ message: `${n.name} 链路验证完成：TCP ${fmt.latency(n.latency?.tcp_ms).text} · 出口 ${fmt.latency(n.latency?.egress_ms).text}` }); }, 1400);
  } else if (k === 'remark' || k === 'edit' || k === 'renew' || k === 'monitor') {
    const titles = { remark: ['编辑备注', entry], edit: ['编辑资产', n.name], renew: ['登记续费', n.name], monitor: ['监控与维护', n.name] };
    const [t, sub] = titles[k];
    const L = n.limits ?? {};
    const body = k === 'monitor'
      ? `<div class="fields">${[['CPU 阈值', L.cpu_percent ?? 90, '%'], ['内存阈值', L.memory_percent ?? 90, '%'], ['磁盘阈值', L.disk_percent ?? 85, '%'], ['出口验证阈值', L.latency_ms ?? 3000, 'ms']].map(([l, v]) => `<label class="field"><span class="field-label">${l}</span><input class="input" value="${esc(v)}" inputmode="numeric"></label>`).join('')}<label class="field is-wide"><span class="field-label">维护截止（选填）</span><input class="input" placeholder="例如 09-24 02:00"></label></div>`
      : k === 'renew'
        ? `<div class="fields"><label class="field"><span class="field-label">金额（USD）</span><input class="input" value="${esc((n.billing?.price_cents ?? 0) / 100)}"></label><label class="field"><span class="field-label">新到期</span><input class="input" value="${esc(n.billing?.due_date ?? '')}"></label><label class="field is-wide"><span class="field-label">备注（选填）</span><textarea class="textarea"></textarea></label></div>`
        : `<label class="field"><span class="field-label">${k === 'remark' ? '备注' : '说明'}</span><textarea class="textarea" placeholder="原型演示，不保存"></textarea></label>`;
    openDialog({ title: t, sub, guard: true, size: k === 'monitor' || k === 'renew' ? 'md' : 'sm', body, foot: button({ label: '取消', attrs: { 'data-dlg': 'cancel' } }) + button({ label: k === 'renew' ? '确认登记续费' : '保存', variant: 'primary', attrs: { 'data-dlg': 'save' } }), init(el, api) { el.querySelector('[data-dlg="save"]').addEventListener('click', () => { api.close(); toast({ message: '已保存（原型演示，未改数据）' }); }); } });
  } else if (k === 'ack' || k === 'ack-all') {
    b.outerHTML = '<span class="muted">已知悉</span>';
    toast({ message: '已标记为已知悉', action: { label: '撤销', run: () => {} } });
  }
}

// ─── 实时补丁 ───────────────────────────────────────────────────────────────

function patchLive(body, n, tab, store) {
  let vals = null;
  if (tab === 'overview') { vals = overviewSlots(store, n); patchBeat(body.querySelector('[data-s="beat"]'), n); }
  else if (tab === 'host') vals = hostSlots(n);
  if (!vals) return;
  for (const [k, v] of Object.entries(vals)) {
    if (!patcher.changed(`${n.id}:${tab}:${k}`, v)) continue;
    const el = body.querySelector(`[data-s="${k}"]`);
    if (el && el.innerHTML !== v) el.innerHTML = v;
  }
}
