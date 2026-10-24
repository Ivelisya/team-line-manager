// 每日流量的 mock（同服务端 GET /traffic/daily）：每条入口每天的量按它本月的流量折成日均，再按 (入口, 日期) 的哈希上下浮动、周末多一点；
// 今天按组织时区已过去的比例算。线路机网卡 ≈ 经过它的业务流量（进、出各一次）再加一点开销，每天的计费量按供应商规则算，区间合计先求下行、上行之和再套规则。
// 已退役的机器：退役那天之前按固定日均有流量（服务端是小时表里的真实数据）。
// 有自己网卡的节点（不是家宽代理、同机 IP 节点，不是直出行）同线路机带网卡与计费量：网卡下行、上行各 ≈ 它的业务流量
import type { DailyBiz, DailyNic, MachineView, TrafficDaily, TrafficDailyNode, TrafficDailyRelay } from '@/lib/types';
import { hashStr } from './fixtures';
import { META } from './meta';

type Ctx = { params: string[]; query: URLSearchParams; body: unknown };
type Reg = (method: string, path: string, h: (c: Ctx) => unknown) => void;

const DAY = 86400_000;
const fmt = new Intl.DateTimeFormat('en-CA', { timeZone: META.org_timezone, year: 'numeric', month: '2-digit', day: '2-digit' });
const localDate = (t: number) => fmt.format(t);
/** 组织时区今天已过去的比例 */
const todayShare = () => { const d = new Date(Date.now() + META.org_utc_offset_s * 1000); return (d.getUTCHours() * 3600 + d.getUTCMinutes() * 60) / 86400; };

function billed(rule: string, rx: number, tx: number) { return rule === 'min' ? Math.min(rx, tx) : rule === 'sum' ? rx + tx : rule === 'out' ? tx : rule === 'in' ? rx : Math.max(rx, tx); }
const NO_NIC = { rule: null, rx: null, tx: null, billed: null, nic_daily: null } as const;
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
        const dn = (e.traffic.month_down || 2e9) / elapsed; const up = (e.traffic.month_up || 2e8) / elapsed;
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
      const n = nodes.get(key) ?? { id: f.node.id, name: f.direct ? `${f.node.name}-直出` : f.node.name, team: f.node.team, kind: f.node.kind, lifecycle: f.node.lifecycle, country: f.node.country, direct: f.direct, down: 0, up: 0, daily: zero(), ...NO_NIC };
      add(n.daily, f.daily); nodes.set(key, n);
      if (f.relay) { const b = biz.get(f.relay.id) ?? zero(); add(b, f.daily); biz.set(f.relay.id, b); }
    }
    // 没流量的在用节点也列出
    for (const m of all) if (m.kind !== 'relay' && m.lifecycle === 'active' && inTeam(m) && (!pick || pick.id === m.id) && !nodes.has(m.id)) nodes.set(m.id, { id: m.id, name: m.name, team: m.team, kind: m.kind, lifecycle: m.lifecycle, country: m.country, direct: false, down: 0, up: 0, daily: zero(), ...NO_NIC });
    const list = [...nodes.values()];
    for (const n of list) {
      n.down = n.daily.reduce((a, d) => a + d.down, 0); n.up = n.daily.reduce((a, d) => a + d.up, 0);
      const m = byId.get(n.id);
      if (n.direct || !m || m.kind === 'proxy' || m.host_machine) continue;
      const rule = m.quota.rule ?? 'max';
      const nd = nicOf(n.id, n.daily, rule);
      Object.assign(n, { rule, rx: nd.rx, tx: nd.tx, billed: nd.billed, nic_daily: nd.daily });
    }
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
    return { tz: META.org_timezone, dates, total, nodes: list, relays } satisfies TrafficDaily;
  });
}
