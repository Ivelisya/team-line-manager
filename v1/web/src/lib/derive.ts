// 从 MachineView 推导卡片/表格要显示的数据（纯函数，便于复用）
import type { AvailabilityDay, DueState, EntryView, MachineView, Meta, Plan, ProbeBucket, ProbeResult, Provider, SubField, Subscription, TrafficReset, UsageMetric } from './types';
import * as F from './format';
import { tzLabel } from './tz';

// ── /meta：中文名、枚举、阈值、默认值只认服务端这一份 ──
// 外壳等 /meta 取回（useMeta 的 queryFn 里 setMeta）之后才渲染页面，所以下面这些同步读取在页面里总有值；登录页不用它们
let META: Meta | null = null;
export const setMeta = (m: Meta) => { META = m; };
export function meta(): Meta {
  if (!META) throw new Error('/meta 还没取回');
  return META;
}
/** 查中文名；没收录的原样返回 */
const lbl = (t: Record<string, string>, k: string) => t[k] ?? k;
export const kindLabel = (k: string) => lbl(meta().kinds, k);
export const lifecycleLabel = (k: string) => lbl(meta().lifecycles, k);
export const healthLabel = (k: string) => lbl(meta().healths, k);
export const entryLabel = (k: string) => lbl(meta().entry_states, k);
export const jobKindLabel = (k: string) => lbl(meta().job_kinds, k);
export const jobStateLabel = (k: string) => lbl(meta().job_states, k);
export const stepLabel = (k: string) => lbl(meta().step_states, k);
/** 告警规则的中文名 */
export const ruleLabel = (k: string) => lbl(meta().rules, k);
/** 网卡计量规则（供应商的「流量计量」） */
export const trafficRuleLabel = (k: string) => lbl(meta().traffic_rules, k);
/** 网卡计量规则写在费用页签「本周期已用」下面一行：「按网卡流量：入站和出站取较大值」（名字同供应商对话框，来自 /meta） */
export const trafficRulePlain = (k: string) => `按网卡流量：${trafficRuleLabel(k)}`;
/** 流量重置方式：自然月 / 账单日 */
export const trafficResetLabel = (k: string) => lbl(meta().traffic_resets, k);
/** 计费周期的那个字：月 / 季 / 半年 / 年 */
export const cycleLabel = (k: string) => meta().cycles.find((c) => c.key === k)?.label ?? k;
export const dueStateLabel = (k: string) => lbl(meta().due_states, k);
/** 告警级别的色调与中文名（同告警页的级别列）；没见过的级别按警告 */
export const severityOf = (s: string) => meta().severities[s] ?? meta().severities.warning ?? { tone: 'warn' as const, label: s };
/** 到期状态 → 色调：已过期红、即将到期黄 */
export const dueTone = (s: DueState | null | undefined): '' | 'warn' | 'bad' => (s === 'overdue' ? 'bad' : s === 'soon' ? 'warn' : '');
/** 下拉选项：计费周期（每月 / 每季 / …）、币种（常用的，带符号） */
export const cycleOptions = () => meta().cycles.map((c) => ({ value: c.key, label: `每${c.label}` }));
export const currencyOptions = () => meta().currencies.map((c) => ({ value: c, label: F.currencyLabel(c) }));

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
/** 本月业务流量合计（下载 + 上传）：卡片、表格、机器页按「本月业务流量」排序共用 */
export const trafficTotal = (m: MachineView) => { const t = monthTraffic(m); return t.up + t.down; };

/** 探测 / 应用的英文原始错误 → 中文（抽屉、卡片的健康原因、告警悬停卡片共用）。只换认得的片段，前面的中文（「出口不通：」）
 *  和认不出的原样留着。片段只吃 ASCII 字符，不会把前缀一起吞掉。
 *  旧 agent 报的是单词（「出口不通：timeout；refused」），新 agent 自己写中文（fleet-agent probe.rs 的 reason_cn），两种都认 */
const ERR_ZH: [RegExp, string | ((m: string, ...g: string[]) => string)][] = [
  [/\b(?:timed out|timeout)(?: after)? (\d+(?:\.\d+)?) ?s\b/gi, (_, n) => `超时 ${n} 秒`],
  [/\b(?:i\/o )?(?:timed out|timeout)\b/gi, '超时'],
  [/\b(?:connection )?refused\b/gi, '连接被拒绝'],
  [/\b(?:connection )?reset(?: by peer)?\b/gi, '连接被重置'],
  [/\b(?:connection (?:closed|aborted)|broken pipe)\b/gi, '连接被关闭'],
  [/\b(?:unexpected eof|end of file|eof)\b/gi, '对端提前断开'],
  [/\b(?:no route to host|(?:network|host) (?:is )?unreachable|unreachable)\b/gi, '网络不可达'],
  [/\bdns\b[\w .:'/-]*|[\w '/-]*\bname resolution\b[\w .:'/-]*|\bno such host\b/gi, '域名解析失败'],
  [/\b(?:tls|ssl)\b[\w .:'/-]*|\bcertificate\b[\w .:'/-]*/gi, 'TLS 握手失败'],
  [/\bsocks5?\b[\w .:'/-]*/gi, '代理握手失败'],
];
export const errText = (s: string) => ERR_ZH.reduce<string>((t, [re, to]) => t.replace(re, to as string), s).replace(/\s+$/, '');
/** 运行中的入口探测失败（直出入口不做 TCP 探测）：返回失败的那次探测。抽屉入口表、线路机页的状态列写「不通」，悬停看 errText(detail) */
export const probeFail = (e: EntryView) => (e.lifecycle !== 'active' || !e.applied ? null : !e.direct && e.tcp && !e.tcp.ok ? e.tcp : e.egress && !e.egress.ok ? e.egress : null);
/** 入口状态列的「不通」写明是哪一段：TCP 连不上入口，还是连上了但出口验收失败（抽屉的「入口探测 N/M 连通」只算 TCP） */
export const probeFailLabel = (e: EntryView) => (probeFail(e) === e.tcp ? 'TCP 不通' : '出口不通');
/** 状态筛选、状态列排序用的值：在用的看健康，其余看生命周期（同机器页状态筛选下拉的 value） */
export const statusOf = (m: MachineView) => (m.lifecycle === 'active' ? m.health.status : m.lifecycle);
/** 状态列升序：有问题的在前（上架失败、故障、降级…），正常的在后。机器页、首页表格的状态列与首页的「异常优先」共用 */
const STATUS_RANK: Record<string, number> = { failed: 0, down: 1, degraded: 2, unknown: 3, maintenance: 4, provisioning: 5, retiring: 6, ok: 7 };
export const statusRank = (m: MachineView) => STATUS_RANK[statusOf(m)] ?? 8;
/** 流量额度按供应商的十进制 GB（1 GB = 10⁹ 字节） */
const GB = 1e9;
/** 流量额度：上限（GB，服务端算好的 quota.limit_gb，0 = 不限）与本周期已用（服务端按网卡 / 业务流量、流量周期算，见 MachineView.quota）。
 *  shared：同机 IP 节点自己没设上限——额度与宿主机共用（宿主机的网卡含它的流量），不是「不限」。source 见 limit_source */
export function quota(m: MachineView) {
  // 旧接口（模拟数据）没有 limit_gb 时退回订阅 / 套餐的
  const limit = m.quota.limit_gb !== undefined ? m.quota.limit_gb ?? 0 : m.subscription?.traffic_gb ?? m.plan?.traffic_gb ?? 0;
  const { used, period_start: start, rule, base_at: baseAt } = m.quota;
  const shared = m.quota.limit_source ? m.quota.limit_source === 'host' : !!m.host_machine && !(limit > 0);
  return { limit, used, start, rule, baseAt, pct: limit > 0 ? (used / (limit * GB)) * 100 : null, rem: limit > 0 ? Math.max(0, limit * GB - used) : null, shared, source: m.quota.limit_source };
}

/** 订阅各项跟随谁：价格、币种、周期、流量上限跟随套餐，账单时区、提前提醒、流量重置日跟随供应商（同服务端 sub_template） */
export const SUB_FROM: Record<SubField, 'plan' | 'provider'> = {
  price_cents: 'plan', currency: 'plan', cycle: 'plan', traffic_gb: 'plan', billing_tz: 'provider', remind_days: 'provider', traffic_reset_day: 'provider',
};
/** 这一项单独设置了 */
export const isOverridden = (s: Subscription | null | undefined, k: SubField) => !!s?.overrides?.includes(k);
/** 跟随时的值（同服务端 subscription_eff）：重置日按供应商的方式——账单日 = 订阅的账单日，自然月 = 1 号。没有模板为 null。
 *  账单日同服务端 meta::billing_day：到期日（YYYY-MM-DD）落在现有 billing_day 上的（小月按月末，如 31 号的 2-28）沿用，否则是它几号 */
export function inheritedValue(m: MachineView, k: SubField, due?: string): string | number | null {
  const t = m.sub_template; if (!t) return null;
  if (k === 'traffic_reset_day') {
    if (t.traffic_reset !== 'billing' || !/^\d{4}-\d{2}-\d{2}$/.test(due ?? '')) return 1;
    const [y, mo, d] = due!.split('-').map(Number);
    const cur = m.subscription?.billing_day;
    return cur && d === Math.min(cur, new Date(Date.UTC(y, mo, 0)).getUTCDate()) ? cur : d;
  }
  return t[k];
}
/** 跟随的来源：「跟随套餐」「跟随供应商」，机器没有套餐 / 供应商时是「默认」 */
export const inheritSource = (m: MachineView, k: SubField) => (SUB_FROM[k] === 'plan' ? (m.plan ? '跟随套餐' : '默认') : (m.provider ? '跟随供应商' : '默认'));
/** 一项的值写成文字（订阅表单「跟随套餐：2000 GB」、上架页的跟随摘要共用）；currency 给价格配币种 */
export function subValueText(k: SubField, v: string | number | null, currency?: string, reset?: TrafficReset): string {
  if (v == null) return k === 'traffic_gb' ? '不限' : '—';
  switch (k) {
    case 'price_cents': return F.money(Number(v), currency ?? meta().defaults.currency);
    case 'cycle': return `每${cycleLabel(String(v))}`;
    case 'remind_days': return `提前 ${v} 天`;
    case 'billing_tz': return tzLabel(String(v));
    case 'traffic_gb': return Number(v) > 0 ? `${v} GB` : '不限';
    case 'traffic_reset_day': return reset === 'billing' ? `每月 ${v} 号（账单日）` : reset === 'month' ? `每月 ${v} 号（自然月）` : `每月 ${v} 号`;
    default: return String(v);
  }
}
/** 上架页选了套餐后的一行：「跟随套餐「X」」，订阅会跟随的值放进悬停说明 tip（同服务端 sub_template；供应商没有时是默认值，时区空 = 组织时区） */
export function inheritSummary(plan: Plan, pv: Provider | undefined) {
  const M = meta();
  const fee = `${subValueText('price_cents', plan.price_cents, plan.currency)} / ${cycleLabel(plan.cycle)}`;
  const reset = `按${lbl(M.traffic_resets, pv?.traffic_reset ?? M.defaults.traffic_reset)}重置`;
  return {
    text: `跟随套餐「${plan.name}」`,
    tip: `套餐：${fee} · 流量 ${subValueText('traffic_gb', plan.traffic_gb)}\n`
      + `${pv ? `供应商「${pv.name}」` : '默认'}：账单时区 ${pv?.timezone || M.org_timezone} · 流量${reset} · ${subValueText('remind_days', pv?.remind_days ?? M.defaults.remind_days)}提醒`,
  };
}
/** 「跟随套餐：2000 GB」：没有模板（旧接口）为 null */
export function inheritText(m: MachineView, k: SubField, due?: string) {
  const t = m.sub_template; if (!t) return null;
  const v = inheritedValue(m, k, due);
  if (k === 'price_cents' && v == null) return '没有套餐，需要单独填';
  return `${inheritSource(m, k)}：${subValueText(k, v, t.currency, t.traffic_reset)}`;
}
/** 额度用量的写法同供应商面板：十进制 GB，不换算成 TB（901.6 GB）。业务流量等其它字节数仍用 F.bytes */
export const quotaGb = (bytes: number): F.Fmt => {
  const x = Math.max(0, bytes) / GB;
  return { v: (x < 10 ? x.toFixed(2) : x.toFixed(1)).replace(/\.?0+$/, ''), u: 'GB' };
};
/** 额度上限（GB 数）：0 / 空 = 不限 */
export const limitGb = (gb: number | null | undefined): F.Fmt => (gb && gb > 0 ? { v: String(gb), u: 'GB' } : { v: '不限', u: '' });
/** 同机关系标的文字与悬停说明（卡片、环形卡、机器表）：同机 IP 节点「同机 · 宿主机名」（short：窄处只写「同机」，宿主机名在说明里），
 *  宿主机「+N IP」；没有同机关系为 null */
export function hostRel(host: MachineView['host_machine'], nodes: MachineView['ip_nodes'], short?: boolean) {
  if (host) return { text: short ? '同机' : `同机 · ${host.name}`, tip: `同机 IP 节点：与宿主机「${host.name}」是同一台机器，CPU / 内存 / 磁盘与网卡读数都和宿主机共用` };
  if (nodes.length) return { text: `+${nodes.length} IP`, tip: `这台机器上还有 ${nodes.length} 个同机 IP 节点：${nodes.map((x) => x.name).join('、')}` };
  return null;
}
/** 流量额度吃紧（首页 KPI）：本周期已用 ≥ meta 的 traffic_quota_warn_pct（同额度告警）的台数（没设额度的不计），
 *  和用得最多的那台（已用百分比向下取整；点它打开那台的流量页签）；没有吃紧的 top 为 null */
export function quotaTight(list: MachineView[]) {
  const warn = meta().thresholds.traffic_quota_warn_pct;
  let n = 0; let top: { id: string; name: string; pct: number } | null = null;
  for (const m of list) {
    const { pct } = quota(m);
    if (pct == null || pct < warn) continue;
    n++;
    if (!top || pct > top.pct) top = { id: m.id, name: m.name, pct };
  }
  return { n, top: top && { ...top, pct: Math.floor(top.pct) } };
}
/** 集群状态在线率格条的悬停：离线（agent 不再上报）的机器名，同 overview.offline 的口径 */
export const offlineNames = (list: MachineView[]) => list.filter((m) => m.agent && !m.agent.online).map((m) => m.name);
/** 资源占用的告警色（CPU / 内存 / 磁盘 / 负载的格条、圆环、迷你条共用）：阈值按指标取 meta.thresholds.usage（bad 红、warn 黄） */
export const usageTone = (pct: number | null | undefined, metric: UsageMetric) => {
  if (pct == null) return undefined;
  const t = meta().thresholds.usage[metric];
  return pct >= t.bad ? 'bad' : pct >= t.warn ? 'warn' : undefined;
};
const ratio = (used: number | null, total: number | null) => (used == null || total == null || !(total > 0) ? null : (used / total) * 100);
const usedTotal = (u: number | null, t: number | null) => (u == null || t == null ? '—' : `${F.str(F.mem(u))} / ${F.str(F.mem(t))}`);
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
/** 探测成功率的档位：卡片心跳格（3 分钟一格）与抽屉可用率（一天一格）共用这一套——≥ 99% 绿、≥ 80% 黄、更低红，没有探测为 null。
 *  整数比较：正好落在边界上不受浮点误差影响 */
const probeTone = (ok: number, total: number): BeatCell => (!total ? null : ok * 100 >= total * 99 ? 'ok' : ok * 100 >= total * 80 ? 'warn' : 'bad');
/** 颜色含义：只放在心跳条「近 1 小时」/「近 30 天」的悬停里，每格的悬停只给数据 */
// 只按成功率着色：跨境线路 150–250 ms 的延迟是常态，按延迟标黄会让正常线路一片警告色（延迟在每格的悬停里）。
// 一格 3 分钟、每 30 秒一次约 6 次：断 1 次（83%）黄，断 2 次（67%）红
export const BEAT_LEGEND = '线路机每 30 秒连一次节点\n绿：成功 ≥ 99% · 黄：成功 80%–99% · 红：成功不到 80% · 灰：没有探测';
export const AVAIL_LEGEND = '绿：成功 ≥ 99% · 黄：成功 80%–99% · 红：成功不到 80%\n灰：没有探测；TCP 与出口取较差的一种';
/** 线路机的格子是中控 → 线路机的连通性（中控每 30 秒连一次它的入口端口），不是它名下入口的合计：入口通不通是各节点的事 */
export const BEAT_LEGEND_RELAY = '中控每 30 秒连一次线路机，只看通不通\n绿：都连上 · 黄：断过 1 次 · 红：断 2 次以上 · 灰：没有数据';
/** 探测桶 → 心跳格（20 格 = 近 1 小时，旧→新）。按桶内成功比例定色（节点与线路机同一套，线路机的桶来源见 BEAT_LEGEND_RELAY） */
export const beatCells = (buckets: ProbeBucket[] | undefined): BeatCell[] => (buckets ?? []).map(([n, ok]) => probeTone(ok, n));
const BEAT_MS = 3 * 60_000;
/** 心跳格的悬停（与 beatCells 一一对应）：这 3 分钟的时段、成功率（次数）、延迟中位数、SYN 重发过几次（丢包，不算进延迟）。
 *  桶按墙钟 3 分钟对齐，最后一格是当前这格（见 API.md /probe-beats） */
export function beatTitles(buckets: ProbeBucket[] | undefined, now = Date.now()): string[] {
  const bs = buckets ?? [];
  const cur = Math.floor(now / BEAT_MS) * BEAT_MS;
  const hm = (t: number) => { const d = new Date(t); return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`; };
  return bs.map(([n, ok, ms, re], i) => {
    const t0 = cur - (bs.length - 1 - i) * BEAT_MS;
    const what = !n ? '没有探测' : `成功 ${Math.round((ok / n) * 100)}%（${ok}/${n}）${ms != null ? ` · 中位 ${Math.round(ms)} ms` : ''}${re ? ` · 重发 ${re} 次` : ''}`;
    return `${hm(t0)}–${hm(t0 + BEAT_MS)}${i === bs.length - 1 ? '（进行中）' : ''} · ${what}`;
  });
}
/** 近 1 小时的总成功率（家宽代理卡的心跳条标题：延迟已在上面的读数里）；没有探测为 null */
export function beatRate(buckets: ProbeBucket[] | undefined): number | null {
  let n = 0; let ok = 0;
  for (const b of buckets ?? []) { n += b[0]; ok += b[1]; }
  return n ? (ok / n) * 100 : null;
}
/** 成功率的写法（近 30 天可用率）：全部成功才写 100%，否则最多写到 99.99%（1439/1440 不会四舍五入成 100%）；没有探测为「—」 */
export const successPct = (p: number | null) => (p == null ? '—' : p >= 100 ? '100%' : `${Math.min(p, 99.99).toFixed(2)}%`);
const WORST_FIRST = ['bad', 'warn', 'ok'] as const;
/** 近 30 天可用率的每日格（旧→新）：取 TCP 与出口两种探测里较差的一档（阈值同心跳格），当天没有探测为空格 */
export const availCells = (days: AvailabilityDay[]): BeatCell[] =>
  days.map((d) => { const a = probeTone(d.tcp_ok, d.tcp_total); const b = probeTone(d.egress_ok, d.egress_total); return WORST_FIRST.find((t) => t === a || t === b) ?? null; });
/** 每日格的悬停（与 availCells 一一对应）：「10-21 · TCP 99.93%（1438/1440） · 出口 100%（288/288）」，没有探测写「没有探测」；
 *  最后一格是今天（还没过完） */
export function availTitles(days: AvailabilityDay[]): string[] {
  const part = (label: string, ok: number, total: number) => (total ? `${label} ${successPct((ok / total) * 100)}（${ok}/${total}）` : '');
  return days.map((d, i) => {
    const what = [part('TCP', d.tcp_ok, d.tcp_total), part('出口', d.egress_ok, d.egress_total)].filter(Boolean).join(' · ') || '没有探测';
    return `${d.date.slice(5)}${i === days.length - 1 ? '（今天，还没过完）' : ''} · ${what}`;
  });
}
/** 有探测、却一个都没成功（延迟为 null）：显示「失败」，和「—」（还没有探测）区分开 */
const allFailed = (ps: (ProbeResult | null)[]) => ps.some((p) => p && !p.ok) && !ps.some((p) => p?.ok);
/** TCP 延迟读数：近 1 小时的中位数（中控给的 median_ms），没有时用最近一次 */
export const tcpMs = (p: ProbeResult | null) => p?.median_ms ?? p?.latency_ms ?? null;
/** 入口探测摘要：TCP 延迟（线路机 → 节点，各入口近 1 小时中位数的平均）与出口通不通，tcpFail / egressFail = 全部失败。
 *  出口探测只判通不通、IP 对不对：它的耗时是经节点去中控回显服务（在德国）的，不是线路延迟，不当读数。
 *  只算 active 的入口（同服务端健康规则）：应用中的还没有探测。
 *  直出入口没有节点、不做 TCP 探测：TCP 的连通数（total / tcpOk）与延迟只算经节点的入口，出口照算 */
export function probeSummary(m: MachineView) {
  const es = m.entries.filter((e) => e.lifecycle === 'active');
  const viaNode = es.filter((e) => !e.direct);
  const tcpOk = viaNode.filter((e) => e.tcp?.ok).length;
  const lat = viaNode.map((e) => tcpMs(e.tcp)).filter((x): x is number => x != null);
  return {
    total: viaNode.length, tcpOk, tcp: lat.length ? Math.round(lat.reduce((a, b) => a + b, 0) / lat.length) : null,
    tcpFail: allFailed(viaNode.map((e) => e.tcp)), egressFail: allFailed(es.map((e) => e.egress)), egressOk: es.some((e) => e.egress?.ok),
  };
}
/** 备用团队里在用、有订阅的机器：到期 +1 天由中控自动删除（到期旁标出来）。旧接口没有 spare 时按否 */
export const autoDeletes = (m: Pick<MachineView, 'spare' | 'lifecycle' | 'subscription'>) => !!m.spare && m.lifecycle === 'active' && !!m.subscription;
/**
 * 剩余价值：已付还没用掉的部分，按天折算。每期天数 = 服务端的 period_days（到期日往前推一个周期 → 到期日），剩余比例 = 距到期天数 / 每期天数；
 * 预付了多期时比例大于 1；已过期为 0。
 */
export function remainingValue(s: Subscription | null) {
  if (!s) return null;
  const days = s.period_days;
  const frac = days > 0 ? Math.max(0, s.due_days / days) : 0;
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
  return `${F.money(s.price_cents, s.currency)}/${cycleLabel(s.cycle)}`;
};
/** 不是按月付的费用旁边给的月均（服务端折好的 monthly_cents）：「≈ $26.60/月」；按月付的为 null */
export const monthlyHint = (s: Subscription | null | undefined) =>
  (s && meta().cycles.find((c) => c.key === s.cycle)?.months !== 1 ? `≈ ${F.money(s.monthly_cents, s.currency)}/月` : null);
