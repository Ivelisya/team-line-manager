// 每日业务流量柱状图：每天一根，下载（--chart-1）与上传（--chart-2）堆叠，悬停看当天的下载、上传与合计。流量页与抽屉「流量」页签共用。
// 最后一根是今天（到目前为止），画淡一点。画法用 bklit（src/components/ui/charts）
import type * as React from 'react';
import { memo, useCallback, useMemo } from 'react';
import type { DailyBiz } from '@/lib/types';
import * as F from '@/lib/format';
import { valueTicks } from '@/lib/chart';
import { BarChart } from '@/components/ui/charts/bar-chart';
import { Bar } from '@/components/ui/charts/bar';
import { BarXAxis } from '@/components/ui/charts/bar-x-axis';
import { Grid } from '@/components/ui/charts/grid';
import { YAxis } from '@/components/ui/charts/y-axis';
import { ChartTooltip } from '@/components/ui/charts/tooltip';
import '@/styles/charts.css';

const DOWN = { label: '↓ 下载', color: 'var(--chart-1)' };
const UP = { label: '↑ 上传', color: 'var(--chart-2)' };
const WEEK = '日一二三四五六';
/** 2026-10-24 → 10-24 */
export const md = (d: string) => d.slice(5);
/** 2026-10-24 → 周六（日期本身没有时区，按日历算星期） */
export const weekday = (d: string) => `周${WEEK[new Date(`${d}T00:00:00Z`).getUTCDay()]}`;
const yBytes = (v: number) => { if (!v) return '0'; const s = F.bytesScale(v); return `${F.inScale(v, s)} ${s.u}`; };
const TODAY_OPACITY = 0.55;

/** 图例（放在面板标题旁，只读不能点掉）：下载 / 上传，加今天那根淡色的说明 */
export function DailyLegend() {
  const sw = (c: string) => ({ '--lc': c }) as React.CSSProperties;
  return (
    <div className="db-legend" aria-hidden>
      {[DOWN, UP].map((v) => <span key={v.label} className="db-legend-item" style={sw(v.color)}><i />{v.label}</span>)}
      <span className="db-legend-item is-today" style={sw('var(--chart-1)')}><i />今天（到目前为止）</span>
    </div>
  );
}

export const DailyBars = memo(function DailyBars({ days, height = 200, yWidth = 60 }: { days: DailyBiz[]; height?: number; yWidth?: number }) {
  const data = useMemo(() => days.map((d) => ({ ...d, sum: d.down + d.up })), [days]);
  const ticks = useMemo(() => valueTicks(Math.max(0, ...data.map((d) => d.sum)), 1000), [data]); // 全为 0 时 valueTicks 给 0 / 250 / … / 1000，网格与纵轴标签对得上
  const last = days[days.length - 1]?.date;
  const long = data.length > 31;
  const title = useCallback((p: Record<string, unknown>) => {
    const date = String(p.date);
    return `${md(date)} ${weekday(date)}${date === last ? '（今天，到目前为止）' : ''} · 合计 ${F.str(F.bytes(Number(p.sum)))}`;
  }, [last]);
  const rows = useCallback((p: Record<string, unknown>) => [
    { ...DOWN, value: F.str(F.bytes(Number(p.down))) },
    { ...UP, value: F.str(F.bytes(Number(p.up))) },
  ], []);
  const dim = useCallback((p: Record<string, unknown>) => (p.date === last ? TODAY_OPACITY : 1), [last]);
  return (
    <BarChart data={data} xDataKey="date" aspectRatio="" style={{ height }} margin={{ top: 8, right: 10, bottom: 26, left: yWidth }} stacked barGap={long ? 0.08 : 0.18}
      yDomain={[0, ticks[ticks.length - 1]]} animationDuration={0}>
      <Grid horizontal vertical={false} strokeDasharray="0" rowTickValues={ticks} numTicksRows={4} />
      <Bar dataKey="down" fill={DOWN.color} lineCap={3} roundEnd animate={false} minBarHeight={2} pointOpacity={dim} fadedOpacity={0.4} />
      <Bar dataKey="up" fill={UP.color} lineCap={3} roundEnd animate={false} minBarHeight={2} pointOpacity={dim} fadedOpacity={0.4} />
      <BarXAxis formatLabel={md} minLabelGap={long ? 46 : 52} fadeOnHover={false} />
      <YAxis ticks={ticks} formatValue={yBytes} animate={false} />
      <ChartTooltip showDatePill={false} showDots={false} showCrosshair={false} damping={0} rows={rows} title={title} />
    </BarChart>
  );
});
