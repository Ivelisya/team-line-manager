// 图例胶囊：点一下隐藏 / 显示这条曲线（hidden 里是被隐藏的 key）。bklit 的 Legend 是纵向的「名称 + 数值 + 占比」列表、只有悬停高亮，没有点掉这一档，
// 所以这里只留一个很薄的按钮条；样式在 styles/charts.css（.tc-legend）。
import type { CSSProperties } from 'react';

export interface LegendLine { key: string; label: string; color: string }

export function ChartLegend({ lines, hidden, onToggle }: { lines: readonly LegendLine[]; hidden: ReadonlySet<string>; onToggle: (key: string) => void }) {
  return (
    <div className="tc-legend">
      {lines.map((l) => (
        <button key={l.key} type="button" className="tc-legend-item" aria-pressed={!hidden.has(l.key)} style={{ '--lc': l.color } as CSSProperties} onClick={() => onToggle(l.key)}
          data-tip={hidden.has(l.key) ? `显示「${l.label}」` : `隐藏「${l.label}」`}><i />{l.label}</button>
      ))}
    </div>
  );
}
