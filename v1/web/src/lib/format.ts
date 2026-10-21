// 数值格式化：返回 {v, u}（值 + 单位），由 <Num> 渲染成 “值 + 灰色小单位”。缺测一律 “—”。

export interface Fmt { v: string; u: string }
export const NA: Fmt = { v: '—', u: '' };

const num = (n: number, digits = 1) => {
  const s = n >= 100 ? n.toFixed(0) : n >= 10 ? n.toFixed(1) : n.toFixed(digits === 1 ? 2 : digits);
  return s.includes('.') ? s.replace(/\.?0+$/, '') : s;
};

/** 量级：除数 + 单位。数字滚动动画要“按目标值的量级”格式化中间值，否则会出现数值是 MiB 级、单位却是 GiB 的错配 */
export interface Scale { div: number; u: string }
const BYTE_U = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
export function bytesScale(n: number): Scale {
  let i = 0; let x = Math.abs(n);
  while (x >= 1024 && i < BYTE_U.length - 1) { x /= 1024; i++; }
  return { div: 1024 ** i, u: BYTE_U[i] };
}
export function rateScale(bps: number): Scale {
  const a = Math.abs(bps);
  if (a < 1e3) return { div: 1, u: 'bps' };
  if (a < 1e6) return { div: 1e3, u: 'Kbps' };
  if (a < 1e9) return { div: 1e6, u: 'Mbps' };
  return { div: 1e9, u: 'Gbps' };
}
/** 用给定量级格式化数值部分（单位由 Scale.u 另行显示） */
export const inScale = (n: number, s: Scale): string => (s.div === 1 ? String(Math.round(Math.abs(n))) : num(Math.abs(n) / s.div));

/** 字节 → B/KiB/MiB/GiB/TiB */
export function bytes(n: number | null | undefined): Fmt {
  if (n == null || !isFinite(n)) return NA;
  const s = bytesScale(n);
  return { v: inScale(n, s), u: s.u };
}
/** bit/s → Kbps/Mbps/Gbps */
export function rate(bps: number | null | undefined): Fmt {
  if (bps == null || !isFinite(bps)) return NA;
  const s = rateScale(bps);
  return { v: inScale(bps, s), u: s.u };
}
export const pct = (n: number | null | undefined, d = 0): Fmt => (n == null ? NA : { v: n.toFixed(d), u: '%' });
export const ms = (n: number | null | undefined): Fmt => (n == null ? NA : { v: String(Math.round(n)), u: 'ms' });
export const gb = (n: number | null | undefined): Fmt => (n == null || n <= 0 ? { v: '不限', u: '' } : n >= 1024 ? { v: num(n / 1024), u: 'TiB' } : { v: String(n), u: 'GiB' });
export const mbps = (n: number | null | undefined): Fmt => (n == null ? NA : n >= 1000 ? { v: num(n / 1000), u: 'Gbps' } : { v: String(n), u: 'Mbps' });

const SYM: Record<string, string> = { CNY: '¥', USD: '$', EUR: '€', HKD: 'HK$', JPY: 'JP¥', GBP: '£' };
export const currencySym = (c: string) => SYM[c] ?? `${c} `;
export function money(cents: number | null | undefined, currency = 'CNY'): string {
  if (cents == null) return '—';
  const v = cents / 100;
  return `${currencySym(currency)}${Number.isInteger(v) ? v : v.toFixed(2)}`;
}
export const CYCLE: Record<string, string> = { monthly: '月', quarterly: '季', semiannual: '半年', yearly: '年', once: '一次性' };
export const cycleLabel = (c: string) => CYCLE[c] ?? c;

/** 到期：N 天后 / 今天 / 已过期 N 天（7 天内为 warn，过期为 bad） */
export function dueFmt(days: number | null | undefined): { text: Fmt; tone: '' | 'warn' | 'bad' } {
  if (days == null) return { text: NA, tone: '' };
  if (days < 0) return { text: { v: `已过期 ${-days}`, u: '天' }, tone: 'bad' };
  if (days === 0) return { text: { v: '今天', u: '' }, tone: 'warn' };
  return { text: { v: String(days), u: '天后' }, tone: days <= 7 ? 'warn' : '' };
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
export function dt(iso: string | null | undefined): string {
  if (!iso) return '—';
  const d = new Date(iso);
  return `${p2(d.getMonth() + 1)}-${p2(d.getDate())} ${p2(d.getHours())}:${p2(d.getMinutes())}`;
}
export function dtFull(iso: string | null | undefined): string {
  if (!iso) return '—';
  const d = new Date(iso);
  return `${d.getFullYear()}-${p2(d.getMonth() + 1)}-${p2(d.getDate())} ${p2(d.getHours())}:${p2(d.getMinutes())}`;
}
export const clock = (iso: string) => { const d = new Date(iso); return `${p2(d.getHours())}:${p2(d.getMinutes())}:${p2(d.getSeconds())}`; };
export function ago(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return '—';
  const s = Math.max(0, (now - new Date(iso).getTime()) / 1000);
  if (s < 60) return `${Math.floor(s)} 秒前`;
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`;
  if (s < 86400) return `${Math.floor(s / 3600)} 小时前`;
  return `${Math.floor(s / 86400)} 天前`;
}
export function duration(a: string | null, b: string | null, now = Date.now()): string {
  if (!a) return '—';
  const s = Math.max(0, ((b ? new Date(b).getTime() : now) - new Date(a).getTime()) / 1000);
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)} 秒`;
  return `${Math.floor(s / 60)} 分 ${Math.floor(s % 60)} 秒`;
}
export const pad2 = p2;
/** 显示名：团队名 + '-' + 两位编号 */
export const displayName = (team: string, n: number) => `${team}-${p2(n)}`;

/** 计费周期的月数（一次性没有周期） */
export const CYCLE_MONTHS: Record<string, number> = { monthly: 1, quarterly: 3, semiannual: 6, yearly: 12 };
/** YYYY-MM-DD 加 n 个月（n 可为负）；目标月没有那一天时落到月末 */
export function addMonths(date: string, n: number): string {
  const [y, m, d] = date.split('-').map(Number);
  const t = new Date(Date.UTC(y, m - 1 + n, 1));
  const last = new Date(Date.UTC(t.getUTCFullYear(), t.getUTCMonth() + 1, 0)).getUTCDate();
  t.setUTCDate(Math.min(d, last));
  return t.toISOString().slice(0, 10);
}

/** YYYY-MM-DD 加减天数（UTC 计算，避免夏令时把一天算成 23 小时） */
export const addDays = (s: string, n: number) => { const [y, m, d] = s.split('-').map(Number); return new Date(Date.UTC(y, m - 1, d + n)).toISOString().slice(0, 10); };

/** 实时速率的量级：空闲（< 1 Mbps）、常态、繁忙（≥ 200 Mbps）、接近满载（≥ 800 Mbps），卡片读数按它着色 */
export type Speed = 'idle' | 'low' | 'high' | 'max';
export const speedOf = (bps: number | null | undefined): Speed => (bps == null || bps < 1e6 ? 'idle' : bps < 2e8 ? 'low' : bps < 8e8 ? 'high' : 'max');

/** 按本地时间问候 */
export function greeting(h = new Date().getHours()): string {
  return h < 5 ? '夜深了' : h < 11 ? '早上好' : h < 13 ? '中午好' : h < 18 ? '下午好' : '晚上好';
}
