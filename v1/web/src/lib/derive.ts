// 从 MachineView 推导卡片/表格要显示的数据（纯函数，便于复用）
import type { MachineView, ProbeBucket, Subscription } from './types';
import * as F from './format';

const GiB = 1024 ** 3;

export const hasHost = (m: MachineView) => m.kind !== 'proxy' && m.host != null;
/** 数据过期：agent 离线（没有近期上报）或已退役。和健康无关：有严重告警的机器照常上报，读数照常显示 */
export const isQuiet = (m: MachineView) => (m.agent != null && !m.agent.online) || m.lifecycle === 'retired';
/** 当前网卡速率：无主机或数据过期（isQuiet）时为 null——各处显示 “—”，也不进迹线 */
export const liveRate = (m: MachineView) => (hasHost(m) && !isQuiet(m) ? { rx: m.host!.net_rx_bps, tx: m.host!.net_tx_bps } : null);
/** 容器（LXC 等）里 /proc/loadavg 是宿主机的，agent 不报负载（null） */
const LOAD_NA = '容器内无法获取本机负载';

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
/** 流量额度：上限（GiB，订阅没填按套餐，0/null = 不限）与本周期已用（服务端按流量重置日算，见 MachineView.quota） */
export function quota(m: MachineView) {
  const limit = m.subscription?.traffic_gb ?? m.plan?.traffic_gb ?? 0;
  const { used, period_start: start } = m.quota;
  return { limit, used, start, pct: limit > 0 ? (used / (limit * GiB)) * 100 : null, rem: limit > 0 ? Math.max(0, limit * GiB - used) : null };
}
/** 资源占用的告警色（CPU / 内存 / 磁盘 / 负载的格条、圆环、迷你条共用）：≥ 90% 红，≥ 75% 黄 */
export const usageTone = (pct: number | null | undefined) => (pct == null ? undefined : pct >= 90 ? 'bad' : pct >= 75 ? 'warn' : undefined);
const ratio = (used: number | null, total: number | null) => (used == null || total == null || !(total > 0) ? null : (used / total) * 100);
const usedTotal = (u: number | null, t: number | null) => (u == null || t == null ? '—' : `${F.str(F.bytes(u))} / ${F.str(F.bytes(t))}`);
/** 资源读数；指标缺测（null）时对应项显示 “—”，进度为 null（不会出现 NaN） */
export function resourceMeters(m: MachineView) {
  const h = m.host; if (!h) return null;
  // 核数只用 agent 报的（容器里是限额）；没报过时负载折不成百分比，显示 “—”
  const cores = m.agent?.cores ?? null;
  const memPct = ratio(h.mem_used, h.mem_total); const diskPct = ratio(h.disk_used, h.disk_total);
  const loadPct = h.load1 == null || !cores ? null : (h.load1 / cores) * 100;
  return {
    cpu: { value: F.pct(h.cpu_pct), pct: h.cpu_pct, sub: cores ? `${cores} 核` : undefined },
    mem: { value: F.pct(memPct), pct: memPct, sub: usedTotal(h.mem_used, h.mem_total) },
    disk: { value: F.pct(diskPct), pct: diskPct, sub: usedTotal(h.disk_used, h.disk_total) },
    // 负载按核数折成百分比（1 分钟平均负载 / 核数）：比裸的 “0.50” 好懂；超过 100% 表示排队，数字照实显示，格条封顶
    load: { value: F.pct(loadPct), pct: loadPct, sub: h.load1 == null ? LOAD_NA : `1 分钟 ${h.load1.toFixed(2)}${cores ? ` · ${cores} 核` : ''}`, hint: h.load1 == null ? LOAD_NA : undefined },
  };
}
export type BeatCell = 'ok' | 'warn' | 'bad' | null;
/**
 * 探测桶 → 心跳格（20 格 = 近 1 小时，旧→新）。按桶内成功比例定色（线路机是它所有入口的合计，个别节点抖动只会变黄）：
 * 没有探测为空格（灰）；< 50% 红；< 95% 或平均延迟 ≥ 150 ms 黄；其余绿。
 */
export const beatCells = (buckets: ProbeBucket[] | undefined): BeatCell[] =>
  (buckets ?? []).map(([n, ok, ms]) => { if (!n) return null; const r = ok / n; return r < 0.5 ? 'bad' : r < 0.95 || (ms ?? 0) >= 150 ? 'warn' : 'ok'; });
const BEAT_MS = 3 * 60_000;
const BEAT_LEGEND = '绿：全部成功 · 黄：有失败或平均延迟 ≥ 150 ms · 红：一半以上失败 · 灰：没有探测';
/** 心跳格的悬停说明（与 beatCells 一一对应）：这 3 分钟的时段、探测次数与成功率、平均延迟，第二行是颜色含义。
 *  桶按墙钟 3 分钟对齐，最后一格是当前这格（见 API.md /probe-beats） */
export function beatTitles(buckets: ProbeBucket[] | undefined, now = Date.now()): string[] {
  const bs = buckets ?? [];
  const cur = Math.floor(now / BEAT_MS) * BEAT_MS;
  const hm = (t: number) => { const d = new Date(t); return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`; };
  return bs.map(([n, ok, ms], i) => {
    const t0 = cur - (bs.length - 1 - i) * BEAT_MS;
    const what = !n ? '没有探测' : `探测 ${n} 次，成功 ${ok} 次（${Math.round((ok / n) * 100)}%）${ms != null ? ` · 平均 ${Math.round(ms)} ms` : ''}`;
    return `${hm(t0)}–${hm(t0 + BEAT_MS)}${i === bs.length - 1 ? '（进行中）' : ''} · ${what}\n${BEAT_LEGEND}`;
  });
}
/** 入口探测摘要：平均 TCP 延迟与出口延迟。只算 active 的入口（同服务端健康规则）：应用中的还没有探测 */
export function probeSummary(m: MachineView) {
  const es = m.entries.filter((e) => e.lifecycle === 'active');
  const tcpOk = es.filter((e) => e.tcp?.ok).length;
  const lat = es.map((e) => e.tcp?.latency_ms).filter((x): x is number => x != null);
  const eg = es.map((e) => e.egress?.latency_ms).filter((x): x is number => x != null);
  return { total: es.length, tcpOk, tcp: lat.length ? Math.round(lat.reduce((a, b) => a + b, 0) / lat.length) : null, egress: eg.length ? Math.round(eg.reduce((a, b) => a + b, 0) / eg.length) : null };
}
/**
 * 剩余价值：已付还没用掉的部分，按天折算。每期天数 = 到期日往前推一个周期的天数，剩余比例 = 距到期天数 / 每期天数；
 * 预付了多期时比例大于 1；已过期为 0。
 */
export function remainingValue(s: Subscription | null) {
  if (!s) return null;
  const months = F.CYCLE_MONTHS[s.cycle];
  const days = Math.round((Date.parse(s.due_date) - Date.parse(F.addMonths(s.due_date, -months))) / 86_400_000);
  const frac = Math.max(0, s.due_days / days);
  return { cents: Math.round(s.price_cents * frac), currency: s.currency, frac, days };
}

/** 标签配色：固定语义的用固定色（类型：线路机青、落地机紫、家宽橙；维护中琥珀），
 *  其余（供应商代号、服务编号）按文字散列到剩下的几种颜色，同一个代号永远同色、也不会和类型标签撞色 */
const TAG_COLORS = ['blue', 'pink', 'cyan', 'green'] as const;
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
