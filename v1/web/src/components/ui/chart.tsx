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
/** 悬停提示框 + 十字线（柱状图传 cursor={{ fill: … }}，画成整列底色）。是返回元素的函数而不是组件：recharts 按子元素的类型
 *  （displayName / name 为 Tooltip）找提示组件，包一层组件就对不上，连鼠标事件都不绑，图上永远没有提示框 */
export function chartTooltip(config: ChartConfig, labelFormat?: (l: string | number) => string, valueFormat?: (k: string, v: number) => string, cursor: object = { stroke: 'var(--line-strong)' }) {
  return (
    <Recharts.Tooltip
      cursor={cursor}
      isAnimationActive={false} // 提示框紧跟十字线；默认的 400ms 位移动画会让它拖在后面
      content={({ active, payload, label }: TipProps) => {
        if (!active || !payload?.length) return null;
        return (
          <div className="chart-tip">
            <div className="chart-tip-t">{labelFormat && label != null ? labelFormat(label) : label}</div>
            {payload.map((p) => (
              <div key={String(p.dataKey)} className="chart-tip-row">
                <span><i style={{ background: p.color }} />{config[String(p.dataKey)]?.label ?? p.dataKey}</span>
                <b className="num">{typeof p.value !== 'number' ? '—' : valueFormat ? valueFormat(String(p.dataKey), p.value) : p.value}</b>
              </div>
            ))}
          </div>
        );
      }}
    />
  );
}

/** 图例胶囊：点一下隐藏 / 显示这条曲线（hidden 里是被隐藏的 key） */
export function ChartLegend({ config, hidden, onToggle }: { config: ChartConfig; hidden: ReadonlySet<string>; onToggle: (key: string) => void }) {
  return (
    <div className="chart-legend">
      {Object.entries(config).map(([k, v]) => (
        <button key={k} type="button" className="chart-legend-item" aria-pressed={!hidden.has(k)} style={{ '--lc': v.color } as React.CSSProperties} onClick={() => onToggle(k)}
          data-tip={hidden.has(k) ? `显示「${v.label}」` : `隐藏「${v.label}」`}><i />{v.label}</button>
      ))}
    </div>
  );
}

export const axisProps = {
  stroke: 'var(--chart-axis)', tickLine: false, axisLine: false, fontSize: 11, tick: { fill: 'var(--chart-axis)' },
} as const;
export { Recharts };
