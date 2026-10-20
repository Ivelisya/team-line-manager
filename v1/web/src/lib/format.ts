// 数值格式化：返回 {v, u}（值 + 单位），由 <Num> 渲染成 “值 + 灰色小单位”。缺测一律 “—”。

export interface Fmt { v: string; u: string }
export const NA: Fmt = { v: '—', u: '' };

const num = (n: number, digits = 1) => {
  const s = n >= 100 ? n.toFixed(0) : n >= 10 ? n.toFixed(1) : n.toFixed(digits === 1 ? 2 : digits);
  return s.includes('.') ? s.replace(/\.?0+$/, '') : s;
};

/** 字节 → B/KiB/MiB/GiB/TiB */
export function bytes(n: number | null | undefined): Fmt {
  if (n == null || !isFinite(n)) return NA;
  const U = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  let i = 0; let x = Math.abs(n);
  while (x >= 1024 && i < U.length - 1) { x /= 1024; i++; }
  return { v: i === 0 ? String(Math.round(x)) : num(x), u: U[i] };
}
/** bit/s → Kbps/Mbps/Gbps */
export function rate(bps: number | null | undefined): Fmt {
  if (bps == null || !isFinite(bps)) return NA;
  if (bps < 1000) return { v: String(Math.round(bps)), u: 'bps' };
  if (bps < 1e6) return { v: num(bps / 1e3), u: 'Kbps' };
  if (bps < 1e9) return { v: num(bps / 1e6), u: 'Mbps' };
  return { v: num(bps / 1e9), u: 'Gbps' };
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

/** 距今天数文字：到期 N 天 / 已过期 N 天 */
export function dueFmt(days: number | null | undefined): { text: Fmt; tone: '' | 'warn' | 'bad' } {
  if (days == null) return { text: NA, tone: '' };
  if (days < 0) return { text: { v: String(-days), u: '天前过期' }, tone: 'bad' };
  return { text: { v: String(days), u: '天' }, tone: days <= 7 ? 'warn' : '' };
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
export function duration(a: string | null, b: string | null): string {
  if (!a) return '—';
  const s = Math.max(0, ((b ? new Date(b).getTime() : Date.now()) - new Date(a).getTime()) / 1000);
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)} 秒`;
  return `${Math.floor(s / 60)} 分 ${Math.floor(s % 60)} 秒`;
}
export const pad2 = p2;
/** 显示名：团队名 + '-' + 两位编号 */
export const displayName = (team: string, n: number) => `${team}-${p2(n)}`;
