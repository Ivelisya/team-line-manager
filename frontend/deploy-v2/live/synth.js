// synth.js — pure builders: team-ops API responses → the prototype data files
// (snapshot.json / detail.json / extra.json / series/*.json, schema in prototype/data/README.md).
//
// Every rule here mirrors prototype/data/tools/build.py (which reads the same data straight from the
// databases), so a page cannot tell a live store from the snapshot store except by the values.
// No DOM, no fetch, no clock: callers pass `now` (server time, Unix s) and the raw responses.
//
// Units (README 通用约定): rates bit/s (Xray and Komari counters are B/s ×8), bytes as integer B,
// Unix seconds as integers, `down` = towards the user's client (Xray downlink), `up` = from it.
// Missing or not-measurable = null; a field whose only source is the offline IP databases (ipintel)
// is null throughout.

export const SCHEMA = 'team-ops-prototype-snapshot/1';
export const BJ_OFFSET = 8 * 3600; // Asia/Shanghai has no DST

export const DICT = {
  health: { ok: '正常', degraded: '降级', fail: '故障', maintenance: '维护中', unknown: '未知', na: '不适用' },
  health_modifier: { stale: '数据过期' },
  lifecycle: { onboarding: '上架中', unbound: '尚未接入', active: '在用', retiring: '下线中', retired: '已下线' },
  step: { queued: '排队', running: '进行中', succeeded: '成功', failed: '失败', rolled_back: '已回滚', skipped: '跳过', cancelled: '已取消', waiting_input: '等待输入', waiting_gate: '等待闸门', blocked: '已停止（风险）' },
  relay_parts: { agent: ['在线', '离线'], xray: ['运行中', '已停止'], panel: ['可达', '不可达'], collector: ['正常', '中断'], config: ['已核对', '待核对'] },
  entry: { enable: ['启用', '停用'], activity: ['有活动', '空闲'], binding: ['已绑定', '未绑定'] },
  access_action: { routed: '已转发', blocked: '已阻止' },
  incident: { critical: '故障', warning: '提醒', resolved: '已恢复', ack: '已知悉' },
  check: { collector: '线路管理通道', host: '主机探针', cpu: 'CPU 容量', memory: '内存容量', disk: '磁盘容量', entry: '业务入口', mapping: '固定出口绑定', tcp: '线路到上游 TCP', egress: '出口', ingress: '实际入口到出口', latency: '出口请求耗时' },
  reason: { timeout: '探测超时', auth: '入口认证失败', egress_mismatch: '出口变更', policy_blocked: '规则阻止', upstream_down: '上游不通', probe_offline: '探针离线', stale: '数据尚未更新', unstable: '链路不稳定', retrying: '出口重试中' },
  billing_cycle: { monthly: '月付', quarterly: '季付', semiannual: '半年付', yearly: '年付', biennial: '两年付', triennial: '三年付', lifetime: '长期' },
  capability: { host: '主机', proxy: '家宽代理', relay: '线路机', probe: '连通性探针', no_host: '无主机' },
  direction: { down: '下行 ↓（流向用户客户端）', up: '上行 ↑（来自用户客户端）', rx: '接收（网卡 RX）', tx: '发送（网卡 TX）' },
  provider_kind: { residential_vps: '家宽 VPS（有主机权限）', nat_vps: 'NAT 家宽 VPS', socks_proxy: '代理产品（无主机权限）', relay_vps: '线路机 VPS' },
};
const CYCLE = { 月付: 'monthly', 季付: 'quarterly', 半年付: 'semiannual', 年付: 'yearly', 两年付: 'biennial', 三年付: 'triennial', 长期: 'lifetime' };
export const TEAM_ORDER = { 团队1: 1, 团队2: 2, 线路机: 3 };
const PROVIDER_CODE = { ATTVPS: 'ATT', NodeMach: 'NM', OhnoICU: 'MX', GoMami: 'GM', 'PH-DualISP': 'PH' };
const PROVIDER_KIND = { ATTVPS: 'residential_vps', NodeMach: 'socks_proxy', OhnoICU: 'nat_vps', GoMami: 'relay_vps', 'PH-DualISP': 'socks_proxy' };
const PROVIDER_HOST_ACCESS = { ATTVPS: true, NodeMach: false, OhnoICU: true, GoMami: true, 'PH-DualISP': false };
// 没有 Komari 地区旗帜（新架构下新增的节点）时按供应商兜底
const PROVIDER_REGION = { ATTVPS: '🇺🇸', NodeMach: '🇺🇸', OhnoICU: '🇲🇽', GoMami: '🇺🇸', 'PH-DualISP': '🇵🇭' };
const REGION = { '🇺🇸': ['US', '美国'], '🇲🇽': ['MX', '墨西哥'], '🇵🇭': ['PH', '菲律宾'] };
const QUOTA_WORD = { up: '只计出站（发送）', down: '只计入站', sum: '双向合计', max: '取较大方向', min: '取较小方向' };
// Server-side text of meta.monitor_statistics.definition (the API does not return it; quoted verbatim).
const AVAILABILITY_DEFINITION = '关键检查缺失不计成功；维护样本不计入成功率与覆盖率分母；历史样本按原记录保留';
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const STRIP_N = 20;
const SPARK_N = 48;
const H5_N = 288;

// ─── small helpers ─────────────────────────────────────────────────────────────────────────────────

/**
 * Python's round(v, n) exactly (build.py uses it everywhere): correctly rounded on the float's exact
 * value, ties to even.  toFixed() is exact but breaks ties upwards, so exact ties are detected from the
 * IEEE-754 bits: v·2·10ⁿ is an odd integer ⇔ v2(mantissa) + exponent + 1 + n = 0.
 */
const F64 = new Float64Array(1), U32 = new Uint32Array(F64.buffer);
function exactTie(v, n) {
  if (v === 0) return false;
  F64[0] = Math.abs(v);
  const lo = U32[0], hi = U32[1];
  const bexp = (hi >>> 20) & 0x7ff;
  if (bexp === 0x7ff) return false;
  let mHi = hi & 0xfffff, mLo = lo, e = bexp - 1075;
  if (bexp) mHi |= 0x100000; else e = -1074;
  let tz = 0;
  if (mLo) { while (!((mLo >>> tz) & 1)) tz++; } else { tz = 32; while (tz < 53 && !((mHi >>> (tz - 32)) & 1)) tz++; }
  return tz + e + 1 + n === 0;
}
export function round(v, n = 0) {
  if (v == null || !Number.isFinite(v)) return null;
  if (Math.abs(v) >= 1e15) return v;
  if (exactTie(v, n)) {
    const p = 10 ** n, f = Math.floor(v * p);
    return (f % 2 === 0 ? f : f + 1) / p;
  }
  return Number(v.toFixed(n));
}
const nz = (v) => (v == null ? 0 : v);
const sum = (arr, f = (x) => x) => arr.reduce((s, x) => s + nz(f(x)), 0);
const pad2 = (n) => String(n).padStart(2, '0');
const isNum = (v) => typeof v === 'number' && Number.isFinite(v);

/** Beijing wall-clock parts of a Unix time. */
export function bjParts(ts) {
  const d = new Date((ts + BJ_OFFSET) * 1000);
  return { y: d.getUTCFullYear(), m: d.getUTCMonth() + 1, d: d.getUTCDate(), H: d.getUTCHours(), M: d.getUTCMinutes(), S: d.getUTCSeconds() };
}
export function bjText(ts, seconds = true) {
  if (!ts) return null;
  const p = bjParts(ts);
  return `${p.y}-${pad2(p.m)}-${pad2(p.d)} ${pad2(p.H)}:${pad2(p.M)}${seconds ? ':' + pad2(p.S) : ''}`;
}
export const bjDayStart = (ts) => Math.floor((ts + BJ_OFFSET) / 86400) * 86400 - BJ_OFFSET;
export function bjMonthStart(ts) {
  const p = bjParts(ts);
  return Date.UTC(p.y, p.m - 1, 1) / 1000 - BJ_OFFSET;
}
/** ISO 8601 (Komari, nanoseconds allowed) → Unix seconds (float), or null. */
export function parseIso(v) {
  if (!v || typeof v !== 'string') return null;
  const m = v.match(/^(\d{4}-\d{2}-\d{2})[T ](\d{2}:\d{2}:\d{2})(\.\d+)?(Z|[+-]\d{2}:?\d{2})?$/);
  if (!m) return null;
  const tz = !m[4] || m[4] === 'Z' ? 'Z' : m[4].length === 5 ? m[4].slice(0, 3) + ':' + m[4].slice(3) : m[4];
  const base = Date.parse(`${m[1]}T${m[2]}${tz}`);
  if (!Number.isFinite(base)) return null;
  return base / 1000 + (m[3] ? Number(m[3]) : 0);
}
export function tzBadge(tz) {
  const z = tz || 'Asia/Shanghai';
  if (z === 'America/Los_Angeles') return 'LA';
  if (z === 'Asia/Shanghai') return null;
  return z.split('/').pop().slice(0, 3).toUpperCase();
}
/**
 * build.py anon() applied to numbers only: floats that look like Unix times become whole seconds,
 * other non-integers keep 3 decimals.  Everything is cloned; strings are untouched (no anonymisation).
 */
export function normalize(o) {
  if (typeof o === 'number') {
    if (!Number.isFinite(o)) return null;
    if (Number.isInteger(o)) return o;
    if (o >= 1.6e9 && o <= 2.0e9) return round(o);
    return round(o, 3);
  }
  if (Array.isArray(o)) return o.map(normalize);
  if (o && typeof o === 'object') {
    const out = {};
    for (const k in o) out[k] = normalize(o[k]);
    return out;
  }
  return o;
}

export function shortName(a) { return String(a.name ?? '').split(' · ')[0]; }
export function nodeId(a) {
  const s = shortName(a);
  let m = s.match(/^团队(\d+)-(\d+)$/);
  if (m) return `t${m[1]}-${m[2]}`;
  m = s.match(/^线路机(\d+)$/);
  if (m) return `r${m[1]}`;
  return 'a-' + String(a.uuid).replace(/-/g, '').slice(0, 8);
}
function sortKey(a) {
  const s = shortName(a);
  let m = s.match(/^团队(\d+)-(\d+)$/);
  if (m) return [TEAM_ORDER[a.team] ?? 9, Number(m[2])];
  m = s.match(/^线路机(\d+)$/);
  return [TEAM_ORDER[a.team] ?? 9, m ? Number(m[1]) : 99];
}
export const sortAssets = (assets) => assets.slice().sort((x, y) => { const a = sortKey(x), b = sortKey(y); return a[0] - b[0] || a[1] - b[1]; });

// ─── time grids (same alignment as build.py) ───────────────────────────────────────────────────────

/** 5-minute business-traffic grid: complete buckets of the last 24 h (build.py T5 / N5). */
export function grid5(now) {
  const t0 = Math.floor((now - 86400) / 300) * 300 + 300;
  const n = Math.floor((Math.floor(now / 300) * 300 - t0) / 300);
  return { t0, n, step: 300 };
}
/** 5-minute host / latency grid (build.py H5_T0): 288 complete buckets ending before the current one. */
export function gridH5(now) { return { t0: Math.floor(now / 300) * 300 - 300 * H5_N, n: H5_N, step: 300 }; }

/** /history rows [{t, up, down}] → columnar 5-minute buckets on grid5(now) (bytes per bucket). */
export function trafficSeries(rows, now) {
  const g = grid5(now);
  const down = new Array(g.n).fill(null), up = new Array(g.n).fill(null);
  for (const r of rows ?? []) {
    const k = Math.floor((r.t - g.t0) / 300);
    if (k >= 0 && k < g.n) { down[k] = (down[k] ?? 0) + nz(r.down); up[k] = (up[k] ?? 0) + nz(r.up); }
  }
  return { t0: g.t0, step: 300, down, up };
}
/** 24 whole-hour buckets ending with the current (partial) hour. */
export function hourly24(rows, now) {
  const t0 = Math.floor(now / 3600) * 3600 - 23 * 3600;
  const down = new Array(24).fill(null), up = new Array(24).fill(null);
  for (const r of rows ?? []) {
    const k = Math.floor((r.t - t0) / 3600);
    if (k >= 0 && k < 24) { down[k] = (down[k] ?? 0) + nz(r.down); up[k] = (up[k] ?? 0) + nz(r.up); }
  }
  return { t0, step: 3600, down, up, last_partial: true };
}
function nodeTraffic24(rows, now) {
  if (!rows) return { last24h: null, peak_24h: null, series5m: null, hourly24: null };
  const s = trafficSeries(rows, now);
  let peak = null, best = -1;
  for (let i = 0; i < s.down.length; i++) {
    const v = nz(s.down[i]) + nz(s.up[i]);
    if (v > best) { best = v; peak = i; }
  }
  const any = s.down.some((v) => v) || s.up.some((v) => v);
  return {
    last24h: { down: sum(s.down), up: sum(s.up) },
    peak_24h: any && peak != null ? { t: s.t0 + peak * 300, down: s.down[peak], up: s.up[peak], step: 300 } : null,
    series5m: s,
    hourly24: hourly24(rows, now),
  };
}

// ─── accumulators the poller keeps (latency strips from monitor probes, host sparks from polls) ───

/** Append one monitor probe sample to a strip state {samples:[{t, ms, lost}]} (dedup by t). */
/** Newest sample of a strip state ({samples:[{t, ms, lost}]}) or null. */
export const lastSample = (state) => state?.samples?.[state.samples.length - 1] ?? null;
export function stripPush(state, t, ms, lost) {
  if (t == null || !Number.isFinite(t) || t <= 0) return false;
  const s = state.samples;
  if (s.length && Math.round(s[s.length - 1].t) === Math.round(t)) return false;
  s.push({ t, ms: lost || ms == null ? null : Math.round(ms), lost: lost ? 1 : 0 });
  if (s.length > STRIP_N) s.splice(0, s.length - STRIP_N);
  return true;
}
/** Strip in the snapshot format (20 cells, oldest first; missing leading cells are null/not lost). */
export function stripOut(state, { taskId = null, interval }) {
  const s = state?.samples ?? [];
  if (!s.length) return null;
  const pad = STRIP_N - s.length;
  const tEnd = Math.round(s[s.length - 1].t);
  const ms = [...new Array(pad).fill(null), ...s.map((x) => x.ms)];
  const lost = [...new Array(pad).fill(0), ...s.map((x) => x.lost)];
  const dt = [...new Array(pad).fill(null), ...s.map((x) => tEnd - Math.round(x.t))];
  const ok = s.filter((x) => x.ms != null && !x.lost).map((x) => x.ms).sort((a, b) => a - b);
  return {
    task_id: taskId, interval_s: interval, t_end: tEnd, dt, ms, lost,
    median_ms: ok.length ? ok[Math.floor(ok.length / 2)] : null,
    loss_pct: round((s.reduce((a, x) => a + x.lost, 0) * 100) / s.length, 1),
    window_s: s.length > 1 ? tEnd - Math.round(s[0].t) : null,
    samples: s.length, source: 'monitor',
  };
}

const SPARK_FIELDS = ['cpu_pct', 'mem_pct', 'net_rx_bps', 'net_tx_bps', 'load1', 'tcp'];
/** Host sample (overview asset.host) → spark fields. */
export function sparkSample(h) {
  if (!h) return null;
  return {
    cpu_pct: isNum(h.cpu) ? h.cpu : null,
    mem_pct: h.ram_total ? (h.ram * 100) / h.ram_total : null,
    net_rx_bps: isNum(h.net_in) ? h.net_in * 8 : null,
    net_tx_bps: isNum(h.net_out) ? h.net_out * 8 : null,
    load1: isNum(h.load) ? h.load : null,
    tcp: isNum(h.connections) ? h.connections : null,
  };
}
/** Add a sample at time t to spark state {buckets: Map(t5 → {n, sums})}. */
export function sparkPush(state, t, sample) {
  if (!sample || t == null) return;
  const b = Math.floor(t / 300) * 300;
  let e = state.buckets.get(b);
  if (!e) { e = { n: {}, s: {} }; state.buckets.set(b, e); }
  for (const f of SPARK_FIELDS) if (sample[f] != null) { e.n[f] = (e.n[f] ?? 0) + 1; e.s[f] = (e.s[f] ?? 0) + sample[f]; }
  for (const k of state.buckets.keys()) if (k < b - (SPARK_N + 2) * 300) state.buckets.delete(k);
}
/** Seed spark state from Komari records ([{time, cpu, ram, net_in, net_out, load, connections}], any resolution). */
export function sparkSeed(state, records, ramTotal) {
  for (const r of records ?? []) {
    const t = parseIso(r.time);
    if (t == null) continue;
    sparkPush(state, t, sparkSample({ ...r, ram_total: ramTotal }));
  }
}
/** 48 complete 5-minute buckets ending before now (build.py spark alignment). */
export function sparkOut(state, now) {
  if (!state) return null;
  const t0 = Math.floor(now / 300) * 300 - SPARK_N * 300;
  const out = { t0, step: 300 };
  for (const f of SPARK_FIELDS) out[f] = [];
  for (let k = 0; k < SPARK_N; k++) {
    const e = state.buckets.get(t0 + k * 300);
    for (const f of SPARK_FIELDS) {
      const v = e && e.n[f] ? e.s[f] / e.n[f] : null;
      out[f].push(v == null ? null : f === 'cpu_pct' || f === 'mem_pct' ? round(v, 1) : f === 'load1' ? round(v, 2) : Math.round(v));
    }
  }
  return out;
}

// ─── the bundle → per-node objects ─────────────────────────────────────────────────────────────────

/**
 * Index the raw responses once.  `raw` = { overview, health, history, clients, incidents, renewals,
 * behavior, activity, jobs, jobDetails, komariHosts, nodeHistory, nodeMonthStart, strips, sparks, pings }.
 */
/**
 * Relays' Xray from the new system (fleetd: /v2/api/v1/relays/{host}/xray + /hosts, fleet-edge relayd) — the only live
 * source once 3x-ui is gone.  Map relay name → { running, version, reported_at, online, byPort: Map(port → inbound) }.
 * Inbound values: cumulative bytes (cum_down = to the client), rates in B/s, established connections on the port.
 */
export function fleetXrayIndex(fleetRelays, now) {
  const out = new Map();
  for (const [name, v] of Object.entries(fleetRelays ?? {})) {
    const x = v?.xray;
    if (!x) continue;
    const proc = x.process?.xray ?? v.host?.xray ?? {};
    const byPort = new Map();
    for (const e of x.entries ?? []) {
      if (e.kind !== 'inbound' || !Number.isFinite(e.port)) continue;
      const conns = e.conns == null ? null : Number(typeof e.conns === 'object' ? e.conns.established ?? e.conns.count ?? NaN : e.conns);
      byPort.set(e.port, { cum_down: e.cum_down ?? null, cum_up: e.cum_up ?? null, rate_down: e.rate_down_bps ?? null, rate_up: e.rate_up_bps ?? null,
        window_down: e.window_down ?? 0, window_up: e.window_up ?? 0, conns: Number.isFinite(conns) ? conns : null, ts: e.ts ?? x.received_at ?? null });
    }
    out.set(name, {
      running: typeof proc.running === 'boolean' ? proc.running : null, version: proc.version ?? null,
      xui_running: x.process?.xui?.running ?? v.host?.xray?.xui_running ?? null,
      api_ok: x.process?.last_query?.ok ?? null, reported_at: x.received_at ?? null, online: !!x.online && now - (x.received_at ?? 0) < 180,
      host_online: v.host ? !!v.host.online : null, byPort,
    });
  }
  return out;
}

export function indexBundle(raw, now) {
  const ov = raw.overview ?? {};
  const assets = sortAssets(ov.assets ?? []);
  const mon = raw.health?.monitor ?? {};
  const clients = new Map((raw.clients ?? []).map((c) => [c.uuid, c]));
  const byUuid = new Map(assets.map((a) => [a.uuid, a]));
  const ID = new Map(assets.map((a) => [a.uuid, nodeId(a)]));
  const SHORT = new Map(assets.map((a) => [a.uuid, shortName(a)]));
  const lastRenewal = new Map();
  for (const r of (raw.renewals ?? []).slice().sort((a, b) => a.ts - b.ts)) lastRenewal.set(r.asset, r);
  const ssMethod = new Map();
  const jobsSorted = (raw.jobs?.jobs ?? []).slice().sort((a, b) => b.created_at - a.created_at);
  for (const j of jobsSorted) {
    const d = raw.jobDetails?.[j.id];
    const ob = d?.params?.outbound;
    if (d && d.status === 'succeeded' && ob?.method && !ssMethod.has(String(d.title).split(' · ')[0])) ssMethod.set(String(d.title).split(' · ')[0], ob.method);
  }
  const relayIps = {};
  for (const j of jobsSorted) { const d = raw.jobDetails?.[j.id]; if (d?.kind === 'relay' && d.params?.ip) relayIps[d.params.name] ??= d.params.ip; }
  const onlines = {};
  for (const [name, r] of Object.entries(ov.relays ?? {})) onlines[name] = new Set(r.panel?.onlines ?? []);
  const beh = new Map((raw.behavior?.nodes ?? []).map((x) => [x.asset, x]));
  const day0 = bjDayStart(now), month0 = bjMonthStart(now);
  return { raw, now, ov, assets, mon, clients, byUuid, ID, SHORT, lastRenewal, ssMethod, relayIps, onlines, beh, day0, month0, fx: fleetXrayIndex(raw.fleetRelays, now) };
}

function hostFresh(h, now) {
  const t = parseIso(h?.time);
  return t != null && now - t >= -30 && now - t < 120;
}
function relayFresh(r, now) { return !!r && now - (r.received_at ?? 0) >= -30 && now - (r.received_at ?? 0) < 120 && !r.error; }

function capacityOf(a) {
  const isRelay = a.team === '线路机';
  let cap = a.capacity_bps, src = 'configured';
  if (cap == null && isRelay) { cap = 1_000_000_000; src = 'relay_default'; }
  if (cap == null) {
    const m = String(a.bandwidth ?? '').match(/^\s*(\d+(?:\.\d+)?)\s*(Mbps|Gbps)\s*$/i);
    if (m) { cap = Number(m[1]) * (m[2].toLowerCase() === 'gbps' ? 1e9 : 1e6); src = 'asset_bandwidth'; }
  }
  return [cap ?? null, src];
}

const EVIDENCE_KEYS = ['rc', 'phase', 'actual_ip', 'expected_ip', 'error', 'attempt', 'limit', 'fresh', 'online', 'enable'];
function checkRow(c) {
  const ev = c.evidence ?? {};
  const evs = {};
  for (const k of EVIDENCE_KEYS) if (k in ev) evs[k] = ev[k];
  if ('expected' in ev) { evs.expected = ev.expected; evs.actual = ev.actual ?? null; }
  return { key: c.key, title: c.title, critical: c.critical, status: c.status, reason: c.reason ?? null, measured_at: c.measured_at || null, value: c.value ?? null, since: c.since ?? null, pending: c.pending ?? null, evidence: evs };
}

/** tracker30 from the monitor's 30 day strip (UTC days: the server's own `days`, see README). */
function tracker30(node, monitorSince) {
  const days = node?.days ?? [];
  const start = monitorSince ?? 0;
  const dates = [], state = [], availability = [], coverage = [], incidents = [];
  for (const d of days) {
    const d0 = Date.parse(d.date + 'T00:00:00Z') / 1000;
    const avail = d.coverage == null ? null : round(d.coverage, 3);
    let st;
    if (avail == null && d0 + 86400 <= start) st = 'none';
    else if (avail == null) st = 'unknown';
    else if (avail >= 100) st = 'ok';
    else if (avail >= 99) st = 'minor';
    else if (avail >= 95) st = 'degraded';
    else st = 'fail';
    dates.push(d.date); state.push(st); availability.push(avail); coverage.push(null); incidents.push(d.incidents ?? 0);
  }
  if (!dates.length) return null;
  const nul = () => new Array(dates.length).fill(null);
  return { start: dates[0], end: dates[dates.length - 1], dates, state, availability, coverage, ok: nul(), failed: nul(), unknown: nul(), maintenance: nul(), incidents, day_basis: 'utc' };
}
function windowsFull(w) {
  const out = {};
  for (const [k, v] of Object.entries(w ?? {})) {
    out[k] = { availability_percent: v.availability_percent ?? null, coverage_percent: v.coverage_percent ?? null, successful: v.successful_samples ?? null, failed: v.failed_samples ?? null, unknown: v.unknown_samples ?? null, missing: v.missing_samples ?? null, maintenance: v.maintenance_samples ?? null, since: v.since ?? null };
  }
  for (const k of ['1', '7', '30']) out[k] ??= { availability_percent: null, coverage_percent: null, successful: null, failed: null, unknown: null, missing: null, maintenance: null, since: null };
  return out;
}

/** Full per-node object (build.py `node` before the core/detail split). */
export function buildNodeFull(a, ix, extra = {}) {
  const { now, ov, mon, clients, onlines, day0, month0 } = ix;
  const u = a.uuid, nid = ix.ID.get(u), sname = ix.SHORT.get(u);
  const k = clients.get(u) ?? {};
  const h = a.host ?? {};
  const n = mon.nodes?.[u] ?? {};
  const isRelay = a.team === '线路机';
  const provider = a.provider ?? null;
  const hasHost = !!(h.metrics_available && (h.ram_total ?? 0) > 0);
  const caps = [];
  if (isRelay) caps.push('relay');
  if (hasHost) caps.push('host');
  if (PROVIDER_KIND[provider] === 'socks_proxy') caps.push('proxy');
  if (String(k.version ?? '').startsWith('relay-probe')) caps.push('probe');
  const regionFlag = k.region || PROVIDER_REGION[provider] || null;
  const reg = REGION[regionFlag] ?? [null, null];
  const ch0 = a.channel ?? null;
  // once the relays run fleet-xray: rate and connection count come from the new system; the cumulative bytes stay on
  // the continuous legacy ledger (x-ui.db counters kept by fleet-xui-traffic) — fleet-xray's counters restart with
  // the process (308 GiB → 117 MiB after a restart) and are never used as 累计用量.
  const fxr = ix.fx?.get(a.relay)?.byPort.get(ch0?.port ?? a.relay_port);
  const ch = fxr ? { ...(ch0 ?? {}), port: ch0?.port ?? a.relay_port, rate_down: fxr.rate_down, rate_up: fxr.rate_up, tcp_connections: fxr.conns,
    sample_at: fxr.ts, fresh: !!ix.fx.get(a.relay).online, last_active: fxr.window_down + fxr.window_up > 0 ? fxr.ts : ch0?.last_active ?? null, fleet: true } : ch0;
  const relayName = a.relay ?? (isRelay ? sname : null);
  let port = a.relay_port ?? null;
  if (isRelay && ch) port = ch.port;
  const entryLabel = isRelay ? `${relayName} 主节点` : relayName ? `${relayName}:${port}` : '直连';
  const hstamp = parseIso(h.time);
  const hFresh = hstamp != null && now - hstamp >= -30 && now - hstamp < 120;
  let status = n.status ?? 'unknown';
  const checks = (n.checks ?? []).map(checkRow);
  // A relay whose Xray the new system reports as running (fleet-edge relayd fresh): the legacy “线路管理通道”
  // (collector over the 3x-ui bridge) is retired, so a collector-only failure is not a relay fault — dashboard and
  // lines page then agree (both read this status / the relay row built from the same fleet data).
  const fxRelay = isRelay ? ix.fx?.get(a.name) : null;
  let fleetCleared = false;
  if (fxRelay?.online && fxRelay.running === true && (status === 'fail' || status === 'degraded')) {
    const other = checks.filter((c) => c.key !== 'collector' && c.status !== 'ok' && c.status !== 'na' && c.status !== 'unknown');
    if (!other.length) {
      for (const c of checks) if (c.key === 'collector') { c.status = 'na'; c.reason = null; c.retired = true; }
      status = 'ok'; fleetCleared = true;
    }
  }
  const firstFailed = checks.find((c) => c.status !== 'ok' && c.status !== 'na')?.key ?? null;
  const ck = Object.fromEntries(checks.map((c) => [c.key, c]));
  const metrics = n.metrics ?? {};
  const pr = a.probe ?? {}, pa = a.path ?? {};
  const traffic24 = nodeTraffic24(ix.raw.nodeHistory?.[u], now);
  const firstHour = ix.raw.nodeMonthStart?.[u];
  const covStart = firstHour != null ? Math.max(month0, firstHour) : ix.raw.nodeMonthStart ? null : Math.max(month0, Math.floor((ov.started_at ?? month0) / 3600) * 3600);
  let quota = null;
  // 线路机：套餐目录的出站配额 + 主机累计出站（fleetd /v2/api/v1/quota，按账期重置；十进制 GB）优先于 Komari 的 traffic_limit
  const fq = isRelay ? (ix.raw.fleetQuota?.relays ?? []).find((q) => q.id === nodeId(a) || q.relay === shortName(a)) : null;
  if (fq?.quota_bytes) {
    quota = { limit_bytes: fq.quota_bytes, type: fq.direction === 'outbound' ? 'up' : fq.direction ?? null, type_word: fq.direction === 'outbound' ? '只计出站（发送）' : QUOTA_WORD[fq.direction] ?? null,
      used_bytes: fq.used_bytes ?? null, used_source: `主机累计出站（${fq.canonical_name ?? '套餐目录'}${fq.period === 'billing_cycle' ? '，按供应商账期' : '，按月'}${fq.reset_day ? ` ${fq.reset_day} 日` : ''}重置）`,
      reset_day: fq.reset_day ?? null, reset_timezone: fq.timezone ?? null, plan: fq.canonical_name ?? null, sampled_at: fq.sampled_at ?? null };
  } else if (k.traffic_limit) {
    quota = { limit_bytes: k.traffic_limit, type: k.traffic_limit_type ?? null, type_word: QUOTA_WORD[k.traffic_limit_type] ?? null,
      used_bytes: k.traffic_limit_type === 'up' ? h.net_total_up ?? null : null, used_source: 'Komari 网卡发送累计（按账期重置）',
      reset_day: a.traffic_reset_day ?? null, reset_timezone: a.traffic_timezone ?? a.timezone ?? null };
  }
  const [capBps, capSrc] = capacityOf(a);
  let utilDown = null, utilUp = null;
  if (capBps) {
    if (isRelay) {
      if (hFresh) { utilUp = round(((h.net_out ?? 0) * 8 * 100) / capBps, 3); utilDown = round(((h.net_in ?? 0) * 8 * 100) / capBps, 3); }
    } else if (ch && ch.fresh && ch.rate_down != null) {
      utilDown = round((ch.rate_down * 8 * 100) / capBps, 3); utilUp = round(((ch.rate_up ?? 0) * 8 * 100) / capBps, 3);
    }
  }
  const bwText = a.bandwidth || (isRelay ? '1 Gbps' : null);
  let host = null;
  if (hasHost) {
    const cores = k.cpu_cores || null;
    host = {
      cpu_pct: round(h.cpu, 2), cores, cpu_model: k.cpu_name ?? null,
      mem_used: h.ram ?? null, mem_total: h.ram_total ?? null, mem_pct: h.ram_total ? round((h.ram * 100) / h.ram_total, 2) : null,
      swap_used: h.swap ?? null, swap_total: h.swap_total ?? null,
      disk_used: h.disk ?? null, disk_total: h.disk_total ?? null, disk_pct: h.disk_total ? round((h.disk * 100) / h.disk_total, 2) : null,
      load1: h.load ?? null, load5: h.load5 ?? null, load15: h.load15 ?? null,
      load5_per_core_pct: cores && h.load5 != null ? round((h.load5 * 100) / cores) : null,
      uptime_s: h.uptime ?? null, processes: h.process ?? null,
      net_rx_bps: Math.trunc((h.net_in ?? 0) * 8), net_tx_bps: Math.trunc((h.net_out ?? 0) * 8),
      net_total_rx: h.net_total_down ?? null, net_total_tx: h.net_total_up ?? null,
      tcp_conns: h.connections ?? null, udp_conns: h.connections_udp ?? null,
      reported_at: hstamp, fresh: hFresh, online: h.online ?? null,
      os: k.os ?? null, kernel: k.kernel_version ?? null, arch: k.arch ?? null, virtualization: k.virtualization ?? null,
      gpu: k.gpu_name == null || k.gpu_name === '' || k.gpu_name === 'None' ? null : k.gpu_name,
    };
  }
  const ver = String(k.version ?? '');
  const agent = {
    kind: ver.startsWith('relay-probe') ? 'relay-probe' : k.version ? 'komari-agent' : null,
    version: k.version ?? null, online: h.online ?? null, reported_at: hstamp,
    presence: pr.publishes_presence ? 'relay_probe' : hasHost ? 'agent' : 'relay_probe', host_metrics: hasHost,
  };
  const tagsRaw = String(k.tags ?? '').split(';').filter(Boolean);
  const tags = tagsRaw.filter((t) => t !== provider && t !== a.relay && t !== a.team && !/\d\s*[MG]bps/.test(t) && !t.includes('·') && !t.includes('Pulse'));
  const cycle = CYCLE[a.billing_cycle] ?? a.billing_cycle ?? null;
  const lr = ix.lastRenewal.get(u);
  const strips = ix.raw.strips?.[u];
  const cl = (ch?.clients ?? [])[0] ?? {};
  const win30 = n.windows ?? {};
  const monitorSince = win30['30']?.since ?? null;
  const beh = ix.beh.get(u);
  const node = {
    id: nid, komari_uuid: u, name: sname, full_name: a.name,
    number: a.number ?? a.name_number ?? (sname.includes('-') ? Number(sname.split('-').pop()) : null),
    team: a.team, kind: isRelay ? 'relay' : PROVIDER_KIND[provider] === 'socks_proxy' ? 'proxy' : 'residential',
    capabilities: caps, lifecycle: 'active',
    provider, provider_code: PROVIDER_CODE[provider] ?? null,
    region: { code: reg[0], name: reg[1], flag: regionFlag },
    ip: a.ip || k.ipv4 || ix.relayIps[a.name] || null, ip_version: 'v4', ipv6: k.ipv6 || null,
    relay: relayName, relay_port: port, relay_ip: a.relay_ip ?? null, entry_label: entryLabel,
    status: {
      code: status, word: DICT.health[status] ?? status,
      reason: fleetCleared ? null : n.reason ?? null, reason_word: !fleetCleared && n.reason ? DICT.reason[n.reason] ?? null : null,
      since: n.since ?? null, checked_at: n.checked_at ?? null, first_failed_check: firstFailed,
      maintenance: n.maintenance ?? false,
      summary_counts: checks.reduce((m, c) => ((m[c.status] = (m[c.status] ?? 0) + 1), m), {}),
      attention: !fleetCleared && (mon.attention ?? []).includes(u),
      ...(fleetCleared ? { source: 'fleet' } : {}),
    },
    checks,
    limits: n.limits ?? null,
    latency: {
      tcp_ms: metrics.tcp_ms ?? (isRelay ? lastSample(strips?.tcp)?.ms ?? null : null), tcp_at: ck.tcp?.measured_at || pr.tcp_at || (isRelay ? lastSample(strips?.tcp)?.t ?? null : null),
      egress_ms: metrics.egress_ms ?? (isRelay ? lastSample(strips?.egress)?.ms ?? null : null), egress_at: ck.egress?.measured_at || pr.egress_at || (isRelay ? lastSample(strips?.egress)?.t ?? null : null),
      egress_pending: metrics.egress_pending ?? null,
      path_ms: metrics.path_ms ?? null, path_at: ck.ingress?.measured_at || pa.measured_at || null,
      ingress_ms: pr.ingress_ms ?? null,
      egress_expected_ip: pr.expected_ip ?? null, egress_actual_ip: pr.actual_ip || pa.actual_ip || null,
      // 没有实际出口（超时 / curl-28 等）不是“不符”：只有两边都有值时才比较，否则 null（未知）
      // fleetd 0.13.3：出口只看连通——拿到实际出口 IP 就是通过；expected_ip 只是参考，不再比较
      egress_verified: pr.actual_ip ? true : null,
      egress_limit_ms: n.limits?.latency_ms ?? null,
      // 线路机自身：没有旧的探测任务；relayd / fleetd 对主节点端口的 TCP 心跳与经主节点入口的出口验证（probes/latest 里以
      // 线路机资产 uuid / r1 / 主机 id 出现）有样本就照节点一样画
      strip_tcp: a.tcp_task_id || (isRelay && strips?.tcp?.samples?.length) ? stripOut(strips?.tcp, { taskId: a.tcp_task_id ?? null, interval: 30 }) : null,
      strip_egress: a.egress_task_id || (isRelay && strips?.egress?.samples?.length) ? stripOut(strips?.egress, { taskId: a.egress_task_id ?? null, interval: 300 }) : null,
    },
    availability: { windows: windowsFull(win30), tracker30: tracker30(n, monitorSince), hourly48: extra.hourly48 ?? null, monitor_since: monitorSince },
    rate: ch ? {
      down_bps: ch.rate_down == null ? null : round(ch.rate_down * 8), up_bps: ch.rate_up == null ? null : round(ch.rate_up * 8),
      sample_at: ch.sample_at ?? null, fresh: !!ch.fresh, source: ch.fleet ? 'fleet_xray' : 'xray_inbound', tcp_connections: ch.tcp_connections ?? null, last_active: ch.last_active ?? null,
    } : null,
    traffic: {
      today: { down: a.daily?.down ?? 0, up: a.daily?.up ?? 0 }, month: { down: a.monthly?.down ?? 0, up: a.monthly?.up ?? 0 },
      day_start: day0, month_start: month0, coverage_start: covStart, month_partial: !!(covStart && covStart > month0 + 3600),
      last24h: traffic24.last24h, peak_24h: traffic24.peak_24h, series5m: traffic24.series5m,
      counter_reset: !!ch?.counter_reset, sample_gap: !!ch?.sample_gap, stats_valid: ch ? ch.stats_valid ?? true : null,
      cumulative: ch ? { down: ch.down ?? null, up: ch.up ?? null, source: '入口累计（连续账本）' } : null,
      quota, host_quality: a.host_traffic_quality ?? null, hourly24: traffic24.hourly24,
    },
    host,
    host_spark: hasHost ? sparkOut(ix.raw.sparks?.[u], now) : null,
    agent,
    billing: {
      price: a.price ?? null, price_cents: a.price_cents ?? null, currency: a.currency || 'USD',
      cycle, cycle_word: DICT.billing_cycle[cycle] ?? a.billing_cycle ?? null, monthly_cents: a.price_cents ?? null,
      due_date: a.due_date ?? null, due_days: a.due_days ?? null, expired_at: a.expired_at ?? null,
      billing_timezone: a.billing_timezone ?? null, tz_badge: tzBadge(a.billing_timezone), supplier_timezone: a.supplier_timezone ?? null,
      auto_renewal_supplier: a.auto_renewal_supplier ?? null, auto_renewal_komari: !!k.auto_renewal,
      renewal_reminder_days: a.renewal_reminder_days ?? null, service_id: a.service_id ?? null, plan: a.plan ?? null, provider,
      source: a.source ?? null,
      last_renewal: lr ? { ts: lr.ts, amount_cents: lr.amount_cents, old_due: lr.old_due, new_due: lr.new_due } : null,
    },
    bandwidth: { text: bwText, capacity_bps: capBps, source: capSrc, measurement: isRelay ? 'host_interface' : 'xray_inbound_payload', util_down_pct: utilDown, util_up_pct: utilUp },
    tags, tags_raw: tagsRaw,
    outbound: isRelay ? null : {
      protocol: a.protocol ?? null, target: a.target ?? null, target_port: a.target_port ?? null, outbound_tag: a.outbound_tag ?? null,
      ss_method: String(a.protocol ?? '').toLowerCase() === 'shadowsocks' ? ix.ssMethod.get(sname) ?? null : null,
    },
    connection: ch ? {
      inbound_tag: ch.tag ?? null, port: ch.port ?? null, inbound_protocol: ch.inbound_protocol ?? null, security: ch.security ?? null, reality: ch.reality ?? null,
      client_label: cl.label ?? null,
      credential_prefix: ch.client_uuid ? String(ch.client_uuid).slice(0, 8) : null,
      credential_prefix_note: '连接凭据（client UUID）前 8 位',
      source_ips: ch.source_ips ?? [], source_ip_count: (ch.source_ips ?? []).length,
      active: cl.label != null && !!onlines[ch.relay]?.has(cl.label), enable: !!ch.enable,
    } : null,
    probe_tasks: { tcp_task_id: a.tcp_task_id ?? null, egress_task_id: a.egress_task_id ?? null, ping_task_id: a.ping_task_id ?? null },
    supplier: {
      specifications: a.supplier_specifications ?? null,
      verification: a.supplier_verification ? Object.fromEntries(['status', 'verified_at', 'fields', 'changed_fields', 'due_date_precision'].map((f) => [f, a.supplier_verification[f] ?? null])) : null,
      inventory_checked_at: a.inventory_checked_at ?? null,
    },
    note: a.note || '', remark_private: k.remark || '', remark_public: k.public_remark || '',
    revision: a.revision ?? null, updated_at: a.updated_at ?? null, sync_pending: a.sync_pending ?? null,
    created_at: parseIso(k.created_at ?? null),
    behavior: beh ? { connections: beh.connections ?? null, domains: beh.domains ?? null, sources: beh.sources ?? null, blocked: beh.blocked ?? null, ip_only: beh.ip_only ?? null, last_seen: beh.last_seen ?? null } : null,
    ipintel: null,
  };
  return node;
}

const CORE_KEYS = ['id', 'name', 'full_name', 'number', 'team', 'kind', 'capabilities', 'lifecycle', 'provider', 'provider_code', 'region', 'ip', 'ip_version', 'ipv6', 'relay', 'relay_port', 'entry_label', 'limits', 'rate', 'host', 'agent', 'billing', 'bandwidth', 'tags', 'outbound', 'behavior'];
/** snapshot.json node (build.py `core`). */
export function coreNode(n) {
  const core = {};
  for (const k of CORE_KEYS) core[k] = n[k];
  core.status = { ...n.status, failed_checks: n.checks.filter((c) => c.status !== 'ok' && c.status !== 'na') };
  const lat = { ...n.latency };
  core.latency = lat;
  const tr = n.availability.tracker30;
  core.availability = {
    windows: Object.fromEntries(Object.entries(n.availability.windows).map(([k, v]) => [k, { availability_percent: v.availability_percent, coverage_percent: v.coverage_percent }])),
    tracker30: tr ? { start: tr.start, end: tr.end, state: tr.state, availability: tr.availability, coverage: tr.coverage, incidents: tr.incidents } : null,
  };
  const { series5m, ...tcore } = n.traffic;
  core.traffic = tcore;
  core.connection = n.connection ? (({ source_ips, ...c }) => c)(n.connection) : null;
  core.host_spark = n.host_spark;
  core.ipintel = null;
  return core;
}

// ─── relays and entries ────────────────────────────────────────────────────────────────────────────

export function buildEntries(ix, nodesById) {
  const { now, ov, onlines } = ix;
  const out = [];
  for (const a of ix.assets) {
    if (a.team !== '线路机') continue;
    const rname = a.name;
    const r = ov.relays?.[rname];
    if (!r) continue;
    const relayId = ix.ID.get(a.uuid);
    const fx = ix.fx?.get(rname);
    for (const i0 of r.inbounds ?? []) {
      const f = fx?.byPort.get(i0.port);
      // fleet-xray counters override the legacy (3x-ui) snapshot for rate / connections / activity / cumulative
      // rate / connections from fleet-xray; cumulative (累计用量) stays on the legacy ledger (see buildNodeFull)
      const i = f ? { ...i0, rate_down: f.rate_down, rate_up: f.rate_up, tcp_connections: f.conns, last_active: f.window_down + f.window_up > 0 ? f.ts : i0.last_active } : i0;
      // 有活动 = the same 5-minute window as the rate: traffic in the window or an established connection now
      const fxActive = f ? (f.conns ?? 0) > 0 || f.window_down + f.window_up > 0 : null;
      const bound = ix.ID.get(i.uuid) ?? null;
      const b = ix.byUuid.get(i.uuid);
      const pr = ix.byUuid.get(i.uuid)?.probe ?? null;
      const cl = (i.clients ?? [])[0] ?? {};
      out.push({
        id: `${rname}:${i.port}`, relay: rname, relay_id: relayId, port: i.port,
        kind: i.kind ?? null, inbound_protocol: i.inbound_protocol ?? null, security: i.security ?? null, reality: i.reality ?? null, tag: i.tag ?? null,
        bound_node: bound, bound_name: ix.SHORT.get(i.uuid) ?? null, team: i.team ?? null, binding: bound ? 'bound' : 'unbound',
        remark: i.remark ?? null, remark_differs: (i.remark ?? '') !== (bound ? b.name : ''),
        enable: !!i.enable, active: fxActive ?? (cl.label != null && !!onlines[rname]?.has(cl.label)), source: f ? 'fleet_xray' : 'panel',
        tcp_connections: i.tcp_connections ?? null, last_active: i.last_active ?? null,
        rate_down_bps: i.rate_down == null ? null : round(i.rate_down * 8), rate_up_bps: i.rate_up == null ? null : round(i.rate_up * 8),
        cumulative: { down: i.down ?? null, up: i.up ?? null, total_limit: i.total || null },
        counter: { down: i.counter_down ?? null, up: i.counter_up ?? null },
        counter_reset: i.counter_reset ?? null, sample_gap: i.sample_gap ?? null, stats_valid: i.stats_valid ?? null,
        outbound: i.kind === 'master' ? null : { protocol: i.protocol ?? null, target: i.target ?? null, target_port: i.target_port ?? null, outbound_tag: i.outbound_tag ?? null },
        link: pr ? { tcp_ms: pr.tcp_ms ?? null, tcp_at: pr.tcp_at ?? null, egress_ms: pr.egress_ms ?? null, egress_at: pr.egress_at ?? null, healthy: pr.healthy ?? null } : null,
        egress: pr ? { expected_ip: pr.expected_ip ?? null, actual_ip: pr.actual_ip ?? null, verified: pr.actual_ip ? true : null } : null,
        source_ips: i.source_ips ?? [],
        client: { label: cl.label ?? null, enabled: cl.enabled ?? null, quota_bytes: cl.quota_bytes || 0, expires_ms: cl.expires_ms || 0, credential_prefix: i.client_uuid ? String(i.client_uuid).slice(0, 8) : null },
        expiry_time: i.expiry_time || 0, supported: i.supported ?? null,
      });
    }
  }
  void now; void nodesById;
  return out;
}
export const coreEntry = (e) => { const { source_ips, ...rest } = e; return { ...rest, source_ip_count: (source_ips ?? []).length, ipintel: null }; };

export function buildRelays(ix, nodesById, entries) {
  const { now, ov, clients } = ix;
  const out = [];
  for (const a of ix.assets) {
    if (a.team !== '线路机') continue;
    const rname = a.name;
    const r = ov.relays?.[rname] ?? {};
    const node = nodesById.get(ix.ID.get(a.uuid));
    const panel = r.panel ?? {};
    const fresh = relayFresh(r, now);
    const rel = entries.filter((e) => e.relay === rname);
    const h = a.host ?? {};
    const cfgOk = !!r.config_valid;
    // 3x-ui / x-ui is retired: Xray runs as fleet-xray under the new system.  Panel and config checks no longer
    // apply (null = not applicable, never a fault); Xray running comes from fleetd, else from the panel only when the
    // panel actually answered — null (unknown) is not "stopped".
    const fx = ix.fx?.get(rname);
    const xray = fx ? fx.running : panel.ok === true && typeof panel.xray_running === 'boolean' ? panel.xray_running : null;
    const parts = { agent: !!h.online && node.host != null && node.host.fresh, xray, panel: null, collector: fx ? fx.online : fresh, config: null };
    const bad = Object.keys(parts).filter((p) => parts[p] === false);
    const code = parts.agent === false || parts.xray === false ? 'fail' : bad.length ? 'degraded' : parts.xray == null ? 'unknown' : 'ok';
    const fleetRow = (ix.raw.fleet?.relays ?? []).find((x) => x.name === rname);
    out.push({
      id: node.id, name: rname, ip: node.ip, provider: node.provider, plan: node.billing.plan,
      status: { code, word: DICT.health[code] ?? code, parts, reasons: bad.map((p) => `${{ agent: '探针', xray: 'Xray', collector: '采集' }[p] ?? p} ${DICT.relay_parts[p][1]}`) },
      managed: fx ? 'fleet' : 'new_system', // x-ui retired either way; 'fleet' = fleetd reports this relay's Xray
      xray: { running: xray, version: fx?.version ?? null, reported_at: fx?.reported_at ?? null, online: fx?.online ?? null, api_ok: fx?.api_ok ?? null, source: fx ? 'fleet' : panel.ok === true ? 'panel' : null },
      bandwidth: node.bandwidth, host: node.host, tcp_connections: h.connections ?? null,
      rate: { rx_bps: Math.trunc((h.net_in ?? 0) * 8), tx_bps: Math.trunc((h.net_out ?? 0) * 8), source: 'host_interface' },
      entries_summary: { total: rel.length, active: rel.filter((e) => e.active).length, disabled: rel.filter((e) => !e.enable).length, unbound: rel.filter((e) => !e.bound_node).length, master_port: rel.find((e) => e.kind === 'master')?.port ?? null },
      node_count: ix.assets.filter((x) => x.relay === rname).length,
      versions: { panel: null, xray: fx?.version ?? (panel.ok === true ? panel.xray_version ?? null : null), agent: clients.get(a.uuid)?.version ?? null },
      // raw panel errors (“<urlopen error [Errno 111] …>”) are never passed to the UI
      panel: { ok: null, error: null, retired: true, xray_running: panel.xray_running ?? null, cpu_pct: round(panel.cpu, 2), mem_used: panel.mem_used ?? null, mem_total: panel.mem_total ?? null, checked_at: panel.checked_at ?? null, online_clients: (panel.onlines ?? []).length },
      collector: { received_at: r.received_at ?? null, sample_ts: r.ts ?? null, fresh, error: humanError(r.error ?? null), duration_s: ov.collector?.duration_seconds?.[rname] ?? null, log_backlog_bytes: r.log_backlog_bytes ?? null, log_size_bytes: r.log_size_bytes ?? null, event_gap: r.event_gap ?? null },
      config: {
        valid: cfgOk, verification: r.config_verification ?? null, logging: r.logging ?? null,
        routing_hash: String(r.routing_hash ?? '').slice(0, 12), template_routing_hash: String(r.template_routing_hash ?? '').slice(0, 12),
        startup_routing_hash: String(r.startup_routing_hash ?? '').slice(0, 12), routing_hash_source: r.routing_hash_source ?? null,
        runtime_rules: (r.runtime_rules ?? []).length, applied: r.applied ?? null, core_id: r.core_id ? String(r.core_id).slice(0, 12) : null,
      },
      probe: { updated_at: r.probe?.updated_at ?? null, assets: (r.probe?.assets ?? []).length, paths_updated_at: r.paths?.updated_at ?? null },
      traffic_quota: node.traffic.quota, billing: node.billing,
      ssh_port: fleetRow?.ssh_port ?? relaySshPort(ix, rname) ?? 22, // build.py: fleet table, else 22
    });
  }
  return out;
}
function relaySshPort(ix, name) {
  for (const d of Object.values(ix.raw.jobDetails ?? {})) if (d?.kind === 'relay' && d.params?.name === name && d.params.ssh_port) return d.params.ssh_port;
  return null;
}

// ─── incidents, replays, onboarding, behaviour ─────────────────────────────────────────────────────

/**
 * Technical transport errors from the legacy backend (“<urlopen error [Errno 111] Connection refused>”) become a short
 * Chinese phrase; anything else passes through unchanged.  The UI never shows Python / socket error reprs.
 */
export function humanError(e) {
  if (e == null || e === '') return e ?? null;
  const t = String(e);
  if (!/urlopen error|Errno|Traceback|Connection (refused|reset)|timed out|Name or service not known|ssl\.|<[A-Za-z_.]+Error/.test(t)) return t;
  if (/refused|Errno 111/.test(t)) return '连接被拒绝（服务未运行）';
  if (/reset|Errno 104/.test(t)) return '连接被对方重置';
  if (/timed out|Errno 110/.test(t)) return '连接超时';
  if (/Name or service not known|Errno -2|getaddrinfo/.test(t)) return '域名解析失败';
  if (/ssl\.|certificate/i.test(t)) return 'TLS 握手失败';
  return '连接失败';
}

const INC_EVIDENCE = ['rc', 'phase', 'ms', 'attempt', 'actual_ip', 'expected_ip', 'error', 'expected', 'actual', 'enable', 'fresh'];
export function incidentRow(inc, ix) {
  const ev = inc.evidence ?? {};
  const evd = ev.evidence ?? {};
  const end = inc.resolved_at ?? ix.now;
  const evOut = { value: ev.value ?? null, measured_at: ev.measured_at ?? null };
  for (const k of INC_EVIDENCE) if (k in evd) evOut[k] = k === 'error' ? humanError(evd[k]) : evd[k];
  return {
    id: inc.id, node: ix.ID.get(inc.asset) ?? null, name: ix.SHORT.get(inc.asset) ?? null, check: inc.key, title: inc.title,
    severity: inc.severity, severity_word: DICT.incident[inc.severity] ?? null,
    status: ev.status ?? null, status_word: DICT.health[ev.status] ?? null,
    reason: ev.reason ?? null, reason_word: ev.reason ? DICT.reason[ev.reason] ?? null : null,
    opened_at: inc.opened_at, last_seen: inc.last_seen, resolved_at: inc.resolved_at ?? null,
    duration_s: round(end - inc.opened_at, 1), open: inc.resolved_at == null,
    ack_at: inc.ack_at ?? null, ack_actor: inc.ack_actor && UUID_RE.test(inc.ack_actor) ? 'admin' : inc.ack_actor ?? null,
    note: inc.note ?? '', evidence: evOut,
  };
}

export function statusReplays(incidents, ix) {
  const NOW = ix.now;
  const long = incidents.filter((i) => (i.resolved_at ?? NOW) - i.opened_at >= 120);
  const cands = long.map((inc) => { const t = inc.opened_at + 60; return [t, incidents.filter((i) => i.opened_at <= t && t <= (i.resolved_at ?? NOW))]; });
  const moments = [];
  for (const [t, live] of cands.sort((a, b) => b[0] - a[0])) {
    if (moments.some((m) => Math.abs(t - m[0]) < 1800)) continue;
    moments.push([t, live]);
  }
  const replay = (t, live) => {
    const byNode = new Map();
    for (const i of live) { if (!byNode.has(i.asset)) byNode.set(i.asset, []); byNode.get(i.asset).push(i); }
    const items = [];
    for (const [asset, incs] of byNode) {
      const sts = incs.map((i) => i.evidence?.status);
      const st = sts.includes('fail') ? 'fail' : sts.includes('degraded') ? 'degraded' : 'unknown';
      const first = incs.slice().sort((x, y) => ((x.evidence?.status === st ? 0 : 1) - (y.evidence?.status === st ? 0 : 1)) || x.opened_at - y.opened_at)[0];
      const ev = first.evidence ?? {};
      const evd = ev.evidence ?? {};
      const since = Math.min(...incs.map((i) => i.opened_at));
      items.push({
        node: ix.ID.get(asset) ?? null, name: ix.SHORT.get(asset) ?? null, status: st, word: DICT.health[st],
        first_failed_check: first.key, check_title: first.title, reason: ev.reason ?? null, reason_word: ev.reason ? DICT.reason[ev.reason] ?? null : null,
        value: ev.value ?? null, evidence: Object.fromEntries(['rc', 'phase', 'ms', 'actual_ip', 'expected_ip', 'error'].filter((k) => k in evd).map((k) => [k, evd[k]])),
        since, duration_s: round(t - since), resolved_at: Math.max(...incs.map((i) => i.resolved_at ?? NOW)),
        checks: [...new Set(incs.map((i) => i.key))].sort(), incident_ids: incs.map((i) => i.id).sort((a, b) => a - b),
      });
    }
    const rank = { fail: 0, degraded: 1, unknown: 2 };
    items.sort((x, y) => rank[x.status] - rank[y.status] || String(x.name ?? '').localeCompare(String(y.name ?? '')));
    const counts = {};
    for (const x of items) counts[x.status] = (counts[x.status] ?? 0) + 1;
    return { at: t, at_text: bjText(t), nodes: items, count: items.length, counts };
  };
  const out = moments.slice(0, 4).map(([t, live]) => replay(t, live));
  if (moments.length) {
    const largest = moments.reduce((best, m) => (new Set(m[1].map((i) => i.asset)).size > new Set(best[1].map((i) => i.asset)).size ? m : best), moments[0]);
    if (out.every((r) => Math.abs(largest[0] - r.at) > 1)) out.push(replay(...largest));
  }
  return out;
}

const JOB_STATUS = { succeeded: 'succeeded', failed: 'failed', rolled_back: 'rolled_back', running: 'running' };
const STEP_STATUS = { ok: 'succeeded', failed: 'failed', skipped: 'skipped', pending: 'queued', running: 'running' };
export function onboardingHistory(ix) {
  const out = [];
  for (const j of (ix.raw.jobs?.jobs ?? []).slice().sort((a, b) => b.created_at - a.created_at)) {
    const d = ix.raw.jobDetails?.[j.id];
    const p = d?.params ?? {};
    const steps = (d?.steps ?? []).map((s) => ({ key: s.key, name: s.name, status: STEP_STATUS[s.status] ?? s.status, duration_s: s.started_at && s.finished_at ? round(s.finished_at - s.started_at, 1) : null, error: humanError(s.error) }));
    const kind = j.kind === 'relay' ? 'relay' : p.outbound?.type === 'socks' ? 'proxy' : 'residential';
    const [ok, total] = String(j.step_summary ?? '0/0').split('/').map(Number);
    const status = JOB_STATUS[j.status] ?? j.status;
    out.push({
      id: String(j.id).slice(0, 8), legacy: true, title: j.title, type: kind, type_word: { relay: '线路机', proxy: '代理产品', residential: '家宽节点' }[kind],
      count: 1, status, status_word: DICT.step[status] ?? null, started_at: j.created_at, finished_at: j.finished_at ?? null,
      duration_s: j.finished_at ? round(j.finished_at - j.created_at, 1) : null, operator: null,
      relay: p.relay ?? null, team: p.team ?? null, provider: p.provider ?? null, service_id: p.service_id ?? null,
      steps, ok_steps: d ? steps.filter((s) => s.status === 'succeeded').length : ok, total_steps: d ? steps.length : total,
      error: humanError(d?.result?.error ?? d?.error ?? null), steps_loaded: !!d,
    });
  }
  return out;
}

export function behaviorBlock(ix) {
  const b = ix.raw.behavior ?? {};
  const act = ix.raw.activity ?? {};
  const nodes = {};
  for (const x of b.nodes ?? []) {
    const nid = ix.ID.get(x.asset);
    if (nid) nodes[nid] = { connections: x.connections ?? null, domains: x.domains ?? null, sources: x.sources ?? null, blocked: x.blocked ?? null, ip_only: x.ip_only ?? null, last_seen: x.last_seen ?? null };
  }
  const vals = Object.values(nodes);
  const events = (b.events ?? []).filter((e) => e.kind !== 'new_domain' && ix.ID.get(e.asset));
  const counts = {};
  for (const e of b.events ?? []) {
    const c = (counts[e.kind] ??= { kind: e.kind, total: 0, unacked: 0, last: null });
    c.total++; if (e.ack_at == null) c.unacked++; c.last = Math.max(c.last ?? 0, e.ts);
  }
  const daily = {};
  for (const e of b.events ?? []) if (e.kind === 'new_domain') { const d = bjText(e.ts).slice(0, 10); daily[d] = (daily[d] ?? 0) + 1; }
  return {
    window_s: 86400, computed_at: act.computed_at ?? b.computed_at ?? null,
    kpis: { active_nodes: vals.filter((v) => v.connections).length, connections: act.total ?? null, targets_sum_by_node: sum(vals, (v) => v.domains), targets_note: '按节点去重后求和（全局去重需服务端另算）', blocked: sum(vals, (v) => v.blocked) },
    top_domains: (act.domains ?? []).map((d) => ({ host: d.host, kind: d.kind, connections: d.connections, sources: d.sources, blocked: d.blocked, last_seen: d.last_seen })),
    top_sources: (act.sources ?? []).map((s) => withIntel({ source_ip: s.source_ip, connections: s.connections, domains: s.domains ?? null, last_seen: s.last_seen ?? null })),
    nodes,
    top_targets_by_node: (b.targets ?? []).slice(0, 40).filter((t) => ix.ID.get(t.asset)).map((t) => ({ node: ix.ID.get(t.asset), host: t.host, kind: t.kind, connections: t.connections, sources: t.sources, blocked: t.blocked, first_seen: t.first_seen, last_seen: t.last_seen, total_recorded: t.total_recorded })),
    events_recent: events.slice(0, 60).map((e) => ({ id: e.id, node: ix.ID.get(e.asset), kind: e.kind, subject: e.subject, ts: e.ts, message: e.message, acked: e.ack_at != null, ...(e.kind === 'new_source' ? { subject_intel: null } : {}) })),
    events_counts: Object.values(counts), events_window_s: 86400,
    new_domain_daily: Object.entries(daily).sort().map(([date, count]) => ({ date, count })),
    timeline_recent: (act.rows ?? []).slice(0, 60).map((r) => withIntel({ ts: r.ts, node: ix.ID.get(r.asset) ?? null, team: r.team, relay: r.relay, source_ip: r.source_ip, host: r.host, port: r.port, protocol: r.protocol, action: r.action, action_word: DICT.access_action[r.action] ?? r.action, client: r.client, kind: r.kind })),
  };
}
// Row-level IP-intelligence fields (README “IP 情报 · 行级字段”) come from offline IP databases that do not
// exist in production: present, always null, so a page reading row.carrier renders “—”.
export const INTEL_ROW = Object.freeze({ carrier: null, asn: null, as_org: null, country: null, province: null, city: null, conn_type: null, conn_conf: null, first_seen: null, last_seen: null, sessions: null, sessions_h24: null });
const withIntel = (row) => ({ ...INTEL_ROW, ...row });
export const accessRow = (r) => withIntel({ ts: r.ts, source_ip: r.source_ip, host: r.host, port: r.port, protocol: r.protocol, action: r.action, action_word: DICT.access_action[r.action] ?? r.action, client: r.client, kind: r.kind });
export function accessRecent(ix) {
  const out = {};
  for (const a of ix.assets) out[ix.ID.get(a.uuid)] = [];
  for (const r of ix.raw.activity?.rows ?? []) {
    const nid = ix.ID.get(r.asset);
    if (nid && out[nid].length < 10) out[nid].push(accessRow(r));
  }
  for (const [uuid, rows] of Object.entries(ix.raw.nodeActivity ?? {})) {
    const nid = ix.ID.get(uuid);
    if (nid) out[nid] = rows.slice(0, 10).map(accessRow);
  }
  return out;
}

// ─── fleet aggregates ──────────────────────────────────────────────────────────────────────────────

export function fleet24(ix) {
  const rows = ix.raw.history;
  if (!rows) return null;
  const s = trafficSeries(rows, ix.now);
  let pk = 0, pu = 0;
  for (let i = 0; i < s.down.length; i++) { if (nz(s.down[i]) > nz(s.down[pk])) pk = i; if (nz(s.up[i]) > nz(s.up[pu])) pu = i; }
  return {
    t0: s.t0, step: 300, unit: 'bytes_per_bucket', down: s.down, up: s.up,
    peak_down: { t: s.t0 + pk * 300, bytes: s.down[pk] ?? null, bps: round((nz(s.down[pk]) * 8) / 300) },
    peak_up: { t: s.t0 + pu * 300, bytes: s.up[pu] ?? null, bps: round((nz(s.up[pu]) * 8) / 300) },
    total: { down: sum(s.down), up: sum(s.up) },
    missing_minutes: s.down.filter((v) => v == null).length * 5,
    note: 'Xray 入口计数（业务流量），全部入口按 5 分钟求和（接口 /history 最细 5 分钟），含线路机主节点入口与监控探测流量',
    resolution_s: 300,
  };
}
function sparkFromFleet(f) {
  if (!f) return null;
  const n = 24;
  return { t0: f.t0 + (f.down.length - n) * 300, step: 300, down_bps: f.down.slice(-n).map((v) => (v == null ? null : round((v * 8) / 300))), up_bps: f.up.slice(-n).map((v) => (v == null ? null : round((v * 8) / 300))), label: '近 2 小时' };
}

function teamRows(nodes) {
  const teams = [...new Set(nodes.map((n) => n.team))].sort((a, b) => (TEAM_ORDER[a] ?? 9) - (TEAM_ORDER[b] ?? 9));
  return teams.map((team) => {
    const ns = nodes.filter((n) => n.team === team);
    const dist = {};
    for (const n of ns) dist[n.status.code] = (dist[n.status.code] ?? 0) + 1;
    const sampled = ns.filter((n) => n.rate && n.rate.fresh && n.rate.down_bps != null);
    return {
      team, count: ns.length, status: Object.fromEntries(['ok', 'degraded', 'fail', 'unknown', 'maintenance'].map((s) => [s, dist[s] ?? 0])),
      rate_down_bps: sum(sampled, (n) => n.rate.down_bps), rate_up_bps: sum(sampled, (n) => n.rate.up_bps),
      rate_sampled: sampled.length, rate_expected: ns.length, rate_complete: sampled.length === ns.length,
      active: sampled.filter((n) => (n.rate.down_bps + n.rate.up_bps) / 8 >= 1024).length,
      today_bytes: sum(ns, (n) => n.traffic.today.down + n.traffic.today.up),
      today: { down: sum(ns, (n) => n.traffic.today.down), up: sum(ns, (n) => n.traffic.today.up) },
      month: { down: sum(ns, (n) => n.traffic.month.down), up: sum(ns, (n) => n.traffic.month.up) },
      monthly_cents: sum(ns, (n) => n.billing.monthly_cents),
    };
  });
}

function top(nodes, key, n = 10) {
  return nodes.filter((x) => key(x) != null).sort((a, b) => key(b) - key(a)).slice(0, n).map((x) => ({ id: x.id, name: x.name, team: x.team, value: key(x) }));
}

export function providersOf(nodes) {
  return ['ATTVPS', 'NodeMach', 'OhnoICU', 'GoMami', ...new Set(nodes.map((n) => n.provider).filter((p) => p && !(p in PROVIDER_CODE)))].map((pname) => {
    const ns = nodes.filter((n) => n.provider === pname);
    const plans = [...new Map(ns.map((n) => [JSON.stringify([n.billing.plan, n.billing.price, n.bandwidth.text]), [n.billing.plan, n.billing.price, n.bandwidth.text]])).values()]
      .sort((a, b) => String(a).localeCompare(String(b)));
    return { name: pname, code: PROVIDER_CODE[pname] ?? null, kind: PROVIDER_KIND[pname] ?? null, kind_word: DICT.provider_kind[PROVIDER_KIND[pname]] ?? null, host_access: PROVIDER_HOST_ACCESS[pname] ?? null, count: ns.length, plans: plans.map(([plan, price, bandwidth]) => ({ plan, price, bandwidth })) };
  }).filter((p) => p.count || p.name in PROVIDER_CODE);
}

// ─── the files ─────────────────────────────────────────────────────────────────────────────────────

/** Everything that depends on the bundle: full nodes, relays, entries, index. */
export function buildModel(raw, now, extraByUuid = {}) {
  const ix = indexBundle(raw, now);
  const full = ix.assets.map((a) => buildNodeFull(a, ix, extraByUuid[a.uuid]));
  const byId = new Map(full.map((n) => [n.id, n]));
  const entries = buildEntries(ix, byId);
  const relays = buildRelays(ix, byId, entries);
  return { ix, full, byId, entries, relays };
}

/** snapshot.json */
export function buildSnapshot(raw, now, meta = {}) {
  const model = buildModel(raw, now);
  const { ix, full, entries, relays } = model;
  const nodes = full;
  const counts = {};
  for (const n of nodes) counts[n.status.code] = (counts[n.status.code] ?? 0) + 1;
  const sampled = nodes.filter((n) => n.rate && n.rate.fresh && n.rate.down_bps != null);
  const due7 = nodes.filter((n) => n.billing.due_days != null && n.billing.due_days <= 7).sort((a, b) => a.billing.due_days - b.billing.due_days);
  const nextDue = nodes.filter((n) => n.billing.due_days != null).sort((a, b) => a.billing.due_days - b.billing.due_days || (a.id < b.id ? -1 : 1));
  const f24 = fleet24(ix);
  const today = { down: sum(nodes, (n) => n.traffic.today.down), up: sum(nodes, (n) => n.traffic.today.up) };
  const month = { down: sum(nodes, (n) => n.traffic.month.down), up: sum(nodes, (n) => n.traffic.month.up) };
  const kpis = {
    normal: { ok: counts.ok ?? 0, total: nodes.length, degraded: counts.degraded ?? 0, fail: counts.fail ?? 0, unknown: counts.unknown ?? 0, maintenance: counts.maintenance ?? 0 },
    attention: (ix.mon.attention ?? []).length,
    business_rate: { down_bps: sum(sampled, (n) => n.rate.down_bps), up_bps: sum(sampled, (n) => n.rate.up_bps), sampled_entries: sampled.length, expected_entries: nodes.filter((n) => n.rate).length, spark: sparkFromFleet(f24) },
    today_business: { ...today, total: today.down + today.up, since: ix.day0, since_text: '北京时间 0 点起' },
    month_business: { ...month, total: month.down + month.up, since: ix.month0, coverage_start: ix.ov.started_at ?? null },
    due_7d: { count: due7.length, items: due7.map((n) => n.id), nearest: nextDue.length ? { id: nextDue[0].id, name: nextDue[0].name, days: nextDue[0].billing.due_days, due_date: nextDue[0].billing.due_date } : null },
    monthly_cost: { cents: sum(nodes, (n) => n.billing.monthly_cents), currency: 'USD', services: nodes.length },
    events_today: ix.ov.events_today ?? null,
  };
  const rateTop = top(nodes, (x) => (x.rate && x.rate.down_bps != null ? x.rate.down_bps + x.rate.up_bps : null));
  for (const rk of rateTop) { const n = model.byId.get(rk.id); Object.assign(rk, { down_bps: n.rate.down_bps, up_bps: n.rate.up_bps, ip: n.ip }); }
  const incidentsAll = (raw.incidents ?? []).map((i) => incidentRow(i, ix));
  const beh = behaviorBlock(ix);
  const hostNodes = nodes.filter((n) => n.capabilities.includes('host'));
  const probeNodes = nodes.filter((n) => n.capabilities.includes('probe'));
  const ov = ix.ov;
  const snapshot = {
    schema: SCHEMA, generated_at: new Date(now * 1000).toISOString(),
    as_of: now, as_of_text: bjText(now) + ' 北京时间', timezone: 'Asia/Shanghai',
    provenance: { source: 'live', api_base: meta.apiBase ?? null, fetched_at: meta.fetchedAt ?? {}, relay_snapshot_at: Object.fromEntries(Object.entries(ov.relays ?? {}).map(([k, v]) => [k, v.received_at ?? null])), ipintel: '实时接口不提供（IP 情报来自离线库，只在快照里）' },
    anonymization: { applied: false, note: '实时数据，未脱敏；只在管理员会话内展示' },
    site: { name: meta.siteName ?? '团队线路管理', description: meta.siteDescription ?? null, komari_version: meta.komariVersion ?? null },
    dict: DICT,
    teams: [...new Set(nodes.map((n) => n.team))].sort((a, b) => (TEAM_ORDER[a] ?? 9) - (TEAM_ORDER[b] ?? 9)).map((t) => ({ name: t, order: TEAM_ORDER[t] ?? 9 })),
    providers: providersOf(nodes),
    kpis,
    groups: teamRows(nodes),
    attention: { items: [], unknown: ix.mon.unknown ?? [], computed_at: ix.mon.checked_at ?? null, definition: '未恢复故障 + 持续降级 ≥ 5 分钟（服务端 monitor.attention）', note: '实时：需处理由页面按节点状态推导' },
    incidents_recent: incidentsAll.slice(0, 30), incidents_total: incidentsAll.length,
    status_replays: statusReplays(raw.incidents ?? [], ix),
    nodes: nodes.map(coreNode),
    relays,
    traffic: { fleet_24h: f24 },
    rankings: {
      rate: rateTop, today: top(nodes, (x) => x.traffic.today.down + x.traffic.today.up),
      cpu_avg_24h: [], mem_avg_24h: [], latency_unstable_24h: [], loss_24h: [],
    },
    upcoming_due: nextDue.slice(0, 10).map((n) => ({ id: n.id, name: n.name, team: n.team, due_date: n.billing.due_date, due_days: n.billing.due_days, tz_badge: n.billing.tz_badge, monthly_cents: n.billing.monthly_cents, provider: n.provider })),
    system: {
      host_agents: { online: hostNodes.filter((n) => n.agent.online && n.host.fresh).length, total: hostNodes.length },
      connectivity_probes: { online: probeNodes.filter((n) => n.agent.online).length, total: probeNodes.length },
      orphan_hosts: raw.komariHosts ? Object.keys(raw.komariHosts).filter((u) => !ix.byUuid.has(u)).length : null,
      collector: { ...Object.fromEntries(['workers', 'configured', 'running', 'waiting', 'oldest_wait_seconds', 'checked_at'].map((k) => [k, ov.collector?.[k] ?? null])), duration_seconds: ov.collector?.duration_seconds ?? null, interval_s: ov.collection_interval ?? 10 },
      monitor: { checked_at: ix.mon.checked_at ?? null, started_at: ov.monitor_started_at ?? null, interval_s: 10 },
      backup: Object.fromEntries(['verified_at', 'file', 'size_bytes', 'integrity', 'scope', 'offsite'].map((k) => [k, ov.backup?.[k] ?? null])),
      storage: { operations_db_bytes: null, komari_db_bytes: null, metrics_db_bytes: null, note: '实时接口不提供数据库文件大小' },
      retention: { activity_days: ov.retention?.activity_days ?? null, minute_days: ov.retention?.minute_days ?? null, hour_days: ov.retention?.hour_days ?? null, komari_minute_rollup_minutes: null, komari_5min_rollup_minutes: null },
      hosts_at: ov.hosts_at ?? null, service_started_at: ov.started_at ?? null, availability_definition: AVAILABILITY_DEFINITION,
      health_ok: raw.health?.ok ?? null,
    },
    behavior_kpis: beh.kpis,
    behavior_top: { window_s: 86400, computed_at: beh.computed_at, total_connections: beh.kpis.connections, domains: beh.top_domains.slice(0, 20), sources: beh.top_sources.slice(0, 12), events_recent: beh.events_recent.slice(0, 12), events_counts: beh.events_counts },
    entries: entries.map(coreEntry),
    access_recent: accessRecent(ix),
    onboarding: { history: onboardingHistory(ix), demo_batch: null, note: '实时：history 是真实的旧流程单台任务；新流水线尚未上线，没有批次' },
    lazy: { detail: 'detail.json', extra: 'extra.json', traffic_5m: 'series/traffic-5m.json', traffic_1m: 'series/traffic-1m.json', host_24h: 'series/host-24h.json', latency_24h: 'series/latency-24h.json' },
    ipintel: null,
    live: true,
  };
  return { snapshot: normalize(snapshot), model };
}

/** hourly48 from /node history (per-minute monitor samples; only the last 24 h exist). */
export function hourly48FromSamples(samples, now) {
  const h0 = Math.floor(now / 3600) * 3600 - 47 * 3600;
  const ok = new Array(48).fill(null), failed = new Array(48).fill(null), unknown = new Array(48).fill(null), maintenance = new Array(48).fill(null);
  for (const s of samples ?? []) {
    const k = Math.floor((s.bucket - h0) / 3600);
    if (k < 0 || k >= 48) continue;
    ok[k] = (ok[k] ?? 0) + nz(s.ok); failed[k] = (failed[k] ?? 0) + nz(s.failed); unknown[k] = (unknown[k] ?? 0) + nz(s.unknown); maintenance[k] = (maintenance[k] ?? 0) + nz(s.maintenance);
  }
  return { t0: h0, step: 3600, ok, failed, unknown, maintenance, note: '接口只保留 24 小时逐分钟样本，前 24 格为空' };
}

/** detail.json nodes.<id> from the model (+ per-node enrichment when a page has opened it). */
export function detailNode(n, model, enrich = {}) {
  const { ix } = model;
  const u = n.komari_uuid;
  const incidents = (ix.raw.incidents ?? []).filter((i) => i.asset === u).slice(0, 20).map((i) => incidentRow(i, ix));
  const renewals = (ix.raw.renewals ?? []).filter((r) => r.asset === u).map((r) => ({ id: r.id, ts: r.ts, node: n.id, name: n.name, actor: r.actor, amount_cents: r.amount_cents, currency: 'USD', old_due: r.old_due, new_due: r.new_due, note: r.note }));
  const audit = auditRows(ix).filter((r) => r.target === 'asset:' + n.id).slice(0, 20);
  const beh = enrich.behavior;
  const targets = beh ? beh.targets.slice().sort((a, b) => (b.total_recorded ?? b.connections) - (a.total_recorded ?? a.connections)).slice(0, 8)
    .map((t) => ({ host: t.host, kind: t.kind, connections: t.total_recorded ?? t.connections, blocked: t.blocked, first_seen: t.first_seen, last_seen: t.last_seen })) : [];
  const sources = beh ? beh.sources.slice().sort((a, b) => b.connections - a.connections).slice(0, 5)
    .map((s) => withIntel({ source_ip: s.source_ip, connections: s.connections, first_seen: null, last_seen: s.last_seen })) : [];
  const nodeDetail = enrich.node;
  const windows = nodeDetail?.windows ? windowsFull(nodeDetail.windows) : n.availability.windows;
  return normalize({
    komari_uuid: u, relay_ip: n.relay_ip, checks: n.checks,
    availability: { windows, tracker30: n.availability.tracker30, hourly48: nodeDetail ? hourly48FromSamples(nodeDetail.history, ix.now) : null, monitor_since: n.availability.monitor_since },
    latency_stability_tcp_24h: enrich.stability ?? null,
    connection_source_ips: n.connection?.source_ips ?? [],
    access: {
      top_targets_90d: targets, top_sources: sources, sources_seen: beh ? beh.sources.length : null,
      recent10: (enrich.activity?.rows ?? ix.raw.nodeActivity?.[u] ?? (ix.raw.activity?.rows ?? []).filter((r) => r.asset === u)).slice(0, 10).map(accessRow),
      enriched: !!(beh || enrich.activity),
    },
    probe_tasks: n.probe_tasks, supplier: n.supplier, note: n.note, remark_private: n.remark_private, remark_public: n.remark_public,
    revision: n.revision, updated_at: n.updated_at, sync_pending: n.sync_pending, created_at: n.created_at,
    incidents, renewals, audit, ipintel: null,
  });
}
export function buildDetail(model) {
  const nodes = {};
  for (const n of model.full) nodes[n.id] = detailNode(n, model, model.enrich?.[n.komari_uuid] ?? {});
  return { as_of: Math.round(model.ix.now), nodes, live: true };
}

export function auditRows(ix) {
  return (ix.raw.audit ?? []).filter((r) => r.action !== 'health_flip').map((r) => {
    let d = r.detail ?? {};
    if (d && typeof d === 'object' && 'before' in d && 'after' in d && d.before && typeof d.before === 'object') {
      const b = d.before, a = d.after ?? {};
      const changed = {};
      for (const k of new Set([...Object.keys(b), ...Object.keys(a)])) {
        if (['updated_at', 'revision', 'sync_pending'].includes(k)) continue;
        if (JSON.stringify(b[k]) !== JSON.stringify(a[k])) changed[k] = [b[k] ?? null, a[k] ?? null];
      }
      d = { changed };
    }
    const tgt = r.target;
    return { id: r.id, ts: r.ts, actor: r.actor && UUID_RE.test(r.actor) ? 'admin' : r.actor, action: r.action, target: ix.ID.has(tgt) ? 'asset:' + ix.ID.get(tgt) : tgt, target_name: ix.SHORT.get(tgt) ?? null, detail: d };
  });
}

/** extra.json */
export function buildExtra(model) {
  const { ix } = model;
  const pings = ix.raw.pings ?? [];
  const counts = {};
  for (const r of ix.raw.audit ?? []) counts[r.action] = (counts[r.action] ?? 0) + 1;
  return normalize({
    as_of: ix.now,
    entries: model.entries.map((e) => ({ ...e, ipintel: null })),
    incidents: (ix.raw.incidents ?? []).map((i) => incidentRow(i, ix)),
    behavior: behaviorBlock(ix),
    probes: pings.map((t) => ({ id: t.id, name: t.name, type: t.type, target: t.target, interval_s: t.interval, executors: (t.clients ?? []).map((c) => ix.ID.get(c) ?? 'orphan') })),
    renewals: (ix.raw.renewals ?? []).map((r) => ({ id: r.id, ts: r.ts, node: ix.ID.get(r.asset) ?? null, name: ix.SHORT.get(r.asset) ?? null, actor: r.actor, amount_cents: r.amount_cents, currency: 'USD', old_due: r.old_due, new_due: r.new_due, note: r.note })),
    audit_recent: auditRows(ix),
    audit_counts: counts, audit_counts_window: '最近 150 条审计（接口上限）',
    live: true,
  });
}

// ─── series files ──────────────────────────────────────────────────────────────────────────────────

export function trafficFile(model, { oneMinute = false } = {}) {
  const g = grid5(model.ix.now);
  const nodes = {};
  for (const n of model.full) if (n.traffic.series5m) nodes[n.id] = { ...n.traffic.series5m };
  return normalize({ t0: g.t0, step: 300, unit: 'bytes_per_bucket', nodes, ...(oneMinute ? { resolution_s: 300, note: '接口 /history 最细 5 分钟：此文件按 5 分钟桶提供（step = 300）' } : {}) });
}

/** Komari getRecordsByUUID records → host_24h columns on the H5 grid. */
export function hostSeries(records, now, { ramTotal, diskTotal } = {}) {
  const g = gridH5(now);
  const acc = new Map();
  for (const r of records ?? []) {
    const t = parseIso(r.time);
    if (t == null) continue;
    const k = Math.floor((t - g.t0) / 300);
    if (k < 0 || k >= g.n) continue;
    let e = acc.get(k);
    if (!e) acc.set(k, (e = { n: 0, s: {}, mx: {} }));
    e.n++;
    for (const f of ['cpu', 'ram', 'disk', 'net_in', 'net_out', 'load', 'connections', 'connections_udp', 'process', 'swap']) {
      if (isNum(r[f])) { e.s[f] = (e.s[f] ?? 0) + r[f]; e.mx[f] = Math.max(e.mx[f] ?? -Infinity, r[f]); }
    }
  }
  const col = (f, map) => Array.from({ length: g.n }, (_, k) => { const e = acc.get(k); return e && e.s[f] != null ? map(e.s[f] / e.n, e.mx[f]) : null; });
  return {
    t0: g.t0, step: 300,
    cpu_pct: col('cpu', (v) => round(v, 1)),
    mem_pct: col('ram', (v) => (ramTotal ? round((v * 100) / ramTotal, 1) : null)),
    disk_pct: col('disk', (v) => (diskTotal ? round((v * 100) / diskTotal, 2) : null)),
    net_rx_bps: col('net_in', (v) => Math.round(v * 8)), net_tx_bps: col('net_out', (v) => Math.round(v * 8)),
    net_rx_bps_max: col('net_in', (_v, mx) => Math.round(mx * 8)), net_tx_bps_max: col('net_out', (_v, mx) => Math.round(mx * 8)),
    load1: col('load', (v) => round(v, 2)), tcp: col('connections', (v) => Math.round(v)), udp: col('connections_udp', (v) => Math.round(v)),
    processes: col('process', (v) => Math.round(v)), swap_used: col('swap', (v) => Math.trunc(v)),
    max_basis: records?.length && (parseIso(records[1]?.time) - parseIso(records[0]?.time)) >= 300 ? '同一 5 分钟桶只有一条记录，峰值等于均值' : '1 分钟记录在 5 分钟桶内取最大',
  };
}

/** Komari getPingRecords (one executor) → latency_24h entry on the H5 grid. */
export function latencySeries(result, now, taskId) {
  if (!result) return null;
  const g = gridH5(now);
  const avg = new Array(g.n).fill(null), loss = new Array(g.n).fill(null);
  let samples = 0, lost = 0;
  for (const r of result.records ?? []) {
    const t = parseIso(r.time);
    const k = t == null ? -1 : Math.floor((t - g.t0) / 300);
    if (k < 0 || k >= g.n) continue;
    samples++;
    if (r.value < 0) { lost++; loss[k] = 100; } else { avg[k] = Math.round(r.value); loss[k] = 0; }
  }
  return { task_id: taskId, avg_ms: avg, min_ms: new Array(g.n).fill(null), max_ms: new Array(g.n).fill(null), loss_pct: loss, samples, lost, value_basis: 'Komari 每 5 分钟桶的最后一个样本（接口不给桶内均值、最小、最大）' };
}
export function stability(lat) {
  const vals = (lat?.avg_ms ?? []).filter((v) => v != null);
  if (vals.length < 3) return null;
  const mean = vals.reduce((a, b) => a + b, 0) / vals.length;
  const sd = Math.sqrt(vals.reduce((a, v) => a + (v - mean) ** 2, 0) / vals.length);
  return { mean_ms: round(mean, 1), stddev_ms: round(sd, 1), p95_ms: vals.slice().sort((a, b) => a - b)[Math.floor(vals.length * 0.95) - 1] };
}

// ─── IP intelligence for the node drawer (fleetd /v2/api/v1/nodes/{id}/sources, fleet/API.md §5a) ───────────────────
// → the prototype shapes: snapshot nodes[].ipintel {summary, breakdown{h24,d14}} and detail ipintel.sources[], plus
// intel records (snapshot ipintel.sources shape) for ui/intel.js intelOf().  Fallback when fleetd has nothing (or is
// not there): the node's own recent access rows and its entry's current source IPs — so the cards never say
// “没有来源连接” while connections exist.

const COUNTRY = { CN: '中国', US: '美国', HK: '中国香港', TW: '中国台湾', JP: '日本', SG: '新加坡', KR: '韩国', MO: '中国澳门' };
/** fleetd intel {isp, asn, as_org, country, region, city, type, type_confidence, flags} → snapshot source record. */
export function intelRecord(x) {
  if (!x?.ip) return null;
  return { ip: x.ip, carrier: x.isp ?? null, asn: x.asn ?? null, as_org: x.as_org ?? null, country: x.country ?? null, country_name: COUNTRY[x.country] ?? x.country ?? null,
    province: x.region ?? null, city: x.city ?? null, conn_type: x.type ?? null, conn_conf: x.type_confidence ?? null, flags: x.flags ?? [] };
}
const distRows = (list) => (list ?? []).map((r) => ({ k: r.label ?? r.key, key: r.key, pct: r.pct ?? null, c: r.count ?? 0, s: r.sources ?? 0 }));
function windowBreakdown(w) {
  if (!w) return null;
  const d = w.distribution ?? {};
  return { total: { connections: w.total ?? 0, sessions: null }, carrier: distRows(d.isp), city: distRows(d.city), province: distRows(d.province), conn_type: (d.type ?? []).map((r) => ({ k: r.key, pct: r.pct ?? null, c: r.count ?? 0, s: r.sources ?? 0 })) };
}
const top1 = (b, k) => (b?.[k]?.[0] ? { k: b[k][0].k, pct: b[k][0].pct } : null);

/**
 * liveIpintel({ h24, d14, activity, entryIps, now }) — h24 / d14: fleetd sources responses (data) or null;
 * activity: the node's access rows (legacy /activity?asset=…); entryIps: the entry's current source IPs.
 * → { summary, breakdown, sources[], intel[], basis: 'fleetd' | 'activity' | null }
 */
export function liveIpintel({ h24 = null, d14 = null, activity = [], entryIps = [], now = Date.now() / 1000 } = {}) {
  if ((h24?.total ?? 0) > 0 || (d14?.total ?? 0) > 0) {
    const b24 = windowBreakdown(h24), b14 = windowBreakdown(d14);
    const byIp = new Map();
    const intel = new Map();
    for (const [w, key] of [[d14, 'd14'], [h24, 'h24']]) {
      for (const s of w?.top_sources ?? []) {
        const x = byIp.get(s.ip) ?? { ip: s.ip, first_seen: s.first_seen ?? null, last_seen: s.last_seen ?? null, connections: { h24: 0, d14: 0 }, blocked_d14: 0, clients: [], new_source_events: [] };
        x.connections[key] = s.count ?? 0;
        if (s.first_seen != null) x.first_seen = Math.min(x.first_seen ?? s.first_seen, s.first_seen);
        if (s.last_seen != null) x.last_seen = Math.max(x.last_seen ?? 0, s.last_seen);
        byIp.set(s.ip, x);
        const r = intelRecord(s.intel ?? { ip: s.ip });
        if (r) intel.set(s.ip, r);
      }
    }
    const sources = [...byIp.values()];
    const mobile = (b) => b?.conn_type?.find((r) => r.k === 'mobile')?.pct ?? 0;
    return {
      basis: 'fleetd',
      summary: {
        sources: { h24: h24?.distinct_sources ?? 0, d14: d14?.distinct_sources ?? 0 },
        new_sources_h24: sources.filter((s) => s.first_seen != null && now - s.first_seen < 86400).length,
        top_carrier: { h24: top1(b24, 'carrier'), d14: top1(b14, 'carrier') }, top_city: { h24: top1(b24, 'city'), d14: top1(b14, 'city') },
        mobile_pct: { h24: mobile(b24), d14: mobile(b14) }, flags: [], anomaly_ids: [],
      },
      breakdown: { h24: b24 ?? { total: { connections: 0 } }, d14: b14 ?? { total: { connections: 0 } } },
      sources, intel: [...intel.values()],
    };
  }
  // fallback: the node's own access rows (24 h) and the entry's current source IPs — counts, no intel
  const rows = (activity ?? []).filter((r) => r.source_ip && now - (r.ts ?? 0) < 86400);
  const ips = new Map();
  for (const r of rows) { const x = ips.get(r.source_ip) ?? { ip: r.source_ip, first_seen: r.ts, last_seen: r.ts, connections: { h24: 0, d14: 0 }, blocked_d14: 0, clients: [], new_source_events: [] }; x.connections.h24++; x.connections.d14++; x.first_seen = Math.min(x.first_seen, r.ts); x.last_seen = Math.max(x.last_seen, r.ts); if (r.client && !x.clients.includes(r.client)) x.clients.push(r.client); ips.set(r.source_ip, x); }
  for (const ip of entryIps ?? []) if (!ips.has(ip)) ips.set(ip, { ip, first_seen: null, last_seen: now, connections: { h24: 0, d14: 0 }, blocked_d14: 0, clients: [], new_source_events: [], current: true });
  if (!ips.size) return { basis: null, summary: { sources: { h24: 0, d14: 0 } }, breakdown: { h24: { total: { connections: 0 } }, d14: { total: { connections: 0 } } }, sources: [], intel: [] };
  const n = rows.length || ips.size;
  return {
    basis: 'activity',
    summary: { sources: { h24: ips.size, d14: ips.size }, new_sources_h24: 0, flags: [], anomaly_ids: [] },
    breakdown: { h24: { total: { connections: n, sessions: null } }, d14: { total: { connections: n, sessions: null } } },
    sources: [...ips.values()], intel: [],
  };
}
