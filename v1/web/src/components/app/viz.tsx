// 首页的小型数据图形（全部是 HTML/SVG，不进 recharts 块）：
// 细进度条 Bar、带标题的资源条 Meter、迷你资源条 MiniMeter、圆环 Ring、迷你走势 Trend、面积折线 AreaSpark、彩色标签 Tag。
// 动效只靠 CSS：首次出现走挂载时的 @keyframes（进度条从左往右长出来、圆环转到位），之后读数变化直接到位、不过渡；令牌在系统“减少动态效果”时归零。
import * as React from 'react';
import type { Fmt } from '@/lib/format';
import * as F from '@/lib/format';
import { SERIES_N, type LivePoint } from '@/lib/live';
import { usageTone, type TagColor } from '@/lib/derive';
import { cn } from '@/lib/utils';
import { Num } from './primitives';

type Vars = React.CSSProperties & Record<`--${string}`, string | number>;
const clamp01 = (x: number) => Math.max(0, Math.min(1, x));

/** 细进度条：一条连续的圆角细条按比例填充（用户：不要一格一格的格子）。gradient 是铺满整条的渐变（额度条按位置绿→黄→红、
 *  在线率条的正常 / 离线两段），填充部分露出它左边对应的那一段；否则单色 color。memo：比例没变的（额度、磁盘）随推送不重画 */
export const Bar = React.memo(function Bar({ pct, color, gradient, className }: { pct: number | null; color?: string; gradient?: string; className?: string }) {
  const p = pct == null ? 0 : clamp01(pct / 100) * 100;
  const fill: React.CSSProperties = gradient
    ? { width: `${p}%`, backgroundImage: gradient, backgroundSize: `${(100 / p) * 100}% 100%` }
    : { width: `${p}%`, backgroundColor: color ?? 'var(--accent)' };
  return <div className={cn('bar', className)} aria-hidden>{p > 0 && <i style={fill} />}</div>;
});

/** 额度条：按位置取色，前半绿→黄，后半黄→红（中间色用 oklab 混，不发灰） */
export const QUOTA_FILL = 'linear-gradient(to right, var(--ok), color-mix(in oklab, var(--ok), var(--warn)) 25%, var(--warn) 50%, color-mix(in oklab, var(--warn), var(--bad)) 75%, var(--bad))';

/** 资源读数：图标 + 名称 + 数值，副行，进度条。metric 决定颜色（--m-cpu / --m-mem / --m-disk / --m-load） */
export function Meter({ icon, label, value, pct, sub, metric, empty, tip }: { icon: React.ReactNode; label: string; value: Fmt; pct: number | null; sub?: React.ReactNode; metric: 'cpu' | 'mem' | 'disk' | 'load'; empty?: boolean; tip?: string }) {
  return (
    <div className={cn('cmeter', empty && 'is-empty')} data-tip={tip} data-metric={metric} data-tone={empty ? undefined : usageTone(pct, metric)}>
      <div className="cmeter-head"><span className="cmeter-label">{icon}{label}</span><span className="cmeter-value"><Num f={empty ? F.NA : value} /></span></div>
      {/* 数据过期时副标题也是旧读数：留空行占位，卡片不跳高 */}
      {sub !== undefined && <div className="cmeter-sub">{empty ? ' ' : sub}</div>}
      <Bar pct={empty ? null : pct} color={`var(--m-${metric})`} />
    </div>
  );
}

/** 迷你资源条（表格单元格、悬浮卡）：细条 + 百分比；数据过期（empty）显示 “—” */
export function MiniMeter({ pct, metric, empty }: { pct: number | null; metric: 'cpu' | 'mem' | 'disk'; empty?: boolean }) {
  return (
    <span className="tmeter" data-metric={metric} data-tone={empty ? undefined : usageTone(pct, metric)}>
      <Bar pct={empty ? null : pct} color={`var(--m-${metric})`} /><Num f={empty ? F.NA : F.pct(pct)} />
    </span>
  );
}

const RING = 64; const RING_W = 6; const RING_R = (RING - RING_W) / 2; const RING_C = 2 * Math.PI * RING_R;
/** 圆环（64px，描边 6）：描边长度随比例变化，首次出现从 0 转到位 */
export function Ring({ pct, metric, children }: { pct: number | null; metric: 'cpu' | 'mem' | 'disk'; children?: React.ReactNode }) {
  const off = RING_C * (1 - clamp01((pct ?? 0) / 100));
  return (
    <div className="gauge" data-metric={metric} data-tone={usageTone(pct, metric)} style={{ width: RING, height: RING }}>
      <svg width={RING} height={RING} viewBox={`0 0 ${RING} ${RING}`} aria-hidden>
        <circle className="gauge-track" cx={RING / 2} cy={RING / 2} r={RING_R} strokeWidth={RING_W} />
        {pct != null && <circle className="gauge-val" cx={RING / 2} cy={RING / 2} r={RING_R} strokeWidth={RING_W} strokeDasharray={RING_C}
          transform={`rotate(-90 ${RING / 2} ${RING / 2})`} style={{ '--ring-c': RING_C, '--ring-off': off } as Vars} />}
      </svg>
      <div className="gauge-center">{children}</div>
    </div>
  );
}

const TW = 100; const TH = 20; // 视图框：宽按比例拉伸（preserveAspectRatio none），线宽不随拉伸变（non-scaling-stroke）
/** 平滑曲线：Catmull-Rom 换成三次贝塞尔，控制点纵向夹在视图框内（不冲出上下沿） */
function smooth(pts: (readonly [number, number])[]) {
  const y = (v: number) => Math.max(0, Math.min(TH, v));
  let d = `M${pts[0][0].toFixed(1)} ${pts[0][1].toFixed(1)}`;
  for (let i = 0; i < pts.length - 1; i++) {
    const p0 = pts[i - 1] ?? pts[i]; const p1 = pts[i]; const p2 = pts[i + 1]; const p3 = pts[i + 2] ?? p2;
    d += `C${(p1[0] + (p2[0] - p0[0]) / 6).toFixed(1)} ${y(p1[1] + (p2[1] - p0[1]) / 6).toFixed(1)} ${(p2[0] - (p3[0] - p1[0]) / 6).toFixed(1)} ${y(p2[1] - (p3[1] - p1[1]) / 6).toFixed(1)} ${p2[0].toFixed(1)} ${p2[1].toFixed(1)}`;
  }
  return d;
}
/** 迷你走势（卡片上的上行 / 下行）：最近 n 个点（紧凑卡 20 个，列窄）的平滑细线 + 淡面积，最新在右，高度相对这几个点里的最大值；
 *  点不满 n 个时铺满整宽（刚打开页面时实时流才来几个点，挤在右边只是一条竖线）。还没有数据是一条底线。颜色跟方向（--dir-c，同方向图标） */
export const Trend = React.memo(function Trend({ values, n = SERIES_N }: { values: number[]; n?: number }) {
  const shown = values.slice(-n);
  if (shown.length < 2) return <svg className="trend" viewBox={`0 0 ${TW} ${TH}`} preserveAspectRatio="none" aria-hidden><path className="trend-base" d={`M0 ${TH - 0.5}H${TW}`} vectorEffect="non-scaling-stroke" /></svg>;
  const max = Math.max(1, ...shown);
  const step = TW / (shown.length - 1);
  const pts = shown.map((v, i) => [i * step, TH - 1 - (Math.max(0, v) / max) * (TH - 3)] as const);
  const line = smooth(pts);
  return (
    <svg className="trend" viewBox={`0 0 ${TW} ${TH}`} preserveAspectRatio="none" aria-hidden>
      <path className="trend-area" d={`${line}L${TW} ${TH}L0 ${TH}Z`} />
      <path className="trend-line" d={line} vectorEffect="non-scaling-stroke" />
    </svg>
  );
});

/** 坐标上限取整档（×10^k），刻度读起来干净；档位密一些，速率小幅波动时坐标不会在 200 / 500 之间来回跳、曲线忽高忽扁 */
const NICE = [1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10];
function niceMax(v: number) {
  if (!(v > 0)) return 1e6;
  const e = 10 ** Math.floor(Math.log10(v));
  return (NICE.find((n) => n * e >= v) ?? 10) * e;
}

/** 面积折线（集群状态里的网卡上行 / 下行）：全站每分钟一个点（SERIES_N 个 = 近 30 分钟，见 lib/live）、最新在右；左轴三档刻度，两端标首尾两个点的时间 */
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

/** 彩色标签（供应商代号、类型、服务编号…）；tip 是悬停说明（全站 TipHost） */
export const Tag = ({ c, children, tip }: { c: TagColor; children: React.ReactNode; tip?: string }) => <span className="tag" data-c={c} data-tip={tip}>{children}</span>;
