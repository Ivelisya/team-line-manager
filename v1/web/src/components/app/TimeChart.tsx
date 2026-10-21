// 时序小图：细线 + 淡填充（原版详情图的语言）。颜色取图表 token：--chart-1 / --chart-2 / --chart-3。
// memo：抽屉随实时推送重渲染时，曲线点没变（lines / 格式化函数由调用方放在模块常量里）就不重画
import { memo, useMemo } from 'react';
import type { SeriesPoint } from '@/lib/types';
import { ChartContainer, ChartLegend, ChartTooltip, Recharts, axisProps, type ChartConfig } from '@/components/ui/chart';

const { AreaChart, Area, LineChart, Line, XAxis, YAxis, CartesianGrid } = Recharts;
export const RANGES = [{ value: '1h', label: '1 小时' }, { value: '6h', label: '6 小时' }, { value: '24h', label: '24 小时' }, { value: '7d', label: '7 天' }, { value: '30d', label: '30 天' }] as const;

const p2 = (n: number) => String(n).padStart(2, '0');
const xFmt = (range: string) => (t: string | number) => {
  const d = new Date(t);
  return range === '7d' || range === '30d' ? `${p2(d.getMonth() + 1)}-${p2(d.getDate())}` : `${p2(d.getHours())}:${p2(d.getMinutes())}`;
};

const MIN = 60_000; const HOUR = 60 * MIN; const DAY = 24 * HOUR;
const TICK_STEP: Record<string, number> = { '1h': 10 * MIN, '6h': HOUR, '24h': 3 * HOUR, '7d': DAY, '30d': 5 * DAY };
/** 横轴刻度落在本地时间的整十分 / 整点 / 零点上（recharts 自己取的是 13:46、16:40 这种） */
function timeTicks(min: number, max: number, range: string): number[] | undefined {
  const step = TICK_STEP[range];
  if (!step || !(max > min)) return undefined;
  const d = new Date(min); d.setHours(0, 0, 0, 0);
  const out: number[] = [];
  for (let t = d.getTime() + Math.ceil((min - d.getTime()) / step) * step; t <= max; t += step) out.push(t);
  return out.length >= 2 ? out : undefined; // 数据比一个步长还短（刚上架的机器看 7 天）：交给 recharts 自己取
}

/** 纵轴刻度：从 0 起 4 段。速率（1000 进位）每段取 1/2/2.5/5×10^k，字节（1024 进位）每段取 2 的幂，
 *  读出来是 250 Mbps、256 MiB 这样的整数，不会出现按十进制取整再换算成 MiB 的 143M、286M */
function valueTicks(max: number, base: 1000 | 1024): number[] | undefined {
  if (!(max > 0)) return undefined;
  const unit = base ** Math.max(0, Math.floor(Math.log(max) / Math.log(base)));
  const raw = max / unit / 4;
  let step: number;
  if (base === 1024) step = 2 ** Math.ceil(Math.log2(raw));
  else { const e = 10 ** Math.floor(Math.log10(raw)); step = ([1, 2, 2.5, 5, 10].find((n) => n * e >= raw) ?? 10) * e; }
  if (unit === 1) step = Math.max(1, step); // 最小单位（B / bps）不分小数格，否则标签取整后重复
  return [0, 1, 2, 3, 4].map((i) => i * step * unit);
}

export interface Line { key: string; label: string; color: string }
/** yBase：数值是速率（1000 进位）或字节（1024 进位）时给出，纵轴按该进位取整档刻度；百分比等固定范围的用 domain */
export const TimeChart = memo(function TimeChart({ points, lines, range, kind = 'area', yFmt, valueFmt, domain, yBase, height = 180, legend = true, connectNulls = false }: {
  points: SeriesPoint[] | undefined; lines: Line[]; range: string; kind?: 'area' | 'line'; yFmt: (v: number) => string; valueFmt?: (k: string, v: number) => string;
  domain?: [number | 'auto', number | 'auto']; yBase?: 1000 | 1024; height?: number; legend?: boolean; connectNulls?: boolean;
}) {
  const config = useMemo<ChartConfig>(() => Object.fromEntries(lines.map((l) => [l.key, { label: l.label, color: l.color }])), [lines]);
  const data = useMemo(() => (points ?? []).map((p) => ({ ...p, t: new Date(p.ts).getTime() })), [points]);
  const xTicks = useMemo(() => (data.length > 1 ? timeTicks(data[0].t, data[data.length - 1].t, range) : undefined), [data, range]);
  const yTicks = useMemo(() => {
    if (!yBase) return undefined;
    let max = 0;
    for (const d of data) for (const l of lines) { const v = (d as Record<string, unknown>)[l.key]; if (typeof v === 'number' && v > max) max = v; }
    return valueTicks(max, yBase);
  }, [data, lines, yBase]);
  const common = {
    data, margin: { top: 6, right: 8, bottom: 0, left: 0 },
  };
  const axes = (
    <>
      <CartesianGrid vertical={false} stroke="var(--chart-grid)" />
      <XAxis dataKey="t" type="number" scale="time" domain={['dataMin', 'dataMax']} ticks={xTicks} tickFormatter={xFmt(range)} {...axisProps} minTickGap={32} />
      <YAxis width={60} tickFormatter={yFmt} {...axisProps} interval={0} ticks={yTicks} domain={yTicks ? [0, yTicks[yTicks.length - 1]] : domain ?? [0, 'auto']} />
      <ChartTooltip config={config} labelFormat={(l) => { const d = new Date(Number(l)); return `${d.getMonth() + 1}-${p2(d.getDate())} ${p2(d.getHours())}:${p2(d.getMinutes())}`; }} valueFormat={valueFmt} />
    </>
  );
  return (
    <div style={{ display: 'grid', gap: 6 }}>
      {legend && lines.length > 1 && <ChartLegend config={config} />}
      <ChartContainer config={config} height={height}>
        {kind === 'area' ? (
          <AreaChart {...common}>
            {axes}
            {lines.map((l, i) => (
              <Area key={l.key} dataKey={l.key} type="monotone" stroke={`var(--color-${l.key})`} strokeWidth={1.5} fill={`var(--color-${l.key})`} fillOpacity={i === 0 ? 0.1 : 0.04} dot={false} isAnimationActive={false} connectNulls={connectNulls} />
            ))}
          </AreaChart>
        ) : (
          <LineChart {...common}>
            {axes}
            {lines.map((l) => <Line key={l.key} dataKey={l.key} type="monotone" stroke={`var(--color-${l.key})`} strokeWidth={1.5} dot={false} isAnimationActive={false} connectNulls={connectNulls} />)}
          </LineChart>
        )}
      </ChartContainer>
    </div>
  );
});
