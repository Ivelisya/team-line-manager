// pages/lines.js — 线路 #/lines（SPEC §6.4；原版 /admin/lines，renders/ops-lines*.png）。
// 原版布局不变，只收拾干净：
//   页头（线路 · 鲜度 · 宽松/紧凑 · 新增线路机 · 新增家宽节点 · ↻）
//   → 控制条（线路机分段 + 跨线路机搜索，§6.4 新增）
//   → 每台线路机一块白面板：头（名称 · 带宽 · 接收/发送利用率 · 健康 ……… 面板 更新于）/ 描述行（IP · 入口数 · 节点数 · 套餐 · 到期 ……… 状态分项）
//      / 资源读数一行（CPU · 内存 · 磁盘 · 负载 · 流量上限 · TCP 连接 + 网卡速率）
//      / 入口分段（原“连接入口”嵌套面板去掉一层边框：标题 + 计数 ……… 团队 活动 状态 · 新增家宽节点 · Xray 日志；入口表贴边，可展开）
//      / 软件与数据更新详情 ……… 重启 Xray
//   → 接入任务 → 管理操作记录。
// 状态：线路机、搜索、排序、每台线路机的三个筛选写 URL（relay=r2&q=20015&sort=-rate&r2.team=团队1）；展开行不持久化。
// 实时：只补丁变化的读数与单元格（§5.3）；按速率 / 活动 / 累计排序时行序最多 5 秒变一次（FLIP），指针或焦点在表里时暂停。

import { fmt, createPatcher } from '../../../shared/index.js';
import {
  icon, esc, num, txt, on, rafBatch, storage,
  pageHead, watchStuck, freshness, segmented, onSegment, setSegmented, setSegmentCounts, hydrate,
  button, iconButton, statusPill, tonePill, neutralChip, tagChip, mark, dot, copyable, infoButton,
  meter, patchMeter, dataTable, patchTable, empty, note, skel, dlRows,
  searchBox, onSearch, setSearch, openMenu, openDialog, confirmAction, toast, logView, timeline,
  sourceCell, sourceLineHtml, intelOf, copyText,
} from '../components.js';
import { planOf, planLine, scrubPlans } from '../plans.js';
import * as M from '../node-model.js';
import { openProvisionWizard, STEP_NAMES } from '../provision.js';
import { HEAD_TOOLS } from '../page-tools.js';
import { isLive, guard, isWired } from '../live-mode.js';
import { entryOp, relayOp } from '../live-ops.js';
import { rotateEntry as rotateLive } from '../live-actions.js';

// 只读预览（实时数据）里被拦下的写操作：data-act → live-mode.js ACTIONS 的 key
const WRITES = {
  logs: 'lines.relay.logs', restart: 'lines.relay.restartXray', disable: 'lines.entry.disable', enable: 'lines.entry.enable',
  reset: 'lines.entry.reset', remark: 'lines.entry.remark', 'remark-save': 'lines.entry.remarkSave', rotate: 'lines.entry.rotate', verify: 'lines.entry.verify',
};

// ─── 常量 ───────────────────────────────────────────────────────────────────

const SORTS = { port: { label: '端口', dir: 1 }, act: { label: '活动', dir: -1 }, rate: { label: '业务速率', dir: -1 }, cum: { label: '累计用量', dir: -1 } };
const LIVE_SORTS = new Set(['act', 'rate', 'cum']);
const FILTERS = {
  team: { all: '全部团队', label: '团队' },
  act: { all: '全部活动', label: '活动', items: [['active', '有活动'], ['idle', '空闲']] },
  state: { all: '全部状态', label: '状态', items: [['enabled', '启用'], ['disabled', '停用']] },
};
const PARTS = ['agent', 'xray', 'panel', 'collector', 'config'];
const PANEL_FRESH_S = 30; // 面板超过 30 秒没更新 → “延迟”样式（原版“等待更新”）

// 审计：动作中文名（原版 actionNames，按术语表修过：更换连接凭据、撤销密钥、重置用量）；没有映射的显示原 key（--fg-3）
const ACTION_NAMES = {
  asset_bandwidth: '更新带宽规格', health_flip: '状态变化', initial_baseline_corrected: '校准初始采样', initial_health_correction: '校准初始状态',
  asset_add: '新增资产', relay_activate: '验证线路机接入', policy_draft: '保存规则草稿', policy_discard: '丢弃规则草稿',
  policy_apply_started: '开始应用规则', policy_apply_applied: '规则应用成功', policy_apply_failed_rolled_back: '应用失败并撤回', policy_apply_rollback_failed: '规则撤回失败',
  asset_edit: '修改资产', renewal: '登记续费', activity_gap: '活动采集缺口', policy_apply: '应用规则', fleet_add: '接入线路机',
  behavior_ack: '确认行为变化', incident_open: '发现故障', incident_resolved: '故障恢复', incident_ack: '确认故障', node_profile: '修改节点监控',
  supplier_verify: '供应商核对', 'apikey.create': '新建密钥', 'apikey.revoke': '撤销密钥', 'apikey.test_cleanup': '清理测试密钥',
  'panel.inbound_enable': '启用 / 停用入口', 'panel.inbound_reset': '重置入口用量', 'panel.client_reset': '重置客户端用量', 'panel.xray_restart': '重启 Xray',
  'panel.inbound_remark': '修改入口备注', 'panel.client_rotate': '更换连接凭据', 'panel.xray_logs': '查看 Xray 日志', 'panel.probe_now': '重新验证链路',
};
const FIELD_NAMES = {
  due_date: '到期日', expired_at: '到期时间', price: '价格', price_cents: '价格', currency: '币种', billing_cycle: '账期', note: '备注', name: '名称', ip: 'IP',
  team: '团队', provider: '供应商', plan: '套餐', kind: '类型', revision: '版本号', sync_pending: '待同步', capacity_bps: '带宽', check: '检查项', severity: '严重度',
  evidence: '证据', incident: '故障编号', reason: '原因', fingerprint: '主机指纹', scope: '权限', expires_days: '有效期', fields: '核对字段', backup: '备份',
  report_sha256: '报告摘要', source: '来源', bandwidth_before: '原带宽', bandwidth_after: '新带宽', tags_before: '原标签', tags_after: '新标签', key_ids: '密钥',
  keys_removed: '删除的密钥', usage_removed: '删除的用量记录', service_id: '服务号', number: '编号', relay: '线路机', enable: '启用', remark: '备注', port: '端口',
};
const AUTO_ACTORS = new Set(['system']); // “显示自动记录”关掉时隐藏：系统自动发现 / 恢复故障、采集缺口、状态翻转

// ─── 小工具 ─────────────────────────────────────────────────────────────────

const csvSet = (s) => new Set(String(s ?? '').split(',').filter(Boolean));
const numHtml = (f, cls = 'v') => `<span class="${cls}">${num(f)}</span>`;
/** 带方向箭头的字节数：↓ 307 GiB（Fmt，箭头跟数字走）。 */
function arrowBytes(v, dir) {
  const b = fmt.bytes(v);
  if (b.kind === 'missing') return b;
  const a = dir === 'down' ? '↓ ' : '↑ ';
  return new fmt.Fmt(`${a}${b.text}`, { kind: b.kind, num: b.num, unit: b.unit, sep: b.sep, prefix: a, raw: b.raw, exact: b.exact });
}
/** 延迟读数：`TCP 44 ms`，超阈值时数值带语气色（TCP 100 / 300，出口验证按节点 limits，默认 3000）。 */
function latencyHtml(label, ms, kind, limits, node) {
  const f = node ? M.latencyF(node, kind, ms) : fmt.latency(ms); // 检查失败 / 超时 → “失败” / “超时”
  const tone = f.kind === 'error' ? null : fmt.latencyTone(ms, kind, limits ?? {});
  const cls = tone === 'warn' ? 't-warn' : tone === 'bad' ? 't-bad' : '';
  return `<span class="muted">${esc(label)}</span> ${num(f, { cls })}`;
}
/** 线路机流量额度说明里的方向词统一成网卡口径（原文“只计出站（发送）”）。 */
const quotaWord = (s) => String(s ?? '').replace(/只计出站（发送）|只计出站/g, '只计发送').replace(/出站/g, '发送').replace(/入站/g, '接收');

// ─── 页面 ───────────────────────────────────────────────────────────────────

export default {
  title: '线路',
  density: true,
  mount(ctx) {
    const { el, app } = ctx;
    const store = app.store;
    const relays = store.relays();
    const patcher = createPatcher();
    const open = new Set();          // 展开的入口（不持久化，§5.6）
    const moreOpen = new Set();      // 展开了“软件与数据更新详情”的线路机
    const editing = new Set();       // 正在编辑备注的入口
    const demoJobs = [];             // 本页发起的演示任务（向导“开始接入”）
    let extra = null, extraErr = null;
    let auditState = { all: false, auto: false, actions: new Set(), actors: new Set(), openRows: new Set() };
    let jobOpen = new Set();
    let st = readState(ctx.params);
    let renderedKey = JSON.stringify(writeState(st));
    let hovering = null, lastResort = 0, resortTimer = null;
    const orders = new Map();        // 每台线路机当前表里的行序（给 FLIP 与节流判断）

    function readState(params) {
      const raw = params.get('sort') ?? 'port';
      const key = raw.replace(/^-/, '');
      const f = {};
      for (const r of relays) f[r.id] = { team: params.get(`${r.id}.team`) ?? '', act: params.get(`${r.id}.act`) ?? '', state: params.get(`${r.id}.state`) ?? '' };
      return {
        relay: relays.some((r) => r.id === params.get('relay')) ? params.get('relay') : '',
        q: params.get('q') ?? '',
        sort: SORTS[key] ? key : 'port',
        dir: SORTS[key] ? (raw.startsWith('-') ? -1 : 1) : 1,
        f,
        entry: params.get('entry') ?? '',
      };
    }
    function writeState(s) {
      const out = { relay: s.relay || null, q: s.q || null, sort: s.sort === 'port' && s.dir === 1 ? null : `${s.dir === -1 ? '-' : ''}${s.sort}` };
      for (const r of relays) for (const k of Object.keys(FILTERS)) out[`${r.id}.${k}`] = s.f[r.id]?.[k] || null;
      return out;
    }
    const commit = () => app.setParams(writeState(st), { replace: true });

    // ─── 数据选择 ───
    // 3x-ui 面板已下线（线路机 Xray 由新系统 fleet-xray 管理）：面板状态不再是故障，也不再禁用任何操作
    const retired = (r) => isLive() || !!r.panel?.retired;
    const panelOk = (r) => retired(r) || (r.panel?.ok !== false && r.status?.parts?.panel !== false);
    const LIVE_PARTS = ['agent', 'xray', 'collector'];
    const nodeOf = (e) => (e.bound_node ? store.node(e.bound_node) : null);
    const isMaster = (e) => e.kind === 'master';
    function hay(e) {
      const n = nodeOf(e);
      return [e.port, e.relay, e.id, e.bound_name, e.team, e.remark, e.egress?.expected_ip, e.egress?.actual_ip, e.outbound?.target, e.client?.label, e.tag,
        isMaster(e) ? '主节点' : '', n?.ip, n?.plan?.name].filter((x) => x != null && x !== '').join(' ').toLowerCase();
    }
    const terms = () => st.q.toLowerCase().split(/\s+/).filter(Boolean);
    const matchQ = (e) => { const t = terms(); if (!t.length) return true; const h = hay(e); return t.every((w) => h.includes(w)); };
    function matchF(r, e, f = st.f[r.id] ?? {}) {
      if (f.team && (e.team ?? '—') !== f.team) return false;
      if (f.act && (e.active ? 'active' : 'idle') !== f.act) return false;
      if (f.state && (e.enable ? 'enabled' : 'disabled') !== f.state) return false;
      return true;
    }
    function sortKey(e) {
      if (st.sort === 'act') return (e.active ? 1e9 : 0) + (e.tcp_connections ?? 0);
      if (st.sort === 'rate') return (e.rate_down_bps ?? 0) + (e.rate_up_bps ?? 0);
      if (st.sort === 'cum') return e.cumulative ? (e.cumulative.down ?? 0) + (e.cumulative.up ?? 0) : null;
      return e.port;
    }
    function rowsOf(r) {
      const list = store.entriesFor(r.id).filter((e) => matchQ(e) && matchF(r, e));
      return list.sort((a, b) => {
        const ka = sortKey(a), kb = sortKey(b);
        if (ka == null && kb != null) return 1;
        if (kb == null && ka != null) return -1;
        return ((ka ?? 0) - (kb ?? 0)) * st.dir || a.port - b.port;
      });
    }
    const visibleRelays = () => relays.filter((r) => !st.relay || st.relay === r.id);
    const sortOf = (key) => (st.sort === key ? { dir: st.dir === 1 ? 'asc' : 'desc', ariaSort: st.dir === 1 ? 'ascending' : 'descending' } : { dir: null, ariaSort: 'none' });

    // ─── 线路机头与读数 ───
    function relayNode(r) { return store.node(r.id); }
    function partsModel(r) {
      const p = r.status?.parts ?? {};
      // 分项词（T-12）：探针 在线/离线 · Xray 运行中/已停止 · 面板 可达/不可达 · 采集 正常/中断 · 配置 已核对/待核对
      return (retired(r) ? LIVE_PARTS : PARTS).map((k) => {
        const x = fmt.relayPart(k, p[k]);
        return { key: k, ...x, word: p[k] == null ? '未知' : x.word, label: k === 'agent' ? '探针' : x.label };
      });
    }
    function partsHtml(r) {
      const parts = partsModel(r);
      const bad = parts.filter((p) => p.tone === 'bad' || p.tone === 'warn');
      const xr = r.xray ?? {};
      if (retired(r)) {
        const tipLive = [
          `探针　Komari agent ${r.host?.online === false ? '离线' : '在线'}${r.host?.reported_at ? ` · ${fmt.clockTime(r.host.reported_at, { seconds: true })} 上报` : ''}`,
          `Xray　${xr.running === true ? '运行中' : xr.running === false ? '已停止' : '未知（新系统还没有这台线路机的上报）'}${xr.version ? ` · ${xr.version}` : ''}${xr.reported_at ? ` · ${fmt.clockTime(xr.reported_at, { seconds: true })} 上报` : ''}`,
          `采集　${xr.reported_at ? `新系统 relayd ${xr.online ? '正常' : '中断'}` : r.collector?.received_at ? `${fmt.clockTime(r.collector.received_at, { seconds: true })}` : '—'}`,
          '管理　由新系统管理（fleet-xray；3x-ui 面板已下线）',
        ].join('\n');
        const w = parts.map((p) => (p.tone === 'bad' || p.tone === 'warn' ? tonePill(`${p.label} ${p.word}`, p.tone) : `<span${p.word === '未知' ? ' class="t-muted"' : ''}>${esc(p.label)} ${esc(p.word)}</span>`)).join('<span class="relay-sep">·</span>');
        return `<span class="relay-parts${bad.length ? ' has-bad' : ''}" data-tip="${esc(tipLive)}">${w}<span class="relay-sep">·</span><span class="t-muted">由新系统管理</span></span>`;
      }
      const tip = [
        `探针　Komari agent ${r.host?.online === false ? '离线' : '在线'}${r.host?.reported_at ? ` · ${fmt.clockTime(r.host.reported_at, { seconds: true })} 上报` : ''}`,
        `Xray　${r.panel?.xray_running === false ? '已停止' : '运行中'} · ${r.versions?.xray ?? '—'}`,
        `面板　3x-ui ${r.versions?.panel ?? '—'} · ${r.panel?.checked_at ? `${fmt.clockTime(r.panel.checked_at, { seconds: true })} 检查` : '—'}${r.panel?.error ? ` · ${r.panel.error}` : ''}`,
        `采集　${r.collector?.received_at ? `${fmt.clockTime(r.collector.received_at, { seconds: true })} · 耗时 ${fmt.num(r.collector.duration_s, 1)} 秒` : '—'}`,
        `配置　路由哈希 ${r.config?.routing_hash ?? '—'} · 规则 ${r.config?.runtime_rules ?? '—'} 条`,
        '（北京时间，每 15 秒随面板检查更新）',
      ].join('\n');
      const words = parts.map((p) => (p.tone === 'ok'
        ? `<span>${esc(p.label)} ${esc(p.word)}</span>`
        : tonePill(`${p.label} ${p.word}`, p.tone))).join('<span class="relay-sep">·</span>');
      return `<span class="relay-parts${bad.length ? ' has-bad' : ''}" data-tip="${esc(tip)}">${words}</span>`;
    }
    function healthHtml(r) {
      const code = r.status?.code ?? 'unknown';
      const reasons = (r.status?.reasons ?? []).join('、');
      const bad = partsModel(r).filter((p) => p.tone === 'bad' || p.tone === 'warn').map((p) => `${p.label} ${p.word}`).join('、');
      const tip = reasons || bad ? `原因：${reasons || bad}` : code === 'unknown' ? '新系统还没有这台线路机的 Xray 上报' : '各分项正常';
      return statusPill(fmt.health(code), { attrs: { 'data-tip': tip } });
    }
    function panelFresh(r) {
      if (retired(r)) {
        // 新系统（fleet-edge relayd）上报的 Xray 状态；没有上报时只说明由新系统管理，不当成故障
        const t = r.xray?.reported_at;
        if (!t) return { state: 'stale', text: '由新系统管理', tip: '线路机 Xray 由新系统（fleet-xray）管理；3x-ui 面板已下线' };
        const age = store.now() - t;
        const tip = `新系统上报 ${fmt.absolute(t)}（约 30 秒一次）`;
        if (age > 180) return { state: 'stale', text: `Xray 已断开 · ${fmt.ago(t).text}`, tip };
        if (age > 60) return { state: 'delayed', text: `Xray 延迟 · ${fmt.ago(t).text}`, tip };
        return { state: 'fresh', text: `Xray 更新于 ${fmt.clockTime(t, { seconds: true })}`, tip };
      }
      const t = r.panel?.checked_at;
      if (!panelOk(r)) return { state: 'stale', text: `面板 不可达${t ? ` · ${fmt.ago(t).text}` : ''}`, tip: r.panel?.error ?? '面板不可达' };
      if (!t) return { state: 'stale', text: '面板 —', tip: '面板没有检查记录' };
      const age = store.now() - t;
      const tip = `3x-ui 面板最后检查 ${fmt.absolute(t)}（每 15 秒一次）`;
      if (age > PANEL_FRESH_S) return { state: 'delayed', text: `面板 延迟 · ${fmt.ago(t).text}`, tip };
      return { state: 'fresh', text: `面板 更新于 ${fmt.clockTime(t, { seconds: true })}`, tip };
    }
    function freshHtml(r) {
      const f = panelFresh(r);
      return `<span class="fresh relay-fresh" data-state="${f.state}" data-s="fresh" data-tip="${esc(f.tip)}" tabindex="0"><span class="dot"></span><span data-f="text">${esc(f.text)}</span></span>`;
    }
    function titleReads(r) {
      const bw = r.bandwidth ?? {};
      return `<span class="relay-read"><span>带宽</span>${numHtml(fmt.bandwidth(bw.capacity_bps), 'num-b')}</span>`
        + `<span class="relay-read" data-tip="网卡接收 / 发送占带宽的比例（线路机取主机网卡，原“下行 / 上行”）"><span>接收</span><span data-s="util-rx">${numHtml(fmt.pctUtil(bw.util_down_pct), 'num-b')}</span><span>发送</span><span data-s="util-tx">${numHtml(fmt.pctUtil(bw.util_up_pct), 'num-b')}</span></span>`;
    }
    function metaHtml(r) {
      const n = relayNode(r);
      const all = store.entriesFor(r.id);
      const p = planOf(n ?? { plan: null });
      const nodes = esc(r.node_count ?? store.nodes().filter((x) => x.relay === r.name && !x.isRelay).length);
      return `<span class="relay-meta">${copyable(r.ip)}<span class="relay-sep">·</span><span><span class="num">${all.length}</span> 个入口</span><span class="relay-sep">·</span><span><span class="num">${nodes}</span> 台节点</span><span class="relay-sep">·</span><span data-tip="${esc(planLine(p))}">${esc(p.name)}</span><span class="relay-sep">·</span><span>到期 ${n ? M.dueHtml(n) : '—'}</span></span>`;
    }
    function metricsModel(r) {
      const h = r.host;
      const n = relayNode(r);
      const out = {};
      const ms = n && h ? M.resourceMeters(n) : null;
      const stale = h?.fresh === false || h?.online === false;
      for (const key of ['cpu', 'mem', 'disk', 'load']) {
        const m = ms?.find((x) => x.key === key);
        out[key] = m ? { ...m, stale } : { key, icon: { cpu: 'cpu', mem: 'memory-stick', disk: 'hard-drive', load: 'gauge' }[key], label: { cpu: 'CPU', mem: '内存', disk: '磁盘', load: '负载' }[key], value: fmt.missing(), pct: 0, sub: '—' };
      }
      const q = r.traffic_quota;
      if (q?.limit_bytes) {
        const pct = ((q.used_bytes ?? 0) / q.limit_bytes) * 100;
        out.quota = {
          key: 'quota', icon: 'database', label: '流量上限', value: fmt.pctResource(pct), pct, tone: pct >= 100 ? 'bad' : pct >= 90 ? 'warn' : null,
          sub: `${fmt.bytes(q.used_bytes).text} / ${fmt.bytes(q.limit_bytes).text}`,
          tip: `${quotaWord(q.type_word)} · 剩余 ${fmt.bytes(Math.max(0, q.limit_bytes - (q.used_bytes ?? 0))).text}\n已用取 ${quotaWord(q.used_source)}\n每月 ${q.reset_day ?? 1} 日按 ${fmt.tzBadge(q.reset_timezone) ?? '北京'} 时间重置`,
        };
      } else out.quota = { key: 'quota', icon: 'database', label: '流量上限', value: fmt.unset(), pct: 0, sub: '—' };
      out.tcp = {
        tcp: r.tcp_connections ?? h?.tcp_conns,
        rx: fmt.rate(h?.net_rx_bps ?? r.rate?.rx_bps), tx: fmt.rate(h?.net_tx_bps ?? r.rate?.tx_bps),
      };
      return out;
    }
    function tcpReading(m) {
      const v = m.tcp.tcp == null ? '<span class="t-muted">—</span>' : `<span class="num">${esc(fmt.num(m.tcp.tcp))}</span>`;
      const sub = `<span class="nowrap">${icon('arrow-down-to-line', 12)} ${num(m.tcp.rx)}</span> <span class="nowrap">${icon('arrow-up-from-line', 12)} ${num(m.tcp.tx)}</span>`;
      return `<div class="meter is-reading" data-s="tcp" data-tip="主机 TCP 连接数（Komari agent）；下方是网卡接收 / 发送速率">
        <div class="meter-head"><span class="meter-label">${icon('activity', 13)}TCP 连接</span><span class="meter-value" data-m="v">${v}</span></div>
        <div class="meter-track"></div><div class="meter-sub" data-m="s">${sub}</div></div>`;
    }
    function metricsHtml(r) {
      const m = metricsModel(r);
      const mt = (x) => meter({ icon: x.icon, label: x.label, value: x.value, pct: x.pct, tone: x.tone, sub: esc(x.sub ?? '—'), tip: x.tip ?? '', slot: x.key, stale: x.stale });
      return `<div class="relay-metrics">${['cpu', 'mem', 'disk', 'load', 'quota'].map((k) => mt(m[k])).join('')}${tcpReading(m)}</div>`;
    }

    // ─── 入口分段 ───
    function countsHtml(r) {
      const all = store.entriesFor(r.id);
      const rows = rowsOf(r);
      const act = all.filter((e) => e.active).length;
      const dis = all.filter((e) => !e.enable).length;
      const unb = all.filter((e) => e.binding !== 'bound' || !e.bound_node).length;
      const conns = all.reduce((a, e) => a + (e.tcp_connections ?? 0), 0);
      const down = all.reduce((a, e) => a + (e.rate_down_bps ?? 0), 0), up = all.reduce((a, e) => a + (e.rate_up_bps ?? 0), 0);
      const filtered = rows.length !== all.length;
      const warnIf = (n) => (n ? ' class="t-warn"' : '');
      return `${filtered ? `<span>显示 <span class="num">${rows.length}</span> / <span class="num">${all.length}</span></span>` : `<span>共 <span class="num">${all.length}</span></span>`}`
        + `<span class="relay-sep">·</span><span${retired(r) ? ' data-tip="近 5 分钟有流量或当前有连接的入口"' : ''}>有活动 <span class="num">${act}</span></span>`
        + `<span class="relay-sep">·</span><span${warnIf(dis)}>停用 <span class="num">${dis}</span></span>`
        + `<span class="relay-sep">·</span><span${warnIf(unb)}>未绑定 <span class="num">${unb}</span></span>`
        + `<span class="relay-sep is-gap">│</span><span data-tip="${retired(r) ? '全部入口已建立的 TCP 连接之和（新系统 Xray 统计）' : '全部入口 TCP 连接之和（3x-ui 面板）'}">连接 <span class="num">${esc(fmt.num(conns))}</span></span>`
        + `<span class="relay-sep">·</span><span data-tip="全部入口的业务速率之和（Xray 入口计数，10 秒一次）">业务 ${num(fmt.rate(down, { dir: 'down' }))} ${num(fmt.rate(up, { dir: 'up' }))}</span>`;
    }
    function filterButtons(r) {
      const f = st.f[r.id] ?? {};
      // 筛选按钮与节点页同一种写法：没选时写类别名（“团队 ▾”），选了写“团队：团队1”并显示软靛蓝底（consistency #1）
      const label = (k) => {
        const v = f[k];
        if (!v) return FILTERS[k].label;
        return `${FILTERS[k].label}：${k === 'team' ? v : FILTERS[k].items.find(([x]) => x === v)?.[1] ?? v}`;
      };
      return Object.keys(FILTERS).map((k) => button({ label: label(k), caret: true, size: 'md', attrs: { 'data-act': 'filter', 'data-k': k, 'data-r': r.id, 'aria-haspopup': 'menu', 'aria-expanded': 'false', 'aria-pressed': f[k] ? 'true' : 'false', 'aria-label': f[k] ? label(k) : `${FILTERS[k].label}：全部` } })).join('');
    }
    function entryHeadHtml(r) {
      return `<div class="entry-head">
        <h3 class="entry-title">入口</h3><span class="entry-counts" data-s="counts">${countsHtml(r)}</span>
        <div class="entry-tools">${filterButtons(r)}${button({ label: '新增家宽节点', icon: 'plus', size: 'md', attrs: { 'data-act': 'add-node', 'data-r': r.id } })}${iconButton({ icon: 'logs', label: '查看 Xray 日志', attrs: { 'data-act': 'logs', 'data-r': r.id } })}</div>
      </div>`;
    }

    // 入口表的列（§6.4）
    function columns(r) {
      const showRemark = store.entriesFor(r.id).some((e) => !isMaster(e) && e.remark_differs);
      const ok = panelOk(r);
      return [
        { key: 'port', label: '端口', sortable: true, width: 120, cell: (e) => `<span class="entry-port">${esc(e.port)}</span>`, sub: (e) => esc([e.inbound_protocol, e.security].filter(Boolean).join(' · ') || '—') },
        {
          key: 'node', label: '绑定节点',
          cell: (e) => {
            if (isMaster(e)) return neutralChip('主节点', '线路机自身的主节点入口');
            const n = nodeOf(e);
            if (!n || e.binding !== 'bound') return neutralChip('未绑定');
            const h = fmt.health(n.status?.code);
            const warn = h.code !== 'ok' ? `${dot({ tone: h.tone === 'unknown' ? 'unknown' : h.tone, tip: `健康：${h.word}` })} ` : '';
            return `${warn}<button type="button" class="cell-link" data-act="node" data-id="${esc(n.id)}" data-tip="打开 ${esc(n.name)} 的详情">${esc(n.name)}</button>`;
          },
          sub: (e) => {
            if (isMaster(e)) return '线路机自身';
            const n = nodeOf(e);
            if (!n) return esc(e.remark || '—');
            return `${esc(e.team ?? n.team)} · ${esc(planOf(n).name)}`;
          },
        },
        ...(showRemark ? [{ key: 'remark', label: '备注', width: 160, cell: (e) => (isMaster(e) || !e.remark_differs ? '<span class="t-muted">—</span>' : `<span class="clip" data-tip="${esc(e.remark)}">${esc(e.remark)}</span>`) }] : []),
        { key: 'state', label: '状态', width: 76, cell: (e) => tonePill(fmt.entryWord('enable', e.enable), e.enable ? 'ok' : 'warn') },
        {
          key: 'act', label: '活动', sortable: true, width: 124,
          cell: (e) => (!ok ? '<span class="t-muted" data-tip="面板不可达，活动状态不可用">—</span>'
            : e.active ? `<span data-tip="${e.source === 'fleet_xray' ? '近 5 分钟有流量或当前有连接（新系统 Xray 统计，与速率同一窗口）' : '面板报告客户端近期有活动，不代表持续传输'}">${tonePill(fmt.entryWord('activity', true), 'ok')}</span>` : tonePill(fmt.entryWord('activity', false), 'neutral', { dot: 'ring' })),
          sub: (e) => (!ok ? '' : e.active ? `<span class="num">${esc(fmt.num(e.tcp_connections ?? 0))}</span> 个连接` : `上次 ${txt(fmt.ago(e.last_active))}`),
        },
        // 列名与节点页同一个“TCP / 出口”；主节点入口 = 线路机自身：取线路机的 TCP 心跳与出口验证（relayd），没有样本时“—”
        {
          key: 'link', label: 'TCP / 出口', align: 'right', width: 116,
          cell: (e) => { const n = nodeOf(e) ?? (isMaster(e) ? store.node(e.relay_id) : null); return latencyHtml('TCP', e.link?.tcp_ms ?? (isMaster(e) ? n?.latency?.tcp_ms : null), 'tcp', n?.limits, n); },
          sub: (e) => { const n = nodeOf(e) ?? (isMaster(e) ? store.node(e.relay_id) : null); return latencyHtml('出口', e.link?.egress_ms ?? (isMaster(e) ? n?.latency?.egress_ms : null), 'egress', n?.limits, n); },
        },
        { key: 'rate', label: '业务速率', align: 'right', sortable: true, width: 124, cell: (e) => numHtml(fmt.rate(e.rate_down_bps, { dir: 'down' })), sub: (e) => num(fmt.rate(e.rate_up_bps, { dir: 'up' })) },
        {
          key: 'cum', label: '累计用量', align: 'right', sortable: true, width: 124,
          cell: (e) => {
            const c = e.cumulative ?? {};
            const total = (c.down ?? 0) + (c.up ?? 0);
            const tip = `合计 ${fmt.bytes(total).text} · ${c.total_limit ? `限额 ${fmt.bytes(c.total_limit).text}` : '未设流量限额'}\n入口累计（重置用量后清零）`;
            return `<span class="v" data-tip="${esc(tip)}">${num(arrowBytes(c.down, 'down'))}</span>`;
          },
          sub: (e) => num(arrowBytes(e.cumulative?.up, 'up')),
        },
        {
          key: 'egress', label: '出口', width: 156,
          cell: (e) => (isMaster(e) ? `<span class="entry-ip">${esc(r.ip)}</span>` : e.egress?.expected_ip ? copyable(e.egress.expected_ip, { cls: 'entry-ip' }) : '<span class="t-muted">—</span>'),
          sub: (e) => {
            if (isMaster(e)) return '<span class="faint">线路机自身</span>';
            const g = e.egress;
            if (!g) return '<span class="faint">—</span>';
            // 出口只看连通（fleetd 0.13.3）：有出口 IP 就写出来，只作信息，不比较台账
            if (g.actual_ip) return `<span data-tip="出口检查拿到的出口 IP">${dot({ tone: 'ok' })} 出口 ${esc(g.actual_ip)}</span>`;
            return '<span class="faint">出口 —</span>';
          },
        },
        {
          key: 'ops', label: '操作', align: 'right', width: 96,
          cell: (e) => `<span class="row-actions">${iconButton({ icon: e.enable ? 'power' : 'play', label: e.enable ? '停用入口' : '启用入口', size: 'sm', cls: 'row-act', attrs: { 'data-act': e.enable ? 'disable' : 'enable', 'data-e': e.id, disabled: !ok } })}${iconButton({ icon: 'rotate-ccw', label: '重置累计用量', size: 'sm', cls: 'row-act', attrs: { 'data-act': 'reset', 'data-e': e.id, disabled: !ok } })}</span>`,
        },
      ];
    }

    // 展开行：两列明细 + 一排 sm 按钮（原版 8 项 + 出口核对、累计合计、Xray 计数、来源画像）
    function detailHtml(e) {
      const r = store.relay(e.relay_id);
      const ok = panelOk(r);
      const c = e.client ?? {};
      const ob = e.outbound ?? {};
      const n = nodeOf(e);
      const remark = editing.has(e.id)
        ? `<span class="entry-remark-edit"><input class="input input-sm" data-remark="${esc(e.id)}" value="${esc(e.remark ?? '')}" aria-label="入口备注" maxlength="100">${button({ label: '保存', variant: 'primary', size: 'sm', attrs: { 'data-act': 'remark-save', 'data-e': e.id } })}${button({ label: '取消', size: 'sm', attrs: { 'data-act': 'remark-cancel', 'data-e': e.id } })}</span>`
        : esc(scrubPlans(e.remark) || '—');
      const srcList = extra?.entries?.find((x) => x.id === e.id)?.source_ips ?? null;
      const intel = e.ipintel;
      const srcHtml = srcList == null
        ? (extraErr ? '<span class="t-muted">读取失败</span>' : '<span class="t-muted">加载中…</span>')
        : srcList.length ? `<span data-tip="${esc(srcList.slice(0, 5).join('\n'))}">${srcList.slice(0, 3).map((ip) => sourceCell(ip, intelOf(store, ip))).join(' ')}${srcList.length > 3 ? ' <span class="faint">…</span>' : ''}</span>` : '<span class="t-muted">—</span>';
      const cum = e.cumulative ?? {};
      const left = [
        { label: '入口标签', value: esc(e.tag ?? '—') },
        { label: '客户端标识', value: esc(c.label ?? '—') },
        { label: '连接凭据', value: c.credential_prefix ? copyable(c.credential_prefix, { display: `${c.credential_prefix}…`, cls: 'mono' }) : '<span class="t-muted">—</span>', tip: '只显示前 8 位；真实连接凭据（client UUID）已从原型数据删除' },
        { label: 'Reality 目标 / SNI', value: esc(e.reality ? `${e.reality.dest} / ${e.reality.sni}` : '—') },
        { label: '转发到', value: ob.protocol ? `${esc(`${ob.protocol} ${ob.target ?? ''}${ob.target_port ? `:${ob.target_port}` : ''}`)}${ob.outbound_tag ? ` <span class="faint">${esc(ob.outbound_tag)}</span>` : ''}` : '<span class="t-muted">—</span>' },
        { label: '备注', value: remark },
      ];
      const right = [
        { label: '来源 IP', value: `<span class="num">${esc(srcList?.length ?? e.source_ip_count ?? 0)}</span> 个 · ${srcHtml}` },
        { label: '来源画像（24 小时）', value: intel ? esc([intel.top_carrier_h24?.k, intel.top_city_h24?.k?.replace('·', ' '), `${intel.sources?.h24 ?? 0} 个来源`, intel.new_sources_h24 ? `新来源 ${intel.new_sources_h24}` : ''].filter(Boolean).join(' · ')) : '<span class="t-muted">—</span>' },
        { label: '最近活动', value: `<span data-x="last">${txt(fmt.ago(e.last_active))}</span>` },
        { label: '连接数', value: `<span class="num" data-x="conn">${esc(fmt.num(e.tcp_connections ?? 0))}</span>` },
        { label: '出口核对', value: e.egress ? esc(`期望 ${e.egress.expected_ip} · 实际 ${e.egress.actual_ip ?? '—'}`) : '<span class="t-muted">—</span>' },
        { label: '累计合计', value: `${num(fmt.bytes((cum.down ?? 0) + (cum.up ?? 0)))} · ${cum.total_limit ? `限额 ${esc(fmt.bytes(cum.total_limit).text)}` : '未设流量限额'}` },
        { label: 'Xray 计数', value: e.counter ? `${num(arrowBytes(e.counter.down, 'down'))} ${num(arrowBytes(e.counter.up, 'up'))}` : '<span class="t-muted">—</span>', tip: 'Xray 入口计数（自上次重启；业务口径，含监控探测）' },
      ];
      const dis = !ok;
      const why = dis ? `<span class="faint">面板不可达：${esc(r.panel?.error ?? '未知原因')}，入口操作暂不可用</span>` : n && !isMaster(e) ? `<span class="faint">${button({ label: `打开 ${n.name} 详情`, variant: 'link', size: 'sm', attrs: { 'data-act': 'node', 'data-id': n.id } })}</span>` : '<span class="faint"></span>';
      const btns = [
        e.enable
          ? button({ label: '停用', icon: 'power', size: 'sm', cls: 'is-danger', attrs: { 'data-act': 'disable', 'data-e': e.id, disabled: dis } })
          : button({ label: '启用', icon: 'play', size: 'sm', attrs: { 'data-act': 'enable', 'data-e': e.id, disabled: dis } }),
        button({ label: '重置用量', icon: 'rotate-ccw', size: 'sm', cls: 'is-danger', attrs: { 'data-act': 'reset', 'data-e': e.id, disabled: dis } }),
        button({ label: '编辑备注', icon: 'pencil', size: 'sm', attrs: { 'data-act': 'remark', 'data-e': e.id, disabled: dis || editing.has(e.id) } }),
        button({ label: '更换连接凭据', icon: 'key-round', size: 'sm', cls: 'is-danger', attrs: { 'data-act': 'rotate', 'data-e': e.id, disabled: dis || isMaster(e) } }),
        button({ label: '重新验证链路', icon: 'refresh-cw', size: 'sm', attrs: { 'data-act': 'verify', 'data-e': e.id, disabled: dis || isMaster(e) } }),
      ].join('');
      return `<div class="dl-grid">${dlRows(left)}${dlRows(right)}</div><div class="expand-actions">${why}${btns}</div>`;
    }

    function tableHtml(r, rows = rowsOf(r)) {
      orders.set(r.id, rows.map((e) => e.id));
      const cur = app.drawerNodeId?.();
      const anyF = Object.values(st.f[r.id] ?? {}).some(Boolean);
      return dataTable({
        // 固定表格布局：数字列每次刷新不再左右移 4 px（craft #5）
        columns: columns(r), rows, getId: (e) => e.id, sortOf, two: true, rowLink: true, cls: 'is-sticky-first is-fixed', label: `${r.name} 入口`,
        expand: { isOpen: (e) => open.has(e.id), render: detailHtml },
        rowAttrs: (e) => ({ class: e.bound_node && e.bound_node === cur ? 'is-current' : '', 'data-node': e.bound_node ?? '' }),
        empty: empty({ icon: 'inbox', text: anyF || st.q ? '没有符合筛选条件的入口' : '这台线路机还没有入口', action: anyF || st.q ? { label: '清除筛选', attrs: { 'data-act': 'clear-f', 'data-r': r.id } } : { label: '新增家宽节点', attrs: { 'data-act': 'add-node', 'data-r': r.id } } }),
      });
    }

    function footHtml(r) {
      const v = r.versions ?? {}, cfg = r.config ?? {}, col = r.collector ?? {}, pnl = r.panel ?? {};
      const id = `relay-more-${r.id}`;
      const left = [
        { label: '管理', value: retired(r) ? '由新系统管理（fleet-xray）' : esc(v.panel ? `3x-ui ${v.panel}` : '—') },
        { label: 'Xray 版本', value: esc(v.xray ?? '—') },
        { label: '主机探针', value: esc(v.agent ? `komari-agent ${v.agent}` : '—') },
        { label: '访问记录', value: cfg.logging ? '正在记录' : '<span class="t-warn">未开启</span>' },
        retired(r) ? null : { label: '运行规则顺序', value: cfg.valid ? '已核对' : '<span class="t-warn">待核对</span>', tip: cfg.verification ? `核对方式 ${cfg.verification}` : '' },
        { label: '模板版本', value: cfg.routing_hash ? `<span class="mono" data-tip="${esc(`运行中 ${cfg.routing_hash}\n模板 ${cfg.template_routing_hash ?? '—'}\n启动时 ${cfg.startup_routing_hash ?? '—'}\ncore ${cfg.core_id ?? '—'}`)}">${esc(cfg.routing_hash.slice(0, 12))}</span>` : '—' },
      ];
      const right = [
        { label: '运行规则', value: cfg.runtime_rules != null ? `<span class="num">${esc(cfg.runtime_rules)}</span> 条` : '—' },
        { label: '采集耗时', value: col.duration_s != null ? `<span class="num">${esc(fmt.num(col.duration_s, 1))}</span> 秒 · ${txt(fmt.ago(col.received_at))}` : '—' },
        { label: '日志积压', value: `${num(fmt.bytes(col.log_backlog_bytes))} · 日志 ${num(fmt.bytes(col.log_size_bytes))}${col.event_gap ? ` · <span class="t-warn">缺口 ${esc(col.event_gap)}</span>` : ''}` },
        retired(r) ? { label: 'Xray 上报', value: r.xray?.reported_at ? `${txt(fmt.ago(r.xray.reported_at))} · Stats API ${r.xray.api_ok === false ? '<span class="t-warn">不可达</span>' : '正常'}` : '<span class="t-muted">新系统还没有上报</span>' }
          : { label: '面板自报', value: `CPU ${num(fmt.pctResource(pnl.cpu_pct))} · 内存 ${num(fmt.bytes(pnl.mem_used))}` },
        retired(r) ? null : { label: '在线客户端', value: `<span class="num">${esc(pnl.online_clients ?? '—')}</span>` },
        { label: '链路探针', value: r.probe ? `<span class="num">${esc(r.probe.assets ?? '—')}</span> 台节点 · ${txt(fmt.ago(r.probe.updated_at))}` : '—' },
      ];
      const meta = [retired(r) ? '由新系统管理' : v.panel && `3x-ui ${v.panel}`, v.xray && `Xray ${v.xray}`, cfg.logging ? '访问记录开启' : '访问记录未开启'].filter(Boolean).join(' · ');
      return `<div class="relay-foot">
          <button type="button" class="disclosure" aria-expanded="${moreOpen.has(r.id)}" aria-controls="${esc(id)}" data-act="more" data-r="${esc(r.id)}">${icon('chevron-right', 14)}<span>软件与数据更新详情</span><span class="disclosure-meta">${esc(meta)}</span></button>
          ${button({ label: '重启 Xray', icon: 'rotate-cw', size: 'sm', cls: 'is-danger', attrs: { 'data-act': 'restart', 'data-r': r.id, disabled: !panelOk(r) } })}
        </div>
        <div class="disclosure-body relay-more" id="${esc(id)}"${moreOpen.has(r.id) ? '' : ' hidden'}><div class="dl-grid">${dlRows(left)}${dlRows(right)}</div></div>`;
    }

    function relayHtml(r) {
      const rows = rowsOf(r);
      if (st.q && !rows.length) {
        orders.set(r.id, []);
        return `<section class="panel relay is-nomatch" data-relay="${esc(r.id)}" aria-label="${esc(r.name)}"><div class="relay-nomatch"><span class="relay-name">${esc(r.name)}</span><span class="muted">无匹配入口</span></div></section>`;
      }
      const ok = panelOk(r);
      const unreachable = ok ? '' : `<div class="entry-note">${note({ tone: 'bad', title: '面板不可达', body: esc(`${r.panel?.error ?? '连接失败'}。下表为最近一次采集的入口数据，活动状态不可用。`), action: button({ label: '重试', variant: 'link', size: 'sm', attrs: { 'data-act': 'retry-panel', 'data-r': r.id } }) })}</div>`;
      return `<section class="panel relay" data-relay="${esc(r.id)}" aria-label="${esc(r.name)}">
        <header class="relay-head">
          <div class="relay-line"><h2 class="relay-name">${esc(r.name)}</h2><span data-s="pill">${healthHtml(r)}</span>${titleReads(r)}${freshHtml(r)}</div>
          <div class="relay-sub"><span data-s="meta">${metaHtml(r)}</span><span data-s="parts">${partsHtml(r)}</span></div>
        </header>
        <div data-s="metrics">${metricsHtml(r)}</div>
        <div class="relay-sec relay-entries">${entryHeadHtml(r)}${unreachable}<div data-table="${esc(r.id)}" style="--dt-min:1100px">${tableHtml(r, rows)}</div></div>
        <div class="relay-sec">${footHtml(r)}</div>
      </section>`;
    }

    // ─── 接入任务（onboarding.history 最近 5 条 + 本页发起的演示任务） ───
    function jobsList() { return [...demoJobs.slice().reverse(), ...store.onboarding().history].slice(0, 5); }
    function parseTitle(j) {
      const [name, code, ip] = String(j.title ?? '').split(' · ');
      return { name: name ?? j.title, code, ip: ip ?? j.ip };
    }
    function jobResult(j) {
      const t = parseTitle(j);
      if (j.result) return j.result;
      if (j.type === 'relay') { const r = store.relay(t.name); return `主节点入口 ${r?.ip ?? '—'}:20001；面板凭据保存在该机 /etc/team-ops/bridge.json`; }
      const n = store.node(t.name);
      const cur = n && n.ip === t.ip;
      return `入口 ${j.relay ?? '—'}:${cur ? n.relay_port ?? '—' : '—'} → 出口 ${t.ip ?? '—'}，已登记为 ${t.name}${cur ? '' : '（这次登记后来被重新上架替换）'}`;
    }
    function jobHtml(j) {
      const t = parseTitle(j);
      const s = fmt.step(j.status);
      const id = `job-${j.id}`;
      const isOpen = jobOpen.has(j.id);
      const when = fmt.stamp(j.started_at).text; // 开始时间全站一个写法“09-23 15:10”（consistency #10）
      const meta = [j.type === 'relay' ? null : j.relay, t.ip, `开始 ${when}`, j.duration_s != null ? `耗时 ${fmt.duration(j.duration_s).text}` : null, `${j.ok_steps}/${j.total_steps} 步`].filter(Boolean).join(' · ');
      const names = STEP_NAMES[j.type === 'relay' ? 'relay' : 'node'];
      const steps = (j.steps ?? []).map((x) => ({
        status: x.status, title: esc(names[x.key] ?? x.name),
        meta: x.duration_s != null ? esc(fmt.duration(x.duration_s).text) : x.status === 'running' ? '进行中' : '',
        desc: x.error ? `<span class="t-bad">${esc(x.error)}</span>` : '',
      }));
      const head = j.status === 'succeeded' ? note({ tone: 'ok', body: esc(jobResult(j)) })
        : j.status === 'running' ? note({ tone: 'info', body: `正在执行第 ${esc(Number(j.ok_steps) + 1)} 步 · ${esc(names[j.steps?.[j.ok_steps]?.key] ?? '')}` })
        : note({ tone: j.status === 'rolled_back' ? 'warn' : 'bad', title: j.status_word ?? s.word, body: esc(`${j.error ?? '未知原因'}${j.status === 'rolled_back' ? '；已按相反顺序撤回前面完成的步骤' : ''}`) });
      return `<div class="job" data-job="${esc(j.id)}">
        <button type="button" class="disclosure job-row" aria-expanded="${isOpen}" aria-controls="${esc(id)}" data-act="job" data-j="${esc(j.id)}">${icon('chevron-right', 14)}<span class="job-title">${esc(j.type === 'relay' ? '新增线路机' : '新增家宽节点')} · ${esc(t.name)}</span>${tonePill(j.status === 'running' ? '进行中' : s.word, s.tone === 'muted' ? 'neutral' : s.tone, { dot: j.status === 'running' ? 'pulse' : 'solid' })}<span class="disclosure-meta job-meta">${esc(meta)}</span>${j.demo ? tagChip('演示') : ''}</button>
        <div class="disclosure-body job-body" id="${esc(id)}"${isOpen ? '' : ' hidden'}>${head}${timeline(steps, { label: '步骤' })}</div>
      </div>`;
    }
    function jobsHtml() {
      const list = jobsList();
      if (!list.length) return '';
      return `<section class="panel" data-jobs aria-label="接入任务"><header class="panel-head"><h2 class="panel-title">接入任务</h2><span class="panel-sub">最近 ${list.length} 条</span><div class="panel-tools"><a class="btn btn-link btn-sm" href="#/onboarding">在上架流水线查看全部</a></div></header><div class="panel-body is-flush job-list">${list.map(jobHtml).join('')}</div></section>`;
    }

    // ─── 管理操作记录（extra.json 的审计，150 条） ───
    const actorWord = (a) => (a === 'system' ? '系统' : a === 'provision' ? '上架流程' : a === 'provider-reconciliation' ? '供应商核对' : /^apikey:/.test(a) ? `密钥 ${a.slice(7)}` : /^[0-9a-f]{8}-[0-9a-f-]{27}$/i.test(a) ? `${a.slice(0, 8)}…` : a);
    function targetWord(x) {
      const t = String(x.target ?? '');
      if (x.target_name) return x.target_name;
      if (t.startsWith('asset:')) return store.node(t.slice(6))?.name ?? t.slice(6);
      if (t.startsWith('relay:')) return t.slice(6);
      if (t.startsWith('team:')) return t.slice(5);
      if (t.startsWith('apikey:')) return `API 密钥 ${t.slice(7, 15)}…`;
      if (t === 'api-keys') return 'API 密钥';
      return t || '—';
    }
    function detailSummary(d) {
      if (!d || !Object.keys(d).length) return null;
      const keys = d.changed ? Object.keys(d.changed) : Object.keys(d);
      const words = [...new Set(keys.map((k) => FIELD_NAMES[k] ?? k))];
      return `${words.slice(0, 4).join(' · ')}${words.length > 4 ? ` 等 ${words.length} 项` : ''}`;
    }
    function auditRows() {
      const all = extra?.audit_recent ?? [];
      return all.filter((x) => (auditState.auto || !AUTO_ACTORS.has(x.actor))
        && (!auditState.actions.size || auditState.actions.has(x.action))
        && (!auditState.actors.size || auditState.actors.has(x.actor)));
    }
    function auditTable() {
      if (extraErr) return `<div class="panel-body">${note({ tone: 'bad', title: '管理操作记录读取失败', body: esc(extraErr), action: button({ label: '重试', variant: 'link', size: 'sm', attrs: { 'data-act': 'retry-extra' } }) })}</div>`;
      if (!extra) return `<div class="audit-skel">${skel('100%', 12)}${skel('90%', 12)}${skel('95%', 12)}</div>`;
      const rows = auditRows();
      const shown = rows.slice(0, auditState.all ? 150 : 10);
      const cols = [
        // 日志类时间：主行相对时间 + 次行精确到秒（与证据对话框同一写法，consistency #10）
        { key: 'ts', label: '时间', cell: (x) => `<span class="nowrap">${txt(fmt.ago(x.ts))}</span>`, sub: (x) => esc(fmt.stamp(x.ts, { seconds: true }).text) },
        { key: 'actor', label: '操作者', cell: (x) => `<span data-tip="${esc(x.actor)}">${esc(actorWord(x.actor))}</span>` },
        { key: 'action', label: '动作', cell: (x) => (ACTION_NAMES[x.action] ? `<span class="chip">${esc(ACTION_NAMES[x.action])}</span>` : `<span class="chip t-muted" data-tip="没有中文名的动作">${esc(x.action)}</span>`) },
        { key: 'target', label: '对象', cell: (x) => esc(targetWord(x)) },
        {
          key: 'detail', label: '详情', cls: 'audit-detail',
          cell: (x) => {
            const sum = detailSummary(x.detail);
            if (!sum) return '<span class="t-muted">—</span>';
            const isOpen = auditState.openRows.has(x.id);
            const json = scrubPlans(JSON.stringify(x.detail, null, 2));
            return `<button type="button" class="disclosure audit-toggle" aria-expanded="${isOpen}" aria-controls="audit-${esc(x.id)}" data-act="audit-row" data-a="${esc(x.id)}">${icon('chevron-right', 14)}<span class="disclosure-meta">${esc(sum)}</span></button><pre class="codeblock" id="audit-${x.id}"${isOpen ? '' : ' hidden'}>${esc(json)}</pre>`;
          },
        },
      ];
      const table = dataTable({ columns: cols, rows: shown, getId: (x) => x.id, two: true, rowLink: false, cls: 'audit-dt is-sticky-first', label: '管理操作记录', rowAttrs: (x) => ({ class: auditState.openRows.has(x.id) ? 'is-detail-open' : '' }), empty: empty({ text: '没有管理操作记录' }) });
      const hidden = (extra.audit_recent?.length ?? 0) - rows.length;
      const foot = `<div class="table-foot"><span>${auditState.all ? `显示全部 ${rows.length} 条` : `显示 ${shown.length} / ${rows.length} 条`}${hidden && !auditState.auto && !auditState.actions.size && !auditState.actors.size ? ` · 已隐藏 ${hidden} 条自动记录` : ''}</span>${rows.length > 10 ? button({ label: auditState.all ? '收起' : '查看全部', variant: 'link', size: 'sm', attrs: { 'data-act': 'audit-all' } }) : '<span></span>'}</div>`;
      return table + foot;
    }
    function auditHead() {
      const sys = store.system();
      const b = sys.backup ?? {}, c = sys.collector ?? {};
      const okB = b.integrity === 'ok';
      const menuBtn = (k, label, set) => button({ label: set.size ? `${label} · ${set.size}` : label, caret: true, size: 'md', attrs: { 'data-act': 'audit-menu', 'data-k': k, 'aria-haspopup': 'menu', 'aria-expanded': 'false', 'aria-pressed': set.size ? 'true' : 'false' } });
      return `<header class="panel-head audit-head is-wrap"><h2 class="panel-title">管理操作记录</h2>${tonePill(okB ? '备份完整' : '备份不完整', okB ? 'ok' : 'bad', { title: `${b.scope ?? ''} ${b.file ?? ''} · ${fmt.bytes(b.size_bytes).text}${b.offsite === false ? ' · 未异地' : ''}` })}<span class="panel-sub">校验 ${txt(fmt.stamp(b.verified_at))} · 采集队列 <span class="num">${esc(c.running ?? 0)}</span> 运行 · <span class="num">${esc(c.waiting ?? 0)}</span> 等待</span>
        <div class="panel-tools">${menuBtn('action', '动作', auditState.actions)}${menuBtn('actor', '操作者', auditState.actors)}<label class="switch-label"><input type="checkbox" class="switch" data-act="audit-auto"${auditState.auto ? ' checked' : ''}>显示自动记录</label></div></header>`;
    }
    function auditHtml() {
      return `<section class="panel" data-audit id="audit" aria-label="管理操作记录">${auditHead()}<div data-audit-body>${auditTable()}</div></section>`;
    }

    // ─── 页面骨架 ───
    function barHtml() {
      const counts = relays.map((r) => store.entriesFor(r.id).filter(matchQ).length);
      const items = [{ value: '', label: '全部', count: counts.reduce((a, b) => a + b, 0) }, ...relays.map((r, i) => ({ value: r.id, label: r.name, count: counts[i] }))];
      return `<div class="lines-bar">${segmented({ name: 'relay', items, value: st.relay, label: '线路机', disableZero: true })}${searchBox({ name: 'q', value: st.q, placeholder: '搜索端口、节点、IP、客户端标识', label: '跨全部线路机搜索入口' })}</div>`;
    }
    function demoHtml() {
      if (app.params().get('demo') !== '1' || isLive()) return '';
      return `<div class="demo-bar">线路机演示：${[
        ['panel', '线路机2 面板不可达'], ['xray', '线路机3 Xray 已停止'], ['collector', '线路机1 采集中断'], ['disable', '线路机1 :20012 停用'], ['clear', '恢复'],
      ].map(([k, l]) => `<button type="button" class="btn btn-link btn-sm" data-demo="${k}">${esc(l)}</button>`).join(' │ ')}</div>`;
    }
    function relaysHtml() { return visibleRelays().map(relayHtml).join(''); }

    const headTools = HEAD_TOOLS.lines({ density: app.density() });

    el.innerHTML = `<div class="page" data-page="lines">
      ${pageHead({ title: '线路', tools: headTools })}
      <div data-bar>${barHtml()}</div>
      <div data-demo>${demoHtml()}</div>
      <div class="stack" data-relays>${relaysHtml()}</div>
      <div data-jobs-wrap>${jobsHtml()}</div>
      <div data-audit-wrap>${auditHtml()}</div>
    </div>`;
    hydrate(el);
    const offs = [watchStuck(el)];

    // 懒加载 extra.json：来源 IP 列表与审计
    function loadExtra() {
      extraErr = null;
      store.load('extra').then((x) => { extra = x; if (!el.isConnected) return; renderAudit(); for (const id of open) patchExpand(id); })
        .catch((e) => { extraErr = `读取 extra.json 失败：${e.message}`; if (el.isConnected) renderAudit(); });
    }
    loadExtra();

    // ─── 重画（局部） ───
    function renderRelays() {
      const box = el.querySelector('[data-relays]');
      box.innerHTML = relaysHtml();
      hydrate(box);
      lastResort = Date.now();
    }
    function renderTable(rid, { flip = false } = {}) {
      const r = store.relay(rid);
      const host = el.querySelector(`[data-table="${CSS.escape(String(rid))}"]`);
      if (!host) { renderRelays(); return; }
      const first = flip ? new Map([...host.querySelectorAll('tr[data-row]')].map((tr) => [tr.dataset.row, tr.getBoundingClientRect().top])) : null;
      host.innerHTML = tableHtml(r);
      const counts = el.querySelector(`[data-relay="${CSS.escape(String(rid))}"] [data-s="counts"]`);
      if (counts) counts.innerHTML = countsHtml(r);
      if (first && !matchMedia('(prefers-reduced-motion: reduce)').matches) {
        for (const tr of host.querySelectorAll('tr[data-row]')) {
          const a = first.get(tr.dataset.row); if (a == null) continue;
          const dy = a - tr.getBoundingClientRect().top;
          if (dy) tr.animate([{ transform: `translateY(${dy}px)` }, { transform: 'none' }], { duration: 220, easing: 'cubic-bezier(.4,0,.2,1)' });
        }
      }
    }
    function renderBar() {
      const seg = el.querySelector('[data-seg="relay"]');
      const counts = relays.map((r) => store.entriesFor(r.id).filter(matchQ).length);
      setSegmentCounts(seg, Object.fromEntries([['', counts.reduce((a, b) => a + b, 0)], ...relays.map((r, i) => [r.id, counts[i]])]));
      setSegmented(seg, st.relay);
      setSearch(el, 'q', st.q);
    }
    function renderJobs() {
      const w = el.querySelector('[data-jobs-wrap]');
      w.innerHTML = jobsHtml();
      hydrate(w);
    }
    function renderAudit() {
      const w = el.querySelector('[data-audit-wrap]');
      if (!w) return;
      w.innerHTML = auditHtml();
      hydrate(w);
    }
    function patchExpand(id) {
      const tr = el.querySelector(`tr[data-expand="${CSS.escape(id)}"]`);
      const e = store.entry(id);
      if (!tr || !e || editing.has(id)) return;
      const html = detailHtml(e);
      const box = tr.querySelector('.expand-body');
      if (box && box.__html !== html) { box.innerHTML = html; box.__html = html; }
    }
    function toggleRow(id, force) {
      const want = force ?? !open.has(id);
      if (want) open.add(id); else { open.delete(id); editing.delete(id); }
      const e = store.entry(id);
      if (e) renderTable(e.relay_id);
      const tr = el.querySelector(`tr[data-row="${CSS.escape(id)}"]`);
      tr?.focus({ preventScroll: true });
    }

    // ─── 实时更新（§5.3） ───
    function patchRelay(r) {
      const sec = el.querySelector(`[data-relay="${CSS.escape(String(r.id))}"]:not(.is-nomatch)`);
      if (!sec) return;
      const m = metricsModel(r);
      for (const k of ['cpu', 'mem', 'disk', 'load', 'quota']) {
        const x = m[k];
        const meterEl = sec.querySelector(`.meter[data-s="${k}"]`);
        if (!meterEl) continue;
        meterEl.classList.toggle('is-stale', !!x.stale);
        patchMeter(meterEl, { value: x.value, pct: x.pct, tone: x.tone, sub: esc(x.sub ?? '—'), tip: x.tip });
      }
      const tcp = sec.querySelector('[data-s="tcp"]');
      if (tcp) {
        const t = el.ownerDocument.createElement('template');
        t.innerHTML = tcpReading(m).trim();
        const nv = t.content.querySelector('[data-m="v"]').innerHTML, ns = t.content.querySelector('[data-m="s"]').innerHTML;
        if (patcher.changed(`${r.id}:tcp:v`, nv)) tcp.querySelector('[data-m="v"]').innerHTML = nv;
        if (patcher.changed(`${r.id}:tcp:s`, ns)) tcp.querySelector('[data-m="s"]').innerHTML = ns;
      }
      const slot = (k, html) => { const s = sec.querySelector(`[data-s="${k}"]`); if (s && patcher.changed(`${r.id}:${k}`, html)) s.innerHTML = html; };
      slot('util-rx', numHtml(fmt.pctUtil(r.bandwidth?.util_down_pct), 'num-b'));
      slot('util-tx', numHtml(fmt.pctUtil(r.bandwidth?.util_up_pct), 'num-b'));
      slot('pill', healthHtml(r));
      slot('parts', partsHtml(r));
      slot('counts', countsHtml(r));
      paintFresh(r, sec);
    }
    function paintFresh(r, sec = el.querySelector(`[data-relay="${CSS.escape(String(r.id))}"]`)) {
      const f = sec?.querySelector('[data-s="fresh"]');
      if (!f) return;
      const m = panelFresh(r);
      if (f.dataset.state !== m.state) f.dataset.state = m.state;
      const t = f.querySelector('[data-f="text"]');
      if (t.textContent !== m.text) t.textContent = m.text;
      if (f.dataset.tip !== m.tip) f.dataset.tip = m.tip;
    }
    function patchEntries(r) {
      const host = el.querySelector(`[data-table="${CSS.escape(String(r.id))}"]`);
      if (!host) return true;
      const rows = rowsOf(r);
      const same = patchTable(host, { columns: columns(r), rows, getId: (e) => e.id });
      for (const id of open) if (store.entry(id)?.relay_id === r.id) patchExpand(id);
      return same;
    }
    const isBusy = (host) => hovering === host || host?.contains(document.activeElement);
    function scheduleResort() {
      if (resortTimer) return;
      const wait = Math.max(0, 5000 - (Date.now() - lastResort));
      resortTimer = setTimeout(() => {
        resortTimer = null;
        for (const r of visibleRelays()) {
          const host = el.querySelector(`[data-table="${CSS.escape(String(r.id))}"]`);
          const ids = rowsOf(r).map((e) => e.id).join();
          if (ids === (orders.get(r.id) ?? []).join()) continue;
          if (isBusy(host)) { scheduleResort(); return; }
          renderTable(r.id, { flip: true });
        }
        lastResort = Date.now();
      }, wait);
    }
    const onChange = rafBatch(() => {
      if (!el.isConnected) return;
      let reorder = false;
      for (const r of visibleRelays()) {
        patchRelay(r);
        const same = patchEntries(r);
        if (!same) {
          // 行集合变了（筛选成员变化）马上重画；只是顺序变了（按实时值排序）交给节流
          const host = el.querySelector(`[data-table="${CSS.escape(String(r.id))}"]`);
          const now = rowsOf(r).map((e) => e.id);
          const had = orders.get(r.id) ?? [];
          if (now.length !== had.length || now.some((id) => !had.includes(id))) renderTable(r.id);
          else if (LIVE_SORTS.has(st.sort)) reorder = true;
          else if (!isBusy(host)) renderTable(r.id);
        }
      }
      if (reorder) scheduleResort();
    });
    offs.push(store.on('change', onChange));
    // 实时：任务详情（步骤耗时）到了、有新任务 → 重画接入任务
    offs.push(store.on('change', (d) => { if (d?.fleet?.onboarding && el.isConnected) renderJobs(); }));
    if (isLive()) app.live.need?.('jobs');
    const tick = setInterval(() => { for (const r of visibleRelays()) paintFresh(r); }, 1000);
    // 相对时间（“上次 3 小时前”“刚刚”）每 30 秒刷新一次
    const slowTick = setInterval(() => { if (el.isConnected) for (const r of visibleRelays()) patchEntries(r); }, 30000);
    offs.push(on(el, 'pointerover', '[data-table]', (e, h) => { hovering = h; }));
    offs.push(on(el, 'pointerout', '[data-table]', (e, h) => { if (!h.contains(e.relatedTarget)) { hovering = null; } }));

    // ─── 交互 ───
    offs.push(onSegment(el, 'density', (v) => app.setDensity(v)));
    offs.push(onSegment(el, 'relay', (v) => { st.relay = v; commit(); }));
    offs.push(onSearch(el, 'q', (v) => { st.q = v; commit(); }, { delay: 200 }));
    offs.push(on(el, 'click', '[data-sort]', (e, b) => {
      const k = b.dataset.sort;
      if (!SORTS[k]) return;
      if (st.sort === k) st.dir = -st.dir; else { st.sort = k; st.dir = SORTS[k].dir; }
      commit();
    }));
    offs.push(on(el, 'click', '[data-table] tr[data-row]', (e, tr) => {
      if (e.target.closest('button, a, input, select, label, .copy')) return;
      if (getSelection()?.toString()) return;
      toggleRow(tr.dataset.row);
    }));
    offs.push(on(el, 'keydown', '[data-table] tr[data-row]', (e, tr) => {
      if (e.target !== tr) return;
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); toggleRow(tr.dataset.row); }
      else if (e.key === 'ArrowRight' && tr.getAttribute('aria-expanded') === 'false') { e.preventDefault(); toggleRow(tr.dataset.row, true); }
      else if (e.key === 'ArrowLeft' && tr.getAttribute('aria-expanded') === 'true') { e.preventDefault(); toggleRow(tr.dataset.row, false); }
    }));
    offs.push(on(el, 'keydown', '[data-remark]', (e, input) => {
      if (e.isComposing) return;
      if (e.key === 'Enter') { e.preventDefault(); saveRemark(input.dataset.remark); }
      else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); editing.delete(input.dataset.remark); patchExpandForce(input.dataset.remark); }
    }));
    offs.push(on(el, 'change', '[data-act="audit-auto"]', (e, x) => { auditState.auto = x.checked; renderAudit(); }));
    offs.push(on(el, 'click', '[data-demo]', (e, b) => demo(b.dataset.demo)));
    offs.push(on(el, 'click', '[data-act]', (e, b) => {
      const act = b.dataset.act;
      const entry = b.dataset.e ? store.entry(b.dataset.e) : null;
      const relay = b.dataset.r ? store.relay(b.dataset.r) : entry ? store.relay(entry.relay_id) : null;
      if (WRITES[act] && guard(WRITES[act], { entry, relay, store })) return;
      if (act === 'retry-panel' && isLive()) { refresh(b); return; } // 实时：重读一次数据（不是写操作）
      if (act === 'node') { e.stopPropagation(); app.openNode(b.dataset.id); }
      else if (act === 'filter') filterMenu(b, relay, b.dataset.k);
      else if (act === 'clear-f') { st.f[relay.id] = { team: '', act: '', state: '' }; st.q = ''; commit(); }
      else if (act === 'add-node') openProvisionWizard(app, 'node', { relayId: relay?.id ?? null, onJob });
      else if (act === 'add-relay') openProvisionWizard(app, 'relay', { onJob });
      else if (act === 'refresh') refresh(b);
      else if (act === 'logs') xrayLogs(relay);
      else if (act === 'restart') restartXray(relay);
      else if (act === 'retry-panel') { b.setAttribute('aria-busy', 'true'); setTimeout(() => toast({ message: `${relay.name} 面板仍不可达（原型演示）`, tone: 'bad' }), 900); }
      else if (act === 'disable') disableEntry(entry);
      else if (act === 'enable') enableEntry(entry);
      else if (act === 'reset') resetEntry(entry);
      else if (act === 'remark') { editing.add(entry.id); open.add(entry.id); patchExpandForce(entry.id); el.querySelector(`[data-remark="${CSS.escape(entry.id)}"]`)?.focus(); }
      else if (act === 'remark-save') saveRemark(entry.id);
      else if (act === 'remark-cancel') { editing.delete(entry.id); patchExpandForce(entry.id); }
      else if (act === 'rotate') rotateEntry(entry);
      else if (act === 'verify') verifyEntry(entry, b);
      else if (act === 'job') { const id = b.dataset.j; if (jobOpen.has(id)) jobOpen.delete(id); else jobOpen.add(id); }
      else if (act === 'audit-all') { auditState.all = !auditState.all; renderAudit(); }
      else if (act === 'audit-row') { const id = Number(b.dataset.a); if (auditState.openRows.has(id)) auditState.openRows.delete(id); else auditState.openRows.add(id); b.closest('tr')?.classList.toggle('is-detail-open', auditState.openRows.has(id)); }
      else if (act === 'more') { const id = b.dataset.r; if (moreOpen.has(id)) moreOpen.delete(id); else moreOpen.add(id); }
      else if (act === 'audit-menu') auditMenu(b, b.dataset.k);
      else if (act === 'retry-extra') { renderAuditLoading(); loadExtra(); }
    }));
    function renderAuditLoading() { extraErr = null; extra = null; renderAudit(); }
    function patchExpandForce(id) {
      const e = store.entry(id);
      if (!e) return;
      renderTable(e.relay_id);
    }

    function filterMenu(b, r, k) {
      // 计数按“其它筛选 + 搜索”算，不算自己这一组（与节点页 facetCounts 同一口径）
      const others = { ...(st.f[r.id] ?? {}), [k]: '' };
      const all = store.entriesFor(r.id).filter((e) => matchQ(e) && matchF(r, e, others));
      const cur = st.f[r.id]?.[k] ?? '';
      const count = (fn) => all.filter(fn).length;
      const order = store.teams().map((t) => t.name);
      const teams = [...new Set(store.entriesFor(r.id).map((e) => e.team ?? '—'))].sort((x, y) => (order.indexOf(x) + 1 || 99) - (order.indexOf(y) + 1 || 99));
      const opts = k === 'team'
        ? teams.map((t) => [t, t, count((e) => (e.team ?? '—') === t)])
        : FILTERS[k].items.map(([v, l]) => [v, l, count((e) => (k === 'act' ? (e.active ? 'active' : 'idle') : (e.enable ? 'enabled' : 'disabled')) === v)]);
      const end = (n, checked) => `<span class="num">${n}</span>${checked ? icon('check', 14) : ''}`;
      openMenu(b, {
        label: FILTERS[k].label, minWidth: 180,
        items: [{ value: '', label: FILTERS[k].all, checked: !cur, end: end(all.length, !cur) }, { sep: true }, ...opts.map(([v, l, n]) => ({ value: v, label: l, checked: cur === v, end: end(n, cur === v) }))],
        onSelect: (v) => { st.f[r.id] = { ...st.f[r.id], [k]: v }; commit(); },
      });
    }
    function auditMenu(b, k) {
      const src = (extra?.audit_recent ?? []).filter((x) => auditState.auto || !AUTO_ACTORS.has(x.actor));
      const set = k === 'action' ? auditState.actions : auditState.actors;
      const counts = new Map();
      for (const x of src) { const v = k === 'action' ? x.action : x.actor; counts.set(v, (counts.get(v) ?? 0) + 1); }
      const items = [...counts].sort((a, b) => b[1] - a[1]).map(([v, n]) => ({ value: v, label: k === 'action' ? ACTION_NAMES[v] ?? v : actorWord(v), checked: set.has(v), count: n }));
      openMenu(b, {
        label: k === 'action' ? '动作' : '操作者', multi: true, minWidth: 220,
        items: [...items, { sep: true }, { value: '__clear', label: '清除' }],
        onSelect: (v, it, checked) => {
          if (v === '__clear') set.clear();
          else if (checked) set.add(v); else set.delete(v);
          const w = el.querySelector('[data-audit-body]');
          w.innerHTML = auditTable();
          const head = el.querySelector('[data-audit] .audit-head .panel-tools');
          const btn = head?.querySelector(`[data-k="${k}"]`);
          if (btn) { btn.querySelector('span').textContent = set.size ? `${k === 'action' ? '动作' : '操作者'} · ${set.size}` : (k === 'action' ? '动作' : '操作者'); btn.setAttribute('aria-pressed', String(!!set.size)); }
        },
      });
    }

    // ─── 操作（确认分级 §3.17；原型只改内存里的快照，刷新即还原） ───
    const entryName = (e) => fmt.entryLabel(e.relay, e.port, { master: isMaster(e) }).replace(' 主节点', `:${e.port} 主节点`);
    const objName = (e) => (isMaster(e) ? `${e.relay} 主节点` : e.bound_name ?? entryName(e));
    function mirror(e) { const n = nodeOf(e); if (n?.connection) { n.connection.enable = e.enable; n.connection.active = e.active; } }
    function afterMutate(e) { renderTable(e.relay_id); const sec = el.querySelector(`[data-relay="${CSS.escape(String(e.relay_id))}"]`); if (sec) sec.querySelector('[data-s="counts"]').innerHTML = countsHtml(store.relay(e.relay_id)); }
    const live = (k) => isWired(k); // 新后台已开放、且是写构建：真实 op（live-ops.js）
    async function disableEntry(e) {
      const ok = await confirmAction({
        action: 'lines.entry.disable',
        title: `停用入口 ${fmt.entryLabel(e.relay, e.port)}`, sub: objName(e),
        body: isMaster(e) ? '线路机自身的主节点入口会立即不可用，通过它直连的用户会断开。可以随时重新启用。' : '该节点的用户会立即断开，出口 IP 不变。可以随时重新启用。',
        impact: `影响 1 个入口；当前 ${fmt.num(e.tcp_connections ?? 0)} 个连接会断开。`, confirm: '停用', danger: true,
      });
      if (!ok) return;
      if (live('lines.entry.disable')) { await entryOp(app, e, 'disable', { label: `停用 ${fmt.entryLabel(e.relay, e.port)}` }); return; }
      const prev = { enable: e.enable, active: e.active };
      e.enable = false; e.active = false; mirror(e); afterMutate(e);
      toast({ message: `已停用 ${fmt.entryLabel(e.relay, e.port)}（原型演示）`, action: { label: '撤销', run: () => { Object.assign(e, prev); mirror(e); afterMutate(e); toast({ message: '已撤销，入口已重新启用', tone: 'info' }); } } });
    }
    function enableEntry(e) {
      if (live('lines.entry.enable')) { entryOp(app, e, 'enable', { label: `启用 ${fmt.entryLabel(e.relay, e.port)}` }); return; }
      e.enable = true; mirror(e); afterMutate(e);
      toast({ message: `已启用 ${fmt.entryLabel(e.relay, e.port)} · ${objName(e)}（原型演示）`, action: { label: '撤销', run: () => { e.enable = false; e.active = false; mirror(e); afterMutate(e); } } });
    }
    async function resetEntry(e) {
      const ok = await confirmAction({
        action: 'lines.entry.reset',
        title: `重置累计用量 ${fmt.entryLabel(e.relay, e.port)}`, sub: objName(e),
        body: `入口的累计 ↓ 下行 ${fmt.bytes(e.cumulative?.down).text}、↑ 上行 ${fmt.bytes(e.cumulative?.up).text} 会清零，无法恢复。本月业务流量统计不受影响。`,
        confirm: '重置', danger: true, typeToConfirm: objName(e),
      });
      if (!ok) return;
      if (live('lines.entry.reset')) { await entryOp(app, e, 'reset', { label: `重置 ${fmt.entryLabel(e.relay, e.port)} 的累计用量` }); return; }
      e.cumulative = { ...(e.cumulative ?? {}), down: 0, up: 0 };
      afterMutate(e);
      toast({ message: `已重置 ${fmt.entryLabel(e.relay, e.port)} 的累计用量 · 已记录审计（原型演示）` });
    }
    function saveRemark(id) {
      const e = store.entry(id);
      const input = el.querySelector(`[data-remark="${CSS.escape(id)}"]`);
      if (!e || !input) return;
      const v = input.value.trim();
      if (live('lines.entry.remarkSave')) {
        if (v.length > 64) { input.setAttribute('aria-invalid', 'true'); toast({ message: '备注最多 64 个字符', tone: 'bad' }); return; }
        editing.delete(id); patchExpandForce(id);
        entryOp(app, e, 'rename', { remark: v, label: `更新备注 ${fmt.entryLabel(e.relay, e.port)}` });
        return;
      }
      e.remark = v; e.remark_differs = !isMaster(e) && v !== '' && !v.startsWith(e.bound_name ?? '\u0000');
      editing.delete(id);
      patchExpandForce(id);
      toast({ message: `已更新备注 · ${fmt.entryLabel(e.relay, e.port)}（原型演示）`, action: { label: '撤销', run: () => {} } });
    }
    async function rotateEntry(e) {
      if (live('lines.entry.rotate')) { await rotateLive(app, e, { name: objName(e), key: 'lines.entry.rotate' }); return; }
      const ok = await confirmAction({
        title: `更换连接凭据 ${fmt.entryLabel(e.relay, e.port)}`, sub: `${objName(e)} · 客户端 UUID`,
        body: '旧凭据立即失效，用户要换成新链接才能连上。', impact: '更换后把新链接发给这个节点的用户；新凭据只显示这一次。',
        confirm: '更换', danger: true, typeToConfirm: objName(e),
      });
      if (!ok) return;
      const uuid = globalThis.crypto?.randomUUID?.() ?? 'xxxxxxxx-xxxx-4xxx-8xxx-xxxxxxxxxxxx'.replace(/x/g, () => (Math.random() * 16 | 0).toString(16));
      if (e.client) e.client.credential_prefix = uuid.slice(0, 8);
      afterMutate(e);
      openDialog({
        title: '新连接凭据', sub: `${objName(e)} · ${fmt.entryLabel(e.relay, e.port)}`, size: 'sm',
        body: `${note({ tone: 'warn', body: '新凭据只显示这一次，关闭后只能看到前 8 位。' })}<div class="row">${copyable(uuid, { cls: 'mono' })}</div><p class="dialog-text faint">原型演示：随机生成，没有写入 3x-ui。</p>`,
        foot: button({ label: '复制连接凭据', icon: 'copy', attrs: { 'data-copy-uuid': '' } }) + button({ label: '完成', variant: 'primary', attrs: { 'data-dlg': 'cancel' } }),
        init(dEl) { dEl.querySelector('[data-copy-uuid]').addEventListener('click', (ev) => copyText(uuid, ev.currentTarget).then((okc) => okc && toast({ message: '已复制连接凭据 · 已记录审计' }))); },
      });
    }
    function verifyEntry(e, b) {
      b.setAttribute('aria-busy', 'true');
      const label = b.querySelector('span'); const prev = label?.textContent;
      if (label) label.textContent = '验证中';
      b.querySelector('.lucide')?.classList.add('spin');
      setTimeout(() => {
        b.removeAttribute('aria-busy'); if (label) label.textContent = prev; b.querySelector('.lucide')?.classList.remove('spin');
        const same = e.egress?.verified !== false;
        toast({ message: `${fmt.entryLabel(e.relay, e.port)} 链路验证${same ? '通过' : '未通过'}：TCP ${fmt.latency(e.link?.tcp_ms).text} · 出口 ${fmt.latency(e.link?.egress_ms).text} · 实际出口 ${e.egress?.actual_ip ?? '—'}`, tone: same ? 'ok' : 'bad' });
      }, 1400);
    }
    async function restartXray(r) {
      const n = store.entriesFor(r.id).length;
      const conns = store.entriesFor(r.id).reduce((a, e) => a + (e.tcp_connections ?? 0), 0);
      const ok = await confirmAction({
        title: `重启 ${r.name} 的 Xray`, sub: `${r.ip} · Xray ${r.versions?.xray ?? ''}`,
        body: `影响本线路机 ${n} 个入口，中断约 1–3 秒，出口 IP 不变。`, impact: `当前 ${fmt.num(conns)} 个连接会断开后自动重连。`, confirm: '重启', danger: true,
        action: 'lines.relay.restartXray',
      });
      if (ok && live('lines.relay.restartXray')) { await relayOp(app, r, 'restart_xray', { label: `重启 ${r.name} 的 Xray` }); return; }
      if (ok) toast({ message: `已重启 ${r.name} 的 Xray · 已记录审计（原型演示）` });
    }
    function xrayLogs(r) {
      const lines = logLinesFor(r);
      const body = () => `<p class="dialog-text faint">最近 ${lines.length} 行 · 原型演示：由这台线路机的最近访问记录拼成 Xray 访问日志格式</p>${logView({ lines, label: `${r.name} Xray 日志`, height: 360 })}`;
      openDialog({
        title: `Xray 日志 · ${r.name}`, sub: `${r.ip} · Xray ${r.versions?.xray ?? '—'} · ${fmt.date(store.now()).text} 北京时间`, size: 'lg', body: body(),
        foot: button({ label: '刷新', icon: 'rotate-cw', attrs: { 'data-logs-refresh': '' } }) + button({ label: '关闭', attrs: { 'data-dlg': 'cancel' } }), // 信息型对话框的“关闭”一律次按钮（consistency #5）
        init(dEl) {
          const log = dEl.querySelector('.log'); if (log) log.scrollTop = log.scrollHeight;
          dEl.querySelector('[data-logs-refresh]').addEventListener('click', (ev) => {
            const btn = ev.currentTarget; btn.querySelector('.lucide')?.classList.add('spin');
            setTimeout(() => { btn.querySelector('.lucide')?.classList.remove('spin'); const l = dEl.querySelector('.log'); if (l) l.scrollTop = l.scrollHeight; }, 700);
          });
        },
      });
    }
    function logLinesFor(r) {
      const out = [];
      for (const e of store.entriesFor(r.id)) {
        const n = nodeOf(e);
        if (!n || isMaster(e)) continue;
        for (const a of store.access(n.id)) {
          const tag = e.outbound?.outbound_tag ?? n.outbound?.outbound_tag ?? 'direct';
          // 左列 HH:MM（日志块时间列 44 宽）；正文照 Xray access log 的写法，日期在对话框副标题
          out.push({ ts: a.ts, t: fmt.clockTime(a.ts), text: `from ${a.source_ip}:${40000 + (a.ts % 20000)} ${a.action === 'blocked' ? 'rejected' : 'accepted'} ${a.protocol}:${a.host}:${a.port} [${e.tag} >> ${a.action === 'blocked' ? 'block' : tag}] email: ${a.client}`, tone: a.action === 'blocked' ? 'warn' : null });
        }
      }
      return out.sort((a, b) => a.ts - b.ts).slice(-100);
    }
    function refresh(b) {
      b.classList.add('is-busy'); b.setAttribute('aria-busy', 'true');
      if (isLive() && app.live.refresh) { // 实时：真的重读一次，转到完成为止
        const done = () => { b.classList.remove('is-busy'); b.removeAttribute('aria-busy'); if (el.isConnected) { renderRelays(); renderJobs(); } };
        app.live.refresh({ force: ['minute', 'slow'] }).then(done, done);
        return;
      }
      setTimeout(() => { b.classList.remove('is-busy'); b.removeAttribute('aria-busy'); renderRelays(); renderJobs(); }, 600);
    }
    function onJob(job) {
      if (!demoJobs.includes(job)) { demoJobs.push(job); jobOpen.add(job.id); }
      if (el.isConnected) renderJobs();
    }

    // ─── 演示状态（?demo=1）：面板不可达、Xray 停止、采集中断、入口停用 ───
    const saved = new Map();
    function snap(r) { if (!saved.has(r.id)) saved.set(r.id, JSON.parse(JSON.stringify({ status: r.status, panel: r.panel, collector: r.collector }))); }
    function demo(k) {
      const r1 = store.relay('r1'), r2 = store.relay('r2'), r3 = store.relay('r3');
      if (k === 'panel') { snap(r2); r2.panel = { ...r2.panel, ok: false, error: 'SSH 桥接超时（45 秒）' }; r2.status = { ...r2.status, code: 'degraded', parts: { ...r2.status.parts, panel: false }, reasons: ['面板不可达'] }; }
      else if (k === 'xray') { snap(r3); r3.panel = { ...r3.panel, xray_running: false }; r3.status = { ...r3.status, code: 'fail', parts: { ...r3.status.parts, xray: false }, reasons: ['Xray 已停止'] }; }
      else if (k === 'collector') { snap(r1); r1.collector = { ...r1.collector, fresh: false }; r1.status = { ...r1.status, code: 'degraded', parts: { ...r1.status.parts, collector: false }, reasons: ['采集中断'] }; }
      else if (k === 'disable') { const e = store.entry('线路机1:20012'); if (e) { e.enable = false; e.active = false; mirror(e); } }
      else if (k === 'clear') {
        for (const [id, s] of saved) Object.assign(store.relay(id), s);
        saved.clear();
        const e = store.entry('线路机1:20012'); if (e) { e.enable = true; mirror(e); }
      }
      renderRelays();
    }

    // ─── 键盘：j/k 在入口行间移动，Enter 展开，o 打开节点抽屉（§5.4） ───
    const rowEls = () => [...el.querySelectorAll('[data-table] tr[data-row]')];
    function moveFocus(d) {
      const rows = rowEls();
      if (!rows.length) return;
      const i = rows.indexOf(document.activeElement);
      const next = rows[i < 0 ? (d > 0 ? 0 : rows.length - 1) : Math.max(0, Math.min(rows.length - 1, i + d))];
      next.focus();
      next.scrollIntoView({ block: 'nearest', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' });
    }
    const inTable = () => !!document.activeElement?.closest?.('[data-table] tr[data-row]');
    offs.push(app.keys.bindAll([
      { combo: 'j', run: () => moveFocus(1), desc: '下一个入口', group: '线路', when: () => !app.drawerOpen() },
      { combo: 'k', run: () => moveFocus(-1), desc: '上一个入口', group: '线路', when: () => !app.drawerOpen() },
      { combo: 'ArrowDown', run: () => moveFocus(1), hidden: true, when: () => !app.drawerOpen() && inTable() },
      { combo: 'ArrowUp', run: () => moveFocus(-1), hidden: true, when: () => !app.drawerOpen() && inTable() },
      { combo: 'o', run: () => { const id = document.activeElement.closest('tr[data-row]')?.dataset.node; if (id) app.openNode(id); }, desc: '打开焦点所在入口的节点详情', group: '线路', when: () => !app.drawerOpen() && inTable() },
    ]) ?? (() => {}));

    // 定位入口（命令面板选中入口 → #/lines?entry=线路机2:20015）：保证它可见、展开、滚到视口里
    function focusEntry(id) {
      const e = store.entry(id);
      if (!e) return;
      if (st.relay && st.relay !== e.relay_id) st.relay = '';
      if (!matchQ(e)) st.q = '';
      if (!matchF(store.relay(e.relay_id), e)) st.f[e.relay_id] = { team: '', act: '', state: '' };
      open.add(id);
      renderedKey = JSON.stringify(writeState(st));
      app.setParams({ ...writeState(st), entry: null }, { replace: true });
      renderAll();
      requestAnimationFrame(() => {
        const tr = el.querySelector(`tr[data-row="${CSS.escape(id)}"]`);
        if (!tr) return;
        tr.scrollIntoView({ block: 'center' });
        tr.focus({ preventScroll: true });
        tr.animate([{ backgroundColor: 'var(--selected)' }, { backgroundColor: 'transparent' }], { duration: 1200, easing: 'ease-out' });
      });
    }
    if (st.entry) queueMicrotask(() => focusEntry(st.entry));

    function renderAll() {
      el.querySelector('[data-demo]').innerHTML = demoHtml();
      renderBar();
      renderRelays();
    }

    return {
      nodeOrder: () => {
        const ids = [];
        for (const r of visibleRelays()) for (const e of rowsOf(r)) if (e.bound_node && !ids.includes(e.bound_node)) ids.push(e.bound_node);
        return ids;
      },
      onDrawer(id) { for (const tr of el.querySelectorAll('[data-table] tr[data-row]')) tr.classList.toggle('is-current', !!id && tr.dataset.node === id); },
      /** ⌘⇧D 切密度时页头分段跟着动（外壳回调）。 */
      onDensity(v) { setSegmented(el.querySelector('[data-seg="density"]'), v); },
      update(params) {
        st = readState(params);
        if (st.entry) { focusEntry(st.entry); return; }
        const key = JSON.stringify(writeState(st));
        const demoOn = params.get('demo') === '1' && !isLive();
        if (key === renderedKey && demoOn === !!el.querySelector('[data-demo] .demo-bar')) return;
        renderedKey = key;
        renderAll();
      },
      unmount() {
        clearInterval(tick); clearInterval(slowTick); clearTimeout(resortTimer);
        offs.forEach((f) => { try { f(); } catch { /* 已解绑 */ } });
      },
    };
  },
};
