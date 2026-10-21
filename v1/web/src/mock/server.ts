// 浏览器内 mock：拦截 /api/v1/* 的 fetch 和实时流 WebSocket。VITE_MOCK=1 时由 main.tsx 安装。
// 有状态：改名、退役、上架、续费等操作都会改内存里的数据，并通过 mock 实时流通知前端刷新。

import type { EntryView, Job, JobLog, JobSummary, MachineView, ProbeBucket, ProvisionCheckItem, ProvisionItem, SeriesPoint } from '@/lib/types';
import { displayName } from '@/lib/format';
import { isQuiet, liveRate } from '@/lib/derive';
import { buildDb, hashStr, quotaOf, rng, type Db } from './fixtures';
import { installAccessMock } from './access';
import { installPolicyMock } from './policy';

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
  constructor() {
    listeners.add(this.fn);
    setTimeout(() => { if (this.readyState === 0) { this.readyState = 1; this.onopen?.(); } }, 50);
  }
  send() {}
  close() { this.readyState = 3; listeners.delete(this.fn); }
}

// ---------- 工具 ----------
const machine = (id: string) => { const m = db.machines.find((x) => x.id === id); if (!m) throw bad('机器不存在', 404, 'not_found'); return m; };
const teamOf = (id: string) => { const t = db.teams.find((x) => x.id === id); if (!t) throw bad('团队不存在', 404, 'not_found'); return t; };
const clone = <T,>(x: T): T => JSON.parse(JSON.stringify(x));

function refreshCounts() {
  for (const t of db.teams) t.node_count = db.machines.filter((m) => m.team?.id === t.id && m.lifecycle !== 'retired').length;
  for (const p of db.plans) p.machine_count = db.machines.filter((m) => m.plan?.id === p.id && m.lifecycle !== 'retired').length;
}
function renameMachine(m: MachineView) {
  if (m.kind === 'relay') m.name = m.label ?? m.name;
  else if (m.team && m.number != null) m.name = displayName(m.team.name, m.number);
  for (const x of db.machines) for (const e of x.entries) {
    if (e.node.id === m.id) e.node.name = m.name;
    if (e.relay.id === m.id) e.relay.name = m.name;
  }
}
/** 同团队在用节点压缩成 01..N；exclude 为即将退役的机器。返回改名清单 */
function compactPlan(teamId: string, excludeId?: string) {
  const team = teamOf(teamId);
  const list = db.machines.filter((x) => x.team?.id === teamId && x.id !== excludeId && x.lifecycle !== 'retired' && x.number != null).sort((a, b) => a.number! - b.number!);
  return list.map((x, i) => ({ m: x, n: i + 1, old_name: x.name, new_name: displayName(team.name, i + 1) })).filter((c) => c.m.number !== c.n);
}
/** 退役完成：释放编号（number=null，名称保留作历史），并压缩同团队编号 */
function finishRetire(m: MachineView) {
  m.lifecycle = 'retired'; m.retired_at = iso(); m.active_job = null; m.health = { status: 'inactive', reasons: [] };
  const tid = m.team?.id; // 退役机器保留原编号与名称（可能和在用机器同名）
  if (tid) {
    const plan = compactPlan(tid);
    for (const c of plan) { c.m.number = c.n; c.m.revision++; renameMachine(c.m); }
    if (plan.length) db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: 'system', action: 'team.compact', title: '自动压缩编号', summary: `${m.name} 退役后 ${plan.length} 台前移`, target_kind: 'team', target_id: tid, detail: { count: plan.length } });
  }
  emit({ type: 'machines' });
}
function recomputeDue(m: MachineView) {
  if (m.subscription) m.subscription.due_days = Math.ceil((new Date(m.subscription.due_date).getTime() - Date.now()) / DAY);
}
const monthly = (cents: number, cycle: string) => cents / ({ monthly: 1, quarterly: 3, semiannual: 6, yearly: 12 }[cycle] ?? 1);

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
    // 需要续费的：自动续费的只在已过期时算（同服务端）
    // 在线/离线按 agent 连通（同 isQuiet）
    online: live.filter((m) => !isQuiet(m)).length, offline: live.filter(isQuiet).length,
    due_7d: live.filter((m) => m.subscription && m.subscription.due_days <= 7 && (m.subscription.due_days < 0 || !m.subscription.auto_renew)).length,
    monthly_cost: Object.entries(live.reduce<Record<string, number>>((acc, m) => { if (m.subscription) acc[m.subscription.currency] = (acc[m.subscription.currency] ?? 0) + Math.round(monthly(m.subscription.price_cents, m.subscription.cycle)); return acc; }, {})).map(([currency, cents]) => ({ currency, cents })),
    traffic_month: { up, down },
    // 实时速率只算数据没过期的（同 liveRate）
    net_rx_bps: live.reduce((s, m) => s + (liveRate(m)?.rx ?? 0), 0), net_tx_bps: live.reduce((s, m) => s + (liveRate(m)?.tx ?? 0), 0),
  };
}

function series(m: MachineView, metric: string, range: string): { metric: string; range: string; resolution: string; points: SeriesPoint[] } {
  const spec: Record<string, [number, number]> = { '1h': [60, 60_000], '6h': [72, 300_000], '24h': [96, 900_000], '7d': [84, 7200_000], '30d': [90, 28800_000] };
  const [n, step] = spec[range] ?? spec['1h'];
  const r = rng(hashStr(m.id + metric));
  const end = Date.now(); const pts: SeriesPoint[] = [];
  const h = m.host; const isDown = m.health.status === 'down';
  const cpu0 = h?.cpu_pct ?? 20; let walk = cpu0;
  for (let i = 0; i < n; i++) {
    const ts = iso(end - (n - 1 - i) * step);
    walk = Math.max(2, Math.min(97, walk + (r() - 0.5) * 9));
    const tail = isDown && i > n - 6;
    if (metric === 'host') {
      pts.push({ ts, cpu_pct: tail ? null : walk, mem_pct: tail ? null : (h?.mem_used != null && h.mem_total ? (h.mem_used / h.mem_total) * 100 : 40) + Math.sin(i / 9) * 4, disk_pct: h?.disk_used != null && h.disk_total ? (h.disk_used / h.disk_total) * 100 + i * 0.01 : 30, load1: tail ? null : +(walk / 40).toFixed(2), net_rx_bps: tail ? null : (h?.net_rx_bps ?? 1e7) * (0.5 + r()), net_tx_bps: tail ? null : (h?.net_tx_bps ?? 1e7) * (0.4 + r()) });
    } else if (metric === 'traffic') {
      const k = step / 60_000;
      pts.push({ ts, up: Math.round(r() * 8e6 * k * (1 + Math.sin(i / 7))), down: Math.round(r() * 3e7 * k * (1 + Math.cos(i / 8))), conns: Math.round(20 + r() * 80) });
    } else {
      const tcpFail = tail || r() < 0.015; const egFail = tail || r() < 0.02;
      pts.push({ ts, tcp_ok: !tcpFail, tcp_ms: tcpFail ? null : Math.round(40 + r() * 70 + (i % 23 === 0 ? 120 : 0)), egress_ok: !egFail, egress_ms: egFail ? null : Math.round(160 + r() * 90), tcp_n: 2, tcp_ok_n: tcpFail ? 0 : 2 });
    }
  }
  return { metric, range, resolution: range === '7d' || range === '30d' ? '1h' : '1m', points: pts };
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
      const per = m.entries.length * (i === 19 ? curMin : 3); // 每条入口每分钟探测一次
      const r = rng(hashStr(`${m.id}:${slot - 19 + i}`));
      if (m.id.endsWith('2') && i < 4) return [0, 0, null]; // 演示：刚开始探测的那段没有数据
      const fail = isDown && i >= 17 ? per : r() < 0.06 ? 1 + Math.floor(r() * per * 0.6) : 0;
      return [per, per - fail, fail === per ? null : Math.round(40 + r() * 70 + (r() < 0.05 ? 120 : 0))];
    });
  }
  return out;
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
    cur.state = 'succeeded'; cur.finished_at = iso(); cur.output = outputs[cur.idx] ?? null;
    log(job.id, 'info', `步骤 ${cur.idx + 1}/${job.steps.length} 完成：${cur.title}`);
    const next = job.steps[cur.idx + 1];
    if (next) { next.state = 'running'; next.started_at = iso(); job.step = next.idx; log(job.id, 'info', `开始：${next.title}`); emit({ type: 'job', id: job.id }); setTimeout(tick, stepMs); }
    else { job.state = 'succeeded'; job.cancellable = false; job.finished_at = iso(); onDone(); emit({ type: 'job', id: job.id }); emit({ type: 'machines' }); }
  };
  log(job.id, 'info', `开始：${job.steps[0].title}`);
  setTimeout(tick, stepMs);
}

function startProvisionJob(m: MachineView) {
  const job = newJob(m.kind === 'relay' ? 'provision_relay' : m.kind === 'proxy' ? 'provision_proxy' : 'provision_vps', { id: m.id, name: m.name }, PSTEPS);
  m.active_job = { id: job.id, kind: job.kind, state: 'running' };
  drive(job, 2800, () => {
    m.lifecycle = 'active'; m.activated_at = iso(); m.active_job = null; m.health = { status: 'ok', reasons: [] };
    if (m.kind !== 'proxy') { m.agent = { version: '1.0.3', last_report_at: iso(), online: true, xray_running: null, cores: 2, health: null }; m.host = { ts: iso(), cpu_pct: 6, load1: 0.1, mem_used: 400 * 1024 ** 2, mem_total: 4 * GiB, disk_used: 4 * GiB, disk_total: 60 * GiB, net_rx_bps: 2e5, net_tx_bps: 1e5, tcp_conns: 12 }; }
    for (const e of m.entries) { e.lifecycle = 'active'; e.applied = true; e.tcp = { ok: true, latency_ms: 52, detail: null, ts: iso() }; e.egress = { ok: true, latency_ms: 190, detail: null, ts: iso() }; }
    refreshCounts();
  });
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
  advance(run, 40_000, () => { m.lifecycle = 'active'; m.active_job = null; m.health = { status: 'ok', reasons: [] }; refreshCounts(); });
  advance(retJob, 50_000, () => { finishRetire(rm); refreshCounts(); });
}

// ---------- 上架校验 ----------
const IP_RE = /^(\d{1,3})(\.\d{1,3}){3}$/;
function checkProvision(items: ProvisionItem[]): ProvisionCheckItem[] {
  const usedNum = new Map<string, Set<number>>();
  for (const m of db.machines) if (m.team && m.number != null && m.lifecycle !== 'retired') { const s = usedNum.get(m.team.id) ?? new Set(); s.add(m.number); usedNum.set(m.team.id, s); }
  const ips = new Set(db.machines.filter((m) => m.lifecycle !== 'retired').map((m) => m.ip));
  const portUsed = new Map<string, number>();
  const out: ProvisionCheckItem[] = [];
  const seenIp = new Set<string>();
  for (const it of items) {
    const errors: string[] = [];
    if (!IP_RE.test(it.ip ?? '') || it.ip.split('.').some((x) => +x > 255)) errors.push('IP 格式不正确');
    else if (ips.has(it.ip) || seenIp.has(it.ip)) errors.push(`IP ${it.ip} 已存在`);
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
    out.push({ ok: errors.length === 0, errors, name, number, relay_port: relayPort });
  }
  return out;
}

function createMachine(it: ProvisionItem, c: ProvisionCheckItem): MachineView {
  const id = `m-${db.seq++}`;
  const plan = db.plans.find((p) => p.id === it.plan_id) ?? null;
  const pv = db.providers.find((p) => p.id === (it.provider_id ?? plan?.provider_id)) ?? null;
  const team = it.kind === 'relay' ? null : teamOf(it.team_id!);
  const m: MachineView = {
    id, kind: it.kind, name: c.name, team: team ? { id: team.id, name: team.name } : null, number: it.kind === 'relay' ? null : c.number, label: it.kind === 'relay' ? it.label ?? null : null,
    ip: it.ip, ssh_port: it.ssh_port ?? 22, provider: pv ? { id: pv.id, name: pv.name } : null, provider_code: pv?.code ?? null,
    plan: plan ? { id: plan.id, name: plan.name, bandwidth_mbps: plan.bandwidth_mbps, traffic_gb: plan.traffic_gb } : null, service_ref: it.service_ref ?? null,
    lifecycle: 'provisioning', lifecycle_reason: null, note: it.note ?? '', revision: 1, created_at: iso(), activated_at: null, retired_at: null,
    subscription: it.subscription?.due_date ? { price_cents: it.subscription.price_cents ?? plan?.price_cents ?? 0, currency: plan?.currency ?? 'USD', cycle: it.subscription.cycle ?? plan?.cycle ?? 'monthly', due_date: it.subscription.due_date, due_days: 0, billing_tz: it.subscription.billing_tz ?? pv?.timezone ?? 'Asia/Shanghai', auto_renew: false, remind_days: 7, traffic_gb: plan?.traffic_gb || null, traffic_reset_day: 1 } : null,
    agent: null, host: null, entries: [], removed_traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 }, quota: { used: 0, period_start: '' }, health: { status: 'unknown', reasons: [{ key: 'provisioning', severity: 'info', text: '上架作业进行中' }] }, active_job: null, failed_job: null, open_incidents: 0, maintenance: false,
    relay: it.kind === 'relay' ? { public_host: it.ip, generation: 1, applied_generation: 0, applied_at: null, apply_error: null, port_min: 20001, port_max: 20999 } : null,
  };
  if (m.kind !== 'relay') {
    const relay = machine(it.relay_id!);
    const e: EntryView = { id: `e${db.seq++}`, relay: { id: relay.id, name: relay.name }, node: { id, name: m.name }, port: c.relay_port ?? 20999, lifecycle: 'pending', applied: false, apply_error: null, tcp: null, egress: null, traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 } };
    m.entries = [e]; relay.entries.push(e);
  }
  recomputeDue(m); m.quota = quotaOf(m);
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
  const t = { id: `t${db.seq++}`, name: body.name.trim(), sort_order: body.sort_order ?? db.teams.length + 1, node_count: 0 };
  db.teams.push(t); emit({ type: 'catalog' }); return t;
});
R('PATCH', '/teams/([^/]+)', ({ params, body }) => {
  const t = teamOf(params[0]);
  if (body.name && body.name !== t.name) {
    if (db.teams.some((x) => x.name === body.name)) throw bad('团队名已存在', 409, 'conflict');
    t.name = body.name; db.machines.filter((m) => m.team?.id === t.id).forEach((m) => { m.team = { id: t.id, name: t.name }; renameMachine(m); });
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
R('GET', '/teams/([^/]+)/renumber', ({ params }) => { const { changes, token } = renumberPlan(params[0]); return { changes, token }; });
R('POST', '/teams/([^/]+)/renumber', ({ params, body }) => {
  const p = renumberPlan(params[0]);
  if (body.token !== p.token) throw bad('数据在预览后已变化，请重新预览', 409, 'conflict');
  for (const c of p.raw) { const m = machine(c.machine_id); m.number = c.n; m.revision++; renameMachine(m); }
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'team.renumber', title: '重新编号', summary: `${teamOf(params[0]).name}：${p.raw.length} 台改名`, target_kind: 'team', target_id: params[0], detail: { count: p.raw.length } });
  emit({ type: 'machines' }); emit({ type: 'catalog' }); return { changed: p.raw.length };
});
R('GET', '/providers', () => clone(db.providers));
R('POST', '/providers', ({ body }) => {
  if (!body.name || !body.code) throw bad('名称与代码必填');
  const p = { id: `p${db.seq++}`, name: body.name, code: String(body.code).toUpperCase(), panel_url: body.panel_url || null, timezone: body.timezone || 'UTC' };
  db.providers.push(p); emit({ type: 'catalog' }); return p;
});
R('PATCH', '/providers/([^/]+)', ({ params, body }) => {
  const p = db.providers.find((x) => x.id === params[0]);
  if (!p) throw bad('没有这个供应商', 404, 'not_found');
  if (body.name !== undefined) p.name = body.name;
  if (body.code !== undefined) p.code = body.code;
  if (body.panel_url !== undefined) p.panel_url = body.panel_url || null;
  if (body.timezone !== undefined) p.timezone = body.timezone || 'UTC';
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
  const { archived, ...rest } = body; Object.assign(p, rest);
  if (archived === true) p.archived_at = iso(); if (archived === false) p.archived_at = null;
  emit({ type: 'catalog' }); return p;
});

R('GET', '/machines', ({ query }) => {
  const scope = query.get('scope') ?? 'live';
  const list = db.machines.filter((m) => scope === 'all' || (scope === 'retired' ? m.lifecycle === 'retired' : m.lifecycle !== 'retired'));
  return clone(list);
});
R('GET', '/machines/([^/]+)', ({ params }) => clone(machine(params[0])));
R('PATCH', '/machines/([^/]+)', ({ params, body }) => {
  const m = machine(params[0]);
  if (body.revision !== m.revision) throw bad('这台机器刚被其他人修改过，请刷新后重试', 409, 'revision_conflict');
  if (body.team_id !== undefined || body.number !== undefined) {
    const team = teamOf(body.team_id ?? m.team?.id);
    const number = body.number ?? m.number;
    if (db.machines.some((x) => x.id !== m.id && x.team?.id === team.id && x.number === number && x.lifecycle !== 'retired')) throw bad(`${displayName(team.name, number)} 已被占用`, 409, 'conflict');
    m.team = { id: team.id, name: team.name }; m.number = number;
  }
  if (body.label !== undefined) m.label = body.label;
  for (const k of ['note', 'service_ref'] as const) if (body[k] !== undefined) (m as any)[k] = body[k];
  if (body.provider_id !== undefined) { const p = db.providers.find((x) => x.id === body.provider_id); m.provider = p ? { id: p.id, name: p.name } : null; m.provider_code = p?.code ?? null; }
  if (body.plan_id !== undefined) { const p = db.plans.find((x) => x.id === body.plan_id); m.plan = p ? { id: p.id, name: p.name, bandwidth_mbps: p.bandwidth_mbps, traffic_gb: p.traffic_gb } : null; }
  m.revision++; renameMachine(m); refreshCounts();
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'machine.update', title: '修改机器', summary: `${m.name}：${Object.keys(body).filter((k) => k !== 'revision').join('、')}`, target_kind: 'machine', target_id: m.id, detail: body });
  emit({ type: 'machines' }); emit({ type: 'catalog' }); return clone(m);
});
R('GET', '/machines/([^/]+)/series', ({ params, query }) => series(machine(params[0]), query.get('metric') ?? 'host', query.get('range') ?? '1h'));
R('GET', '/machines/([^/]+)/retire', ({ params }) => {
  const m = machine(params[0]);
  const renames = m.team && m.lifecycle !== 'retired' ? compactPlan(m.team.id, m.id).map((c) => ({ machine_id: c.m.id, old_name: c.old_name, new_name: c.new_name })) : [];
  return { name: m.name, renames };
});
R('POST', '/machines/([^/]+)/retire', ({ params, body }) => {
  const m = machine(params[0]);
  if (m.lifecycle === 'retired') throw bad('这台机器已退役');
  if (body.confirm !== m.name) throw bad('确认名称不匹配', 400, 'confirm_mismatch');
  const steps: [string, string][] = [['disable', '停用入口'], ...(body.cleanup ? [['cleanup', '清理节点上的服务'] as [string, string]] : []), ['revoke', '撤销令牌'], ['mark', '标记为已退役']];
  const job = newJob('retire', { id: m.id, name: m.name }, steps);
  m.lifecycle = 'retiring'; m.lifecycle_reason = body.reason ?? null; m.active_job = { id: job.id, kind: 'retire', state: 'running' }; m.entries.forEach((e) => { e.lifecycle = 'disabled'; });
  drive(job, 2200, () => { finishRetire(m); refreshCounts(); });
  emit({ type: 'machines' }); return { job_id: job.id };
});
R('POST', '/machines/([^/]+)/purge', ({ params, body }) => {
  const m = machine(params[0]);
  if (m.lifecycle !== 'retired') throw bad('只有已退役的机器才能彻底删除', 409, 'conflict');
  if (body.confirm !== m.name) throw bad('确认名称不匹配', 400, 'confirm_mismatch');
  const job = newJob('purge', { id: m.id, name: m.name }, [['telemetry', '删除遥测数据'], ['record', '删除机器记录']]);
  drive(job, 1800, () => { db.machines = db.machines.filter((x) => x.id !== m.id); db.machines.forEach((x) => { x.entries = x.entries.filter((e) => e.node.id !== m.id); }); refreshCounts(); });
  return { job_id: job.id };
});
R('PUT', '/machines/([^/]+)/subscription', ({ params, body }) => {
  const m = machine(params[0]);
  m.subscription = { traffic_gb: null, traffic_reset_day: null, ...(m.subscription ?? {} as any), ...body, due_days: 0 };
  recomputeDue(m); m.quota = quotaOf(m); emit({ type: 'machine', id: m.id }); emit({ type: 'machines' }); return clone(m.subscription);
});
R('GET', '/machines/([^/]+)/renewals', ({ params }) => clone(db.renewals[params[0]] ?? []));
R('POST', '/machines/([^/]+)/renewals', ({ params, body }) => {
  const m = machine(params[0]); if (!m.subscription) throw bad('这台机器还没有订阅信息，请先保存订阅');
  if (!/^\d{4}-\d{2}-\d{2}$/.test(body.new_due ?? '')) throw bad('新到期日格式应为 YYYY-MM-DD');
  const r = { id: `rn${db.seq++}`, old_due: m.subscription.due_date, new_due: body.new_due, amount_cents: body.amount_cents ?? m.subscription.price_cents, note: body.note ?? null, actor: db.user, created_at: iso() };
  (db.renewals[m.id] ??= []).unshift(r); m.subscription.due_date = body.new_due; recomputeDue(m);
  emit({ type: 'machine', id: m.id }); emit({ type: 'machines' }); return r;
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
  const port = Math.max(20000, ...relay.entries.map((x) => x.port)) + 1;
  const e: EntryView = { id: `e${db.seq++}`, relay: { id: relay.id, name: relay.name }, node: { id: m.id, name: m.name }, port, lifecycle: 'pending', applied: false, apply_error: null, tcp: null, egress: null, traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 } };
  m.entries.push(e); relay.entries.push(e);
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
  return paged(db.jobs.filter((j) => s === 'all' || (s === 'active' ? j.state === 'running' || j.state === 'queued' : j.state !== 'running' && j.state !== 'queued')).map(jobSummary), query);
});
const jobOf = (id: string) => { const j = db.jobs.find((x) => x.id === id); if (!j) throw bad('作业不存在', 404, 'not_found'); return j; };
R('GET', '/jobs/([^/]+)', ({ params }) => { const j = clone(jobOf(params[0])); const s = jobSummary(j); return { ...j, step: s.step, step_title: s.step_title }; });
R('GET', '/jobs/([^/]+)/logs', ({ params, query }) => { const after = +(query.get('after') ?? 0); return clone((db.logs[params[0]] ?? []).filter((l) => l.id > after)); });
/** 重试：body { reset_host_key?, ssh_password? }（C1）。指纹不符不重置、认证失败不给密码时 mock 照样失败 */
R('POST', '/jobs/([^/]+)/retry', ({ params, body }) => {
  const j = jobOf(params[0]); if (j.state !== 'failed') throw bad('只有失败的作业才能重试', 409, 'conflict');
  const f = j.steps.find((s) => s.state === 'failed')!;
  if (f.error_kind === 'host_key' && !body.reset_host_key) throw bad('主机指纹仍与记录的不一致：确认机器重装过后勾选「重置主机指纹」', 409, 'conflict');
  if (f.error_kind === 'auth' && !body.ssh_password) throw bad('中控密钥仍登录不上：请填写 SSH 密码', 409, 'conflict');
  f.state = 'running'; f.error = null; f.error_kind = null; j.state = 'running'; j.cancellable = true; j.error = null; j.finished_at = null; log(j.id, 'info', `从失败的步骤重试${body.reset_host_key ? '（已重置主机指纹）' : ''}`);
  const m = db.machines.find((x) => x.id === j.target?.id);
  if (m) { if (m.lifecycle === 'failed') m.lifecycle = 'provisioning'; m.failed_job = null; m.active_job = { id: j.id, kind: j.kind, state: 'running' }; }
  drive(j, 2500, () => { if (!m) return; if (j.kind === 'retire') { finishRetire(m); refreshCounts(); return; } m.lifecycle = 'active'; m.active_job = null; m.health = { status: 'ok', reasons: [] }; });
  emit({ type: 'job', id: j.id }); emit({ type: 'machines' }); return {};
});
/** 取消：进行中的作业，或失败的上架 / 恢复作业（上架 → 退役并清理；恢复 → 退回已退役） */
R('POST', '/jobs/([^/]+)/cancel', ({ params }) => {
  const j = jobOf(params[0]); if (!j.cancellable) throw bad('这个作业不能取消', 409, 'conflict');
  const failed = j.state === 'failed';
  j.state = 'cancelled'; j.cancellable = false; j.finished_at = iso(); j.steps.forEach((s) => { if (s.state === 'running' || s.state === 'pending' || s.state === 'failed') s.state = 'skipped'; else if (s.state === 'done') s.state = 'compensated'; }); log(j.id, 'warn', '作业已取消，已完成的步骤已补偿');
  const m = db.machines.find((x) => x.id === j.target?.id);
  if (failed && m && m.lifecycle === 'failed') { m.failed_job = null; finishRetire(m); refreshCounts(); }
  emit({ type: 'job', id: j.id }); return {};
});

R('GET', '/overview', () => overview());
R('GET', '/overview/series', ({ query }) => overviewSeries(Math.min(60, +(query.get('minutes') ?? 30))));
R('GET', '/probe-beats', () => probeBeats());
R('GET', '/incidents', ({ query }) => { const s = query.get('state') ?? 'firing'; return paged(db.incidents.filter((i) => s === 'all' || i.state === s), query); });
R('POST', '/incidents/([^/]+)/ack', ({ params }) => { const i = db.incidents.find((x) => x.id === params[0]); if (!i) throw bad('告警不存在', 404, 'not_found'); i.ack_at = iso(); i.ack_by = db.user; emit({ type: 'incidents' }); return {}; });
R('GET', '/silences', () => clone(db.silences));
R('POST', '/silences', ({ body }) => { const m = body.subject_id ? machine(body.subject_id) : null; const s = { id: `s${db.seq++}`, subject_name: m?.name ?? null, rule: null, subject_id: null, ...body }; db.silences.push(s); emit({ type: 'incidents' }); return s; });
R('DELETE', '/silences/([^/]+)', ({ params }) => { db.silences = db.silences.filter((s) => s.id !== params[0]); emit({ type: 'incidents' }); return {}; });
R('GET', '/audit', ({ query }) => clone(db.audit.filter((a) => !query.get('target_id') || a.target_id === query.get('target_id')).slice(0, +(query.get('limit') ?? 50))));

installAccessMock(R, () => db.machines);
installPolicyMock(R, { teams: () => db.teams, machines: () => db.machines, emit, user: () => db.user, bad });
// ---------- 升级 agent ----------
const UPSTEPS: [string, string][] = [['push', '推送 agent'], ['restart', '重启 agent'], ['verify', '确认上报']];
function startUpgrade(m: MachineView) {
  const job = newJob('upgrade_agent', { id: m.id, name: m.name }, UPSTEPS);
  m.active_job = { id: job.id, kind: 'upgrade_agent', state: 'running' };
  drive(job, 1800, () => { m.active_job = null; if (m.agent) m.agent.version = '1.0.4'; });
  return job;
}
R('POST', '/machines/([^/]+)/upgrade-agent', ({ params }) => {
  const m = machine(params[0]);
  if (!m.agent) throw bad('这台机器没有 agent（家宽代理不装 agent）');
  if (m.active_job) throw bad('这台机器有进行中的作业，请等它结束', 409, 'conflict');
  const job = startUpgrade(m); emit({ type: 'machines' });
  return { machine_id: m.id, job_id: job.id };
});
R('POST', '/agents/upgrade', () => {
  const jobs = db.machines.filter((m) => m.lifecycle === 'active' && m.agent && !m.active_job).map((m) => ({ machine_id: m.id, job_id: startUpgrade(m).id }));
  emit({ type: 'machines' }); return { jobs };
});

// ---------- 换线路机 / 搬空线路机 / 恢复已退役的机器 ----------
const MOVE_STEPS: [string, string][] = [['attach_new', '在新线路机建入口'], ['firewall_widen', '放行新线路机（节点防火墙）'], ['wait_applied_new', '等待新线路机生效'], ['verify_new', '经新入口出口验收'], ['detach_old', '摘除旧入口'], ['wait_detached_old', '等待旧线路机摘除'], ['firewall_narrow', '收紧节点防火墙']];
const RESTORE_STEPS: [string, string][] = [['preflight', '预检与身份'], ['bootstrap', '引导密钥'], ['converge', '节点收敛（SSH 加固、SS、防火墙）'], ['agent', '安装 agent'], ['wait_report', '等待首次上报'], ['attach', '接入线路机'], ['wait_applied', '等待线路机生效'], ['verify', '出口验收'], ['activate', '上线']];
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
  const ne: EntryView = { id: `e${db.seq++}`, relay: { id: to.id, name: to.name }, node: { id: m.id, name: m.name }, port: Math.max(20000, ...to.entries.map((x) => x.port)) + 1, lifecycle: 'pending', applied: false, apply_error: null, tcp: null, egress: null, traffic: blankTraffic() };
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
R('POST', '/machines/([^/]+)/move', ({ params, body }) => {
  const m = machine(params[0]);
  const plan = planMove(m, body.to_relay_id, body.from_relay_id);
  if (typeof plan === 'string') throw bad(plan, 409, 'conflict');
  const from = plan.old.relay.name;
  const job = startMove(m, plan);
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'entry.move', title: '换线路机', summary: `${m.name} · ${from} → ${plan.to.name}`, target_kind: 'machine', target_id: m.id, detail: {} });
  emit({ type: 'machines' }); emit({ type: 'job', id: job.id });
  return { job_id: job.id, name: m.name, from, to: plan.to.name, share_changed: true, note: SHARE_NOTE };
});
R('POST', '/relays/([^/]+)/evacuate', ({ params, body }) => {
  const src = machine(params[0]);
  if (src.kind !== 'relay') throw bad('只有线路机能搬空', 409, 'conflict');
  if (body.to_relay_id === src.id) throw bad('目标线路机不能和要搬空的是同一台');
  const dst = machine(body.to_relay_id);
  if (dst.kind !== 'relay' || dst.lifecycle !== 'active') throw bad('目标线路机不在用', 409, 'conflict');
  const jobs: { machine_id: string; name: string; job_id: string }[] = []; const skipped: { machine_id: string; name: string | null; reason: string }[] = [];
  for (const e of [...src.entries]) {
    if (e.lifecycle === 'removing' || e.lifecycle === 'removed') continue;
    const node = machine(e.node.id);
    const plan = planMove(node, dst.id, src.id);
    if (typeof plan === 'string') { skipped.push({ machine_id: node.id, name: node.name, reason: plan }); continue; }
    jobs.push({ machine_id: node.id, name: node.name, job_id: startMove(node, plan).id });
  }
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'relay.evacuate', title: '搬空线路机', summary: `${src.name} → ${dst.name}（${jobs.length} 个作业）`, target_kind: 'machine', target_id: src.id, detail: {} });
  emit({ type: 'machines' });
  return { from: src.name, to: dst.name, jobs, skipped, share_changed: jobs.length > 0, note: SHARE_NOTE };
});

function restorePlan(m: MachineView, teamId: string | null, number: string | null, label: string | null) {
  const blockers: string[] = [];
  if (m.lifecycle !== 'retired') blockers.push('这台机器现在不是已退役状态，不能恢复');
  const other = db.machines.find((x) => x.id !== m.id && x.ip === m.ip && x.lifecycle !== 'retired');
  const ip_conflict = other ? { machine_id: other.id, name: other.name, lifecycle: other.lifecycle } : null;
  if (other) blockers.push(`IP ${m.ip} 已被未退役的机器「${other.name}」占用：先把它退役，或确认这台机器的 IP 没有变`);
  let name = m.name; let num: number | null = null; let team = m.team;
  if (m.kind === 'relay') {
    name = (label?.trim() || m.label || m.name);
    if (db.machines.some((x) => x.kind === 'relay' && x.lifecycle !== 'retired' && x.label === name && x.id !== m.id)) blockers.push(`已经有同名线路机「${name}」：恢复时换一个名字`);
  } else {
    const t = teamId ? db.teams.find((x) => x.id === teamId) : db.teams.find((x) => x.id === m.team?.id);
    if (!t) blockers.push('团队不存在'); else {
      team = { id: t.id, name: t.name };
      const taken = new Set(db.machines.filter((x) => x.team?.id === t.id && x.id !== m.id && x.lifecycle !== 'retired' && x.number != null).map((x) => x.number!));
      if (number) { const n = +number; if (taken.has(n)) blockers.push(`编号 ${n} 已被占用`); else num = n; } else { let n = 1; while (taken.has(n)) n++; num = n; }
      if (num != null) name = displayName(t.name, num);
    }
  }
  return { blockers, ip_conflict, name, num, team, label: m.kind === 'relay' ? name : null };
}
R('GET', '/machines/([^/]+)/restore', ({ params, query }) => {
  const m = machine(params[0]);
  const p = restorePlan(m, query.get('team_id'), query.get('number'), query.get('label'));
  const probe = query.get('probe') !== 'false' && m.kind !== 'proxy';
  const keyLogin = m.id === 'm-t1-3r' ? 'denied' : 'ok';
  const last = db.machines.find((x) => x.kind === 'relay' && x.lifecycle === 'active' && x.id === (m.team?.id === 't2' ? 'r2' : 'r1'));
  return {
    id: m.id, kind: m.kind, lifecycle: m.lifecycle, ip: m.ip, old_name: m.name, name: p.name, team: p.team, number: p.num, label: p.label,
    restorable: p.blockers.length === 0, blockers: p.blockers, warnings: [], ip_conflict: p.ip_conflict, retired_reason: m.lifecycle_reason,
    last_relay: last ? { id: last.id, name: last.name } : null,
    ssh: probe ? { reachable: true, key_login: keyLogin, host_key: 'SHA256:q3Zp1vJm0cJ2oWfX8sB9tqk1eYb7rUo5nHj2LdPaVx4' } : null,
    needs: { relay: m.kind !== 'relay', upstream: m.kind === 'proxy', password: m.kind === 'proxy' ? 'no' : keyLogin === 'denied' ? 'required' : 'no' },
  };
});
R('POST', '/machines/([^/]+)/restore', ({ params, body }) => {
  const m = machine(params[0]);
  const p = restorePlan(m, body.team_id ?? null, body.number != null ? String(body.number) : null, body.label ?? null);
  if (p.blockers.length) throw bad(p.blockers.join('；'), 409, 'conflict');
  const relay = m.kind === 'relay' ? null : db.machines.find((x) => x.id === body.relay_id && x.kind === 'relay' && x.lifecycle === 'active');
  if (m.kind !== 'relay' && !relay) throw bad('要选一台在用的线路机');
  if (m.kind === 'proxy' && !body.upstream?.host) throw bad('上游代理要重新填协议、地址和端口');
  const old = m.name;
  m.lifecycle = 'provisioning'; m.lifecycle_reason = '恢复中'; if (p.team) m.team = p.team; m.number = p.num; if (m.kind === 'relay') m.label = p.label; m.name = p.name; m.revision++;
  m.health = { status: 'unknown', reasons: [{ key: 'provisioning', severity: 'info', text: '恢复作业进行中' }] };
  let e: EntryView | null = null;
  if (relay) {
    e = { id: `e${db.seq++}`, relay: { id: relay.id, name: relay.name }, node: { id: m.id, name: m.name }, port: Math.max(20000, ...relay.entries.map((x) => x.port)) + 1, lifecycle: 'pending', applied: false, apply_error: null, tcp: null, egress: null, traffic: blankTraffic() };
    m.entries = [e]; relay.entries.push(e);
  }
  const relaySteps: [string, string][] = [['preflight', '预检与身份'], ['bootstrap', '引导密钥'], ['converge_relay', '线路机收敛（SSH 加固、Xray）'], ['agent', '安装 agent'], ['wait_report', '等待首次上报'], ['wait_applied', '等待 Xray 配置生效'], ['activate', '上线']];
  const steps = m.kind === 'proxy' ? RESTORE_STEPS.filter(([k]) => ['attach', 'wait_applied', 'verify', 'activate'].includes(k)) : m.kind === 'relay' ? relaySteps : RESTORE_STEPS;
  const job = newJob(`restore_${m.kind}`, { id: m.id, name: m.name }, steps);
  m.active_job = { id: job.id, kind: job.kind, state: 'running' };
  drive(job, 2000, () => {
    m.lifecycle = 'active'; m.retired_at = null; m.lifecycle_reason = `已恢复${body.reason ? `：${body.reason}` : ''}`; m.active_job = null; m.health = { status: 'ok', reasons: [] };
    if (m.kind !== 'proxy') { m.agent = { version: '1.0.3', last_report_at: iso(), online: true, xray_running: null, cores: 2, health: null }; m.host = { ts: iso(), cpu_pct: 5, load1: 0.1, mem_used: 380 * 1024 ** 2, mem_total: 4 * GiB, disk_used: 4 * GiB, disk_total: 60 * GiB, net_rx_bps: 2e5, net_tx_bps: 1e5, tcp_conns: 10 }; }
    if (e) { e.lifecycle = 'active'; e.applied = true; e.tcp = { ok: true, latency_ms: 50, detail: null, ts: iso() }; e.egress = { ok: true, latency_ms: 180, detail: null, ts: iso() }; }
    refreshCounts();
  });
  db.audit.unshift({ id: `a${db.seq++}`, ts: iso(), actor: db.user, action: 'machine.restore', title: '恢复机器', summary: `${old} → ${m.name}`, target_kind: 'machine', target_id: m.id, detail: {} });
  refreshCounts(); emit({ type: 'machines' }); emit({ type: 'catalog' });
  return { job_id: job.id, machine_id: m.id, name: m.name, old_name: old, number: m.number };
});

// ---------- 设置：API 密钥 / 通知渠道 ----------
const rand = (n: number) => Array.from({ length: n }, () => 'abcdefghijklmnopqrstuvwxyz0123456789'[Math.floor(Math.random() * 36)]).join('');
R('GET', '/api-keys', () => clone(db.apiKeys));
R('POST', '/api-keys', ({ body }) => {
  const scopes: string[] = body.scopes ?? [];
  if (!String(body.name ?? '').trim()) throw bad('请填写密钥名称');
  if (!scopes.length || scopes.some((x) => !['read', 'write', 'secrets', 'exec'].includes(x))) throw bad('请至少选择一项有效权限');
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

// ---------- 安装 ----------
/** 模拟上报：改读数，按服务端 metrics 消息的形状推增量（见 lib/live.ts） */
function jitter() {
  const machines: Record<string, unknown> = {};
  const now = Math.floor(Date.now() / 1000);
  for (const m of db.machines) if (m.host && m.lifecycle === 'active' && m.health.status !== 'down') {
    const h = m.host; const f = () => 0.85 + Math.random() * 0.3;
    h.cpu_pct = Math.max(1, Math.min(99, (h.cpu_pct ?? 10) * f())); h.net_rx_bps = Math.round((h.net_rx_bps ?? 1e6) * f()); h.net_tx_bps = Math.round((h.net_tx_bps ?? 1e6) * f()); h.ts = iso(now * 1000);
    if (h.load1 != null) h.load1 = +((h.cpu_pct ?? 0) / 40).toFixed(2);
    if (m.agent) m.agent.last_report_at = h.ts;
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
