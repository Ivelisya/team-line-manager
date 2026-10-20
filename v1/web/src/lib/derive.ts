// 从 MachineView 推导卡片/表格要显示的数据（纯函数，便于复用）
import type { MachineView, Plan, SeriesPoint } from './types';
import * as F from './format';

const GiB = 1024 ** 3;

export const hasHost = (m: MachineView) => m.kind !== 'proxy' && m.host != null;
/** 数据过期：agent 离线，或健康为故障 */
export const isQuiet = (m: MachineView) => m.health.status === 'down' || (m.agent != null && !m.agent.online) || m.lifecycle === 'retired';

export function entryText(m: MachineView): string {
  if (m.kind === 'relay') return `${m.entries.length} 条入口`;
  const e = m.entries[0];
  if (!e) return '—';
  return `${e.relay.name}:${e.port}${m.entries.length > 1 ? ` +${m.entries.length - 1}` : ''}`;
}
export function monthTraffic(m: MachineView) {
  let up = 0; let down = 0; let tu = 0; let td = 0;
  for (const e of m.entries) { up += e.traffic.month_up; down += e.traffic.month_down; tu += e.traffic.today_up; td += e.traffic.today_down; }
  return { up, down, today_up: tu, today_down: td };
}
/** 流量额度：上限（GiB，0/null = 不限）与已用（入口业务流量合计） */
export function quota(m: MachineView) {
  const limit = m.subscription?.traffic_gb ?? m.plan?.traffic_gb ?? 0;
  const t = monthTraffic(m); const used = t.up + t.down;
  return { limit, used, pct: limit > 0 ? (used / (limit * GiB)) * 100 : null, rem: limit > 0 ? Math.max(0, limit * GiB - used) : null };
}
export function resourceMeters(m: MachineView, plans: Plan[]) {
  const h = m.host; if (!h) return null;
  const cores = plans.find((p) => p.id === m.plan?.id)?.cpu_cores || 2;
  const memPct = (h.mem_used / h.mem_total) * 100; const diskPct = (h.disk_used / h.disk_total) * 100;
  const usedTotal = (u: number, t: number) => `${F.bytes(u).v} ${F.bytes(u).u} / ${F.bytes(t).v} ${F.bytes(t).u}`;
  return {
    cpu: { value: F.pct(h.cpu_pct), pct: h.cpu_pct, sub: `${cores} 核` },
    mem: { value: F.pct(memPct), pct: memPct, sub: usedTotal(h.mem_used, h.mem_total) },
    disk: { value: F.pct(diskPct), pct: diskPct, sub: usedTotal(h.disk_used, h.disk_total) },
    load: { value: { v: h.load1.toFixed(2), u: '' } as F.Fmt, pct: (h.load1 / cores) * 100, sub: `1 分钟 · ${h.tcp_conns} 连接` },
  };
}
/** 探测延迟 → 心跳格 */
export function beatCells(points: SeriesPoint[] | undefined, n = 20): ('ok' | 'warn' | 'bad' | null)[] {
  if (!points) return [];
  return points.slice(-n).map((p) => { const v = p.tcp_ms as number | null; return p.tcp_ok === false || v == null ? 'bad' : v >= 150 ? 'warn' : 'ok'; });
}
/** 入口探测摘要：最差 TCP 延迟与出口延迟 */
export function probeSummary(m: MachineView) {
  const es = m.entries.filter((e) => e.lifecycle === 'active' || e.lifecycle === 'provisioning');
  const tcpOk = es.filter((e) => e.tcp?.ok).length;
  const lat = es.map((e) => e.tcp?.latency_ms).filter((x): x is number => x != null);
  const eg = es.map((e) => e.egress?.latency_ms).filter((x): x is number => x != null);
  return { total: es.length, tcpOk, tcp: lat.length ? Math.round(lat.reduce((a, b) => a + b, 0) / lat.length) : null, egress: eg.length ? Math.round(eg.reduce((a, b) => a + b, 0) / eg.length) : null };
}
export const subLabel = (m: MachineView) => {
  const s = m.subscription; if (!s) return null;
  return `${F.money(s.price_cents, s.currency)}/${F.cycleLabel(s.cycle)}`;
};
