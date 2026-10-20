// 本地复刻“原版”运营页（plugin/public）所需的 /api/team-ops/* 接口形状。
// 数据全部来自 prototype/data 的脱敏快照；只为在本机渲染原版界面、量取样式。
// 时间戳按请求时刻整体平移（now - as_of），让原版的鲜度判断（30/40 秒）通过。
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const DATA = path.resolve(here, '../../data');
const ROOT = path.resolve(here, '../../..');
const load = f => JSON.parse(fs.readFileSync(path.join(DATA, f), 'utf8'));
const S = load('snapshot.json');
const D = load('detail.json');
const X = load('extra.json');
const T5 = load('series/traffic-5m.json');
const AS_OF = S.as_of;

export const options = { replay: process.env.REPLAY !== '0' };

const byId = Object.fromEntries(S.nodes.map(n => [n.id, n]));
const relayByName = Object.fromEntries(S.relays.map(r => [r.name, r]));
const off = () => Math.floor(Date.now() / 1000) - AS_OF;
const sh = t => (t == null ? t : t + off());

// 两个真实发生过的状态（status_replays），用来让徽章、需要关注等区域有内容。
function replayState() {
  const map = {};
  if (!options.replay) return map;
  const picks = [S.status_replays[0], S.status_replays[2]].filter(Boolean);
  for (const r of picks) for (const n of r.nodes) map[n.node] = n;
  return map;
}

function statusOf(n, rp) {
  const r = rp[n.id];
  if (r) return { code: r.status, reason: r.reason, since: r.since + (AS_OF - r.at), first: r.first_failed_check, value: r.value };
  return { code: n.status.code, reason: n.status.reason, since: n.status.since, first: null };
}

function checksOf(n, rp) {
  const base = (D.nodes[n.id]?.checks || []).map(c => ({ ...c, measured_at: sh(c.measured_at), since: sh(c.since) }));
  const r = rp[n.id];
  if (r) for (const c of base) if (r.checks.includes(c.key)) { c.status = r.status; c.reason = r.reason; if (r.value != null) c.value = r.value; }
  return base;
}

function channelOf(n) {
  const rate = n.rate || {};
  return {
    fresh: !!rate.fresh,
    rate_up: rate.up_bps == null ? null : rate.up_bps / 8,
    rate_down: rate.down_bps == null ? null : rate.down_bps / 8,
    tcp_connections: rate.tcp_connections ?? null,
    port: n.relay_port ?? n.connection?.port ?? null,
    daily: { up: n.traffic?.today?.up ?? 0, down: n.traffic?.today?.down ?? 0 },
    last_active: sh(rate.last_active),
    sample_at: sh(rate.sample_at),
    stats_valid: n.traffic?.stats_valid !== false,
  };
}

function assetOf(n, rp = replayState()) {
  const h = n.host;
  const st = statusOf(n, rp);
  const lat = n.latency || {};
  const pt = D.nodes[n.id]?.probe_tasks || {};
  const b = n.billing || {};
  const isRelay = n.team === '线路机';
  const r = isRelay ? relayByName[n.name] : null;
  return {
    uuid: n.id,
    name: n.full_name,
    ip: n.ip,
    team: n.team,
    relay: isRelay ? n.name : n.relay,
    relay_port: n.relay_port,
    provider: n.provider,
    plan: b.plan,
    note: D.nodes[n.id]?.note || '',
    price_cents: b.price_cents,
    due_days: b.due_days,
    due_date: b.due_date,
    billing_timezone: b.billing_timezone,
    supplier_timezone: b.supplier_timezone,
    billing_cycle: b.cycle_word || b.cycle,
    revision: D.nodes[n.id]?.revision || 1,
    traffic_quota_bytes: null,
    monthly: { up: n.traffic?.month?.up ?? 0, down: n.traffic?.month?.down ?? 0 },
    daily: { up: n.traffic?.today?.up ?? 0, down: n.traffic?.today?.down ?? 0 },
    channel: channelOf(n),
    host: h ? { cpu: h.cpu_pct, ram: h.mem_used, ram_total: h.mem_total, disk: h.disk_used, disk_total: h.disk_total, online: h.online } : null,
    host_measured: !!h,
    host_fresh: !!h?.fresh,
    capacity: n.bandwidth?.capacity_bps ? { bps: n.bandwidth.capacity_bps, source: n.bandwidth.source, measurement: n.bandwidth.measurement, upload_percent: n.bandwidth.util_up_pct, download_percent: n.bandwidth.util_down_pct } : null,
    profile: null,
    // 首页（LuminaPlus 补丁）读取的附加字段
    health: { status: st.code, reason: st.reason, since: sh(st.since), summary_counts: n.status.summary_counts },
    probe: { egress_ms: st.code === 'fail' ? null : lat.egress_ms, egress_at: sh(lat.egress_at), actual_ip: lat.egress_actual_ip, tcp_ms: lat.tcp_ms, tcp_at: sh(lat.tcp_at) },
    region: n.region?.code, tcp_task_id: pt.tcp_task_id ?? null, egress_task_id: pt.egress_task_id ?? null, ping_task_id: pt.ping_task_id ?? null,
    started_at: sh(S.system.service_started_at),
  };
}

export function overview() {
  const rp = replayState();
  const assets = S.nodes.map(n => assetOf(n, rp));
  const groups = {};
  for (const g of S.groups) groups[g.team] = {
    count: g.count, rate_up: g.rate_up_bps / 8, rate_down: g.rate_down_bps / 8, rate_complete: g.rate_complete,
    daily_bytes: g.today_bytes, monthly_cents: g.monthly_cents, active: g.active,
  };
  const relays = {};
  for (const r of S.relays) relays[r.name] = { fresh: r.collector.fresh, config_valid: r.config.valid, logging: r.config.logging, routing_hash: r.config.routing_hash };
  return {
    now: sh(AS_OF), started_at: sh(S.system.service_started_at), assets, groups, relays, direct: [],
    monthly_cents: S.kpis.monthly_cost.cents, events_today: 0,
    collector: { running: S.system.collector.running, waiting: S.system.collector.waiting },
    backup: { integrity: S.system.backup.integrity, verified_at: sh(S.system.backup.verified_at) },
    hosts_at: sh(S.system.hosts_at),
  };
}

function daysOf(n) {
  const t = n.availability?.tracker30;
  if (!t) return [];
  const start = new Date(t.start + 'T00:00:00Z');
  return t.coverage.map((c, i) => ({ date: new Date(start.getTime() + i * 86400000).toISOString().slice(0, 10), coverage: c, incidents: t.incidents[i] }));
}

export function health() {
  const rp = replayState();
  const nodes = {};
  const counts = { ok: 0, degraded: 0, fail: 0, unknown: 0, maintenance: 0 };
  const attention = [];
  for (const n of S.nodes) {
    const st = statusOf(n, rp);
    counts[st.code] = (counts[st.code] || 0) + 1;
    if (st.code === 'fail' || st.code === 'degraded') attention.push(n.id);
    const lat = n.latency || {};
    nodes[n.id] = {
      uuid: n.id, name: n.full_name, team: n.team, relay: n.relay,
      status: st.code, reason: st.reason, since: sh(st.since),
      checks: checksOf(n, rp),
      metrics: {
        cpu: n.host?.cpu_pct ?? null, memory: n.host?.mem_pct ?? null, disk: n.host?.disk_pct ?? null,
        tcp_ms: lat.tcp_ms ?? null, egress_ms: st.code === 'fail' ? null : (lat.egress_ms ?? null), path_ms: lat.path_ms ?? null, egress_pending: !!lat.egress_pending,
      },
      windows: { 30: n.availability?.windows?.['30'] },
      days: daysOf(n),
      checked_at: sh(n.status.checked_at),
    };
  }
  return { version: '1.2.0', ok: true, monitor: { checked_at: sh(S.system.monitor.checked_at), nodes, counts, attention, unknown: [] } };
}

export function history() {
  const f = S.traffic.fleet_24h;
  const out = [];
  for (let i = 0; i < f.down.length; i += 5) {
    const d = f.down.slice(i, i + 5), u = f.up.slice(i, i + 5);
    const valid = d.some(v => v != null);
    out.push({ t: sh(f.t0 + i * f.step), up: valid ? u.reduce((s, v) => s + (v || 0), 0) : null, down: valid ? d.reduce((s, v) => s + (v || 0), 0) : null });
  }
  return out;
}

function incidentOf(i) {
  return { id: i.id, asset: i.node, key: i.check, check: i.check, title: i.title, severity: i.severity, opened_at: sh(i.opened_at), last_seen: sh(i.last_seen), resolved_at: sh(i.resolved_at), ack_at: sh(i.ack_at), ack_actor: i.ack_actor, note: i.note, reason: i.reason, evidence: { ...(i.evidence || {}), reason: i.reason } };
}

export function incidents() { return X.incidents.map(incidentOf); }
export function renewals() { return X.renewals.map(r => ({ id: r.id, ts: sh(r.ts), asset: r.node, actor: r.actor, amount_cents: r.amount_cents, old_due: r.old_due, new_due: r.new_due, note: r.note })); }

export function komariLatest() {
  const out = {};
  const iso = t => new Date(t * 1000).toISOString();
  for (const n of S.nodes) {
    const h = n.host;
    if (!h) { out[n.id] = { client: n.id, online: !!n.agent?.online, time: iso(sh(AS_OF)), cpu: 0, ram: 0, ram_total: 0, disk: 0, disk_total: 0, net_in: 0, net_out: 0, net_total_up: 0, net_total_down: 0, load: 0, uptime: 0, process: 0, connections: 0, connections_udp: 0 }; continue; }
    out[n.id] = { client: n.id, online: h.online, time: iso(sh(h.reported_at)), cpu: h.cpu_pct, gpu: 0, ram: h.mem_used, ram_total: h.mem_total, swap: h.swap_used, swap_total: h.swap_total, load: h.load1, load5: h.load5, load15: h.load15, temp: 0, disk: h.disk_used, disk_total: h.disk_total, net_in: h.net_rx_bps / 8, net_out: h.net_tx_bps / 8, net_total_up: h.net_total_tx, net_total_down: h.net_total_rx, process: h.processes, connections: h.tcp_conns, connections_udp: h.udp_conns, uptime: h.uptime_s };
  }
  return out;
}

export function pingTasks() {
  return X.probes.map(p => ({ id: p.id, interval: p.interval_s, name: p.name, loss: 0, clients: p.executors, type: p.type, target: p.target, weight: p.id }));
}

export function pingRecords(q) {
  const uuid = q.get('uuid'), task = q.get('task_id');
  const records = [];
  const tasks = pingTasks();
  for (const n of S.nodes) {
    const pt = D.nodes[n.id]?.probe_tasks || {};
    if (uuid && n.id !== uuid) continue;
    if (task && String(pt.tcp_task_id) !== String(task)) continue;
    const st = n.latency?.strip_tcp;
    if (!st || pt.tcp_task_id == null) continue;
    st.ms.forEach((ms, k) => records.push({ task_id: pt.tcp_task_id, time: new Date(sh(st.t_end - st.dt[k]) * 1000).toISOString(), value: st.lost[k] ? -1 : ms, client: n.id }));
  }
  const used = new Set(records.map(r => r.task_id));
  return { count: records.length, records, tasks: tasks.filter(t => used.has(t.id)) };
}

export function node(id) {
  const n = byId[id];
  if (!n) return null;
  const h = health().monitor.nodes[id];
  const eg = n.latency?.strip_egress;
  const now = sh(AS_OF);
  const hist = eg ? eg.ms.map((ms, k) => ({ asset: id, bucket: now - (eg.ms.length - k) * 60, ok: eg.lost[k] ? 0 : 1, failed: eg.lost[k] ? 1 : 0, unknown: 0, maintenance: 0, data: { egress_ms: ms } })) : [];
  if (h.status === 'fail') for (const r of hist.slice(-6)) { r.ok = 0; r.failed = 1; r.data.egress_ms = null; }
  const tr = T5.nodes[id];
  const traffic = tr ? tr.down.map((d, k) => ({ t: sh(tr.t0 + k * tr.step), down: d, up: tr.up[k] })) : [];
  const rows = (S.access_recent[id] || []).map((r, k) => ({ id: k + 1, ts: sh(r.ts), asset: id, team: n.team, relay: n.relay, source_ip: r.source_ip, host: r.host, port: r.port, protocol: r.protocol, action: r.action, client: r.client, kind: r.kind }));
  const w = D.nodes[id]?.availability?.windows || n.availability?.windows;
  return { asset: assetOf(n), health: h, history: hist, windows: w, incidents: (D.nodes[id]?.incidents || []).map(incidentOf), audit: [], traffic, activity: { rows } };
}

export function panel() {
  const relays = {};
  for (const r of S.relays) {
    const inb = X.entries.filter(e => e.relay === r.name).map(e => {
      const bound = byId[e.bound_node];
      return {
        kind: e.kind, id: e.port, port: e.port, protocol: e.inbound_protocol, security: e.security,
        client_uuid: (e.client?.credential_prefix || '00000000') + '-demo-4000-8000-000000000000',
        reality: e.reality, source_ips: e.source_ips || D.nodes[e.bound_node]?.connection_source_ips || [],
        outbound_protocol: e.outbound?.protocol, remark: e.remark, tag: e.tag, enable: e.enable, email: e.client?.label,
        online: e.active, last_active: sh(e.last_active), tcp_connections: e.tcp_connections,
        up: e.cumulative?.up, down: e.cumulative?.down, total: e.cumulative?.total_limit || 0,
        asset_uuid: e.bound_node, asset_name: bound?.full_name, team: e.team,
        outbound_tag: e.outbound?.outbound_tag, target: e.outbound?.target, target_port: e.outbound?.target_port,
        egress_expected: e.egress?.expected_ip ?? (e.kind === 'master' ? r.ip : null),
      };
    });
    relays[r.name] = {
      ok: r.panel.ok, error: r.panel.error, version: r.versions.panel, xray_version: r.versions.xray, xray_running: r.panel.xray_running,
      cpu: r.panel.cpu_pct, mem_used: r.panel.mem_used, mem_total: r.panel.mem_total,
      checked_at: sh(AS_OF) - 2, fresh: true, onlines_count: r.panel.online_clients, inbounds: inb,
    };
  }
  return { relays };
}

export function audit() {
  return X.audit_recent.map(a => ({ id: a.id, ts: sh(a.ts), actor: a.actor, action: a.action, target: a.target.startsWith('asset:') ? a.target.slice(6) : a.target, detail: a.detail }));
}

export function defaults() {
  const next = {};
  for (const r of S.relays) next[r.name] = Math.max(...X.entries.filter(e => e.relay === r.name).map(e => e.port)) + 1;
  return { relay_names: S.relays.map(r => r.name), public_key: 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA演示公钥 team-ops-provision', next_ports: next, teams: ['团队1', '团队2'], providers: ['ATTVPS', 'NodeMach', 'OhnoICU'], panel_version: '3.7.0', packages: { xui: { version: '3.7.0' }, agent: { version: '1.5.0' } } };
}

function jobOf(j) {
  const ok = j.steps.filter(s => s.status === 'succeeded').length;
  return { id: j.id, kind: j.type === 'relay' ? 'relay' : 'node', title: (j.type === 'relay' ? '新增线路机 · ' : '新增家宽节点 · ') + j.title, status: j.status === 'succeeded' ? 'succeeded' : j.status === 'rolled_back' ? 'rolled_back' : j.status, created_at: sh(j.started_at), finished_at: sh(j.finished_at), step_summary: `${ok}/${j.steps.length}` };
}
export function jobs(limit = 5) { return { jobs: S.onboarding.history.slice(0, limit).map(jobOf) }; }
export function job(id) {
  const j = S.onboarding.history.find(x => x.id === id);
  if (!j) return null;
  let t = j.started_at;
  const steps = j.steps.map(s => { const st = sh(t); t += s.duration_s || 0; return { key: s.key, name: s.name, status: s.status === 'succeeded' ? 'ok' : s.status, started_at: st, finished_at: sh(t), log: [`${s.name} …`, `完成（${s.duration_s ?? '—'} 秒）`], error: s.error }; });
  return { ...jobOf(j), params: { relay: j.relay, team: j.team }, steps, result: { name: j.title, relay: j.relay, port: null, egress_ip: j.title.split(' · ').at(-1) }, error: j.error };
}

export function behavior(q) {
  const B = X.behavior;
  const asset = q.get('asset'), team = q.get('team');
  const keep = id => (!asset || id === asset) && (!team || byId[id]?.team === team);
  const nodes = Object.entries(B.nodes).filter(([id]) => keep(id)).map(([id, v]) => ({ asset: id, team: byId[id]?.team, connections: v.connections, domains: v.domains, sources: v.sources, blocked: v.blocked, ip_only: v.ip_only, last_seen: sh(v.last_seen) }));
  const targets = B.top_targets_by_node.filter(t => keep(t.node)).sort((a, b) => b.connections - a.connections).slice(0, 100).map(t => ({ asset: t.node, host: t.host, kind: t.kind, connections: t.connections, sources: t.sources, blocked: t.blocked, first_seen: sh(t.first_seen), last_seen: sh(t.last_seen), total_recorded: t.total_recorded }));
  const firstNode = ip => (B.timeline_recent.find(r => r.source_ip === ip) || {}).node || 'r1';
  const sources = B.top_sources.map(s => ({ asset: firstNode(s.source_ip), source_ip: s.source_ip, connections: s.connections, targets: s.domains, last_seen: sh(s.last_seen) })).filter(s => keep(s.asset));
  const events = B.events_recent.filter(e => keep(e.node)).map(e => ({ id: e.id, asset: e.node, kind: e.kind, subject: e.subject, ts: sh(e.ts), ack_at: e.acked ? sh(e.ts) : null, message: e.message }));
  return { now: sh(AS_OF), nodes, targets, sources, events, offset: 0, next_offset: B.top_targets_by_node.length > 100 ? 100 : null, computed_at: sh(B.computed_at) };
}

export function activity(q) {
  const B = X.behavior;
  const asset = q.get('asset');
  const rows = B.timeline_recent.filter(r => !asset || r.node === asset).slice(0, 100).map((r, k) => ({ id: k + 1, ts: sh(r.ts), asset: r.node, team: r.team, relay: r.relay, source_ip: r.source_ip, host: r.host, port: r.port, protocol: r.protocol, action: r.action, client: r.client, kind: r.kind }));
  return { rows, domains: B.top_domains.map(d => ({ host: d.host, connections: d.connections })), sources: B.top_sources.map(s => ({ source_ip: s.source_ip, domains: s.domains, connections: s.connections })), total: B.kpis.connections, next_cursor: 'demo' };
}

export function policies() { return { active: {}, drafts: {}, versions: [], draft_hash: 'empty' }; }

export function apiKeys() {
  const t = sh(AS_OF);
  return [
    { id: 5, name: 'agent', scope: 'full', prefix: 'tlm_6a1f09c2', created_at: t - 86400 * 6, expires_at: t + 86400 * 84, last_used_at: t - 40, use_count: 812, revoked_at: null },
    { id: 4, name: 'dashboard-read', scope: 'read', prefix: 'tlm_c03d7e51', created_at: t - 86400 * 5, expires_at: null, last_used_at: t - 3600 * 5, use_count: 96, revoked_at: null },
    { id: 3, name: 'ops-automation', scope: 'full', prefix: 'tlm_9b22ad17', created_at: t - 86400 * 7, expires_at: t + 86400 * 23, last_used_at: t - 86400, use_count: 22, revoked_at: null },
    { id: 2, name: 'test-cleanup', scope: 'full', prefix: 'tlm_1e7f0b3a', created_at: t - 86400 * 8, expires_at: t + 86400 * 22, last_used_at: t - 86400 * 8, use_count: 3, revoked_at: t - 86400 * 8 },
    { id: 1, name: 'smoke', scope: 'full', prefix: 'tlm_77aa0c64', created_at: t - 86400 * 9, expires_at: t - 86400, last_used_at: t - 86400 * 9, use_count: 1, revoked_at: t - 86400 * 9 },
  ];
}

let schemaCache;
export function schema() {
  if (schemaCache) return schemaCache;
  const md = fs.readFileSync(path.join(ROOT, 'research/inventory/api.md'), 'utf8');
  const endpoints = [];
  let method = null;
  for (const line of md.split('\n')) {
    if (line.startsWith('## 2. GET')) method = 'GET';
    else if (line.startsWith('## 3. POST')) method = 'POST';
    else if (line.startsWith('## 4')) method = null;
    const m = method && line.match(/^\| `([^`]+)`[^|]*\| ([a-z内部（）密钥禁用会话路由单独注册，要求 admin]+?) \|[^|]*\|([^|]*)\|/);
    if (m) endpoints.push({ method, path: m[1].split('（')[0], scope: /full/.test(m[2]) ? 'full' : /write/.test(m[2]) ? 'write' : 'read', description: m[3].trim().replace(/`/g, '').slice(0, 120) });
  }
  schemaCache = { auth: { header: 'X-Api-Key', base_url: 'https://38.58.57.22/ops/api', example: 'curl -H "X-Api-Key: tlm_…" https://38.58.57.22/ops/api/overview' }, endpoints, scopes: {}, ui_actions: [], ai_notes: '' };
  return schemaCache;
}

export function apiUsage() {
  const t = sh(AS_OF);
  const paths = ['/overview', '/health', '/jobs/6711498f', '/activity', '/node', '/behavior', '/renewals', '/assets/edit'];
  return Array.from({ length: 24 }, (_, k) => ({ ts: t - k * 137, key_id: 5, name: k % 5 === 3 ? 'dashboard-read' : 'agent', method: paths[k % paths.length] === '/assets/edit' ? 'POST' : 'GET', path: paths[k % paths.length], status: k === 6 ? 409 : k === 11 ? 404 : 200, ip: '203.0.113.' + (60 + (k % 3)) }));
}

export function route(pathname, q) {
  switch (pathname) {
    case '/overview': return overview();
    case '/health': return health();
    case '/history': return history();
    case '/incidents': return incidents();
    case '/renewals': return renewals();
    case '/node': return node(q.get('asset'));
    case '/panel/overview': return panel();
    case '/audit': return audit();
    case '/provision/defaults': return defaults();
    case '/jobs': return jobs(Number(q.get('limit') || 5));
    case '/behavior': return behavior(q);
    case '/activity': return activity(q);
    case '/policies': return policies();
    case '/policies/preview': return { draft_hash: 'empty', policies: {}, nodes: S.nodes.filter(n => n.team === '团队2').map(n => ({ uuid: n.id, name: n.full_name, relay: n.relay, policy: { mode: 'denylist' }, would_block: { connections: 0, domains: [] } })), routing_hashes: {}, notice: '预览基于最近 24 小时的访问记录模拟；域名被隐藏或只有 IP 的连接无法匹配白名单。' };
    case '/api-keys': return apiKeys();
    case '/schema': return schema();
    case '/api-keys/usage': return apiUsage();
    case '/fleet': return { relays: [], entries: [] };
    case '/panel/action': return { ok: true, msg: '演示环境：操作未执行', uuid: '00000000-demo-4000-8000-000000000000', lines: Array.from({ length: 40 }, (_, k) => `2026/09/23 15:${String(20 + (k >> 2)).padStart(2, '0')}:${String((k * 7) % 60).padStart(2, '0')} [Info] [${1000 + k}] proxy/vless/inbound: firstLen = 517`), obj: { entries: [{ entry_port: 20002, ok: true, ms: 842, actual_ip: '198.51.100.11', measured_at: sh(AS_OF) }] } };
    default:
      if (pathname.startsWith('/jobs/')) return job(decodeURIComponent(pathname.slice(6)));
      return undefined;
  }
}

// ---- 原生 Komari 外壳需要的最小接口 ----
export const site = S.site;
export function komariClients() {
  return S.nodes.map((n, k) => ({
    uuid: n.id, token: '••••', name: n.full_name, cpu_name: n.host?.cpu_model || '', virtualization: n.host?.virtualization || '', arch: n.host?.arch || '', cpu_cores: n.host?.cores || 0,
    os: n.capabilities.includes('probe') ? '连通性探针' : (n.host?.os || ''), kernel_version: n.host?.kernel || '', gpu_name: '', region: n.region?.flag || '',
    remark: D.nodes[n.id]?.remark_private || '', public_remark: D.nodes[n.id]?.remark_public || '', mem_total: n.host?.mem_total || 0, swap_total: n.host?.swap_total || 0, disk_total: n.host?.disk_total || 0,
    version: n.agent?.version || '', weight: k, price: Number(n.billing?.price || 0), billing_cycle: 30, auto_renewal: false, currency: '$', expired_at: n.billing?.expired_at, group: n.team, tags: ['家宽', ...(n.tags || [])].join(';'), hidden: false,
    traffic_limit: 0, traffic_limit_type: 'max', ipv4: n.ip, ipv6: '', created_at: '2026-09-14T08:00:00+08:00', updated_at: '2026-09-23T15:00:00+08:00',
  }));
}
