// 每日业务流量柱状图：每天一根，下载（--chart-1）与上传（--chart-2）堆叠，悬停看当天的下载、上传与合计。流量页与抽屉「流量」页签共用。
// 最后一根是今天（到目前为止），画淡一点
import type * as React from 'react';
import { memo, useMemo } from 'react';
import type { DailyBiz } from '@/lib/types';
import * as F from '@/lib/format';
import { valueTicks } from '@/components/app/TimeChart';
import { ChartContainer, chartTooltip, Recharts, axisProps, type ChartConfig } from '@/components/ui/chart';

const { BarChart, Bar, Cell, XAxis, YAxis, CartesianGrid } = Recharts;
const CFG: ChartConfig = { down: { label: '↓ 下载', color: 'var(--chart-1)' }, up: { label: '↑ 上传', color: 'var(--chart-2)' } };
const CURSOR = { fill: 'var(--line)' };
const WEEK = '日一二三四五六';
/** 2026-10-24 → 10-24 */
export const md = (d: string) => d.slice(5);
/** 2026-10-24 → 周六（日期本身没有时区，按日历算星期） */
export const weekday = (d: string) => `周${WEEK[new Date(`${d}T00:00:00Z`).getUTCDay()]}`;
const yBytes = (v: number) => { if (!v) return '0'; const s = F.bytesScale(v); return `${F.inScale(v, s)} ${s.u}`; };
const vBytes = (_: string, v: number) => F.str(F.bytes(v));

/** 图例（放在面板标题旁，只读不能点掉）：下载 / 上传，加今天那根淡色的说明（样式同 TimeChart 的 ChartLegend） */
export function DailyLegend() {
  const sw = (c: string) => ({ '--lc': c }) as React.CSSProperties;
  return (
    <div className="chart-legend" aria-hidden>
      {Object.entries(CFG).map(([k, v]) => <span key={k} className="chart-legend-item is-static" style={sw(v.color as string)}><i />{v.label}</span>)}
      <span className="chart-legend-item is-static is-today" style={sw('var(--chart-1)')}><i />今天（到目前为止）</span>
    </div>
  );
}

export const DailyBars = memo(function DailyBars({ days, height = 200, yWidth = 60 }: { days: DailyBiz[]; height?: number; yWidth?: number }) {
  const data = useMemo(() => days.map((d) => ({ ...d, sum: d.down + d.up })), [days]);
  const ticks = useMemo(() => valueTicks(Math.max(0, ...data.map((d) => d.sum)), 1000), [data]);
  const last = days[days.length - 1]?.date;
  const label = (l: string | number) => {
    const d = data.find((x) => x.date === l);
    return `${md(String(l))} ${weekday(String(l))}${l === last ? '（今天，到目前为止）' : ''} · 合计 ${d ? F.str(F.bytes(d.sum)) : '—'}`;
  };
  // 非零的至少画 2px：4 TB 刻度下的 10 GB 不到 1px，整根看不见；0 仍不画
  const minH = useMemo(() => ({ down: (_: number | null | undefined, i: number) => (data[i]?.down ? 2 : 0), up: (_: number | null | undefined, i: number) => (data[i]?.up ? 2 : 0) }), [data]);
  const long = data.length > 31;
  const fade = (k: string) => data.map((d) => <Cell key={`${k}${d.date}`} fillOpacity={d.date === last ? 0.55 : 1} />);
  return (
    <ChartContainer config={CFG} height={height}>
      <BarChart data={data} margin={{ top: 6, right: 8, bottom: 0, left: 0 }} barCategoryGap={long ? 1 : '18%'}>
        <CartesianGrid vertical={false} stroke="var(--chart-grid)" />
        <XAxis dataKey="date" tickFormatter={md} minTickGap={long ? 36 : 24} {...axisProps} />
        <YAxis width={yWidth} tickFormatter={yBytes} ticks={ticks} interval={0} domain={ticks ? [0, ticks[ticks.length - 1]] : [0, 'auto']} {...axisProps} />
        {chartTooltip(CFG, label, vBytes, CURSOR)}
        <Bar dataKey="down" stackId="a" fill="var(--color-down)" minPointSize={minH.down} isAnimationActive={false}>{fade('d')}</Bar>
        <Bar dataKey="up" stackId="a" fill="var(--color-up)" radius={[2, 2, 0, 0]} minPointSize={minH.up} isAnimationActive={false}>{fade('u')}</Bar>
      </BarChart>
    </ChartContainer>
  );
});
