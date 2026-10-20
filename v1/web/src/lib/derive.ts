// 从 MachineView 推导卡片/表格要显示的数据（纯函数，便于复用）
import type { MachineView, Plan, SeriesPoint } from './types';
import * as F from './format';

const GiB = 1024 ** 3;

export const hasHost = (m: MachineView) => m.kind !== 'proxy' && m.host != null;
/** 数据过期：agent 离线，或健康为故障 */
export const isQuiet = (m: MachineView) => m.health.status === 'down' || (m.agent != null && !m.agent.online) || m.lifecycle === 'retired';
/** 当前网卡速率：无主机或数据过期（isQuiet）时为 null——各处显示 “—”，也不进迹线 */
export const liveRate = (m: MachineView) => (hasHost(m) && !isQuiet(m) ? { rx: m.host!.net_rx_bps, tx: m.host!.net_tx_bps } : null);
/** 容器（LXC 等）里 /proc/loadavg 是宿主机的，agent 不报负载（null） */
export const LOAD_NA = '容器内无法获取本机负载';

export function entryText(m: MachineView): string {
  if (m.kind === 'relay') return `${m.entries.length} 条入口`;
  const e = m.entries[0];
  if (!e) return '—';
  return `${e.relay.name}:${e.port}${m.entries.length > 1 ? ` +${m.entries.length - 1}` : ''}`;
}
export function monthTraffic(m: MachineView) {
  const r = m.removed_traffic; // 换线后旧入口的流量仍算在本月里
  let up = r.month_up; let down = r.month_down; let tu = r.today_up; let td = r.today_down;
  for (const e of m.entries) { up += e.traffic.month_up; down += e.traffic.month_down; tu += e.traffic.today_up; td += e.traffic.today_down; }
  return { up, down, today_up: tu, today_down: td };
}
/** 流量额度：上限（GiB，0/null = 不限）与已用（入口业务流量合计） */
export function quota(m: MachineView) {
  const limit = m.subscription?.traffic_gb ?? m.plan?.traffic_gb ?? 0;
  const t = monthTraffic(m); const used = t.up + t.down;
  return { limit, used, pct: limit > 0 ? (used / (limit * GiB)) * 100 : null, rem: limit > 0 ? Math.max(0, limit * GiB - used) : null };
}
const ratio = (used: number | null, total: number | null) => (used == null || total == null || !(total > 0) ? null : (used / total) * 100);
const usedTotal = (u: number | null, t: number | null) => (u == null || t == null ? '—' : `${F.bytes(u).v} ${F.bytes(u).u} / ${F.bytes(t).v} ${F.bytes(t).u}`);
/** 资源读数；指标缺测（null）时对应项显示 “—”，进度为 null（不会出现 NaN） */
export function resourceMeters(m: MachineView, plans: Plan[]) {
  const h = m.host; if (!h) return null;
  const cores = plans.find((p) => p.id === m.plan?.id)?.cpu_cores || 2;
  const memPct = ratio(h.mem_used, h.mem_total); const diskPct = ratio(h.disk_used, h.disk_total);
  const loadPct = h.load1 == null ? null : (h.load1 / cores) * 100;
  return {
    cpu: { value: F.pct(h.cpu_pct), pct: h.cpu_pct, sub: `${cores} 核` },
    mem: { value: F.pct(memPct), pct: memPct, sub: usedTotal(h.mem_used, h.mem_total) },
    disk: { value: F.pct(diskPct), pct: diskPct, sub: usedTotal(h.disk_used, h.disk_total) },
    // 负载按核数折成百分比（1 分钟平均负载 / 核数）：比裸的 “0.50” 好懂；超过 100% 表示排队，数字照实显示，格条封顶
    load: { value: F.pct(loadPct), pct: loadPct, sub: h.load1 == null ? LOAD_NA : `1 分钟 ${h.load1.toFixed(2)} · ${cores} 核`, hint: h.load1 == null ? LOAD_NA : undefined },
  };
}
/**
 * 探测 → 心跳格。取最近 60 个 1 分钟点，每 3 点聚成一格，共 20 格——对应“近 1 小时”。
 * 按格内 TCP 探测成功比例定色（线路机是它所有入口的合计，个别节点抖动只会变黄）：≥95% 且不慢为绿，≥50% 为黄，否则红。
 * 点数不足 60 时从最新一端对齐，早的一端留空。
 */
export function beatCells(points: SeriesPoint[] | undefined, n = 20, minutes = 60): ('ok' | 'warn' | 'bad' | null)[] {
  if (!points?.length) return [];
  const size = Math.max(1, Math.ceil(minutes / n));
  const pts = points.slice(-n * size);
  const cells: ('ok' | 'warn' | 'bad')[] = [];
  for (let end = pts.length; end > 0; end -= size) {
    let ok = 0; let total = 0; let slow = false;
    for (const p of pts.slice(Math.max(0, end - size), end)) {
      ok += Number(p.tcp_ok_n ?? 0); total += Number(p.tcp_n ?? 0);
      if (Number(p.tcp_ms ?? 0) >= 150) slow = true;
    }
    const r = total ? ok / total : 0;
    cells.unshift(r < 0.5 ? 'bad' : r < 0.95 || slow ? 'warn' : 'ok');
  }
  return cells;
}
/** 入口探测摘要：最差 TCP 延迟与出口延迟 */
export function probeSummary(m: MachineView) {
  const es = m.entries.filter((e) => e.lifecycle === 'active' || e.lifecycle === 'provisioning');
  const tcpOk = es.filter((e) => e.tcp?.ok).length;
  const lat = es.map((e) => e.tcp?.latency_ms).filter((x): x is number => x != null);
  const eg = es.map((e) => e.egress?.latency_ms).filter((x): x is number => x != null);
  return { total: es.length, tcpOk, tcp: lat.length ? Math.round(lat.reduce((a, b) => a + b, 0) / lat.length) : null, egress: eg.length ? Math.round(eg.reduce((a, b) => a + b, 0) / eg.length) : null };
}
/**
 * 剩余价值：当前已付周期还没用掉的部分，按天折算。本期 = [到期日 − 一个周期, 到期日)，剩余比例 = 距到期天数 / 本期天数。
 * 一次性付费没有周期，不折算（null）；已过期为 0。
 */
export function remainingValue(m: MachineView) {
  const s = m.subscription;
  const months = s && F.CYCLE_MONTHS[s.cycle];
  if (!s || !months) return null;
  const days = Math.round((Date.parse(s.due_date) - Date.parse(F.addMonths(s.due_date, -months))) / 86_400_000);
  const frac = Math.max(0, Math.min(1, s.due_days / days));
  return { cents: Math.round(s.price_cents * frac), currency: s.currency, frac, days };
}

/** 标签配色：固定语义的用固定色（类型：线路机青、落地机紫、家宽橙；维护中琥珀），
 *  其余（供应商代号、服务编号）按文字散列到剩下的几种颜色，同一个代号永远同色、也不会和类型标签撞色 */
export const TAG_COLORS = ['blue', 'pink', 'cyan', 'green'] as const;
export type TagColor = (typeof TAG_COLORS)[number] | 'gray' | 'teal' | 'violet' | 'orange' | 'amber';
export function tagColor(text: string): TagColor {
  let h = 0;
  for (const ch of text) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  return TAG_COLORS[h % TAG_COLORS.length];
}

export const subLabel = (m: MachineView) => {
  const s = m.subscription; if (!s) return null;
  return `${F.money(s.price_cents, s.currency)}/${F.cycleLabel(s.cycle)}`;
};
