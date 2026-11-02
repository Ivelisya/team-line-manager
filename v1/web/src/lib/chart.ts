// 曲线共用的常量与坐标轴 / 悬浮格式化。不依赖 recharts：lib/routers.ts 等首屏代码也要用，不要在这里（或 lib 下）导入 TimeChart，
// 否则 recharts 会被拖进首屏代码块。
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
