// 时序小图：细线 + 淡填充（原版详情图的语言）。颜色取图表 token：--chart-1 / --chart-2 / --chart-3。
import type { SeriesPoint } from '@/lib/types';
import { ChartContainer, ChartLegend, ChartTooltip, Recharts, axisProps, type ChartConfig } from '@/components/ui/chart';

const { AreaChart, Area, LineChart, Line, XAxis, YAxis, CartesianGrid } = Recharts;
export const RANGES = [{ value: '1h', label: '1 小时' }, { value: '6h', label: '6 小时' }, { value: '24h', label: '24 小时' }, { value: '7d', label: '7 天' }, { value: '30d', label: '30 天' }] as const;

const p2 = (n: number) => String(n).padStart(2, '0');
const xFmt = (range: string) => (t: string | number) => {
  const d = new Date(t);
  return range === '7d' || range === '30d' ? `${p2(d.getMonth() + 1)}-${p2(d.getDate())}` : `${p2(d.getHours())}:${p2(d.getMinutes())}`;
};

export interface Line { key: string; label: string; color: string }
export function TimeChart({ points, lines, range, kind = 'area', yFmt, valueFmt, domain, height = 180, legend = true, connectNulls = false }: {
  points: SeriesPoint[] | undefined; lines: Line[]; range: string; kind?: 'area' | 'line'; yFmt: (v: number) => string; valueFmt?: (k: string, v: number) => string;
  domain?: [number | 'auto', number | 'auto']; height?: number; legend?: boolean; connectNulls?: boolean;
}) {
  const config: ChartConfig = Object.fromEntries(lines.map((l) => [l.key, { label: l.label, color: l.color }]));
  const data = (points ?? []).map((p) => ({ ...p, t: new Date(p.ts).getTime() }));
  const common = {
    data, margin: { top: 6, right: 8, bottom: 0, left: 0 },
  };
  const axes = (
    <>
      <CartesianGrid vertical={false} stroke="var(--chart-grid)" />
      <XAxis dataKey="t" type="number" scale="time" domain={['dataMin', 'dataMax']} tickFormatter={xFmt(range)} {...axisProps} minTickGap={48} />
      <YAxis width={52} tickFormatter={yFmt} {...axisProps} domain={domain ?? [0, 'auto']} />
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
}
