// 浏览器内 mock：拦截 /api/v1/* 的 fetch 和实时流 WebSocket。VITE_MOCK=1 时由 main.tsx 安装。
// 有状态：改名、退役、上架、续费等操作都会改内存里的数据，并通过 mock 实时流通知前端刷新。

import type { Availability, AvailabilityDay, EntryView, Incident, Job, JobLog, JobSummary, MachineView, ProbeBucket, ProvisionCheckItem, ProvisionItem, SeriesPoint } from '@/lib/types';
import { addDays, addMonths, displayName } from '@/lib/format';
import { isQuiet, liveRate } from '@/lib/derive';
import { makeDefaultCompare } from '@/lib/sort';
import { cycleMonths, META, subCalc } from './meta';

const LIFECYCLE_LABEL = META.lifecycles;
import { buildDb, countryOf, hashStr, IP_STEPS, rng, trafficOf, type Db } from './fixtures';
import { installAccessMock } from './access';
import { installPolicyMock } from './policy';
import { installRoutersMock } from './routers';
import { installTerminalMock } from './terminal';

const db: Db = buildDb();
const DAY = 86400_000;
const iso = (t = Date.now()) => new Date(t).toISOString();
const GiB = 1024 ** 3;

class HttpErr extends Error { constructor(public status: number, public code: string, msg: string) { super(msg); } }
const bad = (msg: string, status = 400, code = 'bad_request') => new HttpErr(status, code, msg);

// ---------- 实时流 ----------
type Ev = { type: string; id?: string; [k: string]: unknown };
const listeners = new Set<(e: Ev) => void>();
const emit = (e: Ev) => listeners.forEach((l) => l(e));

/** 只顶替 /api/v1/stream 的 WebSocket（Vite 热更新自己的 WebSocket 照常） */
class MockStream {
  readyState = 0; onmessage: ((e: MessageEvent) => void) | null = null; onopen: (() => void) | null = null; onclose: (() => void) | null = null; onerror: (() => void) | null = null;
  private fn = (e: Ev) => this.onmessage?.(new MessageEvent('message', { data: JSON.stringify(e) }));
  // 同中控：每 15 s 一个文本心跳，前端 45 s 收不到任何消息就当连接已死
  private hb = setInterval(() => this.fn({ type: 'hb' }), 15_000);
  constructor() {
    listeners.add(this.fn);
    setTimeout(() => { if (this.readyState === 0) { this.readyState = 1; this.onopen?.(); } }, 50);
  }
  send() {}
  close() { this.readyState = 3; listeners.delete(this.fn); clearInterval(this.hb); }
}

// ---------- 工具 ----------
const machine = (id: string) => { const m = db.machines.find((x) => x.id === id); if (!m) throw bad('机器不存在', 404, 'not_found'); return m; };
const teamOf = (id: string) => { const t = db.teams.find((x) => x.id === id); if (!t) throw bad('团队不存在', 404, 'not_found'); return t; };
const clone = <T,>(x: T): T => JSON.parse(JSON.stringify(x));

/** 由机器算出来的字段：团队 / 套餐的台数，宿主机的同机 IP 节点名单（未退役的，按默认顺序） */
function refreshCounts() {
  for (const t of db.teams) t.node_count = db.machines.filter((m) => m.team?.id === t.id && m.lifecycle !== 'retired').length;
  for (const p of db.plans) p.machine_count = db.machines.filter((m) => m.plan?.id === p.id && m.lifecycle !== 'retired').length;
  const order = makeDefaultCompare(db.teams);
  for (const m of db.machines) recomputeDue(m);
  for (const m of db.machines) m.ip_nodes = db.machines.filter((x) => x.host_machine?.id === m.id && x.lifecycle !== 'retired').sort(order).map((x) => ({ id: x.id, name: x.name }));
}
function renameMachine(m: MachineView) {
  if (m.kind === 'relay') m.name = m.label ?? m.name;
  else if (m.team && m.number != null) m.name = displayName(m.team.name, m.number);
  for (const x of db.machines) {
    if (x.host_machine?.id === m.id) x.host_machine.name = m.name;
    for (const e of x.entries) {
      if (e.node.id === m.id) e.node.name = m.name;
      if (e.relay.id === m.id) e.relay.name = m.name;
    }
  }
}
/** 同团队在用节点压缩成 01..N；exclude 为即将退役的机器（退役宿主机时连同它的同机 IP 节点）。返回改名清单 */
function compactPlan(teamId: string, exclude: string[] = []) {
  const team = teamOf(teamId);
  const list = db.machines.filter((x) => x.team?.id === teamId && !exclude.includes(x.id) && x.lifecycle !== 'retired' && x.number != null).sort((a, b) => a.number! - b.number!);
  return list.map((x, i) => ({ m: x, n: i + 1, old_name: x.name, new_name: displayName(team.name, i + 1) })).filter((c) => c.m.number !== c.n);
}
/** 压缩同团队编号并记一条审计 */
function compactApply(tid: string, why: string) {
  const plan = compactPlan(tid);
  for (const c of plan) { c.m.number = c.n; c.m.revision++; renameMachine(c.m); }
  if (plan.length) db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: 'system', action: 'team.compact', title: '自动压缩编号', summary: `${why} ${plan.length} 台前移`, target_kind: 'team', target_id: tid, detail: { count: plan.length } });
}
/** 回收站的一行（同服务端 recycle::record）：name 是退役前的名字，进过备用的 last_name 是回收时的备用名 */
function recycleRecord(m: MachineView) {
  const up = m.entries.reduce((a, e) => a + e.traffic.month_up, 0), down = m.entries.reduce((a, e) => a + e.traffic.month_down, 0);
  const s = m.subscription;
  db.recycle.unshift({
    id: Math.max(0, ...db.recycle.map((r) => r.id)) + 1, source: 'fleet', source_id: m.id, machine_id: m.id, kind: m.kind,
    name: m.spare_from ?? m.name, last_name: m.spare_from ? m.name : null, team: m.spare_from ? m.spare_from.replace(/-\d+$/, '') : m.team?.name ?? null, ip: m.ip, host_name: m.host_machine?.name ?? null,
    provider: m.provider?.name ?? null, plan: m.plan?.name ?? null, service_ref: m.service_ref, price_cents: s?.price_cents ?? null, currency: s?.currency ?? null, cycle: s?.cycle ?? null,
    activated_at: m.activated_at, retired_at: m.spare_since ?? m.retired_at, due_date: s?.due_date ?? null, recycled_at: iso(), reason: m.lifecycle_reason, traffic_up: up, traffic_down: down, nic_rx: null, nic_tx: null,
    actor: db.user, snapshot: {
      machine: { note: m.note }, name: m.name, team: m.team?.name ?? null, host: m.host_machine?.name ?? null, provider: m.provider?.name ?? null, plan: m.plan?.name ?? null, subscription: s ? clone(s) : null,
      renewals: (db.renewals[m.id] ?? []).map((r) => ({ id: r.id, old_due: r.old_due ?? r.new_due, new_due: r.new_due, amount_cents: r.amount_cents ?? 0, currency: r.currency, note: r.note ?? '', actor: r.actor, created_at: r.created_at })),
      entries: m.entries.map((e) => ({ relay: e.relay.name, node: e.node.name, port: e.port, lifecycle: e.lifecycle, created_at: m.activated_at ?? iso(), removed_at: e.lifecycle === 'removed' ? iso() : null })),
      outbound: { protocol: 'shadowsocks', host: m.ip, port: db.ssPorts[m.id] ?? 8388, method: 'aes-256-gcm', username: null, updated_at: m.activated_at ?? iso() },
    },
  });
}
/** 立即回收完成（同服务端 finalize）：记进回收站，机器从列表里消失，原团队编号前移补号 */
function finishRetire(m: MachineView) {
  m.lifecycle = 'retired'; m.retired_at = iso(); m.active_job = null;
  const tid = m.team?.id;
  recycleRecord(m);
  db.machines = db.machines.filter((x) => x !== m);
  for (const x of db.machines) if (x.kind === 'relay') x.entries = x.entries.filter((e) => e.node.id !== m.id);
  if (tid) compactApply(tid, `${m.name} 回收后`);
  refreshCounts();
  emit({ type: 'machines' });
}
/** 距到期天数与服务端算的几项（到期状态、月费、续一期的到期日、每期天数） */
function recomputeDue(m: MachineView) {
  if (m.subscription) { m.subscription.due_days = Math.ceil((new Date(m.subscription.due_date).getTime() - Date.now()) / DAY); subCalc(m.subscription); }
  m.recycle_on = m.spare && m.lifecycle === 'active' && m.subscription ? addDays(m.subscription.due_date, 2) : null; // 备用：到期 + 2 天 0 点起自动回收
}

function overview() {
  const live = db.machines.filter((m) => m.lifecycle === 'active');
  const cnt = (list: MachineView[]) => ({
    total: list.length, ok: list.filter((m) => m.health.status === 'ok').length, degraded: list.filter((m) => m.health.status === 'degraded').length,
    down: list.filter((m) => m.health.status === 'down').length, unknown: list.filter((m) => m.health.status === 'unknown').length, maintenance: list.filter((m) => m.health.status === 'maintenance').length,
  });
  let up = 0; let down = 0;
  for (const m of live.filter((x) => x.kind !== 'relay')) for (const e of m.entries) { up += e.traffic.month_up; down += e.traffic.month_down; }
  return {
    nodes: cnt(live.filter((m) => m.kind !== 'relay')), relays: cnt(live.filter((m) => m.kind === 'relay')),
    provisioning: db.machines.filter((m) => m.lifecycle === 'provisioning').length, retiring: db.machines.filter((m) => m.lifecycle === 'retiring').length,
    failed: db.machines.filter((m) => m.lifecycle === 'failed').length,
    // 需要续费的按订阅的 due_state（同服务端）：自动续费的只在已过期时算
    // 在线/离线按 agent 连通（同 isQuiet），只数有自己 agent 的（家宽代理、同机 IP 节点不计）
    online: live.filter((m) => m.agent && !m.host_machine && !isQuiet(m)).length, offline: live.filter((m) => m.agent && !m.host_machine && isQuiet(m)).length,
    due_soon: live.filter((m) => !m.spare && m.subscription?.due_state === 'soon').length, overdue: live.filter((m) => !m.spare && m.subscription?.due_state === 'overdue').length,
    monthly_cost: Object.entries(live.reduce<Record<string, number>>((acc, m) => { if (m.subscription) acc[m.subscription.currency] = (acc[m.subscription.currency] ?? 0) + m.subscription.monthly_cents; return acc; }, {})).map(([currency, cents]) => ({ currency, cents })),
    traffic_month: { up, down },
    // 实时速率只算数据没过期的（同 liveRate）；同机 IP 节点的读数就是宿主机的，不重复算
    net_rx_bps: live.reduce((s, m) => s + (m.host_machine ? 0 : liveRate(m)?.rx ?? 0), 0), net_tx_bps: live.reduce((s, m) => s + (m.host_machine ? 0 : liveRate(m)?.tx ?? 0), 0),
  };
}

function series(m: MachineView, metric: string, range: string): { metric: string; range: string; resolution: string; points: SeriesPoint[] } {
  const spec: Record<string, [number, number]> = { '1h': [60, 60_000], '6h': [72, 300_000], '24h': [96, 900_000], '7d': [84, 7200_000], '30d': [90, 28800_000] };
  const [n, step] = spec[range] ?? spec['1h'];
  const hourly = range === '7d' || range === '30d';
  const r = rng(hashStr(m.id + metric));
  const end = Date.now(); const pts: SeriesPoint[] = [];
  const h = m.host; const isDown = m.health.status === 'down';
  const cpu0 = h?.cpu_pct ?? 20; let walk = cpu0;
  // 只有上架之后、退役之前有数据：刚上架的点数不够画线，抽屉写「刚上架，数据积累中」
  const from = m.activated_at ? Date.parse(m.activated_at) : end; const to = m.retired_at ? Date.parse(m.retired_at) : end;
  for (let i = 0; i < n; i++) {
    const t = end - (n - 1 - i) * step; const ts = iso(t);
    walk = Math.max(2, Math.min(97, walk + (r() - 0.5) * 9));
    if (t < from || t > to) continue;
    const tail = isDown && i > n - 6;
    if (metric === 'host') {
      // 按小时的（同服务端 HOST_HOURLY）另有每小时峰值 cpu_max、没有 load1
      const cpu: Record<string, number | null> = tail ? { cpu_pct: null } : hourly ? { cpu_pct: walk, cpu_max: Math.min(100, walk + 5 + r() * 25) } : { cpu_pct: walk, load1: +(walk / 40).toFixed(2) };
      pts.push({ ts, ...cpu, mem_pct: tail ? null : (h?.mem_used != null && h.mem_total ? (h.mem_used / h.mem_total) * 100 : 40) + Math.sin(i / 9) * 4, disk_pct: h?.disk_used != null && h.disk_total ? (h.disk_used / h.disk_total) * 100 + i * 0.01 : 30, net_rx_bps: tail ? null : (h?.net_rx_bps ?? 1e7) * (0.5 + r()), net_tx_bps: tail ? null : (h?.net_tx_bps ?? 1e7) * (0.4 + r()) });
    } else if (metric === 'traffic') {
      const k = step / 60_000;
      pts.push({ ts, up: Math.round(r() * 8e6 * k * (1 + Math.sin(i / 7))), down: Math.round(r() * 3e7 * k * (1 + Math.cos(i / 8))), conns: Math.round(20 + r() * 80) });
    } else {
      const tcpFail = tail || r() < 0.015; const egFail = tail || r() < 0.02;
      pts.push({ ts, tcp_ok: !tcpFail, tcp_ms: tcpFail ? null : Math.round(40 + r() * 70 + (i % 23 === 0 ? 120 : 0)), egress_ok: !egFail, egress_ms: egFail ? null : Math.round(160 + r() * 90), tcp_n: 2, tcp_ok_n: tcpFail ? 0 : 2 });
    }
  }
  return { metric, range, resolution: hourly ? '1h' : '1m', points: pts };
}

/** 心跳格：在用机器 20 个墙钟对齐的 3 分钟桶（旧→新，同服务端 date_bin），最后一桶是当前这格（只走了一部分），
 *  [探测次数, 成功次数, 平均 TCP ms]；线路机合计它的全部入口 */
function probeBeats() {
  const out: Record<string, ProbeBucket[]> = {};
  const now = Date.now(); const slot = Math.floor(now / 180_000);
  const curMin = 1 + Math.floor((now % 180_000) / 60_000); // 当前这格已经过了几分钟（含正在走的这一分钟）
  for (const m of db.machines) {
    if (m.lifecycle !== 'active' || !m.entries.length) continue;
    const isDown = m.health.status === 'down';
    out[m.id] = Array.from({ length: 20 }, (_, i): ProbeBucket => {
      const per = m.kind === 'relay' ? 2 * (i === 19 ? curMin : 3) : m.entries.length * (i === 19 ? curMin : 3); // 节点：每条入口每分钟探测一次；线路机：中控每 30 秒连它一次
      const r = rng(hashStr(`${m.id}:${slot - 19 + i}`));
      if (m.id.endsWith('2') && i < 4) return [0, 0, null]; // 演示：刚开始探测的那段没有数据
      const fail = isDown && i >= 17 ? per : r() < 0.06 ? 1 + Math.floor(r() * per * 0.6) : 0;
      return [per, per - fail, fail === per ? null : Math.round(40 + r() * 70 + (r() < 0.05 ? 120 : 0))];
    });
  }
  return out;
}
/** 近 N 天可用率（同服务端 /availability）：按账单时区分天，上架之前、退役之后的天没有探测；每条入口每天 TCP 2880 次（30 秒一次）、
 *  出口 1440 次，今天按已过去的部分算。大多数天全成功，个别天有零星失败，故障机最近两天大面积失败 */
function availability(m: MachineView, days: number): Availability {
  if (!Number.isInteger(days) || days < 1 || days > 90) throw bad('days 只能是 1–90');
  const fmt = new Intl.DateTimeFormat('en-CA', { timeZone: m.subscription?.billing_tz ?? META.org_timezone }); // 账单时区里的 YYYY-MM-DD
  const today = fmt.format(Date.now());
  const from = m.activated_at && fmt.format(Date.parse(m.activated_at));
  const to = m.retired_at ? fmt.format(Date.parse(m.retired_at)) : today;
  // 有探测的入口数：应用中的还没有探测；节点至少算一条（已退役机器的入口已摘掉、fixtures 里是空的，退役前照样有探测）
  const live = m.entries.filter((e) => e.lifecycle !== 'pending').length;
  const n = m.kind === 'relay' ? live : Math.max(1, live);
  const elapsed = (Date.now() - new Date().setHours(0, 0, 0, 0)) / DAY;
  const items = Array.from({ length: days }, (_, i): AvailabilityDay => {
    const date = addDays(today, i - days + 1);
    if (!n || !from || date < from || date > to) return { date, tcp_total: 0, tcp_ok: 0, egress_total: 0, egress_ok: 0 };
    const r = rng(hashStr(`${m.id}:${date}`));
    const share = date === today ? elapsed : 1;
    const tcp = Math.round(2880 * n * share); const egress = Math.round(1440 * n * share);
    const down = m.health.status === 'down' && date >= addDays(today, -1);
    const lossy = r() < 0.12;
    const fail = (total: number) => Math.round(total * (down ? 0.2 + r() * 0.5 : lossy ? r() * 0.03 : 0));
    return { date, tcp_total: tcp, tcp_ok: tcp - fail(tcp), egress_total: egress, egress_ok: egress - fail(egress) };
  });
  const sum = (k: keyof Omit<AvailabilityDay, 'date'>) => items.reduce((s, d) => s + d[k], 0);
  const pct = (ok: number, total: number) => (total ? (ok / total) * 100 : null);
  return { days, items, tcp_pct: pct(sum('tcp_ok'), sum('tcp_total')), egress_pct: pct(sum('egress_ok'), sum('egress_total')) };
}
/** 全站网卡速率的每分钟历史（旧→新），围着当前总速率随机波动；同服务端只给结束满 20 s 的分钟（end 为其后第一个分钟起点） */
function overviewSeries(minutes: number) {
  const o = overview(); const r = rng(hashStr(`ov:${Math.floor(Date.now() / 60_000)}`));
  const end = Math.floor((Date.now() - 20_000) / 60_000) * 60_000;
  return { points: Array.from({ length: minutes }, (_, i) => ({ ts: iso(end - (minutes - i) * 60_000), net_rx_bps: Math.round(o.net_rx_bps * (0.7 + r() * 0.6)), net_tx_bps: Math.round(o.net_tx_bps * (0.7 + r() * 0.6)) })) };
}

// ---------- 作业模拟 ----------
const PSTEPS: [string, string][] = [['ssh', '连接并检查系统'], ['agent', '安装 agent'], ['ss', '配置 Shadowsocks'], ['entry', '接入线路机'], ['verify', '验证连通性']];
function newJob(kind: string, target: Job['target'], steps: [string, string][]): Job {
  const id = `j-${db.seq++}`;
  const t = Date.now();
  const j: Job = { id, kind, target, state: 'running', step: 0, step_count: steps.length, step_title: null, cancellable: true, error: null, created_by: db.user, created_at: iso(t), started_at: iso(t), finished_at: null,
    steps: steps.map(([key, title], idx) => ({ idx, key, title, state: idx === 0 ? 'running' : 'pending', started_at: idx === 0 ? iso(t) : null, finished_at: null, output: null, error: null })) };
  db.jobs.unshift(j); db.logs[id] = [];
  return j;
}
function log(jobId: string, level: string, msg: string) {
  const arr = db.logs[jobId] ?? (db.logs[jobId] = []);
  arr.push({ id: arr.length + 1, ts: iso(), level, msg } as JobLog);
}
/** 按固定间隔推进步骤；onDone 在最后一步完成后调用 */
function drive(job: Job, stepMs: number, onDone: () => void, outputs: Record<string, unknown>[] = []) {
  const tick = () => {
    if (job.state !== 'running') return;
    const cur = job.steps.find((s) => s.state === 'running');
    if (!cur) return;
    cur.state = 'done'; cur.finished_at = iso(); cur.output = outputs[cur.idx] ?? null;
    log(job.id, 'info', `步骤 ${cur.idx + 1}/${job.steps.length} 完成：${cur.title}`);
    const next = job.steps[cur.idx + 1];
    if (next) { next.state = 'running'; next.started_at = iso(); job.step = next.idx; log(job.id, 'info', `开始：${next.title}`); emit({ type: 'job', id: job.id }); setTimeout(tick, stepMs); }
    else { job.state = 'succeeded'; job.cancellable = false; job.finished_at = iso(); onDone(); emit({ type: 'job', id: job.id }); emit({ type: 'machines' }); }
  };
  log(job.id, 'info', `开始：${job.steps[0].title}`);
  setTimeout(tick, stepMs);
}

/** 同机 IP 节点的 agent 与主机读数就是宿主机那一份（同一个对象，读数随宿主机一起变） */
function borrowHost(m: MachineView, h: MachineView) { m.agent = h.agent; m.host = h.host; }
/** 同机 IP 节点上架 / 恢复的步骤输出：在宿主机上核对 IP、收敛（不做系统基线，bbr 为 null） */
const ipOutputs = (m: MachineView, h: MachineView) => [
  { ss_port: db.ssPorts[m.id], local_ips: `${h.ip}, ${m.ip}`, egress_ip: m.ip, bind_ip: m.bind_ip },
  { bbr: null, nft_relays: m.entries.map((e) => machine(e.relay.id).ip).join(', '), ssserver_updated: false },
];
function startProvisionJob(m: MachineView) {
  const h = m.host_machine && machine(m.host_machine.id);
  const job = newJob(h ? 'provision_ip' : m.kind === 'relay' ? 'provision_relay' : m.kind === 'proxy' ? 'provision_proxy' : 'provision_vps', { id: m.id, name: m.name }, h ? IP_STEPS : PSTEPS);
  m.active_job = { id: job.id, kind: job.kind, state: 'running' };
  drive(job, 2800, () => {
    m.lifecycle = 'active'; m.activated_at = iso(); m.active_job = null; m.health = { status: 'ok', reasons: [] };
    if (!h && m.kind !== 'proxy') { m.tune = { profile: 'tune-v1', at: iso(), current: true }; m.agent = { version: '1.0.3', last_report_at: iso(), online: true, xray_running: null, cores: 2, health: null }; m.host = { ts: iso(), cpu_pct: 6, load1: 0.1, mem_used: 400 * 1024 ** 2, mem_total: 4 * GiB, disk_used: 4 * GiB, disk_total: 60 * GiB, net_rx_bps: 2e5, net_tx_bps: 1e5, tcp_conns: 12 }; }
    for (const e of m.entries) { e.lifecycle = 'active'; e.applied = true; e.tcp = { ok: true, latency_ms: 52, detail: null, ts: iso() }; e.egress = { ok: true, latency_ms: 190, detail: null, ts: iso() }; }
    refreshCounts();
  }, h ? ipOutputs(m, h) : []);
}
// 固定作业：运行中的 j-run 慢速推进，便于看时间线
function bootFixtures() {
  const run = db.jobs.find((j) => j.id === 'j-run')!;
  const m = db.machines.find((x) => x.id === 'm-t1-13p')!;
  run.steps[2].state = 'running'; run.steps[3].state = 'pending';
  const retJob = db.jobs.find((j) => j.id === 'j-ret')!;
  const rm = db.machines.find((x) => x.id === 'm-t2-07')!;
  rm.active_job = { id: retJob.id, kind: 'retire', state: 'running' };
  m.active_job = { id: run.id, kind: 'provision_vps', state: 'running' };
  // j-run 每 ~25 秒前进一步（使用真实对象）
  const advance = (job: Job, ms: number, done: () => void) => {
    const t = setInterval(() => {
      const cur = job.steps.find((s) => s.state === 'running');
      if (!cur) { clearInterval(t); return; }
      cur.state = 'done'; cur.finished_at = iso(); log(job.id, 'info', `步骤 ${cur.idx + 1}/${job.steps.length} 完成：${cur.title}`);
      const next = job.steps[cur.idx + 1];
      if (next) { next.state = 'running'; next.started_at = iso(); job.step = next.idx; } else { job.state = 'succeeded'; job.cancellable = false; job.finished_at = iso(); clearInterval(t); done(); }
      emit({ type: 'job', id: job.id }); emit({ type: 'machines' });
    }, ms);
  };
  advance(run, 40_000, () => { m.lifecycle = 'active'; m.activated_at = iso(); m.active_job = null; m.health = { status: 'ok', reasons: [] }; refreshCounts(); });
  advance(retJob, 50_000, () => { finishRetire(rm); refreshCounts(); });
}

// ---------- 上架校验 ----------
const IP_RE = /^(\d{1,3})(\.\d{1,3}){3}$/;
/** 宿主机上最小的空闲 SS 端口：8389 起，跳过未退役同机节点（except 这台除外）已占的和 extra（同一批里已分出去的） */
function freeSsPort(hostId: string, extra: number[] = [], except?: string) {
  const taken = new Set(extra);
  for (const x of db.machines) if (x.host_machine?.id === hostId && x.lifecycle !== 'retired' && x.id !== except) taken.add(db.ssPorts[x.id]);
  let p = 8389; while (taken.has(p)) p++;
  return p;
}
function checkProvision(items: ProvisionItem[]): ProvisionCheckItem[] {
  const usedNum = new Map<string, Set<number>>();
  for (const m of db.machines) if (m.team && m.number != null && m.lifecycle !== 'retired') { const s = usedNum.get(m.team.id) ?? new Set(); s.add(m.number); usedNum.set(m.team.id, s); }
  const ips = new Map(db.machines.filter((m) => m.lifecycle !== 'retired').map((m) => [m.ip, m.name]));
  const portUsed = new Map<string, number>();
  const ssBatch = new Map<string, number[]>(); // 宿主机 → 这一批分到的 SS 端口
  const out: ProvisionCheckItem[] = [];
  const seenIp = new Set<string>();
  for (const it of items) {
    const errors: string[] = [];
    if (!IP_RE.test(it.ip ?? '') || it.ip.split('.').some((x) => +x > 255)) errors.push('IP 格式不正确');
    else if (ips.has(it.ip)) errors.push(`这个 IP 已经属于 ${ips.get(it.ip)}`); // 同服务端原话
    else if (seenIp.has(it.ip)) errors.push('这一批里 IP 重复');
    // 不是一台机器的地址（同服务端 provision::not_a_host 的原话，只模拟回环与链路本地）
    else if (it.kind !== 'proxy' && !it.host_id && /^(127\.|169\.254\.)/.test(it.ip)) errors.push(`IP ${it.ip} 是${it.ip.startsWith('127.') ? '回环' : '链路本地'}地址，不能上架`);
    seenIp.add(it.ip);
    if (it.ssh_port != null && (it.ssh_port < 1 || it.ssh_port > 65535)) errors.push('SSH 端口应在 1–65535');
    let name = ''; let number: number | null = null; let relayPort: number | undefined;
    if (it.kind === 'relay') {
      if (!it.label?.trim()) errors.push('线路机需要填写名称');
      else if (db.machines.some((m) => m.kind === 'relay' && m.label === it.label && m.lifecycle !== 'retired')) errors.push(`线路机名 ${it.label} 已被占用`);
      name = it.label ?? '';
    } else {
      const team = db.teams.find((t) => t.id === it.team_id);
      if (!team) errors.push('请选择团队');
      const relay = db.machines.find((m) => m.id === it.relay_id && m.kind === 'relay');
      if (!relay) errors.push('请选择要挂载的线路机');
      if (team) {
        const used = usedNum.get(team.id) ?? new Set<number>();
        if (it.number != null && it.number !== ('' as unknown as number)) {
          if (!Number.isInteger(it.number) || it.number < 1 || it.number > 999) errors.push('编号应为 1–999 的整数');
          else if (used.has(it.number)) errors.push(`${displayName(team.name, it.number)} 已被占用`);
          else number = it.number;
        } else { let n = 1; while (used.has(n)) n++; number = n; }
        if (number != null) { used.add(number); usedNum.set(team.id, used); name = displayName(team.name, number); }
      }
      if (relay) {
        const taken = new Set(relay.entries.map((e) => e.port)); let p = relay.relay?.port_min ?? 20001; const extra = portUsed.get(relay.id) ?? 0;
        let skip = extra; while (taken.has(p) || skip-- > 0) p++;
        while (taken.has(p)) p++;
        relayPort = p; portUsed.set(relay.id, extra + 1);
      }
      if (it.kind === 'vps') { if (it.ss_port != null && (it.ss_port < 1 || it.ss_port > 65535)) errors.push('SS 端口应在 1–65535'); }
      if (it.kind === 'proxy') {
        if (!it.upstream?.host) errors.push('家宽代理需要填写上游主机');
        if (!it.upstream?.port) errors.push('家宽代理需要填写上游端口');
      }
    }
    if (it.kind !== 'proxy' && it.login?.password === '' ) errors.push('登录密码为空：不填请整项留空以使用中控密钥');
    if (it.subscription?.due_date && !/^\d{4}-\d{2}-\d{2}$/.test(it.subscription.due_date)) errors.push('到期日格式应为 YYYY-MM-DD');
    // 同机 IP 节点：宿主机要是在用的普通 VPS 节点；SS 端口在宿主机上暂定（同服务端 provision::validate）
    let ssPort: number | null = null;
    if (it.host_id) {
      const h = db.machines.find((m) => m.id === it.host_id);
      if (!h) errors.push('宿主机不存在');
      else if (it.kind !== 'vps') errors.push('只有 VPS 节点（vps）能加在宿主机上');
      else if (h.kind !== 'vps' || h.host_machine) errors.push(`「${h.name}」不能当宿主机：宿主机要是普通 VPS 节点（不能是线路机、家宽代理或同机 IP 节点）`);
      else if (h.lifecycle !== 'active') errors.push(`宿主机「${h.name}」现在是「${LIFECYCLE_LABEL[h.lifecycle]}」，只能加在运行中的 VPS 节点上`);
      else {
        const batch = ssBatch.get(h.id) ?? [];
        ssPort = freeSsPort(h.id, batch);
        ssBatch.set(h.id, [...batch, ssPort]);
      }
    }
    // 同服务端 provision::check：库里的校验都过了再看 SSH 通不通；模拟里 192.0.2.x（文档用地址段）当作连不上
    if (!errors.length && it.kind !== 'proxy' && !it.host_id && it.ip.startsWith('192.0.2.')) errors.push(`SSH ${it.ip}:${it.ssh_port ?? 22} 连不上（3 秒没有响应）`);
    out.push({ ok: errors.length === 0, errors, name, number, relay_port: relayPort, ss_port: ssPort });
  }
  return out;
}

function createMachine(it: ProvisionItem, c: ProvisionCheckItem): MachineView {
  const id = `m-${db.seq++}`;
  // 同机 IP 节点：SSH 端口（只展示）与供应商取宿主机的，不带套餐，SS 端口用检查时暂定的；agent 与主机读数上架中就借用宿主机的（同服务端）
  const h = it.host_id ? machine(it.host_id) : null;
  const plan = h ? null : db.plans.find((p) => p.id === it.plan_id) ?? null;
  const pv = db.providers.find((p) => p.id === (h ? h.provider?.id : it.provider_id ?? plan?.provider_id)) ?? null;
  const team = it.kind === 'relay' ? null : teamOf(it.team_id!);
  if (h) db.ssPorts[id] = c.ss_port!;
  const m: MachineView = {
    id, kind: it.kind, name: c.name, team: team ? { id: team.id, name: team.name } : null, spare: team?.spare ?? false, number: it.kind === 'relay' ? null : c.number, label: it.kind === 'relay' ? it.label ?? null : null,
    ip: it.ip, ssh_port: h?.ssh_port ?? it.ssh_port ?? 22, country: it.country ?? countryOf(it.ip), country_set: it.country ?? null, host_machine: h && { id: h.id, name: h.name }, bind_ip: null, ip_nodes: [], provider: pv ? { id: pv.id, name: pv.name } : null, provider_code: pv?.code ?? null,
    plan: plan ? { id: plan.id, name: plan.name, bandwidth_mbps: plan.bandwidth_mbps, traffic_gb: plan.traffic_gb } : null, service_ref: it.service_ref ?? null,
    lifecycle: 'provisioning', lifecycle_reason: null, note: it.note ?? '', revision: 1, created_at: iso(), activated_at: null, retired_at: null, spare_from: null, spare_since: null, recycle_on: null,
    subscription: it.subscription?.due_date ? subCalc({ price_cents: it.subscription.price_cents ?? plan?.price_cents ?? 0, currency: plan?.currency ?? META.defaults.currency, cycle: it.subscription.cycle ?? plan?.cycle ?? META.defaults.cycle, due_date: it.subscription.due_date, due_days: 0, billing_tz: it.subscription.billing_tz ?? pv?.timezone ?? META.org_timezone, auto_renew: false, remind_days: pv?.remind_days ?? META.defaults.remind_days, traffic_gb: plan?.traffic_gb || null, traffic_reset_day: 1 } as NonNullable<MachineView['subscription']>) : null,
    agent: null, host: null, entries: [], removed_traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 }, nic: null, month_counted_from: null, quota: { used: 0, period_start: '', period_end: null, mode: 'monthly', counted_from: null, rule: null, base: null }, health: { status: 'unknown', reasons: [{ key: 'provisioning', severity: 'info', text: '上架作业进行中' }] }, active_job: null, failed_job: null, open_incidents: 0, maintenance: false,
    relay: it.kind === 'relay' ? { public_host: it.ip, generation: 1, applied_generation: 0, applied_at: null, apply_error: null, port_min: 20001, port_max: 20999 } : null,
    tune: it.kind === 'proxy' || h ? null : { profile: null, at: null, current: false },
  };
  if (h) borrowHost(m, h);
  if (m.kind !== 'relay') {
    const relay = machine(it.relay_id!);
    const e: EntryView = { id: `e${db.seq++}`, relay: { id: relay.id, name: relay.name }, node: { id, name: m.name }, direct: false, port: c.relay_port ?? 20999, lifecycle: 'pending', applied: false, apply_error: null, tcp: null, egress: null, reach: null, traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 } };
    m.entries = [e]; relay.entries.push(e);
  }
  recomputeDue(m); Object.assign(m, trafficOf(m, db.machines));
  db.machines.push(m); refreshCounts();
  return m;
}

// ---------- 路由 ----------
type Ctx = { params: string[]; query: URLSearchParams; body: any };
type Handler = (c: Ctx) => unknown;
const routes: [string, RegExp, Handler][] = [];
const R = (method: string, path: string, h: Handler) => routes.push([method, new RegExp(`^${path}$`), h]);

R('POST', '/auth/login', ({ body }) => { if (!body?.username) throw bad('请输入用户名'); db.user = body.username; sessionStorage.setItem('mock:auth', '1'); return { username: db.user }; });
R('POST', '/auth/logout', () => { sessionStorage.setItem('mock:auth', '0'); return {}; });
R('GET', '/auth/me', () => { if (sessionStorage.getItem('mock:auth') === '0') throw bad('未登录', 401, 'unauthorized'); return { username: db.user }; });

R('GET', '/teams', () => clone(db.teams));
R('POST', '/teams', ({ body }) => {
  if (!body.name?.trim()) throw bad('团队名不能为空');
  if (db.teams.some((t) => t.name === body.name)) throw bad('团队名已存在', 409, 'conflict');
  const t = { id: `t${db.seq++}`, name: body.name.trim(), sort_order: body.sort_order ?? db.teams.length + 1, spare: false, node_count: 0 };
  db.teams.push(t); emit({ type: 'catalog' }); return t;
});
R('PATCH', '/teams/([^/]+)', ({ params, body }) => {
  const t = teamOf(params[0]);
  if (body.name && body.name !== t.name) {
    if (db.teams.some((x) => x.name === body.name)) throw bad('团队名已存在', 409, 'conflict');
    t.name = body.name; db.machines.filter((m) => m.team?.id === t.id).forEach((m) => { m.team = { id: t.id, name: t.name }; renameMachine(m); });
    refreshCounts(); // 宿主机 ip_nodes 里的名字跟着改
    emit({ type: 'machines' });
  }
  if (body.sort_order != null) t.sort_order = body.sort_order;
  emit({ type: 'catalog' }); return t;
});
const renumberPlan = (tid: string) => {
  const list = db.machines.filter((m) => m.team?.id === tid && m.lifecycle !== 'retired' && m.number != null).sort((a, b) => a.number! - b.number!);
  const team = teamOf(tid);
  const changes = list.map((m, i) => ({ machine_id: m.id, old_name: m.name, new_name: displayName(team.name, i + 1), n: i + 1 })).filter((c, i) => list[i].number !== c.n);
  return { changes: changes.map(({ n: _n, ...c }) => c), token: `tok-${hashStr(JSON.stringify(changes))}`, raw: changes };
};
R('GET', '/teams/([^/]+)/renumber', ({ params }) => { const { changes, token } = renumberPlan(params[0]); return { changes, notes: [], token }; });
R('POST', '/teams/([^/]+)/renumber', ({ params, body }) => {
  const p = renumberPlan(params[0]);
  if (body.token !== p.token) throw bad('数据在预览后已变化，请重新预览', 409, 'conflict');
  for (const c of p.raw) { const m = machine(c.machine_id); m.number = c.n; m.revision++; renameMachine(m); }
  refreshCounts(); // 宿主机 ip_nodes 里的名字与顺序跟着改
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'team.renumber', title: '重新编号', summary: `${teamOf(params[0]).name}：${p.raw.length} 台改名`, target_kind: 'team', target_id: params[0], detail: { count: p.raw.length } });
  emit({ type: 'machines' }); emit({ type: 'catalog' }); return { changed: p.raw.length };
});
R('GET', '/providers', () => clone(db.providers));
R('POST', '/providers', ({ body }) => {
  if (!body.name || !body.code) throw bad('名称与代码必填');
  const p = { id: `p${db.seq++}`, name: body.name, code: String(body.code).toUpperCase(), panel_url: body.panel_url || null, timezone: body.timezone || null, traffic_rule: body.traffic_rule ?? META.defaults.traffic_rule };
  db.providers.push(p); emit({ type: 'catalog' }); return p;
});
R('PATCH', '/providers/([^/]+)', ({ params, body }) => {
  const p = db.providers.find((x) => x.id === params[0]);
  if (!p) throw bad('没有这个供应商', 404, 'not_found');
  if (body.name !== undefined) p.name = body.name;
  if (body.code !== undefined) p.code = body.code;
  if (body.panel_url !== undefined) p.panel_url = body.panel_url || null;
  if (body.timezone !== undefined) p.timezone = body.timezone || null; // 空 = 跟随组织时区
  if (body.traffic_rule !== undefined) p.traffic_rule = body.traffic_rule;
  emit({ type: 'catalog' }); return { id: p.id };
});
// 删目录项：同中控，还有机器、套餐用着时 409 写明还有什么
const inUse = (parts: [number, string][]) => parts.filter(([n]) => n > 0).map(([n, w]) => `${n} ${w}`).join('、');
const delAudit = (action: string, title: string, kind: string, id: string, name: string) => db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action, title, summary: name, target_kind: kind, target_id: id, detail: { name } });
R('DELETE', '/teams/([^/]+)', ({ params }) => {
  const t = teamOf(params[0]);
  if (t.spare) throw bad('备用团队由系统管理，不能删除', 409, 'conflict');
  const u = inUse([[db.machines.filter((m) => m.team?.id === t.id).length, '台机器']]);
  if (u) throw bad(`「${t.name}」还有 ${u}：先把它们移到别的团队再删`, 409, 'conflict');
  db.teams = db.teams.filter((x) => x.id !== t.id); delAudit('team.delete', '删除团队', 'team', t.id, t.name);
  emit({ type: 'catalog' }); return { id: t.id };
});
R('DELETE', '/providers/([^/]+)', ({ params }) => {
  const p = db.providers.find((x) => x.id === params[0]); if (!p) throw bad('没有这个供应商', 404, 'not_found');
  const u = inUse([[db.plans.filter((x) => x.provider_id === p.id).length, '个套餐（含已归档的）'], [db.machines.filter((m) => m.provider?.id === p.id).length, '台机器（含已退役的）']]);
  if (u) throw bad(`「${p.name}」还有 ${u}：先删掉或改到别的供应商再删`, 409, 'conflict');
  db.providers = db.providers.filter((x) => x.id !== p.id); delAudit('provider.delete', '删除供应商', 'provider', p.id, p.name);
  emit({ type: 'catalog' }); return { id: p.id };
});
R('DELETE', '/plans/([^/]+)', ({ params }) => {
  const p = db.plans.find((x) => x.id === params[0]); if (!p) throw bad('没有这个套餐', 404, 'not_found');
  const u = inUse([[db.machines.filter((m) => m.plan?.id === p.id).length, '台机器']]);
  if (u) throw bad(`「${p.name}」还有 ${u}用着：不再上架的可以归档`, 409, 'conflict');
  db.plans = db.plans.filter((x) => x.id !== p.id); delAudit('plan.delete', '删除套餐', 'plan', p.id, p.name);
  emit({ type: 'catalog' }); return { id: p.id };
});
R('GET', '/plans', () => clone(db.plans));
R('POST', '/plans', ({ body }) => {
  if (!body.name || !body.provider_id) throw bad('套餐名与供应商必填');
  const p = { id: `pl${db.seq++}`, archived_at: null, machine_count: 0, notes: null, cpu_cores: 0, mem_mb: 0, disk_gb: 0, bandwidth_mbps: 0, traffic_gb: 0, price_cents: 0, currency: 'CNY', cycle: 'monthly', ...body };
  db.plans.push(p); emit({ type: 'catalog' }); return p;
});
R('PATCH', '/plans/([^/]+)', ({ params, body }) => {
  const p = db.plans.find((x) => x.id === params[0]); if (!p) throw bad('套餐不存在', 404, 'not_found');
  const before = { ...p } as Record<string, unknown>;
  const { archived, ...rest } = body; Object.assign(p, rest);
  if (archived === true) p.archived_at = iso(); if (archived === false) p.archived_at = null;
  // 同中控：审计记套餐名与改了的字段（before / after）；只改了归档的标题写归档 / 取消归档（摘要的改动明细这里从简）
  const after = p as unknown as Record<string, unknown>;
  const keys = Object.keys(after).filter((k) => JSON.stringify(after[k]) !== JSON.stringify(before[k]));
  const title = keys.length === 1 && keys[0] === 'archived_at' ? (p.archived_at ? '归档套餐' : '取消归档套餐') : '修改套餐';
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'plan.update', title, summary: p.name, target_kind: 'plan', target_id: p.id, detail: { name: p.name, currency: p.currency, before: Object.fromEntries(keys.map((k) => [k, before[k]])), after: Object.fromEntries(keys.map((k) => [k, after[k]])) } });
  emit({ type: 'catalog' }); return p;
});

R('GET', '/recycle-bin', ({ query }) => {
  const q = (query.get('q') ?? '').trim().toLowerCase(); const src = query.get('source') ?? '';
  const off = Number(query.get('offset') ?? 0), lim = Number(query.get('limit') ?? 50);
  const hit = db.recycle.filter((r) => (!src || r.source === src) && (!q || [r.name, r.last_name, r.ip, r.team, r.provider, r.plan, r.service_ref].some((v) => v?.toLowerCase().includes(q))));
  const items = hit.slice(off, off + lim).map(({ snapshot: _s, ...r }) => r);
  return { items, total: hit.length, next_offset: off + lim < hit.length ? off + lim : null };
});
R('GET', '/recycle-bin/([^/]+)', ({ params }) => { const r = db.recycle.find((x) => String(x.id) === params[0]); if (!r) throw bad('记录不存在', 404, 'not_found'); return clone(r); });
R('GET', '/machines', () => {
  return clone(db.machines); // 回收了的不在列表里（scope=live / all 同）
});
R('GET', '/machines/([^/]+)', ({ params }) => clone(machine(params[0])));
R('PATCH', '/machines/([^/]+)', ({ params, body }) => {
  const m = machine(params[0]);
  if (body.revision !== m.revision) throw bad('这台机器刚被其他人修改过，请刷新后重试', 409, 'revision_conflict');
  if (body.team_id !== undefined || body.number !== undefined) {
    const team = teamOf(body.team_id ?? m.team?.id);
    const number = body.number ?? m.number;
    if (db.machines.some((x) => x.id !== m.id && x.team?.id === team.id && x.number === number && x.lifecycle !== 'retired')) throw bad(`${displayName(team.name, number)} 已被占用`, 409, 'conflict');
    const wasSpare = m.spare, oldName = m.name;
    m.team = { id: team.id, name: team.name }; m.spare = team.spare; m.number = number;
    if (m.spare && !wasSpare) { m.spare_from = oldName; m.spare_since = iso(); } else if (!m.spare) { m.spare_from = null; m.spare_since = null; } // 同服务端 patch_tx
  }
  if (body.label !== undefined) m.label = body.label;
  if (body.country !== undefined) {
    if (body.country !== null && !/^[A-Z]{2}$/.test(body.country)) throw bad('国家代码应为 ISO 3166-1 两位代码');
    m.country_set = body.country; m.country = body.country ?? countryOf(m.ip);
  }
  for (const k of ['note', 'service_ref'] as const) if (body[k] !== undefined) (m as any)[k] = body[k];
  if (body.provider_id !== undefined) { const p = db.providers.find((x) => x.id === body.provider_id); m.provider = p ? { id: p.id, name: p.name } : null; m.provider_code = p?.code ?? null; }
  if (body.plan_id !== undefined) { const p = db.plans.find((x) => x.id === body.plan_id); m.plan = p ? { id: p.id, name: p.name, bandwidth_mbps: p.bandwidth_mbps, traffic_gb: p.traffic_gb } : null; }
  m.revision++; renameMachine(m); refreshCounts();
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'machine.update', title: '修改机器', summary: `${m.name}：${Object.keys(body).filter((k) => k !== 'revision').join('、')}`, target_kind: 'machine', target_id: m.id, detail: body });
  emit({ type: 'machines' }); emit({ type: 'catalog' }); return clone(m);
});
// 同机 IP 节点的主机曲线是宿主机的
R('GET', '/machines/([^/]+)/series', ({ params, query }) => {
  const m = machine(params[0]); const metric = query.get('metric') ?? 'host';
  return series(metric === 'host' && m.host_machine ? machine(m.host_machine.id) : m, metric, query.get('range') ?? '1h');
});
R('GET', '/machines/([^/]+)/availability', ({ params, query }) => availability(machine(params[0]), Number(query.get('days') ?? 30)));
/** 宿主机上还没开始退役的同机 IP 节点：退役宿主机时一并退役 */
const ipNodesLeaving = (m: MachineView) => db.machines.filter((x) => x.host_machine?.id === m.id && x.lifecycle !== 'retiring' && x.lifecycle !== 'retired');
/** 软路由里还指向这台的出口（mock 只有每个团队一台软路由，出口名按编号） */
const exitsUsing = (m: MachineView) => (m.team && !m.spare && m.entries.some((e) => e.lifecycle === 'active') ? [{ id: `rt-${m.team.id}`, name: `${m.team.name}-路由A`, exits: [{ exit: `节点${String(m.number ?? 1).padStart(2, '0')}`, node: m.name }] }] : []);
/** 不能移入备用的原因（同服务端 spare_eligible），能则 null */
function spareWhy(m: MachineView): string | null {
  if (m.kind === 'relay') return '线路机不属于任何团队';
  if (m.spare) return '已经在备用里';
  if (m.lifecycle !== 'active') return '只有在用的机器能进备用';
  if (!m.subscription) return '没有订阅，没有到期日可等';
  if (m.subscription.due_days <= -2) return '订阅已过期满 1 天，马上就会被清理';
  return null;
}
const spareTeam = () => db.teams.find((t) => t.spare)!;
const freeSpareNumber = (skip: string[] = []) => { const taken = new Set(db.machines.filter((x) => x.team?.id === spareTeam().id && !skip.includes(x.id)).map((x) => x.number)); let n = 1; while (taken.has(n)) n++; return n; };
/** 备用团队接下来 n 个最小的空号（宿主机先、同机 IP 节点依次） */
const freeSpareNumbers = (n: number) => { const taken = new Set(db.machines.filter((x) => x.team?.id === spareTeam().id).map((x) => x.number)); const out: number[] = []; for (let k = 1; out.length < n; k++) if (!taken.has(k)) out.push(k); return out; };
R('GET', '/machines/([^/]+)/retire', ({ params }) => {
  const m = machine(params[0]);
  const xs = ipNodesLeaving(m);
  // 改名按所有受影响的团队算（同服务端）
  const leaving = m.lifecycle !== 'retired' ? [m, ...xs] : [];
  const teams = [...new Set(leaving.flatMap((x) => (x.team ? [x.team.id] : [])))];
  const renames = teams.flatMap((t) => compactPlan(t, leaving.map((x) => x.id))).map((c) => ({ machine_id: c.m.id, old_name: c.old_name, new_name: c.new_name }));
  const why = spareWhy(m);
  const sp = spareTeam(); const nums = freeSpareNumbers(1 + xs.length);
  const ips = xs.map((x) => ({ id: x.id, name: x.name }));
  const spareIps = xs.map((x, i) => ({ id: x.id, name: x.name, to_name: displayName(sp.name, nums[i + 1]) }));
  return {
    name: m.name, renames, ip_nodes: ips, routers: exitsUsing(m),
    blockers: m.kind === 'relay' && m.entries.some((e) => !e.direct) ? [`这台线路机上还有 ${m.entries.filter((e) => !e.direct).length} 个节点入口：先把节点迁走或退役。可先用「搬空线路机」把节点换到其他线路机`] : [],
    spare: { ok: !why, why, team: { id: sp.id, name: sp.name }, to_name: displayName(sp.name, nums[0]), recycle_on: m.subscription ? addDays(m.subscription.due_date, 2) : null, ip_nodes: spareIps, notes: m.subscription?.auto_renew ? ['这台开着自动续费：进备用时会关掉，到期后照常回收（供应商那边的自动扣款要自己去关）'] : [] },
  };
});
/** 移入备用（同服务端 move_to_spare）：机器照常运行，改到备用团队最小的空号，原团队前移补号 */
function moveToSpare(m: MachineView, reason: string | null) {
  const sp = spareTeam(); const from = m.team!;
  m.spare_from = m.name; m.spare_since = iso(); m.team = { id: sp.id, name: sp.name }; m.spare = true; m.number = freeSpareNumber([m.id]); m.lifecycle_reason = reason; m.revision++;
  renameMachine(m); compactApply(from.id, `${m.spare_from} 退役后`);
}
function startRetire(m: MachineView, cleanup: boolean, reason: string | null) {
  const steps: [string, string][] = [['disable', '停用入口'], ...(cleanup ? [['cleanup', '清理节点上的服务'] as [string, string]] : []), ['revoke', '撤销令牌'], ['mark', '标记为已退役']];
  const job = newJob('retire', { id: m.id, name: m.name }, steps);
  m.lifecycle = 'retiring'; m.lifecycle_reason = reason; m.active_job = { id: job.id, kind: 'retire', state: 'running' }; m.failed_job = null; m.entries.forEach((e) => { e.lifecycle = 'disabled'; });
  drive(job, 2200, () => { finishRetire(m); refreshCounts(); });
  return job;
}
R('POST', '/machines/([^/]+)/retire', ({ params, body }) => {
  const m = machine(params[0]);
  if (m.lifecycle === 'retired') throw bad('这台机器已退役');
  if (body.confirm !== m.name) throw bad('确认名称不匹配', 400, 'confirm_mismatch');
  const xs = ipNodesLeaving(m);
  if (!body.now) {
    const why = spareWhy(m);
    if (why) throw bad(`不能移入备用：${why}`, 409, 'conflict');
    const from = m.name; const xfrom = xs.map((x) => x.name);
    const autoOff = !!m.subscription?.auto_renew;
    if (m.subscription) m.subscription.auto_renew = false;
    moveToSpare(m, body.reason ?? null);
    for (const x of xs) moveToSpare(x, `随宿主机「${from}」退役`);
    refreshCounts();
    db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'machine.retire', title: '退役机器', summary: `${from} 移入备用 → ${m.name}`, target_kind: 'machine', target_id: m.id, detail: { mode: 'spare' } });
    emit({ type: 'machines' }); emit({ type: 'catalog' });
    return { mode: 'spare', name: m.name, from, recycle_on: m.recycle_on, ip_nodes: xs.map((x, i) => ({ id: x.id, name: x.name, from: xfrom[i] })), auto_renew_off: autoOff, notes: autoOff ? ['自动续费已关闭（供应商那边的自动扣款要自己去关）'] : [] };
  }
  const job = startRetire(m, !!body.cleanup, body.reason ?? null);
  // 宿主机：同机 IP 节点一并退役，进行中 / 失败的作业作废，各自一个退役作业（不登录清理：宿主机的清理连同它们的实例一起删）
  const ip_node_jobs = xs.map((x) => {
    for (const j of db.jobs) if (j.target?.id === x.id && ['queued', 'running', 'failed'].includes(j.state)) { j.state = 'cancelled'; j.cancellable = false; j.finished_at = iso(); }
    return { machine_id: x.id, name: x.name, job_id: startRetire(x, false, `随宿主机「${m.name}」退役`).id };
  });
  emit({ type: 'machines' }); return { mode: 'now', job_id: job.id, ip_node_jobs };
});
R('PUT', '/machines/([^/]+)/subscription', ({ params, body }) => {
  const m = machine(params[0]);
  if (body.traffic_used_gb !== undefined) throw bad('流量已用改为由 agent 按网卡统计，不再按面板校准', 400, 'bad_request');
  m.subscription = { traffic_gb: null, traffic_reset_day: null, ...(m.subscription ?? {} as any), ...body, due_days: 0 };
  recomputeDue(m); Object.assign(m, trafficOf(m, db.machines)); emit({ type: 'machine', id: m.id }); emit({ type: 'machines' }); return clone(m.subscription);
});
R('GET', '/machines/([^/]+)/renewals', ({ params }) => clone(db.renewals[params[0]] ?? []));
R('POST', '/machines/([^/]+)/renewals', ({ params, body }) => {
  const m = machine(params[0]); if (!m.subscription) throw bad('这台机器还没有订阅信息，请先保存订阅');
  // new_due 与 cycles（续几个周期，从当前到期日一次加够）二选一，同服务端
  if ((body.new_due == null) === (body.cycles == null)) throw bad('new_due 与 cycles 要给且只给一个');
  const newDue: string = body.new_due ?? addMonths(m.subscription.due_date, cycleMonths(m.subscription.cycle) * Number(body.cycles));
  if (!/^\d{4}-\d{2}-\d{2}$/.test(newDue)) throw bad('新到期日格式应为 YYYY-MM-DD');
  const r = { id: `rn${db.seq++}`, old_due: m.subscription.due_date, new_due: newDue, amount_cents: body.amount_cents ?? m.subscription.price_cents, currency: m.subscription.currency, note: body.note ?? null, actor: db.user, created_at: iso() };
  (db.renewals[m.id] ??= []).unshift(r); m.subscription.due_date = newDue; recomputeDue(m);
  emit({ type: 'machine', id: m.id }); emit({ type: 'machines' }); return { id: r.id, old_due: r.old_due, new_due: r.new_due };
});
R('POST', '/provision/check', ({ body }) => ({ items: checkProvision(body.items ?? []) }));
R('POST', '/provision', ({ body }) => {
  const items: ProvisionItem[] = body.items ?? [];
  const res = checkProvision(items);
  if (!items.length) throw bad('没有要上架的机器');
  if (res.some((x) => !x.ok)) throw bad('有行未通过检查，请先修正后重新检查', 422, 'validation');
  const jobs = items.map((it, i) => { const m = createMachine(it, res[i]); startProvisionJob(m); return { machine_id: m.id, job_id: m.active_job!.id, name: m.name }; });
  emit({ type: 'machines' }); emit({ type: 'catalog' }); return { jobs };
});

R('GET', '/relays', () => clone(db.machines.filter((m) => m.kind === 'relay' && m.lifecycle !== 'retired')));
const findEntry = (id: string) => { for (const m of db.machines) for (const e of m.entries) if (e.id === id) return e; throw bad('入口不存在', 404, 'not_found'); };
/** 同一个入口在线路机与节点两处各有一份引用（同一对象），改一次即可 */
R('POST', '/entries/([^/]+)/actions', ({ params, body }) => {
  const e = findEntry(params[0]);
  if (body.action === 'rotate') { if (body.confirm !== `${e.relay.name}:${e.port}`) throw bad('确认内容不匹配', 400, 'confirm_mismatch'); e.applied = false; setTimeout(() => { e.applied = true; emit({ type: 'machines' }); }, 2500); }
  else if (body.action === 'disable') { e.lifecycle = 'disabled'; e.applied = false; setTimeout(() => { e.applied = true; emit({ type: 'machines' }); }, 1800); }
  else if (body.action === 'enable') { e.lifecycle = 'active'; e.applied = false; setTimeout(() => { e.applied = true; emit({ type: 'machines' }); }, 1800); }
  else throw bad('未知操作');
  const relay = machine(e.relay.id); if (relay.relay) relay.relay.generation++;
  emit({ type: 'machines' }); emit({ type: 'relay', id: e.relay.id }); return {};
});
R('GET', '/entries/([^/]+)/share', ({ params }) => {
  const e = findEntry(params[0]); const relay = machine(e.relay.id);
  return { url: `vless://00000000-0000-4000-8000-${String(hashStr(e.id)).padStart(12, '0')}@${relay.relay?.public_host ?? relay.ip}:${e.port}?encryption=none&type=tcp#${encodeURIComponent(e.node.name)}` };
});
R('POST', '/machines/([^/]+)/entries', ({ params, body }) => {
  const m = machine(params[0]); const relay = machine(body.relay_id);
  // 线路机给自己加 = 直出入口（同服务端：每台至多一条，只出现在线路机的入口里）
  const direct = m.kind === 'relay';
  if (direct && relay.id !== m.id) throw bad('线路机上只能加它自己的直出入口（relay_id 填这台线路机自己）', 409, 'conflict');
  if (direct && m.entries.some((x) => x.direct)) throw bad('这台线路机已经有直出入口', 409, 'conflict');
  const port = Math.max(20000, ...relay.entries.map((x) => x.port)) + 1;
  const e: EntryView = { id: `e${db.seq++}`, relay: { id: relay.id, name: relay.name }, node: { id: m.id, name: direct ? `${m.name}-直出` : m.name }, direct, port, lifecycle: 'pending', applied: false, apply_error: null, tcp: null, egress: null, reach: null, traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 } };
  if (!direct) m.entries.push(e);
  relay.entries.push(e);
  const job = newJob('attach_entry', { id: m.id, name: m.name }, [['apply', '等待线路机应用'], ['verify', '出口验收']]);
  drive(job, 2000, () => { e.lifecycle = 'active'; e.applied = true; });
  emit({ type: 'machines' }); return { job_id: job.id };
});

/** 分页接口的形状（同服务端 api::paged） */
const paged = <T,>(all: T[], query: URLSearchParams) => {
  const limit = +(query.get('limit') ?? 100); const offset = +(query.get('offset') ?? 0);
  const items = clone(all.slice(offset, offset + limit));
  return { items, total: all.length, next_offset: offset + items.length < all.length ? offset + items.length : null };
};
/** 列表项同服务端：不带 steps / cancellable / result，带当前步骤下标与标题 */
const jobSummary = ({ steps, cancellable: _c, result: _r, ...j }: Job): JobSummary => {
  const i = steps.findIndex((x) => x.state !== 'done' && x.state !== 'skipped');
  const step = i < 0 ? steps.length : i;
  return { ...j, step, step_title: steps[step]?.title ?? null };
};
R('GET', '/jobs', ({ query }) => {
  const s = query.get('state') ?? 'all';
  // 同中控：provision 是大类，其余按作业类型原值
  const k = query.get('kind') ?? '';
  const kindOk = (kind: string) => !k || (k === 'provision' ? kind.startsWith(`${k}_`) : kind === k);
  return paged(db.jobs.filter((j) => kindOk(j.kind) && (s === 'all' || (s === 'active' ? j.state === 'running' || j.state === 'queued' : j.state !== 'running' && j.state !== 'queued'))).map(jobSummary), query);
});
const jobOf = (id: string) => { const j = db.jobs.find((x) => x.id === id); if (!j) throw bad('作业不存在', 404, 'not_found'); return j; };
R('GET', '/jobs/([^/]+)', ({ params }) => { const j = clone(jobOf(params[0])); const s = jobSummary(j); return { ...j, step: s.step, step_title: s.step_title }; });
R('GET', '/jobs/([^/]+)/logs', ({ params, query }) => {
  const after = +(query.get('after') ?? 0);
  const rest = (db.logs[params[0]] ?? []).filter((l) => l.id > after);
  const tail = query.get('tail');
  if (tail == null) return clone(rest);
  const logs = rest.slice(-Math.min(Math.max(+tail, 1), 5000));
  return clone({ logs, skipped: rest.length - logs.length, after: logs.length ? logs[logs.length - 1].id : after });
});
/** 重试：body { reset_host_key?, ssh_password? }（C1）。指纹不符不重置、认证失败不给密码时 mock 照样失败 */
R('POST', '/jobs/([^/]+)/retry', ({ params, body }) => {
  const j = jobOf(params[0]); if (j.state !== 'failed') throw bad('只有失败的作业才能重试', 409, 'conflict');
  const f = j.steps.find((s) => s.state === 'failed')!;
  const ipNode = j.kind === 'provision_ip' || !!db.machines.find((x) => x.id === j.target?.id)?.host_machine;
  if (body.reset_host_key && (f.error_kind !== 'host_key' || ipNode)) throw bad(ipNode ? '同机 IP 节点不能在这里重置主机指纹：先在宿主机的作业里重置' : '只有主机指纹不符的失败才能重置指纹', 409, 'conflict');
  if (f.error_kind === 'host_key' && !body.reset_host_key) throw bad('主机指纹仍与记录的不一致：确认机器重装过后勾选「重置主机指纹」', 409, 'conflict');
  if (f.error_kind === 'auth' && !body.ssh_password) throw bad('中控密钥仍登录不上：请填写 SSH 密码', 409, 'conflict');
  f.state = 'running'; f.error = null; f.error_kind = null; j.state = 'running'; j.cancellable = true; j.error = null; j.finished_at = null; log(j.id, 'info', `从失败的步骤重试${body.reset_host_key ? '（已重置主机指纹）' : ''}`);
  const m = db.machines.find((x) => x.id === j.target?.id);
  if (m) { if (m.lifecycle === 'failed') m.lifecycle = 'provisioning'; m.failed_job = null; m.active_job = { id: j.id, kind: j.kind, state: 'running' }; }
  drive(j, 2500, () => { if (!m) return; if (j.kind === 'retire') { finishRetire(m); refreshCounts(); return; } m.lifecycle = 'active'; m.activated_at ??= iso(); m.active_job = null; m.health = { status: 'ok', reasons: [] }; });
  emit({ type: 'job', id: j.id }); emit({ type: 'machines' }); return {};
});
/** 取消：进行中的作业，或失败的上架 / 恢复作业（上架 → 退役并清理；恢复 → 退回已退役）；升级 agent 只有排队中的（编排里等它的跟着取消） */
R('POST', '/jobs/([^/]+)/cancel', ({ params }) => {
  const j = jobOf(params[0]); if (!j.cancellable) throw bad('这个作业不能取消', 409, 'conflict');
  const failed = j.state === 'failed';
  const m = db.machines.find((x) => x.id === j.target?.id);
  if (j.kind === 'upgrade_agent') {
    // 同中控：排队时取消的没开始过，步骤原样留着
    j.state = 'cancelled'; j.cancellable = false; j.finished_at = iso(); log(j.id, 'warn', '已取消'); if (m) m.active_job = null;
    upgradeNext(j); emit({ type: 'job', id: j.id }); emit({ type: 'machines' }); return {};
  }
  j.state = 'cancelled'; j.cancellable = false; j.finished_at = iso(); j.steps.forEach((s) => { if (s.state === 'running' || s.state === 'pending' || s.state === 'failed') s.state = 'skipped'; else if (s.state === 'done') s.state = 'compensated'; }); log(j.id, 'warn', '作业已取消，已完成的步骤已补偿');
  if (failed && m && m.lifecycle === 'failed') { m.failed_job = null; finishRetire(m); refreshCounts(); }
  emit({ type: 'job', id: j.id }); return {};
});

R('GET', '/meta', () => clone(META));
R('GET', '/overview', () => overview());
R('GET', '/overview/series', ({ query }) => overviewSeries(Math.min(60, +(query.get('minutes') ?? 30))));
R('GET', '/probe-beats', () => probeBeats());
/** 告警的顺序同服务端：进行中在前 → 严重度高在前 → 开始时间新在前；subject_id（只要这台机器的）时不看严重度，只按时间 */
const SEV_RANK: Record<string, number> = { critical: 0, warning: 1, info: 2 };
const byState = (a: Incident, b: Incident) => (a.state === b.state ? 0 : a.state === 'firing' ? -1 : 1);
const bySev = (a: Incident, b: Incident) => (SEV_RANK[a.severity] ?? 1) - (SEV_RANK[b.severity] ?? 1);
const byOpened = (a: Incident, b: Incident) => b.opened_at.localeCompare(a.opened_at);
R('GET', '/incidents', ({ query }) => {
  const s = query.get('state') ?? 'firing'; const sid = query.get('subject_id');
  const order = sid ? (a: Incident, b: Incident) => byState(a, b) || byOpened(a, b) : (a: Incident, b: Incident) => byState(a, b) || bySev(a, b) || byOpened(a, b);
  const list = db.incidents.filter((i) => (s === 'all' || i.state === s) && (!sid || i.subject_id === sid)).sort(order);
  return { ...paged(list, query), acked: list.filter((i) => i.ack_at).length };
});
R('POST', '/incidents/([^/]+)/ack', ({ params }) => { const i = db.incidents.find((x) => x.id === params[0]); if (!i) throw bad('告警不存在', 404, 'not_found'); i.ack_at = iso(); i.ack_by = db.user; emit({ type: 'incidents' }); return {}; });
/** 静默同服务端：只列没结束的（含还没开始的预约），按开始时间排；新建的校验顺序与中文原因同服务端 */
const RULE_NAMES = META.rules;
/** 维护中同服务端：正在生效、标记为维护的静默落在这台、它的宿主机或「全部机器」上；维护中的健康不给原因，结束后回到维护前的健康 */
const preMaint = new Map<string, MachineView['health']>();
function syncMaint() {
  const now = Date.now();
  const live = db.silences.filter((s) => s.is_maintenance && Date.parse(s.starts_at) <= now && Date.parse(s.ends_at) > now);
  const ids = new Set(live.map((s) => s.subject_id));
  for (const m of db.machines) {
    if (m.lifecycle !== 'active') continue;
    const on = ids.has(null) || ids.has(m.id) || (!!m.host_machine && ids.has(m.host_machine.id));
    if (on === m.maintenance) continue;
    if (on) { preMaint.set(m.id, m.health); m.health = { status: 'maintenance', reasons: [] }; }
    else { m.health = preMaint.get(m.id) ?? { status: 'ok', reasons: [] }; preMaint.delete(m.id); }
    m.maintenance = on;
  }
  emit({ type: 'machines' });
}
R('GET', '/silences', () => clone(db.silences.filter((s) => Date.parse(s.ends_at) > Date.now()).sort((a, b) => Date.parse(a.starts_at) - Date.parse(b.starts_at))));
R('POST', '/silences', ({ body }) => {
  const s = Date.parse(body.starts_at); const e = Date.parse(body.ends_at); const now = Date.now();
  if (!(e > s)) throw bad('结束时间要晚于开始时间');
  if (e - s > 30 * DAY) throw bad('最长 30 天；更久的请直接把机器退役');
  if (e <= now) throw bad('结束时间已经过去了');
  if (s > now + 90 * DAY) throw bad('最多提前 90 天预约');
  if (body.rule && !RULE_NAMES[body.rule]) throw bad(`没有「${body.rule}」这条告警规则`);
  if (body.rule && body.is_maintenance) throw bad('维护窗口针对整台机器，不能只选一条规则；只想静默这条告警就不要标记为维护');
  const m = body.subject_id ? db.machines.find((x) => x.id === body.subject_id) : null;
  if (body.subject_id && !m) throw bad('subject_id 指向的机器不存在');
  const id = `s${db.seq++}`;
  db.silences.push({ id, subject_id: m?.id ?? null, subject_name: m?.name ?? null, rule: body.rule ?? null, rule_name: body.rule ? RULE_NAMES[body.rule] : null, starts_at: body.starts_at, ends_at: body.ends_at, is_maintenance: !!body.is_maintenance, comment: body.comment ?? '' });
  syncMaint(); emit({ type: 'incidents' }); return { id };
});
R('DELETE', '/silences/([^/]+)', ({ params }) => {
  if (!db.silences.some((s) => s.id === params[0])) throw bad('没有这条静默', 404, 'not_found');
  db.silences = db.silences.filter((s) => s.id !== params[0]); syncMaint(); emit({ type: 'incidents' }); return { ok: true };
});
/** 同中控 api/misc.rs audit_cat：操作记录的分类 */
const AUDIT_EXEC = new Set(['exec', 'ssh.login', 'terminal.open', 'terminal.close', 'machine.logs', 'server.logs']);
const auditCat = (action: string) => (action.startsWith('auth.') ? 'auth' : AUDIT_EXEC.has(action) ? 'exec' : /^(machine|entry|relay|subscription|agent|job|renewal)(\.|$)/.test(action) ? 'machine' : 'config');
R('GET', '/audit', ({ query }) => {
  const cat = query.get('cat') ?? ''; const q = (query.get('q') ?? '').trim().toLowerCase();
  const list = db.audit.filter((a) => (!query.get('target_id') || a.target_id === query.get('target_id')) && (!cat || cat === 'all' || auditCat(a.action) === cat)
    && (!q || `${a.title} ${a.summary} ${a.actor}`.toLowerCase().includes(q)));
  const before = query.get('before_id'); const from = before ? list.findIndex((a) => a.id === before) + 1 : 0;
  return clone(list.slice(from, from + +(query.get('limit') ?? 50)));
});

installAccessMock(R, () => db.machines);
installPolicyMock(R, { teams: () => db.teams, machines: () => db.machines, emit, user: () => db.user, bad });
const routersMock = installRoutersMock(R, { teams: () => db.teams, machines: () => db.machines, incidents: () => db.incidents, emit, bad });
installTerminalMock(R, { machines: () => db.machines, routers: routersMock.routers, bad });
// ---------- 升级 agent ----------
const UPSTEPS: [string, string][] = [['push', '推送 agent'], ['restart', '重启 agent'], ['verify', '确认上报']];
/** 「全部升级」的编排（同中控作业参数 after）：排队中的作业 → 它要等的作业；halted：因为要等的没成功而取消的 */
const upgradeAfter = new Map<string, string>();
const halted = new Set<string>();
/** 开始升级：同中控，已经开始的不能取消（推送、重启到一半中断更危险） */
function startUpgrade(m: MachineView, job = newJob('upgrade_agent', { id: m.id, name: m.name }, UPSTEPS)) {
  job.cancellable = false;
  m.active_job = { id: job.id, kind: 'upgrade_agent', state: 'running' };
  drive(job, 1800, () => { m.active_job = null; if (m.agent) m.agent.version = '1.0.4'; upgradeNext(job); });
  return job;
}
/** 排队等 `after`：排队中的可以取消 */
function queueUpgrade(m: MachineView, after: string) {
  const job = newJob('upgrade_agent', { id: m.id, name: m.name }, UPSTEPS);
  job.state = 'queued'; job.started_at = null; job.steps[0].state = 'pending'; job.steps[0].started_at = null;
  upgradeAfter.set(job.id, after);
  m.active_job = { id: job.id, kind: 'upgrade_agent', state: 'queued' };
  return job;
}
/** `done` 结束了：等它的作业，它成功就开始，否则一步不做直接取消（原因同中控 jobs::halted；mock 的升级不会失败） */
function upgradeNext(done: Job) {
  for (const [id, after] of [...upgradeAfter]) {
    if (after !== done.id) continue;
    upgradeAfter.delete(id);
    const j = jobOf(id); const m = machine(j.target!.id);
    if (done.state === 'succeeded') {
      j.state = 'running'; j.started_at = iso(); j.steps[0].state = 'running'; j.steps[0].started_at = iso();
      startUpgrade(m, j);
    } else {
      j.state = 'cancelled'; j.cancellable = false; j.finished_at = iso(); m.active_job = null; halted.add(id);
      j.error = halted.has(done.id) ? done.error : `「${done.target?.name}」的升级没有完成（已取消），后面的机器不再升级`;
      log(id, 'warn', `没有开始：${j.error}`);
      upgradeNext(j);
    }
    emit({ type: 'job', id }); emit({ type: 'machines' });
  }
}
R('POST', '/machines/([^/]+)/upgrade-agent', ({ params }) => {
  const m = machine(params[0]);
  if (m.host_machine) throw bad(`同机 IP 节点没有自己的 agent，用的是宿主机「${m.host_machine.name}」的：升级宿主机的 agent`, 409, 'conflict');
  if (!m.agent) throw bad('这台机器没有 agent（家宽代理不装 agent）');
  if (m.active_job) throw bad('这台机器有进行中的作业，请等它结束', 409, 'conflict');
  const job = startUpgrade(m); emit({ type: 'machines' });
  return { machine_id: m.id, job_id: job.id };
});
// ---------- 系统调优 ----------
R('POST', '/machines/([^/]+)/tune', ({ params }) => {
  const m = machine(params[0]);
  if (m.host_machine) throw bad(`同机 IP 节点和宿主机共用系统设置：对宿主机「${m.host_machine.name}」调优`, 409, 'conflict');
  if (m.kind === 'proxy') throw bad('家宽代理没有我们的机器', 409, 'conflict');
  if (m.lifecycle !== 'active') throw bad('只有在用的机器能调优', 409, 'conflict');
  if (m.active_job) throw bad('这台机器有进行中的作业，请等它结束', 409, 'conflict');
  const job = newJob('tune', { id: m.id, name: m.name }, [['tune', '系统调优']]);
  job.cancellable = false;
  m.active_job = { id: job.id, kind: 'tune', state: 'running' };
  drive(job, 2500, () => { m.active_job = null; m.tune = { profile: 'tune-v1', at: iso(), current: true }; }, [{
    profile: 'tune-v1', applied: ['net.core.rmem_max', 'net.core.wmem_max', 'net.ipv4.tcp_rmem', 'net.ipv4.tcp_wmem'], ok: ['net.ipv4.tcp_congestion_control'], kept: ['net.core.somaxconn'],
    skipped: [{ key: 'net.netfilter.nf_conntrack_max', why: 'missing', got: null }], journald: 'changed', ntp: 'synced', nofile: '65536', container: false, mem_mb: 3911, swap_mb: 0, bbr: true,
  }]);
  emit({ type: 'machines' });
  return { machine_id: m.id, name: m.name, job_id: job.id };
});
R('POST', '/agents/upgrade', () => {
  // 同中控：线路机在前，第一台做金丝雀；线路机一台等前一台，VPS 节点与家宽代理都等最后一台线路机（没有线路机时等金丝雀），之后并发
  const ms = db.machines.filter((m) => m.lifecycle === 'active' && m.agent && !m.host_machine && !m.active_job)
    .sort((a, b) => Number(b.kind === 'relay') - Number(a.kind === 'relay'));
  let canary: string | undefined; let lastRelay: string | undefined;
  const jobs = ms.map((m) => {
    const after = m.kind === 'relay' ? lastRelay : lastRelay ?? canary;
    const job = after ? queueUpgrade(m, after) : startUpgrade(m);
    canary ??= job.id; if (m.kind === 'relay') lastRelay = job.id;
    return { machine_id: m.id, name: m.name, kind: m.kind, job_id: job.id, after: after ?? null };
  });
  // 说法同中控 upgrade_order 的几种情形
  const relays = ms.filter((m) => m.kind === 'relay').length; const nodes = ms.length - relays; const c = ms[0]?.name;
  const order = ms.length === 0 ? '没有要升级的机器' : ms.length === 1 ? `只有「${c}」一台`
    : relays === 0 ? `先升「${c}」做金丝雀，成功后其余 ${nodes - 1} 台节点并发升级；新版本在金丝雀上出问题的话其余的都会取消`
    : relays === 1 ? `先升「${c}」做金丝雀，成功后 ${nodes} 台节点并发升级；新版本在金丝雀上出问题的话其余的都会取消`
    : nodes === 0 ? `先升「${c}」做金丝雀，成功后其余 ${relays - 1} 台线路机逐台升级；新版本在任何一台上出问题，没开始的都会取消`
    : `先升「${c}」做金丝雀，成功后其余 ${relays - 1} 台线路机逐台升级，线路机都成功后 ${nodes} 台节点并发升级；新版本在任何一台线路机上出问题，没开始的都会取消`;
  emit({ type: 'machines' }); return { jobs, skipped: [], order };
});

// ---------- 换线路机 / 搬空线路机 ----------
const MOVE_STEPS: [string, string][] = [['attach_new', '在新线路机建入口'], ['firewall_widen', '放行新线路机（节点防火墙）'], ['wait_applied_new', '等待新线路机应用'], ['verify_new', '经新入口出口验收'], ['routers', '改软路由出口'], ['detach_old', '摘除旧入口'], ['wait_detached_old', '等待旧线路机摘除'], ['firewall_narrow', '收紧节点防火墙']];
const SHARE_NOTE = '换线路机后分享链接会变：客户端需要换成新的分享链接（作业完成后到节点的入口里复制）';
const blankTraffic = () => ({ today_up: 0, today_down: 0, month_up: 0, month_down: 0 });

/** 校验一次换线路机；返回计划，或不能换的原因 */
function planMove(m: MachineView, toId: string, fromId?: string): { old: EntryView; to: MachineView } | string {
  if (m.kind === 'relay') return `「${m.name}」是线路机，只有节点能换线路机`;
  if (m.lifecycle !== 'active') return `「${m.name}」现在不是在用状态，不能换线路机`;
  if (m.active_job) return `「${m.name}」还有进行中的作业，等它结束再换线路机`;
  const to = db.machines.find((x) => x.id === toId && x.kind === 'relay');
  if (!to) return '目标线路机不存在';
  if (to.lifecycle !== 'active') return `目标线路机「${to.name}」不在用`;
  const live = m.entries.filter((e) => !fromId || e.relay.id === fromId);
  if (live.length === 0) return `「${m.name}」没有在用的入口`;
  if (live.length > 1) return `「${m.name}」有 ${live.length} 个入口：请指定要换掉哪条线路机上的入口`;
  const old = live[0];
  if (old.lifecycle !== 'active') return `「${m.name}」在 ${old.relay.name} 上的入口还没生效或已停用`;
  if (old.relay.id === to.id) return `目标线路机「${to.name}」就是节点当前所在的线路机`;
  if (m.entries.some((e) => e.relay.id === to.id)) return `「${m.name}」在「${to.name}」上已经有入口`;
  return { old, to };
}
function startMove(m: MachineView, plan: { old: EntryView; to: MachineView }) {
  const { old, to } = plan;
  const ne: EntryView = { id: `e${db.seq++}`, relay: { id: to.id, name: to.name }, node: { id: m.id, name: m.name }, direct: false, port: Math.max(20000, ...to.entries.map((x) => x.port)) + 1, lifecycle: 'pending', applied: false, apply_error: null, tcp: null, egress: null, reach: null, traffic: blankTraffic() };
  m.entries.push(ne); to.entries.push(ne);
  const job = newJob('move_entry', { id: m.id, name: m.name }, MOVE_STEPS);
  job.result = { new_entry_id: ne.id, old_entry_id: old.id, from_relay_id: old.relay.id, to_relay_id: to.id, share_changed: true, note: '客户端需要换成新的分享链接' };
  m.active_job = { id: job.id, kind: 'move_entry', state: 'running' };
  drive(job, 2200, () => {
    ne.lifecycle = 'active'; ne.applied = true; ne.tcp = { ok: true, latency_ms: 48, detail: null, ts: iso() }; ne.egress = { ok: true, latency_ms: 176, detail: null, ts: iso() };
    m.entries = m.entries.filter((e) => e.id !== old.id); const from = machine(old.relay.id); from.entries = from.entries.filter((e) => e.id !== old.id);
    m.active_job = null; emit({ type: 'relay', id: from.id }); emit({ type: 'relay', id: to.id });
  }, [{ entry_id: ne.id, relay: to.name, port: ne.port }]);
  return job;
}
// 预览：mock 的软路由快照不对入口，routers 一律为空
R('GET', '/machines/([^/]+)/move', ({ params, query }) => {
  const m = machine(params[0]);
  const plan = planMove(m, query.get('to_relay_id') ?? '', query.get('from_relay_id') ?? undefined);
  const base = { routers_enabled: query.get('routers') !== 'false', routers: [], warnings: [], share_changed: true, note: SHARE_NOTE };
  if (typeof plan === 'string') return { ...base, ok: false, blockers: [plan] };
  return { ...base, name: m.name, from: plan.old.relay.name, to: plan.to.name, ok: true, blockers: [] };
});
R('GET', '/relays/([^/]+)/evacuate', ({ params, query }) => {
  const src = machine(params[0]);
  const dst = machine(query.get('to_relay_id') ?? '');
  const jobs: { machine_id: string; name: string; routers: [] }[] = []; const skipped: { machine_id: string; name: string | null; reason: string }[] = [];
  for (const e of src.entries) {
    if (e.lifecycle === 'removing' || e.lifecycle === 'removed' || e.direct) continue;
    const node = machine(e.node.id);
    const plan = planMove(node, dst.id, src.id);
    if (typeof plan === 'string') skipped.push({ machine_id: node.id, name: node.name, reason: plan });
    else jobs.push({ machine_id: node.id, name: node.name, routers: [] });
  }
  return { from: src.name, to: dst.name, jobs, skipped, warnings: [], share_changed: jobs.length > 0, note: SHARE_NOTE };
});
R('POST', '/machines/([^/]+)/move', ({ params, body }) => {
  const m = machine(params[0]);
  const plan = planMove(m, body.to_relay_id, body.from_relay_id);
  if (typeof plan === 'string') throw bad(plan, 409, 'conflict');
  const from = plan.old.relay.name;
  const job = startMove(m, plan);
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'entry.move', title: '换线路机', summary: `${m.name} · ${from} → ${plan.to.name}`, target_kind: 'machine', target_id: m.id, detail: {} });
  emit({ type: 'machines' }); emit({ type: 'job', id: job.id });
  return { job_id: job.id, name: m.name, from, to: plan.to.name, routers_enabled: body.routers !== false, routers: [], ok: true, blockers: [], warnings: [], share_changed: true, note: SHARE_NOTE };
});
R('POST', '/relays/([^/]+)/evacuate', ({ params, body }) => {
  const src = machine(params[0]);
  if (src.kind !== 'relay') throw bad('只有线路机能搬空', 409, 'conflict');
  if (body.to_relay_id === src.id) throw bad('目标线路机不能和要搬空的是同一台');
  const dst = machine(body.to_relay_id);
  if (dst.kind !== 'relay' || dst.lifecycle !== 'active') throw bad('目标线路机不在用', 409, 'conflict');
  const jobs: { machine_id: string; name: string; job_id: string; routers: [] }[] = []; const skipped: { machine_id: string; name: string | null; reason: string }[] = [];
  for (const e of [...src.entries]) {
    if (e.lifecycle === 'removing' || e.lifecycle === 'removed') continue;
    const node = machine(e.node.id);
    const plan = planMove(node, dst.id, src.id);
    if (typeof plan === 'string') { skipped.push({ machine_id: node.id, name: node.name, reason: plan }); continue; }
    jobs.push({ machine_id: node.id, name: node.name, job_id: startMove(node, plan).id, routers: [] });
  }
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'relay.evacuate', title: '搬空线路机', summary: `${src.name} → ${dst.name}（${jobs.length} 个作业）`, target_kind: 'machine', target_id: src.id, detail: {} });
  emit({ type: 'machines' });
  return { from: src.name, to: dst.name, jobs, skipped, warnings: [], share_changed: jobs.length > 0, note: SHARE_NOTE };
});

// ---------- 替换节点 ----------
const REPLACE_STEPS: [string, string][] = [['routers', '改软路由出口'], ['renumber', '交换编号']];
/** 模拟的计划：旧节点去「备用」团队（没有就要求指定），新节点接旧编号；软路由按旧节点的在用入口编一台「<团队>-路由A」 */
function replacePlan(old: MachineView, newId: string | null, teamId: string | null, number: string | null, routers: boolean) {
  const blockers: string[] = []; const warnings: string[] = [];
  const nw = db.machines.find((x) => x.id === newId);
  if (!nw) throw bad('新节点不存在');
  for (const [x, who] of [[old, '旧节点'], [nw, '新节点']] as const) {
    if (x.kind === 'relay') blockers.push(`${who}「${x.name}」是线路机：只有节点能替换`);
    else if (x.lifecycle !== 'active') blockers.push(`${who}「${x.name}」现在不在用，只有在用的节点能替换`);
    if (x.active_job) blockers.push(`「${x.name}」还有进行中的作业：等它结束再替换`);
  }
  if (!nw.entries.some((e) => e.lifecycle === 'active')) blockers.push(`新节点「${nw.name}」没有在用的入口`);
  const spare = teamId ? db.teams.find((t) => t.id === teamId) : db.teams.find((t) => t.spare);
  if (!spare) blockers.push('没有找到旧节点要去的备用团队：指定旧节点去哪个团队');
  let num: number | null = null;
  if (spare) {
    const taken = new Set(db.machines.filter((x) => x.team?.id === spare.id && x.id !== old.id && x.id !== nw.id && x.lifecycle !== 'retired' && x.number != null).map((x) => x.number!));
    if (spare.id === old.team?.id && old.number != null) taken.add(old.number);
    if (number) { const n = +number; if (taken.has(n)) blockers.push(`编号 ${n} 已被占用`); else num = n; } else { let n = 1; while (taken.has(n)) n++; num = n; }
    if (!spare.spare) warnings.push('旧节点要去的不是备用团队：不会到期自动回收');
  }
  if (nw.team?.id !== old.team?.id) warnings.push(`新节点「${nw.name}」不在旧节点的团队，替换时改到这个团队`);
  const rts = old.entries.filter((e) => e.lifecycle === 'active').length && old.team ? [{
    id: `rt-${old.team.id}`, name: `${old.team.name}-路由A`, state: 'online',
    exits: old.entries.filter((e) => e.lifecycle === 'active').map((e, i) => {
      const ne = nw.entries.find((x) => x.relay.id === e.relay.id && x.lifecycle === 'active') ?? nw.entries.find((x) => x.lifecycle === 'active');
      const host = machine(e.relay.id).ip;
      const exit = `节点${String((old.number ?? 1) + i).padStart(2, '0')}`;
      // 第二个出口演示「映射不了」：服务端把它也列在出口里，带原因，同时进阻断
      if (i === 1) { const why = `旧入口在线路机 ${e.relay.name} 上，新节点在这台线路机上没有入口，而它有 2 个入口，不知道换成哪个：先给新节点在 ${e.relay.name} 上加入口，或只留一个入口`; blockers.push(`软路由「${old.team!.name}-路由A」的出口「${exit}」：${why}`); return { exit, old_entry_id: e.id, new_entry_id: null, from: `${host}:${e.port}`, to: null, error: why }; }
      return { exit, old_entry_id: e.id, new_entry_id: ne?.id ?? '', from: `${host}:${e.port}`, to: ne ? `${machine(ne.relay.id).ip}:${ne.port}` : '?' };
    }),
  }] : [];
  if (routers && rts.length === 0) warnings.push('没有软路由的出口指向旧节点：只交换编号');
  const side = (x: MachineView) => ({ id: x.id, name: x.name, kind: x.kind, team: x.team });
  return {
    nw, spare, num,
    view: {
      old: side(old), new: side(nw), new_name: old.team && old.number != null ? displayName(old.team.name, old.number) : old.name,
      old_to: { team: spare ? { id: spare.id, name: spare.name, spare: spare.spare } : null, number: num, name: spare && num != null ? displayName(spare.name, num) : old.name },
      renames: nw.team ? compactPlan(nw.team.id, [nw.id]).map((c) => ({ machine_id: c.m.id, old_name: c.old_name, new_name: c.new_name })) : [], routers_enabled: routers, routers: rts, ok: blockers.length === 0, blockers, warnings,
    },
  };
}
R('GET', '/machines/([^/]+)/replace', ({ params, query }) => replacePlan(machine(params[0]), query.get('new_machine_id'), query.get('team_id'), query.get('number'), query.get('routers') !== 'false').view);
R('POST', '/machines/([^/]+)/replace', ({ params, body }) => {
  const old = machine(params[0]);
  const p = replacePlan(old, body.new_machine_id ?? null, body.old_to?.team_id ?? null, body.old_to?.number != null ? String(body.old_to.number) : null, body.routers !== false);
  if (!p.view.ok) throw bad(p.view.blockers.join('；'), 409, 'conflict');
  const { nw, spare, num } = p;
  const job = newJob('replace', { id: old.id, name: old.name }, REPLACE_STEPS);
  old.active_job = { id: job.id, kind: 'replace', state: 'running' };
  drive(job, 2500, () => {
    const [team, number] = [old.team, old.number];
    if (spare!.spare && !old.spare) { old.spare_from = old.name; old.spare_since = iso(); } old.team = { id: spare!.id, name: spare!.name }; old.spare = spare!.spare; old.number = num; old.name = displayName(spare!.name, num!); old.revision++;
    nw.team = team; nw.spare = false; nw.number = number; nw.name = p.view.new_name; nw.revision++;
    old.active_job = null; refreshCounts();
  }, [{ routers: p.view.routers.map((r) => ({ id: r.id, name: r.name, exits: r.exits.map((e) => ({ exit: e.exit, to: e.to })), backup: `/etc/fleet-backup/openclash/${r.name}.yaml.20261023-020000Z`, delay_ms: r.exits.map(() => 120) })) }, { old: p.view.old_to.name, new: p.view.new_name, renumbered: 0 }]);
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'machine.replace', title: '替换节点', summary: `${old.name} 由 ${nw.name} 接替，旧节点改为 ${p.view.old_to.name}（改 ${p.view.routers.length} 台软路由）`, target_kind: 'machine', target_id: old.id, detail: {} });
  emit({ type: 'machines' }); emit({ type: 'job', id: job.id });
  return { ...p.view, job_id: job.id };
});

// ---------- 设置：API 密钥 / 通知渠道 ----------
const rand = (n: number) => Array.from({ length: n }, () => 'abcdefghijklmnopqrstuvwxyz0123456789'[Math.floor(Math.random() * 36)]).join('');
R('GET', '/api-keys', () => clone(db.apiKeys));
R('POST', '/api-keys', ({ body }) => {
  const scopes: string[] = body.scopes ?? [];
  if (!String(body.name ?? '').trim()) throw bad('请填写密钥名称');
  if (!scopes.length || scopes.some((x) => !['read', 'write', 'secrets', 'exec'].includes(x))) throw bad('请至少选择一项有效权限');
  recheck(body.password, body.totp);
  const prefix = rand(8); const id = `k${db.seq++}`;
  db.apiKeys.unshift({ id, name: String(body.name).trim(), prefix, scopes, created_at: iso(), expires_at: body.expires_days ? iso(Date.now() + body.expires_days * DAY) : null, last_used_at: null, revoked_at: null });
  emit({ type: 'api_keys' });
  return { id, token: `flk_${prefix}_${rand(32)}` };
});
R('DELETE', '/api-keys/([^/]+)', ({ params }) => {
  const k = db.apiKeys.find((x) => x.id === params[0]); if (!k) throw bad('密钥不存在', 404, 'not_found');
  k.revoked_at = iso(); emit({ type: 'api_keys' }); return {};
});
const hintOf = (kind: string, cfg: Record<string, string>) => (kind === 'telegram' ? `chat ${cfg.chat_id ?? ''}` : (() => { try { return new URL(cfg.url).hostname; } catch { return '—'; } })());
const channel = (id: string) => { const c = db.channels.find((x) => x.id === id); if (!c) throw bad('渠道不存在', 404, 'not_found'); return c; };
const pub = (c: Db['channels'][number]) => { const { config: _c, ...rest } = c; return rest; };
const checkCfg = (kind: string, cfg: Record<string, string>) => {
  const need = kind === 'telegram' ? ['bot_token', 'chat_id'] : ['url'];
  if (!['telegram', 'wecom', 'webhook'].includes(kind)) throw bad('不支持的渠道类型');
  if (need.some((k) => !String(cfg?.[k] ?? '').trim())) throw bad('配置不完整');
};
R('GET', '/notify-channels', () => db.channels.map(pub));
R('POST', '/notify-channels', ({ body }) => {
  if (!String(body.name ?? '').trim()) throw bad('请填写渠道名称');
  checkCfg(body.kind, body.config);
  const c = { id: `c${db.seq++}`, name: String(body.name).trim(), kind: body.kind, min_severity: body.min_severity ?? 'warning', enabled: body.enabled !== false, created_at: iso(), config_hint: hintOf(body.kind, body.config), config: body.config };
  db.channels.push(c); emit({ type: 'catalog' }); return { id: c.id };
});
R('PATCH', '/notify-channels/([^/]+)', ({ params, body }) => {
  const c = channel(params[0]);
  if (body.name !== undefined) c.name = String(body.name).trim();
  if (body.config !== undefined) { checkCfg(c.kind, body.config); c.config = body.config; c.config_hint = hintOf(c.kind, body.config); }
  if (body.min_severity !== undefined) c.min_severity = body.min_severity;
  if (body.enabled !== undefined) c.enabled = !!body.enabled;
  emit({ type: 'catalog' }); return pub(c);
});
R('DELETE', '/notify-channels/([^/]+)', ({ params }) => { channel(params[0]); db.channels = db.channels.filter((c) => c.id !== params[0]); emit({ type: 'catalog' }); return {}; });
R('POST', '/notify-channels/([^/]+)/test', ({ params }) => {
  const c = channel(params[0]);
  const fail = c.name.includes('失败') || JSON.stringify(c.config).includes('fail');
  const error = fail ? '对端返回 HTTP 502（Bad Gateway）' : null;
  db.notifyLog.unshift({ id: `n${db.seq++}`, channel_id: c.id, channel_name: c.name, incident_id: null, ts: iso(), ok: !fail, error, kind: 'test', subject_name: null, rule_name: null });
  return { ok: !fail, error };
});
R('GET', '/notify-log', ({ query }) => clone(db.notifyLog.slice(0, +(query.get('limit') ?? 100))));

// ---------- 账户安全：两步验证 / 登录会话 / 改密码 ----------
// mock 不真算 TOTP：任意 6 位数字都算对，000000 算错；恢复码按列表核对、用一次就删。密码只认「wrong」为错
const acct = {
  totp: false, pending: false, recovery: [] as string[],
  sessions: [
    { id: 'a1b2c3d4e5f60718', created_at: iso(Date.now() - 2 * DAY), last_seen_at: iso(), expires_at: iso(Date.now() + 12 * DAY), ip: '198.51.100.23', user_agent: navigator.userAgent, current: true },
    { id: '0f1e2d3c4b5a6978', created_at: iso(Date.now() - 6 * DAY), last_seen_at: iso(Date.now() - 3 * 3600_000), expires_at: iso(Date.now() + 8 * DAY), ip: '203.0.113.88', user_agent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1', current: false },
    { id: '99aa88bb77cc66dd', created_at: iso(Date.now() - 9 * DAY), last_seen_at: iso(Date.now() - 4 * DAY), expires_at: iso(Date.now() + 5 * DAY), ip: '2001:db8:4:1::27', user_agent: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0', current: false },
  ],
};
const recoveryCodes = () => Array.from({ length: 10 }, () => `${rand(4)}-${rand(4)}-${rand(4)}`);
/** 校验第二步：6 位数字（000000 为错）或一个没用过的恢复码 */
const checkCode = (c: string) => {
  const s = String(c ?? '').trim();
  if (/^\d{6}$/.test(s.replace(/\s/g, ''))) { if (s === '000000') throw bad('两步验证码不对或已经用过', 401, 'totp_invalid'); return; }
  const i = acct.recovery.indexOf(s.toLowerCase());
  if (i < 0) throw bad('两步验证码不对或已经用过', 401, 'totp_invalid');
  acct.recovery.splice(i, 1);
};
const checkPw = (p: string) => { if (!p || p === 'wrong') throw bad('当前密码不对', 401, 'bad_password'); };
/** 再次校验：当前密码，开了两步验证还要验证码 */
function recheck(pw: string, code: string | undefined) {
  if (acct.totp && !String(code ?? '').trim()) throw bad('请输入两步验证码', 401, 'totp_required');
  checkPw(pw); if (acct.totp) checkCode(String(code));
}
const mockQr = (text: string) => {
  // 不是真的二维码：按文本哈希画一张 25×25 的点阵，外观像就行
  let h = hashStr(text); let cells = '';
  for (let y = 0; y < 25; y++) for (let x = 0; x < 25; x++) {
    h = (Math.imul(h, 1103515245) + 12345) >>> 0;
    // 三个角上的定位方块：7×7 外框 + 3×3 实心
    const lx = x > 17 ? x - 18 : x; const ly = y > 17 ? y - 18 : y;
    const finder = lx < 7 && ly < 7 && !(x > 17 && y > 17);
    const on = finder ? lx === 0 || lx === 6 || ly === 0 || ly === 6 || (lx >= 2 && lx <= 4 && ly >= 2 && ly <= 4) : (h >>> 16) % 2 === 0;
    if (on) cells += `M${x + 2} ${y + 2}h1v1h-1z`;
  }
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 29 29" shape-rendering="crispEdges"><rect width="29" height="29" fill="#fff"/><path d="${cells}" fill="#000"/></svg>`;
};
R('GET', '/account/totp', () => ({ enabled: acct.totp, recovery_codes_left: acct.recovery.length }));
R('POST', '/account/totp/setup', ({ body }) => {
  if (acct.totp) throw bad('两步验证已经开着；要换手机请先停用再重新开启', 409, 'conflict');
  checkPw(body?.password);
  const secret = Array.from({ length: 32 }, () => 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567'[Math.floor(Math.random() * 32)]).join('');
  const url = `otpauth://totp/Fleet:${db.user}%40mock?secret=${secret}&issuer=Fleet&algorithm=SHA1&digits=6&period=30`;
  acct.pending = true;
  return { secret, otpauth_url: url, qr_svg: mockQr(url) };
});
R('POST', '/account/totp/enable', ({ body }) => {
  if (!acct.pending) throw bad('没有待确认的密钥，或已超过 10 分钟，请重新开始');
  if (!/^\d{6}$/.test(String(body.code ?? '').replace(/\s/g, ''))) throw bad('验证码是 6 位数字');
  if (body.code === '000000') throw bad('验证码不对：确认扫的是刚才的二维码、手机时间准确', 400, 'totp_invalid');
  acct.totp = true; acct.pending = false; acct.recovery = recoveryCodes();
  return { recovery_codes: [...acct.recovery] };
});
R('POST', '/account/totp/disable', ({ body }) => {
  if (!acct.totp) throw bad('两步验证没有开', 409, 'conflict');
  checkPw(body.password); checkCode(body.code);
  acct.totp = false; acct.recovery = []; return { ok: true };
});
R('POST', '/account/totp/recovery-codes', ({ body }) => {
  if (!acct.totp) throw bad('两步验证没有开', 409, 'conflict');
  checkCode(body.code); acct.recovery = recoveryCodes();
  return { recovery_codes: [...acct.recovery] };
});
R('GET', '/account/sessions', () => clone(acct.sessions));
R('DELETE', '/account/sessions/([^/]+)', ({ params }) => {
  const s = acct.sessions.find((x) => x.id === params[0]); if (!s) throw bad('没有这个会话（可能已经过期或被撤销）', 404, 'not_found');
  acct.sessions = acct.sessions.filter((x) => x !== s); return { ok: true, current: s.current };
});
R('POST', '/account/sessions/revoke-others', ({ body }) => { recheck(body?.password, body?.totp); const n = acct.sessions.filter((s) => !s.current).length; acct.sessions = acct.sessions.filter((s) => s.current); return { revoked: n }; });
R('POST', '/account/password', ({ body }) => {
  if ([...String(body.new ?? '')].length < 12) throw bad('新密码至少 12 个字符');
  if (acct.totp && !String(body.totp ?? '').trim()) throw bad('请输入两步验证码', 401, 'totp_required');
  checkPw(body.current); if (acct.totp) checkCode(body.totp);
  if (body.new === body.current) throw bad('新密码和当前密码一样');
  const n = acct.sessions.filter((s) => !s.current).length; acct.sessions = acct.sessions.filter((s) => s.current);
  const keys = body.revoke_api_keys ? db.apiKeys.filter((k) => !k.revoked_at) : [];
  keys.forEach((k) => { k.revoked_at = iso(); }); if (keys.length) emit({ type: 'api_keys' });
  return { ok: true, revoked_sessions: n, revoked_api_keys: keys.length };
});
// 登录：开了两步验证时要码（放到路由表最前面，盖过上面不看验证码的那条）
routes.unshift(['POST', /^\/auth\/login$/, ({ body }) => {
  if (!body?.username) throw bad('请输入用户名');
  if (body.password === 'wrong') throw bad('用户名或密码不对', 401, 'bad_credentials');
  if (acct.totp) { if (!String(body.totp ?? '').trim()) throw bad('请输入两步验证码', 401, 'totp_required'); checkCode(body.totp); }
  db.user = body.username; sessionStorage.setItem('mock:auth', '1'); return { username: db.user };
}]);

// ---------- 安装 ----------
/** 模拟上报：改读数，按服务端 metrics 消息的形状推增量（见 lib/live.ts） */
function jitter() {
  const machines: Record<string, unknown> = {};
  const now = Math.floor(Date.now() / 1000);
  for (const m of db.machines) if (m.host && m.lifecycle === 'active' && m.health.status !== 'down') {
    const h = m.host; const f = () => 0.85 + Math.random() * 0.3;
    // 同机 IP 节点的读数就是宿主机那一份（同一个对象，宿主机排在前面已经改过），只照推
    if (!m.host_machine) {
      h.cpu_pct = Math.max(1, Math.min(99, (h.cpu_pct ?? 10) * f())); h.net_rx_bps = Math.round((h.net_rx_bps ?? 1e6) * f()); h.net_tx_bps = Math.round((h.net_tx_bps ?? 1e6) * f()); h.ts = iso(now * 1000);
      if (h.load1 != null) h.load1 = +((h.cpu_pct ?? 0) / 40).toFixed(2);
      if (m.agent) m.agent.last_report_at = h.ts;
    }
    machines[m.id] = { host: [now, h.cpu_pct, h.load1, h.mem_used, h.mem_total, h.disk_used, h.disk_total, h.net_rx_bps, h.net_tx_bps, h.tcp_conns], seen: now };
  }
  emit({ type: 'metrics', machines, entries: {}, overview: overview() });
}

export function installMock() {
  bootFixtures();
  const realFetch = window.fetch.bind(window);
  window.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.href);
    if (url.pathname === '/echo') return new Response('198.51.100.200', { status: 200 });
    if (!url.pathname.startsWith('/api/v1/')) return realFetch(input, init);
    const path = url.pathname.slice('/api/v1'.length);
    const method = (init?.method ?? (input instanceof Request ? input.method : 'GET')).toUpperCase();
    await new Promise((r) => setTimeout(r, 60 + Math.random() * 120));
    const json = (status: number, b: unknown) => new Response(JSON.stringify(b), { status, headers: { 'Content-Type': 'application/json' } });
    if (method !== 'GET' && new Headers(init?.headers).get('X-Fleet-Request') !== '1') return json(403, { error: { code: 'csrf', message: '缺少 X-Fleet-Request 头' } });
    if (path !== '/auth/login' && path !== '/auth/me' && sessionStorage.getItem('mock:auth') === '0') return json(401, { error: { code: 'unauthorized', message: '未登录' } });
    for (const [m, re, h] of routes) {
      if (m !== method) continue;
      const mt = re.exec(path); if (!mt) continue;
      try {
        let body: unknown = {};
        if (init?.body && typeof init.body === 'string') body = JSON.parse(init.body);
        return json(200, { data: h({ params: mt.slice(1).map(decodeURIComponent), query: url.searchParams, body }) });
      } catch (e) {
        if (e instanceof HttpErr) return json(e.status, { error: { code: e.code, message: e.message } });
        console.error('[mock]', e); return json(500, { error: { code: 'internal', message: `mock 内部错误：${(e as Error).message}` } });
      }
    }
    return json(404, { error: { code: 'not_found', message: `mock 未实现：${method} ${path}` } });
  };
  window.WebSocket = new Proxy(window.WebSocket, {
    construct: (Real, args: ConstructorParameters<typeof WebSocket>) => (String(args[0]).includes('/api/v1/stream') ? new MockStream() : new Real(...args)),
  });
  setInterval(jitter, 10_000);
  // 演示入口：?logout=1 可模拟未登录
  if (new URLSearchParams(location.search).get('logout') === '1') sessionStorage.setItem('mock:auth', '0');
  else if (sessionStorage.getItem('mock:auth') === null) sessionStorage.setItem('mock:auth', '1');
}
