// 从 MachineView 推导卡片/表格要显示的数据（纯函数，便于复用）
import type { AvailabilityDay, MachineView, ProbeBucket, ProbeResult, Subscription } from './types';
import * as F from './format';

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
/** 流量额度按供应商的十进制 GB（1 GB = 10⁹ 字节） */
const GB = 1e9;
/** 流量额度：上限（GB，订阅没填按套餐，0/null = 不限）与本周期已用（服务端按网卡 / 业务流量、流量重置日算，见 MachineView.quota）。
 *  shared：同机 IP 节点自己没设上限——额度与宿主机共用（宿主机的网卡含它的流量），不是「不限」 */
export function quota(m: MachineView) {
  const limit = m.subscription?.traffic_gb ?? m.plan?.traffic_gb ?? 0;
  const { used, period_start: start, rule, base_at: baseAt } = m.quota;
  return { limit, used, start, rule, baseAt, pct: limit > 0 ? (used / (limit * GB)) * 100 : null, rem: limit > 0 ? Math.max(0, limit * GB - used) : null, shared: !!m.host_machine && !(limit > 0) };
}
/** 额度用量的写法同供应商面板：十进制 GB，不换算成 TB（901.6 GB）。业务流量等其它字节数仍用 F.bytes */
export const quotaGb = (bytes: number): F.Fmt => {
  const x = Math.max(0, bytes) / GB;
  return { v: (x < 10 ? x.toFixed(2) : x.toFixed(1)).replace(/\.?0+$/, ''), u: 'GB' };
};
/** 额度上限（GB 数）：0 / 空 = 不限 */
export const limitGb = (gb: number | null | undefined): F.Fmt => (gb && gb > 0 ? { v: String(gb), u: 'GB' } : { v: '不限', u: '' });
/** 网卡计量规则（供应商的「流量计量」）；null = 没有网卡读数，按业务流量 */
export const RULE_LABEL: Record<string, string> = { min: '网卡入出取小', max: '网卡入出取大', sum: '网卡入出相加', out: '只算网卡出站' };
/** 同机关系标的文字与悬停说明（卡片、环形卡、机器表）：同机 IP 节点「同机 · 宿主机名」（short：窄处只写「同机」，宿主机名在说明里），
 *  宿主机「+N IP」；没有同机关系为 null */
export function hostRel(host: MachineView['host_machine'], nodes: MachineView['ip_nodes'], short?: boolean) {
  if (host) return { text: short ? '同机' : `同机 · ${host.name}`, tip: `同机 IP 节点：与宿主机「${host.name}」是同一台机器，CPU / 内存 / 磁盘与网卡读数都和宿主机共用` };
  if (nodes.length) return { text: `+${nodes.length} IP`, tip: `这台机器上还有 ${nodes.length} 个同机 IP 节点：${nodes.map((x) => x.name).join('、')}` };
  return null;
}
/** 流量额度吃紧（首页 KPI）：本周期已用 ≥ 80% 的台数（没设额度的不计），和用得最多的那台（已用百分比向下取整）；没有吃紧的 top 为 null */
export function quotaTight(list: MachineView[]) {
  let n = 0; let top: { name: string; pct: number } | null = null;
  for (const m of list) {
    const { pct } = quota(m);
    if (pct == null || pct < 80) continue;
    n++;
    if (!top || pct > top.pct) top = { name: m.name, pct };
  }
  return { n, top: top && { name: top.name, pct: Math.floor(top.pct) } };
}
/** 资源占用的告警色（CPU / 内存 / 磁盘 / 负载的格条、圆环、迷你条共用）：≥ 90% 红，≥ 75% 黄 */
export const usageTone = (pct: number | null | undefined) => (pct == null ? undefined : pct >= 90 ? 'bad' : pct >= 75 ? 'warn' : undefined);
const SEVERITY = { critical: { tone: 'bad', label: '严重' }, warning: { tone: 'warn', label: '警告' }, info: { tone: 'info', label: '提示' } } as const;
/** 告警级别的色调与中文名（同告警页的级别列）；没见过的级别按警告 */
export const severityOf = (s: string) => SEVERITY[s as keyof typeof SEVERITY] ?? SEVERITY.warning;
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
const BEAT_LEGEND = '绿：成功 ≥ 95% 且平均 < 150 ms · 黄：成功 50%–95% 或平均 ≥ 150 ms\n红：成功不到一半 · 灰：没有探测';
/** 心跳格的悬停说明（与 beatCells 一一对应）：这 3 分钟的时段、探测次数与成功率、平均延迟，后两行是颜色含义（阈值同 beatCells）。
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
/** 成功率的写法（近 30 天可用率）：全部成功才写 100%，否则最多写到 99.99%（1439/1440 不会四舍五入成 100%）；没有探测为「—」 */
export const successPct = (p: number | null) => (p == null ? '—' : p >= 100 ? '100%' : `${Math.min(p, 99.99).toFixed(2)}%`);
/** 一天里一种探测的档位：成功 ≥ 99.5% 绿、≥ 95% 黄、更低红（整数比较，正好落在边界上不受浮点误差影响）；没有探测为 null */
const dayTone = (ok: number, total: number): BeatCell => (!total ? null : ok * 1000 >= total * 995 ? 'ok' : ok * 100 >= total * 95 ? 'warn' : 'bad');
const WORST_FIRST = ['bad', 'warn', 'ok'] as const;
/** 近 30 天可用率的每日格（旧→新）：取 TCP 与出口两种探测里较差的一档，当天没有探测为空格 */
export const availCells = (days: AvailabilityDay[]): BeatCell[] =>
  days.map((d) => { const a = dayTone(d.tcp_ok, d.tcp_total); const b = dayTone(d.egress_ok, d.egress_total); return WORST_FIRST.find((t) => t === a || t === b) ?? null; });
const AVAIL_LEGEND = '绿：成功 ≥ 99.5% · 黄：成功 95%–99.5%\n红：成功不到 95% · 灰：没有探测（TCP 与出口取较差的一种）';
/** 每日格的悬停说明（与 availCells 一一对应）：「10-21 · TCP 99.93%（1438/1440） · 出口 100%（288/288）」，没有探测写「没有探测」；
 *  最后一格是今天（还没过完）；后两行是颜色含义（阈值同 availCells） */
export function availTitles(days: AvailabilityDay[]): string[] {
  const part = (label: string, ok: number, total: number) => (total ? `${label} ${successPct((ok / total) * 100)}（${ok}/${total}）` : '');
  return days.map((d, i) => {
    const what = [part('TCP', d.tcp_ok, d.tcp_total), part('出口', d.egress_ok, d.egress_total)].filter(Boolean).join(' · ') || '没有探测';
    return `${d.date.slice(5)}${i === days.length - 1 ? '（今天，还没过完）' : ''} · ${what}\n${AVAIL_LEGEND}`;
  });
}
/** 有探测、却一个都没成功（延迟为 null）：显示「失败」，和「—」（还没有探测）区分开 */
const allFailed = (ps: (ProbeResult | null)[]) => ps.some((p) => p && !p.ok) && !ps.some((p) => p?.ok);
/** 入口探测摘要：平均 TCP 延迟与出口延迟，tcpFail / egressFail = 全部失败。只算 active 的入口（同服务端健康规则）：应用中的还没有探测。
 *  直出入口没有节点、不做 TCP 探测：TCP 的连通数（total / tcpOk）与延迟只算经节点的入口，出口照算 */
export function probeSummary(m: MachineView) {
  const es = m.entries.filter((e) => e.lifecycle === 'active');
  const viaNode = es.filter((e) => !e.direct);
  const tcpOk = viaNode.filter((e) => e.tcp?.ok).length;
  const lat = viaNode.map((e) => e.tcp?.latency_ms).filter((x): x is number => x != null);
  const eg = es.map((e) => e.egress?.latency_ms).filter((x): x is number => x != null);
  return {
    total: viaNode.length, tcpOk, tcp: lat.length ? Math.round(lat.reduce((a, b) => a + b, 0) / lat.length) : null, egress: eg.length ? Math.round(eg.reduce((a, b) => a + b, 0) / eg.length) : null,
    tcpFail: allFailed(viaNode.map((e) => e.tcp)), egressFail: allFailed(es.map((e) => e.egress)),
  };
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

/** 标签配色：固定语义的用固定色（类型：线路机青、家宽橙；同机关系紫；维护中琥珀），
 *  供应商按代号散列到剩下的几种颜色，同一家永远同色、也不会和类型标签撞色 */
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
