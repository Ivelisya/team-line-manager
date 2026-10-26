// 每日流量的 mock（同服务端 GET /traffic/daily）：每条入口每天的量按它本月的流量折成日均，再按 (入口, 日期) 的哈希上下浮动、周末多一点；
// 今天按组织时区已过去的比例算。线路机网卡 ≈ 经过它的业务流量（进、出各一次）再加一点开销，每天的计费量按供应商规则算，区间合计先求下行、上行之和再套规则。
// 已退役的机器：退役那天之前按固定日均有流量（服务端是小时表里的真实数据）。
// 有自己网卡的节点（不是家宽代理、同机 IP 节点，不是直出行）同线路机带网卡与计费量：网卡下行、上行各 ≈ 它的业务流量
import type { DailyBiz, DailyNic, MachineView, TrafficDaily, TrafficDailyNode, TrafficDailyRelay, TrafficDailyTeam } from '@/lib/types';
import { hashStr } from './fixtures';
import { META } from './meta';

type Ctx = { params: string[]; query: URLSearchParams; body: unknown };
type Reg = (method: string, path: string, h: (c: Ctx) => unknown) => void;

const DAY = 86400_000;
const fmt = new Intl.DateTimeFormat('en-CA', { timeZone: META.org_timezone, year: 'numeric', month: '2-digit', day: '2-digit' });
const localDate = (t: number) => fmt.format(t);
/** 组织时区今天已过去的比例 */
const todayShare = () => { const d = new Date(Date.now() + META.org_utc_offset_s * 1000); return (d.getUTCHours() * 3600 + d.getUTCMinutes() * 60) / 86400; };

/** 闲置判定（同服务端 traffic.rs）：在用、上架满 7 天、近 7 天日均业务流量低于 0.5 GB */
export const IDLE = { days: 7, gb_per_day: 0.5 };
export const mockIdle = (m: MachineView, sum7: number) => m.lifecycle === 'active' && !!m.activated_at && Date.now() - Date.parse(m.activated_at) >= IDLE.days * DAY && sum7 / IDLE.days < IDLE.gb_per_day * 1e9;
/** 月费（USD 分）：在用且有价格的 */
export const mockMonthly = (m: MachineView) => (m.lifecycle === 'active' ? m.subscription?.monthly_usd_cents ?? null : null);
/** 每 GB 的美元：月费 ÷ 按 days 天折成的 30 天流量（同服务端 usd_per_gb） */
export const mockPerGb = (cents: number | null, bytes: number, days: number) => (cents == null || bytes <= 0 ? null : cents / 100 / ((bytes / days) * 30 / 1e9));
/** 一台机器近 30 天、近 7 天的业务流量（粗估：本月流量折成日均；节点看自己的入口，线路机看挂在它上面的） */
export function mockUsage(m: MachineView): { bytes30: number; sum7: number } {
  const elapsed = Math.max(1, Number(localDate(Date.now()).slice(8)) - 1 + todayShare());
  const perDay = m.entries.reduce((a, e) => a + ((e.traffic.month_down || 2e9) + (e.traffic.month_up || 2e8)) / elapsed, 0);
  return { bytes30: Math.round(perDay * 30), sum7: Math.round(perDay * 7) };
}

function billed(rule: string, rx: number, tx: number) { return rule === 'min' ? Math.min(rx, tx) : rule === 'sum' ? rx + tx : rule === 'out' ? tx : rule === 'in' ? rx : Math.max(rx, tx); }
const NO_NIC = { rule: null, rx: null, tx: null, billed: null, nic_daily: null } as const;
const NO_COST = { monthly_usd_cents: null, usd_per_gb: null, avg7: null, idle: false, held: false };
/** 网卡 ≈ 经过的业务流量（进、出各一次）再加一点开销；每天按规则算计费量，区间合计先求下行、上行之和再套规则 */
function nicOf(id: string, biz: DailyBiz[], rule: string) {
  const daily: DailyNic[] = biz.map((d) => { const s = d.down + d.up; const rx = Math.round(s * (1.02 + 0.03 * (hashStr(id + d.date) % 10) / 10)); const tx = Math.round(s * 1.01); return { date: d.date, rx, tx, billed: billed(rule, rx, tx) }; });
  const rx = daily.reduce((a, d) => a + d.rx, 0); const tx = daily.reduce((a, d) => a + d.tx, 0);
  return { daily, rx, tx, billed: billed(rule, rx, tx) };
}

export function installTrafficMock(R: Reg, machines: () => MachineView[], teams: () => { id: string; name: string }[], bad: (msg: string, status?: number, code?: string) => Error) {
  R('GET', '/traffic/daily', ({ query }) => {
    const days = Number(query.get('days') ?? 30);
    if (!(days >= 1 && days <= 92)) throw bad('days 只能是 1–92');
    const all = machines();
    const tq = query.get('team') ?? ''; const mq = query.get('machine') ?? '';
    const team = tq ? teams().find((t) => t.id === tq || t.name === tq) : null;
    if (tq && !team) throw bad(`没有团队「${tq}」`, 404, 'not_found');
    const pick = mq ? all.find((m) => m.id === mq || m.name === mq) : null;
    if (mq && !pick) throw bad(`没有机器「${mq}」`, 404, 'not_found');
    const now = Date.now();
    const dates = Array.from({ length: days }, (_, i) => localDate(now - (days - 1 - i) * DAY));
    const today = dates[dates.length - 1];
    const dayOfMonth = Number(today.slice(8));
    const zero = (): DailyBiz[] => dates.map((date) => ({ date, down: 0, up: 0 }));
    const byId = new Map(all.map((m) => [m.id, m]));
    // (节点, 线路机, 是否直出, 每天)
    const flows: { node: MachineView; relay: MachineView | null; direct: boolean; daily: DailyBiz[] }[] = [];
    const vary = (key: string, date: string) => {
      const h = hashStr(key + date) / 0xffffffff; const wd = new Date(`${date}T00:00:00Z`).getUTCDay();
      return (0.55 + 0.9 * h) * (wd === 0 || wd === 6 ? 1.25 : 1) * (date === today ? todayShare() : 1);
    };
    for (const r of all.filter((x) => x.kind === 'relay')) {
      for (const e of r.entries) {
        const node = byId.get(e.node.id); if (!node) continue;
        const elapsed = Math.max(1, dayOfMonth - 1 + todayShare());
        // 备用团队里的机器基本没人用：流量压到百分之三，演示「闲置」
        const k = node.spare ? 0.03 : 1;
        const dn = k * (e.traffic.month_down || 2e9) / elapsed; const up = k * (e.traffic.month_up || 2e8) / elapsed;
        const start = node.activated_at ? localDate(Date.parse(node.activated_at)) : dates[0];
        flows.push({ node, relay: r, direct: e.direct, daily: dates.map((date) => (date < start ? { date, down: 0, up: 0 } : { date, down: Math.round(dn * vary(e.id, date)), up: Math.round(up * vary(`${e.id}u`, date)) })) });
      }
    }
    // 已退役的机器：退役那天之前有流量，记在第一台线路机上
    const relay0 = all.find((x) => x.kind === 'relay') ?? null;
    for (const m of all.filter((x) => x.lifecycle === 'retired' && x.kind !== 'relay' && x.retired_at)) {
      const end = localDate(Date.parse(m.retired_at!));
      if (end < dates[0]) continue;
      flows.push({ node: m, relay: relay0, direct: false, daily: dates.map((date) => (date > end ? { date, down: 0, up: 0 } : { date, down: Math.round(3e9 * vary(m.id, date)), up: Math.round(3e8 * vary(`${m.id}u`, date)) })) });
    }
    const inTeam = (m: MachineView) => !team || m.team?.id === team.id;
    const kept = flows.filter((f) => inTeam(f.node) && (!pick || f.node.id === pick.id || f.relay?.id === pick.id));
    const add = (to: DailyBiz[], from: DailyBiz[]) => from.forEach((d, i) => { to[i].down += d.down; to[i].up += d.up; });
    const total = zero();
    const nodes = new Map<string, TrafficDailyNode>();
    const biz = new Map<string, DailyBiz[]>();
    for (const f of kept) {
      add(total, f.daily);
      const key = `${f.node.id}${f.direct ? ':d' : ''}`;
      const n = nodes.get(key) ?? { id: f.node.id, name: f.direct ? `${f.node.name}-直出` : f.node.name, team: f.node.team, kind: f.node.kind, lifecycle: f.node.lifecycle, country: f.node.country, direct: f.direct, down: 0, up: 0, daily: zero(), ...NO_NIC, ...NO_COST };
      add(n.daily, f.daily); nodes.set(key, n);
      if (f.relay) { const b = biz.get(f.relay.id) ?? zero(); add(b, f.daily); biz.set(f.relay.id, b); }
    }
    // 没流量的在用节点也列出
    for (const m of all) if (m.kind !== 'relay' && m.lifecycle === 'active' && inTeam(m) && (!pick || pick.id === m.id) && !nodes.has(m.id)) nodes.set(m.id, { id: m.id, name: m.name, team: m.team, kind: m.kind, lifecycle: m.lifecycle, country: m.country, direct: false, down: 0, up: 0, daily: zero(), ...NO_NIC, ...NO_COST });
    const list = [...nodes.values()];
    for (const n of list) {
      n.down = n.daily.reduce((a, d) => a + d.down, 0); n.up = n.daily.reduce((a, d) => a + d.up, 0);
      const m = byId.get(n.id);
      if (n.direct || !m || m.kind === 'proxy' || m.host_machine) continue;
      const rule = m.quota.rule ?? 'max';
      const nd = nicOf(n.id, n.daily, rule);
      Object.assign(n, { rule, rx: nd.rx, tx: nd.tx, billed: nd.billed, nic_daily: nd.daily });
    }
    // 成本与闲置：近 7 / 30 天的量取自（不受过滤影响的）全部流量，页面至少取回 30 天
    const use = new Map<string, { b30: number; s7: number }>();
    for (const f of flows) {
      if (f.direct) continue;
      const u = use.get(f.node.id) ?? { b30: 0, s7: 0 };
      const sum = (ds: DailyBiz[]) => ds.reduce((a, d) => a + d.down + d.up, 0);
      u.b30 += sum(f.daily.slice(-30)); u.s7 += sum(f.daily.slice(-IDLE.days)); use.set(f.node.id, u);
    }
    for (const n of list) {
      const m = byId.get(n.id); if (!m || n.direct) continue;
      n.held = !!m.recycle_held; n.monthly_usd_cents = mockMonthly(m); n.usd_per_gb = mockPerGb(n.monthly_usd_cents, n.down + n.up, days);
      const s7 = use.get(n.id)?.s7 ?? 0; n.avg7 = Math.round(s7 / IDLE.days); n.idle = mockIdle(m, s7);
    }
    const teamRows = new Map<string, TrafficDailyTeam>();
    for (const m of all) {
      if (m.kind === 'relay' || !m.team) continue;
      const t = teamRows.get(m.team.id) ?? { id: m.team.id, name: m.team.name, spare: !!m.spare, nodes: 0, idle: 0, monthly_usd_cents: null, held: 0, held_bytes_30d: 0, unpriced: 0, bytes_30d: 0, usd_per_gb: null };
      const u = use.get(m.id) ?? { b30: 0, s7: 0 };
      t.bytes_30d += u.b30;
      if (m.lifecycle === 'active') {
        t.nodes++; t.idle += !m.spare && mockIdle(m, u.s7) ? 1 : 0;
        // 备用团队只有被扣住不回收的几台照常续费（同服务端），月费、没价格、USD/GB 只算它们
        const held = !!m.spare && !!m.recycle_held;
        if (held) { t.held++; t.held_bytes_30d += u.b30; }
        if (!m.spare || held) { const c = mockMonthly(m); if (c == null) t.unpriced++; else t.monthly_usd_cents = (t.monthly_usd_cents ?? 0) + c; }
      }
      teamRows.set(m.team.id, t);
    }
    const teamList = teams().map((x) => teamRows.get(x.id)).filter((t): t is TrafficDailyTeam => !!t && (t.nodes > 0 || t.bytes_30d > 0)).map((t) => ({ ...t, usd_per_gb: mockPerGb(t.monthly_usd_cents, t.spare ? t.held_bytes_30d : t.bytes_30d, 30) }));
    list.sort((a, b) => b.down + b.up - (a.down + a.up));
    const filtered = !!team || !!pick;
    const relays: TrafficDailyRelay[] = all.filter((r) => r.kind === 'relay' && (filtered ? biz.has(r.id) || pick?.id === r.id : r.lifecycle !== 'retired')).map((r) => {
      const business = biz.get(r.id) ?? zero();
      // 网卡按这台线路机全部的业务流量算（过滤只影响 business）
      const own = zero(); flows.filter((f) => f.relay?.id === r.id).forEach((f) => add(own, f.daily));
      const rule = r.quota.rule ?? 'max';
      const { daily, rx, tx, billed: b } = nicOf(r.id, own, rule);
      return { id: r.id, name: r.name, lifecycle: r.lifecycle, country: r.country, rule, rx, tx, billed: b, down: business.reduce((a, d) => a + d.down, 0), up: business.reduce((a, d) => a + d.up, 0), daily, business };
    });
    return { tz: META.org_timezone, dates, total, idle: IDLE, teams: teamList, nodes: list, relays } satisfies TrafficDaily;
  });
}
