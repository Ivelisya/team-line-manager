// shadcn chart 写法：ChartContainer 注入 --color-<key> 变量，Recharts 里用 var(--color-key)；工具提示统一样式。
import * as React from 'react';
import * as Recharts from 'recharts';
import { cn } from '@/lib/utils';

export type ChartConfig = Record<string, { label: string; color: string }>;

export function ChartContainer({ config, className, height, children }: { config: ChartConfig; className?: string; height?: number; children: React.ReactElement }) {
  const style = { ...Object.fromEntries(Object.entries(config).map(([k, v]) => [`--color-${k}`, v.color])), ...(height ? { height } : {}) } as React.CSSProperties;
  return (
    <div className={cn('chart-box', className)} style={style}>
      <Recharts.ResponsiveContainer width="100%" height="100%">{children}</Recharts.ResponsiveContainer>
    </div>
  );
}

interface TipProps { active?: boolean; payload?: { dataKey?: string | number; value?: unknown; color?: string }[]; label?: string | number }
export function ChartTooltip({ config, labelFormat, valueFormat }: { config: ChartConfig; labelFormat?: (l: string | number) => string; valueFormat?: (k: string, v: number) => string }) {
  return (
    <Recharts.Tooltip
      cursor={{ stroke: 'var(--line-strong)' }}
      content={({ active, payload, label }: TipProps) => {
        if (!active || !payload?.length) return null;
        return (
          <div className="popover" style={{ padding: '8px 10px', minWidth: 140, fontSize: 12 }}>
            <div style={{ color: 'var(--fg-2)', marginBottom: 4 }}>{labelFormat && label != null ? labelFormat(label) : label}</div>
            {payload.map((p) => (
              <div key={String(p.dataKey)} style={{ display: 'flex', justifyContent: 'space-between', gap: 12 }}>
                <span><i style={{ display: 'inline-block', width: 8, height: 8, borderRadius: 2, background: p.color, marginRight: 6 }} />{config[String(p.dataKey)]?.label ?? p.dataKey}</span>
                <b className="num">{typeof p.value !== 'number' ? '—' : valueFormat ? valueFormat(String(p.dataKey), p.value) : p.value}</b>
              </div>
            ))}
          </div>
        );
      }}
    />
  );
}

export function ChartLegend({ config }: { config: ChartConfig }) {
  return (
    <div className="chart-legend">
      {Object.entries(config).map(([k, v]) => <span key={k}><i style={{ background: v.color }} />{v.label}</span>)}
    </div>
  );
}

export const axisProps = {
  stroke: 'var(--chart-axis)', tickLine: false, axisLine: false, fontSize: 11, tick: { fill: 'var(--chart-axis)' },
} as const;
export { Recharts };
