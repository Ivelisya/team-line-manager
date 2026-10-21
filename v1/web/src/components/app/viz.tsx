// 首页的小型数据图形（全部是 HTML/SVG，不进 recharts 块）：
// 分段格条 Cells、带标题的资源格条 CellMeter、迷你资源条 MiniMeter、圆环 Ring、点阵迹线 DotTrace、面积折线 AreaSpark、彩色标签 Tag。
// 动效只靠 CSS：值变化走 transition，首次出现走 @starting-style；令牌在系统“减少动态效果”时归零。
import * as React from 'react';
import type { Fmt } from '@/lib/format';
import * as F from '@/lib/format';
import { SERIES_N, type LivePoint } from '@/lib/live';
import { usageTone, type TagColor } from '@/lib/derive';
import { cn } from '@/lib/utils';
import { Num } from './primitives';

type Vars = React.CSSProperties & Record<`--${string}`, string | number>;
const clamp01 = (x: number) => Math.max(0, Math.min(1, x));
export const CELL_N = 18;

/** 分段格条：n 格，按比例点亮；最后一格按剩余比例半亮。colors 给了就逐格取色（额度条的绿→黄→红）。
 *  memo：卡片随推送重渲染时，比例没变的格条（额度、磁盘）不重画；colors 传模块常量 */
export const Cells = React.memo(function Cells({ pct, color, colors, n = CELL_N, className }: { pct: number | null; color?: string; colors?: readonly string[]; n?: number; className?: string }) {
  const f = pct == null ? 0 : clamp01(pct / 100) * n;
  return (
    <div className={cn('cells', className)} style={color ? ({ '--c': color } as Vars) : undefined} aria-hidden>
      {Array.from({ length: n }, (_, i) => {
        const l = clamp01(f - i);
        const s: Vars = { '--i': i };
        if (l > 0) s['--l'] = l;
        if (colors) s['--c'] = colors[i];
        return <i key={i} style={s} data-on={l > 0 ? '' : undefined} />;
      })}
    </div>
  );
});

/** 额度条每格的位置色：前半绿→黄，后半黄→红 */
export const quotaColors = (n: number) => Array.from({ length: n }, (_, i) => {
  const p = (i + 0.5) / n;
  return p < 0.5 ? `color-mix(in oklab, var(--warn) ${Math.round((p / 0.5) * 100)}%, var(--ok))` : `color-mix(in oklab, var(--bad) ${Math.round(((p - 0.5) / 0.5) * 100)}%, var(--warn))`;
});
export const QUOTA_COLORS = quotaColors(CELL_N);

/** 资源读数：图标 + 名称 + 数值，副行，格条。metric 决定颜色（--m-cpu / --m-mem / --m-disk / --m-load） */
export function CellMeter({ icon, label, value, pct, sub, metric, empty, title }: { icon: React.ReactNode; label: string; value: Fmt; pct: number | null; sub?: React.ReactNode; metric: 'cpu' | 'mem' | 'disk' | 'load'; empty?: boolean; title?: string }) {
  return (
    <div className={cn('cmeter', empty && 'is-empty')} title={title} data-metric={metric} data-tone={empty ? undefined : usageTone(pct)}>
      <div className="cmeter-head"><span className="cmeter-label">{icon}{label}</span><span className="cmeter-value"><Num f={empty ? F.NA : value} /></span></div>
      {/* 数据过期时副标题也是旧读数：留空行占位，卡片不跳高 */}
      {sub !== undefined && <div className="cmeter-sub">{empty ? ' ' : sub}</div>}
      <Cells pct={empty ? null : pct} color={`var(--m-${metric})`} />
    </div>
  );
}

/** 迷你资源条（表格单元格、悬浮卡）：8 格 + 百分比；数据过期（empty）显示 “—” */
export function MiniMeter({ pct, metric, empty }: { pct: number | null; metric: 'cpu' | 'mem' | 'disk'; empty?: boolean }) {
  return (
    <span className="tmeter" data-metric={metric} data-tone={empty ? undefined : usageTone(pct)}>
      <Cells pct={empty ? null : pct} n={8} color={`var(--m-${metric})`} /><Num f={empty ? F.NA : F.pct(pct)} />
    </span>
  );
}

/** 圆环：描边长度随比例变化，首次出现从 0 转到位 */
export function Ring({ pct, metric, size = 64, stroke = 6, children }: { pct: number | null; metric: 'cpu' | 'mem' | 'disk'; size?: number; stroke?: number; children?: React.ReactNode }) {
  const r = (size - stroke) / 2;
  const c = 2 * Math.PI * r;
  const off = c * (1 - clamp01((pct ?? 0) / 100));
  return (
    <div className="gauge" data-metric={metric} data-tone={usageTone(pct)} style={{ width: size, height: size }}>
      <svg width={size} height={size} viewBox={`0 0 ${size} ${size}`} aria-hidden>
        <circle className="gauge-track" cx={size / 2} cy={size / 2} r={r} strokeWidth={stroke} />
        {pct != null && <circle className="gauge-val" cx={size / 2} cy={size / 2} r={r} strokeWidth={stroke} strokeDasharray={c}
          transform={`rotate(-90 ${size / 2} ${size / 2})`} style={{ '--ring-c': c, '--ring-off': off } as Vars} />}
      </svg>
      <div className="gauge-center">{children}</div>
    </div>
  );
}

/** 点阵迹线：最近 SERIES_N 个点，最新在右；点的大小与浓淡随速率（相对本条最大值），没数据的槽是浅灰小点 */
export const DotTrace = React.memo(function DotTrace({ values }: { values: number[] }) {
  const max = Math.max(1, ...values);
  const pad = SERIES_N - values.length;
  return (
    <span className="dots" aria-hidden>
      {Array.from({ length: SERIES_N }, (_, i) => {
        const v = i < pad ? null : values[i - pad];
        return <i key={i} data-on={v != null && v > 0 ? '' : undefined} style={v != null && v > 0 ? ({ '--l': clamp01(v / max) } as Vars) : undefined} />;
      })}
    </span>
  );
});

/** 坐标上限取 1/2/5×10^k 的整档，刻度读起来干净 */
function niceMax(v: number) {
  if (!(v > 0)) return 1e6;
  const e = 10 ** Math.floor(Math.log10(v));
  const f = v / e;
  return (f <= 1 ? 1 : f <= 2 ? 2 : f <= 5 ? 5 : 10) * e;
}

/** 面积折线（集群状态里的上行 / 下行）：最近 SERIES_N 个点、最新在右；左轴三档刻度，两端标时间 */
export function AreaSpark({ points, pick, tone }: { points: readonly LivePoint[]; pick: (p: LivePoint) => number; tone: 'up' | 'down' }) {
  const W = 300; const H = 100;
  const vals = points.map(pick);
  const max = niceMax(Math.max(0, ...vals) * 1.1);
  // 点不满 SERIES_N 时铺满整宽（两端的时间标签照实写），不让一小段线挤在最右边
  const step = W / Math.max(1, vals.length - 1);
  const x0 = vals.length > 1 ? 0 : W;
  const xy = vals.map((v, i) => [x0 + i * step, H - (v / max) * H] as const);
  const line = xy.map(([x, y], i) => `${i ? 'L' : 'M'}${x.toFixed(1)} ${y.toFixed(1)}`).join('');
  const last = xy[xy.length - 1];
  return (
    <div className="spark" data-tone={tone}>
      <div className="spark-axis" aria-hidden>{[max, max / 2, 0].map((v) => <span key={v}>{F.rate(v).v} {F.rate(v).u || 'bps'}</span>)}</div>
      <div className="spark-plot">
        <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" aria-hidden>
          {[0, H / 2, H].map((y) => <line key={y} className="spark-grid" x1={0} x2={W} y1={y} y2={y} vectorEffect="non-scaling-stroke" />)}
          {xy.length > 1 && <path className="spark-area" d={`${line}L${last[0]} ${H}L${x0} ${H}Z`} />}
          {xy.length > 1 && <path className="spark-line" d={line} vectorEffect="non-scaling-stroke" />}
        </svg>
        {last && <i className="spark-dot" style={{ left: `${(last[0] / W) * 100}%`, top: `${(last[1] / H) * 100}%` }} />}
      </div>
      <div className="spark-time" aria-hidden><span>{points.length > 1 ? F.clock(new Date(points[0].t).toISOString()) : ''}</span><span>{last ? F.clock(new Date(points[points.length - 1].t).toISOString()) : '等待数据…'}</span></div>
    </div>
  );
}

/** 彩色标签（供应商代号、类型、服务编号…） */
export const Tag = ({ c, children, title }: { c: TagColor; children: React.ReactNode; title?: string }) => <span className="tag" data-c={c} title={title}>{children}</span>;
