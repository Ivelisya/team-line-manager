// 曲线共用的常量、坐标轴刻度与悬浮格式化。不依赖图表库（bklit / d3）：lib/routers.ts 等首屏代码也要用，不要在这里（或 lib 下）导入 TimeChart，
// 否则图表代码会被拖进首屏代码块。
import * as F from './format';

/** 各时间范围的毫秒数 */
export const RANGE_MS: Record<string, number> = { '1h': 3600_000, '6h': 6 * 3600_000, '24h': 86400_000, '7d': 7 * 86400_000, '30d': 30 * 86400_000 };

/** 抽屉开着时曲线的重取间隔（实时流只推当前读数，不推曲线），随范围放大：范围越长点越粗、取一次越贵
 *  （线路机 24 小时流量曲线一次 130–350 ms 数据库时间，每分钟才多一个点）。机器抽屉与软路由抽屉共用 */
const POLL: Record<string, number> = { '1h': 30_000, '6h': 60_000, '24h': 120_000 };
export const chartPoll = (range: string) => POLL[range] ?? 300_000;

// y*：纵轴刻度（0 写成「0」不带单位）；v*：悬浮提示里的值
export const yMs = (v: number) => (v ? `${v} ms` : '0');
export const vMs = (_: string, v: number) => `${Math.round(v)} ms`;
export const yPct = (v: number) => `${Math.round(v)}%`;
export const vPct = (_: string, v: number) => `${v.toFixed(1)}%`;
export const yRate = (v: number) => (v ? F.str(F.rate(v, true)) : '0');
export const vRate = (_: string, v: number) => F.str(F.rate(v));

/** 纵轴刻度：从 0 起 4 段。速率（1000 进位）每段取 1/2/2.5/5×10^k，字节（1024 进位）每段取 2 的幂，
 *  读出来是 250 Mbps、256 MiB 这样的整数，不会出现按十进制取整再换算成 MiB 的 143M、286M */
export function valueTicks(max: number, base: 1000 | 1024): number[] | undefined {
  if (!(max > 0)) return undefined;
  const unit = base ** Math.max(0, Math.floor(Math.log(max) / Math.log(base)));
  const raw = max / unit / 4;
  let step: number;
  if (base === 1024) step = 2 ** Math.ceil(Math.log2(raw));
  else { const e = 10 ** Math.floor(Math.log10(raw)); step = ([1, 2, 2.5, 5, 10].find((n) => n * e >= raw) ?? 10) * e; }
  if (unit === 1) step = Math.max(1, step); // 最小单位（B / bps）不分小数格，否则标签取整后重复
  return [0, 1, 2, 3, 4].map((i) => i * step * unit);
}

/** 纵轴上限取整档（1 / 1.5 / 2 / 2.5 / 3 / 4 / 5 / 6 / 8 / 10 × 10^k），不指定进位的图（延迟 ms）用 */
export function niceCeil(v: number): number {
  const e = 10 ** Math.floor(Math.log10(v));
  return ([1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10].find((n) => n * e >= v) ?? 10) * e;
}
/** 整数步长：只取 1 / 2 / 5（以及 25 / 250 …）× 10^k，且 ≥ 1，不出现 1.5、2.5 这样的小数档 */
const intStep = (raw: number) => {
  const r = Math.max(1, raw); const e = 10 ** Math.floor(Math.log10(r));
  return ([1, 2, 2.5, 5, 10].map((n) => n * e).find((s) => Number.isInteger(s) && s >= r)) ?? 10 * e;
};
/** 整数刻度（延迟 ms、次数）：从 0 起 4 段，步长是整数（max = 6 → 0 / 2 / 4 / 6 / 8；max = 10 → 0 / 5 / 10 / 15 / 20；max = 0 → 0…4） */
export const intTicks = (max: number) => { const step = intStep(max / 4); return [0, 1, 2, 3, 4].map((i) => i * step); };
/** 没有指定进位 / 范围的图：数据全是整数用整数刻度，否则按 niceCeil 的步长；从 0 起 4 段；全为 0 时 0 与 1 */
export const autoTicks = (max: number, integers: boolean) => (max > 0 ? (integers ? intTicks(max) : [0, 1, 2, 3, 4].map((i) => i * niceCeil(max / 4))) : [0, 1]);

/** 百分比刻度（没有固定上限的百分比，如负载）：从 0 起 4 段，每段取 5 / 10 / 20 / 25 / 50 / 100 … 这样的整档，不出现 7% / 14% / 21% */
export const pctTicks = (max: number) => { const step = [5, 10, 20, 25, 50, 100, 200, 500, 1000, 2000, 5000].find((s) => s * 4 >= max) ?? Math.ceil(max / 4); return [0, 1, 2, 3, 4].map((i) => i * step); };
