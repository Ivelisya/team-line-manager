// Mock 数据：约 28 台机器（2 线路机、团队1/团队2 的落地机、1 台无主机家宽代理、1 台故障、1 台退役中、2 台已退役、1 台上架中、
// 团队1-03 上的 1 个同机 IP 节点）。
// IP 只用文档保留段 203.0.113.0/24 与 198.51.100.0/24。

import type { ApiKey, Incident, Job, NotifyChannel, NotifyLogEntry, JobLog, MachineView, Plan, Provider, Renewal, Silence, AuditEntry, Team, EntryView, Lifecycle, HealthStatus, Subscription } from '@/lib/types';
import { addMonths, bytes, displayName, str } from '@/lib/format';
import { quota } from '@/lib/derive';

export function rng(seed: number) {
  let a = seed >>> 0;
  return () => { a = (a + 0x6d2b79f5) >>> 0; let t = a; t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61); return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
}
export const hashStr = (s: string) => { let h = 2166136261; for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619); } return h >>> 0; };

const DAY = 86400_000;
const now = () => Date.now();
const iso = (t: number) => new Date(t).toISOString();
const dateOnly = (t: number) => iso(t).slice(0, 10);

/** 流量额度周期（同服务端 views::period_start）：有重置日取今天或之前最近一次（月份没有这一天按月末），否则本月 1 号；已用按本月业务流量估，
 *  宿主机连同它的同机 IP 节点（含已退役的）一起算（额度共用） */
export function quotaOf(m: MachineView, all: MachineView[]): MachineView['quota'] {
  const d = new Date(); const day = m.subscription?.traffic_reset_day ?? 1;
  const at = (y: number, mo: number) => new Date(Date.UTC(y, mo, Math.min(day, new Date(Date.UTC(y, mo + 1, 0)).getUTCDate())));
  let s = at(d.getFullYear(), d.getMonth());
  if (s.getUTCDate() > d.getDate()) s = at(d.getFullYear(), d.getMonth() - 1);
  let used = 0;
  for (const x of [m, ...all.filter((x) => x.host_machine?.id === m.id)]) {
    used += x.removed_traffic.month_up + x.removed_traffic.month_down;
    for (const e of x.entries) used += e.traffic.month_up + e.traffic.month_down;
  }
  return { used, period_start: s.toISOString().slice(0, 10) };
}

export interface Db {
  user: string;
  teams: Team[]; providers: Provider[]; plans: Plan[];
  machines: MachineView[];
  jobs: Job[]; logs: Record<string, JobLog[]>;
  incidents: Incident[]; silences: Silence[]; audit: AuditEntry[]; renewals: Record<string, Renewal[]>;
  apiKeys: ApiKey[]; channels: (NotifyChannel & { config: Record<string, string> })[]; notifyLog: NotifyLogEntry[];
  /** 同机 IP 节点在宿主机上的 SS 端口（服务端存在 node_outbound，MachineView 里没有） */
  ssPorts: Record<string, number>;
  seq: number;
}

export function buildDb(): Db {
  const r = rng(20261020);
  const teams: Team[] = [
    { id: 't1', name: '团队1', sort_order: 1, node_count: 0 },
    { id: 't2', name: '团队2', sort_order: 2, node_count: 0 },
  ];
  const providers: Provider[] = [
    { id: 'p1', name: '搬瓦工', code: 'BWH', panel_url: 'https://panel.example.com/bwh', timezone: 'America/Los_Angeles' },
    { id: 'p2', name: '腾讯云', code: 'TCT', panel_url: 'https://panel.example.com/tct', timezone: 'Asia/Shanghai' },
    { id: 'p3', name: '家宽服务商', code: 'HOME', panel_url: null, timezone: 'Asia/Shanghai' },
  ];
  const plans: Plan[] = [
    { id: 'pl1', provider_id: 'p1', name: 'CN2 GIA 2C/4G', cpu_cores: 2, mem_mb: 4096, disk_gb: 80, bandwidth_mbps: 1000, traffic_gb: 1000, price_cents: 6900, currency: 'USD', cycle: 'quarterly', notes: '限速 1Gbps', archived_at: null, machine_count: 0 },
    { id: 'pl2', provider_id: 'p1', name: 'CN2 GIA 1C/2G', cpu_cores: 1, mem_mb: 2048, disk_gb: 40, bandwidth_mbps: 1000, traffic_gb: 500, price_cents: 4999, currency: 'USD', cycle: 'quarterly', notes: null, archived_at: null, machine_count: 0 },
    { id: 'pl3', provider_id: 'p2', name: '轻量 2C/4G', cpu_cores: 2, mem_mb: 4096, disk_gb: 60, bandwidth_mbps: 200, traffic_gb: 20000, price_cents: 8800, currency: 'CNY', cycle: 'monthly', notes: '中转专用', archived_at: null, machine_count: 0 },
    { id: 'pl4', provider_id: 'p3', name: '家宽代理 100M', cpu_cores: 0, mem_mb: 0, disk_gb: 0, bandwidth_mbps: 100, traffic_gb: 0, price_cents: 3500, currency: 'CNY', cycle: 'monthly', notes: '按月租用', archived_at: null, machine_count: 0 },
    { id: 'pl5', provider_id: 'p1', name: 'HK 旧款 1C/1G', cpu_cores: 1, mem_mb: 1024, disk_gb: 20, bandwidth_mbps: 100, traffic_gb: 300, price_cents: 2999, currency: 'USD', cycle: 'yearly', notes: '已停售', archived_at: iso(now() - 90 * DAY), machine_count: 0 },
  ];
  const plan = (id: string) => plans.find((p) => p.id === id)!;
  const prov = (id: string) => providers.find((p) => p.id === id)!;

  const machines: MachineView[] = [];
  const entriesByRelay: Record<string, EntryView[]> = { r1: [], r2: [] };
  let eSeq = 1;
  const GiB = 1024 ** 3;

  const sub = (planId: string, dueDays: number, trafficGb: number | null): Subscription => {
    const p = plan(planId);
    return { price_cents: p.price_cents, currency: p.currency, cycle: p.cycle, due_date: dateOnly(now() + dueDays * DAY), due_days: dueDays, billing_tz: prov(p.provider_id).timezone ?? 'Asia/Shanghai', auto_renew: false, remind_days: 7, traffic_gb: trafficGb, traffic_reset_day: 1 };
  };
  const hostOf = (id: string, cpuBase: number): MachineView['host'] => {
    const rr = rng(hashStr(id));
    const memTotal = (2 + Math.floor(rr() * 3)) * 1024 * 1024 * 1024;
    const diskTotal = 60 * GiB;
    return {
      ts: iso(now() - 12_000), cpu_pct: Math.min(99, cpuBase + rr() * 12), load1: +(cpuBase / 40 + rr() * 0.5).toFixed(2),
      mem_used: memTotal * (0.35 + rr() * 0.4), mem_total: memTotal, disk_used: diskTotal * (0.2 + rr() * 0.5), disk_total: diskTotal,
      net_rx_bps: (3 + rr() * 90) * 1e6, net_tx_bps: (2 + rr() * 60) * 1e6, tcp_conns: Math.floor(80 + rr() * 900),
    };
  };
  const base = (o: Partial<MachineView> & Pick<MachineView, 'id' | 'kind' | 'name' | 'ip'>): MachineView => ({
    team: null, number: null, label: null, ssh_port: 22, host_machine: null, ip_nodes: [], provider: null, provider_code: null, plan: null, service_ref: null,
    lifecycle: 'active', lifecycle_reason: null, note: '', revision: 1, created_at: iso(now() - 120 * DAY), activated_at: iso(now() - 100 * DAY), retired_at: null,
    subscription: null, agent: null, host: null, entries: [], removed_traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 }, quota: { used: 0, period_start: '' }, health: { status: 'ok', reasons: [] },
    active_job: null, failed_job: null, open_incidents: 0, maintenance: false, relay: null, ...o,
  });

  // 线路机
  const relayDefs = [
    { id: 'r1', label: '线路机1', ip: '203.0.113.10', host: 'relay1.example.net', gen: 12, applied: 12, planId: 'pl3', due: 19 },
    { id: 'r2', label: '线路机2', ip: '203.0.113.11', host: 'relay2.example.net', gen: 8, applied: 7, planId: 'pl3', due: 5 },
  ];
  for (const d of relayDefs) {
    const p = plan(d.planId);
    machines.push(base({
      id: d.id, kind: 'relay', name: d.label, label: d.label, ip: d.ip, provider: { id: 'p2', name: '腾讯云' }, provider_code: 'TCT',
      plan: { id: p.id, name: p.name, bandwidth_mbps: p.bandwidth_mbps, traffic_gb: p.traffic_gb }, subscription: sub(d.planId, d.due, p.traffic_gb),
      agent: { version: '1.0.3', last_report_at: iso(now() - 8000), online: true, xray_running: true, cores: 2, health: null }, host: hostOf(d.id, d.id === 'r1' ? 32 : 18),
      relay: { public_host: d.host, generation: d.gen, applied_generation: d.applied, applied_at: iso(now() - 2 * 3600_000), apply_error: null, port_min: 20001, port_max: 20999 },
      health: { status: 'ok', reasons: [] },
    }));
  }

  // 节点定义：团队、编号、套餐、状态
  type Def = { team: 't1' | 't2'; n: number; kind: 'vps' | 'proxy'; planId: string; relay: 'r1' | 'r2'; due: number; status?: HealthStatus; online?: boolean; resetDay?: number; lifecycle?: Lifecycle; cpu?: number; octet?: number; provider?: string };
  const defs: Def[] = [];
  const t1Plans = ['pl1', 'pl2', 'pl1', 'pl1', 'pl2', 'pl1', 'pl2', 'pl1', 'pl1', 'pl2', 'pl1', 'pl2'];
  for (let i = 1; i <= 12; i++) defs.push({ team: 't1', n: i, kind: 'vps', planId: t1Plans[i - 1], relay: 'r1', due: [22, 41, 8, 60, 2, 75, 33, 15, 90, 4, 28, 51][i - 1], cpu: 8 + Math.floor(r() * 40), octet: 10 + i });
  const t2Plans = ['pl1', 'pl2', 'pl1', 'pl1', 'pl2', 'pl1', 'pl1', 'pl2', 'pl1', 'pl1'];
  for (let i = 1; i <= 10; i++) defs.push({ team: 't2', n: i, kind: 'vps', planId: t2Plans[i - 1], relay: 'r2', due: [12, 36, 55, 9, 70, 25, 18, 44, 6, 63][i - 1], cpu: 6 + Math.floor(r() * 45), octet: 20 + i });
  defs.push({ team: 't2', n: 11, kind: 'proxy', planId: 'pl4', relay: 'r2', due: 11, octet: 41, provider: 'p3' });
  // 状态覆盖
  const ov = (team: string, n: number, patch: Partial<Def>) => Object.assign(defs.find((d) => d.team === team && d.n === n)!, patch);
  ov('t1', 5, { status: 'down' }); ov('t2', 3, { status: 'down', online: true }); ov('t1', 3, { resetDay: 15 }); ov('t1', 4, { resetDay: 31 }); ov('t1', 9, { status: 'degraded', cpu: 91 }); ov('t2', 4, { status: 'maintenance' });
  ov('t2', 7, { lifecycle: 'retiring', status: 'inactive' }); ov('t2', 9, { status: 'unknown' });
  ov('t2', 10, { lifecycle: 'retiring', status: 'inactive' }); // 退役作业失败（j-retfail），停在退役中
  ov('t1', 10, { cpu: 86 });

  for (const d of defs) {
    const team = teams.find((t) => t.id === d.team)!;
    const id = `m-${d.team}-${String(d.n).padStart(2, '0')}`;
    const name = displayName(team.name, d.n);
    const p = plan(d.planId);
    const pv = prov(d.provider ?? p.provider_id);
    const relay = machines.find((m) => m.id === d.relay)!;
    const port = 20000 + entriesByRelay[d.relay].length + 1;
    const status: HealthStatus = d.status ?? 'ok';
    const down = status === 'down'; const offline = down && !d.online; // 故障但 agent 照常上报（如出口不通）
    const lifecycle: Lifecycle = d.lifecycle ?? 'active';
    const entry: EntryView = {
      id: `e${eSeq++}`, relay: { id: relay.id, name: relay.name }, node: { id, name }, direct: false, port,
      lifecycle: lifecycle === 'retiring' ? 'disabled' : 'active', applied: true, apply_error: null,
      tcp: { ok: !offline, latency_ms: offline ? null : Math.round(18 + r() * 160), detail: offline ? '连接超时' : null, ts: iso(now() - 20_000) },
      egress: { ok: !down, latency_ms: down ? null : Math.round(120 + r() * 220), detail: down ? '出口探测失败' : null, ts: iso(now() - 40_000) },
      traffic: { today_up: Math.round((0.3 + r() * 6) * GiB), today_down: Math.round((0.5 + r() * 14) * GiB), month_up: Math.round((20 + r() * 220) * GiB), month_down: Math.round((60 + r() * 520) * GiB) },
    };
    if (d.kind === 'proxy') entry.egress = { ok: true, latency_ms: 210, detail: null, ts: iso(now() - 30_000) };
    entriesByRelay[d.relay].push(entry);
    const reasons = offline ? [{ key: 'agent_offline', severity: 'critical', text: '14 分钟没有上报' }, { key: 'tcp_fail', severity: 'critical', text: '线路机1 到节点 TCP 不通' }]
      : down ? [{ key: 'egress_fail', severity: 'critical', text: `${relay.name}:${port} 出口不通：timeout` }]
      : status === 'degraded' ? [{ key: 'cpu_high', severity: 'warning', text: 'CPU 持续高于 90%' }]
      : status === 'maintenance' ? [{ key: 'maintenance', severity: 'info', text: '维护窗口内，告警已静默' }]
      : status === 'unknown' ? [{ key: 'no_data', severity: 'warning', text: '尚无探测数据' }] : [];
    const m = base({
      id, kind: d.kind, name, team: { id: team.id, name: team.name }, number: d.n, ip: `${d.team === 't1' ? '198.51.100' : '203.0.113'}.${d.octet ?? 50}`,
      provider: { id: pv.id, name: pv.name }, provider_code: pv.code, plan: { id: p.id, name: p.name, bandwidth_mbps: p.bandwidth_mbps, traffic_gb: p.traffic_gb },
      service_ref: `SVC-${1000 + d.n + (d.team === 't2' ? 40 : 0)}`, lifecycle, lifecycle_reason: lifecycle === 'retiring' ? '客户退租' : null,
      subscription: { ...sub(d.planId, d.due, p.traffic_gb || null), traffic_reset_day: d.resetDay ?? 1 },
      agent: d.kind === 'proxy' ? null : { version: '1.0.3', last_report_at: iso(now() - (offline ? 14 * 60_000 : 9000)), online: !offline && status !== 'unknown', xray_running: null, cores: 2, health: null },
      host: d.kind === 'proxy' || status === 'unknown' ? null : hostOf(id, d.cpu ?? 20),
      entries: [entry], health: { status, reasons }, maintenance: status === 'maintenance', open_incidents: offline ? 2 : down ? 1 : status === 'degraded' ? 1 : 0,
      note: d.n === 1 ? '主力线路，勿动' : '',
    });
    if (offline && m.host) { m.host.ts = iso(now() - 14 * 60_000); }
    if (id === 'm-t2-06' && m.host) m.host.load1 = null; // LXC 容器：agent 拿不到本机负载
    machines.push(m);
  }

  // 上架中（运行中的作业，步骤展示用）
  {
    const p = plan('pl1'); const team = teams[0];
    const id = 'm-t1-13p';
    const e: EntryView = { id: `e${eSeq++}`, relay: { id: 'r1', name: '线路机1' }, node: { id, name: displayName(team.name, 13) }, direct: false, port: 20000 + entriesByRelay.r1.length + 1, lifecycle: 'pending', applied: false, apply_error: null, tcp: null, egress: null, traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 } };
    entriesByRelay.r1.push(e);
    machines.push(base({
      id, kind: 'vps', name: displayName(team.name, 13), team: { id: team.id, name: team.name }, number: 13, ip: '198.51.100.77', lifecycle: 'provisioning', activated_at: null, created_at: iso(now() - 6 * 60_000),
      provider: { id: 'p1', name: '搬瓦工' }, provider_code: 'BWH', plan: { id: p.id, name: p.name, bandwidth_mbps: p.bandwidth_mbps, traffic_gb: p.traffic_gb },
      entries: [e], health: { status: 'unknown', reasons: [{ key: 'provisioning', severity: 'info', text: '上架作业进行中' }] },
      active_job: { id: 'j-run', kind: 'provision_vps', state: 'running' },
    }));
  }
  // 上架失败（j-fail：主机指纹不符）
  {
    const p = plan('pl1'); const team = teams[1];
    machines.push(base({
      id: 'x1', kind: 'vps', name: displayName(team.name, 12), team: { id: team.id, name: team.name }, number: 12, ip: '203.0.113.77', lifecycle: 'failed', lifecycle_reason: '上架作业失败', activated_at: null, created_at: iso(now() - 95 * 60_000),
      provider: { id: 'p1', name: '搬瓦工' }, provider_code: 'BWH', plan: { id: p.id, name: p.name, bandwidth_mbps: p.bandwidth_mbps, traffic_gb: p.traffic_gb },
      health: { status: 'inactive', reasons: [] }, failed_job: { id: 'j-fail', kind: 'provision_vps', state: 'failed' },
    }));
  }
  // 已退役 ×2
  for (const [tid, n, ip, days] of [['t1', 3, '198.51.100.88', 40], ['t2', 5, '203.0.113.99', 12]] as const) {
    const team = teams.find((t) => t.id === tid)!; const p = plan('pl2');
    machines.push(base({
      id: `m-${tid}-${n}r`, kind: 'vps', name: displayName(team.name, n), team: { id: team.id, name: team.name }, number: n, ip, lifecycle: 'retired', lifecycle_reason: n === 3 ? '线路淘汰' : '供应商到期未续',
      retired_at: iso(now() - days * DAY), provider: { id: 'p1', name: '搬瓦工' }, provider_code: 'BWH', plan: { id: p.id, name: p.name, bandwidth_mbps: p.bandwidth_mbps, traffic_gb: p.traffic_gb },
      health: { status: 'inactive', reasons: [] },
    }));
  }
  // 同机 IP 节点：团队1-03 上另配的一个 IP。SSH 端口、供应商、agent 与主机读数是宿主机的（agent、host 与宿主机同一个对象，读数一起变），
  // 不带套餐、默认不记费用；SS 端口 8389
  {
    const h = machines.find((m) => m.id === 'm-t1-03')!; const team = teams[0];
    const id = 'm-t1-14'; const name = displayName(team.name, 14);
    const e: EntryView = {
      id: `e${eSeq++}`, relay: { id: 'r1', name: '线路机1' }, node: { id, name }, direct: false, port: 20000 + entriesByRelay.r1.length + 1, lifecycle: 'active', applied: true, apply_error: null,
      tcp: { ok: true, latency_ms: 47, detail: null, ts: iso(now() - 20_000) }, egress: { ok: true, latency_ms: 214, detail: null, ts: iso(now() - 40_000) },
      traffic: { today_up: Math.round(1.1 * GiB), today_down: Math.round(3.6 * GiB), month_up: Math.round(46 * GiB), month_down: Math.round(128 * GiB) },
    };
    entriesByRelay.r1.push(e);
    machines.push(base({
      id, kind: 'vps', name, team: { id: team.id, name: team.name }, number: 14, ip: '198.51.100.66', ssh_port: h.ssh_port, host_machine: { id: h.id, name: h.name },
      provider: h.provider, provider_code: h.provider_code, activated_at: iso(now() - 9 * DAY), agent: h.agent, host: h.host, entries: [e],
    }));
    h.ip_nodes = [{ id, name }];
  }

  // 把入口挂到线路机上
  for (const rl of ['r1', 'r2'] as const) {
    const m = machines.find((x) => x.id === rl)!;
    m.entries = entriesByRelay[rl];
    if (rl === 'r2') { const last = m.entries[m.entries.length - 1]; last.applied = false; last.apply_error = null; }
  }
  // 线路机 2 的最后一个入口还没应用 → 降级提示
  machines[1].health = { status: 'degraded', reasons: [{ key: 'relay_apply_lag', severity: 'warning', text: '配置未应用（配置代数 8 · 已应用 7）' }] };
  machines[1].host = hostOf('r2', 58);

  for (const m of machines) m.quota = quotaOf(m, machines);
  // 统计
  for (const t of teams) t.node_count = machines.filter((m) => m.team?.id === t.id && m.lifecycle !== 'retired').length;
  for (const p of plans) p.machine_count = machines.filter((m) => m.plan?.id === p.id && m.lifecycle !== 'retired').length;

  // 作业：步骤输出与服务端同形
  const STEP_OUT: Record<string, Record<string, unknown>> = {
    preflight: { host_key: 'SHA256:q3Zp1vJm0cJ2oWfX8sB9tqk1eYb7rUo5nHj2LdPaVx4', arch: 'x86_64', os: 'debian-12', kernel: '6.1.0-26-amd64', mem_mb: '3911' },
    agent: { binary_updated: true, token_id: '0192a0c4-0000-7000-8000-000000000001' },
    converge: { bbr: true, nft_relays: '203.0.113.10, 203.0.113.11', ssserver_updated: false },
    attach: { entry_id: '0192a0c4-0000-7000-8000-000000000002', port: 20014, generation: 9 },
    verify: { latency_ms: 189.6, egress_ip: '198.51.100.23' },
    detach: { entries: 2 }, cleanup: { ss: 'gone' }, revoke: { revoked: 1 }, finalize: { renumbered: 3 },
  };
  const t0 = now();
  const mk = (id: string, kind: string, target: Job['target'], state: Job['state'], stepsDef: [string, string][], doneUntil: number, ageMin: number, err?: string): Job => {
    const created = t0 - ageMin * 60_000;
    return {
      id, kind, target, state, step: Math.min(doneUntil, stepsDef.length - 1), step_count: stepsDef.length, step_title: null, cancellable: state === 'running', error: err ?? null, created_by: 'admin', created_at: iso(created), started_at: iso(created + 2000), finished_at: state === 'running' ? null : iso(created + 120_000),
      steps: stepsDef.map(([key, title], i) => ({
        idx: i, key, title,
        state: i < doneUntil ? 'done' : i === doneUntil ? (state === 'running' ? 'running' : state === 'failed' ? 'failed' : 'pending') : 'pending',
        started_at: i <= doneUntil ? iso(created + 2000 + i * 18_000) : null, finished_at: i < doneUntil ? iso(created + 2000 + (i + 1) * 18_000) : null,
        output: i < doneUntil ? STEP_OUT[key] ?? null : null,
        error: i === doneUntil && state === 'failed' ? err ?? '失败' : null,
      })),
    };
  };
  const PSTEPS: [string, string][] = [['preflight', '连接并检查系统'], ['agent', '安装 agent'], ['converge', '配置 Shadowsocks'], ['attach', '接入线路机'], ['verify', '验证连通性']];
  const RSTEPS: [string, string][] = [['detach', '停用入口'], ['cleanup', '清理节点上的服务'], ['revoke', '撤销令牌'], ['finalize', '标记为已退役']];
  const jobs: Job[] = [
    mk('j-run', 'provision_vps', { id: 'm-t1-13p', name: '团队1-13' }, 'running', PSTEPS, 2, 6),
    mk('j-fail', 'provision_vps', { id: 'x1', name: '团队2-12' }, 'failed', PSTEPS, 0, 95, '主机指纹与记录的不一致：203.0.113.77 可能重装过'),
    mk('j-ret', 'retire', { id: 'm-t2-07', name: '团队2-07' }, 'running', RSTEPS, 1, 3),
    mk('j-retfail', 'retire', { id: 'm-t2-10', name: '团队2-10' }, 'failed', RSTEPS, 1, 30, 'SSH 连接超时：203.0.113.30:22'),
    mk('j-done1', 'retire', { id: 'm-t2-5r', name: '团队2-05' }, 'succeeded', RSTEPS, 4, 60 * 24 * 12),
    mk('j-done2', 'provision_vps', { id: 'm-t1-12', name: '团队1-12' }, 'succeeded', PSTEPS, 5, 60 * 24 * 30),
    mk('j-done3', 'retire', { id: 'm-t1-3r', name: '团队1-03' }, 'succeeded', RSTEPS, 4, 60 * 24 * 40),
  ];
  jobs[0].steps[3].state = 'pending';
  const failStep = (id: string, kind: string) => { const j = jobs.find((x) => x.id === id)!; j.steps.find((x) => x.state === 'failed')!.error_kind = kind; return j; };
  failStep('j-fail', 'host_key').cancellable = true; // 失败的上架作业可以取消（机器退役并清理）
  failStep('j-retfail', 'network');
  machines.find((m) => m.id === 'm-t2-10')!.failed_job = { id: 'j-retfail', kind: 'retire', state: 'failed' };
  // 已取消并补偿的上架、升级 agent 作业，让步骤条的“已补偿 / 已跳过”和 upgrade_agent 都有样本
  const cj = mk('j-cancel', 'provision_vps', { id: 'x2', name: '团队2-14' }, 'cancelled', PSTEPS, 3, 180);
  cj.steps.forEach((x, i) => { x.state = i < 3 ? 'compensated' : 'skipped'; if (i >= 3) { x.started_at = null; x.finished_at = null; } });
  const uj = mk('j-up', 'upgrade_agent', { id: 'r1', name: '线路机1' }, 'succeeded', [['push', '推送 agent'], ['restart', '重启 agent'], ['verify', '确认上报']], 3, 60 * 5);
  jobs.splice(3, 0, cj, uj);
  // 同机 IP 节点的上架：先在宿主机上核对 IP，收敛不做系统基线（bbr 为 null）
  const ij = mk('j-ip', 'provision_ip', { id: 'm-t1-14', name: '团队1-14' }, 'succeeded', IP_STEPS, 6, 60 * 24 * 9);
  ij.steps[0].output = { ss_port: 8389, local_ips: '198.51.100.13, 198.51.100.66', egress_ip: '198.51.100.66' };
  ij.steps[1].output = { bbr: null, nft_relays: '203.0.113.10', ssserver_updated: false };
  ij.steps[4].output = { latency_ms: 214.2, egress_ip: '198.51.100.66' };
  jobs.push(ij);
  const logs: Record<string, JobLog[]> = {};
  const logT = (jobId: string, lines: [string, string][]) => {
    const j = jobs.find((x) => x.id === jobId)!; const c = new Date(j.created_at).getTime();
    logs[jobId] = lines.map(([level, msg], i) => ({ id: i + 1, ts: iso(c + 2000 + i * 7000), level, msg }));
  };
  logT('j-run', [['info', '开始上架 团队1-13 (198.51.100.77)'], ['info', '连接 198.51.100.77:22 ...'], ['info', '系统: Debian 12 x86_64，内存 4096 MB'], ['info', '步骤 1/5 完成'], ['info', '下载 agent 1.0.3 ...'], ['info', '注册 agent，令牌已写入 /etc/fleet/agent.toml'], ['info', '步骤 2/5 完成'], ['info', '写入 /etc/ss/config.json，端口 8388'], ['warn', 'ufw 未启用，跳过防火墙规则'], ['info', '启动 ss-server ...']]);
  logT('j-fail', [['info', '开始上架 团队2-12 (203.0.113.77)'], ['info', '连接 203.0.113.77:22 ...'], ['error', '主机指纹与记录的不一致：203.0.113.77 可能重装过']]);
  logT('j-retfail', [['info', '开始退役 团队2-10'], ['info', '停用入口 线路机2:20010，已应用'], ['info', '清理节点上的服务 ...'], ['error', 'SSH 连接超时：203.0.113.30:22']]);
  logT('j-ret', [['info', '开始退役 团队2-07'], ['info', '停用入口 线路机2:20007，已应用'], ['info', '清理节点上的服务 ...']]);
  logT('j-done1', [['info', '退役完成']]); logT('j-done2', [['info', '上架完成']]); logT('j-done3', [['info', '退役完成']]);
  logT('j-ip', [['info', '在宿主机 团队1-03 上核对 IP 198.51.100.66'], ['info', '宿主机本机 IPv4：198.51.100.13, 198.51.100.66'], ['info', '经 198.51.100.66 访问中控，回显来源 198.51.100.66'], ['info', '装好 fleet-ssserver@8389 与防火墙表 ss_guard_8389'], ['info', '上架完成']]);

  const incidents: Incident[] = [
    { id: 'i1', subject: { id: 'm-t1-05', name: '团队1-05' }, subject_id: 'm-t1-05', subject_name: '团队1-05', rule: 'agent_offline', rule_name: 'agent 离线', severity: 'critical', state: 'firing', summary: 'agent 已离线 14 分钟', opened_at: iso(now() - 14 * 60_000), resolved_at: null, last_seen_at: iso(now() - 60_000), ack_at: null, ack_by: null, reopen_count: 0 },
    { id: 'i2', subject: { id: 'm-t1-05', name: '团队1-05' }, subject_id: 'm-t1-05', subject_name: '团队1-05', rule: 'tcp_fail', rule_name: '线路到节点不通', severity: 'critical', state: 'firing', summary: '线路机到节点 TCP 探测连续失败 5 次', opened_at: iso(now() - 12 * 60_000), resolved_at: null, last_seen_at: iso(now() - 60_000), ack_at: iso(now() - 5 * 60_000), ack_by: 'admin', reopen_count: 0 },
    { id: 'i3', subject: { id: 'm-t1-09', name: '团队1-09' }, subject_id: 'm-t1-09', subject_name: '团队1-09', rule: 'egress_fail', rule_name: '出口不通', severity: 'warning', state: 'firing', summary: '出口验证连续失败 3 次（经线路机1:20009 访问外网超时）', opened_at: iso(now() - 50 * 60_000), resolved_at: null, last_seen_at: iso(now() - 60_000), ack_at: null, ack_by: null, reopen_count: 2 },
    { id: 'i4', subject: { id: 'r2', name: '线路机2' }, subject_id: 'r2', subject_name: '线路机2', rule: 'relay_apply_lag', rule_name: '配置未应用', severity: 'warning', state: 'firing', summary: '线路机2：配置未应用（配置代数 8 · 已应用 7）', opened_at: iso(now() - 25 * 60_000), resolved_at: null, last_seen_at: iso(now() - 60_000), ack_at: null, ack_by: null, reopen_count: 0 },
    { id: 'i5', subject: { id: 'm-t2-03', name: '团队2-03' }, subject_id: 'm-t2-03', subject_name: '团队2-03', rule: 'renewal_due', rule_name: '即将到期', severity: 'info', state: 'resolved', summary: '7 天内到期，请安排续费', opened_at: iso(now() - 26 * 3600_000), resolved_at: iso(now() - 25 * 3600_000), last_seen_at: iso(now() - 60_000), ack_at: null, ack_by: null, reopen_count: 0 },
    // 已恢复的历史（抽屉「最近告警」）：团队1-05 昨天出口断过半小时、来回反复，三天前 CPU 偏高；团队1-09 六天前 TCP 断过
    { id: 'i6', subject: { id: 'm-t1-05', name: '团队1-05' }, subject_id: 'm-t1-05', subject_name: '团队1-05', rule: 'egress_fail', rule_name: '出口不通', severity: 'critical', state: 'resolved', summary: '出口验证连续失败 3 次（经线路机1:20005 访问外网超时）', opened_at: iso(now() - 26 * 3600_000), resolved_at: iso(now() - 26 * 3600_000 + 28 * 60_000), last_seen_at: iso(now() - 26 * 3600_000 + 27 * 60_000), ack_at: null, ack_by: null, reopen_count: 2 },
    { id: 'i7', subject: { id: 'm-t1-05', name: '团队1-05' }, subject_id: 'm-t1-05', subject_name: '团队1-05', rule: 'host_cpu_high', rule_name: 'CPU 持续偏高', severity: 'warning', state: 'resolved', summary: 'CPU 持续 10 分钟高于 90%（最高 97%）', opened_at: iso(now() - 3 * DAY), resolved_at: iso(now() - 3 * DAY + 95 * 60_000), last_seen_at: iso(now() - 3 * DAY + 94 * 60_000), ack_at: iso(now() - 3 * DAY + 20 * 60_000), ack_by: 'admin', reopen_count: 0 },
    { id: 'i8', subject: { id: 'm-t1-09', name: '团队1-09' }, subject_id: 'm-t1-09', subject_name: '团队1-09', rule: 'tcp_fail', rule_name: '线路到节点不通', severity: 'critical', state: 'resolved', summary: '线路机到节点 TCP 探测连续失败 5 次', opened_at: iso(now() - 6 * DAY), resolved_at: iso(now() - 6 * DAY + 12 * 60_000), last_seen_at: iso(now() - 6 * DAY + 11 * 60_000), ack_at: null, ack_by: null, reopen_count: 0 },
  ];
  // 流量额度吃紧（traffic_quota_high，同服务端规则）：本周期已用 ≥ 90% 警告、≥ 100% 严重；开始时间错开几小时
  let tight = 0;
  for (const m of machines) {
    const q = quota(m);
    if (m.lifecycle !== 'active' || q.pct == null || q.pct < 90) continue;
    const pct = Math.floor(q.pct);
    incidents.push({ id: `iq-${m.id}`, subject: { id: m.id, name: m.name }, subject_id: m.id, subject_name: m.name, rule: 'traffic_quota_high', rule_name: '流量额度吃紧', severity: pct >= 100 ? 'critical' : 'warning', state: 'firing',
      summary: `本周期已用 ${pct}%（${str(bytes(q.used))} / ${q.limit} GiB，${q.start.slice(5)} 起）`, opened_at: iso(now() - (2 + 3 * tight++) * 3600_000), resolved_at: null, last_seen_at: iso(now() - 60_000), ack_at: null, ack_by: null, reopen_count: 0 });
    m.open_incidents++;
  }
  const silences: Silence[] = [{ id: 's1', subject_id: 'm-t2-04', subject_name: '团队2-04', rule: null, starts_at: iso(now() - 3600_000), ends_at: iso(now() + 3 * 3600_000), is_maintenance: true, comment: '换内核，预计 4 小时' }];
  const audit: AuditEntry[] = [
    { id: 'a3', ts: iso(now() - 25 * 60_000), actor: '运维 CLI', action: 'exec', title: '执行命令', summary: '团队1-03 · journalctl -u fleet-agent --since "1 hour ago" --no-pager | grep -i -E "err… · 退出码 0', target_kind: 'machine', target_id: 'm-t1-03', detail: { name: '团队1-03', command: 'journalctl -u fleet-agent --since "1 hour ago" --no-pager | grep -i -E "error|warn" | tail -n 200', exit_code: 0 } },
    { id: 'a1', ts: iso(now() - 40 * 60_000), actor: 'admin', action: 'machine.rename', title: '改名', summary: '团队1-07 → 团队1-03', target_kind: 'machine', target_id: 'm-t1-03', detail: { old: '团队1-07', new: '团队1-03' } },
    { id: 'a2', ts: iso(now() - 3 * 3600_000), actor: 'admin', action: 'renewal.create', title: '登记续费', summary: '团队2-09 到期日顺延 3 个月', target_kind: 'machine', target_id: 'm-t2-09', detail: { months: 3 } },
    { id: 'a4', ts: iso(now() - 12 * DAY), actor: 'admin', action: 'machine.provision', title: '上架', summary: '团队1-07 · 198.51.100.13', target_kind: 'machine', target_id: 'm-t1-03', detail: { name: '团队1-07', ip: '198.51.100.13', kind: 'vps' } },
  ];
  const apiKeys: ApiKey[] = [
    { id: 'k1', name: '备份脚本', prefix: 'a3f9c2d1', scopes: ['read'], created_at: iso(now() - 40 * DAY), expires_at: iso(now() + 50 * DAY), last_used_at: iso(now() - 2 * 3600_000), revoked_at: null },
    { id: 'k2', name: '运维 CLI', prefix: '7be40c18', scopes: ['read', 'write', 'secrets', 'exec'], created_at: iso(now() - 9 * DAY), expires_at: null, last_used_at: iso(now() - 12 * 60_000), revoked_at: null },
    { id: 'k3', name: '旧监控（已停用）', prefix: '19d8e0aa', scopes: ['read'], created_at: iso(now() - 120 * DAY), expires_at: iso(now() - 30 * DAY), last_used_at: iso(now() - 31 * DAY), revoked_at: iso(now() - 33 * DAY) },
  ];
  const channels: Db['channels'] = [
    { id: 'c1', name: '运维群', kind: 'telegram', min_severity: 'warning', enabled: true, created_at: iso(now() - 20 * DAY), config_hint: 'chat -1001234567', config: { bot_token: '123:abc', chat_id: '-1001234567' } },
    { id: 'c2', name: '值班企业微信', kind: 'wecom', min_severity: 'critical', enabled: true, created_at: iso(now() - 12 * DAY), config_hint: 'qyapi.weixin.qq.com', config: { url: 'https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=x' } },
    { id: 'c3', name: '自建告警网关', kind: 'webhook', min_severity: 'info', enabled: false, created_at: iso(now() - 3 * DAY), config_hint: 'hooks.example.com', config: { url: 'https://hooks.example.com/fleet' } },
  ];
  const notifyLog: NotifyLogEntry[] = [
    { id: 'n1', channel_id: 'c1', channel_name: '运维群', incident_id: 'i1-0000-aaaa', ts: iso(now() - 14 * 60_000), ok: true, error: null, kind: 'firing', subject_name: '团队1-04', rule_name: '出口不通' },
    { id: 'n2', channel_id: 'c2', channel_name: '值班企业微信', incident_id: 'i1-0000-aaaa', ts: iso(now() - 14 * 60_000), ok: true, error: null, kind: 'firing', subject_name: '团队1-04', rule_name: '出口不通' },
    { id: 'n3', channel_id: 'c1', channel_name: '运维群', incident_id: 'i3-0000-bbbb', ts: iso(now() - 50 * 60_000), ok: false, error: 'Telegram 返回 429：请求过于频繁，请 17 秒后重试', kind: 'resolved', subject_name: '线路机2', rule_name: '配置未应用' },
    { id: 'n4', channel_id: 'c3', channel_name: '自建告警网关', incident_id: null, ts: iso(now() - 3 * DAY), ok: true, error: null, kind: 'test', subject_name: null, rule_name: null },
    { id: 'n5', channel_id: 'c1', channel_name: '运维群', incident_id: 'i9-0000-cccc', ts: iso(now() - 4 * DAY), ok: true, error: null, kind: 'repeat', subject_name: null, rule_name: 'agent 离线' },
  ];
  const s3 = machines.find((m) => m.id === 'm-t1-03')!.subscription!;
  const renewals: Record<string, Renewal[]> = { 'm-t1-03': [{ id: 'rn1', old_due: addMonths(s3.due_date, -3), new_due: s3.due_date, amount_cents: s3.price_cents, note: '账单 #2041', actor: 'admin', created_at: iso(now() - 84 * DAY) }] };
  return { user: 'admin', teams, providers, plans, machines, jobs, logs, incidents, silences, audit, renewals, apiKeys, channels, notifyLog, ssPorts: { 'm-t1-14': 8389 }, seq: 100 };
}

/** 同机 IP 节点上架 / 恢复的步骤（同服务端 jobs::PROVISION_IP） */
export const IP_STEPS: [string, string][] = [['ip_check', '在宿主机上核对 IP'], ['converge', '节点收敛（SS 实例、防火墙）'], ['attach', '接入线路机'], ['wait_applied', '等待线路机应用'], ['verify', '出口验收'], ['activate', '上线']];
