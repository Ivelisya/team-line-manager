// 数值格式化：返回 {v, u}（值 + 单位），由 <Num> 渲染成 “值 + 灰色小单位”。缺测一律 “—”。

export interface Fmt { v: string; u: string }
export const NA: Fmt = { v: '—', u: '' };
/** 纯文本（title、提示里用）：“值 单位” */
export const str = (f: Fmt) => (f.u ? `${f.v} ${f.u}` : f.v);

const num = (n: number, digits = 1) => {
  const s = n >= 100 ? n.toFixed(0) : n >= 10 ? n.toFixed(1) : n.toFixed(digits === 1 ? 2 : digits);
  return s.includes('.') ? s.replace(/\.?0+$/, '') : s;
};

/** 量级：除数 + 单位。数字滚动动画要“按目标值的量级”格式化中间值，否则会出现数值是 MB 级、单位却是 GB 的错配 */
export interface Scale { div: number; u: string }
// 流量用十进制（同供应商的流量额度 GB）；内存、磁盘用二进制（GiB，同系统报的容量）
const BYTE_U = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
const BIN_U = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
function scaleOf(n: number, base: number, units: string[]): Scale {
  let i = 0; let x = Math.abs(n);
  while (x >= base && i < units.length - 1) { x /= base; i++; }
  return { div: base ** i, u: units[i] };
}
export const bytesScale = (n: number): Scale => scaleOf(n, 1000, BYTE_U);
export function rateScale(bps: number): Scale {
  const a = Math.abs(bps);
  if (a < 1e3) return { div: 1, u: 'bps' };
  if (a < 1e6) return { div: 1e3, u: 'Kbps' };
  if (a < 1e9) return { div: 1e6, u: 'Mbps' };
  return { div: 1e9, u: 'Gbps' };
}
/** 用给定量级格式化数值部分（单位由 Scale.u 另行显示） */
export const inScale = (n: number, s: Scale): string => (s.div === 1 ? String(Math.round(Math.abs(n))) : num(Math.abs(n) / s.div));
/** 流量的数值部分：小数位固定（不到 10 两位、不到 100 一位），不去掉末尾的 0——推送里涨着的读数不会在「6.19」「6.2」之间跳，各处同一个数写法一样 */
export const inScaleBytes = (n: number, s: Scale): string => {
  if (s.div === 1) return String(Math.round(Math.abs(n)));
  const x = Math.abs(n) / s.div;
  return x >= 100 ? x.toFixed(0) : x >= 10 ? x.toFixed(1) : x.toFixed(2);
};

/** 流量字节 → B/KB/MB/GB/TB（十进制），小数位固定（inScaleBytes） */
export function bytes(n: number | null | undefined): Fmt {
  if (n == null || !isFinite(n)) return NA;
  const s = bytesScale(n);
  return { v: inScaleBytes(n, s), u: s.u };
}
/** 内存、磁盘字节 → B/KiB/MiB/GiB/TiB（二进制） */
export function mem(n: number | null | undefined): Fmt {
  if (n == null || !isFinite(n)) return NA;
  const s = scaleOf(n, 1024, BIN_U);
  return { v: inScale(n, s), u: s.u };
}
/** 操作人：用户名原样；API 密钥「名称（API 密钥）」；cli 是中控上的命令行 */
export const actor = (a: string) => (a === 'cli' ? '中控命令行' : a.startsWith('apikey:') ? `${a.slice(7)}（API 密钥）` : a);
/** bit/s → Kbps/Mbps/Gbps */
export function rate(bps: number | null | undefined): Fmt {
  if (bps == null || !isFinite(bps)) return NA;
  const s = rateScale(bps);
  return { v: inScale(bps, s), u: s.u };
}
export const pct = (n: number | null | undefined, d = 0): Fmt => (n == null ? NA : { v: n.toFixed(d), u: '%' });
/** 延迟：不到 0.5 ms（同机房、本机）四舍五入成 0，写「<1 ms」，不像是没测到 */
export const ms = (n: number | null | undefined): Fmt => (n == null ? NA : { v: Math.round(n) < 1 ? '<1' : String(Math.round(n)), u: 'ms' });
export const mbps = (n: number | null | undefined): Fmt => (n == null ? NA : n >= 1000 ? { v: num(n / 1000), u: 'Gbps' } : { v: String(n), u: 'Mbps' });

// 币种符号只是显示用（币种列表、默认币种在 /meta）；没收录的写代码
const SYM: Record<string, string> = { CNY: '¥', USD: '$', EUR: '€', HKD: 'HK$', JPY: 'JP¥', GBP: '£' };
const currencySym = (c: string) => SYM[c] ?? `${c} `;
export function money(cents: number | null | undefined, currency: string): string {
  if (cents == null) return '—';
  const v = cents / 100;
  // 同一列里 $59 与 $28.88 并排时对不齐：一律两位小数（日元没有辅币单位）；千分位（$1,527.39）
  const d = currency === 'JPY' ? 0 : 2;
  return `${currencySym(currency)}${v.toLocaleString('en-US', { minimumFractionDigits: d, maximumFractionDigits: d })}`;
}
/** 每 GB 业务流量的美元：「$0.80」，不到 $0.1 写三位；没有月费或没流量为「—」 */
export const usdPerGb = (n: number | null | undefined): string => (n == null || !isFinite(n) ? '—' : `$${n < 0.1 ? n.toFixed(3) : n.toFixed(2)}`);
/** 汇率：「1 CNY = 0.1404 USD」（4 位有效数字） */
export const fxRate = (currency: string, usdPerUnit: number) => `1 ${currency} = ${Number(usdPerUnit.toPrecision(4))} USD`;
/** 币种下拉的一项：「USD $」 */
export const currencyLabel = (c: string) => `${c} ${currencySym(c).trim()}`.trim();
/** 当前值不在常用选项里（供应商时区 Asia/Hong_Kong、套餐币种 SGD 等）时补进去，否则下拉显示为空 */
export const withCurrent = (opts: { value: string; label: string }[], v: string) => (opts.some((o) => o.value === v) ? opts : [{ value: v, label: v }, ...opts]);

/** 到期：N 天后 / 今天 / 已过期 N 天。数字在 f 里，文字在 pre / post（不按数字排版）；past：已过期（后面不再接“到期”二字）。
 *  tone 由调用方按服务端的 due_state 给（derive.dueTone：已过期红、即将到期黄），这里不自己按天数判断 */
export interface Due { pre: string; f: Fmt | null; post: string; past: boolean; tone: '' | 'warn' | 'bad' }
export function dueFmt(days: number | null | undefined, tone: Due['tone'] = ''): Due {
  if (days == null) return { pre: '', f: NA, post: '', past: false, tone: '' };
  if (days < 0) return { pre: '已过期 ', f: { v: String(-days), u: '天' }, post: '', past: true, tone };
  if (days === 0) return { pre: '今天', f: null, post: '', past: false, tone };
  return { pre: '', f: { v: String(days), u: '天' }, post: '后', past: false, tone };
}
/** 时长：已上架/已运行 */
export function since(iso: string | null | undefined, now = Date.now()): Fmt {
  if (!iso) return NA;
  const s = Math.max(0, (now - new Date(iso).getTime()) / 1000);
  if (s < 3600) return { v: String(Math.max(1, Math.floor(s / 60))), u: '分钟' };
  if (s < 86400) return { v: String(Math.floor(s / 3600)), u: '小时' };
  return { v: String(Math.floor(s / 86400)), u: '天' };
}
const p2 = (n: number) => String(n).padStart(2, '0');
// ── 时刻按组织时区显示（P-23）：浏览器可能在别的时区（如 UTC），零点附近会差一天；/meta 取回时 setMeta 设上，之前（登录页）用本机时区 ──
let TZ: string | undefined;
let wall: Intl.DateTimeFormat | null = null;
export const setTimeZone = (tz?: string) => { TZ = tz; wall = null; };
/** 组织时区里的「YYYY-MM-DD HH:MM:SS」（sv-SE 就是这个写法；格式化器按时区缓存，推送热路径上不每次新建） */
const local = (t: string | number) => (wall ??= new Intl.DateTimeFormat('sv-SE', { timeZone: TZ, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' })).format(new Date(t));
/** 组织时区的 `HH:MM` */
export const hm = (t: string | number): string => local(t).slice(11, 16);
/** 组织时区的 `MM-DD HH:MM`；时间戳（毫秒）或 ISO 字符串，空为「—」 */
export const dt = (iso: string | number | null | undefined): string => (iso == null || iso === '' ? '—' : local(iso).slice(5, 16));
/** 月-日（MM-DD） */
export const md = (iso: string | null | undefined): string => (iso ? dt(iso).slice(0, 5) : '—');
/** 组织时区的 `YYYY-MM-DD HH:MM` */
export const dtFull = (iso: string | null | undefined): string => (iso ? local(iso).slice(0, 16) : '—');
/** 组织时区的 `HH:MM:SS` */
export const clock = (iso: string) => local(iso).slice(11);
export function ago(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return '—';
  const s = Math.max(0, (now - new Date(iso).getTime()) / 1000);
  if (s < 60) return `${Math.floor(s)} 秒前`;
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`;
  if (s < 86400) return `${Math.floor(s / 3600)} 小时前`;
  return `${Math.floor(s / 86400)} 天前`;
}
/** 倒数的时长（还剩多久、多久后开始）：按分钟向上取整，2 天 4 小时 / 3 小时 20 分钟 / 15 分钟（同 fleetctl） */
export function span(ms: number): string {
  const m = Math.max(1, Math.ceil(ms / 60_000));
  const d = Math.floor(m / 1440); const h = Math.floor((m % 1440) / 60); const r = m % 60;
  if (d) return h ? `${d} 天 ${h} 小时` : `${d} 天`;
  if (h) return r ? `${h} 小时 ${r} 分钟` : `${h} 小时`;
  return `${r} 分钟`;
}
export function duration(a: string | null, b: string | null, now = Date.now()): string {
  if (!a) return '—';
  const s = Math.max(0, ((b ? new Date(b).getTime() : now) - new Date(a).getTime()) / 1000);
  if (s < 1) return '不到 1 秒';
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)} 秒`;
  return `${Math.floor(s / 60)} 分 ${Math.floor(s % 60)} 秒`;
}
export const pad2 = p2;
/** 显示名：团队名 + '-' + 两位编号 */
export const displayName = (team: string, n: number) => `${team}-${p2(n)}`;

/** YYYY-MM-DD 加 n 个月（n 可为负）；目标月没有那一天时落到月末。
 *  只给日期选择器翻月用（components/ui/date-field）：续费的新到期日、每期天数等账单日期由服务端算（next_due、period_days），不在这里算 */
export function addMonths(date: string, n: number): string {
  const [y, m, d] = date.split('-').map(Number);
  const t = new Date(Date.UTC(y, m - 1 + n, 1));
  const last = new Date(Date.UTC(t.getUTCFullYear(), t.getUTCMonth() + 1, 0)).getUTCDate();
  t.setUTCDate(Math.min(d, last));
  return t.toISOString().slice(0, 10);
}

/** YYYY-MM-DD 加减天数（UTC 计算，避免夏令时把一天算成 23 小时） */
export const addDays = (s: string, n: number) => { const [y, m, d] = s.split('-').map(Number); return new Date(Date.UTC(y, m - 1, d + n)).toISOString().slice(0, 10); };

/** 实时速率的提示档：空闲（< 100 Kbps）、常态、接近套餐带宽（≥ 80%，只有知道套餐带宽时才判）。颜色不当状态用：常态读数用正文色，只有接近套餐带宽才换成提示色 */
export type Speed = 'idle' | 'ok' | 'near';
export const speedOf = (bps: number | null | undefined, planMbps?: number | null): Speed =>
  bps == null || bps < 1e5 ? 'idle' : planMbps && bps >= planMbps * 1e6 * 0.8 ? 'near' : 'ok';

/** 按本地时间问候 */
export function greeting(h = new Date().getHours()): string {
  return h < 5 ? '夜深了' : h < 11 ? '早上好' : h < 13 ? '中午好' : h < 18 ? '下午好' : '晚上好';
}
