// 浏览器内 mock：拦截 /api/v1/* 的 fetch，并替换 EventSource。VITE_MOCK=1 时由 main.tsx 安装。
// 有状态：改名、退役、上架、续费等操作都会改内存里的数据，并通过 mock SSE 通知前端刷新。

import type { EntryView, Job, JobLog, MachineView, ProvisionCheckItem, ProvisionItem, SeriesPoint } from '@/lib/types';
import { displayName } from '@/lib/format';
import { buildDb, hashStr, rng, type Db } from './fixtures';
import { installAccessMock } from './access';

const db: Db = buildDb();
const DAY = 86400_000;
const iso = (t = Date.now()) => new Date(t).toISOString();
const GiB = 1024 ** 3;

class HttpErr extends Error { constructor(public status: number, public code: string, msg: string) { super(msg); } }
const bad = (msg: string, status = 400, code = 'bad_request') => new HttpErr(status, code, msg);

// ---------- SSE ----------
type Ev = { type: string; id?: string };
const listeners = new Set<(e: Ev) => void>();
const emit = (e: Ev) => listeners.forEach((l) => l(e));

class MockEventSource {
  readyState = 1; url: string; onmessage: ((e: MessageEvent) => void) | null = null; onerror: (() => void) | null = null; onopen: (() => void) | null = null;
  private subs: ((e: MessageEvent) => void)[] = []; private fn: (e: Ev) => void;
  constructor(url: string) {
    this.url = url;
    this.fn = (e) => { const m = new MessageEvent('message', { data: JSON.stringify(e) }); this.onmessage?.(m); this.subs.forEach((s) => s(m)); };
    listeners.add(this.fn);
    setTimeout(() => this.onopen?.(), 50);
  }
  addEventListener(t: string, cb: (e: MessageEvent) => void) { if (t === 'message') this.subs.push(cb); }
  removeEventListener(t: string, cb: (e: MessageEvent) => void) { if (t === 'message') this.subs = this.subs.filter((s) => s !== cb); }
  close() { this.readyState = 2; listeners.delete(this.fn); }
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
const monthly = (cents: number, cycle: string) => cents / ({ monthly: 1, quarterly: 3, semiannual: 6, yearly: 12, once: 36 }[cycle] ?? 1);

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
    due_7d: live.filter((m) => m.subscription && m.subscription.due_days <= 7).length,
    monthly_cost: Object.entries(live.reduce<Record<string, number>>((acc, m) => { if (m.subscription) acc[m.subscription.currency] = (acc[m.subscription.currency] ?? 0) + Math.round(monthly(m.subscription.price_cents, m.subscription.cycle)); return acc; }, {})).map(([currency, cents]) => ({ currency, cents })),
    traffic_month: { up, down },
    net_rx_bps: live.reduce((s, m) => s + (m.host?.net_rx_bps ?? 0), 0), net_tx_bps: live.reduce((s, m) => s + (m.host?.net_tx_bps ?? 0), 0),
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
      pts.push({ ts, cpu_pct: tail ? null : walk, mem_pct: tail ? null : (h ? (h.mem_used / h.mem_total) * 100 : 40) + Math.sin(i / 9) * 4, disk_pct: h ? (h.disk_used / h.disk_total) * 100 + i * 0.01 : 30, load1: tail ? null : +(walk / 40).toFixed(2), net_rx_bps: tail ? null : (h?.net_rx_bps ?? 1e7) * (0.5 + r()), net_tx_bps: tail ? null : (h?.net_tx_bps ?? 1e7) * (0.4 + r()) });
    } else if (metric === 'traffic') {
      const k = step / 60_000;
      pts.push({ ts, up: Math.round(r() * 8e6 * k * (1 + Math.sin(i / 7))), down: Math.round(r() * 3e7 * k * (1 + Math.cos(i / 8))), conns: Math.round(20 + r() * 80) });
    } else {
      const tcpFail = tail || r() < 0.015; const egFail = tail || r() < 0.02;
      pts.push({ ts, tcp_ok: !tcpFail, tcp_ms: tcpFail ? null : Math.round(40 + r() * 70 + (i % 23 === 0 ? 120 : 0)), egress_ok: !egFail, egress_ms: egFail ? null : Math.round(160 + r() * 90) });
    }
  }
  return { metric, range, resolution: range === '7d' || range === '30d' ? '1h' : '1m', points: pts };
}

// ---------- 作业模拟 ----------
const PSTEPS: [string, string][] = [['ssh', '连接并检查系统'], ['agent', '安装 agent'], ['ss', '配置 Shadowsocks'], ['entry', '下发线路机入口'], ['verify', '验证连通性']];
function newJob(kind: string, target: Job['target'], steps: [string, string][]): Job {
  const id = `j-${db.seq++}`;
  const t = Date.now();
  const j: Job = { id, kind, target, state: 'running', step: 0, step_count: steps.length, cancellable: true, error: null, created_by: db.user, created_at: iso(t), started_at: iso(t), finished_at: null,
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
    if (m.kind !== 'proxy') { m.agent = { version: '1.0.3', last_report_at: iso(), online: true }; m.host = { ts: iso(), cpu_pct: 6, load1: 0.1, mem_used: 400 * 1024 ** 2, mem_total: 4 * GiB, disk_used: 4 * GiB, disk_total: 60 * GiB, net_rx_bps: 2e5, net_tx_bps: 1e5, tcp_conns: 12 }; }
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
      cur.state = 'succeeded'; cur.finished_at = iso(); log(job.id, 'info', `步骤 ${cur.idx + 1}/${job.steps.length} 完成：${cur.title}`);
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
          if (!Number.isInteger(it.number) || it.number < 1 || it.number > 99) errors.push('编号应为 1–99 的整数');
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
    subscription: it.subscription?.due_date ? { price_cents: it.subscription.price_cents ?? plan?.price_cents ?? 0, currency: plan?.currency ?? 'CNY', cycle: it.subscription.cycle ?? plan?.cycle ?? 'monthly', due_date: it.subscription.due_date, due_days: 0, billing_tz: it.subscription.billing_tz ?? 'Asia/Shanghai', auto_renew: false, remind_days: 7, traffic_gb: plan?.traffic_gb || null, traffic_reset_day: 1 } : null,
    agent: null, host: null, entries: [], health: { status: 'unknown', reasons: [{ key: 'provisioning', severity: 'info', text: '上架作业进行中' }] }, active_job: null, open_incidents: 0, maintenance: false,
    relay: it.kind === 'relay' ? { public_host: it.ip, generation: 1, applied_generation: 0, applied_at: null, apply_error: null, port_min: 20001, port_max: 20999 } : null,
  };
  if (m.kind !== 'relay') {
    const relay = machine(it.relay_id!);
    const e: EntryView = { id: `e${db.seq++}`, relay: { id: relay.id, name: relay.name }, node: { id, name: m.name }, port: c.relay_port ?? 20999, lifecycle: 'provisioning', applied: false, apply_error: null, tcp: null, egress: null, traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 } };
    m.entries = [e]; relay.entries.push(e);
  }
  recomputeDue(m);
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
  const p = { id: `p${db.seq++}`, name: body.name, code: String(body.code).toUpperCase(), panel_url: body.panel_url ?? null, timezone: body.timezone ?? null };
  db.providers.push(p); emit({ type: 'catalog' }); return p;
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
  const steps: [string, string][] = [['disable', '停用入口'], ...(body.cleanup ? [['cleanup', '清理节点上的服务'] as [string, string]] : []), ['revoke', '吊销凭据'], ['mark', '标记为已退役']];
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
  recomputeDue(m); emit({ type: 'machine', id: m.id }); emit({ type: 'machines' }); return clone(m.subscription);
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
  const e: EntryView = { id: `e${db.seq++}`, relay: { id: relay.id, name: relay.name }, node: { id: m.id, name: m.name }, port, lifecycle: 'provisioning', applied: false, apply_error: null, tcp: null, egress: null, traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 } };
  m.entries.push(e); relay.entries.push(e);
  const job = newJob('attach_entry', { id: m.id, name: m.name }, [['apply', '下发线路机配置'], ['verify', '验证连通性']]);
  drive(job, 2000, () => { e.lifecycle = 'active'; e.applied = true; });
  emit({ type: 'machines' }); return { job_id: job.id };
});

R('GET', '/jobs', ({ query }) => {
  const s = query.get('state') ?? 'all'; const limit = +(query.get('limit') ?? 100);
  return clone(db.jobs.filter((j) => s === 'all' || (s === 'active' ? j.state === 'running' || j.state === 'queued' : j.state !== 'running' && j.state !== 'queued')).slice(0, limit));
});
const jobOf = (id: string) => { const j = db.jobs.find((x) => x.id === id); if (!j) throw bad('作业不存在', 404, 'not_found'); return j; };
R('GET', '/jobs/([^/]+)', ({ params }) => clone(jobOf(params[0])));
R('GET', '/jobs/([^/]+)/logs', ({ params, query }) => { const after = +(query.get('after') ?? 0); return clone((db.logs[params[0]] ?? []).filter((l) => l.id > after)); });
R('POST', '/jobs/([^/]+)/retry', ({ params }) => {
  const j = jobOf(params[0]); if (j.state !== 'failed') throw bad('只有失败的作业才能重试', 409, 'conflict');
  const f = j.steps.find((s) => s.state === 'failed')!; f.state = 'running'; f.error = null; j.state = 'running'; j.cancellable = true; j.error = null; j.finished_at = null; log(j.id, 'info', '从失败的步骤重试');
  drive(j, 2500, () => { const m = db.machines.find((x) => x.id === j.target?.id); if (m) { m.lifecycle = 'active'; m.active_job = null; } }); emit({ type: 'job', id: j.id }); return {};
});
R('POST', '/jobs/([^/]+)/cancel', ({ params }) => {
  const j = jobOf(params[0]); if (j.state !== 'running') throw bad('作业不在进行中', 409, 'conflict');
  j.state = 'cancelled'; j.cancellable = false; j.finished_at = iso(); j.steps.forEach((s) => { if (s.state === 'running' || s.state === 'pending') s.state = 'skipped'; }); log(j.id, 'warn', '作业已取消，已完成的步骤已补偿'); emit({ type: 'job', id: j.id }); return {};
});

R('GET', '/overview', () => overview());
R('GET', '/incidents', ({ query }) => { const s = query.get('state') ?? 'all'; return clone(db.incidents.filter((i) => s === 'all' || i.state === s)); });
R('POST', '/incidents/([^/]+)/ack', ({ params }) => { const i = db.incidents.find((x) => x.id === params[0]); if (!i) throw bad('告警不存在', 404, 'not_found'); i.ack_at = iso(); i.ack_by = db.user; emit({ type: 'incidents' }); return {}; });
R('GET', '/silences', () => clone(db.silences));
R('POST', '/silences', ({ body }) => { const m = body.subject_id ? machine(body.subject_id) : null; const s = { id: `s${db.seq++}`, subject_name: m?.name ?? null, rule: null, subject_id: null, ...body }; db.silences.push(s); emit({ type: 'incidents' }); return s; });
R('DELETE', '/silences/([^/]+)', ({ params }) => { db.silences = db.silences.filter((s) => s.id !== params[0]); emit({ type: 'incidents' }); return {}; });
R('GET', '/audit', ({ query }) => clone(db.audit.filter((a) => !query.get('target_id') || a.target_id === query.get('target_id')).slice(0, +(query.get('limit') ?? 50))));

installAccessMock(R, () => db.machines);

// ---------- 安装 ----------
function jitter() {
  for (const m of db.machines) if (m.host && m.lifecycle === 'active' && m.health.status !== 'down') {
    const h = m.host; const f = () => 0.85 + Math.random() * 0.3;
    h.cpu_pct = Math.max(1, Math.min(99, h.cpu_pct * f())); h.net_rx_bps *= f(); h.net_tx_bps *= f(); h.ts = iso();
    h.load1 = +(h.cpu_pct / 40).toFixed(2);
    if (m.agent) m.agent.last_report_at = iso();
  }
  emit({ type: 'tick' });
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
  (window as unknown as { EventSource: unknown }).EventSource = MockEventSource;
  setInterval(jitter, 15_000);
  // 演示入口：?logout=1 可模拟未登录
  if (new URLSearchParams(location.search).get('logout') === '1') sessionStorage.setItem('mock:auth', '0');
  else if (sessionStorage.getItem('mock:auth') === null) sessionStorage.setItem('mock:auth', '1');
}
