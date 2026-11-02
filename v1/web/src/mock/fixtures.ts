// Mock 数据：约 28 台机器（2 线路机、团队1/团队2 的 VPS 节点、1 台无主机家宽代理、1 台故障、1 台退役中、3 台备用、1 台上架中、
// 团队1-03 上的 1 个同机 IP 节点），外加回收站里的几条记录。
// IP 只用文档保留段 203.0.113.0/24 与 198.51.100.0/24。

import type { ApiKey, Incident, Job, NotifyChannel, NotifyLogEntry, JobLog, MachineView, Plan, Provider, Renewal, Silence, AuditEntry, Team, EntryView, Lifecycle, HealthStatus, Subscription, SubTemplate, SubField, UsdTotal, RecycleDetail } from '@/lib/types';
import { addDays, addMonths, displayName, str } from '@/lib/format';
import { inheritedValue, quota, quotaGb } from '@/lib/derive';
import { META, subCalc } from './meta';

/** mock 的国家：内网地址查不到（同中控的离线 IP 库），其余按地址末段分到几个国家 */
export const countryOf = (ip: string) => (/^(10|192\.168|127)\./.test(ip) || ip.includes(':') ? null : ['US', 'US', 'US', 'JP', 'HK', 'SG', 'DE'][Number(ip.split('.').pop()) % 7]);

export function rng(seed: number) {
  let a = seed >>> 0;
  return () => { a = (a + 0x6d2b79f5) >>> 0; let t = a; t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61); return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
}
export const hashStr = (s: string) => { let h = 2166136261; for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619); } return h >>> 0; };

const DAY = 86400_000;
const now = () => Date.now();
const iso = (t: number) => new Date(t).toISOString();
const dateOnly = (t: number) => iso(t).slice(0, 10);

/** 接入前的面板读数（割接遗留，字节）：线路机1 本周期含 400 GB，演示额度格的「含接入前」 */
const OPENING: Record<string, number> = { r1: 400e9 };
/** 自己有 agent 的机器（线路机、VPS、宿主机）的网卡流量：业务流量 ×1.08（加密与包头开销），宿主机连同它的同机 IP 节点一起算；家宽代理、同机 IP 节点没有 */
export function nicOf(m: MachineView, all: MachineView[]): MachineView['nic'] {
  if (m.kind === 'proxy' || m.host_machine || !m.agent) return null;
  let tu = 0; let td = 0; let mu = 0; let md = 0;
  for (const x of [m, ...all.filter((x) => x.host_machine?.id === m.id)]) {
    const r = x.removed_traffic; tu += r.today_up; td += r.today_down; mu += r.month_up; md += r.month_down;
    for (const e of x.entries) { tu += e.traffic.today_up; td += e.traffic.today_down; mu += e.traffic.month_up; md += e.traffic.month_down; }
  }
  const k = (n: number) => Math.round(n * 1.08);
  return { today_rx: k(td), today_tx: k(tu), month_rx: k(md), month_tx: k(mu) };
}
/** 数据从哪天起才有：上架晚于起点（YYYY-MM-DD）时是上架那天，否则 null */
const countedFrom = (m: MachineView, since: string) => (m.activated_at && dateOnly(Date.parse(m.activated_at)) > since ? dateOnly(Date.parse(m.activated_at)) : null);

/** 流量额度周期（同服务端 views::period_start）：有重置日取今天或之前最近一次（月份没有这一天按月末），否则本月 1 号；搬瓦工的机器（没单独设置重置日的）是滚动 30 天，
 *  起点 = 今天往前 29 天、没有下次重置日。已用：有网卡读数的机器按供应商规则从网卡算，家宽代理与同机 IP 节点是业务流量（宿主机连同它的同机 IP 节点含已退役的一起算）；
 *  加上接入前读数（OPENING） */
/** 近 7 天日均与预计用满（服务端算法的简化：用周期内已过的天数求日均，不足 1 天不预测） */
function forecastOf(used: number, limit: number | null | undefined, start: string, end: Date | null): Pick<MachineView['quota'], 'daily_avg' | 'forecast_full_at'> {
  const days = (Date.now() - Date.parse(start)) / 864e5;
  if (days < 1) return { daily_avg: null, forecast_full_at: null };
  const daily = used / days;
  const left = limit && limit > 0 && daily > 0 ? ((limit * 1e9 - used) / daily) * 864e5 : null;
  const full = left != null && left > 0 && end && Date.now() + left < end.getTime() ? new Date(Math.floor((Date.now() + left) / 36e5) * 36e5).toISOString() : null;
  return { daily_avg: Math.round(daily), forecast_full_at: full };
}
export function quotaOf(m: MachineView, all: MachineView[]): MachineView['quota'] {
  // 按网卡计量的机器：订阅里的生效规则（单独设置的，或供应商的）；没有订阅的取供应商的
  const rule = m.kind === 'proxy' || m.host_machine ? null : m.subscription?.traffic_rule ?? ({ p1: 'min', p2: 'out' } as Record<string, MachineView['quota']['rule']>)[m.provider?.id ?? ''] ?? 'max';
  const d = new Date(); const day = m.subscription?.traffic_reset_day ?? 1;
  const at = (y: number, mo: number) => new Date(Date.UTC(y, mo, Math.min(day, new Date(Date.UTC(y, mo + 1, 0)).getUTCDate())));
  let s = at(d.getFullYear(), d.getMonth());
  if (s.getUTCDate() > d.getDate()) s = at(d.getFullYear(), d.getMonth() - 1);
  const rolling = m.provider?.id === 'p1' && day === 1;
  if (rolling) s = new Date(Date.UTC(d.getFullYear(), d.getMonth(), d.getDate() - 29));
  const nic = nicOf(m, all);
  let used = 0;
  if (nic && rule) used = rule === 'min' ? Math.min(nic.month_rx, nic.month_tx) : rule === 'max' ? Math.max(nic.month_rx, nic.month_tx) : rule === 'out' ? nic.month_tx : rule === 'in' ? nic.month_rx : nic.month_rx + nic.month_tx;
  else for (const x of [m, ...all.filter((x) => x.host_machine?.id === m.id)]) {
    used += x.removed_traffic.month_up + x.removed_traffic.month_down;
    for (const e of x.entries) used += e.traffic.month_up + e.traffic.month_down;
  }
  const base = OPENING[m.id] ?? null; const start = s.toISOString().slice(0, 10);
  const limit = m.subscription?.traffic_gb ?? m.plan?.traffic_gb ?? null;
  return {
    used: used + (base ?? 0), period_start: start, period_end: rolling ? null : at(d.getFullYear(), d.getMonth() + (s.getUTCMonth() === d.getMonth() ? 1 : 0)).toISOString().slice(0, 10),
    mode: rolling ? 'rolling30' : 'monthly', counted_from: base == null ? countedFrom(m, start) : null, rule, base,
    limit_gb: limit && limit > 0 ? limit : null, limit_source: m.host_machine ? 'host' : limit && limit > 0 ? (m.subscription?.traffic_gb != null ? 'subscription' : 'plan') : 'none',
    ...forecastOf(used, limit, start, rolling ? null : at(d.getFullYear(), d.getMonth() + (s.getUTCMonth() === d.getMonth() ? 1 : 0))),
  };
}
/** 机器的流量字段（服务端算好的）：网卡今日 / 本月、本月从哪天起有数、额度周期 */
export function trafficOf(m: MachineView, all: MachineView[]): Pick<MachineView, 'nic' | 'month_counted_from' | 'quota'> {
  const d = new Date();
  return { nic: nicOf(m, all), month_counted_from: countedFrom(m, new Date(Date.UTC(d.getFullYear(), d.getMonth(), 0)).toISOString().slice(0, 10)), quota: quotaOf(m, all) };
}

const NO_USD: UsdTotal = { cents: 0, by_currency: [], unconverted: [] }; // 团队月费的占位：GET /teams 时按在用机器现算
export const SUB_FIELDS: SubField[] = ['price_cents', 'currency', 'cycle', 'billing_tz', 'remind_days', 'traffic_gb', 'traffic_reset_day', 'traffic_rule'];
/** 订阅模板（同服务端 sub_template）：没单独设置的项取这里；没有套餐时 price_cents 为 null */
export function templateOf(plan: Plan | undefined, pv: Provider | undefined): SubTemplate {
  return {
    price_cents: plan?.price_cents ?? null, currency: plan?.currency ?? META.defaults.currency, cycle: plan?.cycle ?? META.defaults.cycle,
    billing_tz: pv?.timezone || META.org_timezone, remind_days: pv?.remind_days ?? META.defaults.remind_days,
    traffic_gb: plan?.traffic_gb ?? null, traffic_reset: pv?.traffic_reset ?? META.defaults.traffic_reset, traffic_rule: pv?.traffic_rule ?? META.defaults.traffic_rule,
  };
}
const same = (a: unknown, b: unknown) => (a || null) === (b || null); // 流量上限 0 与 null 都是不限
/** 直接设定到期日后的账单日（同服务端 meta::billing_day）：落在现有账单日上的（小月按月末算，如账单日 31 号的 2-28）沿用现有的，否则改成新到期日几号 */
export function billingDay(due: string, cur: number | null | undefined): number {
  const [y, mo, d] = due.split('-').map(Number);
  const len = new Date(Date.UTC(y, mo, 0)).getUTCDate();
  return cur != null && d === Math.min(Math.max(cur, 1), 31, len) ? cur : d;
}
/** 订阅模板、账单日、单独设置了哪几项：按订阅现有的值和跟随值比（和套餐 / 供应商相同的值按跟随存，同服务端写订阅时的 subscription_normalize） */
export function syncSub(m: MachineView, plans: Plan[], providers: Provider[]) {
  m.sub_template = templateOf(plans.find((p) => p.id === m.plan?.id), providers.find((p) => p.id === m.provider?.id));
  const s = m.subscription; if (!s) return;
  s.billing_day = billingDay(s.due_date, s.billing_day);
  s.overrides = SUB_FIELDS.filter((k) => !same(s[k], inheritedValue(m, k, s.due_date)));
}
/** 套餐或供应商改了：没单独设置的项跟着变（服务端是视图里现算，这里把值写回订阅） */
export function refreshFollowers(m: MachineView, plans: Plan[], providers: Provider[]) {
  const s = m.subscription; if (!s) return;
  const own = s.overrides;
  m.sub_template = templateOf(plans.find((p) => p.id === m.plan?.id), providers.find((p) => p.id === m.provider?.id));
  for (const k of SUB_FIELDS) if (!own.includes(k)) (s as unknown as Record<string, unknown>)[k] = inheritedValue(m, k, s.due_date);
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
  /** 回收站（只读列表）：新回收的插在最前 */
  recycle: RecycleDetail[];
  seq: number;
}

export function buildDb(): Db {
  const r = rng(20261020);
  const teams: Team[] = [
    { id: 't1', name: '团队1', sort_order: 1, spare: false, node_count: 0, monthly_usd: NO_USD, monthly_usd_held: NO_USD, held_count: 0, held_total: 0 },
    { id: 't2', name: '团队2', sort_order: 2, spare: false, node_count: 0, monthly_usd: NO_USD, monthly_usd_held: NO_USD, held_count: 0, held_total: 0 },
    { id: 'ts', name: '备用', sort_order: 999, spare: true, node_count: 0, monthly_usd: NO_USD, monthly_usd_held: NO_USD, held_count: 0, held_total: 0 },
  ];
  const providers: Provider[] = [
    { id: 'p1', name: '搬瓦工', code: 'BWH', panel_url: 'https://panel.example.com/bwh', timezone: 'America/Los_Angeles', traffic_rule: 'min', traffic_reset: 'rolling30', remind_days: 7 },
    { id: 'p2', name: '腾讯云', code: 'TCT', panel_url: 'https://panel.example.com/tct', timezone: 'Asia/Shanghai', traffic_rule: 'out', traffic_reset: 'billing', remind_days: 7 },
    { id: 'p3', name: '家宽服务商', code: 'HOME', panel_url: null, timezone: null, traffic_rule: 'max', traffic_reset: 'month', remind_days: 7 },
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
  const REACH_OK = (): EntryView['reach'] => ({ ok: true, ts: iso(now() - 30_000), detail: null, fails: 0 });
  const GiB = 1024 ** 3;

  const sub = (planId: string, dueDays: number, trafficGb: number | null): Subscription => {
    const p = plan(planId);
    return subCalc({ price_cents: p.price_cents, currency: p.currency, cycle: p.cycle, due_date: dateOnly(now() + dueDays * DAY), due_days: dueDays, billing_tz: prov(p.provider_id).timezone ?? META.org_timezone, auto_renew: false, remind_days: META.defaults.remind_days, traffic_gb: trafficGb, traffic_reset_day: 1, traffic_rule: prov(p.provider_id).traffic_rule } as Subscription);
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
    team: null, spare: false, number: null, label: null, ssh_port: 22, country: countryOf(o.ip), country_set: null, host_machine: null, bind_ip: null, egress_ip: null, ip_nodes: [], provider: null, provider_code: null, plan: null, service_ref: null,
    lifecycle: 'active', lifecycle_reason: null, note: '', revision: 1, created_at: iso(now() - 120 * DAY), activated_at: iso(now() - 100 * DAY), retired_at: null, spare_from: null, spare_since: null, recycle_on: null, recycle_held: false, idle_spare: false, spare_unbilled: false, live_ip_nodes: [], renewing_ip_nodes: [], router_exits: [], billing_exempt: false, no_router_exits: false,
    subscription: null, sub_template: templateOf(undefined, undefined), agent: null, host: null, entries: [], removed_traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 }, nic: null, month_counted_from: null, quota: { used: 0, period_start: '', period_end: null, mode: 'monthly', counted_from: null, rule: null, base: null, limit_gb: null, limit_source: 'none', daily_avg: null, forecast_full_at: null }, health: { status: 'ok', reasons: [] },
    active_job: null, failed_job: null, open_incidents: 0, maintenance: false, relay: null,
    tune: o.kind === 'proxy' || o.host_machine ? null : { profile: 'tune-v2', at: iso(now() - 3 * DAY), current: true, params: o.kind === 'relay'
      ? { bw: 1000, bw_plan: 2560, bw_source: 'plan', rtt: 172, rtt_source: 'sampled', rtt_samples: 41, mem_mb: 3911, buf_max: 45097152 }
      : { bw: 50, bw_plan: 50, bw_source: 'plan', rtt: 100, rtt_source: 'default', rtt_samples: 0, mem_mb: 3911, buf_max: 16777216 } }, ...o,
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
      // AnyTLS：线路机1 在跑，线路机2 还没装 sing-box
      agent: { version: '1.0.3', last_report_at: iso(now() - 8000), online: true, xray_running: true, cores: 2, health: null, nic_ifaces: ['ens3'],
        anytls: d.id === 'r1' ? { installed: true, running: true, version: '1.14.4', last_error: null } : { installed: false, running: false } }, host: hostOf(d.id, d.id === 'r1' ? 32 : 18),
      relay: { public_host: d.host, generation: d.gen, applied_generation: d.applied, applied_at: iso(now() - 2 * 3600_000), apply_error: null, port_min: 20001, port_max: 20999, protocols: 'both' },
      health: { status: 'ok', reasons: [] },
    }));
  }

  // 节点定义：团队、编号、套餐、状态
  type Def = { team: 't1' | 't2'; n: number; kind: 'vps' | 'proxy'; planId: string; relay: 'r1' | 'r2'; due: number; status?: HealthStatus; online?: boolean; resetDay?: number; lifecycle?: Lifecycle; cpu?: number; octet?: number; provider?: string; unlimited?: boolean; fresh?: number };
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
  ov('t1', 6, { unlimited: true }); ov('t2', 6, { unlimited: true }); // 不限流量的 VPS
  ov('t1', 12, { fresh: 3 }); ov('t2', 11, { fresh: 3 }); // 刚接入几天：本月 / 本周期数据不完整（「X 起」）

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
    // 几台 VPS 节点是穿透入口（节点主动连入口）：TCP 延迟是节点 → 线路机的
    const reverse = d.kind === 'vps' && d.n % 4 === 2 && !offline;
    const entry: EntryView = {
      id: `e${eSeq++}`, relay: { id: relay.id, name: relay.name }, node: { id, name }, direct: false, mode: reverse ? 'reverse' : 'normal', port,
      lifecycle: lifecycle === 'retiring' ? 'disabled' : 'active', applied: true, apply_error: null, reach: REACH_OK(),
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
      subscription: { ...sub(d.planId, d.due, d.unlimited ? 0 : p.traffic_gb || null), traffic_reset_day: d.resetDay ?? 1 },
      agent: d.kind === 'proxy' ? null : { version: '1.0.3', last_report_at: iso(now() - (offline ? 14 * 60_000 : 9000)), online: !offline && status !== 'unknown', xray_running: null, cores: 2, health: null, nic_ifaces: ['ens3'] },
      host: d.kind === 'proxy' || status === 'unknown' ? null : hostOf(id, d.cpu ?? 20),
      entries: [entry], health: { status, reasons }, maintenance: status === 'maintenance', open_incidents: offline ? 2 : down ? 1 : status === 'degraded' ? 1 : 0,
      note: d.n === 1 ? '主力线路，勿动' : '', ...(d.fresh && { activated_at: iso(now() - d.fresh * DAY) }),
    });
    if (offline && m.host) { m.host.ts = iso(now() - 14 * 60_000); }
    if (id === 'm-t2-06' && m.host) m.host.load1 = null; // LXC 容器：agent 拿不到本机负载
    machines.push(m);
  }

  // 上架中（运行中的作业，步骤展示用）
  {
    const p = plan('pl1'); const team = teams[0];
    const id = 'm-t1-13p';
    const e: EntryView = { id: `e${eSeq++}`, relay: { id: 'r1', name: '线路机1' }, node: { id, name: displayName(team.name, 13) }, direct: false, mode: 'normal', port: 20000 + entriesByRelay.r1.length + 1, lifecycle: 'pending', applied: false, apply_error: null, tcp: null, egress: null, reach: null, traffic: { today_up: 0, today_down: 0, month_up: 0, month_down: 0 } };
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
  // 备用 ×3：退役时移进备用团队，机器照常运行；订阅到期 + 2 天 0 点起自动回收
  [['t1', 3, '198.51.100.88', 40, 4, '线路淘汰'], ['t2', 5, '203.0.113.99', 12, 11, '供应商到期未续'], ['t1', 8, '198.51.100.89', 25, -1, '换新机']].forEach(([tid, n, ip, days, due, why], i) => {
    const team = teams.find((t) => t.id === tid)!; const spare = teams.find((t) => t.spare)!; const p = plan('pl2'); const rid = tid === 't1' ? 'r1' : 'r2'; const relay = machines.find((x) => x.id === rid)!;
    const id = `m-ts-${i + 1}`; const name = displayName(spare.name, i + 1); const sb = sub('pl2', due as number, p.traffic_gb || null);
    const e: EntryView = {
      id: `e${eSeq++}`, relay: { id: relay.id, name: relay.name }, node: { id, name }, direct: false, mode: 'normal', port: 20000 + entriesByRelay[rid].length + 1, lifecycle: 'active', applied: true, apply_error: null, reach: REACH_OK(),
      tcp: { ok: true, latency_ms: Math.round(30 + r() * 120), detail: null, ts: iso(now() - 20_000) }, egress: { ok: true, latency_ms: Math.round(120 + r() * 200), detail: null, ts: iso(now() - 40_000) },
      traffic: { today_up: Math.round(r() * GiB), today_down: Math.round(r() * 3 * GiB), month_up: Math.round(r() * 30 * GiB), month_down: Math.round(r() * 90 * GiB) },
    };
    entriesByRelay[rid].push(e);
    machines.push(base({
      id, kind: 'vps', name, team: { id: spare.id, name: spare.name }, spare: true, number: i + 1, ip: ip as string, service_ref: `SVC-${2000 + i}`, lifecycle_reason: why as string,
      spare_from: displayName(team.name, n as number), spare_since: iso(now() - (days as number) * DAY / 4),
      // 第三台的出口还被软路由指着：不给回收日（同服务端 recycle_held）
      recycle_on: i === 2 ? null : addDays(sb.due_date, 2), recycle_held: i === 2, idle_spare: i !== 2, spare_unbilled: i !== 2, renewing_ip_nodes: [], router_exits: i === 2 ? [`${team.name}-路由A · 节点${String(n).padStart(2, '0')}`] : [],
      provider: { id: 'p1', name: '搬瓦工' }, provider_code: 'BWH', plan: { id: p.id, name: p.name, bandwidth_mbps: p.bandwidth_mbps, traffic_gb: p.traffic_gb }, subscription: sb,
      agent: { version: '1.0.3', last_report_at: iso(now() - 9000), online: true, xray_running: null, cores: 2, health: null }, host: hostOf(id, 12), entries: [e],
    }));
  });
  // 同机 IP 节点：团队1-03 上另配的一个 IP。SSH 端口、供应商、agent 与主机读数是宿主机的（agent、host 与宿主机同一个对象，读数一起变），
  // 不带套餐、默认不记费用；SS 端口 8389
  {
    const h = machines.find((m) => m.id === 'm-t1-03')!; const team = teams[0];
    const id = 'm-t1-14'; const name = displayName(team.name, 14);
    const e: EntryView = {
      id: `e${eSeq++}`, relay: { id: 'r1', name: '线路机1' }, node: { id, name }, direct: false, mode: 'normal', port: 20000 + entriesByRelay.r1.length + 1, lifecycle: 'active', applied: true, apply_error: null, reach: REACH_OK(),
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

  const m2Entries = entriesByRelay.r2;
  // 把入口挂到线路机上
  for (const rl of ['r1', 'r2'] as const) {
    const m = machines.find((x) => x.id === rl)!;
    m.entries = entriesByRelay[rl];
    if (rl === 'r2') { const last = m.entries[m.entries.length - 1]; last.applied = false; last.apply_error = null; }
  }
  // 线路机 2 的最后一个入口还没应用 → 降级提示
  // 线路机 2：中控连不上其中三个客户入口端口（中控 → 线路机最近一次 TCP 检查失败）
  [2, 3, 4].forEach((i) => { m2Entries[i].reach = { ok: false, ts: iso(now() - 25_000), detail: `端口 ${m2Entries[i].port}：connect: 连接被拒绝 (os error 111)`, fails: 3 }; });
  // 另一个口刚才偶发连不上一次：只提示「刚才连不上 1 次」，不算连不上
  m2Entries[1].reach = { ok: false, ts: iso(now() - 40_000), detail: `端口 ${m2Entries[1].port}：connect: 连接超时`, fails: 1 };
  // 原因文本同服务端 reach.rs：每个端口一段「端口 N：原因」，同一原因的多个端口在网页上合并成「N 个端口超时 10 秒」
  const reachText = [...m2Entries.slice(2).map((e, i) => `端口 ${e.port}：${i === 0 ? '连接被拒绝' : '超时 10 秒'}`)].join('；');
  machines[1].health = { status: 'down', reasons: [{ key: 'relay_unreachable', severity: 'critical', text: `中控连续 3 次以上连不上客户入口（${reachText}）` }, { key: 'relay_apply_lag', severity: 'warning', text: '配置未应用（配置版本 8，已生效 7）' }] };
  machines[1].host = hostOf('r2', 58);
  // 调优样本：线路机 2 是旧档，团队1 的 03 号还没调优过
  machines[1].tune = { profile: 'tune-v1', at: iso(now() - 40 * DAY), current: false, params: null };
  const untuned = machines.find((x) => x.team?.id === 't1' && x.number === 3 && x.kind === 'vps');
  if (untuned) untuned.tune = { profile: null, at: null, current: false, params: null };

  // 团队1-03 单独设置按双向计费（供应商是单向），费用页签演示「单独设置」
  const dual = machines.find((x) => x.id === 'm-t1-03');
  if (dual?.subscription) dual.subscription.traffic_rule = 'sum';
  for (const m of machines) Object.assign(m, trafficOf(m, machines));
  // 线路机 2 本周期已超出额度（约 113%）：额度告警、搬迁建议（超额时的口径）、费用页标红用
  const r2 = machines[1];
  if (r2.subscription) { r2.subscription.traffic_gb = Math.max(1, Math.floor(r2.quota.used / 1e9 / 1.13)); Object.assign(r2, trafficOf(r2, machines)); }
  // 团队2 的 05 号：出口已改指向别处，团队的软路由都没有出口指着它（抽屉标一句，同服务端 no_router_exits）
  const unpointed = machines.find((x) => x.team?.id === 't2' && x.number === 5 && x.kind !== 'relay');
  if (unpointed) unpointed.no_router_exits = true;
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
    mk('j-done1', 'retire', { id: 'm-t2-05', name: '团队2-05' }, 'succeeded', RSTEPS, 4, 60 * 24 * 12),
    mk('j-done2', 'provision_vps', { id: 'm-t1-12', name: '团队1-12' }, 'succeeded', PSTEPS, 5, 60 * 24 * 30),
    mk('j-done3', 'retire', { id: 'm-t1-03', name: '团队1-03' }, 'succeeded', RSTEPS, 4, 60 * 24 * 40),
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
  // 改软路由出口失败、换回也失败：需要人工处理（manual 非空）
  const mj = mk('j-manual', 'replace', { id: 'm-t1-03', name: '团队1-03' }, 'failed', [['preflight', '检查'], ['router_edit', '改软路由出口'], ['swap', '交换编号']], 1, 20, '软路由「团队2-路由B」：经隧道连不上路由，换回也没成功');
  mj.steps[1].error_kind = 'network';
  mj.manual = { message: '软路由「团队2-路由B」的配置可能停在改了一半：经隧道连不上路由，换回没成功。备份在 /etc/fleet-backup/openclash/config.20261023-005514Z；登录路由检查 OpenClash 配置后，点重试。', routers: [{ id: 'rt-b', name: '团队2-路由B', exits: ['节点03', '节点07'] }, { id: 'rt-a', name: '团队2-路由A', exits: ['节点12'] }] };
  jobs.splice(3, 0, cj, uj, mj);
  // 同机 IP 节点的上架：先在宿主机上核对 IP，收敛不做系统基线（bbr 为 null）
  const ij = mk('j-ip', 'provision_ip', { id: 'm-t1-14', name: '团队1-14' }, 'succeeded', IP_STEPS, 6, 60 * 24 * 9);
  ij.steps[0].output = { ss_port: 8389, local_ips: '198.51.100.13, 198.51.100.66', egress_ip: '198.51.100.66', bind_ip: null };
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
    { id: 'i1', engine: 'machine', subject: { id: 'm-t1-05', name: '团队1-05' }, subject_id: 'm-t1-05', subject_name: '团队1-05', rule: 'agent_offline', rule_name: 'agent 离线', severity: 'critical', state: 'firing', summary: 'agent 已离线 14 分钟', opened_at: iso(now() - 14 * 60_000), resolved_at: null, last_seen_at: iso(now() - 60_000), ack_at: null, ack_by: null, reopen_count: 0 },
    { id: 'i2', engine: 'machine', subject: { id: 'm-t1-05', name: '团队1-05' }, subject_id: 'm-t1-05', subject_name: '团队1-05', rule: 'tcp_fail', rule_name: '线路机到节点不通', severity: 'critical', state: 'firing', summary: '线路机到节点 TCP 探测连续失败 5 次', opened_at: iso(now() - 12 * 60_000), resolved_at: null, last_seen_at: iso(now() - 60_000), ack_at: iso(now() - 5 * 60_000), ack_by: 'admin', reopen_count: 0 },
    { id: 'i3', engine: 'machine', subject: { id: 'm-t1-09', name: '团队1-09' }, subject_id: 'm-t1-09', subject_name: '团队1-09', rule: 'egress_fail', rule_name: '出口不通', severity: 'warning', state: 'firing', summary: '出口验证连续失败 3 次（经线路机1:20009 访问外网超时）', opened_at: iso(now() - 50 * 60_000), resolved_at: null, last_seen_at: iso(now() - 60_000), ack_at: null, ack_by: null, reopen_count: 2, recent_count: 4, chronic: true },
    { id: 'i4', engine: 'machine', subject: { id: 'r2', name: '线路机2' }, subject_id: 'r2', subject_name: '线路机2', rule: 'relay_apply_lag', rule_name: '配置未应用', severity: 'warning', state: 'firing', summary: '线路机2：配置未应用（配置版本 8，已生效 7）', opened_at: iso(now() - 25 * 60_000), resolved_at: null, last_seen_at: iso(now() - 60_000), ack_at: null, ack_by: null, reopen_count: 0 },
    { id: 'i5', engine: 'machine', subject: { id: 'm-t2-03', name: '团队2-03' }, subject_id: 'm-t2-03', subject_name: '团队2-03', rule: 'renewal_due', rule_name: '即将到期', severity: 'info', state: 'resolved', summary: '7 天内到期，请安排续费', opened_at: iso(now() - 26 * 3600_000), resolved_at: iso(now() - 25 * 3600_000), last_seen_at: iso(now() - 60_000), ack_at: null, ack_by: null, reopen_count: 0 },
    // 已恢复的历史（抽屉「最近告警」）：团队1-05 昨天出口断过半小时、来回反复，三天前 CPU 偏高；团队1-09 六天前 TCP 断过
    { id: 'i6', engine: 'machine', subject: { id: 'm-t1-05', name: '团队1-05' }, subject_id: 'm-t1-05', subject_name: '团队1-05', rule: 'egress_fail', rule_name: '出口不通', severity: 'critical', state: 'resolved', summary: '出口验证连续失败 3 次（经线路机1:20005 访问外网超时）', opened_at: iso(now() - 26 * 3600_000), resolved_at: iso(now() - 26 * 3600_000 + 28 * 60_000), last_seen_at: iso(now() - 26 * 3600_000 + 27 * 60_000), ack_at: null, ack_by: null, reopen_count: 2, recent_count: 3, chronic: true },
    { id: 'i7', engine: 'machine', subject: { id: 'm-t1-05', name: '团队1-05' }, subject_id: 'm-t1-05', subject_name: '团队1-05', rule: 'host_cpu_high', rule_name: 'CPU 持续偏高', severity: 'warning', state: 'resolved', summary: 'CPU 持续 10 分钟高于 90%（最高 97%）', opened_at: iso(now() - 3 * DAY), resolved_at: iso(now() - 3 * DAY + 95 * 60_000), last_seen_at: iso(now() - 3 * DAY + 94 * 60_000), ack_at: iso(now() - 3 * DAY + 20 * 60_000), ack_by: 'admin', reopen_count: 0 },
    { id: 'i8', engine: 'machine', subject: { id: 'm-t1-09', name: '团队1-09' }, subject_id: 'm-t1-09', subject_name: '团队1-09', rule: 'tcp_fail', rule_name: '线路机到节点不通', severity: 'critical', state: 'resolved', summary: '线路机到节点 TCP 探测连续失败 5 次', opened_at: iso(now() - 6 * DAY), resolved_at: iso(now() - 6 * DAY + 12 * 60_000), last_seen_at: iso(now() - 6 * DAY + 11 * 60_000), ack_at: null, ack_by: null, reopen_count: 0 },
    { id: 'i9', engine: 'machine', subject: { id: 'm-t1-09', name: '团队1-09' }, subject_id: 'm-t1-09', subject_name: '团队1-09', rule: 'egress_fail', rule_name: '出口不通', severity: 'warning', state: 'resolved', summary: '出口验证连续失败 3 次（经线路机1:20009 访问外网超时）', opened_at: iso(now() - 2 * DAY), resolved_at: iso(now() - 2 * DAY + 20 * 60_000), last_seen_at: iso(now() - 2 * DAY + 20 * 60_000), ack_at: null, ack_by: null, reopen_count: 0, recent_count: 4, chronic: true },
    // 持续握手丢包（tcp_loss_high）：四天前同供应商两台一起，摘要带供应商关联
    { id: 'i10', engine: 'machine', subject: { id: 'm-t1-09', name: '团队1-09' }, subject_id: 'm-t1-09', subject_name: '团队1-09', rule: 'tcp_loss_high', rule_name: '持续握手丢包', severity: 'warning', state: 'resolved', summary: '团队1-09：近 30 分钟线路机到节点 TCP 探测 62 次，失败 8 次（13%），成功的 54 次里 21 次 SYN 重传（39%）；同供应商 ATTVPS 共 2 台同时（可能是供应商入站丢包）', opened_at: iso(now() - 4 * DAY), resolved_at: iso(now() - 4 * DAY + 3 * 3600_000), last_seen_at: iso(now() - 4 * DAY + 3 * 3600_000 - 60_000), ack_at: null, ack_by: null, reopen_count: 0 },
  ];
  // 流量额度吃紧（traffic_quota_high，同服务端规则）：本周期已用 ≥ 90% 警告、≥ 100% 严重；开始时间错开几小时
  let tight = 0;
  for (const m of machines) {
    const q = quota(m);
    if (m.lifecycle !== 'active' || q.pct == null || q.pct < 90) continue;
    const pct = Math.floor(q.pct);
    incidents.push({ id: `iq-${m.id}`, engine: 'machine', subject: { id: m.id, name: m.name }, subject_id: m.id, subject_name: m.name, rule: 'traffic_quota_high', rule_name: '流量额度吃紧', severity: pct >= 100 ? 'critical' : 'warning', state: 'firing',
      summary: `本周期已用 ${pct}%（${str(quotaGb(q.used))} / ${q.limit} GB，${q.start.slice(5)} 起）`, opened_at: iso(now() - (2 + 3 * tight++) * 3600_000), resolved_at: null, last_seen_at: iso(now() - 60_000), ack_at: null, ack_by: null, reopen_count: 0 });
    m.open_incidents++;
  }
  // 进行中的维护、只静默一条规则的、明天凌晨 2 点预约的割接（本地时间）
  const tomorrow2 = new Date(now()); tomorrow2.setDate(tomorrow2.getDate() + 1); tomorrow2.setHours(2, 0, 0, 0);
  const silences: Silence[] = [
    { id: 's1', subject_kind: 'machine', subject_id: 'm-t2-04', subject_name: '团队2-04', rule: null, rule_name: null, starts_at: iso(now() - 3600_000), ends_at: iso(now() + 3 * 3600_000), is_maintenance: true, comment: '换内核，预计 4 小时' },
    { id: 's2', subject_kind: 'machine', subject_id: 'm-t1-03', subject_name: '团队1-03', rule: 'clock_skew', rule_name: 'agent 时钟不准', starts_at: iso(now() - 2 * 3600_000), ends_at: iso(now() + 22 * 3600_000), is_maintenance: false, comment: '等供应商校时' },
    { id: 's3', subject_kind: 'machine', subject_id: 'r2', subject_name: '线路机2', rule: null, rule_name: null, starts_at: iso(tomorrow2.getTime()), ends_at: iso(tomorrow2.getTime() + 4 * 3600_000), is_maintenance: true, comment: '机房割接（供应商通知）' },
    { id: 's4', subject_kind: 'router', subject_id: 'rt-b', subject_name: '团队2-路由B', rule: null, rule_name: null, starts_at: iso(now() - 2 * 3600_000), ends_at: iso(now() + 2 * 3600_000), is_maintenance: true, comment: '客户现场换宽带' },
  ];
  const audit: AuditEntry[] = [
    // 下面几条照中控的写法：title / summary 是 audit_text 按 detail 拼的
    { id: 'a9', ts: iso(now() - 8 * 60_000), actor: 'apikey:运维 CLI', action: 'plan.update', title: '归档套餐', summary: 'CN2 GIA 1C/2G', target_kind: 'plan', target_id: 'pl2', detail: { name: 'CN2 GIA 1C/2G', currency: 'USD', before: { archived_at: null }, after: { archived_at: iso(now() - 8 * 60_000) } } },
    { id: 'a8', ts: iso(now() - 9 * 60_000), actor: 'admin', action: 'plan.update', title: '修改套餐', summary: 'CN2 GIA 2C/4G · 价格 59.00 → 69.00 USD，流量 800 → 1000 GB', target_kind: 'plan', target_id: 'pl1', detail: { name: 'CN2 GIA 2C/4G', currency: 'USD', before: { price_cents: 5900, traffic_gb: 800 }, after: { price_cents: 6900, traffic_gb: 1000 } } },
    { id: 'a7', ts: iso(now() - 15 * 60_000), actor: 'admin', action: 'exec', title: '执行命令', summary: '团队1-05 · systemctl restart fleet-agent · 退出码 1', target_kind: 'machine', target_id: 'm-t1-05', detail: { name: '团队1-05', command: 'systemctl restart fleet-agent', exit_code: 1, duration_ms: 1840, timed_out: false } },
    { id: 'a6', ts: iso(now() - 20 * 60_000), actor: 'admin', action: 'ssh.login', title: 'SSH 登录', summary: '团队1-03 · 交互式', target_kind: 'vps', target_id: 'm-t1-03', detail: { name: '团队1-03', command: false } },
    { id: 'a3', ts: iso(now() - 25 * 60_000), actor: 'apikey:运维 CLI', action: 'exec', title: '执行命令', summary: '团队1-03 · journalctl -u fleet-agent --since "1 hour ago" --no-pager | grep -i -E "err… · 退出码 0', target_kind: 'machine', target_id: 'm-t1-03', detail: { name: '团队1-03', command: 'journalctl -u fleet-agent --since "1 hour ago" --no-pager | grep -i -E "error|warn" | tail -n 200', exit_code: 0, duration_ms: 640, timed_out: false } },
    { id: 'a1', ts: iso(now() - 40 * 60_000), actor: 'admin', action: 'machine.rename', title: '改名', summary: '团队1-07 → 团队1-03', target_kind: 'machine', target_id: 'm-t1-03', detail: { name: '团队1-03', old_name: '团队1-07' } },
    { id: 'a2', ts: iso(now() - 3 * 3600_000), actor: 'admin', action: 'renewal.create', title: '登记续费', summary: '团队2-09 到期日顺延 3 个月', target_kind: 'machine', target_id: 'm-t2-09', detail: { months: 3 } },
    { id: 'a5', ts: iso(now() - 13 * 3600_000), actor: 'cli', action: 'exec', title: '执行命令', summary: '团队2-02 · apt-get update · 执行中或中断（未记录结果）', target_kind: 'machine', target_id: 'm-t2-02', detail: { name: '团队2-02', command: 'apt-get update', started: true } },
    { id: 'a10', ts: iso(now() - 26 * 3600_000), actor: 'admin', action: 'agent.upgrade', title: '升级 agent', summary: '12 台，跳过 1 台', target_kind: 'machine', target_id: null, detail: { count: 12, skipped: 1 } },
    { id: 'a11', ts: iso(now() - 27 * 3600_000), actor: 'admin', action: 'machine.update', title: '修改资料', summary: '团队1-05 · 国家 HK → JP', target_kind: 'machine', target_id: 'm-t1-05', detail: { name: '团队1-05', old_name: '团队1-05', before: { team_id: 't1', number: 5, label: null, country: 'HK' }, after: { team_id: 't1', number: 5, label: null, country: 'JP' } } },
    { id: 'a12', ts: iso(now() - 28 * 3600_000), actor: 'admin', action: 'auth.login_failed', title: '登录失败', summary: '203.0.113.9', target_kind: 'user', target_id: null, detail: { ip: '203.0.113.9' } },
    { id: 'a4', ts: iso(now() - 12 * DAY), actor: 'admin', action: 'machine.provision', title: '上架', summary: '团队1-07 · 198.51.100.13', target_kind: 'machine', target_id: 'm-t1-03', detail: { name: '团队1-07', ip: '198.51.100.13', kind: 'vps' } },
  ];
  const apiKeys: ApiKey[] = [
    { id: 'k1', name: '备份脚本', prefix: 'a3f9c2d1', scopes: ['read'], created_at: iso(now() - 40 * DAY), expires_at: iso(now() + 50 * DAY), last_used_at: iso(now() - 2 * 3600_000), revoked_at: null },
    { id: 'k2', name: '运维 CLI', prefix: '7be40c18', scopes: ['read', 'write', 'secrets', 'exec'], created_at: iso(now() - 9 * DAY), expires_at: null, last_used_at: iso(now() - 12 * 60_000), revoked_at: null },
    { id: 'k3', name: '旧监控（已停用）', prefix: '19d8e0aa', scopes: ['read'], created_at: iso(now() - 120 * DAY), expires_at: iso(now() - 30 * DAY), last_used_at: iso(now() - 31 * DAY), revoked_at: iso(now() - 33 * DAY) },
  ];
  const channels: Db['channels'] = [
    { id: 'c1', name: '运维群', kind: 'telegram', min_severity: 'warning', enabled: true, monthly_report: true, created_at: iso(now() - 20 * DAY), config_hint: 'chat -1001234567', config: { bot_token: '123:abc', chat_id: '-1001234567' } },
    { id: 'c2', name: '值班飞书', kind: 'feishu', min_severity: 'critical', enabled: true, monthly_report: false, created_at: iso(now() - 12 * DAY), config_hint: 'open.feishu.cn · 已加签', config: { url: 'https://open.feishu.cn/open-apis/bot/v2/hook/x', secret: 'x' } },
    { id: 'c3', name: '研发群飞书', kind: 'feishu', min_severity: 'info', enabled: false, monthly_report: true, created_at: iso(now() - 3 * DAY), config_hint: 'open.larksuite.com', config: { url: 'https://open.larksuite.com/open-apis/bot/v2/hook/y' } },
  ];
  const notifyLog: NotifyLogEntry[] = [
    { id: 'n1', channel_id: 'c1', channel_name: '运维群', incident_id: 'i1-0000-aaaa', ts: iso(now() - 14 * 60_000), ok: true, error: null, kind: 'firing', subject_name: '团队1-04', rule_name: '出口不通' },
    { id: 'n2', channel_id: 'c2', channel_name: '值班飞书', incident_id: 'i1-0000-aaaa', ts: iso(now() - 14 * 60_000), ok: true, error: null, kind: 'firing', subject_name: '团队1-04', rule_name: '出口不通' },
    { id: 'n3', channel_id: 'c1', channel_name: '运维群', incident_id: 'i3-0000-bbbb', ts: iso(now() - 50 * 60_000), ok: false, error: 'Telegram 返回 429：请求过于频繁，请 17 秒后重试', kind: 'resolved', subject_name: '线路机2', rule_name: '配置未应用' },
    // 渠道已删除：名字随渠道没了（中控 left join 为 null）
    { id: 'n6', channel_id: 'c9', channel_name: null, incident_id: 'i9-0000-cccc', ts: iso(now() - 2 * DAY), ok: true, error: null, kind: 'firing', subject_name: '团队2-01', rule_name: 'agent 离线' },
    { id: 'n4', channel_id: 'c3', channel_name: '研发群飞书', incident_id: null, ts: iso(now() - 3 * DAY), ok: true, error: null, kind: 'test', subject_name: null, rule_name: null },
    { id: 'n5', channel_id: 'c1', channel_name: '运维群', incident_id: 'i9-0000-cccc', ts: iso(now() - 4 * DAY), ok: true, error: null, kind: 'repeat', subject_name: null, rule_name: 'agent 离线' },
    { id: 'n7', channel_id: 'c1', channel_name: '运维群', incident_id: null, ts: iso(now() - 9 * DAY), ok: true, error: null, kind: 'report', subject_name: '月报', rule_name: null },
  ];
  const s3 = machines.find((m) => m.id === 'm-t1-03')!.subscription!;
  const renewals: Record<string, Renewal[]> = { 'm-t1-03': [{ id: 'rn1', old_due: addMonths(s3.due_date, -3), new_due: s3.due_date, amount_cents: s3.price_cents, currency: s3.currency, note: '账单 #2041', actor: 'admin', created_at: iso(now() - 84 * DAY) }] };
  for (const m of machines) syncSub(m, plans, providers);
  return { user: 'admin', teams, providers, plans, machines, jobs, logs, incidents, silences, audit, renewals, apiKeys, channels, notifyLog, ssPorts: { 'm-t1-14': 8389 }, recycle: recycled(), seq: 100 };
}

/** 同机 IP 节点上架 / 恢复的步骤（同服务端 jobs::PROVISION_IP） */
export const IP_STEPS: [string, string][] = [['ip_check', '在宿主机上核对 IP'], ['converge', '节点收敛（SS 实例、防火墙）'], ['attach', '接入线路机'], ['wait_applied', '等待线路机应用'], ['verify', '出口验收'], ['activate', '上线']];

/** 回收站里的几条记录：本面板（fleet）回收的和旧面板（legacy）导入的 */
function recycled(): RecycleDetail[] {
  const GiB = 1024 ** 3;
  const ago = (d: number) => iso(now() - d * DAY);
  const row = (id: number, o: Partial<RecycleDetail> & Pick<RecycleDetail, 'source' | 'kind' | 'name'>): RecycleDetail => ({
    id, source_id: o.source === 'legacy' ? `legacy-${id}` : `m-gone-${id}`, machine_id: o.source === 'legacy' ? null : `m-gone-${id}`, last_name: null, team: null, ip: null, host_name: null,
    provider: null, plan: null, service_ref: null, price_cents: null, currency: null, cycle: null, activated_at: null, retired_at: null, due_date: null, recycled_at: ago(1), reason: null,
    traffic_up: null, traffic_down: null, nic_rx: null, nic_tx: null, actor: '系统', snapshot: {}, ...o,
  });
  return [
    row(5, { source: 'fleet', kind: 'vps', name: '团队2-07', last_name: '备用-01', team: '团队2', ip: '203.0.113.31', provider: '搬瓦工', plan: 'KVM 2C4G', service_ref: 'SVC-1031', price_cents: 1999, currency: 'USD', cycle: 'monthly',
      activated_at: ago(130), retired_at: ago(6), due_date: dateOnly(now() - 4 * DAY), recycled_at: ago(1), reason: `备用到期自动回收（到期日 ${dateOnly(now() - 4 * DAY)}）`,
      traffic_up: 310 * GiB, traffic_down: 1240 * GiB, nic_rx: 1400 * GiB, nic_tx: 330 * GiB, snapshot: {
        machine: { note: '客户退租' }, name: '备用-01', team: '备用', host: null, provider: '搬瓦工', plan: 'KVM 2C4G', subscription: { due_date: dateOnly(now() - 4 * DAY), price_cents: 1999 },
        renewals: [0, 1, 2].map((i) => ({ id: `rn-gone-${i}`, old_due: dateOnly(now() - (34 + 30 * i) * DAY), new_due: dateOnly(now() - (4 + 30 * i) * DAY), amount_cents: 1999, currency: 'USD', note: '', actor: 'admin', created_at: ago(34 + 30 * i) })),
        entries: [{ relay: '线路机1', node: '团队2-07', port: 20011, lifecycle: 'removed', created_at: ago(120), removed_at: ago(6) }, { relay: '线路机2', node: '团队2-07', port: 20034, lifecycle: 'removed', created_at: ago(100), removed_at: ago(6) }],
        outbound: { protocol: 'shadowsocks', host: '203.0.113.31', port: 8388, method: 'aes-256-gcm', username: null, updated_at: ago(130) },
      } }),
    row(4, { source: 'fleet', kind: 'vps', name: '团队1-12', team: '团队1', ip: '198.51.100.42', provider: '搬瓦工', plan: 'KVM 1C2G', service_ref: 'SVC-1012', price_cents: 1299, currency: 'USD', cycle: 'quarterly',
      activated_at: ago(210), retired_at: ago(22), due_date: dateOnly(now() - 20 * DAY), recycled_at: ago(19), reason: '手动立即回收：线路淘汰', actor: 'admin',
      traffic_up: 120 * GiB, traffic_down: 640 * GiB, nic_rx: 700 * GiB, nic_tx: 140 * GiB, snapshot: {
        machine: { note: '' }, name: '团队1-12', team: '团队1', host: null, provider: '搬瓦工', plan: 'KVM 1C2G', subscription: { due_date: dateOnly(now() - 20 * DAY), price_cents: 1299 },
        renewals: [{ id: 'rn-gone-9', old_due: dateOnly(now() - 110 * DAY), new_due: dateOnly(now() - 20 * DAY), amount_cents: 1299, currency: 'USD', note: '', actor: 'admin', created_at: ago(112) }],
        entries: [{ relay: '线路机1', node: '团队1-12', port: 20003, lifecycle: 'removed', created_at: ago(200), removed_at: ago(22) }],
        outbound: { protocol: 'shadowsocks', host: '198.51.100.42', port: 8388, method: 'aes-256-gcm', username: null, updated_at: ago(210) },
      } }),
    row(3, { source: 'fleet', kind: 'proxy', name: '团队2-09', team: '团队2', ip: 'hk-home.example.net', provider: 'IPRoyal', plan: '家宽 10G', price_cents: 3500, currency: 'USD', cycle: 'monthly',
      activated_at: ago(90), retired_at: ago(41), due_date: dateOnly(now() - 40 * DAY), recycled_at: ago(38), reason: '备用到期自动回收', traffic_up: 22 * GiB, traffic_down: 180 * GiB }),
    row(2, { source: 'legacy', kind: 'vps', name: '团队1-33', team: '团队1', ip: '198.51.100.120', provider: '搬瓦工', plan: 'KVM 2C4G', service_ref: '60131', price_cents: 1999, currency: 'USD', cycle: 'monthly',
      activated_at: ago(300), retired_at: ago(60), due_date: dateOnly(now() - 59 * DAY), recycled_at: ago(5), reason: '旧面板已退役（迁移时未接管）', actor: 'admin', traffic_up: 80 * GiB, traffic_down: 410 * GiB }),
    row(1, { source: 'legacy', kind: 'relay', name: '线路机0', ip: '203.0.113.5', provider: '腾讯云', plan: '2C4G 5M', price_cents: 9800, currency: 'CNY', cycle: 'monthly',
      activated_at: ago(420), retired_at: ago(120), recycled_at: ago(5), reason: '旧面板 2026-09-30 标注「弃用·仅备份，勿续费」，最后流量 10-08', actor: 'admin' }),
  ];
}
