// 报表的 mock（同服务端 reports.rs）：月度可用率 GET /reports/uptime、月报预览 GET /reports/monthly 与立即发送 POST /reports/monthly/send。
// 每台在用节点（不含线路机、备用团队）每条入口每天 TCP 2880 次、出口 1440 次，成功率按 (机器, 月份) 的哈希生成：大多数台近乎 100%，个别有零星失败，
// 故障中的机器明显偏低，月内才上架的算到上架那天，上架在窗口之后的没有探测（null）；节点按 TCP 成功率从差到好，供应商按平均从差到好。
import type { Incident, MachineView, MonthlyReport, NotifyChannel, NotifyLogEntry, UptimeItem, UptimeProvider, UptimeReport } from '@/lib/types';
import { hashStr, rng } from './fixtures';
import { META } from './meta';
import { mockMonthly, mockUsage } from './traffic';

type Ctx = { params: string[]; query: URLSearchParams; body: unknown };
type Reg = (method: string, path: string, h: (c: Ctx) => unknown) => void;
type Bad = (msg: string, status?: number, code?: string) => Error;

const DAY = 86400_000;
const fmt = new Intl.DateTimeFormat('en-CA', { timeZone: META.org_timezone, year: 'numeric', month: '2-digit', day: '2-digit' });
const localDate = (t: number) => fmt.format(t);
const MONTH_RE = /^\d{4}-\d{2}$/;
const prevMonth = (m: string) => { const [y, n] = m.split('-').map(Number); return n === 1 ? `${y - 1}-12` : `${y}-${String(n - 1).padStart(2, '0')}`; };
const lastDay = (m: string) => new Date(Date.UTC(+m.slice(0, 4), +m.slice(5), 0)).getUTCDate();

const pct = (ok: number, total: number) => (total > 0 ? (100 * ok) / total : null);
const asc = (a: number | null, b: number | null) => (a == null ? (b == null ? 0 : 1) : b == null ? -1 : a - b);
const mean = (v: number[]) => (v.length ? v.reduce((s, x) => s + x, 0) / v.length : null);
/** 通知里的百分比（同服务端 pct_text）：不到 100 的最多写 99.99，没有写 - */
const pctText = (p: number | null) => (p == null ? '-' : p >= 100 ? '100%' : `${Math.min(p, 99.99).toFixed(2)}%`);

export function installReportsMock(R: Reg, machines: () => MachineView[], incidents: () => Incident[], channels: () => (NotifyChannel & { config: unknown })[], notifyLog: () => NotifyLogEntry[], nextId: () => string, bad: Bad) {
  const today = () => localDate(Date.now());
  const window = (month: string | null) => {
    const t = today(); const cur = t.slice(0, 7);
    const m = month || cur;
    if (!MONTH_RE.test(m) || +m.slice(5) < 1 || +m.slice(5) > 12) throw bad('month 写成 YYYY-MM，例如 2026-09');
    if (m > cur) throw bad('还没到这个月');
    const from = `${m}-01`; const to = m === cur ? t : `${m}-${String(lastDay(m)).padStart(2, '0')}`;
    return { month: m, from, to, days: Number(to.slice(8)) };
  };

  function uptime(month: string | null): UptimeReport {
    const w = window(month);
    const now = Date.now();
    const items = machines().filter((m) => m.kind !== 'relay' && m.lifecycle === 'active' && !m.spare).map((m): UptimeItem => {
      const n = Math.max(1, m.entries.filter((e) => e.lifecycle !== 'pending').length);
      // 窗口内有探测的天数：上架在窗口之后的没有；月内才上架的从上架那天起
      const start = m.activated_at ? localDate(Date.parse(m.activated_at)) : w.from;
      const days = start > w.to || (m.kind === 'proxy' && m.id.endsWith('2')) ? 0 : start > w.from ? Number(w.to.slice(8)) - Number(start.slice(8)) + 1 : w.days;
      const r = rng(hashStr(`${m.id}:${w.month}`));
      const down = m.health.status === 'down' && w.month === today().slice(0, 7);
      const kind = r();
      const lossy = down ? 0.02 + r() * 0.05 : kind < 0.10 ? 0.0005 + r() * 0.004 : kind < 0.2 ? 0.0002 + r() * 0.0008 : 0;
      const tcpTotal = Math.round(2880 * n * days); const egTotal = Math.round(1440 * n * days);
      const tcpOk = Math.round(tcpTotal * (1 - lossy)); const egOk = Math.round(egTotal * (1 - lossy * 0.6));
      const bad = tcpTotal - tcpOk;
      const outages = !tcpTotal ? 0 : down ? 4 + Math.floor(r() * 4) : bad === 0 ? 0 : 1 + Math.floor(r() * 3);
      const inc = incidents().filter((i) => (i.subject?.id ?? i.subject_id) === m.id && now - Date.parse(i.opened_at) <= 30 * DAY).length;
      return {
        id: m.id, name: m.name, team: m.team, provider: m.provider ? { id: m.provider.id, name: m.provider.name } : null,
        tcp_total: tcpTotal, tcp_ok: tcpOk, tcp_pct: pct(tcpOk, tcpTotal), egress_total: egTotal, egress_ok: egOk, egress_pct: pct(egOk, egTotal),
        outages, longest_outage_h: outages ? (down ? 3 + Math.floor(r() * 9) : 1 + Math.floor(r() * 2)) : 0, incidents_30d: inc,
      };
    });
    items.sort((a, b) => asc(a.tcp_pct, b.tcp_pct) || asc(a.egress_pct, b.egress_pct));
    const groups = new Map<string | null, { name: string; its: UptimeItem[] }>();
    for (const it of items) {
      const id = it.provider?.id ?? null;
      (groups.get(id) ?? groups.set(id, { name: it.provider?.name ?? '未填供应商', its: [] }).get(id)!).its.push(it);
    }
    const providers = [...groups].map(([id, { name, its }]): UptimeProvider => {
      const tcp = its.filter((i) => i.tcp_pct != null);
      const eg = its.map((i) => i.egress_pct).filter((x): x is number => x != null);
      return {
        id, name, nodes: its.length, with_data: tcp.length, avg_tcp_pct: mean(tcp.map((i) => i.tcp_pct!)), worst_tcp_pct: tcp[0]?.tcp_pct ?? null, worst: tcp[0] ? { id: tcp[0].id, name: tcp[0].name } : null,
        avg_egress_pct: mean(eg), worst_egress_pct: eg.length ? Math.min(...eg) : null, outages: its.reduce((s, i) => s + i.outages, 0), incidents_30d: its.reduce((s, i) => s + i.incidents_30d, 0),
      };
    }).sort((a, b) => asc(a.avg_tcp_pct, b.avg_tcp_pct) || a.name.localeCompare(b.name));
    return { month: w.month, days: w.days, from: w.from, to: w.to, tz: META.org_timezone, items, providers };
  }
  R('GET', '/reports/uptime', ({ query }) => {
    const month = query.get('month'); const days = query.get('days');
    if (month && days) throw bad('month 和 days 只能给一个');
    if (days) {
      const n = Number(days);
      if (!(n >= 1 && n <= 90)) throw bad('days 只能是 1–90');
      return { ...uptime(null), month: null, days: n };
    }
    return uptime(month);
  });

  const money = (c: number) => `$${(c / 100).toFixed(2)}`;
  const gb = (b: number) => (b >= 1e12 ? `${(b / 1e12).toFixed(2)} TB` : b >= 1e9 ? `${(b / 1e9).toFixed(2)} GB` : `${(b / 1e6).toFixed(1)} MB`);
  function monthly(month: string | null): MonthlyReport {
    const m = month || prevMonth(today().slice(0, 7));
    const w = window(m);
    const up = uptime(m);
    const active = machines().filter((x) => x.lifecycle === 'active' && !x.spare);
    const costly = active.filter((x) => x.kind !== 'relay').map((x) => ({ x, usd: mockMonthly(x), g: mockUsage(x).bytes30 / 1e9 }))
      .filter((c) => c.usd != null && c.g >= 1).map((c) => ({ id: c.x.id, name: c.x.name, monthly_usd_cents: c.usd!, gb: c.g, usd_per_gb: c.usd! / 100 / c.g }))
      .sort((a, b) => b.usd_per_gb - a.usd_per_gb).slice(0, 3);
    const worst = up.items.filter((i) => i.tcp_pct != null).slice(0, 3).map((i) => ({ id: i.id, name: i.name, tcp_pct: i.tcp_pct!, egress_pct: i.egress_pct, outages: i.outages, longest_outage_h: i.longest_outage_h }));
    const ruleCount = new Map<string, number>();
    for (const i of incidents()) ruleCount.set(i.rule, (ruleCount.get(i.rule) ?? 0) + 1);
    const top_rules = [...ruleCount].sort((a, b) => b[1] - a[1]).slice(0, 3).map(([rule, count]) => ({ rule, name: META.rules[rule] ?? rule, count }));
    const cents = active.reduce((s, x) => s + (mockMonthly(x) ?? 0), 0);
    const down = Math.round(2.4e12 * (0.8 + (hashStr(m) % 40) / 100)); const upb = Math.round(down * 0.11);
    const r: Omit<MonthlyReport, 'text'> = {
      month: m, from: w.from, to: w.to, tz: META.org_timezone, traffic: { down, up: upb, total: down + upb }, monthly_usd: { cents, unconverted: [] }, machines: active.length,
      costly, worst_uptime: worst, top_rules, due_soon: active.filter((x) => x.subscription?.due_state === 'soon').length, overdue: active.filter((x) => x.subscription?.due_state === 'overdue').length,
    };
    const list = (title: string, rows: string[]) => (rows.length ? [`${title}：`, ...rows.map((s, i) => `${i + 1}. ${s}`)] : [`${title}：无`]);
    const text = [
      `${r.month.slice(0, 4)} 年 ${Number(r.month.slice(5))} 月（${r.from} 至 ${r.to}，${META.org_timezone_label}）`,
      `总流量：${gb(r.traffic.total)}（下载 ${gb(r.traffic.down)}，上传 ${gb(r.traffic.up)}）`,
      `月费合计：${money(cents)}（在用 ${r.machines} 台，按当前订阅折成 USD）`,
      ...list('USD/GB 最高的 3 台', costly.map((c) => `${c.name} $${c.usd_per_gb.toFixed(3)}/GB（月费 ${money(c.monthly_usd_cents)}，流量 ${c.gb.toFixed(1)} GB）`)),
      ...list('可用率最差的 3 台（TCP 探测成功率）', worst.map((x) => `${x.name} ${pctText(x.tcp_pct)}（出口 ${pctText(x.egress_pct)}）${x.outages ? `，断线 ${x.outages} 次、最长 ${x.longest_outage_h} 小时` : ''}`)),
      ...list('告警次数前 3 的规则', top_rules.map((x) => `${x.name} ${x.count} 次`)),
      `即将到期：${r.due_soon} 台（已过期 ${r.overdue} 台）`,
    ].join('\n');
    return { ...r, text };
  }
  R('GET', '/reports/monthly', ({ query }) => monthly(query.get('month')));
  R('POST', '/reports/monthly/send', ({ body }) => {
    const to = channels().filter((c) => c.enabled && c.monthly_report);
    if (!to.length) throw bad('没有开启「月报」的通知渠道：在设置的通知渠道里勾选「月报」（并且渠道已启用）', 409, 'conflict');
    const r = monthly((body as { month?: string } | null)?.month ?? null);
    for (const c of to) notifyLog().unshift({ id: nextId(), channel_id: c.id, channel_name: c.name, incident_id: null, ts: new Date().toISOString(), ok: true, error: null, kind: 'report', subject_name: '月报', rule_name: null });
    return { month: r.month, channels: to.length, text: r.text };
  });
}
