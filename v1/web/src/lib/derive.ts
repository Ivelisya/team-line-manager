// 从 MachineView 推导卡片/表格要显示的数据（纯函数，便于复用）
import type { AvailabilityDay, Cycle, DueState, EntryView, MachineView, Meta, Plan, ProbeBucket, ProbeResult, Provider, SubField, Subscription, TrafficReset, UsageMetric, UsdTotal } from './types';
import * as F from './format';
import { tzLabel } from './tz';
import { primaryIssue, substituteOf, type ReachEffective } from './probePoints';

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
/** 网卡计量规则（供应商的「流量计量」） */
export const trafficRuleLabel = (k: string) => lbl(meta().traffic_rules, k);
/** 按生效的规则从下行、上行算计费量（取小 / 取大 / 相加 / 只算上行 / 只算下行，没有规则取大）：流量页线路机的区间合计先求下行之和、上行之和再套它，和额度已用同口径 */
export const nicBilled = (rule: string, rx: number, tx: number) => (rule === 'min' ? Math.min(rx, tx) : rule === 'sum' ? rx + tx : rule === 'out' ? tx : rule === 'in' ? rx : Math.max(rx, tx));
/** 网卡计量规则（费用页签「本周期已用」旁的 (i)、额度悬停）：「按网卡流量：下行、上行取较大值」（名字同供应商对话框，来自 /meta） */
export const trafficRulePlain = (k: string) => `按网卡，${trafficRuleLabel(k)}`;
/** 网卡计量规则的下拉选项（供应商对话框、订阅编辑器共用，来自 /meta）：/meta 的键按字母序，这里双向在前、单向随后 */
const RULE_ORDER = ['sum', 'max', 'min', 'out', 'in'];
const ruleRank = (k: string) => { const i = RULE_ORDER.indexOf(k); return i < 0 ? RULE_ORDER.length : i; };
export const trafficRuleOptions = () => Object.entries(meta().traffic_rules).sort(([a], [b]) => ruleRank(a) - ruleRank(b)).map(([value, label]) => ({ value, label }));
/** 流量重置方式：自然月 / 账单日 / 滚动 30 天 */
export const trafficResetLabel = (k: string) => lbl(meta().traffic_resets, k);
/** 流量重置方式的说法：按自然月、按账单日、滚动 30 天 */
export const trafficResetText = (k: string) => (k === 'rolling30' ? trafficResetLabel(k) : `按${trafficResetLabel(k)}`);
/** 计费周期的那个字：月 / 季 / 半年 / 年 */
export const cycleLabel = (k: string) => meta().cycles.find((c) => c.key === k)?.label ?? k;
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
const LOAD_NA = '容器里读不到负载';

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
/** 本月流量（卡片底行、首页表格、机器页的「本月流量」）：自己有 agent 的机器是网卡口径（down = 下行、up = 上行），
 *  家宽代理、同机 IP 节点是业务口径（down = 下载、up = 上传）；from：数据从哪天起才有（不完整时） */
export function monthFlow(m: MachineView) {
  const from = m.month_counted_from;
  if (m.nic) return { kind: 'nic' as const, down: m.nic.month_rx, up: m.nic.month_tx, total: m.nic.month_rx + m.nic.month_tx, from };
  const t = monthTraffic(m);
  return { kind: 'biz' as const, down: t.down, up: t.up, total: t.down + t.up, from };
}
/** 本月流量合计（主值）：卡片、表格、机器页按「本月流量」排序共用 */
export const trafficTotal = (m: MachineView) => monthFlow(m).total;
/** 组织时区的格式化器（sv-SE：「2026-10-08」「23:00」）：按时区 + 选项缓存复用，推送热路径上不每次新建 Intl.DateTimeFormat */
const orgFmts = new Map<string, Intl.DateTimeFormat>();
export function orgFmt(o: Intl.DateTimeFormatOptions = {}): Intl.DateTimeFormat {
  const tz = meta().org_timezone; const k = `${tz}|${JSON.stringify(o)}`;
  let f = orgFmts.get(k);
  if (!f) orgFmts.set(k, (f = new Intl.DateTimeFormat('sv-SE', { timeZone: tz, ...o })));
  return f;
}
/** 日期写成「10-22」：服务端给日期（YYYY-MM-DD）或时刻，时刻取组织时区的那一天 */
export const mmdd = (s: string) => (/^\d{4}-\d{2}-\d{2}$/.test(s) ? s : orgFmt().format(new Date(s))).slice(5);
/** 「X 起」的写法（数据不完整时标出从哪天起才有）：一般只写日期「10-09」；正好落在周期 / 月的起点那天（没传 start 时按每月 1 号）时，
 *  差几个小时才是缺的那部分，写到时刻「10-01 14:00」，不然读成「整月都有」。from 只有日期没有时刻时照写日期 */
export function fromLabel(from: string, start?: string | null): string {
  const d = mmdd(from);
  if (/^\d{4}-\d{2}-\d{2}$/.test(from) || !(start ? mmdd(start) === d : d.endsWith('-01'))) return d;
  return `${d} ${orgFmt({ hour: '2-digit', minute: '2-digit' }).format(new Date(from))}`;
}
/** 网卡口径的本月流量：显示的是下行 + 上行的合计；供应商按别的规则计费时写出规则和本周期的计费量（流量额度用的就是它；流量页线路机的区间合计也先求和再套同一条规则） */
function nicTip(m: MachineView) {
  const rule = m.quota.rule;
  const head = rule && rule !== 'sum'
    ? `网卡下行 + 上行的合计；按「${meta().traffic_rules[rule] ?? rule}」计费，本周期计费量 ${F.str(F.bytes(m.quota.used))}`
    : '网卡下行 + 上行的合计，和计费口径一致';
  return `${head}\n下行是收到的，上行是发出的`;
}
/** 本月流量的悬停说明（卡片底行、表格、流量页签）：口径，数据不完整时再加全句（数字后面只留灰字「10-22 起」） */
export function monthFlowTip(m: MachineView) {
  const f = monthFlow(m);
  return [
    f.kind === 'nic' ? nicTip(m) : m.host_machine ? '这是经这个节点的客户流量，网卡与宿主机共用' : '客户经入口的下载和上传',
    f.from && `从 ${fromLabel(f.from)} 开始统计，之前的没算进来，所以比供应商面板上的少`,
  ].filter(Boolean).join('\n');
}

/** 探测 / 应用的英文原始错误（主要是 ssh 的报错行和系统返回的 timeout、refused 之类）→ 中文（抽屉、卡片的健康原因、告警悬停卡片共用）。
 *  只换认得的片段，前面的中文（「出口不通：」）和认不出的原样留着。片段只吃 ASCII 字符，不会把前缀一起吞掉 */
const ERR_ZH: [RegExp, string | ((m: string, ...g: string[]) => string)][] = [
  // ssh 自己的报错行（原文在服务端日志里）：本机端口是软路由的反向隧道，被拒绝 = 隧道没连上来；其它地址写成「连不上 地址:端口」，后面的英文由下面几条接着换。先于下面各条（它们会先把 refused 换掉）
  [/ssh: connect to host (?:127\.0\.0\.1|localhost) port (\d+): (?:connection refused|连接被拒绝)/gi, (_, port) => `隧道没连上（中控上的端口 ${port} 没有响应）`],
  [/ssh: connect to host (\S+) port (\d+): ?/gi, (_, host, port) => `连不上 ${host}:${port}：`],
  [/\b(?:connection )?(?:timed out|timeout) during (?:ssh )?banner exchange\b|\bbanner exchange\b/gi, 'SSH 握手超时'], // 先于下面的「超时」，不留半句中文半句英文
  [/\b(?:timed out|timeout)(?: after)? (\d+(?:\.\d+)?) ?s\b/gi, (_, n) => `超时 ${n} 秒`],
  [/\b(?:i\/o )?(?:timed out|timeout)\b/gi, '超时'],
  [/\b(?:connection )?refused\b/gi, '连接被拒绝'],
  [/\b(?:connection )?reset(?: by peer)?\b/gi, '连接被重置'],
  [/\b(?:connection (?:closed|aborted)|broken pipe)\b/gi, '连接被关闭'],
  [/\b(?:unexpected eof|end of file|eof)\b/gi, '对端提前断开'],
  [/\b(?:no route to host|(?:network|host) (?:is )?unreachable|unreachable)\b/gi, '网络不可达'],
  [/\bdns\b[\w .:'/-]*|[\w '/-]*\bname resolution\b[\w .:'/-]*|\bno such host\b/gi, '域名解析失败'],
  [/(?:\b(?:tls|ssl)\b[\w .:'/-]*|\bcertificate\b[\w .:'/-]*)(?:握手失败)?/gi, 'TLS 握手失败'],
  [/\bsocks5?\b[\w .:'/-]*(?:握手失败)?/gi, '代理握手失败'], // 后面已经带「握手失败」的一并吃掉，不叠词
];
export const errText = (s: string) => ERR_ZH.reduce<string>((t, [re, to]) => t.replace(re, to as string), s).replace(/\s+$/, '');
/** 连不上的端口逐个列出的原因（relay_unreachable：「（端口 20001：超时 10 秒；端口 20002：超时 10 秒；…）」）：同一原因的端口合并成
 *  「28 个端口超时 10 秒」，两个以内照旧写端口号；括号外的文字不动。parts：逐个端口的明细（展开看），没有可合并的为 null */
const PORT_LIST = /（((?:端口 \d+：[^；（）]+)(?:；端口 \d+：[^；（）]+)+)）/;
export function compactPorts(s: string): { text: string; parts: string[] | null } {
  const m = PORT_LIST.exec(s);
  if (!m) return { text: s, parts: null };
  const parts = m[1].split('；');
  const by = new Map<string, string[]>();
  for (const p of parts) { const x = /^端口 (\d+)：(.+)$/.exec(p)!; by.set(x[2], [...(by.get(x[2]) ?? []), x[1]]); }
  if ([...by.values()].every((ps) => ps.length <= 2)) return { text: s, parts: null };
  const short = [...by].map(([why, ps]) => (ps.length > 2 ? `${ps.length} 个端口${why}` : `端口 ${ps.join('、')}：${why}`)).join('；');
  return { text: s.replace(m[0], `（${short}）`), parts };
}
/** 原因的显示文字：英文换中文，逐个端口的合并（卡片胶囊悬停、告警说明用；抽屉里另可展开明细） */
export const reasonText = (s: string) => compactPorts(errText(s)).text;
/** 入口探测失败的悬停：常见的连接错误先写成一句人话（端口拒绝连接 / 连接超时 / 网络不可达），原文放第二行；其它原样（英文换成中文） */
const PROBE_ERR: [RegExp, string][] = [[/拒绝|refused|os error 111/i, '端口拒绝连接'], [/超时|time[d ]?-?out|os error 110/i, '连接超时'], [/不可达|unreachable|os error 11[13]/i, '网络不可达']];
export function probeErr(detail: string | null | undefined) {
  if (!detail) return undefined;
  const raw = errText(detail); const hit = PROBE_ERR.find(([re]) => re.test(detail));
  if (!hit) return raw;
  const sec = hit[1] === '连接超时' ? detail.match(/(\d+(?:\.\d+)?)\s*(?:s\b|秒)/)?.[1] : undefined;
  const text = sec ? `超时 ${sec} 秒` : hit[1];
  return text === raw ? raw : `${text}\n${raw}`;
}
/** 运行中的入口探测失败（直出入口不做 TCP 探测）：返回失败的那次探测。抽屉入口表、线路机页的状态列写「不通」，悬停看 errText(detail) */
/** 穿透入口：节点主动连入口，线路机不连节点 */
export const isReverse = (e: EntryView) => e.mode === 'reverse';
/** 穿透入口的说明（入口表、卡片共用） */
export const REVERSE_TIP = '穿透：节点上的 bridge 主动连这个入口，线路机不连节点\nTCP 延迟是节点连线路机入口端口量的（节点 → 线路机），只画图、不告警\n隧道断了体现为出口探测失败（照常告警）';
/** TCP 延迟的方向：普通入口是线路机 → 节点（线路机量），穿透入口是节点 → 线路机（节点主动连线路机，节点上的 agent 量）；直出入口没有 TCP；
 *  有穿透也有普通的混着写「↔」。入口表、卡片、图卡共用 */
export const tcpDir = (es: EntryView[]) => {
  const t = es.filter((e) => !e.direct); const rv = t.filter(isReverse).length;
  return rv === 0 ? '线路机 → 节点' : rv === t.length ? '节点 → 线路机' : '线路机 ↔ 节点';
};
/** 没有线路机 → 节点的 TCP 探测：直出（没有节点）、穿透（线路机不连节点；它的 TCP 是节点 → 线路机，不进「节点通」连通数与告警） */
export const noTcp = (e: EntryView) => e.direct || isReverse(e);
export const probeFail = (e: EntryView) => (e.lifecycle !== 'active' || !e.applied ? null : !noTcp(e) && e.tcp && !e.tcp.ok ? e.tcp : e.egress && !e.egress.ok ? e.egress : null);
/** 入口状态列的「不通」写明是哪一段：TCP 连不上入口，还是连上了但出口验收失败（抽屉的「入口探测 N/M 连通」只算 TCP） */
export const probeFailLabel = (e: EntryView) => (probeFail(e) === e.tcp ? 'TCP 不通' : '出口不通');
/** 同一个端口连续连不上几次才算「中控连不上」：同 relay_unreachable 告警（reach.rs） */
const REACH_FAILS = 3;
/** 服务端数连续连不上次数的封顶（views.rs REACH_SEEN）：到了这个数写成「20+」 */
export const REACH_SEEN = 20;
const reachOf = (e: EntryView) => (e.lifecycle === 'active' && e.applied && e.reach?.ok === false ? e.reach : null);
/** 中控连不上这个入口的端口（中控 → 线路机的 TCP 检查连续 3 次失败，与上面「线路机 → 节点」是两段）：返回最近一次检查；通的、没检查过、不在运行中的为 null */
export const reachFail = (e: EntryView) => { const r = reachOf(e); return r && r.fails >= REACH_FAILS ? r : null; };
/** 刚才连不上 1–2 次（还不算连不上、不告警，同软路由出口的「刚才测不通」）：返回最近一次检查 */
export const reachFlaky = (e: EntryView) => { const r = reachOf(e); return r && r.fails < REACH_FAILS ? r : null; };
/** 入口表里的异常行（排在前面、折叠时也全露出、计入「N 条异常」）：中控连不上、线路机到节点探测不通、应用失败 */
export const entryBad = (e: EntryView) => !!reachFail(e) || !!probeFail(e) || (e.lifecycle === 'active' && !!e.apply_error);
/** 闲置备用（客户侧口径）：在用、在备用团队、没有软路由出口指着、没有在用的同机 IP 节点——不关系到客户，不开告警，总览不算进故障，可用率不含（读服务端 MachineView::idle_spare）；
 *  不看自动续费（只开着自动续费的备用机照样闲置，只是照常续费，见 isUnbilledSpare）；卡片上状态照常显示 */
export const isIdleSpare = (m: MachineView) => m.idle_spare;
/** 不续费的备用机（费用侧口径）：在用、在备用团队、没被扣住不回收（软路由出口指着 / 同机 IP 节点在用 / 开着自动续费都算被扣住）——等到期放生的，不提醒续费、不算到期，月费不进合计，待付清单默认不列。
 *  读服务端 MachineView::spare_unbilled。一定是闲置备用 */
export const isUnbilledSpare = (m: MachineView) => m.spare_unbilled;
/** 状态筛选、状态列排序用的值：在用的看健康，其余看生命周期（同机器页状态筛选下拉的 value） */
export const statusOf = (m: MachineView) => (m.lifecycle === 'active' ? m.health.status : m.lifecycle);
/** 状态列升序：有问题的在前（上架失败、故障、降级…），正常的在后。机器页、首页表格的状态列与首页的「异常优先」共用 */
const STATUS_RANK: Record<string, number> = { failed: 0, down: 1, degraded: 2, unknown: 3, maintenance: 4, provisioning: 5, retiring: 6, ok: 7 };
export const statusRank = (m: MachineView) => STATUS_RANK[statusOf(m)] ?? 8;
/** 流量额度按供应商的十进制 GB（1 GB = 10⁹ 字节） */
const GB = 1e9;
/** 流量额度：上限（GB，服务端算好的 quota.limit_gb，0 = 不限）与本周期已用（服务端按网卡 / 业务流量、流量周期算，见 MachineView.quota）。
 *  shared：同机 IP 节点自己没设上限——额度与宿主机共用（宿主机的网卡含它的流量），不是「不限」；unset：没设套餐、订阅里也没写上限，同样不是「不限」。source 见 limit_source */
export function quota(m: MachineView) {
  const limit = m.quota.limit_gb ?? 0;
  const { used, period_start: start, period_end: end, counted_from: from, rule, base } = m.quota;
  const shared = m.quota.limit_source === 'host';
  // 没设套餐、订阅里也没写流量上限：没设置 ≠ 不限（套餐里写了不限、或订阅里单独写了 0 才是不限）
  const unset = !(limit > 0) && !shared && m.quota.limit_source === 'none' && !m.plan && !isOverridden(m.subscription, 'traffic_gb');
  return { limit, used, start, end, from, base, rolling: m.quota.mode === 'rolling30', rule, pct: limit > 0 ? (used / (limit * GB)) * 100 : null, rem: limit > 0 ? Math.max(0, limit * GB - used) : null, shared, unset, unlimited: !(limit > 0) && !shared && !unset, source: m.quota.limit_source };
}
/** 预计用满的一句话（服务端按近 7 天日均算好，早于重置日才有）：「预计 11-02 用满（重置 11-13）」；没有为 null */
export function quotaForecast(m: MachineView): string | null {
  const { forecast_full_at: at, period_end: end } = m.quota;
  return at ? `预计 ${mmdd(at)} 用满${end ? `（重置 ${mmdd(end)}）` : ''}` : null;
}
/** 额度格里已用的叫法：滚动 30 天的是「近 30 天已用」 */
export const quotaUsedLabel = (q: ReturnType<typeof quota>) => (q.rolling ? '近 30 天已用' : '本周期已用');
/** 额度怎么计的一句话：规则 · 按哪块网卡（家宽代理、同机 IP 节点没有自己的网卡，按业务流量下载加上传，不跟供应商的计量规则） */
export function quotaRuleText(m: MachineView) {
  const { rule } = m.quota; const nics = m.agent?.nic_ifaces;
  return rule ? (nics?.length ? `按 ${nics.join('、')} 网卡，${trafficRuleLabel(rule)}` : trafficRulePlain(rule)) : '按业务流量，下载加上传';
}
/** 计费方式的短名（费用页签、流量页的小标签，来自 /meta）：「单向 · 取较大」 */
export const trafficRuleShort = (k: string) => meta().traffic_rule_short[k] ?? trafficRuleLabel(k);
/** 费用页签的计费方式小标签：「单向 · 取较大」，悬停写全名和来源（这台单独设置的，或跟随供应商）；按业务流量的机器（家宽代理、同机 IP 节点）为 null */
export function trafficRuleTag(m: MachineView) {
  const rule = m.quota.rule;
  if (!rule) return null;
  const from = isOverridden(m.subscription, 'traffic_rule') ? '这台单独设置' : m.provider ? `跟随供应商「${m.provider.name}」` : '默认';
  return { text: trafficRuleShort(rule), tip: `${trafficRuleLabel(rule)}
${from}` };
}
/** 流量周期（费用页签）：「每月 13 号重置（跟随供应商：按账单日）· 10-13 → 11-13」，滚动 30 天「滚动 30 天（跟随供应商）」 */
export function quotaCycleText(m: MachineView) {
  const q = quota(m);
  if (q.rolling) return '最近 30 天滚动，跟随供应商';
  const from = isOverridden(m.subscription, 'traffic_reset_day') ? '单独设置' : inheritSource(m, 'traffic_reset_day');
  return `每月 ${m.subscription?.traffic_reset_day ?? Number(q.start.slice(8))} 号重置${from && `，${from}`} · ${mmdd(q.start)} 至 ${mmdd(q.end!)}`;
}
/** 已用的补充说明（各一句）：接入后实测 + 接入前读数（已用 = 两者之和）、数据不完整（之前的用量不在内） */
export function quotaNotes(m: MachineView) {
  const q = quota(m);
  return [
    q.base != null && `接入后实测 ${F.str(quotaGb(q.used - q.base))}，加上接入前的面板读数 ${F.str(quotaGb(q.base))}\n${mmdd(q.end!)} 重置后只算实测`,
    q.from && `从 ${fromLabel(q.from, q.start)} 开始统计，之前的没算进来，实际剩余可能更少`,
  ].filter((x): x is string => !!x);
}
/** 额度格右侧「已用 / 上限」的悬停：周期起止 · 计量规则（含按哪块网卡），再接补充说明 */
export function quotaTip(m: MachineView) {
  const q = quota(m);
  return [
    q.rolling ? '最近 30 天' : `${mmdd(q.start)} 至 ${mmdd(q.end!)}，每月 ${m.subscription?.traffic_reset_day ?? Number(q.start.slice(8))} 号重置`,
    quotaRuleText(m),
    ...quotaNotes(m),
  ].join('\n');
}

/** 订阅各项跟随谁：价格、币种、周期、流量上限跟随套餐，账单时区、提前提醒、流量重置日、流量计费跟随供应商（同服务端 sub_template） */
export const SUB_FROM: Record<SubField, 'plan' | 'provider'> = {
  price_cents: 'plan', currency: 'plan', cycle: 'plan', traffic_gb: 'plan', billing_tz: 'provider', remind_days: 'provider', traffic_reset_day: 'provider', traffic_rule: 'provider',
};
/** 这一项单独设置了 */
export const isOverridden = (s: Subscription | null | undefined, k: SubField) => !!s?.overrides.includes(k);
/** 跟随时的值（同服务端 subscription_eff）：重置日按供应商的方式——账单日 = 订阅的账单日，自然月 = 1 号。没有模板为 null。
 *  账单日同服务端 meta::billing_day：到期日（YYYY-MM-DD）落在现有 billing_day 上的（小月按月末，如 31 号的 2-28）沿用，否则是它几号 */
export function inheritedValue(m: MachineView, k: SubField, due?: string): string | number | null {
  const t = m.sub_template;
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
    case 'traffic_rule': return trafficRuleLabel(String(v));
    case 'traffic_reset_day': return reset === 'billing' ? `每月 ${v} 号（账单日）` : reset === 'month' ? `每月 ${v} 号（自然月）` : `每月 ${v} 号`;
    default: return String(v);
  }
}
/** 上架页选了套餐后的一行：「跟随套餐「X」」，订阅会跟随的值放进悬停说明 tip（同服务端 sub_template；供应商没有时是默认值，时区空 = 组织时区） */
export function inheritSummary(plan: Plan, pv: Provider | undefined) {
  const M = meta();
  const fee = `${subValueText('price_cents', plan.price_cents, plan.currency)} / ${cycleLabel(plan.cycle)}`;
  const rk = pv?.traffic_reset ?? M.defaults.traffic_reset;
  const reset = rk === 'rolling30' ? trafficResetLabel(rk) : `${trafficResetText(rk)}重置`;
  return {
    text: `跟随套餐「${plan.name}」`,
    tip: `套餐：${fee} · 流量 ${subValueText('traffic_gb', plan.traffic_gb)}\n`
      + `${pv ? `供应商「${pv.name}」` : '默认'}：账单时区 ${pv?.timezone || M.org_timezone} · 流量${reset} · ${subValueText('remind_days', pv?.remind_days ?? M.defaults.remind_days)}提醒`,
  };
}
/** 「跟随套餐：2000 GB」 */
export function inheritText(m: MachineView, k: SubField, due?: string) {
  if (k === 'price_cents' && inheritedValue(m, k, due) == null) return '没有套餐，需要单独设置';
  return `${inheritSource(m, k)}：${inheritValueText(m, k, due)}`;
}
/** 跟随的值写成文字（不带来源，订阅表单里跟随态直接显示它） */
export const inheritValueText = (m: MachineView, k: SubField, due?: string) =>
  subValueText(k, inheritedValue(m, k, due), m.sub_template.currency, m.sub_template.traffic_reset);
/** 额度用量的写法同供应商面板：十进制 GB，不换算成 TB（901.6 GB）。业务流量等其它字节数仍用 F.bytes */
export const quotaGb = (bytes: number): F.Fmt => {
  const x = Math.max(0, bytes) / GB;
  // 100 GB 以上取整（卡片、表格、费用页签同一个函数；不会再出现一处「1426 GB」另一处「1040.2 GB」）
  return { v: x >= 100 ? String(Math.round(x)) : (x < 10 ? x.toFixed(2) : x.toFixed(1)).replace(/\.0+$|(\.\d)0$/, '$1'), u: 'GB' };
};
/** 额度上限（GB 数）：0 / 空 = 不限 */
export const limitGb = (gb: number | null | undefined): F.Fmt => (gb && gb > 0 ? { v: String(gb), u: 'GB' } : { v: '不限', u: '' });
/** 同机关系标的文字与悬停说明（卡片、环形卡、机器表）：同机 IP 节点「同机 · 宿主机名」（short：窄处只写「同机」，宿主机名在说明里），
 *  宿主机「+N IP」；没有同机关系为 null */
export function hostRel(host: MachineView['host_machine'], nodes: MachineView['ip_nodes'], short?: boolean) {
  if (host) return { text: short ? '同机' : `同机 · ${host.name}`, tip: `和宿主机「${host.name}」是同一台机器，CPU、内存、磁盘和网卡读数都共用` };
  if (nodes.length) return { text: `+${nodes.length} IP`, tip: `还有 ${nodes.length} 个同机 IP 节点：${nodes.map((x) => x.name).join('、')}` };
  return null;
}
/** 流量额度吃紧（首页 KPI，同额度告警）：本周期已用 ≥ meta 的 traffic_quota_warn_pct，或预计 7 天内用满（forecast_full_at）的台数（没设额度的不计），
 *  和最紧的那台（预计用满最早的；都没有预测的取已用百分比最高的，向下取整；点它打开那台的流量页签）；没有吃紧的 top 为 null。
 *  top.forecast：那台预计用满的一句话，没有预测为 null */
export function quotaTight(list: MachineView[]) {
  const warn = meta().thresholds.traffic_quota_warn_pct;
  const soon = Date.now() + 7 * 864e5;
  let n = 0; let top: { id: string; name: string; pct: number; at: number; forecast: string | null } | null = null;
  for (const m of list) {
    const { pct } = quota(m);
    // 已超限的没有预测日期：当成「已经用满」（-Infinity）排最前
    const at = pct != null && pct >= 100 ? -Infinity : m.quota.forecast_full_at ? Date.parse(m.quota.forecast_full_at) : Infinity;
    if (!(pct != null && pct >= warn) && at > soon) continue;
    n++;
    const p = pct ?? 0;
    if (!top || at < top.at || (at === top.at && p > top.pct)) top = { id: m.id, name: m.name, pct: p, at, forecast: quotaForecast(m) };
  }
  // full：预计用满的日期（MM-DD），首页卡上只写它，整句放悬停
  return { n, top: top && { id: top.id, name: top.name, pct: Math.floor(top.pct), forecast: top.forecast, full: Number.isFinite(top.at) ? mmdd(new Date(top.at).toISOString()) : null, over: top.at === -Infinity } };
}
/** 集群状态在线率格条的悬停：离线（agent 不再上报）的机器名，同 overview.offline 的口径（同机 IP 节点随宿主机的 agent，宿主机离线它们也各计一台） */
export const offlineNames = (list: MachineView[]) => list.filter((m) => m.agent && !m.agent.online).map((m) => m.name);
/** 即将到期（due_state = soon，不含不续费的备用机；备用团队里被出口指着、有在用 IP 节点、开着自动续费的照常续费）里最近的一台（首页 KPI「即将到期」的说明行）；没有为 null */
export function nextDue(list: MachineView[]) {
  let top: { id: string; name: string; days: number } | null = null;
  for (const m of list) {
    const s = m.subscription;
    if (m.lifecycle !== 'active' || isUnbilledSpare(m) || s?.due_state !== 'soon') continue;
    if (!top || s.due_days < top.days) top = { id: m.id, name: m.name, days: s.due_days };
  }
  return top;
}
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
export const BEAT_LEGEND = '线路机定时探测节点能不能连上\n绿：99% 以上 · 黄：80%–99% · 红：不到 80% · 灰：没有探测';
export const AVAIL_LEGEND = '绿：99% 以上 · 黄：80%–99% · 红：不到 80% · 灰：没有探测\nTCP 和出口都看，取较差的';
const RELAY_COLORS = '绿：都连上 · 黄：断过 1 次 · 红：断 2 次以上 · 灰：没有数据';
/** 线路机的格子是主探测来源 → 线路机的连通性（每 30 秒连一次，轮流连它的各个入口端口），不是它名下入口的合计：入口通不通是各节点的事。
 *  主探测来源是中控或放在靠近客户的地方的探测点（设置「探测点」，GET /settings/reach-source），说明随来源变；还没取到时按中控 */
export const BEAT_LEGEND_RELAY = `中控定时连线路机的各个入口，只看通不通\n${RELAY_COLORS}`;
export function beatLegendRelay(src: ReachEffective | undefined): string {
  switch (src?.state) {
    case 'probe': return `${src.label}定时连线路机的入口，看通不通；悬停可以看延迟\n${RELAY_COLORS}`;
    case 'fallback': {
      const sub = substituteOf(src);
      return `${primaryIssue(src)}，暂由${sub}代测${src.probe_point_id ? '；悬停可以看延迟' : '，只看通不通'}\n连不上照常告警，连得上不算恢复\n${RELAY_COLORS}`;
    }
    case 'none': return `${primaryIssue(src)}，没有能代测的来源（中控探测关着），暂时没有入口的数据\n${RELAY_COLORS}`;
    default: return BEAT_LEGEND_RELAY;
  }
}
/** 心跳格标题里的来源：「中控探测」「香港探测点」「中控代测」「香港探测点代测」 */
export const reachWin = (src: ReachEffective | undefined) =>
  `${src?.state === 'probe' ? src.label : src?.state === 'fallback' ? `${substituteOf(src)}代测` : src?.state === 'none' ? '暂无来源' : '中控探测'} · 近 1 小时`;
/** 「谁连不上」的主语：中控 / 香港探测点 / 代测的那一路（入口表、卡片的原因） */
export const reachWho = (src: ReachEffective | undefined) => (src?.state === 'probe' ? src.label : src?.state === 'fallback' ? substituteOf(src) : '中控');
/** 线路机近 1 小时建连延迟（探测点才有）：各格中位数的中位数；没有为 null */
export function relayRttMs(buckets: ProbeBucket[] | undefined): number | null {
  const v = (buckets ?? []).map((b) => b[2]).filter((x): x is number => x != null).sort((a, b) => a - b);
  if (!v.length) return null;
  const m = v.length >> 1;
  return v.length % 2 ? v[m] : (v[m - 1] + v[m]) / 2;
}
/** 探测桶 → 心跳格（20 格 = 近 1 小时，旧→新）。节点按桶内成功比例定色；线路机按断了几次（同 BEAT_LEGEND_RELAY）：
 *  当前这一格还没满 6 次时，按比例会把断 1 次标成红 */
export const beatCells = (buckets: ProbeBucket[] | undefined, relay = false): BeatCell[] =>
  (buckets ?? []).map(([n, ok]): BeatCell => (!relay ? probeTone(ok, n) : !n ? null : n - ok === 0 ? 'ok' : n - ok === 1 ? 'warn' : 'bad'));
const BEAT_MS = 3 * 60_000;
/** 心跳格的悬停（与 beatCells 一一对应）：这 3 分钟的时段、成功率（次数）、延迟中位数、SYN 重发过几次（丢包，不算进延迟）。
 *  桶按墙钟 3 分钟对齐，最后一格是当前这格（见 API.md /probe-beats） */
export function beatTitles(buckets: ProbeBucket[] | undefined, now = Date.now()): string[] {
  const bs = buckets ?? [];
  const cur = Math.floor(now / BEAT_MS) * BEAT_MS;
  return bs.map(([n, ok, ms, re], i) => {
    const t0 = cur - (bs.length - 1 - i) * BEAT_MS;
    const what = !n ? '没有探测' : `成功 ${Math.round((ok / n) * 100)}%（${ok}/${n}）${ms != null ? ` · 延迟中位 ${Math.round(ms)} ms` : ''}${re ? ` · 重试 ${re} 次` : ''}`;
    return `${F.hm(t0)}–${F.hm(t0 + BEAT_MS)}${i === bs.length - 1 ? '（进行中）' : ''} · ${what}`;
  });
}
/** 近 1 小时的总成功率（家宽代理卡的心跳条标题：延迟已在上面的读数里）；没有探测为 null */
export function beatRate(buckets: ProbeBucket[] | undefined): number | null {
  let n = 0; let ok = 0;
  for (const b of buckets ?? []) { n += b[0]; ok += b[1]; }
  return n ? (ok / n) * 100 : null;
}
/** 成功率的着色（告警页月度可用率、待付表的出口成功率共用）：没有数据或到 99.9% 不着色，未满 99.9 标黄、未满 98 标红；配 .up-pct[data-tone]。
 *  先舍入到显示的小数位（digits，告警页 2 位、待付表 1 位）再比较：99.86 显示成「99.9%」就不能标黄 */
export const uptimeTone = (p: number | null, digits = 2) => {
  const r = p == null ? null : Number(p.toFixed(digits));
  return r == null || r >= 99.9 ? undefined : r >= 98 ? 'warn' : 'bad';
};
/** 成功率的写法（近 30 天可用率）：全部成功才写 100%，否则最多写到 99.99%（1439/1440 不会四舍五入成 100%）；没有探测为「—」 */
export const successPct = (p: number | null) => (p == null ? '—' : p >= 100 ? '100%' : `${Math.min(p, 99.99).toFixed(2)}%`);
const WORST_FIRST = ['bad', 'warn', 'ok'] as const;
/** 近 30 天可用率的每日格（旧→新）：取 TCP 与出口两种探测里较差的一档（阈值同心跳格），当天没有探测为空格 */
export const availCells = (days: AvailabilityDay[]): BeatCell[] =>
  days.map((d) => { const a = probeTone(d.tcp_ok, d.tcp_total); const b = probeTone(d.egress_ok, d.egress_total); return WORST_FIRST.find((t) => t === a || t === b) ?? null; });
/** 每日格的悬停（与 availCells 一一对应）：「10-21 · 节点通 99.93%（1438/1440） · 出口通 100%（288/288）」，没有探测写「没有探测」；
 *  最后一格是今天（还没过完） */
export function availTitles(days: AvailabilityDay[]): string[] {
  const part = (label: string, ok: number, total: number) => (total ? `${label} ${successPct((ok / total) * 100)}（${ok}/${total}）` : '');
  return days.map((d, i) => {
    const what = [part('节点通', d.tcp_ok, d.tcp_total), part('出口通', d.egress_ok, d.egress_total)].filter(Boolean).join('\n') || '没有探测';
    return `${d.date.slice(5)}${i === days.length - 1 ? '（今天）' : ''}
${what}`;
  });
}
/** 有探测、却一个都没成功（延迟为 null）：显示「失败」，和「—」（还没有探测）区分开 */
const allFailed = (ps: (ProbeResult | null)[]) => ps.some((p) => p && !p.ok) && !ps.some((p) => p?.ok);
/** TCP 延迟读数：近 1 小时的中位数（中控给的 median_ms），没有时用最近一次 */
export const tcpMs = (p: ProbeResult | null) => p?.median_ms ?? p?.latency_ms ?? null;
/** 入口探测摘要：TCP 延迟（线路机 → 节点；穿透入口是节点 → 线路机）与出口通不通，tcpFail / egressFail = 全部失败。
 *  TCP 读数取各入口近 1 小时中位数里最小的那条（挂两台线路机、一条 5 ms 一条 65 ms 时，平均出来的数哪条线都不是），tcpTip 列各条。
 *  出口探测只判通不通、IP 对不对：它的耗时是经节点去中控回显服务（在德国）的，不是线路延迟，不当读数。
 *  只算 active 的入口（同服务端健康规则）：应用中的还没有探测。
 *  直出入口没有节点，不做 TCP 探测；穿透入口的 TCP 是节点连线路机量的：延迟读数与「失败」算上它（没有样本的旧 agent 不算），
 *  但连通数（total / tcpOk）只算线路机连节点的入口（穿透的不进「节点通」，也不告警），出口照算；reverse 是在用的穿透入口条数（卡片上标「穿透」）。
 *  入口可用（线路机卡片）：all = 全部在用入口（含直出），usable = TCP（直出不看）与出口都通的条数；
 *  tcpBad / egressBad 是各自不通（含还没有探测）的条数 */
export function probeSummary(m: MachineView) {
  const es = m.entries.filter((e) => e.lifecycle === 'active');
  const viaNode = es.filter((e) => !noTcp(e));
  const tcpOk = viaNode.filter((e) => e.tcp?.ok).length;
  const egOk = es.filter((e) => e.egress?.ok).length;
  const timed = es.filter((e) => !e.direct);
  const lat = timed.flatMap((e) => { const ms = tcpMs(e.tcp); return ms == null ? [] : [{ name: `${e.relay.name}:${e.port}`, ms, e }]; });
  const dir = tcpDir(lat.map((x) => x.e));
  const each = lat.length > 1 ? lat.map((x) => `\n${x.name} ${Math.round(x.ms)} ms${dir === '线路机 ↔ 节点' ? `（${isReverse(x.e) ? '节点 → 线路机' : '线路机 → 节点'}）` : ''}`).join('') : '';
  return {
    total: viaNode.length, tcpOk, tcp: lat.length ? Math.round(Math.min(...lat.map((x) => x.ms))) : null,
    tcpTip: `${lat.length ? dir : '线路机 → 节点'}的 TCP 延迟，近 1 小时中位数${lat.length > 1 ? '，取最小的一条' : ''}${dir === '线路机 → 节点' ? '' : '\n穿透入口是节点主动连线路机入口端口量的'}\n曲线是按分钟的平均，数字会略有不同${each}`,
    tcpFail: allFailed(timed.map((e) => e.tcp)), egressFail: allFailed(es.map((e) => e.egress)), egressOk: egOk > 0,
    off: m.entries.filter((e) => e.lifecycle === 'disabled').length, all: es.length, usable: es.filter((e) => (noTcp(e) || e.tcp?.ok) && e.egress?.ok).length, reverse: es.filter(isReverse).length, tcpBad: viaNode.length - tcpOk, egressBad: es.length - egOk, reachBad: es.filter(reachFail).length,
  };
}
/** 入口三段连通数（全站同一组名词与数字，卡片、抽屉、线路页都从 probeSummary 来）：
 *  入口可达 = 主探测来源（中控或探测点）连线路机上客户用的入口端口，同一端口连续 3 次连不上算不可达（同 relay_unreachable）；
 *  节点通 = 线路机 → 节点的 TCP（直出入口不经节点，不算）；出口通 = 经入口访问外网的出口验证。都只算在用的入口 */
export const linkCounts = (ps: Pick<ReturnType<typeof probeSummary>, 'all' | 'reachBad' | 'tcpOk' | 'total' | 'egressBad'>) => ({
  reach: { ok: ps.all - ps.reachBad, total: ps.all }, node: { ok: ps.tcpOk, total: ps.total }, egress: { ok: ps.all - ps.egressBad, total: ps.all },
});
export type LinkKey = keyof ReturnType<typeof linkCounts>;
export const LINK_LABEL: Record<LinkKey, string> = { reach: '入口可达', node: '节点通', egress: '出口通' };
/** 各段的悬停说明（who：主探测来源的叫法，reachWho） */
export const linkTip = (k: LinkKey, who: string) => ({
  reach: `${who}连线路机上客户用的入口端口，同一端口连续 3 次连不上算不可达`,
  node: '线路机到节点的 TCP 建连（直出入口不经节点，不算）',
  egress: '经入口访问外网的出口验证',
})[k];
/** 节点有几条入口的线路机端口连不上（节点自己没问题，告警记在线路机上）：抽屉抬头的胶囊与节点卡片心跳行共用 */
export const unreachTip = (who: string, n: number) => `${who}连不上 ${n} 条入口在线路机上的端口，客户经这几条入口连不上\n告警记在线路机上`;
/** 备用团队里在用、有订阅的机器：从哪天起自动回收（MM-DD，到期旁标出来）；不在备用里、没有订阅为 null */
export const recycleOn = (m: Pick<MachineView, 'lifecycle' | 'recycle_on'>) => (m.lifecycle === 'active' ? m.recycle_on?.slice(5) ?? null : null);
/**
 * 剩余价值：已付还没用掉的部分，按天折算。每期天数 = 服务端的 period_days（到期日往前推一个周期 → 到期日），剩余比例 = 距到期天数 / 每期天数；
 * 预付了多期时比例大于 1；已过期为 0。
 */
export function remainingValue(s: Subscription | null) {
  if (!s) return null;
  const days = s.period_days;
  const frac = days > 0 ? Math.max(0, s.due_days / days) : 0;
  // 计费统一按 USD：每期价格用服务端折好的 price_usd_cents；没有汇率时按原币种
  const usd = s.price_usd_cents != null;
  return { cents: Math.round((usd ? s.price_usd_cents! : s.price_cents) * frac), currency: usd ? 'USD' : s.currency, frac, days };
}

/** 标签配色：固定语义的用固定色（类型：线路机青、家宽橙；同机关系紫；维护中琥珀），
 *  供应商、团队按名字散列到剩下的几种中性色相（蓝、靛、天青、蓝灰），同一家永远同色；
 *  不取绿、黄、红、粉：标签常挨着状态标（「正常」绿、「故障」红），撞色读起来像状态 */
const TAG_COLORS = ['blue', 'indigo', 'cyan', 'slate'] as const;
export type TagColor = (typeof TAG_COLORS)[number] | 'gray' | 'teal' | 'violet' | 'orange' | 'amber' | 'green' | 'pink';
export function tagColor(text: string): TagColor {
  let h = 0;
  for (const ch of text) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  return TAG_COLORS[h % TAG_COLORS.length];
}

/** 带价格的订阅或套餐（套餐没有 monthly_cents，USD 字段也是可选的） */
type Priced = { price_cents: number; currency: string; cycle: Cycle; monthly_cents?: number; monthly_usd_cents?: number | null; usd_per_unit?: number | null };
const isMonthly = (cycle: Cycle) => meta().cycles.find((c) => c.key === cycle)?.months === 1;
/** 月费的展示（订阅、套餐各处统一用它）：计费统一按 USD，主数字是服务端折好的月费。
 *  没有汇率（monthly_usd_cents 为 null）时主数字写原币种、`sub` 写「未折算」（套餐没有 monthly_cents，主数字写每期价格）；
 *  有汇率时 `sub` 是原价（原币种不是 USD 或不是按月付才有，否则 null）。`tip` 是悬停说明（priceHint） */
export function feeView(f: Priced): { main: string; unit: string; sub: string | null; tip: string | null; converted: boolean } {
  const tip = priceHint(f);
  if (f.monthly_usd_cents != null) {
    const orig = f.currency !== 'USD' || !isMonthly(f.cycle) ? `${F.money(f.price_cents, f.currency)} / ${cycleLabel(f.cycle)}` : null;
    return { main: F.money(f.monthly_usd_cents, 'USD'), unit: '/ 月', sub: orig, tip, converted: true };
  }
  return f.monthly_cents != null
    ? { main: F.money(f.monthly_cents, f.currency), unit: '/ 月', sub: '未折算', tip, converted: false }
    : { main: F.money(f.price_cents, f.currency), unit: `/ ${cycleLabel(f.cycle)}`, sub: '未折算', tip, converted: false };
}
/** 月费的文字：「$37.50/月」；没有汇率时「¥299.99/月」（原币种，悬停见 priceHint 说明未折算） */
export const subLabel = (m: MachineView) => (m.subscription ? `${feeView(m.subscription).main}/月` : null);
/** 原价说明（月费旁的小字或悬停）：原币种不是 USD 或不是按月付时写原价「原价 ¥299.99/月 · 1 CNY = 0.1404 USD」；
 *  没有汇率写「未折算」；USD 按月付的为 null */
export function priceHint(s: Priced | null | undefined): string | null {
  if (!s) return null;
  const orig = `原价 ${F.money(s.price_cents, s.currency)}/${cycleLabel(s.cycle)}`;
  if (s.monthly_usd_cents == null) return `${orig} · 未折算：没有 ${s.currency} 的汇率（设置 → 汇率）`;
  if (s.currency === 'USD') return isMonthly(s.cycle) ? null : orig;
  return `${orig} · ${F.fxRate(s.currency, s.usd_per_unit!)}`;
}
/** 月费合计（USD）的悬停说明：各原币种金额与所用汇率，没有汇率的单列 */
export function usdTotalTip(t: UsdTotal | undefined): string {
  if (!t) return '';
  const parts = t.by_currency.map((p) => (p.currency === 'USD' ? `USD ${F.money(p.cents, 'USD')}` : `${F.money(p.cents, p.currency)} ≈ ${F.money(p.usd_cents, 'USD')}（${F.fxRate(p.currency, p.usd_per_unit)}）`));
  const un = t.unconverted.map((p) => F.money(p.cents, p.currency));
  return [...parts, ...(un.length ? [`没有汇率，未计入合计：${un.join(' + ')}`] : [])].join('；');
}
