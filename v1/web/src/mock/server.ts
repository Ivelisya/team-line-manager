// 浏览器内 mock：拦截 /api/v1/* 的 fetch 和实时流 WebSocket。VITE_MOCK=1 时由 main.tsx 安装。
// 有状态：改名、退役、上架、续费等操作都会改内存里的数据，并通过 mock 实时流通知前端刷新。

import type { Availability, AvailabilityDay, EntryMode, EntryView, Incident, Job, JobLog, JobSummary, MachineView, ProbeBucket, ProvisionCheckItem, ProvisionItem, SeriesPoint, SilenceKind } from '@/lib/types';
import { addDays, addMonths, displayName } from '@/lib/format';
import { inheritedValue, isIdleSpare, isQuiet, isUnbilledSpare, liveRate } from '@/lib/derive';
import { makeDefaultCompare } from '@/lib/sort';
import { cycleMonths, META, subCalc } from './meta';

const LIFECYCLE_LABEL = META.lifecycles;
import { billingDay, buildDb, countryOf, hashStr, IP_STEPS, refreshFollowers, rng, SUB_FIELDS, syncSub, templateOf, trafficOf, type Db } from './fixtures';
import { installAccessMock } from './access';
import { installReportsMock } from './reports';
import { installTrafficMock, mockIdle, mockMonthly, mockPerGb, mockUsage } from './traffic';
import { installFxMock, toUsd, usdPerUnit, usdTotal } from './fx';
import { installPolicyMock, policyHook } from './policy';
import { installRoutersMock } from './routers';
import { installTerminalMock } from './terminal';
import { installClientSubsMock, subNamesOfMachine } from './clientSubs';

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
/** 套餐、供应商改了：没单独设置的订阅项跟着变，同时重算到期状态、月费折算与流量额度 */
function refreshAllFollowers() {
  for (const m of db.machines) { refreshFollowers(m, db.plans, db.providers); if (m.subscription) { recomputeDue(m); Object.assign(m, trafficOf(m, db.machines)); } }
}
function recomputeDue(m: MachineView) {
  if (m.subscription) { m.subscription.due_days = Math.ceil((new Date(m.subscription.due_date).getTime() - Date.now()) / DAY); subCalc(m.subscription); }
  // 备用宿主机上不在备用团队、没退役的同机 IP 节点（同服务端 live_ip_nodes）
  m.live_ip_nodes = m.spare && m.lifecycle === 'active' ? db.machines.filter((x) => x.host_machine?.id === m.id && x.lifecycle !== 'retired' && !x.spare).map((x) => x.name) : [];
  // 备用宿主机上在备用团队里、开着自动续费的同机 IP 节点（同服务端 renewing_ip_nodes）：宿主机回收会连带它们
  m.renewing_ip_nodes = m.spare && m.lifecycle === 'active' ? db.machines.filter((x) => x.host_machine?.id === m.id && x.lifecycle !== 'retired' && x.spare && !!x.subscription?.auto_renew).map((x) => x.name) : [];
  m.recycle_held = m.spare && m.lifecycle === 'active' && (m.router_exits.length > 0 || m.live_ip_nodes.length > 0 || m.renewing_ip_nodes.length > 0 || !!m.subscription?.auto_renew); // 费用侧：还被出口指着、IP 节点还在用、IP 节点开着自动续费、自己开着自动续费的备用不自动回收，照常续费
  m.idle_spare = m.spare && m.lifecycle === 'active' && !m.router_exits.length && !m.live_ip_nodes.length; // 闲置备用（客户侧）：没有出口指着、没有在用 IP 节点，不看自动续费
  m.spare_unbilled = m.spare && m.lifecycle === 'active' && !m.recycle_held; // 不续费的备用机（费用侧）：没被扣住
  m.recycle_on = m.spare && m.lifecycle === 'active' && m.subscription && !m.recycle_held ? addDays(m.subscription.due_date, 2) : null; // 备用：到期 + 2 天 0 点起自动回收
}

function overview() {
  const live = db.machines.filter((m) => m.lifecycle === 'active');
  // 闲置备用（同服务端 MachineView::idle_spare）：不进健康计数与在线 / 离线，另数 spare；速率照算
  const idle = live.filter(isIdleSpare); const biz = live.filter((m) => !isIdleSpare(m));
  // 费用侧（同服务端 spare_unbilled）：不续费的备用机不算到期、月费不进合计，另列小计；开着自动续费的闲置备用照常续费
  const unbilled = live.filter(isUnbilledSpare); const billed = live.filter((m) => !isUnbilledSpare(m));
  const cnt = (list: MachineView[]) => ({
    total: list.length, ok: list.filter((m) => m.health.status === 'ok').length, degraded: list.filter((m) => m.health.status === 'degraded').length,
    down: list.filter((m) => m.health.status === 'down').length, unknown: list.filter((m) => m.health.status === 'unknown').length, maintenance: list.filter((m) => m.health.status === 'maintenance').length,
  });
  let up = 0; let down = 0;
  for (const m of live.filter((x) => x.kind !== 'relay')) for (const e of m.entries) { up += e.traffic.month_up; down += e.traffic.month_down; }
  // 本月数据的起点（同服务端）：在用线路机里有一台月初前就在用就完整，否则取其中最早的首次上架时刻
  const rf = live.filter((m) => m.kind === 'relay').map((m) => m.month_counted_from);
  const counted_from = rf.some((t) => !t) ? null : rf.sort()[0] ?? null;
  return {
    nodes: cnt(biz.filter((m) => m.kind !== 'relay')), relays: cnt(biz.filter((m) => m.kind === 'relay')), spare: idle.length,
    spare_health: cnt(idle), // 闲置备用自己的健康计数（同服务端）：备用池里有几台能顶替
    provisioning: db.machines.filter((m) => m.lifecycle === 'provisioning').length, retiring: db.machines.filter((m) => m.lifecycle === 'retiring').length,
    failed: db.machines.filter((m) => m.lifecycle === 'failed').length,
    // 需要续费的按订阅的 due_state（同服务端）：自动续费的只在已过期时算
    // 在线/离线按台机器、按 agent 连通（同 isQuiet）：同机 IP 节点随宿主机计入，家宽代理不需要 agent（no_agent）
    online: biz.filter((m) => m.agent && !isQuiet(m)).length, offline: biz.filter((m) => m.agent && isQuiet(m)).length,
    // 闲置备用不计入 no_agent、ip_nodes（同 total / online / offline，同服务端 overview_of）
    no_agent: biz.filter((m) => !m.agent).length, ip_nodes: biz.filter((m) => m.host_machine).length,
    due_soon: billed.filter((m) => m.subscription?.due_state === 'soon').length, overdue: billed.filter((m) => m.subscription?.due_state === 'overdue').length,
    // 不续费的备用机：月费合计不含，另列小计（同服务端 monthly_usd_spare）；备用团队里被出口指着 / 有在用 IP 节点 / 开着自动续费的照常续费，算进合计
    monthly_usd: usdTotal(billed.flatMap((m) => (m.subscription ? [m.subscription] : []))),
    paid_machines: billed.filter((m) => m.subscription).length,
    monthly_usd_spare: { ...usdTotal(unbilled.flatMap((m) => (m.subscription ? [m.subscription] : []))), count: unbilled.filter((m) => m.subscription).length },
    traffic_month: { up, down, counted_from },
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
    if (!h && m.kind !== 'proxy') { m.tune = { profile: 'tune-v2', at: iso(), current: true, params: { bw: 1000, bw_plan: null, bw_source: 'default' as const, rtt: 100, rtt_source: 'default' as const, rtt_samples: 0, mem_mb: 3911, buf_max: 27097152 } }; m.agent = { version: '1.0.3', last_report_at: iso(), online: true, xray_running: null, cores: 2, health: null }; m.host = { ts: iso(), cpu_pct: 6, load1: 0.1, mem_used: 400 * 1024 ** 2, mem_total: 4 * GiB, disk_used: 4 * GiB, disk_total: 60 * GiB, net_rx_bps: 2e5, net_tx_bps: 1e5, tcp_conns: 12 }; }
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
/** 检查结果；relay_port 是模拟里给新入口预留的端口（真实接口不返回，网页不看） */
type MockCheck = ProvisionCheckItem & { relay_port?: number };
function checkProvision(items: ProvisionItem[]): MockCheck[] {
  const usedNum = new Map<string, Set<number>>();
  for (const m of db.machines) if (m.team && m.number != null && m.lifecycle !== 'retired') { const s = usedNum.get(m.team.id) ?? new Set(); s.add(m.number); usedNum.set(m.team.id, s); }
  const ips = new Map(db.machines.filter((m) => m.lifecycle !== 'retired').map((m) => [m.ip, m.name]));
  const portUsed = new Map<string, number>();
  const ssBatch = new Map<string, number[]>(); // 宿主机 → 这一批分到的 SS 端口
  const out: MockCheck[] = [];
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
    // SSH 通了再用中控密钥试登录（同服务端 provision::ssh_check）：模拟里末段是 66 的当作密钥被拒（没填密码时不通过），末段是 20 的当作 Ubuntu 20.04
    const warnings: string[] = [];
    if (!errors.length && it.kind !== 'proxy' && !it.host_id) {
      const port = `${it.ip}:${it.ssh_port ?? 22}`;
      if (it.ip.endsWith('.66') && !it.login?.password) errors.push(`中控密钥登录不了 ${port}（密钥被拒），也没填 root 密码。二选一：提供 root 密码（网页填「root 密码」，fleetctl 用 --ssh-password-env 变量名）；或在供应商面板（或机器上 root 的 ~/.ssh/authorized_keys）录入中控公钥：${META.ssh_public_key}`);
      else if (it.ip.endsWith('.20')) warnings.push('系统是 Ubuntu 20.04，已过标准支持期、不再有常规安全更新（Ubuntu 22.04 起仍在支持）：可以照常上架，建议找机会重装成新版本');
    }
    // 入口模式（同服务端 provision::entry_modes）：没给按设置里的默认；家宽代理、同机 IP 节点要穿透时退回普通并提醒
    let entryMode: EntryMode | null = null;
    if (it.kind !== 'relay') {
      const want = it.entry_mode ?? ENTRY_MODE.default;
      const why = want === 'reverse' ? (it.kind === 'proxy' ? '家宽代理没有 SSH 和 agent，装不了穿透要的 bridge' : it.host_id ? '同机 IP 节点暂不支持穿透（bridge 要从它自己的 IP 出站，第一版没做）' : null) : null;
      entryMode = why ? 'normal' : want;
      if (why) warnings.push(`按普通模式建入口：${why}`);
    }
    out.push({ ok: errors.length === 0, errors, warnings, name, number, relay_port: relayPort, ss_port: ssPort, entry_mode: entryMode });
  }
  return out;
}

function createMachine(it: ProvisionItem, c: MockCheck): MachineView {
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
    sub_template: templateOf(plan ?? undefined, pv ?? undefined), lifecycle: 'provisioning', lifecycle_reason: null, note: it.note ?? '', revision: 1, created_at: iso(), activated_at: null, retired_at: null, spare_from: null, spare_since: null, router_exits: [], recycle_on: null, recycle_held: false,
    subscription: it.subscription?.due_date ? subCalc({ price_cents: it.subscription.price_cents ?? plan?.price_cents ?? 0, currency: plan?.currency ?? META.defaults.currency, cycle: it.subscription.cycle ?? plan?.cycle ?? META.defaults.cycle, due_date: it.subscription.due_date, due_days: 0, billing_tz: it.subscription.billing_tz ?? pv?.timezone ?? META.org_timezone, auto_renew: false, remind_days: pv?.remind_days ?? META.defaults.remind_days, traffic_gb: plan?.traffic_gb || null, traffic_reset_day: 1, traffic_rule: pv?.traffic_rule ?? META.defaults.traffic_rule } as NonNullable<MachineView['subscription']>) : null,
    agent: null, host: null, entries: [], removed_traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 }, nic: null, month_counted_from: null, quota: { used: 0, period_start: '', period_end: null, mode: 'monthly', counted_from: null, rule: null, base: null, limit_gb: null, limit_source: 'none', daily_avg: null, forecast_full_at: null }, health: { status: 'unknown', reasons: [{ key: 'provisioning', severity: 'info', text: '上架作业进行中' }] }, active_job: null, failed_job: null, open_incidents: 0, maintenance: false,
    relay: it.kind === 'relay' ? { public_host: it.ip, generation: 1, applied_generation: 0, applied_at: null, apply_error: null, port_min: 20001, port_max: 20999 } : null,
    tune: it.kind === 'proxy' || h ? null : { profile: null, at: null, current: false, params: null },
  };
  if (h) borrowHost(m, h);
  if (m.kind !== 'relay') {
    const relay = machine(it.relay_id!);
    const e: EntryView = { id: `e${db.seq++}`, relay: { id: relay.id, name: relay.name }, node: { id, name: m.name }, direct: false, mode: c.entry_mode ?? 'normal', port: c.relay_port ?? 20999, lifecycle: 'pending', applied: false, apply_error: null, tcp: null, egress: null, reach: null, traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 } };
    m.entries = [e]; relay.entries.push(e);
  }
  recomputeDue(m); Object.assign(m, trafficOf(m, db.machines));
  syncSub(m, db.plans, db.providers);
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

// 团队带在用机器的月费合计（USD，同服务端）
R('GET', '/teams', () => clone(db.teams).map((t) => {
  const ms = db.machines.filter((m) => m.lifecycle === 'active' && m.team?.id === t.id && m.subscription);
  const held = t.spare ? ms.filter((m) => m.recycle_held) : [];
  return { ...t, monthly_usd: usdTotal(ms.map((m) => m.subscription!)), monthly_usd_held: usdTotal(held.map((m) => m.subscription!)), held_count: held.length,
    held_total: t.spare ? db.machines.filter((m) => m.lifecycle === 'active' && m.team?.id === t.id && m.recycle_held).length : 0 };
}));
R('POST', '/teams', ({ body }) => {
  if (!body.name?.trim()) throw bad('团队名不能为空');
  if (db.teams.some((t) => t.name === body.name)) throw bad('团队名已存在', 409, 'conflict');
  const t = { id: `t${db.seq++}`, name: body.name.trim(), sort_order: body.sort_order ?? db.teams.length + 1, spare: false, node_count: 0, monthly_usd: usdTotal([]) };
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
  const p = { id: `p${db.seq++}`, name: body.name, code: String(body.code).toUpperCase(), panel_url: body.panel_url || null, timezone: body.timezone || null, traffic_rule: body.traffic_rule ?? META.defaults.traffic_rule, traffic_reset: body.traffic_reset ?? META.defaults.traffic_reset, remind_days: body.remind_days ?? META.defaults.remind_days };
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
  if (body.traffic_reset !== undefined) p.traffic_reset = body.traffic_reset;
  if (body.remind_days !== undefined) p.remind_days = body.remind_days;
  refreshAllFollowers();
  emit({ type: 'catalog' }); emit({ type: 'machines' }); return { id: p.id };
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
R('GET', '/plans', () => clone(db.plans).map((p) => ({ ...p, usd_per_unit: usdPerUnit(p.currency), price_usd_cents: toUsd(p.price_cents, p.currency), monthly_usd_cents: toUsd(Math.round(p.price_cents / cycleMonths(p.cycle)), p.currency) })));
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
  refreshAllFollowers();
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
/** 进备用时记下的原团队（同服务端 machine.spare_from_team_id）：机器 id → 团队 id；移出备用清掉 */
const spareFromTeam = new Map<string, string>();
/** 随宿主机换团队的同机 IP 节点及去向团队（同服务端 IP_WITH_HOST / IP_DEST）：进备用带走所有不在备用的（含别的团队的）；
 *  撤销退役（原团队是备用团队）只带与宿主机同一次进备用的（spare_since 相同），各回进备用前的团队（与宿主机原团队相同的、没记的跟宿主机走，那个团队已不在的留在备用）；
 *  其余只带还在原团队的。`stay`：撤销时原团队已不在、留在备用的 */
function ipFollow(m: MachineView, to: { id: string; spare: boolean }) {
  const out: { x: MachineView; dest: string }[] = []; const stay: MachineView[] = [];
  const from = m.team?.id; if (!from || from === to.id) return { out, stay };
  const fromSpare = !!m.spare;
  for (const x of ipNodesLeaving(m)) {
    if (to.spare) { if (!x.spare) out.push({ x, dest: to.id }); continue; }
    if (x.team?.id !== from || (fromSpare && x.spare_since !== m.spare_since)) continue;
    const back = fromSpare ? spareFromTeam.get(x.id) : undefined;
    // 与宿主机进备用前同团队的跟宿主机走（同普通改团队；服务端 IP_DEST 的 $6）
    if (!back || back === to.id || back === spareFromTeam.get(m.id)) out.push({ x, dest: to.id });
    else if (db.teams.some((t) => t.id === back && !t.spare)) out.push({ x, dest: back });
    else stay.push(x);
  }
  return { out, stay };
}
const freeNumber = (tid: string) => { const used = new Set(db.machines.filter((x) => x.team?.id === tid && x.lifecycle !== 'retired').map((x) => x.number)); let n = 1; while (used.has(n)) n++; return n; };
R('GET', '/machines/([^/]+)/rename-preview', ({ params, query }) => {
  const m = machine(params[0]); const tid = query.get('team_id') ?? m.team?.id; const team = tid ? teamOf(tid) : null;
  if (!team || m.kind === 'relay') return { name: m.name, old_name: m.name, renames: [], ip_nodes: [], router_exits: [], spare_blocked: null, held: null, warnings: [] };
  const number = query.get('number') ? Number(query.get('number')) : team.id !== m.team?.id ? freeNumber(team.id) : m.number!;
  const moved = !!m.team && team.id !== m.team.id;
  // 宿主机换团队：还在原团队的同机 IP 节点一起走，宿主机先占 number，它们依次取剩下的最小空号（同服务端）
  const { out: follow, stay } = moved ? ipFollow(m, team) : { out: [], stay: [] };
  const ips = follow.map((f) => f.x);
  const takenOf = new Map<string, Set<number>>();
  const takenIn = (tid: string) => { let t = takenOf.get(tid); if (!t) { t = new Set(db.machines.filter((x) => x.team?.id === tid && x.lifecycle !== 'retired').map((x) => x.number!)); takenOf.set(tid, t); } return t; };
  takenIn(team.id).add(number);
  const ip_nodes = follow.map(({ x, dest }) => { const t = takenIn(dest); let k = 1; while (t.has(k)) k++; t.add(k); return { machine_id: x.id, old_name: x.name, new_name: displayName(teamOf(dest).name, k) }; });
  // 少了人的团队：原团队，加上带走的 IP 节点所在的别的团队（目标团队只取空号，不压缩）
  const shrunk = [...new Set([m, ...ips].flatMap((x) => (x.team && x.team.id !== team.id ? [x.team.id] : [])))];
  const plan = moved ? shrunk.flatMap((t) => compactPlan(t, [m.id, ...ips.map((x) => x.id)])) : [];
  return { name: displayName(team.name, number), old_name: m.name, renames: plan.map((c) => ({ machine_id: c.m.id, old_name: c.old_name, new_name: c.new_name })), ip_nodes,
    // 换团队：还指向这台和一起走的同机 IP 节点的软路由出口（同服务端，进备用、改到别的团队都列）
    router_exits: moved ? [m, ...ips].flatMap((x) => exitsUsing(x).flatMap((r) => r.exits.map((e) => ({ node: x.name, router: r.name, exit: e.exit })))) : [],
    spare_blocked: moved && team.spare && !m.spare ? spareGate(m) : null,
    held: moved && team.spare && !m.spare && !spareGate(m) ? heldReason(m, ips) : null,
    warnings: [...stay.map((x) => `${x.name} 原团队已不在，留在备用`), ...compactExitsNote(plan, '不想错位就别把这台挪走：要换掉它用替换（新节点接它的编号，团队里谁都不改名）')] };
});
R('PATCH', '/machines/([^/]+)', ({ params, body }) => {
  const m = machine(params[0]);
  if (body.revision !== m.revision) throw bad('这台机器刚被其他人修改过，请刷新后重试', 409, 'revision_conflict');
  if (body.team_id !== undefined || body.number !== undefined) {
    const team = teamOf(body.team_id ?? m.team?.id);
    const from = m.team?.id;
    const number = body.number ?? (team.id !== from ? freeNumber(team.id) : m.number); // 换团队没给编号：排到新团队的最小空号（同服务端）
    if (db.machines.some((x) => x.id !== m.id && x.team?.id === team.id && x.number === number && x.lifecycle !== 'retired')) throw bad(`${displayName(team.name, number)} 已被占用`, 409, 'conflict');
    const wasSpare = m.spare, oldName = m.name;
    const blocked = team.id !== from && team.spare && !wasSpare ? spareGate(m) : null; // 手动改进备用和退役的「移进备用」同一套资格（同服务端 spare_gate）
    if (blocked) throw bad(`不能改到备用团队：${blocked}。要进备用请用退役（网页「退役」、fleetctl retire），现在就回收选「立即回收」`, 409, 'conflict');
    const ips = ipFollow(m, team).out; // 宿主机换团队：同机 IP 节点一起走（进备用带走所有不在备用的；撤销退役只带同一次进备用的，各回自己进备用前的团队）
    const at = iso(); // 同一次进备用的时刻相同（撤销时据此认）
    const srcTeams = [...new Set([...(from ? [from] : []), ...ips.flatMap(({ x }) => (x.team ? [x.team.id] : []))])].filter((t) => t !== team.id);
    const enter = (x: MachineView, old: string, oldTeam: string | undefined) => { // 同服务端 patch_nobump：进备用记时刻、原名、原团队，移出备用清掉
      if (x.spare && !wasSpare) { x.spare_from = old; x.spare_since = at; if (oldTeam) spareFromTeam.set(x.id, oldTeam); } else if (!x.spare) { x.spare_from = null; x.spare_since = null; spareFromTeam.delete(x.id); }
    };
    m.team = { id: team.id, name: team.name }; m.spare = team.spare; m.number = number;
    enter(m, oldName, from);
    for (const { x, dest } of ips) { // 接在宿主机之后取去向团队的最小空号
      const xold = x.name, xteam = x.team?.id; const dt = teamOf(dest); const k = freeNumber(dt.id); x.team = { id: dt.id, name: dt.name }; x.spare = dt.spare; x.number = k; x.revision++;
      enter(x, xold, xteam);
      renameMachine(x);
    }
    for (const t of srcTeams) compactApply(t, '改团队后'); // 原团队（和带走的 IP 节点所在的团队）少了人：编号连续
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
/** 压缩改名的节点还被出口指着：面板名和出口名会对不上（同服务端 routers::compact_exits_note）；没有返回 [] */
function compactExitsNote(changes: { m: MachineView; old_name: string; new_name: string }[], tail: string): string[] {
  const list = changes.flatMap((c) => { const xs = exitsUsing(c.m).flatMap((r) => r.exits.map((e) => `${r.name} · ${e.exit}`)); return xs.length ? [`${c.old_name}（${xs.join('、')}）→ ${c.new_name}`] : []; });
  return list.length ? [`编号压缩会给软路由出口指着的节点改名，出口名不跟着改，改完面板名和出口名就对不上了：${list.join('；')}。${tail}`] : [];
}
/** 出口还指着这台或它上面没退役的同机 IP 节点：进备用后不自动回收（同服务端 exits_using_with_ip_nodes） */
const exitsHold = (m: MachineView, xs: MachineView[]) => [m, ...xs].some((x) => exitsUsing(x).length > 0);
/** 进备用后不自动回收的原因（同服务端 spare_held_reason）：出口还指着，或宿主机上已经在备用里、开着自动续费的同机 IP 节点（不随这次进备用、自动续费不会被关）；没有为 null */
function heldReason(m: MachineView, xs: MachineView[]): string | null {
  const why = [exitsHold(m, xs) && '软路由出口还指着它', db.machines.some((x) => x.host_machine?.id === m.id && x.lifecycle !== 'retired' && x.spare && !!x.subscription?.auto_renew) && '同机 IP 节点开着自动续费'].filter(Boolean);
  return why.length ? why.join('、') : null;
}
/** 进备用的自动续费提示（同服务端 spare_notes，预览 done = false、结果 done = true）：一起进备用的里开着自动续费的会在中控关掉；被出口指着（held）时不劝关供应商那边的自动扣款 */
function spareRenewNotes(m: MachineView, xs: MachineView[], held: string | null, done: boolean): string[] {
  const on = [m, ...xs.filter((x) => !x.spare)].filter((x) => x.subscription?.auto_renew);
  if (!on.length) return [];
  const who = on.length === 1 && on[0] === m ? '' : on.map((x) => x.name).join('、');
  const tail = `；${held}，不会自动回收，到期要自己登记续费，供应商那边的自动扣款先别关`;
  if (held) return [done ? `${who ? `${who} 的` : ''}自动续费已在中控关闭${tail}` : `${who ? `${who} 开着自动续费` : '这台开着自动续费'}：进备用时中控这边会关掉${tail}`];
  return [done ? `${who ? `${who} 的` : ''}自动续费已关闭（供应商那边的自动扣款要自己去关）` : `${who ? `${who} 开着自动续费` : '这台开着自动续费'}：进备用时会关掉，到期后照常回收（供应商那边的自动扣款要自己去关）`];
}
/** 不能移入备用的原因（同服务端 spare_eligible），能则 null */
function spareWhy(m: MachineView): string | null {
  if (m.kind === 'relay') return '线路机不属于任何团队';
  if (m.spare) return '已经在备用里';
  if (m.lifecycle !== 'active') return '只有在用的机器能进备用';
  if (!m.subscription) return '没有订阅，没有到期日可等';
  if (m.subscription.due_days <= -2) return '订阅已过期满 1 天，马上就会被清理';
  return null;
}
/** 手动改到备用团队的资格：spareWhy，再加宿主机自己不能有进行中的作业 */
const spareGate = (m: MachineView) => spareWhy(m) ?? (m.active_job ? '有进行中的作业，等它结束再退役' : null);
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
  const plan = teams.flatMap((t) => compactPlan(t, leaving.map((x) => x.id)));
  const renames = plan.map((c) => ({ machine_id: c.m.id, old_name: c.old_name, new_name: c.new_name }));
  const warnings = compactExitsNote(plan, '要换掉这台又不想错位，先用替换（新节点接它的编号，团队里谁都不改名，它自己进备用）');
  const why = spareWhy(m);
  // 进备用时一起移入的：已经在备用里的同机 IP 节点不动、不列（同服务端 move_to_spare 的 ip_nodes_of 过滤）
  const movers = xs.filter((x) => !x.spare);
  const sp = spareTeam(); const nums = freeSpareNumbers(1 + movers.length);
  const ips = xs.map((x) => ({ id: x.id, name: x.name }));
  const spareIps = movers.map((x, i) => ({ id: x.id, name: x.name, to_name: displayName(sp.name, nums[i + 1]) }));
  const held = heldReason(m, movers); // 进备用后不自动回收的原因（出口还指着、同机 IP 节点开着自动续费），没有回收日
  return {
    name: m.name, renames, ip_nodes: ips, routers: exitsUsing(m), warnings,
    blockers: m.kind === 'relay' && m.entries.some((e) => !e.direct) ? [`这台线路机上还有 ${m.entries.filter((e) => !e.direct).length} 个节点入口：先把节点迁走或退役。可先用「搬空线路机」把节点换到其他线路机`] : [],
    spare: { ok: !why, why, team: { id: sp.id, name: sp.name }, to_name: displayName(sp.name, nums[0]), recycle_on: m.subscription && !held ? addDays(m.subscription.due_date, 2) : null, held, ip_nodes: spareIps, warnings: [], notes: spareRenewNotes(m, movers, held, false) },
  };
});
/** 移入备用（同服务端 move_to_spare）：机器照常运行，改到备用团队最小的空号，原团队前移补号 */
function moveToSpare(m: MachineView, reason: string | null, at = iso()) {
  const sp = spareTeam(); const from = m.team!;
  m.spare_from = m.name; m.spare_since = at; spareFromTeam.set(m.id, from.id); m.team = { id: sp.id, name: sp.name }; m.spare = true; m.number = freeSpareNumber([m.id]); m.lifecycle_reason = reason; m.revision++;
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
    const from = m.name; const movers = xs.filter((y) => !y.spare); const xfrom = movers.map((x) => x.name); // 已在备用里的同机 IP 节点不动、不列
    // 出口与自动续费要在进备用前算（进备用后 exitsUsing 不再返回）；一起进备用的同机 IP 节点的自动续费也关掉（同服务端 patch_tx）
    const held = heldReason(m, movers); const notes = spareRenewNotes(m, movers, held, true);
    const autoOff = notes.length > 0;
    for (const x of [m, ...movers]) if (x.subscription) x.subscription.auto_renew = false;
    const at = iso(); // 宿主机和随行 IP 节点同一次进备用，时刻相同
    moveToSpare(m, body.reason ?? null, at);
    for (const x of movers) moveToSpare(x, `随宿主机「${from}」退役`, at);
    refreshCounts();
    db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'machine.retire', title: '退役机器', summary: `${from} 移入备用 → ${m.name}`, target_kind: 'machine', target_id: m.id, detail: { mode: 'spare' } });
    emit({ type: 'machines' }); emit({ type: 'catalog' });
    return { mode: 'spare', name: m.name, from, recycle_on: held ? null : m.recycle_on, held, ip_nodes: movers.map((x, i) => ({ id: x.id, name: x.name, from: xfrom[i] })), auto_renew_off: autoOff, notes };
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
  // 单独设置的带值，null = 跟随：先按套餐 / 供应商的值填上，再由 syncSub 判定哪些算单独设置（和跟随值相同的按跟随，同服务端）
  const s = { auto_renew: false, ...(m.subscription ?? {}), due_date: body.due_date, billing_day: billingDay(String(body.due_date), m.subscription?.billing_day), due_days: 0 } as any;
  if (body.auto_renew !== undefined) s.auto_renew = body.auto_renew;
  m.subscription = s;
  for (const k of SUB_FIELDS) s[k] = body[k] ?? inheritedValue(m, k, s.due_date) ?? (k === 'price_cents' ? 0 : null);
  syncSub(m, db.plans, db.providers);
  recomputeDue(m); Object.assign(m, trafficOf(m, db.machines)); emit({ type: 'machine', id: m.id }); emit({ type: 'machines' }); return clone(m.subscription);
});
R('GET', '/machines/([^/]+)/renewals', ({ params }) => clone(db.renewals[params[0]] ?? []));
R('POST', '/machines/([^/]+)/renewals', ({ params, body }) => {
  const m = machine(params[0]); if (!m.subscription) throw bad('这台机器还没有订阅信息，请先保存订阅');
  // new_due 与 cycles（续几个周期，从当前到期日一次加够）二选一，同服务端
  if ((body.new_due == null) === (body.cycles == null)) throw bad('new_due 与 cycles 要给且只给一个');
  // 按周期推从账单日算（同服务端：账单日 31 号的 1-31 → 2-28 → 3-31）
  const onBday = `${m.subscription.due_date.slice(0, 8)}${String(m.subscription.billing_day).padStart(2, '0')}`;
  const newDue: string = body.new_due ?? addMonths(onBday, cycleMonths(m.subscription.cycle) * Number(body.cycles));
  if (!/^\d{4}-\d{2}-\d{2}$/.test(newDue)) throw bad('新到期日格式应为 YYYY-MM-DD');
  const r = { id: `rn${db.seq++}`, old_due: m.subscription.due_date, new_due: newDue, amount_cents: body.amount_cents ?? m.subscription.price_cents, currency: m.subscription.currency, note: body.note ?? null, actor: db.user, created_at: iso() };
  (db.renewals[m.id] ??= []).unshift(r); m.subscription.billing_day = billingDay(newDue, m.subscription.billing_day); m.subscription.due_date = newDue; recomputeDue(m);
  emit({ type: 'machine', id: m.id }); emit({ type: 'machines' }); return { id: r.id, old_due: r.old_due, new_due: r.new_due };
});
// 待付清单（同服务端 billing.rs）：在用机器里窗口内到期的，按 供应商 × 到期日 分组；不续费的备用机（要 include_spare）、自动续费的不算待付，
// 备用团队里被出口指着 / 有在用 IP 节点 / 开着自动续费的不自动回收、照常续费，机器项带 spare 与 spare_hold
R('GET', '/billing/due', ({ query }) => {
  const days = Number(query.get('days') ?? 30);
  const withSpare = query.get('include_spare') === 'true';
  const rows = db.machines.filter((m) => m.lifecycle === 'active' && (!m.spare || withSpare || !isUnbilledSpare(m)) && m.subscription && m.subscription.due_days <= days)
    .sort((a, b) => a.subscription!.due_date.localeCompare(b.subscription!.due_date) || (a.provider?.name ?? '￿').localeCompare(b.provider?.name ?? '￿'));
  const view = (m: MachineView) => {
    const s = m.subscription!; const u = mockUsage(m); const monthly = mockMonthly(m);
    const hold = m.spare && !isUnbilledSpare(m) ? [m.live_ip_nodes?.length ? `同机 IP 节点 ${m.live_ip_nodes.join('、')} 还在用` : '', m.router_exits?.length ? `软路由出口 ${m.router_exits.join('、')} 还指着` : '', m.renewing_ip_nodes?.length ? `同机 IP 节点 ${m.renewing_ip_nodes.join('、')} 开着自动续费` : '', s.auto_renew ? '开着自动续费' : ''].filter(Boolean).join('；') : null;
    return { id: m.id, name: m.name, kind: m.kind, due_date: s.due_date, due_days: s.due_days, billing_tz: s.billing_tz, spare: m.spare, spare_hold: hold, cycle: s.cycle, price_cents: s.price_cents, currency: s.currency, price_usd_cents: toUsd(s.price_cents, s.currency),
      monthly_usd_cents: monthly, bytes_30d: u.bytes30, usd_per_gb: mockPerGb(monthly, u.bytes30, 30), idle: mockIdle(m, u.sum7), avg7: Math.round(u.sum7 / 7), avail_pct: availability(m, 30).egress_pct };
  };
  const prov = (m: MachineView) => ({ id: m.provider?.id ?? null, name: m.provider?.name ?? null, panel_url: db.providers.find((p) => p.id === m.provider?.id)?.panel_url ?? null });
  const total = (ms: MachineView[]) => usdTotal(ms.map((m) => ({ currency: m.subscription!.currency, monthly_cents: m.subscription!.price_cents })));
  const pay = rows.filter((m) => !m.subscription!.auto_renew);
  const groups: { provider: ReturnType<typeof prov>; due_date: string; tz: string; ms: MachineView[] }[] = [];
  for (const m of pay) {
    const g = groups.find((x) => x.provider.id === (m.provider?.id ?? null) && x.due_date === m.subscription!.due_date && x.tz === m.subscription!.billing_tz);
    if (g) g.ms.push(m); else groups.push({ provider: prov(m), due_date: m.subscription!.due_date, tz: m.subscription!.billing_tz, ms: [m] });
  }
  return { days, until: null, total: total(pay), groups: groups.map((g) => ({ provider: g.provider, due_date: g.due_date, billing_tz: g.tz, machines: g.ms.map(view), total: total(g.ms) })),
    auto: rows.filter((m) => m.subscription!.auto_renew).map((m) => ({ provider: prov(m), machine: view(m) })) };
});
R('POST', '/renewals/batch', ({ body }) => {
  const ms = (body.items ?? []).map((i: any) => ({ m: machine(i.machine_id), amount: i.amount_cents })); // eslint-disable-line @typescript-eslint/no-explicit-any
  if (!ms.length) throw bad('一次登记 1–200 台');
  const cycles = Number(body.cycles ?? 1);
  const note = [body.ref?.trim() ? `账单 ${body.ref.trim()}` : '', body.note?.trim() ?? ''].filter(Boolean).join('，');
  const items = ms.map(({ m, amount }: { m: MachineView; amount?: number }) => {
    const s = m.subscription; if (!s) throw bad(`${m.name}：这台机器还没有账单信息，先填写费用`, 409, 'conflict');
    const onBday = `${s.due_date.slice(0, 8)}${String(s.billing_day).padStart(2, '0')}`;
    const cents = amount ?? s.price_cents * cycles;
    return { m, s, old_due: s.due_date, new_due: addMonths(onBday, cycleMonths(s.cycle) * cycles), cents };
  });
  const out = items.map((i: any) => ({ machine_id: i.m.id, name: i.m.name, old_due: i.old_due, new_due: i.new_due, amount_cents: i.cents, currency: i.s.currency, usd_cents: toUsd(i.cents, i.s.currency) })); // eslint-disable-line @typescript-eslint/no-explicit-any
  const total = usdTotal(items.map((i: any) => ({ currency: i.s.currency, monthly_cents: i.cents }))); // eslint-disable-line @typescript-eslint/no-explicit-any
  if (body.dry_run) return { dry_run: true, items: out, total };
  for (const i of items as any[]) { // eslint-disable-line @typescript-eslint/no-explicit-any
    (db.renewals[i.m.id] ??= []).unshift({ id: `rn${db.seq++}`, old_due: i.old_due, new_due: i.new_due, amount_cents: i.cents, currency: i.s.currency, note, actor: db.user, created_at: iso() });
    i.s.billing_day = billingDay(i.new_due, i.s.billing_day); i.s.due_date = i.new_due; recomputeDue(i.m);
    emit({ type: 'machine', id: i.m.id });
  }
  emit({ type: 'machines' }); return { dry_run: false, items: out, total };
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
// 指向入口的软路由出口：mock 的快照按团队造（同退役预览的 exitsUsing），直出入口没有
R('GET', '/entries/([^/]+)/routers', ({ params }) => {
  const e = findEntry(params[0]);
  return e.direct ? [] : exitsUsing(machine(e.node.id)).map((r) => ({ id: r.id, name: r.name, state: 'online', exits: r.exits.map((x) => x.exit) }));
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
  // 模式（同服务端 add_entry）：没给按默认；直出、家宽代理、同机 IP 节点只能普通
  const want: EntryMode = body.mode ?? ENTRY_MODE.default;
  const why = want !== 'reverse' ? null : direct ? '线路机直出入口没有落地节点，用不着穿透' : m.kind === 'proxy' ? '家宽代理没有 SSH 和 agent，装不了穿透要的 bridge' : m.host_machine ? '同机 IP 节点暂不支持穿透（bridge 要从它自己的 IP 出站，第一版没做）' : null;
  const mode: EntryMode = why ? 'normal' : want;
  const e: EntryView = { id: `e${db.seq++}`, relay: { id: relay.id, name: relay.name }, node: { id: m.id, name: direct ? `${m.name}-直出` : m.name }, direct, mode, port, lifecycle: 'pending', applied: false, apply_error: null, tcp: null, egress: null, reach: null, traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 } };
  if (!direct) m.entries.push(e);
  relay.entries.push(e);
  const job = newJob('attach_entry', { id: m.id, name: m.name }, [['apply', '等待线路机应用'], ['verify', '出口验收']]);
  drive(job, 2000, () => { e.lifecycle = 'active'; e.applied = true; });
  emit({ type: 'machines' }); return { job_id: job.id, entry_mode: mode, note: why ? `按普通模式建入口：${why}` : null };
});

// 穿透开关（同服务端 api/reverse.rs）：预览列出会发生什么与不能切的原因；发起建作业（推 bridge → 切换 → 验收），完成后改模式、TCP 一格变成「-（穿透）」
const ENTRY_MODE: { default: EntryMode; set: boolean } = { default: 'reverse', set: false };
const entryModeView = () => ({ default: ENTRY_MODE.default, label: ENTRY_MODE.default === 'reverse' ? '穿透' : '普通', set: ENTRY_MODE.set, builtin: 'reverse' });
R('GET', '/settings/entry-mode', () => entryModeView());
R('PUT', '/settings/entry-mode', ({ body }) => {
  if (body.default !== 'normal' && body.default !== 'reverse') throw bad(`入口模式只能是 normal（普通）或 reverse（穿透），给的是「${body.default}」`);
  Object.assign(ENTRY_MODE, { default: body.default, set: true }); emit({ type: 'settings' }); return entryModeView();
});
function reversePlan(id: string, on: boolean) {
  const e = findEntry(id); const node = machine(e.node.id); const relay = machine(e.relay.id);
  const blockers: string[] = [];
  if (e.direct) blockers.push('线路机直出入口没有落地节点，用不着穿透');
  if (e.lifecycle !== 'active') blockers.push('入口现在不在用：只有在用的入口能切换（停用的先启用）');
  if ((e.mode === 'reverse') === on) blockers.push(`入口已经是${on ? '穿透' : '普通'}模式`);
  if (on && node.kind === 'proxy') blockers.push('家宽代理没有 SSH 和 agent，装不了穿透要的 bridge');
  if (on && node.host_machine) blockers.push('同机 IP 节点暂不支持穿透（bridge 要从它自己的 IP 出站，第一版没做）');
  if (node.active_job) blockers.push(`「${node.name}」还有进行中的作业：等它结束再切换`);
  const impacts = on
    ? [`节点「${node.name}」上装 bridge（推 xray 程序、装 fleet-xray-bridge 单元），由它主动连 ${relay.ip}:${e.port}（这个入口自己的端口），客户流量经这条隧道从节点自己的 IP 出去`]
    : [`线路机 ${relay.name} 改回连节点的 ssserver（节点上一直留着，防火墙照旧放行这台线路机），节点入站丢包的问题会回来`, '验收通过后节点上的 bridge 停掉删掉（xray 程序留着）'];
  impacts.push(`线路机 ${relay.name} 不重启 Xray：只换这个入口的入站，这个入口上正在进行的连接断开一次、客户端自动重连；线路机上别的入口不受影响`, '分享链接、客户端订阅、软路由出口都不变（同一个入口端口与凭据）', '验收：线路机应用后经入口的出口探测要看到节点自己的 IP；不通就自动切回原来的模式、作业失败');
  const warnings = on ? ['之后这个入口没有线路机 → 节点的 TCP 探测（显示「-（穿透）」）；隧道断了体现为出口探测失败（照常告警），隧道挂不上时中控会叫节点 agent 重启 bridge'] : [];
  const mode = e.mode ?? 'normal';
  return { e, node, view: { entry: { id: e.id, name: `${node.name} · ${relay.name}:${e.port}`, node_id: node.id, node: node.name, relay_id: relay.id, relay: relay.name, port: e.port, mode }, on, from: mode, to: on ? 'reverse' : 'normal', ok: !blockers.length, blockers, impacts, warnings } };
}
R('GET', '/entries/([^/]+)/reverse', ({ params, query }) => reversePlan(params[0], query.get('on') === 'true').view);
R('POST', '/entries/([^/]+)/reverse', ({ params, body }) => {
  const on = body.on === true;
  const { e, node, view } = reversePlan(params[0], on);
  if (!view.ok) throw bad(view.blockers.join('；'), 409, 'conflict');
  const job = newJob('reverse', { id: node.id, name: node.name }, [['bridge_up', '推 bridge 并启动'], ['switch', '切换入口模式并验收出口'], ['bridge_down', '收起 bridge']]);
  node.active_job = { id: job.id, kind: 'reverse', state: 'running' };
  drive(job, 2000, () => { e.mode = on ? 'reverse' : 'normal'; if (on) e.tcp = null; else e.tcp = { ok: true, latency_ms: 48, detail: null, ts: iso() }; node.active_job = null; });
  emit({ type: 'machines' }); return { ...view, job_id: job.id };
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
  const tid = query.get('target_id');
  return paged(db.jobs.filter((j) => kindOk(j.kind) && (!tid || j.target?.id === tid) && (s === 'all' || (s === 'active' ? j.state === 'running' || j.state === 'queued' : j.state !== 'running' && j.state !== 'queued'))).map(jobSummary), query);
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
/** 清理作业记录：已成功、已取消的（dry_run 只数） */
R('POST', '/jobs/prune', ({ body }) => {
  const b = (body ?? {}) as { dry_run?: boolean };
  const gone = db.jobs.filter((j) => j.state === 'succeeded' || j.state === 'cancelled');
  if (b.dry_run) return { jobs: gone.length, dry_run: true };
  db.jobs = db.jobs.filter((j) => !gone.includes(j));
  for (const j of gone) delete db.logs[j.id];
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'job.prune', title: '清理作业记录', summary: `${gone.length} 个`, target_kind: 'job', target_id: null, detail: { jobs: gone.length } });
  emit({ type: 'resync' });
  return { jobs: gone.length };
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
// 探测点（设置「探测点」）：演示数据只有一个在线的香港探测点、主来源是中控；改主来源、开关、新增只改内存
const MOCK_PROBES: Record<string, unknown>[] = [{
  id: 'pp-hk', name: '香港', label: '香港探测点', enabled: true, note: '阿里云香港 · Alpine', state: 'online', installed: true, primary: false, rank: null,
  last_seen_at: iso(), last_kept_at: iso(), discard_reason: null, last_ip: '47.242.0.10', version: '0.1.0+mock', created_at: iso(), bootstrap_expires_at: null, install: null, upgrade: null, upgrade_failed: null,
  relays: [{ relay_id: 'r1', name: '线路机1', avg_ms: 158, median_ms: 156, total: 120, ok: 120 }],
}];
const reachSrc: { order: string[]; central_enabled: boolean } = { order: [], central_enabled: true };
const reachView = () => {
  const order = reachSrc.order.map((id) => MOCK_PROBES.find((x) => x.id === id)).filter((p): p is Record<string, unknown> => !!p).map((p) => ({ id: p.id as string, name: p.name as string, label: p.label as string }));
  const p = order[0];
  const effective = p ? { state: 'probe', probe_point_id: p.id, label: p.label, primary_label: p.label, primary_issue: null, substitute: null } : { state: 'central', probe_point_id: null, label: '中控', primary_label: null, primary_issue: null, substitute: null };
  return { order, primary_name: p ? p.name : '中控', central_enabled: reachSrc.central_enabled, effective, offline_after_s: 180 };
};
R('GET', '/settings/reach-source', () => reachView());
R('PUT', '/settings/reach-source', ({ body }) => {
  const b = body as { order?: string[]; central_enabled?: boolean };
  if (b.order) reachSrc.order = b.order;
  if (b.central_enabled != null) reachSrc.central_enabled = b.central_enabled;
  MOCK_PROBES.forEach((p) => { const i = reachSrc.order.indexOf(p.id as string); p.primary = i === 0; p.rank = i < 0 ? null : i + 1; });
  emit({ type: 'settings' }); return reachView();
});
// 到各线路机的统计窗口（同服务端 ?window=）：每 30 秒一次，1 小时 120 次；长窗口偶尔断几次
const WIN_HOURS: Record<string, number> = { '1h': 1, '24h': 24, '7d': 168, '30d': 720 };
R('GET', '/probe-points', ({ query }) => {
  const window = query.get('window') ?? '1h'; const h = WIN_HOURS[window] ?? 1;
  return clone(MOCK_PROBES).map((p) => ({ ...p, window, maintenance: inMaint(p.id as string), relays: (p.relays as { total: number; ok: number }[]).map((r) => ({ ...r, total: r.total * h, ok: r.ok * h - Math.floor(h / 24) * 3 })) }));
});
// 线路机抽屉的客户入口连通性（同服务端 /reach-quality）：中控只判通不通，香港探测点带建连延迟
R('GET', '/machines/([^/]+)/reach-quality', ({ params }) => {
  const m = machine(params[0]);
  if (m.kind !== 'relay') throw bad('只有线路机有客户入口连通性');
  const win = (h: number, ms: number | null) => ({ total: 120 * h, ok: 120 * h - Math.floor(h / 24) * 2, avg_ms: ms, median_ms: ms == null ? null : ms - 2 });
  const w = (ms: number | null) => ({ '24h': win(24, ms), '7d': win(168, ms), '30d': win(720, ms) });
  return { sources: [{ source: 'central', label: '中控', primary: !reachSrc.order.length, windows: w(null) }, { source: 'pp-hk', label: '香港探测点', primary: reachSrc.order[0] === 'pp-hk', windows: w(158) }] };
});
R('POST', '/probe-points', ({ body }) => {
  const b = body as { name: string; note?: string };
  const id = `pp-${MOCK_PROBES.length + 1}`;
  MOCK_PROBES.push({ id, name: b.name, label: `${b.name}探测点`, enabled: true, note: b.note ?? '', state: 'pending', installed: false, primary: false, rank: null, last_seen_at: null, last_ip: null, version: null, created_at: iso(), bootstrap_expires_at: iso(), install: null, upgrade: null, upgrade_failed: null, relays: [] });
  emit({ type: 'settings' });
  return { id, bootstrap: { cmd: `{ curl -fsS https://fleet.example/api/v1/probe-bootstrap/MOCK 2>/dev/null || wget -qO- https://fleet.example/api/v1/probe-bootstrap/MOCK; } | sh`, expires_at: iso() } };
});
// 三网 PING：全局 8 槽位；演示的每台线路机三个目标，延迟在 150–200 ms 上下，偶尔丢一个
const PING_GLOBAL = { mode: 'tcp', slots: [['武汉电信', 'hb-ct-v4.ip.zstaticcdn.com'], ['武汉联通', 'hb-cu-v4.ip.zstaticcdn.com'], ['武汉移动', 'hb-cm-v4.ip.zstaticcdn.com'], ['BGP', ''], ['Node 1', ''], ['Node 2', ''], ['Node 3', ''], ['Node 4', '']].map(([name, target]) => ({ name, target })) };
const pingView = () => ({ ...clone(PING_GLOBAL), modes: ['tcp', 'icmp'], interval_s: 60, count: 5, tcp_default_port: 80, defaults: clone(PING_GLOBAL) });
R('GET', '/settings/ping-targets', () => pingView());
R('PUT', '/settings/ping-targets', ({ body }) => { Object.assign(PING_GLOBAL, body as object); emit({ type: 'settings' }); return pingView(); });
type PingSlotV = { name: string; target: string };
type PingOverrideV = { mode?: 'tcp' | 'icmp' | null; slots?: (PingSlotV | null)[] };
const PING_OVERRIDE = new Map<string, PingOverrideV>();
/** 这台线路机此刻合成的槽位（覆盖优先，同服务端） */
const pingSlotsOf = (id: string) => {
  const o = PING_OVERRIDE.get(id);
  return PING_GLOBAL.slots.map((g, slot) => { const s = o?.slots?.[slot]; return s ? { ...s, slot, source: 'override' as const } : { ...g, slot, source: 'global' as const }; });
};
const pingModeOf = (id: string) => PING_OVERRIDE.get(id)?.mode ?? PING_GLOBAL.mode;
const relayIds = () => db.machines.filter((m) => m.kind === 'relay' && m.lifecycle === 'active').map((m) => m.id);
const pingRows = (id: string) => {
  const k = Math.max(0, relayIds().indexOf(id));
  return pingSlotsOf(id).filter((s) => s.target).map((s) => {
    const base = 150 + 15 * s.slot + 10 * k;
    const cells = Array.from({ length: 20 }, (_, i) => [base + ((i * 7 + s.slot * 3) % 23), (i + s.slot) % 9 === 0 ? 20 : 0]);
    return { slot: s.slot, name: s.name, target: s.target, mode: pingModeOf(id), cur_ms: cells[19][0], cur_loss: cells[19][1], cur_ts: iso(), cells };
  });
};
const relayOf = (id: string) => { const m = machine(id); if (m.kind !== 'relay') throw bad('不是线路机', 404, 'not_found'); return m; };
const machinePingView = (id: string) => ({ mode: pingModeOf(id), slots: pingSlotsOf(id), override: clone(PING_OVERRIDE.get(id) ?? null), global: clone(PING_GLOBAL), current: pingRows(id) });
R('GET', '/ping-beats', () => Object.fromEntries(relayIds().map((id) => [id, pingRows(id)]).filter(([, rows]) => (rows as unknown[]).length)));
R('GET', '/machines/([^/]+)/ping', ({ params }) => { relayOf(params[0]); return machinePingView(params[0]); });
R('PUT', '/machines/([^/]+)/ping', ({ params, body }) => {
  relayOf(params[0]);
  const o = (body as { override: PingOverrideV | null }).override;
  if (!o || (o.mode == null && (o.slots ?? []).every((s) => s == null))) PING_OVERRIDE.delete(params[0]); else PING_OVERRIDE.set(params[0], clone(o));
  emit({ type: 'machine', id: params[0] }); return machinePingView(params[0]);
});
R('GET', '/machines/([^/]+)/ping-series', ({ params, query }) => {
  relayOf(params[0]);
  const range = query.get('range') ?? '24h';
  const span: Record<string, number> = { '1h': 3600, '6h': 6 * 3600, '24h': 86400, '7d': 7 * 86400, '30d': 30 * 86400 };
  if (!span[range]) throw bad('range 不对');
  const step = span[range] > 86400 ? 3600 : 60;
  const n = Math.min(span[range] / step, 720); const now = Date.now();
  return { range, step_s: step, series: pingRows(params[0]).map((r) => ({ slot: r.slot, name: r.name, target: r.target, points: Array.from({ length: n }, (_, i) => {
    const ts = new Date(now - (n - 1 - i) * step * 1000).toISOString();
    const c = r.cells[i % 20];
    return { ts, rtt_ms: c[0], loss_pct: c[1] };
  }) })) };
});
/** 告警的顺序同服务端：进行中在前 → 严重度高在前 → 开始时间新在前；subject_id（只要这台机器的）时不看严重度，只按时间 */
const SEV_RANK: Record<string, number> = { critical: 0, warning: 1, info: 2 };
const byState = (a: Incident, b: Incident) => (a.state === b.state ? 0 : a.state === 'firing' ? -1 : 1);
const bySev = (a: Incident, b: Incident) => (SEV_RANK[a.severity] ?? 1) - (SEV_RANK[b.severity] ?? 1);
const byOpened = (a: Incident, b: Incident) => b.opened_at.localeCompare(a.opened_at);
/** 建议命令（同服务端 api/incidents.rs 的 suggest，只做这几条）：只有进行中的才有 */
const shq = (s: string) => (/^[\p{L}\p{N}\-_.:/@+=,]+$/u.test(s) ? s : `'${s}'`);
function suggest(i: Incident): string[] {
  const name = i.subject?.name ?? i.subject_name;
  if (i.state !== 'firing' || !name) return [];
  const kind = db.machines.find((m) => m.id === i.subject_id)?.kind;
  if (i.rule === 'traffic_quota_high' && kind === 'relay') return [`fleetctl rebalance ${shq(name)}`];
  if (i.rule === 'agent_offline') return [`fleetctl logs ${shq(name)}`];
  if (i.rule === 'tcp_loss_high') return [`fleetctl diag ${shq(name)}`];
  if (i.rule === 'nonbusiness_traffic_high') return [`fleetctl logs ${shq(name)}`, `fleetctl exec ${shq(name)} -- 'ss -s; nft list ruleset | head -50'`];
  if (i.rule === 'business_traffic_spike') return [`fleetctl traffic --node ${shq(name)} --days 7`];
  if (i.rule === 'router_exit_drift') return [`fleetctl router sync ${shq(name)}`];
  if (i.chronic && kind === 'vps' && (i.rule === 'tcp_fail' || i.rule === 'egress_fail')) return [`fleetctl placement --node ${shq(name)}`];
  return [];
}
R('GET', '/incidents', ({ query }) => {
  const s = query.get('state') ?? 'firing'; const sid = query.get('subject_id');
  const order = sid ? (a: Incident, b: Incident) => byState(a, b) || byOpened(a, b) : (a: Incident, b: Incident) => byState(a, b) || bySev(a, b) || byOpened(a, b);
  const list = db.incidents.filter((i) => (s === 'all' || i.state === s) && (!sid || i.subject_id === sid)).sort(order).map((i) => ({ ...i, suggested: i.suggested ?? suggest(i) }));
  return { ...paged(list, query), acked: list.filter((i) => i.ack_at).length };
});
R('POST', '/incidents/([^/]+)/ack', ({ params }) => { const i = db.incidents.find((x) => x.id === params[0]); if (!i) throw bad('告警不存在', 404, 'not_found'); i.ack_at = iso(); i.ack_by = db.user; emit({ type: 'incidents' }); return {}; });
/** 静默同服务端：只列没结束的（含还没开始的预约），按开始时间排；新建的校验顺序与中文原因同服务端 */
const RULE_NAMES = META.rules;
/** 正在生效、标记为维护的静默的对象 id（null = 全部）。软路由、探测点按这份算 maintenance（同服务端 silence.rs，只按 id 匹配） */
const maintIds = () => { const now = Date.now(); return new Set(db.silences.filter((s) => s.is_maintenance && Date.parse(s.starts_at) <= now && Date.parse(s.ends_at) > now).map((s) => s.subject_id)); };
const inMaint = (id: string) => { const ids = maintIds(); return ids.has(null) || ids.has(id); };
/** 维护中同服务端：正在生效、标记为维护的静默落在这台、它的宿主机或「全部机器」上；维护中的健康不给原因，结束后回到维护前的健康 */
const preMaint = new Map<string, MachineView['health']>();
function syncMaint() {
  const ids = maintIds();
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
  if (body.subject_kind && !body.subject_id) throw bad('给了 subject_kind 就要给 subject_id；全部对象两个都不给');
  const kind: SilenceKind | null = body.subject_id ? body.subject_kind ?? 'machine' : null;
  if (kind && !SILENCE_KINDS[kind]) throw bad('subject_kind 只能是 machine / router / probe_point');
  if (kind && body.rule && ruleKind(body.rule) !== kind) throw bad(`「${RULE_NAMES[body.rule]}」不是${SILENCE_KINDS[kind]}的告警规则`);
  const name = kind === 'router' ? routersMock.routers().find((x) => x.id === body.subject_id)?.name
    : kind === 'probe_point' ? MOCK_PROBES.find((x) => x.id === body.subject_id)?.name as string | undefined
    : kind ? db.machines.find((x) => x.id === body.subject_id)?.name : null;
  if (kind && !name) throw bad(`subject_id 指向的${SILENCE_KINDS[kind]}不存在`);
  const id = `s${db.seq++}`;
  db.silences.push({ id, subject_kind: kind, subject_id: kind ? body.subject_id : null, subject_name: name ?? null, rule: body.rule ?? null, rule_name: body.rule ? RULE_NAMES[body.rule] : null, starts_at: body.starts_at, ends_at: body.ends_at, is_maintenance: !!body.is_maintenance, comment: body.comment ?? '' });
  silenceChanged(kind); return { id };
});
R('DELETE', '/silences/([^/]+)', ({ params }) => {
  const s = db.silences.find((x) => x.id === params[0]);
  if (!s) throw bad('没有这条静默', 404, 'not_found');
  db.silences = db.silences.filter((x) => x.id !== params[0]); silenceChanged(s.subject_kind); return { ok: true };
});
/** 同服务端 api/incidents.rs：对象类型的叫法、规则属于哪类对象（中控的只能用在全部上） */
const SILENCE_KINDS: Record<SilenceKind, string> = { machine: '机器', router: '软路由', probe_point: '探测点' };
const ruleKind = (r: string): SilenceKind | null => (r.startsWith('router_') ? 'router' : r.startsWith('probe_point_') ? 'probe_point' : r.startsWith('central_') ? null : 'machine');
/** 静默变了：机器的维护中跟上；软路由的（或全部的）发 routers 事件，路由卡片跟上（探测点列表自己 30 秒重取） */
function silenceChanged(kind: SilenceKind | null) {
  syncMaint(); emit({ type: 'incidents' });
  if (kind == null || kind === 'router') emit({ type: 'routers' });
}
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
installTrafficMock(R, () => db.machines, () => db.teams, bad);
installReportsMock(R, () => db.machines, () => db.incidents, () => db.channels, () => db.notifyLog, () => `n${db.seq++}`, bad);
// 汇率变了：各台订阅的 USD 折算重算，发 settings（同中控）
installFxMock(R, () => [...new Set([...db.plans.filter((p) => !p.archived_at).map((p) => p.currency), ...db.machines.filter((m) => m.lifecycle !== 'retired' && m.subscription).map((m) => m.subscription!.currency)])],
  () => db.user, () => { db.machines.forEach(recomputeDue); emit({ type: 'settings' }); }, bad);
installPolicyMock(R, { teams: () => db.teams, machines: () => db.machines, emit, user: () => db.user, bad });
const routersMock = installRoutersMock(R, { teams: () => db.teams, machines: () => db.machines, incidents: () => db.incidents, maintenance: inMaint, emit, bad });
installTerminalMock(R, { machines: () => db.machines, routers: routersMock.routers, bad });
installClientSubsMock(R, { teams: () => db.teams, machines: () => db.machines, bad });
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
  const tp = m.kind === 'relay'
    ? { bw: 1000, bw_plan: 2560, bw_source: 'plan' as const, rtt: 172, rtt_source: 'sampled' as const, rtt_samples: 41, mem_mb: 3911, buf_max: 45097152 }
    : { bw: 1000, bw_plan: null, bw_source: 'default' as const, rtt: 100, rtt_source: 'default' as const, rtt_samples: 0, mem_mb: 3911, buf_max: 27097152 };
  drive(job, 2500, () => { m.active_job = null; m.tune = { profile: 'tune-v2', at: iso(), current: true, params: tp }; }, [{
    profile: 'tune-v2', applied: ['net.core.rmem_max', 'net.core.wmem_max', 'net.ipv4.tcp_rmem', 'net.ipv4.tcp_wmem'], ok: ['net.ipv4.tcp_congestion_control'], kept: ['net.core.somaxconn', 'fs.file-max'],
    before: { 'net.core.rmem_max': '212992', 'net.core.wmem_max': '212992', 'net.ipv4.tcp_rmem': '4096 131072 6291456', 'net.ipv4.tcp_wmem': '4096 16384 4194304' },
    skipped: [{ key: 'net.netfilter.nf_conntrack_max', why: 'missing', got: null }], params: tp, snapshot: 'created', warnings: [],
    route: m.kind === 'relay' ? { initcwnd: 10000, initrwnd: 10000 } : null,
    journald: 'changed', ntp: 'synced', nofile: '65536', container: false, mem_mb: 3911, swap_mb: 0, bbr: true,
  }]);
  emit({ type: 'machines' });
  return { machine_id: m.id, name: m.name, job_id: job.id };
});
R('POST', '/machines/([^/]+)/ss-refresh', ({ params }) => {
  const m = machine(params[0]);
  if (m.host_machine) throw bad(`同机 IP 节点和宿主机共用 ssserver 的黑名单文件：对宿主机「${m.host_machine.name}」下发，同机的实例一起刷`, 409, 'conflict');
  if (m.kind === 'relay') throw bad('线路机上没有落地节点的 ssserver', 409, 'conflict');
  if (m.kind === 'proxy') throw bad('家宽代理没有我们的 ssserver', 409, 'conflict');
  if (m.lifecycle !== 'active') throw bad('只有在用的节点能下发', 409, 'conflict');
  if (m.active_job) throw bad('这台机器有进行中的作业，请等它结束', 409, 'conflict');
  const job = newJob('ss_refresh', { id: m.id, name: m.name }, [['ss', '下发 SS 配置'], ['verify', '出口验收']]);
  job.cancellable = false;
  m.active_job = { id: job.id, kind: 'ss_refresh', state: 'running' };
  drive(job, 2500, () => { m.active_job = null; }, [{ instances: [{ port: 8388, ip: null, restarted: true, nft_relays: '203.0.113.7' }] }, { entries: [{ latency_ms: 142, egress_ip: '198.51.100.20' }] }]);
  emit({ type: 'machines' });
  return { machine_id: m.id, name: m.name, job_id: job.id };
});
// ---------- 按需测速（同服务端 jobs/speedtest.rs）----------
R('POST', '/machines/([^/]+)/speedtest', ({ params, body }) => {
  const m = machine(params[0]);
  const sp = META.speedtest;
  const size = body?.size_mb ?? sp.default_mb;
  if (!sp.sizes_mb.includes(size)) throw bad(`size_mb 只能是 ${sp.sizes_mb.join(' / ')}`);
  const target = sp.targets.find((t) => t.key === (body?.target ?? sp.targets[0].key));
  if (!target) throw bad(`target 只能是 ${sp.targets.map((t) => t.key).join(' / ')}`);
  if (m.host_machine) throw bad(`同机 IP 节点和宿主机共用网络：对宿主机「${m.host_machine.name}」测速`, 409, 'conflict');
  if (m.kind === 'proxy') throw bad('家宽代理没有我们的机器', 409, 'conflict');
  if (m.lifecycle !== 'active') throw bad('只有在用的机器能测速', 409, 'conflict');
  if (m.active_job) throw bad('这台机器有进行中的作业，请等它结束', 409, 'conflict');
  const job = newJob('speedtest', { id: m.id, name: m.name }, [['speedtest', '测速']]);
  job.cancellable = false;
  m.active_job = { id: job.id, kind: 'speedtest', state: 'running' };
  // 速率按 (机器, 目标) 定：线路机快一些；编号尾数为 4 的那台慢线路，500 MB 在 300 秒内下不完（演示「没下完」）
  const r = rng(hashStr(`${m.id}:${target.key}`));
  const slow = m.number != null && m.number % 10 === 4;
  const mbps = Math.round((slow ? 5 + r() * 5 : m.kind === 'relay' ? 600 + r() * 300 : 120 + r() * 380) * 10) / 10;
  const ttfb = Math.round((0.18 + r() * 0.35) * 1000) / 1000;
  const full = size * 8 / mbps; // 下完要多少秒
  const complete = full + ttfb <= 300;
  const total = complete ? Math.round((full + ttfb) * 100) / 100 : 300;
  const bytes = complete ? size * 1_000_000 : Math.round((300 - ttfb) * mbps / 8 * 1_000_000);
  const out = { target: target.key, target_name: target.name, url: `https://${target.key}-us-ping.vultr.com/vultr.com.${size <= 100 ? '100MB' : '1000MB'}.bin`, size_mb: size, bytes, complete,
    avg_mbps: mbps, avg_mb_s: Math.round(mbps / 8 * 100) / 100, total_s: total, ttfb_s: ttfb, remote_ip: `203.0.113.${10 + Math.floor(r() * 200)}` };
  log(job.id, 'info', `从${target.name}（${out.url}）下载 ${size} MB，走这台机器自己的出站路径，会用掉约 ${size} MB 流量`);
  if (!complete) log(job.id, 'warn', `300 秒内只下载了 ${(bytes / 1e6).toFixed(1)} MB，没下完：速率按已下载的算`);
  log(job.id, 'info', `平均速率 ${mbps} Mbit/s（${out.avg_mb_s} MB/s），耗时 ${total} 秒，首字节 ${Math.round(ttfb * 1000)} 毫秒，下载 ${(bytes / 1e6).toFixed(1)} MB，对端 ${out.remote_ip}`);
  drive(job, 4000, () => { m.active_job = null; }, [out]);
  emit({ type: 'machines' });
  return { machine_id: m.id, name: m.name, job_id: job.id, size_mb: size, target: target.key, target_name: target.name, traffic_mb: size };
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
  return { from: src.name, to: dst.name, jobs, skipped, warnings: [], router_restarts: [], offline_routers: [], share_changed: jobs.length > 0, note: SHARE_NOTE };
});
// 批量换线路机（同后端 moves_view）：ids 里每个节点各算一次计划；有一个不能换就 ok 为 false、原因进 blockers
function movesPlan(ids: string[], toId: string, fromId?: string, routersOn = true) {
  const dst = machine(toId);
  const plans: { node: MachineView; plan: { old: EntryView; to: MachineView }; routers: import('@/lib/types').RouterExitPlan[] }[] = []; const skipped: { machine_id: string; name: string | null; reason: string }[] = [];
  const blockers: string[] = []; const restarts = new Map<string, { id: string; name: string; times: number; nodes: number }>();
  const offline = new Map<string, import('@/lib/types').OfflineRouter>();
  for (const id of ids) {
    const node = machine(id);
    const plan = planMove(node, dst.id, fromId);
    if (typeof plan === 'string') { skipped.push({ machine_id: node.id, name: node.name, reason: plan }); blockers.push(plan.includes(node.name) ? plan : `${node.name}：${plan}`); continue; }
    // 指向旧入口的软路由出口（同后端 routers_view：换线路机的新端口建好才知道，new_entry_id 为 null、to 写「server:新端口」）；开着改软路由时不在线的路由挡住
    const hits: import('@/lib/types').OfflineRouter[] = [];
    const routers = routersMock.routers().flatMap((r) => {
      const exits = (r.exit_entries ?? []).filter((x) => x.entry_id === plan.old.id).map((x) => ({ exit: x.exit, old_entry_id: plan.old.id, new_entry_id: null, from: `${machine(plan.old.relay.id).ip}:${plan.old.port}`, to: `${dst.ip}:新端口` }));
      if (!exits.length) return [];
      if (routersOn && r.state !== 'online') hits.push({ id: r.id, name: r.name, state: r.state, error: r.error, exits: exits.map((e) => ({ node: node.name, exit: e.exit })) });
      return [{ id: r.id, name: r.name, state: r.state, exits }];
    });
    // 指向旧入口的路由不在线：这个节点跳过（不计入 router_restarts），路由按 id 合并进 offline_routers
    if (hits.length) {
      skipped.push({ machine_id: node.id, name: node.name, reason: hits.map((h) => `软路由「${h.name}」现在${h.state === 'offline' ? '离线' : '还没接入'}。它的出口 ${h.exits.map((e) => e.exit).join('、')} 指向这个入口，要等它在线再换线路机（或选不改软路由）`).join('；') });
      for (const h of hits) { const o = offline.get(h.id); if (o) o.exits.push(...h.exits); else offline.set(h.id, h); }
      continue;
    }
    if (routersOn) for (const r of routers) { const t = restarts.get(r.id) ?? { id: r.id, name: r.name, times: 1, nodes: 0 }; t.nodes++; restarts.set(r.id, t); }
    plans.push({ node, plan, routers });
  }
  return { dst, plans, skipped, blockers: [...new Set(blockers)], offline: [...offline.values()], routerRestarts: [...restarts.values()].sort((a, b) => b.nodes - a.nodes || a.name.localeCompare(b.name)) };
}
R('GET', '/moves', ({ query }) => {
  const { dst, plans, skipped, blockers, offline, routerRestarts } = movesPlan((query.get('machine_ids') ?? '').split(',').filter(Boolean), query.get('to_relay_id') ?? '', query.get('from_relay_id') ?? undefined, query.get('routers') !== 'false');
  const jobs = plans.map(({ node, plan, routers }) => ({ machine_id: node.id, name: node.name, from: plan.old.relay.name, old_entry: { id: plan.old.id, relay: plan.old.relay.name, port: plan.old.port }, routers }));
  return { from: jobs.length ? [...new Set(plans.map((x) => x.plan.old.relay.name))].join('、') : '', to: dst.name, jobs, skipped, warnings: [], router_restarts: routerRestarts, offline_routers: offline, share_changed: jobs.length > 0, note: SHARE_NOTE, ok: skipped.length === 0, blockers };
});
// 搬迁建议（mock 的简化算法：按本月流量从大到小，凑够需要少走的日均）
R('GET', '/relays/([^/]+)/rebalance', ({ params, query }) => {
  const src = machine(params[0]); const q = src.quota; const limit = (q.limit_gb ?? 0) * 1e9;
  const skip = new Set((query.get('exclude') ?? '').split(',').filter(Boolean));
  const room = (x: MachineView) => (x.quota.limit_gb ?? 0) * 1e9 - x.quota.used;
  const target = db.machines.filter((x) => x.kind === 'relay' && x.lifecycle === 'active' && x.id !== src.id && x.health.status !== 'down').sort((a, b) => room(b) - room(a))[0] ?? null;
  const days = Math.max(1, (Date.parse(q.period_end ?? '') - Date.now()) / 864e5);
  // 已超额度：目标改为搬走后日均降到原来的 20% 以下（同服务端 OVER_LIMIT_KEEP_PCT）
  const over = q.used >= limit;
  const daily = q.daily_avg ?? 0; const allowed = over ? daily * 0.2 : (limit - q.used) / days;
  const needed = daily > 0 && q.used + daily * days > limit;
  const es = src.entries.filter((e) => e.lifecycle === 'active' && !e.direct && !skip.has(e.node.id) && !target?.entries.some((t) => t.node.id === e.node.id))
    .map((e) => ({ e, month: e.traffic.month_up + e.traffic.month_down })).sort((a, b) => b.month - a.month);
  const total = es.reduce((s, x) => s + x.month, 0) || 1;
  const moves: { node: { id: string; name: string }; entry_id: string; port: number; period_bytes: number; daily_bytes: number; needs_from: boolean }[] = []; let cut = 0;
  for (const x of es) {
    if (!needed || daily - cut <= allowed) break;
    const d = Math.round((daily * x.month) / total); cut += d;
    moves.push({ node: x.e.node, entry_id: x.e.id, port: x.e.port, period_bytes: x.month, daily_bytes: d, needs_from: false });
  }
  const pct = (v: number) => Math.round((v * 1000) / limit) / 10;
  return {
    relay: { id: src.id, name: src.name }, limit_gb: q.limit_gb, used: q.used, period_end: q.period_end, daily_avg: q.daily_avg, forecast_full_at: q.forecast_full_at, projected_pct: daily > 0 ? pct(q.used + daily * days) : null,
    needed, over_limit: over, ok: needed && moves.length > 0 && daily - cut <= allowed, reason: needed ? null : '按近 7 天的速度，重置前用不满，不用搬',
    target: target && { id: target.id, name: target.name, limit_gb: target.quota.limit_gb, used: target.quota.used, daily_avg: target.quota.daily_avg, projected_pct: target.quota.limit_gb ? pct2(target, 0) : null },
    moves, after: { daily_avg: daily - cut, projected_pct: pct(q.used + (daily - cut) * days) }, target_after: { fits: true, projected_pct: target?.quota.limit_gb ? pct2(target, cut) : null },
  };
});
// 放置矩阵（mock：往返按 id 算出固定的值，当前线路机偏慢一点让几个节点有建议；不看额度）
R('GET', '/placement', () => {
  const relays = db.machines.filter((x) => x.kind === 'relay' && x.lifecycle === 'active');
  const nodes = db.machines.filter((x) => x.kind === 'vps' && x.lifecycle === 'active' && !x.spare);
  const h = (s: string) => [...s].reduce((a, c) => (a * 31 + c.charCodeAt(0)) >>> 0, 7);
  const name = (id: string) => relays.find((r) => r.id === id)?.name ?? '';
  const groups = new Map<string, { id: string; name: string; from: { id: string; name: string }; port: number; reason: string }[]>();
  const out = nodes.map((n) => {
    const live = n.entries.filter((e) => e.lifecycle === 'active' && !e.direct);
    const cur = live.length === 1 ? live[0] : null;
    const cells = relays.map((r) => {
      const v = h(n.id + r.id);
      const ok = v % 9 === 0 ? 280 : 288;
      return { relay_id: r.id, samples: 288, ok, success_pct: Math.round((ok * 1000) / 288) / 10, p50_ms: 120 + (v % 90) + (cur?.relay.id === r.id && h(n.id) % 3 === 0 ? 25 : 0) };
    });
    const good = cells.filter((c) => (c.success_pct ?? 0) >= 99 && relays.find((r) => r.id === c.relay_id)?.health.status !== 'down').sort((a, b) => a.p50_ms - b.p50_ms);
    const best = good[0]?.relay_id ?? null;
    const cc = cells.find((c) => c.relay_id === cur?.relay.id);
    const curGood = !!cc && (cc.success_pct ?? 0) >= 99;
    const suggest = !!cur && !!best && best !== cur.relay.id && (!curGood || cc!.p50_ms - good[0].p50_ms >= 10);
    const reason = suggest ? (curGood ? `可快 ${cc!.p50_ms - good[0].p50_ms} ms` : `当前成功率 ${cc?.success_pct}%`) : null;
    if (suggest && cur && best) groups.set(best, [...(groups.get(best) ?? []), { id: n.id, name: n.name, from: { id: cur.relay.id, name: cur.relay.name }, port: cur.port, reason: reason! }]);
    return { id: n.id, name: n.name, team: n.team, ip: n.ip, current_relay_id: cur?.relay.id ?? null, entry_relay_ids: live.map((e) => e.relay.id), entries: live.length, cells, tcp_fail_7d: h(n.id) % 7 === 0 ? 3 : 0, daily_bytes: 5e9, best_relay_id: best, suggest, reason, no_room: [] };
  });
  return {
    relays: relays.map((r) => ({ id: r.id, name: r.name, status: r.health.status })), nodes: out,
    suggestions: [...groups].map(([id, ns]) => ({ to: { id, name: name(id) }, nodes: ns, command: `fleetctl move ${ns.map((x) => x.name).join(',')} --to ${name(id)}` })),
    single_suggestions: [], window_h: 24, interval_s: 300, min_success_pct: 99, min_gain_ms: 10,
  };
});
const pct2 = (x: MachineView, extra: number) => { const days = Math.max(1, (Date.parse(x.quota.period_end ?? '') - Date.now()) / 864e5); return Math.round(((x.quota.used + ((x.quota.daily_avg ?? 0) + extra) * days) * 1000) / ((x.quota.limit_gb ?? 1) * 1e9)) / 10; };
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

R('POST', '/moves', ({ body }) => {
  const { dst, plans, skipped, blockers, routerRestarts } = movesPlan(body.machine_ids ?? [], body.to_relay_id, body.from_relay_id, body.routers !== false);
  if (blockers.length) throw bad(blockers.join('；'), 409, 'conflict');
  const jobs = plans.map(({ node, plan, routers }) => ({ machine_id: node.id, name: node.name, from: plan.old.relay.name, old_entry: { id: plan.old.id, relay: plan.old.relay.name, port: plan.old.port }, job_id: startMove(node, plan).id, routers }));
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'entry.move', title: '批量换线路机', summary: `${jobs.length} 个节点 → ${dst.name}`, target_kind: 'machine', target_id: dst.id, detail: {} });
  emit({ type: 'machines' });
  return { from: [...new Set(jobs.map((x) => x.from))].join('、'), to: dst.name, jobs, skipped, warnings: [], router_restarts: routerRestarts, share_changed: jobs.length > 0, note: SHARE_NOTE, ok: true, blockers: [] };
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
  // 访问策略与订阅设置按机器 id 存：随编号转给新节点（同 api/replace.rs），预览列出来
  const oldPolicy = policyHook.nodeSummary(old.id); const oldSubs = subNamesOfMachine(old.id);
  if (oldPolicy || oldSubs.length) {
    const parts = [oldPolicy && `访问策略（${oldPolicy}）`, oldSubs.length && `客户端订阅的排除 / 另加（${oldSubs.join('、')}）`].filter(Boolean);
    let w = `旧节点「${old.name}」单独设的${parts.join('和')}随编号转给新节点「${nw.name}」；旧节点进备用后跟随备用团队`;
    const newPolicy = oldPolicy ? policyHook.nodeSummary(nw.id) : null;
    if (newPolicy) w += `，新节点自己单独设的访问策略（${newPolicy}）会被覆盖`;
    warnings.push(w);
  }
  // 新节点此刻故障 / 降级、没开自动续费又快到期（同服务端 new_node_notes）
  const why = nw.health.reasons.length ? `（${nw.health.reasons.map((r) => r.text).join('；')}）` : '';
  if (nw.health.status === 'down') warnings.push(`新节点「${nw.name}」此刻故障${why}：替换后客户的出口就走它，会不通；先查清，或换一台正常的`);
  else if (nw.health.status === 'degraded') warnings.push(`新节点「${nw.name}」此刻降级${why}：照常能用，替换前先看看原因`);
  const ns = nw.subscription;
  if (ns && !ns.auto_renew && ns.due_days <= ns.remind_days) {
    const left = ns.due_days < 0 ? `已过期 ${-ns.due_days} 天` : ns.due_days === 0 ? '今天到期' : `还有 ${ns.due_days} 天`;
    warnings.push(`新节点「${nw.name}」${ns.due_date} 到期（${left}）${nw.spare ? '；它在备用里，进备用时自动续费关了，替换出来也不会自己打开' : ''}：替换后它接客户，要继续用记得登记续费（或打开自动续费）`);
  }
  const plan = nw.team ? compactPlan(nw.team.id, [nw.id]) : [];
  if (nw.team) warnings.push(...compactExitsNote(plan.filter((c) => c.m.id !== old.id), `不想错位就换一台新节点：用备用团队里的，或${nw.team.name}里编号最大的那台（后面没有节点要前移）`));
  const side = (x: MachineView) => ({ id: x.id, name: x.name, kind: x.kind, team: x.team });
  return {
    nw, spare, num,
    view: {
      old: side(old), new: side(nw), new_name: old.team && old.number != null ? displayName(old.team.name, old.number) : old.name,
      old_to: { team: spare ? { id: spare.id, name: spare.name, spare: spare.spare } : null, number: num, name: spare && num != null ? displayName(spare.name, num) : old.name },
      renames: plan.map((c) => ({ machine_id: c.m.id, old_name: c.old_name, new_name: c.new_name })), routers_enabled: routers, routers: rts, ok: blockers.length === 0, blockers, warnings,
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
  if (!['telegram', 'feishu'].includes(kind)) throw bad('不支持的渠道类型');
  if (need.some((k) => !String(cfg?.[k] ?? '').trim())) throw bad('配置不完整');
};
R('GET', '/notify-channels', () => db.channels.map(pub));
R('POST', '/notify-channels', ({ body }) => {
  if (!String(body.name ?? '').trim()) throw bad('请填写渠道名称');
  checkCfg(body.kind, body.config);
  const c = { id: `c${db.seq++}`, name: String(body.name).trim(), kind: body.kind, min_severity: body.min_severity ?? 'warning', enabled: body.enabled !== false, monthly_report: body.monthly_report === true, created_at: iso(), config_hint: hintOf(body.kind, body.config), config: body.config };
  db.channels.push(c); emit({ type: 'catalog' }); return { id: c.id };
});
R('PATCH', '/notify-channels/([^/]+)', ({ params, body }) => {
  const c = channel(params[0]);
  if (body.name !== undefined) c.name = String(body.name).trim();
  if (body.config !== undefined) { checkCfg(c.kind, body.config); c.config = body.config; c.config_hint = hintOf(c.kind, body.config); }
  if (body.min_severity !== undefined) c.min_severity = body.min_severity;
  if (body.enabled !== undefined) c.enabled = !!body.enabled;
  if (body.monthly_report !== undefined) c.monthly_report = !!body.monthly_report;
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
