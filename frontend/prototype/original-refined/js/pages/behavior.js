// pages/behavior.js — 节点行为 #/behavior（SPEC §6.5）。版面与原版 /admin/behavior 一致（renders/ops-behavior*.png）：
//   页头（节点行为 + 鲜度 + 宽松/紧凑 + 导出 + 更多 ▾ + ↻）→ 页面分段 [节点行为 │ 访问规则]
//   节点行为：筛选条（团队 · 节点 · 时间范围 · 搜索 · 目标类型 · 处理结果）→ 选择上下文（选了节点时）
//            → 4 张 KPI（有活动的节点 · 连接记录 · 访问目标 · 已阻止的连接）
//            → 面板 + 下划线页签 [域名与 IP │ 行为变化 ● │ 来源地址 │ 节点概览] + 表 + 表尾分页
//   访问规则：两栏（选择管理范围 │ 规则编辑）+ 规则发布记录（本轮只做外形与本地草稿）。
// 精修（SPEC §6.5、§4）：筛选即时生效（去掉“查询”，dna-ops I4）；“更多筛选”直接放进筛选条（≤900 收进“筛选 ▾”）；
//   KPI 外形照首页；“行为变化”从卡片列表改成表格；来源 IP 带运营商 / 城市 / 接入方式画像（ipintel）；
//   未读圆点不统计“新域名”（§9 Q5）；术语“已拦截”→“已阻止”（T-05）；时间一律北京时间。
// URL（§5.6）：mode=rules、team、asset（节点筛选）、range、q、type、result、view、page、sort、ek（事件类型）、scope（规则范围）。
//   注意：外壳把 node / tab 留给节点抽屉（node=t1-01&tab=access），所以节点筛选用 asset、页签用 view，互不冲突。

import { fmt } from '../../../shared/index.js';
import {
  icon, esc, num, on,
  pageHead, watchStuck, freshness, segmented, onSegment, setSegmented, hydrate, tabs,
  kpiTile, kpiValue, patchKpi, button, iconButton, statusPill, tonePill, tagChip, neutralChip, copyable, mark,
  panel, note, empty, dataTable, patchTable, tableSkeleton,
  openMenu, openDialog, confirmAction, toast,
  select, onSelect, searchBox, onSearch, setSearch,
  intelOf, sourceCell, sourceLineHtml,
} from '../components.js';
import * as M from '../node-model.js';
import { HEAD_TOOLS } from '../page-tools.js';
import { isLive, guard, isWired } from '../live-mode.js';
import { behaviorAck } from '../live-actions.js';
import { onFleet, legacyGet as legacyGetLive } from '../fleet.js';
import * as POL from '../live-policies.js';
import { ensureIntel, toRecord } from '../live-intel.js';

// 访问记录：原版访问记录页已下线 → 节点抽屉的访问页签（没选节点时给说明）
let appRef = null;
const openActivity = (node) => {
  if (node && appRef) appRef.openNode(node.id, 'access');
  else toast({ message: '逐条访问记录在节点详情的“访问”页签：先选一个节点', tone: 'info' });
};

// ─── 常量 ────────────────────────────────────────────────────────────────────

const MODES = [{ value: 'behavior', label: '节点行为' }, { value: 'rules', label: '访问规则' }];
const RANGES = [{ value: '1h', label: '1 小时' }, { value: '24h', label: '24 小时' }, { value: '7d', label: '7 天' }, { value: '14d', label: '14 天' }];
const RANGE_WORD = { '1h': '最近 1 小时', '24h': '最近 24 小时', '7d': '最近 7 天', '14d': '最近 14 天' };
const KINDS = [{ value: '', label: '全部目标' }, { value: 'domain', label: '仅域名' }, { value: 'ip', label: '仅 IP' }];
const RESULTS = [{ value: '', label: '全部结果' }, { value: 'blocked', label: '已阻止' }, { value: 'routed', label: '已转发' }];
const VIEWS = [{ value: 'targets', label: '域名与 IP' }, { value: 'events', label: '行为变化' }, { value: 'sources', label: '来源地址' }, { value: 'nodes', label: '节点概览' }];
const EVENT_KINDS = [{ value: '', label: '全部' }, { value: 'new_source', label: '新来源' }, { value: 'connection_spike', label: '连接突增' }, { value: 'blocked_rise', label: '阻止增多' }, { value: 'new_domain', label: '新域名' }];
const EVENT_WORD = Object.fromEntries(EVENT_KINDS.slice(1).map((k) => [k.value, k.label]));
const SORT_DEFAULT = { targets: '-connections', sources: '-connections', nodes: '-last', events: '-ts' };
const PAGE_SIZE = 100; // 原版服务端分页：每页 100 条（§3.10）
const RULE_MODES = [
  { value: 'observe', label: '观察模式' }, { value: 'allow', label: '域名白名单' }, { value: 'deny', label: '域名黑名单' },
  { value: 'pause', label: '暂停新连接' }, { value: 'inherit', label: '继承团队规则' },
];
const RULE_WORD = Object.fromEntries(RULE_MODES.map((m) => [m.value, m.label]));

// 访问规则草稿（原型只存在内存里；生产环境是服务端草稿 + revision）
const drafts = new Map();

const dash = '<span class="t-muted">—</span>';
const pick = (list, v, def) => (list.some((x) => x.value === v) ? v : def);

function readState(p) {
  return {
    mode: pick(MODES, p.get('mode'), 'behavior'),
    team: p.get('team') ?? '',
    asset: p.get('asset') ?? '',
    range: pick(RANGES, p.get('range'), '24h'),
    q: (p.get('q') ?? '').trim(),
    kind: pick(KINDS, p.get('type') ?? '', ''),
    result: pick(RESULTS, p.get('result') ?? '', ''),
    view: pick(VIEWS, p.get('view'), 'targets'),
    page: Math.max(1, parseInt(p.get('page') ?? '1', 10) || 1),
    sort: p.get('sort') ?? '',
    ek: pick(EVENT_KINDS, p.get('ek') ?? '', ''),
    scope: p.get('scope') ?? '',
  };
}
function writeState(st) {
  return {
    mode: st.mode === 'behavior' ? null : st.mode,
    team: st.team || null,
    asset: st.asset || null,
    range: st.range === '24h' ? null : st.range,
    q: st.q || null,
    type: st.kind || null,
    result: st.result || null,
    view: st.view === 'targets' ? null : st.view,
    page: st.page > 1 ? String(st.page) : null,
    sort: st.sort && st.sort !== SORT_DEFAULT[st.view] ? st.sort : null,
    ek: st.ek || null,
    scope: st.mode === 'rules' && st.scope ? st.scope : null,
  };
}

export default {
  title: '节点行为',
  density: true,
  mount({ el, app, params }) {
    appRef = app;
    const store = app.store;
    const B = store.behavior();                 // snapshot：KPI 与 24 小时 top（首屏即有）
    let st = readState(params);
    let ex = null, exError = null;               // extra.json 的 behavior（域名与节点 × 目标、事件、时间线）
    const nodeSources = new Map();               // asset → 该节点的来源行（detail.json，选了节点时按需加载）
    const unacked = Object.fromEntries((B.top?.events_counts ?? []).map((c) => [c.kind, c.unacked]));
    const totals = Object.fromEntries((B.top?.events_counts ?? []).map((c) => [c.kind, c.total]));
    let order = [];                              // 当前表的行 id 顺序（j/k、抽屉 [ ]）
    let drawerId = null;

    // ─── 节点名称与筛选 ───
    const nodeOf = (id) => store.node(id);
    const relayName = (id) => store.node(id)?.name ?? id;
    function nameOf(id) {
      if (id?.startsWith('direct:')) { const [, relay, port] = id.split(':'); return `${relay}:${port}`; }
      const n = nodeOf(id);
      if (!n) return id ?? '—';
      return n.isRelay ? `${n.name} 主节点` : n.name;
    }
    const teamOf = (id) => nodeOf(id)?.team ?? (id?.startsWith('direct:') ? '未分配直连' : '');
    const matchNode = (id) => (st.asset ? id === st.asset : st.team ? teamOf(id) === st.team : true);
    const hasFilters = () => !!(st.team || st.asset || st.q || st.kind || st.result || st.range !== '24h');
    const nodeFiltered = () => !!(st.team || st.asset);
    const qMatch = (...xs) => !st.q || xs.some((x) => x != null && String(x).toLowerCase().includes(st.q.toLowerCase()));

    // ─── 单元格小件 ───
    /** 节点名（点击 = 只看这个节点的行为，原 ◎）。线路机主入口：中性 chip“主节点”。 */
    function nodeLine(id, { strong = false } = {}) {
      const n = nodeOf(id);
      if (!n) return `<span>${esc(nameOf(id))}</span>`;
      const label = n.isRelay ? neutralChip('主节点') : `<span class="ilink-text${strong ? ' strong' : ''}">${esc(n.name)}</span>`;
      return `<button type="button" class="ilink" data-focus="${esc(id)}" data-tip="只看 ${esc(nameOf(id))} 的行为">${label}${icon('scan-search', 13)}</button>`;
    }
    const nodeTeamSub = (id) => { const n = nodeOf(id); return n?.isRelay ? esc(M.entryText(store, n).replace('主节点 ', `${n.name} `)) : esc(teamOf(id) || '—'); };
    const nodeIpSub = (id) => { const n = nodeOf(id); if (!n) return ''; return esc([n.ip, n.isRelay ? M.entryText(store, n).replace('主节点 ', `${n.name} `) : n.relay].filter(Boolean).join(' · ')); };
    const count = (v, unit = '') => `<span class="v">${num(fmt.int(v, unit))}</span>`;
    const agoHtml = (t, tip = '') => (t ? `<span class="nowrap" data-ago="${esc(t)}" data-tip="${esc([fmt.absolute(t), tip].filter(Boolean).join('\n'))}">${esc(fmt.ago(t).text)}</span>` : dash);
    function resultPills(r) {
      const blocked = r.blocked ?? 0;
      const routed = Math.max(0, (r.connections ?? 0) - blocked);
      const out = [];
      if (routed > 0 || !blocked) out.push(statusPill({ word: '已转发', tone: 'ok', dot: 'solid' }, { count: routed }));
      if (blocked) out.push(statusPill({ word: '已阻止', tone: 'bad', dot: 'solid' }, { count: blocked }));
      return `<span class="chip-wrap">${out.join('')}</span>`;
    }
    const intel = (ip, row) => {
      // 新后台的行上直接带 source_intel（fleetd 0.9.7 起）；否则查已取到的情报；都没有时交给 ensureIntel 去取
      const full = intelOf(store, ip) ?? (row?.source_intel ? toRecord(ip, row.source_intel) : null);
      return full ? { ...row, ...full } : row?.carrier ? row : null;
    };

    // ─── 排序（本地；数值列都可排序，写进 URL sort=-connections） ───
    const sortSpec = () => { const s = st.sort || SORT_DEFAULT[st.view]; return { key: s.replace(/^-/, ''), dir: s.startsWith('-') ? -1 : 1 }; };
    const sortOf = (key) => { const s = sortSpec(); if (s.key !== key) return { dir: null, ariaSort: 'none' }; return { dir: s.dir === 1 ? 'asc' : 'desc', ariaSort: s.dir === 1 ? 'ascending' : 'descending' }; };
    function sortRows(rows, getters) {
      const { key, dir } = sortSpec();
      const g = getters[key];
      if (!g) return rows;
      return rows.slice().sort((a, b) => {
        const va = g(a), vb = g(b);
        if (va == null && vb == null) return 0;
        if (va == null) return 1;
        if (vb == null) return -1;
        return (va < vb ? -1 : va > vb ? 1 : 0) * dir;
      });
    }

    // ─── 数据：四个页签的行 ───
    function targetRows() {
      if (!ex) return [];
      const rows = ex.top_targets_by_node.filter((r) => matchNode(r.node)
        && (!st.kind || r.kind === st.kind)
        && (!st.result || (st.result === 'blocked' ? r.blocked > 0 : r.connections - r.blocked > 0))
        && qMatch(r.host, nameOf(r.node)));
      return sortRows(rows, { sources: (r) => r.sources, connections: (r) => r.connections, last: (r) => r.last_seen })
        .map((r) => ({ ...r, id: `${r.node}|${r.host}` }));
    }
    function sourceRows() {
      if (st.asset) {
        const list = nodeSources.get(st.asset);
        if (!list) return null; // 加载中
        const rows = list.filter((s) => qMatch(s.source_ip, s.carrier, s.city, s.province));
        return sortRows(rows, { connections: (r) => r.connections, domains: (r) => r.domains, last: (r) => r.last_seen });
      }
      if (!ex) return [];
      const rows = ex.top_sources.map((s) => ({ ...s, id: s.source_ip, x: intel(s.source_ip, s) }))
        .filter((s) => (!st.team || (s.x?.teams ?? []).includes(st.team)) && qMatch(s.source_ip, s.carrier, s.city, s.province));
      return sortRows(rows, { connections: (r) => r.connections, domains: (r) => r.domains, last: (r) => r.last_seen });
    }
    function eventRows(withKind = true) {
      if (!ex) return [];
      return ex.events_recent
        .filter((e) => matchNode(e.node) && (!withKind || !st.ek || e.kind === st.ek) && qMatch(e.subject, nameOf(e.node), e.message, e.subject_intel?.carrier, e.subject_intel?.city))
        .slice().sort((a, b) => b.ts - a.ts);
    }
    // 节点概览用节点自身的 behavior（live.js 会实时累加连接数与最近活动），不用 extra.json 的静态副本
    const nodeRow = (id) => { const n = nodeOf(id); return n?.behavior ? { id, ...n.behavior, egress: n.latency?.egress_ms ?? null } : null; };
    function nodeRows() {
      const rows = store.nodes().filter((n) => n.behavior?.connections).map((n) => nodeRow(n.id))
        .filter((r) => matchNode(r.id) && qMatch(nameOf(r.id), nodeOf(r.id)?.ip) && (!st.result || st.result !== 'blocked' || r.blocked > 0));
      return sortRows(rows, { connections: (r) => r.connections, domains: (r) => r.domains, sources: (r) => r.sources, egress: (r) => r.egress, last: (r) => r.last_seen });
    }

    // ─── KPI（按团队 / 节点筛选；无筛选时用服务端汇总，与原版一致） ───
    // 无筛选时用服务端汇总（含未归属的直连入口）；实时部分按各节点连接数的增量累加上去
    const sumConn = () => store.nodes().reduce((a, n) => a + (n.behavior?.connections ?? 0), 0);
    const conn0 = sumConn();
    function kpiModel() {
      const all = store.nodes();
      let active, conn, targets, blocked, scope;
      const blockedBy = [];
      if (!nodeFiltered()) {
        const k = B.kpis;
        active = k.active_nodes; conn = k.connections + (sumConn() - conn0); targets = k.targets_sum_by_node; blocked = k.blocked; scope = all.length;
        for (const n of all) if (n.behavior?.blocked) blockedBy.push([nameOf(n.id), n.behavior.blocked]);
      } else {
        const list = all.filter((n) => matchNode(n.id));
        scope = list.length; active = 0; conn = 0; targets = 0; blocked = 0;
        for (const n of list) {
          const b = n.behavior ?? {};
          if (b.connections) active++;
          conn += b.connections ?? 0; targets += b.domains ?? 0; blocked += b.blocked ?? 0;
          if (b.blocked) blockedBy.push([nameOf(n.id), b.blocked]);
        }
      }
      const snap = st.range === '24h' ? RANGE_WORD['24h'] : '最近 24 小时（快照）';
      const bSub = blocked
        ? blockedBy.slice(0, 2).map(([w, c]) => `${esc(w)} <span class="num">${esc(fmt.num(c))}</span>`).join(' · ') + (blockedBy.length > 2 ? ` 等 ${num(fmt.int(blockedBy.length, '台'))}` : '')
        : `${snap}没有`;
      return {
        // 计数一律“N 台”、数字用 Inter（consistency #9、#12）
        active: { value: kpiValue(fmt.num(active)), sub: `${nodeFiltered() ? '筛选内' : ''}共 ${num(fmt.int(scope, '台'))}` },
        conn: { value: kpiValue(fmt.num(conn)), sub: snap },
        targets: { value: kpiValue(fmt.num(targets)), sub: '按节点去重后求和' },
        blocked: { value: kpiValue(fmt.num(blocked)), sub: bSub },
      };
    }
    function kpisHtml() {
      const m = kpiModel();
      const f = nodeFiltered();
      return [
        // 在本页筛选的 KPI 整卡可点（与节点页、流水线同一种，consistency #13）：有活动的节点 → 节点概览页签；已阻止 → 只看已阻止
        kpiTile({ key: 'active', label: '有活动的节点', filtered: f, clickable: true, pressed: st.view === 'nodes', info: '所选时间范围内至少有一条入口连接的节点数（含线路机主入口）。点卡片看“节点概览”页签。', ...m.active }),
        kpiTile({ key: 'conn', label: '连接记录', filtered: f, info: 'Xray access log 里一条连接记一行；不是字节，也不等于用户数或页面浏览量。', ...m.conn }),
        kpiTile({ key: 'targets', label: '访问目标', filtered: f, info: '各节点分别去重后相加：同一个域名被 3 个节点访问会算 3 次，所以比全机队去重数大。全机队去重需要服务端另算（brief K4）。', ...m.targets }),
        kpiTile({ key: 'blocked', label: '已阻止的连接', filtered: f, clickable: true, pressed: st.result === 'blocked', info: '访问规则或 Xray 内置规则丢弃的连接：私网地址与 BT 流量由 Xray 内置规则丢弃。点卡片只看已阻止的连接，再点取消。', ...m.blocked }),
      ].join('');
    }

    // ─── 页签（计数随筛选变化；“行为变化”只有未读圆点） ───
    const unread = () => Object.entries(unacked).some(([k, n]) => k !== 'new_domain' && n > 0);
    function tabsHtml() {
      const srcRows = ex || st.asset ? sourceRows() : null;
      const counts = ex ? { targets: targetRows().length, sources: srcRows?.length ?? null, nodes: nodeRows().length } : {};
      return tabs({ name: 'view', label: '节点行为视图', value: st.view, items: VIEWS.map((v) => ({ ...v, count: counts[v.value] ?? undefined, unread: v.value === 'events' && unread() })) });
    }

    // ─── 表：列定义 ───
    function tableSpec() {
      if (st.view === 'targets') {
        return {
          label: '域名与 IP',
          rows: targetRows(),
          columns: [
            { key: 'host', label: '目标', width: '30%', cls: 'clip', cell: (r) => `<span class="strong">${copyable(r.host)}</span>`, sub: (r) => (r.kind === 'ip' ? '仅 IP' : '域名') },
            { key: 'node', label: '节点', cell: (r) => nodeLine(r.node), sub: (r) => nodeTeamSub(r.node) },
            { key: 'sources', label: '来源', align: 'right', sortable: true, cell: (r) => count(r.sources, '个来源') },
            { key: 'connections', label: '连接数', align: 'right', sortable: true, cell: (r) => `<span data-tip="${esc(`${RANGE_WORD['24h']} ${fmt.num(r.connections)} 条 · 累计记录 ${fmt.num(r.total_recorded)} 条`)}">${count(r.connections)}</span>` },
            { key: 'result', label: '处理结果', cell: (r) => resultPills(r) },
            { key: 'last', label: '最近连接', sortable: true, cell: (r) => agoHtml(r.last_seen, r.first_seen ? `首次 ${fmt.absolute(r.first_seen)}` : '') },
          ],
          empty: emptyFor('targets'),
        };
      }
      if (st.view === 'sources') {
        const rows = sourceRows();
        const perNode = !!st.asset;
        return {
          label: '来源地址',
          rows: rows ?? [],
          loading: rows == null,
          // 选了节点：数值是这个节点上的连接（detail.json）；“节点”列换成客户端标识，按节点拆分的目标数没有采集，不放一列 —
          columns: [
            { key: 'ip', label: '来源 IP', width: '34%', cell: (r) => sourceCell(r.source_ip, r.x, { relayName }), sub: (r) => sourceLineHtml(r.x, { ip: r.source_ip }) || '<span class="t-muted">—</span>' },
            perNode
              ? { key: 'client', label: '客户端标识', cell: (r) => esc(r.clients?.[0] ?? '未标记客户端'), sub: (r) => esc(r.clients?.length > 1 ? `另有 ${r.clients.slice(1).join('、')}` : `${nameOf(st.asset)} 上`) }
              : { key: 'nodes', label: '节点', cell: (r) => sourceNodes(r), sub: (r) => esc((r.x?.teams ?? []).join(' · ') || '—') },
            { key: 'connections', label: '连接', align: 'right', sortable: true, cell: (r) => count(r.connections), sub: (r) => (r.sessions_h24 != null ? esc(`${fmt.num(r.sessions_h24)} 段会话`) : '') },
            ...(perNode ? [] : [{ key: 'domains', label: '目标', align: 'right', sortable: true, cell: (r) => count(r.domains) }]),
            { key: 'last', label: '最近连接', sortable: true, cell: (r) => agoHtml(r.last_seen), sub: (r) => (r.first_seen ? esc(`首次 ${fmt.stamp(r.first_seen).text}`) : '') },
          ],
          empty: emptyFor('sources'),
        };
      }
      if (st.view === 'events') {
        return {
          label: '行为变化',
          rows: eventRows(),
          columns: [
            { key: 'ts', label: '时间', width: 120, cell: (r) => agoHtml(r.ts), sub: (r) => esc(fmt.stamp(r.ts, { seconds: true }).text) },
            { key: 'node', label: '节点', cell: (r) => nodeLine(r.node), sub: (r) => nodeTeamSub(r.node) },
            { key: 'kind', label: '类型', cell: (r) => tagChip(EVENT_WORD[r.kind] ?? r.kind) },
            { key: 'subject', label: '对象', cell: (r) => subjectCell(r), sub: (r) => subjectSub(r) },
            { key: 'message', label: '说明', cls: 'clip', cell: (r) => { const m = groupDigits(r.message); return `<span data-tip="${esc(m)}">${esc(m)}</span>`; } },
            { key: 'ack', label: '处理', cell: (r) => ackCell(r) },
          ],
          empty: emptyFor('events'),
        };
      }
      return {
        label: '节点概览',
        rows: nodeRows(),
        columns: [
          { key: 'name', label: '节点', cell: (r) => nodeLine(r.id, { strong: true }), sub: (r) => nodeIpSub(r.id) },
          { key: 'connections', label: '连接', align: 'right', sortable: true, cell: (r) => count(r.connections), sub: (r) => (r.blocked ? `<span class="t-bad">已阻止 ${esc(fmt.num(r.blocked))}</span>` : '') },
          { key: 'domains', label: '域名', align: 'right', sortable: true, cell: (r) => count(r.domains), sub: (r) => (r.ip_only ? esc(`仅 IP ${fmt.num(r.ip_only)}`) : '') },
          { key: 'sources', label: '来源', align: 'right', sortable: true, cell: (r) => count(r.sources) },
          { key: 'egress', label: '出口验证', align: 'right', sortable: true, cell: (r) => egressCell(r.id) },
          { key: 'last', label: '最近活动', sortable: true, cell: (r) => agoHtml(r.last_seen) },
        ],
        empty: emptyFor('nodes'),
      };
    }
    function sourceNodes(r) {
      if (st.asset) return nodeLine(st.asset);
      const x = r.x;
      const n = x?.nodes?.h24 ?? null;
      const ids = x?.node_ids ?? [];
      if (n === 1 && ids.length === 1) return nodeLine(ids[0]);
      if (n == null) return dash;
      const names = ids.slice(0, 16).map(nameOf).join('、') + (ids.length > 16 ? ` 等 ${ids.length} 台` : '');
      return `<span class="v" data-tip="${esc(`${RANGE_WORD['24h']}接入 ${n} 台（14 天 ${x.nodes.d14} 台）\n${names}`)}">${num(fmt.int(n, '台'))}</span>`;
    }
    function egressCell(id) {
      const n = nodeOf(id);
      const ms = n?.latency?.egress_ms;
      if (n && M.probeFail(n, 'egress')) return `<span class="v" data-egress="${esc(id)}">${num(M.latencyF(n, 'egress'))}</span>`;
      if (ms == null) return `<span class="v" data-egress="${esc(id)}">${dash}</span>`;
      const tone = fmt.latencyTone(ms, 'egress', n.limits ?? {});
      const cls = tone === 'warn' ? ' t-warn' : tone === 'bad' ? ' t-bad' : '';
      return `<span class="v${cls}" data-egress="${esc(id)}" data-tip="${esc(`出口验证 ${n.latency?.egress_at ? fmt.ago(n.latency.egress_at).text : ''}`)}">${num(fmt.latency(ms))}</span>`;
    }
    /** 说明文字里的 4 位以上数字加千分位（“1051 次”→“1,051 次”，与对象列一致，consistency #20）。 */
    const groupDigits = (msg) => String(msg ?? '').replace(/\d{4,}/g, (d) => fmt.num(Number(d)));
    function spikeNums(msg) {
      const a = /(\d+)\s*次入口连接/.exec(msg ?? ''), b = /中位数\s*(\d+)/.exec(msg ?? ''), w = /(\d+)\s*分钟内/.exec(msg ?? '');
      return { n: a ? Number(a[1]) : null, med: b ? Number(b[1]) : null, win: w ? Number(w[1]) : 5 };
    }
    function subjectCell(r) {
      if (r.kind === 'new_source') return sourceCell(r.subject, intel(r.subject, r.subject_intel), { relayName, isNew: false }); // 类型列已写“新来源”，这里不再重复标记
      if (r.kind === 'connection_spike') { const s = spikeNums(r.message); return s.n != null ? `<span class="v">${num(fmt.int(s.n, `次 / ${s.win} 分钟`))}</span>` : '入口连接'; }
      return `<span class="strong">${copyable(r.subject)}</span>`;
    }
    function subjectSub(r) {
      if (r.kind === 'new_source') return sourceLineHtml(intel(r.subject, r.subject_intel), { ip: r.subject });
      if (r.kind === 'connection_spike') { const s = spikeNums(r.message); return s.med != null ? esc(`平时中位数 ${fmt.num(s.med)} 次`) : ''; }
      return '';
    }
    function ackCell(r) {
      return r.acked
        ? `<span class="t-secondary" data-ack="${esc(r.id)}">已查看</span>`
        : `<span data-ack="${esc(r.id)}">${button({ label: '标记已查看', icon: 'check', size: 'sm', attrs: { 'data-act': 'ack', 'data-id': r.id } })}</span>`;
    }
    function emptyFor(view) {
      const clear = hasFilters() ? { label: '清除筛选', attrs: { 'data-act': 'clear' } } : null;
      if (view === 'targets' && st.result === 'blocked' && ex) {
        const by = store.nodes().filter((n) => n.behavior?.blocked && matchNode(n.id)).map((n) => `${nameOf(n.id)} ${fmt.num(n.behavior.blocked)} 条`);
        return empty({ icon: 'shield-check', text: by.length ? `前 ${ex.top_targets_by_node.length} 组目标里没有已阻止的连接（已阻止的在 ${by.join('、')}）` : '没有已阻止的连接', action: { label: '取消“只看已阻止”', attrs: { 'data-act': 'only-blocked' } } });
      }
      if (view === 'targets' && st.asset && ex && !st.q && !st.kind && !st.result) {
        const b = nodeOf(st.asset)?.behavior;
        return empty({ text: `快照只带了全机队前 ${ex.top_targets_by_node.length} 组“节点 × 目标”，${nameOf(st.asset)} 不在其中${b?.connections ? `（24 小时 ${fmt.num(b.connections)} 条连接、${fmt.num(b.domains)} 个目标）` : ''}`, action: { label: '看来源地址', attrs: { 'data-act': 'goto-sources' } } });
      }
      if (view === 'events' && st.ek === 'new_domain') {
        const days = ex?.new_domain_daily ?? [];
        const avg = days.length ? Math.round(days.reduce((a, d) => a + d.count, 0) / days.length) : null;
        return empty({ icon: 'globe', text: `新域名事件没有逐条明细：14 天共 ${fmt.num(totals.new_domain ?? 0)} 条${avg ? `，平均每天 ${fmt.num(avg)} 条` : ''}；页签圆点不统计它` });
      }
      if (view === 'events' && st.ek === 'blocked_rise') return empty({ icon: 'shield-check', text: '最近没有阻止增多的事件' });
      if (clear) return empty({ text: '没有符合筛选条件的记录', action: clear });
      return empty({ text: view === 'events' ? '最近没有行为变化' : '没有记录' });
    }

    // ─── 筛选条（SPEC §6.5：即时生效；次要筛选在 ≤900 收进“筛选 ▾”） ───
    // 筛选控件与节点页同一种：按钮 + 菜单（原生 <select> 换掉，consistency #1）。没选时写类别名“团队 ▾”，选了写“团队：团队1”、软靛蓝底
    const extraCount = () => (st.kind ? 1 : 0) + (st.result ? 1 : 0);
    const FILTER_WORD = { team: '团队', asset: '节点', kind: '目标类型', result: '处理结果' };
    function filterValueWord(k) {
      if (k === 'team') return st.team;
      if (k === 'asset') return st.asset ? nameOf(st.asset) : '';
      if (k === 'kind') return KINDS.find((x) => x.value === st.kind && x.value)?.label ?? '';
      return RESULTS.find((x) => x.value === st.result && x.value)?.label ?? '';
    }
    function filterBtn(k) {
      const v = filterValueWord(k);
      const off = (k === 'kind' || k === 'result') && st.view !== 'targets';
      const btn = button({ label: v ? `${FILTER_WORD[k]}：${v}` : FILTER_WORD[k], caret: true, attrs: { 'data-act': 'ffilter', 'data-k': k, 'aria-haspopup': 'menu', 'aria-expanded': 'false', 'aria-pressed': v ? 'true' : 'false', disabled: off } });
      return off ? `<span class="select-wrap" data-tip="只作用于“域名与 IP”页签">${btn}</span>` : btn;
    }
    function fformHtml() {
      return `<span data-fslot="team">${filterBtn('team')}</span>
        <span data-fslot="asset">${filterBtn('asset')}</span>
        ${segmented({ name: 'range', items: RANGES, value: st.range, label: '时间范围' })}
        ${searchBox({ name: 'q', value: st.q, placeholder: '搜索域名或来源 IP' })}
        <span class="fform-more"><span data-fslot="kind">${filterBtn('kind')}</span><span data-fslot="result">${filterBtn('result')}</span></span>
        <span class="fform-menu">${button({ label: `筛选${extraCount() ? ` ${extraCount()}` : ''}`, icon: 'list-filter', caret: true, attrs: { 'data-act': 'more-filters', 'aria-haspopup': 'menu', 'aria-expanded': 'false' } })}</span>
        <span class="fform-end" data-clear${hasFilters() ? '' : ' hidden'}>${button({ label: '清除', variant: 'quiet', attrs: { 'data-act': 'clear' } })}</span>`;
    }
    function filterMenu(b, k) {
      let items;
      if (k === 'team') items = [{ value: '', label: '全部团队', checked: !st.team }, { sep: true }, ...store.teams().map((t) => ({ value: t.name, label: t.name, checked: st.team === t.name, count: store.nodes().filter((n) => n.team === t.name).length }))];
      else if (k === 'asset') {
        const teams = store.teams().filter((t) => !st.team || t.name === st.team);
        items = [{ value: '', label: '全部节点', checked: !st.asset }];
        for (const t of teams) {
          const ns = store.nodes().filter((n) => n.team === t.name);
          if (!ns.length) continue;
          items.push({ group: t.name }, ...ns.map((n) => ({ value: n.id, label: nameOf(n.id), checked: st.asset === n.id })));
        }
      } else if (k === 'kind') items = KINDS.map((x) => ({ value: x.value, label: x.label, checked: st.kind === x.value }));
      else items = RESULTS.map((x) => ({ value: x.value, label: x.label, checked: st.result === x.value }));
      openMenu(b, {
        label: FILTER_WORD[k], minWidth: k === 'asset' ? 220 : 180, items,
        onSelect: (v) => {
          if (k === 'team') commit({ team: v, asset: st.asset && v && teamOf(st.asset) !== v ? '' : st.asset, page: 1 });
          else if (k === 'asset') commit({ asset: v, page: 1 });
          else if (k === 'kind') commit({ kind: v, page: 1 });
          else commit({ result: v, page: 1 });
        },
      });
    }
    // 筛选条只同步变化的部分：搜索框不重建（打字时不丢焦点与光标）；筛选按钮按身份（data-act + data-k）换掉，焦点找得回来
    function syncForm() {
      const f = el.querySelector('[data-fform]');
      if (!f) return;
      for (const k of ['team', 'asset', 'kind', 'result']) {
        const slot = f.querySelector(`[data-fslot="${k}"]`);
        const html = filterBtn(k);
        if (slot && slot.__html !== html) {
          const had = slot.contains(document.activeElement);
          slot.innerHTML = html; slot.__html = html;
          if (had) slot.querySelector('button')?.focus({ preventScroll: true });
        }
      }
      setSegmented(f.querySelector('[data-seg="range"]'), st.range);
      setSearch(f, 'q', st.q);
      const mb = f.querySelector('[data-act="more-filters"] span');
      if (mb) mb.textContent = `筛选${extraCount() ? ` ${extraCount()}` : ''}`;
      f.querySelector('[data-clear]').hidden = !hasFilters();
    }

    // ─── 选择上下文（选了节点时） ───
    function ctxHtml() {
      const n = nodeOf(st.asset);
      if (!n) return '';
      const b = n.behavior ?? {};
      return `<div class="ctxbar" role="region" aria-label="当前节点">
        <span class="ctxbar-name">${esc(nameOf(n.id))}</span>${copyable(n.ip)}
        <span class="ctxbar-meta">${esc(n.isRelay ? M.entryText(store, n) : [n.team, n.entry_label].filter(Boolean).join(' · '))}</span>
        <span data-ctx-pill>${statusPill(M.healthOf(store, n))}</span>
        <span class="ctxbar-meta" data-ctx-stats>${ctxStats(n)}</span>
        <div class="ctxbar-end">${button({ label: '节点详情', icon: 'panel-right-open', size: 'sm', attrs: { 'data-act': 'ctx-open' } })}${button({ label: '访问规则', icon: 'shield-check', size: 'sm', attrs: { 'data-act': 'ctx-rules' } })}${iconButton({ icon: 'x', label: '取消节点筛选', size: 'sm', attrs: { 'data-act': 'ctx-clear' } })}</div>
      </div>`;
    }
    function ctxStats(n) { const b = n.behavior ?? {}; return esc(`24 小时 ${fmt.num(b.connections ?? 0)} 条连接 · ${fmt.num(b.domains ?? 0)} 个目标 · ${fmt.num(b.sources ?? 0)} 个来源`); }
    function rangeNote() {
      if (st.range === '24h') return '';
      return note({ tone: 'info', title: `${RANGE_WORD[st.range]}`, body: isLive() ? '接口目前只提供最近 24 小时的汇总；新后台会按所选时间范围查询。下面仍是 24 小时的数据。' : '原型快照只有最近 24 小时的汇总；生产环境会按所选时间范围重新查询。下面仍是 24 小时的数据。' });
    }

    // ─── 渲染：节点行为 ───
    function behaviorHtml() {
      return `<div class="fform" role="search" aria-label="筛选节点行为" data-fform>${fformHtml()}</div>
        <div data-ctx${st.asset ? '' : ' hidden'}>${ctxHtml()}</div>
        <div data-note${st.range === '24h' ? ' hidden' : ''}>${rangeNote()}</div>
        <section class="kpi-grid" data-kpis aria-label="行为概览">${kpisHtml()}</section>
        <section class="panel" data-list aria-label="行为明细">
          ${tabsHtml()}
          <div class="panel-bar" data-bar hidden></div>
          <div data-table>${tableSkeleton(6, 8)}</div>
          <div class="table-foot" data-foot hidden></div>
        </section>`;
    }
    /** 重画一块区域时保住键盘焦点：记下焦点元素的“身份”（data-value / data-sort / data-row / data-act+data-id），重画后找回来。 */
    function keepFocus(box, paint) {
      const a = document.activeElement;
      const inside = box && a && box.contains(a) && a !== box;
      const who = inside ? ['data-value', 'data-sort', 'data-row', 'data-id'].map((k) => [k, a.getAttribute(k)]).find(([, v]) => v != null) : null;
      paint();
      if (!who) return;
      const next = box.querySelector(`[${who[0]}="${CSS.escape(who[1])}"]`) ?? (who[0] === 'data-id' ? box.querySelector(`tr[data-row="${CSS.escape(who[1])}"]`) : null);
      next?.focus({ preventScroll: true });
    }
    function renderTabs() {
      const cur = el.querySelector('[data-list] > .tabs');
      if (!cur) return;
      const list = cur.parentElement;
      keepFocus(list, () => {
        const t = document.createElement('template');
        t.innerHTML = tabsHtml().trim();
        const next = t.content.firstElementChild;
        list.querySelector(':scope > .tabs').replaceWith(next);
        hydrate(list);
      });
    }
    function renderBar() {
      const bar = el.querySelector('[data-bar]');
      if (!bar) return;
      if (st.view === 'events' && ex) {
        const base = eventRows(false);
        const counts = Object.fromEntries(EVENT_KINDS.map((k) => [k.value, k.value ? base.filter((e) => e.kind === k.value).length : base.length]));
        const unackedTotal = Object.entries(unacked).filter(([k]) => k !== 'new_domain').reduce((a, [, n]) => a + n, 0);
        const html = `${segmented({ name: 'ek', size: 'sm', label: '事件类型', value: st.ek, items: EVENT_KINDS.map((k) => ({ ...k, count: k.value === 'new_domain' ? undefined : counts[k.value] })) })}
          <span class="panel-bar-end">最近 ${num(fmt.int(ex.events_recent.length, '条'))} · 14 天未查看 ${num(fmt.int(unackedTotal, '条'))}（不含新域名 ${num(fmt.int(unacked.new_domain ?? 0, '条'))}）</span>`;
        keepFocus(bar, () => { bar.innerHTML = html; bar.hidden = false; hydrate(bar); });
      } else if (st.view === 'sources' && st.team && !st.asset) {
        bar.innerHTML = `<span>按团队筛选只决定显示哪些来源；连接与目标是该来源在全机队的合计。选一个节点可看这个节点上的来源明细。</span>`;
        bar.hidden = false;
      } else if (st.view === 'sources' && st.asset) {
        bar.innerHTML = `<span>${esc(nameOf(st.asset))} 上 ${RANGE_WORD['24h']}有连接的来源；数值是这个节点上的连接。</span>`;
        bar.hidden = false;
      } else { bar.hidden = true; bar.innerHTML = ''; }
    }
    // 对话框 / 抽屉开着时不重画背后的列表（行会换位置，看起来像背后的页面被滚走了）；关掉后补一次
    let listDeferred = false;
    const lockObs = new MutationObserver(() => { if (listDeferred && !document.documentElement.classList.contains('is-locked') && el.isConnected) { listDeferred = false; renderList(); } });
    lockObs.observe(document.documentElement, { attributes: true, attributeFilter: ['class'] });
    function renderList() {
      if (document.documentElement.classList.contains('is-locked') && el.querySelector('[data-table] table')) { listDeferred = true; return; }
      const box = el.querySelector('[data-table]');
      const foot = el.querySelector('[data-foot]');
      if (!box) return;
      renderBar();
      if (exError && st.view !== 'nodes') {
        box.innerHTML = `<div class="panel-body">${note({ tone: 'bad', title: '行为明细加载失败', body: esc(`读取 data/extra.json 失败：${exError}`), action: button({ label: '重试', size: 'sm', attrs: { 'data-act': 'retry' } }) })}</div>`;
        foot.hidden = true;
        return;
      }
      // 已有表格时不回到骨架：旧行变淡留在原处，新数据到了就地换上（筛选、切节点不闪白）
      const hasRows = !!box.querySelector('table');
      if (!ex && st.view !== 'nodes') { if (hasRows) box.classList.add('is-refreshing'); else { box.innerHTML = tableSkeleton(6, 8); foot.hidden = true; } return; }
      const spec = tableSpec();
      if (spec.loading) { if (hasRows) box.classList.add('is-refreshing'); else { box.innerHTML = tableSkeleton(5, 6); foot.hidden = true; } return; }
      box.classList.remove('is-refreshing');
      const total = spec.rows.length;
      const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));
      const page = Math.min(st.page, pages);
      const rows = spec.rows.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE);
      order = rows.map((r) => r.node ?? r.id);
      const evidence = st.view !== 'nodes'; // 前三个页签点行打开证据对话框，“节点概览”打开节点抽屉
      keepFocus(box, () => { box.innerHTML = dataTable({ columns: spec.columns, rows, getId: (r) => r.id, sortOf, current: st.view === 'nodes' ? drawerId : null, two: true, rowLink: true, empty: spec.empty, label: spec.label, cls: 'is-sticky-first', rowIcon: evidence ? 'scan-search' : 'chevron-right', rowTip: evidence ? '查看证据' : '' }); });
      foot.hidden = total === 0;
      // 实时：这一页列出的来源 IP 都要有情报（运营商 · 城市 · 接入方式）；缺的去新后台取，到了重画这一页
      if (isLive()) ensureIntel(store, rows.map((r) => r.source_ip ?? (r.kind === 'new_source' ? r.subject : null)), () => { if (el.isConnected) renderList(); });
      foot.innerHTML = `<span>每页 ${num(fmt.int(PAGE_SIZE, '条'))}</span><span>第 <span class="num">${page}</span> 页 · 共 ${num(fmt.int(total, '条'))}</span><span class="row">${button({ label: '上一页', size: 'sm', attrs: { 'data-act': 'page', 'data-dir': '-1', disabled: page <= 1 } })}${button({ label: '下一页', size: 'sm', attrs: { 'data-act': 'page', 'data-dir': '1', disabled: page >= pages } })}</span>`;
    }
    function renderKpis() {
      const box = el.querySelector('[data-kpis]');
      if (box) box.innerHTML = kpisHtml();
    }
    function renderCtx() {
      const c = el.querySelector('[data-ctx]');
      if (!c) return;
      c.innerHTML = ctxHtml();
      c.hidden = !st.asset || !c.innerHTML;
    }
    function renderNote() {
      const c = el.querySelector('[data-note]');
      if (!c) return;
      c.innerHTML = rangeNote();
      c.hidden = st.range === '24h';
    }
    function ensureNodeSources() {
      const id = st.asset;
      if (!id || nodeSources.has(id)) return;
      store.detail(id).then((d) => {
        const list = (d?.ipintel?.sources ?? []).filter((s) => (s.connections?.h24 ?? 0) > 0).map((s) => {
          const x = intel(s.ip, null);
          return { id: s.ip, source_ip: s.ip, connections: s.connections.h24, domains: null, last_seen: s.last_seen, first_seen: s.first_seen, sessions_h24: s.sessions?.h24 ?? null, clients: s.clients ?? [], x, carrier: x?.carrier, city: x?.city, province: x?.province };
        });
        nodeSources.set(id, list);
        if (st.asset === id && el.isConnected) { renderTabs(); if (st.view === 'sources') renderList(); }
      }).catch((e) => {
        nodeSources.set(id, []);
        toast({ message: `读取 ${nameOf(id)} 的来源明细失败：${e.message}`, tone: 'bad' });
      });
    }

    // ─── 渲染：访问规则（本轮只做外形与本地草稿，SPEC §6.5） ───
    function scopes() {
      const teams = store.teams().filter((t) => t.name !== '线路机').map((t) => ({ key: `team:${t.name}`, name: t.name, sub: `${store.nodes().filter((n) => n.team === t.name && !n.isRelay).length} 台家宽节点`, team: t.name }));
      const nodes = store.nodes().filter((n) => n.relay_port).map((n) => ({ key: `node:${n.id}`, name: nameOf(n.id), sub: n.ip, node: n }));
      return { teams, nodes };
    }
    const currentScope = () => { const s = scopes(); const all = [...s.teams, ...s.nodes]; return all.find((x) => x.key === st.scope) ?? all.find((x) => x.key === (st.asset ? `node:${st.asset}` : '')) ?? s.teams[0]; };
    // 实时：规则状态来自接口（live-policies.js：新后台开放了就走 fleetd，否则旧接口只读）；原型：本地草稿
    const pol = { state: null, loading: false, error: null, loadedAt: 0 };
    const polOn = () => isLive();
    function loadPol({ force = false } = {}) {
      if (!polOn() || pol.loading || (!force && pol.state && Date.now() - pol.loadedAt < 15000)) return;
      pol.loading = true;
      POL.loadPolicies().then((x) => { pol.state = x; pol.error = null; pol.loadedAt = Date.now(); }, (e) => { pol.error = e.message; })
        .finally(() => { pol.loading = false; if (el.isConnected && st.mode === 'rules') renderBody(); });
    }
    const liveMode = (sc) => (polOn() && pol.state?.active?.[POL.scopeKey(sc)]?.mode) || (sc.team ? 'observe' : 'inherit');
    /** 草稿：实时 = 接口里这个范围的草稿（{mode, domains, note}），原型 = 本地 Map。 */
    function draftOf(sc) {
      if (!polOn()) return drafts.get(sc.key) ?? null;
      const d = pol.state?.drafts?.[POL.scopeKey(sc)];
      return d ? { mode: d.policy?.mode, domains: POL.domainsText(d.policy?.domains), note: d.policy?.note ?? '' } : null;
    }
    const draftCount = () => (polOn() ? Object.keys(pol.state?.drafts ?? {}).length : drafts.size);
    /** 线上生效的规则（实时）：编辑框在没有草稿时显示它。 */
    function activeOf(sc) {
      const a = polOn() ? pol.state?.active?.[POL.scopeKey(sc)] : null;
      return a ? { mode: a.mode, domains: POL.domainsText(a.domains), note: a.note ?? '' } : null;
    }
    function pickHtml(sc, cur) {
      const d = draftOf(sc);
      const chip = d ? tonePill('草稿', 'warn', { dot: 'none', title: '有未应用的草稿' }) : tagChip(RULE_WORD[liveMode(sc)]);
      return `<button type="button" class="pick" data-scope="${esc(sc.key)}"${sc.key === cur.key ? ' aria-current="true"' : ''}><span class="pick-main"><span class="pick-name">${esc(sc.name)}</span><span class="pick-sub">${esc(sc.sub)}</span></span>${chip}</button>`;
    }
    function rulesHtml() {
      const s = scopes();
      const cur = currentScope();
      const d = draftOf(cur);
      const a = activeOf(cur);
      const shown = d ?? a;
      const mode = d?.mode ?? liveMode(cur);
      const modes = RULE_MODES.filter((m) => m.value !== 'inherit' || !cur.team);
      const n = cur.node;
      const teamMode = (t) => RULE_WORD[liveMode({ team: t, key: `team:${t}` })];
      const sub = cur.team
        ? `${cur.sub} · 线上生效 ${RULE_WORD[liveMode(cur)]}`
        : liveMode(cur) === 'inherit'
          ? `${n.ip} · ${n.isRelay ? M.entryText(store, n) : n.entry_label ?? '未绑定'} · 线上${RULE_WORD.inherit}（${n.team} ${teamMode(n.team)}）`
          : `${n.ip} · ${n.isRelay ? M.entryText(store, n) : n.entry_label ?? '未绑定'} · 线上生效 ${RULE_WORD[liveMode(cur)]}`;
      const list = `<div class="picklist rules-list" role="list">
          <div class="picklist-title">团队默认规则</div>${s.teams.map((x) => pickHtml(x, cur)).join('')}
          <div class="picklist-title">单独配置节点</div>${s.nodes.map((x) => pickHtml(x, cur)).join('')}
        </div>`;
      const pill = d ? statusPill({ word: '待应用', tone: 'warn', dot: 'solid' }) : statusPill({ word: '已同步', tone: 'ok', dot: 'solid' });
      const form = `<form class="stack-sm" data-rule-form novalidate>
          <label class="field"><span class="field-label">访问方式</span>${select({ name: 'rule-mode', value: mode, options: modes, label: '访问方式' })}</label>
          <label class="field"><span class="field-label">域名列表</span><textarea class="textarea rules-domains" name="domains" rows="6" spellcheck="false" placeholder="example.com&#10;=login.example.org" aria-describedby="rules-hint">${esc(shown?.domains ?? '')}</textarea><span class="field-hint" id="rules-hint">每行一个域名，最多 300 个；前缀 = 表示精确匹配，*. 前缀会自动去掉；不接受 IP、网址或端口。白名单和黑名单不能为空。</span><span class="field-error" data-rule-error hidden></span></label>
          <label class="field"><span class="field-label">规则备注（选填）</span><input class="input" name="note" value="${esc(shown?.note ?? '')}" placeholder="备注（选填）" autocomplete="off"></label>
          <div class="row">${button({ label: '保存草稿', variant: 'primary', attrs: { 'data-act': 'rule-save', disabled: true } })}${button({ label: '预览影响', attrs: { 'data-act': 'rule-preview' } })}${button({ label: '丢弃全部草稿', variant: 'quiet', attrs: { 'data-act': 'rule-discard', disabled: !draftCount() } })}</div>
        </form>`;
      const history = dataTable({ columns: historyCols(), rows: polOn() ? (pol.state?.versions ?? []) : [], getId: (v) => String(v.id), rowLink: false, two: false,
        empty: polOn() && !pol.state ? empty({ icon: pol.error ? 'circle-alert' : 'loader-circle', text: pol.error ? `读取访问规则失败：${pol.error}` : '正在读取访问规则' }) : empty({ icon: 'shield-check', text: '访问规则还没有发布过' }), label: '规则发布记录' });
      return `<div class="rules-grid">
          ${panel({ title: '选择管理范围', count: `${s.teams.length + s.nodes.length} 个范围`, body: list, cls: 'rules-scopes' })}
          ${panel({ title: cur.name, sub: esc(sub), tools: pill, body: form, cls: 'rules-editor' })}
        </div>
        ${panel({ title: '规则发布记录', tools: button({ label: '刷新状态', icon: 'rotate-cw', size: 'md', attrs: { 'data-act': 'rule-refresh' } }), body: history, flush: true })}`;
    }

    // 规则发布记录：状态词同旧运营页；已应用、且不是最新一版的可以回滚（新后台开放 policies 时）
    const V_WORD = { applied: ['已应用', 'ok'], applying: ['进行中', 'info'], failed_rolled_back: ['已回滚', 'warn'], rollback_failed: ['失败', 'bad'], rolled_back: ['已回滚', 'warn'], superseded: ['已被取代', 'neutral'] };
    function versionResult(v) {
      const r = v.result ?? {};
      if (r.error) return String(r.error);
      return Object.entries(r).map(([k, x]) => `${k}：${x?.ok ? '成功' : x?.rollback ? '已撤回' : x?.rollback_error || x?.error || '—'}`).join(' / ') || '—';
    }
    function historyCols() {
      const latest = (pol.state?.versions ?? []).find((v) => v.status === 'applied')?.id;
      return [
        { key: 't', label: '时间', cell: (v) => `<span class="nowrap" data-tip="${esc(fmt.stamp(v.ts).exact ?? '')}">${esc(fmt.stamp(v.ts).text)}</span>` },
        { key: 'v', label: '版本', cell: (v) => `<span class="mono">${esc(v.id)}</span>` },
        { key: 's', label: '状态', cell: (v) => { const [w, t] = V_WORD[v.status] ?? [v.status, 'neutral']; return statusPill({ word: w, tone: t, dot: v.status === 'applying' ? 'pulse' : 'solid' }); } },
        { key: 'r', label: '处理结果', cell: (v) => `<span class="clip" data-tip="${esc(versionResult(v))}">${esc(versionResult(v))}</span>` },
        { key: 'a', label: '操作', align: 'right', cell: (v) => (v.status === 'applied' && v.id !== latest && POL.canRollback() ? button({ label: '回滚到这个版本', icon: 'undo-2', size: 'sm', attrs: { 'data-act': 'rule-rollback', 'data-v': v.id } }) : '') },
      ];
    }

    // 新后台推 policies（草稿、发布、回滚变化）：在访问规则里就重读
    const offPol = onFleet(({ resource }) => { if (resource === 'policies' && st.mode === 'rules' && el.isConnected) loadPol({ force: true }); });

    // ─── 首屏 ───
    const tools = HEAD_TOOLS.behavior({ density: app.density() });
    el.innerHTML = `<div class="page" data-page="behavior">
      ${pageHead({ title: '节点行为', tools })}
      <div class="row">${segmented({ name: 'mode', role: 'tablist', items: MODES, value: st.mode, label: '节点行为 / 访问规则' })}</div>
      <div class="stack" data-body></div>
    </div>`;
    const body = el.querySelector('[data-body]');
    function renderBody() {
      if (st.mode === 'rules') loadPol();
      body.innerHTML = st.mode === 'rules' ? rulesHtml() : behaviorHtml();
      hydrate(body);
      if (st.mode === 'behavior') { if (st.asset) ensureNodeSources(); renderList(); }
    }
    renderBody();
    hydrate(el);

    function loadExtra() {
      exError = null;
      store.extra().then((x) => {
        ex = x.behavior;
        if (!el.isConnected || st.mode !== 'behavior') return;
        renderTabs(); renderList();
      }).catch((e) => {
        exError = e.message ?? String(e);
        if (el.isConnected && st.mode === 'behavior') { renderTabs(); renderList(); }
      });
    }
    loadExtra();

    // ─── 状态写回 URL ───
    let renderedKey = JSON.stringify(writeState(st));
    // 只写 URL，不先改 st：update(params) 需要拿“上一次渲染的状态”做对比（只重画变化的部分）
    function commit(patch) {
      app.setParams(writeState({ ...st, ...patch }), { replace: true });
    }

    // ─── 交互 ───
    const offs = [watchStuck(el)];
    offs.push(onSegment(el, 'density', (v) => app.setDensity(v)));
    offs.push(onSegment(el, 'mode', (v) => commit({ mode: v })));
    offs.push(onSegment(el, 'range', (v) => commit({ range: v, page: 1 })));
    offs.push(onSegment(el, 'ek', (v) => commit({ ek: v, page: 1 })));
    offs.push(onSelect(el, 'rule-mode', () => markDirty()));
    offs.push(on(el, 'input', '[data-rule-form]', () => markDirty()));
    offs.push(onSearch(el, 'q', (v) => commit({ q: v, page: 1 })));
    offs.push(on(el, 'tab-change', '[data-tabs="view"]', (e) => commit({ view: e.detail.value, page: 1, sort: '', ek: e.detail.value === 'events' ? st.ek : '' })));
    offs.push(on(el, 'click', '[data-sort]', (e, b) => {
      const key = b.dataset.sort;
      const s = sortSpec();
      const next = s.key === key ? (s.dir === -1 ? key : `-${key}`) : `-${key}`;
      commit({ sort: next, page: 1 });
    }));
    offs.push(on(el, 'click', '[data-focus]', (e, b) => { e.stopPropagation(); commit({ asset: b.dataset.focus, team: '', page: 1 }); }));
    offs.push(on(el, 'click', '[data-scope]', (e, b) => commit({ scope: b.dataset.scope })));
    offs.push(on(el, 'click', '[data-row]', (e, tr) => {
      if (e.target.closest('button, a, input, select, label')) return;
      openRow(tr.dataset.row);
    }));
    offs.push(on(el, 'keydown', 'tr[data-row]', (e, tr) => {
      if (e.target !== tr) return;
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); openRow(tr.dataset.row); }
    }));
    offs.push(on(el, 'click', '[data-act]', (e, b) => {
      const act = b.dataset.act;
      // 只读预览：标记已查看、访问规则的保存 / 丢弃都是写操作
      const W = { ack: 'behavior.event.ack', 'rule-save': 'behavior.rules.saveDraft', 'rule-discard': 'behavior.rules.discard', 'rule-rollback': 'behavior.rules.rollback' }[act];
      if (W && isLive()) {
        e.preventDefault(); e.stopPropagation();
        if (guard(W, { node: st.asset ? store.node(st.asset) : ruleNode() })) return;
        // 写开关打开：标记已查看真的写回（POST /behavior/ack），成功后只把这一行置为已查看（旧运营页同样不弹提示）
        if (act === 'ack' && isWired(W)) behaviorAck(app, b.dataset.id, b).then((ok) => { if (ok) markAcked(Number(b.dataset.id)); });
        // 访问规则（新后台开放 policies）：服务端草稿、丢弃、回滚
        else if (act === 'rule-save') saveDraft();
        else if (act === 'rule-discard') discardDrafts();
        else if (act === 'rule-rollback') rollbackRules(b.dataset.v, b);
        return;
      }
      if (act === 'clear') commit({ team: '', asset: '', q: '', kind: '', result: '', range: '24h', ek: '', page: 1 });
      else if (act === 'more-filters') moreFiltersMenu(b);
      else if (act === 'ffilter') filterMenu(b, b.dataset.k);
      else if (act === 'goto-nodes') commit({ view: 'nodes', page: 1, sort: '' });
      else if (act === 'goto-sources') commit({ view: 'sources', page: 1, sort: '' });
      else if (act === 'only-blocked') commit(st.result === 'blocked' ? { result: '', page: 1 } : { result: 'blocked', view: 'targets', page: 1, sort: '' });
      else if (act === 'ctx-open') app.openNode(st.asset, 'access');
      else if (act === 'ctx-rules') commit({ mode: 'rules', scope: `node:${st.asset}` });
      else if (act === 'ctx-clear') commit({ asset: '', page: 1 });
      else if (act === 'ack') { e.stopPropagation(); ack(Number(b.dataset.id)); }
      else if (act === 'page') commit({ page: Math.max(1, st.page + Number(b.dataset.dir)) });
      else if (act === 'retry') { loadExtra(); renderList(); }
      else if (act === 'export') exportJson();
      else if (act === 'more') moreMenu(b);
      else if (act === 'refresh') refresh(b);
      else if (act === 'rule-save') { e.preventDefault(); saveDraft(); }
      else if (act === 'rule-preview') { e.preventDefault(); previewRules(); }
      else if (act === 'rule-discard') { e.preventDefault(); discardDrafts(); }
      else if (act === 'rule-refresh') { if (isLive()) { b.classList.add('is-busy'); loadPol({ force: true }); setTimeout(() => b.classList.remove('is-busy'), 400); } else refresh(b, { quiet: true }); }
    }));
    offs.push(on(el, 'submit', '[data-rule-form]', (e) => { e.preventDefault(); if (guard('behavior.rules.saveDraft', { node: ruleNode() })) return; saveDraft(); }));
    function ruleNode() { const k = st.scope ?? ''; return k.startsWith('node:') ? store.node(k.slice(5)) : null; }
    offs.push(on(el, 'click', '[data-kpi]', (e, k) => {
      if (e.target.closest('[data-info], .kpi-action')) return; // ⓘ 口径说明不当作点卡片
      if (k.dataset.kpi === 'active') commit(st.view === 'nodes' ? { view: 'targets', page: 1, sort: '' } : { view: 'nodes', page: 1, sort: '' });
      else if (k.dataset.kpi === 'blocked') commit(st.result === 'blocked' ? { result: '', page: 1 } : { result: 'blocked', view: 'targets', page: 1, sort: '' });
    }));

    function moreFiltersMenu(b) {
      openMenu(b, {
        label: '更多筛选', minWidth: 200,
        items: [
          { group: '目标类型' }, ...KINDS.map((k) => ({ value: `type:${k.value}`, label: k.label, checked: st.kind === k.value, disabled: st.view !== 'targets' })),
          { sep: true },
          { group: '处理结果' }, ...RESULTS.map((k) => ({ value: `result:${k.value}`, label: k.label, checked: st.result === k.value, disabled: st.view !== 'targets' })),
        ],
        onSelect: (v) => { const [k, val] = v.split(':'); commit(k === 'type' ? { kind: val, page: 1 } : { result: val, page: 1 }); },
      });
    }
    function moreMenu(b) {
      openMenu(b, {
        label: '更多', minWidth: 200,
        items: [
          { value: 'activity', label: '查看访问记录', icon: 'history' },
          { value: 'link', label: '复制当前筛选的链接', icon: 'copy' },
        ],
        onSelect: (v) => {
          if (v === 'activity' && isLive()) openActivity(st.asset ? store.node(st.asset) : null);
          else if (v === 'activity') toast({ message: `访问记录页不在本轮原型里（SPEC §6.7）；生产环境会带上当前筛选${st.asset ? `（${nameOf(st.asset)}）` : ''}打开`, tone: 'info' });
          else navigator.clipboard?.writeText(location.href).then(() => toast({ message: '已复制当前筛选的链接' }), () => toast({ message: '复制失败，请从地址栏复制', tone: 'bad' }));
        },
      });
    }
    function refresh(b, { quiet = false } = {}) {
      if (b.classList.contains('is-busy') || b.getAttribute('aria-busy') === 'true') return;
      if (b.classList.contains('icon-btn')) b.classList.add('is-busy'); else b.setAttribute('aria-busy', 'true');
      const wait = isLive() && app.live.refresh ? app.live.refresh({ force: ['minute'] }).catch(() => {}) : new Promise((r) => setTimeout(r, 600));
      wait.then(() => {
        b.classList.remove('is-busy'); b.removeAttribute('aria-busy');
        if (!el.isConnected) return;
        if (st.mode === 'behavior') { renderKpis(); renderTabs(); renderList(); }
        else if (!quiet) renderBody();
      });
    }
    function exportJson() {
      const date = fmt.date(store.now()).text;
      let name, data;
      if (st.mode === 'rules') {
        const s = scopes();
        name = `访问规则-${date}.json`;
        data = { exported_at: fmt.absolute(store.now()), scopes: [...s.teams, ...s.nodes].map((x) => ({ scope: polOn() ? POL.scopeKey(x) : x.key, name: x.name, mode: RULE_WORD[liveMode(x)], active: activeOf(x), draft: draftOf(x) })), versions: polOn() ? (pol.state?.versions ?? []) : [] };
      } else {
        name = `节点行为-${date}.json`;
        const f = writeState(st);
        data = {
          exported_at: fmt.absolute(store.now()),
          filters: Object.fromEntries(Object.entries(f).filter(([, v]) => v != null)),
          scope_note: '按当前筛选导出；数值是最近 24 小时（快照）',
          kpis: Object.fromEntries(Object.entries(kpiModel()).map(([k, v]) => [k, v.value.replace(/<[^>]+>/g, '')])),
          targets: ex ? targetRows().map(({ id, ...r }) => ({ ...r, node_name: nameOf(r.node) })) : [],
          sources: (sourceRows() ?? []).map(({ x, ...r }) => r),
          events: eventRows().map((r) => ({ ...r, node_name: nameOf(r.node) })),
          nodes: nodeRows().map((r) => ({ ...r, name: nameOf(r.id) })),
        };
      }
      const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
      const a = document.createElement('a');
      a.href = URL.createObjectURL(blob);
      a.download = name;
      document.body.appendChild(a); a.click(); a.remove();
      setTimeout(() => URL.revokeObjectURL(a.href), 1000);
      toast({ message: `已导出 ${name}` });
    }

    // ─── 行点击：证据对话框 / 抽屉 ───
    function openRow(id) {
      if (st.view === 'targets') {
        const r = ex?.top_targets_by_node.find((x) => `${x.node}|${x.host}` === id);
        if (r) openEvidence({ host: r.host, node: r.node, total: r.connections, title: r.host, sub: nameOf(r.node) });
      } else if (st.view === 'sources') {
        const r = (sourceRows() ?? []).find((x) => x.id === id);
        if (r) openEvidence({ source: r.source_ip, node: st.asset || null, total: r.connections, title: r.source_ip, sub: st.asset ? nameOf(st.asset) : '全部节点', x: r.x });
      } else if (st.view === 'events') {
        const ev = ex?.events_recent.find((x) => String(x.id) === id);
        if (!ev) return;
        if (ev.kind === 'new_source') openEvidence({ source: ev.subject, node: ev.node, title: ev.subject, sub: `${nameOf(ev.node)} · 新来源 ${fmt.ago(ev.ts).text}`, x: intel(ev.subject, ev.subject_intel) });
        else openEvidence({ node: ev.node, title: nameOf(ev.node), sub: `${EVENT_WORD[ev.kind] ?? ev.kind} · ${fmt.absolute(ev.ts)}`, message: ev.message, at: ev.ts });
      } else if (st.view === 'nodes') app.openNode(id, 'access');
    }
    /** 证据对话框（原“连接记录 / 来源记录”）：标题 = 目标或来源，副标题 = 节点；统计行 + 表 5 列。 */
    function openEvidence({ host, node, source, total, title, sub, x, message, at }) {
      const seen = new Set();
      let rows = [];
      const push = (r, nid) => {
        if (host && r.host !== host) return;
        if (source && r.source_ip !== source) return;
        const k = `${r.ts}|${nid}|${r.source_ip}|${r.host}|${r.port}`;
        if (seen.has(k)) return;
        seen.add(k);
        rows.push({ ...r, node: nid });
      };
      const ids = node ? [node] : store.nodes().map((n) => n.id);
      for (const id of ids) for (const r of store.access(id)) push(r, id);
      for (const r of ex?.timeline_recent ?? []) if (!node || r.node === node) push(r, r.node);
      rows.sort((a, b) => b.ts - a.ts);
      // 实时：逐条记录向新后台要（节点 / 目标 / 来源 + 时间窗；事件取发生前 10 分钟到后 5 分钟），不再只看本地最近几条
      const live = isLive() && !!globalThis.TEAMOPS?.source;
      let loading = live, more = false, oldest = null, loadErr = null;
      const uuidOf = (id) => globalThis.TEAMOPS?.assetOf?.(id) ?? null;
      const idOfUuid = new Map(store.nodes().map((n) => [uuidOf(n.id), n.id]).filter(([u]) => u));
      async function fetchRows(before) {
        const now = store.now();
        const q = { limit: 200 };
        if (node && uuidOf(node)) q.asset = uuidOf(node);
        if (host) q.host = host;
        if (source) q.source_ip = source;
        if (at) { q.before = before ?? at + 300; q.seconds = Math.min(14 * 86400, Math.max(300, Math.ceil(now - (at - 600)))); }
        else { q.seconds = 86400; if (before) q.before = before; }
        const d = await legacyGetLive('/activity', q);
        const got = (d?.rows ?? []).filter((r) => !at || r.ts >= at - 600).map((r) => ({ ...r, node: idOfUuid.get(r.asset) ?? node ?? null }));
        more = (d?.rows?.length ?? 0) >= 200 && (!at || got.length === (d?.rows?.length ?? 0));
        oldest = got.length ? got[got.length - 1].ts : oldest;
        return got;
      }
      const statLine = () => [
        total != null ? `<b class="num">${esc(fmt.num(total))}</b> 条连接 · ${RANGE_WORD['24h']}` : null,
        live ? (loading ? '正在读取逐条记录…' : at ? `事件前后 ${rows.length} 条记录` : `${rows.length}${more ? '+' : ''} 条记录`) : `快照里有 ${rows.length} 条逐条记录`,
        x ? `来源 ${sourceLineHtml(x, { ip: source })}` : null,
      ].filter(Boolean).join(' · ');
      const evTable = () => (live && loading ? tableSkeleton(6, 5) : loadErr ? note({ tone: 'bad', title: '读取逐条记录失败', body: esc(loadErr) }) : rows.length ? dataTable({
        label: '连接记录', rowLink: false, two: true, rows: rows.slice(0, live ? rows.length : 50).map((r, i) => ({ ...r, id: String(i) })),
        columns: [
          { key: 'ts', label: '时间', cell: (r) => agoHtml(r.ts), sub: (r) => esc(fmt.stamp(r.ts, { seconds: true }).text) },
          { key: 'node', label: '节点', cell: (r) => `<span class="strong">${esc(nameOf(r.node))}</span>`, sub: (r) => esc([teamOf(r.node), nodeOf(r.node)?.relay].filter(Boolean).join(' · ')) },
          { key: 'src', label: '来源', cell: (r) => sourceCell(r.source_ip, intel(r.source_ip, r), { relayName }), sub: (r) => sourceLineHtml(intel(r.source_ip, r), { ip: r.source_ip, prefix: r.client || '未标记客户端' }) },
          { key: 'host', label: '目标', cls: 'clip', cell: (r) => esc(r.host), sub: (r) => esc(`${String(r.protocol ?? '').toUpperCase()} :${r.port} · ${r.kind === 'ip' ? '仅 IP' : '域名'}`) },
          { key: 'act', label: '结果', cell: (r) => { const w = fmt.accessAction(r.action); return statusPill({ word: w.word, tone: w.tone ?? 'ok', dot: 'solid' }); } },
        ],
      }) + (live && more ? `<div class="ev-more">${button({ label: '加载更早的记录', size: 'sm', attrs: { 'data-ev-more': '' } })}</div>` : '') : empty({ text: live ? (at ? '事件前后没有逐条记录' : '这段时间没有逐条记录') : '快照里没有这组的逐条记录（快照只带每个节点最近 10 条与全机队最近 60 条）' }));
      openDialog({
        title, sub, size: 'lg',
        body: `${message ? `<p class="dialog-text">${esc(message)}</p>` : ''}<p class="dialog-text muted" data-ev-stat>${statLine()}</p><div class="evidence-table">${evTable()}</div>`,
        foot: button({ label: '查看访问记录', icon: 'history', attrs: { 'data-dlg-act': 'activity' } }) + button({ label: '关闭', attrs: { 'data-dlg': 'cancel' } }),
        init(dlg) {
          const repaint = () => { const t = dlg.querySelector('.evidence-table'); const sl = dlg.querySelector('[data-ev-stat]'); if (!t || !dlg.isConnected) return; if (sl) sl.innerHTML = statLine(); t.innerHTML = evTable(); hydrate(t); };
          const withIntel = () => { if (isLive()) ensureIntel(store, [...rows.slice(0, 200).map((r) => r.source_ip), source], repaint); };
          if (live) {
            fetchRows().then((got) => { rows = got; loading = false; repaint(); withIntel(); }, (e) => { loading = false; loadErr = e.message; repaint(); });
            dlg.addEventListener('click', (e) => {
              const b = e.target.closest('[data-ev-more]');
              if (!b) return;
              b.disabled = true;
              fetchRows(oldest).then((got) => { rows = rows.concat(got); repaint(); withIntel(); }, () => { b.disabled = false; });
            });
          } else withIntel();
          dlg.addEventListener('click', (e) => {
            if (e.target.closest('[data-dlg-act="activity"]') && isLive()) openActivity(st.asset ? store.node(st.asset) : null);
            else if (e.target.closest('[data-dlg-act="activity"]')) toast({ message: '访问记录页不在本轮原型里（SPEC §6.7）；生产环境会带上这组筛选打开', tone: 'info' });
          });
        },
      });
    }

    // ─── 行为变化：标记已查看（L0：直接执行 + toast 撤销） ───
    function markAcked(id) {
      const ev = ex?.events_recent.find((x) => x.id === id);
      if (!ev || ev.acked) return;
      ev.acked = true;
      unacked[ev.kind] = Math.max(0, (unacked[ev.kind] ?? 1) - 1);
      patchAck(ev);
    }
    function ack(id) {
      const ev = ex?.events_recent.find((x) => x.id === id);
      if (!ev || ev.acked) return;
      ev.acked = true;
      unacked[ev.kind] = Math.max(0, (unacked[ev.kind] ?? 1) - 1);
      patchAck(ev);
      toast({
        message: `已标记已查看 · ${nameOf(ev.node)} ${EVENT_WORD[ev.kind] ?? ''}`,
        action: { label: '撤销', run: () => { ev.acked = false; unacked[ev.kind] = (unacked[ev.kind] ?? 0) + 1; patchAck(ev); } },
      });
    }
    function patchAck(ev) {
      const cell = el.querySelector(`[data-ack="${esc(ev.id)}"]`);
      if (cell) {
        const hadFocus = cell.contains(document.activeElement);
        const t = document.createElement('template');
        t.innerHTML = ackCell(ev);
        const next = t.content.firstElementChild;
        cell.replaceWith(next);
        if (hadFocus) (next.querySelector('button') ?? next.closest('tr'))?.focus({ preventScroll: true });
      }
      renderTabs();
      if (st.view === 'events') renderBar();
    }

    // ─── 访问规则：草稿、预览、丢弃 ───
    // 访问规则：没改过时“保存草稿”不可点（craft #33）
    function markDirty() {
      const save = el.querySelector('[data-act="rule-save"]');
      if (save) save.disabled = false;
      const err = el.querySelector('[data-rule-error]');
      if (err) err.hidden = true;
      el.querySelector('.rules-domains')?.removeAttribute('aria-invalid');
    }
    function readForm() {
      const f = el.querySelector('[data-rule-form]');
      return { mode: f.querySelector('[data-select="rule-mode"]').value, domains: f.querySelector('.rules-domains').value.trim(), note: f.querySelector('[name="note"]').value.trim() };
    }
    function validate({ mode, domains }) {
      const lines = domains.split(/\n+/).map((s) => s.trim()).filter(Boolean);
      if ((mode === 'allow' || mode === 'deny') && !lines.length) return `${RULE_WORD[mode]}不能为空`;
      if (lines.length > 300) return `最多 300 个域名，现在有 ${lines.length} 个`;
      const bad = lines.find((s) => /^(\d{1,3}\.){3}\d{1,3}$/.test(s.replace(/^=/, '')) || /[/:]/.test(s) || /\s/.test(s));
      if (bad) return `“${bad}”不是域名（不接受 IP、网址或端口）`;
      return '';
    }
    function saveDraft() {
      const cur = currentScope();
      const v = readForm();
      const err = validate(v);
      const errEl = el.querySelector('[data-rule-error]');
      const ta = el.querySelector('.rules-domains');
      if (err) { errEl.textContent = err; errEl.hidden = false; ta.setAttribute('aria-invalid', 'true'); ta.focus(); return; }
      if (polOn()) {
        const btn = el.querySelector('[data-act="rule-save"]');
        if (btn) { btn.disabled = true; btn.setAttribute('aria-busy', 'true'); }
        POL.saveDraft(pol.state, POL.scopeKey(cur), v).then(() => {
          toast({ message: `草稿已保存 · ${cur.name}，尚未改变业务连接` });
          loadPol({ force: true });
        }, (x) => {
          if (btn?.isConnected) { btn.disabled = false; btn.removeAttribute('aria-busy'); }
          if (!x.auth) { errEl.textContent = x.status === 409 ? `${x.message}（别人刚改过，已重新读取）` : `保存失败：${x.message}`; errEl.hidden = false; if (x.status === 409) loadPol({ force: true }); }
        });
        return;
      }
      if (v.mode === liveMode(cur) && !v.domains && !v.note) drafts.delete(cur.key); else drafts.set(cur.key, v);
      renderBody();
      toast({ message: drafts.has(cur.key) ? `草稿已保存 · ${cur.name}，尚未改变业务连接` : `${cur.name} 与线上一致，没有草稿` });
    }
    /** 实时预览：接口给每个节点应用后的模式与最近 24 小时的模拟阻止次数（旧 /policies/preview 同形）。 */
    async function previewLive() {
      let pv;
      try { pv = await POL.loadPreview(pol.state); } catch (x) { if (!x.auth) toast({ message: `读取预览失败：${x.message}`, tone: 'bad' }); return; }
      const nodes = pv?.nodes ?? [];
      const restricted = nodes.filter((x) => x.policy?.mode && x.policy.mode !== 'observe');
      const total = restricted.reduce((a, x) => a + (x.would_block?.connections ?? 0), 0);
      const canApply = !!draftCount() && isWired('behavior.rules.apply');
      const bodyHtml = draftCount()
        ? `${pv?.notice ? note({ tone: 'warn', body: esc(pv.notice) }) : ''}
           ${dataTable({ label: '预览', rowLink: false, two: false, rows: nodes, getId: (x) => String(x.asset ?? x.name), columns: [
             { key: 'name', label: '节点', cell: (r) => `<span class="strong">${esc(String(r.name ?? '').split(' · ')[0])}</span>` },
             { key: 'relay', label: '线路机', cell: (r) => esc(r.relay ?? '—') },
             { key: 'mode', label: '应用后的模式', cell: (r) => { const m = r.policy?.mode ?? 'observe'; return m !== 'observe' ? statusPill({ word: RULE_WORD[m] ?? m, tone: 'warn', dot: 'solid' }) : statusPill({ word: RULE_WORD[m], tone: 'neutral', dot: 'none' }); } },
             { key: 'sim', label: '模拟阻止', align: 'right', cell: (r) => num(fmt.num(r.would_block?.connections ?? 0)) },
           ] })}
           ${canApply ? '<label class="check"><input type="checkbox" data-preview-ok>我已核对以上范围，确认把这份配置应用到所列线路机；规则会持久保存，影响之后的新连接。</label>' : ''}`
        : empty({ icon: 'shield-check', text: '没有草稿，所有范围都与线上一致' });
      openDialog({
        title: '预览访问规则的影响', sub: `涉及 ${restricted.length} 个启用限制的节点 · 最近 24 小时有 ${fmt.num(total)} 条记录将命中阻止`, size: 'lg', body: bodyHtml,
        foot: button({ label: '返回编辑', attrs: { 'data-dlg': 'cancel' } }) + (draftCount() ? button({ label: '应用预览中的配置', variant: 'danger', attrs: { 'data-dlg-act': 'apply', disabled: true } }) : ''),
        init(dlg, api) {
          const ok = dlg.querySelector('[data-dlg-act="apply"]');
          dlg.querySelector('[data-preview-ok]')?.addEventListener('change', (e) => { ok.disabled = !e.target.checked; });
          ok?.addEventListener('click', async () => {
            if (guard('behavior.rules.apply', { node: ruleNode() })) { api.close(); return; }
            ok.disabled = true; ok.setAttribute('aria-busy', 'true');
            try {
              const out = await POL.applyDrafts(pv);
              api.close();
              loadPol({ force: true });
              await POL.trackOps(app, out?.ops, `应用访问规则${out?.version_id ? ` ${out.version_id}` : ''}`);
              loadPol({ force: true });
            } catch (x) {
              if (ok.isConnected) { ok.disabled = false; ok.removeAttribute('aria-busy'); }
              if (!x.auth) toast({ message: x.status === 409 ? `草稿已变化，请重新预览：${x.message}` : `应用失败：${x.message}`, tone: 'bad' });
            }
          });
          // 不能应用（只读构建或新后台没开放）：按钮可点，点了给同一个提示（guard）
          if (ok && !canApply) ok.disabled = false;
        },
      });
    }
    async function rollbackRules(versionId, b) {
      const v = (pol.state?.versions ?? []).find((x) => String(x.id) === String(versionId));
      if (!v) return;
      const yes = await confirmAction({ action: 'behavior.rules.rollback', title: `回滚访问规则到 ${v.id}`, sub: `发布于 ${fmt.stamp(v.ts).text}`,
        body: '线上规则换回这个版本；之后的改动不再生效（草稿不受影响）。', impact: '每台线路机经 Xray API 热更新规则，影响之后的新连接。', confirm: '回滚', danger: true, typeToConfirm: String(v.id) });
      if (!yes) return;
      b.disabled = true; b.setAttribute('aria-busy', 'true');
      try {
        const out = await POL.rollbackTo(v.id);
        loadPol({ force: true });
        await POL.trackOps(app, out?.ops, `回滚访问规则到 ${v.id}`);
        loadPol({ force: true });
      } catch (x) { if (!x.auth) toast({ message: `回滚失败：${x.message}`, tone: 'bad' }); }
      finally { if (b.isConnected) { b.disabled = false; b.removeAttribute('aria-busy'); } }
    }
    function previewRules() {
      if (polOn()) { previewLive(); return; }
      const list = [...drafts.entries()];
      const s = scopes();
      const all = [...s.teams, ...s.nodes];
      const affected = [];
      for (const [key, d] of list) {
        const sc = all.find((x) => x.key === key);
        if (!sc) continue;
        const members = sc.team ? store.nodes().filter((n) => n.team === sc.team && !n.isRelay && !drafts.has(`node:${n.id}`)) : [sc.node];
        for (const n of members) affected.push({ id: n.id, name: nameOf(n.id), relay: n.relay ?? '—', mode: d.mode === 'inherit' ? RULE_WORD.observe : RULE_WORD[d.mode], restrict: d.mode !== 'observe' && d.mode !== 'inherit' });
      }
      const restricted = affected.filter((a) => a.restrict).length;
      const body = list.length
        ? `${note({ tone: 'warn', body: `影响按最近 24 小时的连接模拟；域名被隐藏或只有 IP 的连接无法匹配白名单。${isLive() ? '接口不提供模拟阻止次数。' : '原型不计算模拟阻止次数。'}` })}
           ${dataTable({ label: '预览', rowLink: false, two: false, rows: affected.map((a) => ({ ...a })), columns: [
             { key: 'name', label: '节点', cell: (r) => `<span class="strong">${esc(r.name)}</span>` },
             { key: 'relay', label: '线路机', cell: (r) => esc(r.relay) },
             { key: 'mode', label: '应用后的模式', cell: (r) => (r.restrict ? statusPill({ word: r.mode, tone: 'warn', dot: 'solid' }) : statusPill({ word: r.mode, tone: 'neutral', dot: 'none' })) },
             { key: 'sim', label: '模拟阻止', align: 'right', cell: () => dash },
           ] })}
           <label class="check"><input type="checkbox" data-preview-ok>我已核对以上范围，确认把这份配置应用到所列线路机；规则会持久保存，影响之后的新连接。</label>`
        : empty({ icon: 'shield-check', text: '没有草稿，所有范围都与线上一致' });
      openDialog({
        title: '预览访问规则的影响', sub: `涉及 ${restricted} 台启用限制的节点 · 共 ${affected.length} 台`, size: 'lg', body,
        foot: button({ label: '返回编辑', attrs: { 'data-dlg': 'cancel' } }) + (list.length ? button({ label: '应用预览中的配置', variant: 'danger', attrs: { 'data-dlg-act': 'apply', disabled: true } }) : ''),
        init(dlg, api) {
          const ok = dlg.querySelector('[data-dlg-act="apply"]');
          dlg.querySelector('[data-preview-ok]')?.addEventListener('change', (e) => { ok.disabled = !e.target.checked; });
          ok?.addEventListener('click', () => { api.close(); if (guard('behavior.rules.apply', { node: ruleNode() })) return; toast({ message: '原型不会真的应用访问规则；生产环境会先对所有线路机试运行，再逐台应用', tone: 'info' }); });
        },
      });
    }
    async function discardDrafts() {
      const n = draftCount();
      if (!n) return;
      const ok = await confirmAction({ action: 'behavior.rules.discard', title: '丢弃全部草稿', body: `会丢弃 ${n} 份草稿，线上规则不变。`, confirm: '丢弃草稿', danger: true });
      if (!ok) return;
      if (polOn()) {
        try { await POL.discardDrafts(pol.state); toast({ message: `已丢弃 ${n} 份草稿` }); }
        catch (x) { if (!x.auth) toast({ message: x.status === 409 ? `草稿已变化，已重新读取：${x.message}` : `丢弃失败：${x.message}`, tone: 'bad' }); }
        loadPol({ force: true });
        return;
      }
      drafts.clear();
      renderBody();
      toast({ message: `已丢弃 ${n} 份草稿` });
    }

    // ─── 实时：上下文条的健康 pill、出口验证列；相对时间每 30 秒刷新 ───
    offs.push(store.on('change', (diff) => {
      if (!el.isConnected || st.mode !== 'behavior') return;
      if (st.asset && (diff?.nodes?.[st.asset] || diff?.replay !== undefined)) {
        const n = nodeOf(st.asset);
        const p = el.querySelector('[data-ctx-pill]'), s = el.querySelector('[data-ctx-stats]');
        if (p && n) { const html = statusPill(M.healthOf(store, n)); if (p.innerHTML !== html) { p.innerHTML = html; p.firstElementChild?.classList.add('is-flash'); } }
        if (s && n) { const html = ctxStats(n); if (s.innerHTML !== html) s.innerHTML = html; }
      }
      if (!diff?.nodes) return;
      // KPI：只改变化的数字
      const k = el.querySelector('[data-kpis]');
      const m = kpiModel();
      if (k) for (const key of Object.keys(m)) patchKpi(k, key, { value: m[key].value, sub: m[key].sub });
      // 节点概览：行序不变（不在用户眼前跳行，§5.3 规则 3），只补丁变化的格子
      if (st.view === 'nodes') {
        const rows = order.map(nodeRow).filter(Boolean);
        patchTable(el.querySelector('[data-table]'), { columns: tableSpec().columns, rows, getId: (r) => r.id });
      }
    }));
    const agoTimer = setInterval(() => {
      for (const s of el.querySelectorAll('[data-ago]')) { const t = fmt.ago(Number(s.dataset.ago)).text; if (s.textContent !== t) s.textContent = t; }
    }, 30000);

    // ─── 键盘：j / k 在行间移动，Enter / o 打开（§5.4） ───
    const rowsEls = () => [...el.querySelectorAll('[data-table] tr.is-link')];
    function moveRow(d) {
      const rows = rowsEls();
      if (!rows.length) return;
      const i = rows.indexOf(document.activeElement);
      const next = rows[i < 0 ? (d > 0 ? 0 : rows.length - 1) : Math.max(0, Math.min(rows.length - 1, i + d))];
      next.focus();
      next.scrollIntoView({ block: 'nearest' });
    }
    const unbind = app.keys.bindAll([
      { combo: 'j', run: () => moveRow(1), desc: '下一行', group: '节点行为', when: () => !app.drawerOpen() && st.mode === 'behavior' },
      { combo: 'k', run: () => moveRow(-1), desc: '上一行', group: '节点行为', when: () => !app.drawerOpen() && st.mode === 'behavior' },
      { combo: 'o', run: () => { const r = document.activeElement?.closest?.('tr[data-row]'); if (r) openRow(r.dataset.row); }, desc: '打开当前行（证据 / 抽屉）', group: '节点行为', when: () => !app.drawerOpen() && !!document.activeElement?.closest?.('[data-table] tr[data-row]') },
    ]);

    return {
      nodeOrder: () => [...new Set(order.filter((id) => store.node(id)))],
      onDrawer(id) {
        drawerId = id;
        for (const tr of el.querySelectorAll('[data-table] tr[data-row]')) tr.classList.toggle('is-current', st.view === 'nodes' && tr.dataset.row === id);
      },
      onDensity(v) { setSegmented(el.querySelector('[data-seg="density"]'), v); },
      update(params) {
        const prev = st;
        st = readState(params);
        const key = JSON.stringify(writeState(st));
        if (key === renderedKey) return;
        renderedKey = key;
        if (prev.mode !== st.mode) { setSegmented(el.querySelector('[data-seg="mode"]'), st.mode); renderBody(); return; }
        if (st.mode === 'rules') { renderBody(); return; }
        if (st.asset) ensureNodeSources();
        syncForm();
        if (prev.asset !== st.asset) renderCtx();
        if (prev.range !== st.range) renderNote();
        renderKpis();
        renderTabs();
        renderList();
        if (prev.page !== st.page) { const l = el.querySelector('[data-list]'); if (l && l.getBoundingClientRect().top < 0) l.scrollIntoView({ block: 'start' }); }
      },
      unmount() { offs.forEach((f) => f()); clearInterval(agoTimer); unbind(); offPol(); lockObs.disconnect(); },
    };
  },
};
