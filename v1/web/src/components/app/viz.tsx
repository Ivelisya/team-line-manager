// 首页的小型数据图形（全部是 HTML/SVG，不进 recharts 块）：
// 分段格条 Cells、带标题的资源格条 CellMeter、迷你资源条 MiniMeter、圆环 Ring、点阵迹线 DotTrace、面积折线 AreaSpark、彩色标签 Tag。
// 动效只靠 CSS：首次出现走挂载时的 @keyframes（格条从左往右扫亮、圆环转到位），之后读数变化直接到位、不过渡；令牌在系统“减少动态效果”时归零。
import * as React from 'react';
import type { Fmt } from '@/lib/format';
import * as F from '@/lib/format';
import { SERIES_N, type LivePoint } from '@/lib/live';
import { usageTone, type TagColor } from '@/lib/derive';
import { cn } from '@/lib/utils';
import { Num } from './primitives';

type Vars = React.CSSProperties & Record<`--${string}`, string | number>;
const clamp01 = (x: number) => Math.max(0, Math.min(1, x));
const CELL_N = 18;

/** 点亮部分的背景：逐格取色（相邻同色合并成一段），最后一格按剩余比例半亮；格子位置用 .cells 的 --tile（home.css） */
function litFill(colorOf: (i: number) => string, f: number) {
  const full = Math.floor(f);
  const at = (k: number) => `calc(${k} * var(--tile))`;
  const stops: string[] = [];
  for (let i = 0; i < full;) {
    const c = colorOf(i);
    let j = i + 1;
    while (j < full && colorOf(j) === c) j++;
    stops.push(`${c} ${at(i)} ${at(j)}`);
    i = j;
  }
  const part = f - full;
  if (part > 0) stops.push(`color-mix(in srgb, ${colorOf(full)} ${Math.round((0.42 + part * 0.58) * 100)}%, transparent) ${at(full)} ${at(full + 1)}`);
  stops.push(`transparent ${at(Math.ceil(f))}`);
  return `linear-gradient(to right, ${stops.join(', ')})`;
}

/** 分段格条：n 格，按比例点亮；最后一格按剩余比例半亮。colors 给了就逐格取色（额度条的绿→黄→红、在线率条）。
 *  整条只有一个元素：格子由 mask 切出（home.css .cells），点亮部分是一段背景渐变——每格一个元素时一张卡 70 多个、一页几千个。
 *  memo：卡片随推送重渲染时，比例没变的格条（额度、磁盘）不重画；colors 传模块常量 */
export const Cells = React.memo(function Cells({ pct, color, colors, n = CELL_N, className }: { pct: number | null; color?: string; colors?: readonly string[]; n?: number; className?: string }) {
  const f = pct == null ? 0 : clamp01(pct / 100) * n;
  const s: Vars = { '--n': n };
  if (f > 0) s.backgroundImage = litFill(colors ? (i) => colors[i] : () => color ?? 'var(--accent)', f);
  return <div className={cn('cells', className)} style={s} aria-hidden />;
});

/** 额度条每格的位置色：前半绿→黄，后半黄→红 */
export const quotaColors = (n: number) => Array.from({ length: n }, (_, i) => {
  const p = (i + 0.5) / n;
  return p < 0.5 ? `color-mix(in oklab, var(--warn) ${Math.round((p / 0.5) * 100)}%, var(--ok))` : `color-mix(in oklab, var(--bad) ${Math.round(((p - 0.5) / 0.5) * 100)}%, var(--warn))`;
});
export const QUOTA_COLORS = quotaColors(CELL_N);

/** 资源读数：图标 + 名称 + 数值，副行，格条。metric 决定颜色（--m-cpu / --m-mem / --m-disk / --m-load） */
export function CellMeter({ icon, label, value, pct, sub, metric, empty, tip }: { icon: React.ReactNode; label: string; value: Fmt; pct: number | null; sub?: React.ReactNode; metric: 'cpu' | 'mem' | 'disk' | 'load'; empty?: boolean; tip?: string }) {
  return (
    <div className={cn('cmeter', empty && 'is-empty')} data-tip={tip} data-metric={metric} data-tone={empty ? undefined : usageTone(pct, metric)}>
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
    <span className="tmeter" data-metric={metric} data-tone={empty ? undefined : usageTone(pct, metric)}>
      <Cells pct={empty ? null : pct} n={8} color={`var(--m-${metric})`} /><Num f={empty ? F.NA : F.pct(pct)} />
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

const DOT_BOX = 6; // px：每个点的背景层大小（最大的点直径 6px）
/** 点阵迹线：最近 n 个点（紧凑卡 20 个，列窄），最新在右；点的大小与浓淡随速率（相对显示的这几个点里的最大值），没数据的槽是浅灰小点。
 *  整条一个元素：每个点是一层径向渐变，按百分比定位（同 flex 两端对齐）。原来每点一个元素，一张卡 60 个，
 *  每条推送所有点都要改样式（实测 96 条迹线一次更新，样式与布局约 45ms，改成渐变层约 10ms，含拼字符串） */
export const DotTrace = React.memo(function DotTrace({ values, n = SERIES_N }: { values: number[]; n?: number }) {
  const shown = values.slice(-n);
  const max = Math.max(1, ...shown);
  const pad = n - shown.length;
  const layers = Array.from({ length: n }, (_, i) => {
    const v = i < pad ? 0 : shown[i - pad];
    const l = v > 0 ? clamp01(v / max) : -1;
    const r = l < 0 ? 1.5 : 2 * (0.8 + l * 0.7);
    const c = l < 0 ? 'var(--cell-off)' : `color-mix(in srgb, var(--spd, var(--accent)) ${Math.round((0.35 + l * 0.65) * 100)}%, transparent)`;
    return `radial-gradient(circle, ${c} ${r.toFixed(2)}px, transparent ${(r + 0.5).toFixed(2)}px) ${((i / (n - 1)) * 100).toFixed(2)}% 50% / ${DOT_BOX}px ${DOT_BOX}px no-repeat`;
  });
  return <span className="dots" style={{ background: layers.join(',') }} aria-hidden />;
});

/** 坐标上限取整档（×10^k），刻度读起来干净；档位密一些，速率小幅波动时坐标不会在 200 / 500 之间来回跳、曲线忽高忽扁 */
const NICE = [1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10];
function niceMax(v: number) {
  if (!(v > 0)) return 1e6;
  const e = 10 ** Math.floor(Math.log10(v));
  return (NICE.find((n) => n * e >= v) ?? 10) * e;
}

/** 面积折线（集群状态里的网卡上行 / 下行）：全站每分钟一个点（SERIES_N 个 = 近 30 分钟，见 lib/live）、最新在右；左轴三档刻度（0 只写「0」），
 *  两端标首尾两个点的时刻（到分钟）。悬停：竖线 + 那一分钟的时刻与速率 */
export function AreaSpark({ points, pick, tone }: { points: readonly LivePoint[]; pick: (p: LivePoint) => number; tone: 'up' | 'down' }) {
  const W = 300; const H = 100;
  const [hover, setHover] = React.useState<number | null>(null);
  const vals = points.map(pick);
  const max = niceMax(Math.max(0, ...vals) * 1.1);
  // 点不满 SERIES_N 时铺满整宽（两端的时间标签照实写），不让一小段线挤在最右边
  const step = W / Math.max(1, vals.length - 1);
  const x0 = vals.length > 1 ? 0 : W;
  const xy = vals.map((v, i) => [x0 + i * step, H - (v / max) * H] as const);
  const line = xy.map(([x, y], i) => `${i ? 'L' : 'M'}${x.toFixed(1)} ${y.toFixed(1)}`).join('');
  const last = xy[xy.length - 1];
  const dot = (p: readonly [number, number]) => ({ left: `${(p[0] / W) * 100}%`, top: `${(p[1] / H) * 100}%` });
  const onMove = (e: React.PointerEvent<HTMLDivElement>) => {
    if (vals.length < 2) return;
    const r = e.currentTarget.getBoundingClientRect();
    setHover(Math.max(0, Math.min(vals.length - 1, Math.round(((e.clientX - r.left) / r.width) * (vals.length - 1)))));
  };
  const h = hover != null && hover < xy.length ? hover : null;
  const hx = h != null ? (xy[h][0] / W) * 100 : 0;
  return (
    <div className="spark" data-tone={tone}>
      <div className="spark-axis" aria-hidden>{[max, max / 2, 0].map((v) => <span key={v}>{v ? F.str(F.rate(v, true)) : '0'}</span>)}</div>
      <div className="spark-plot" onPointerMove={onMove} onPointerLeave={() => setHover(null)}>
        <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" aria-hidden>
          {[0, H / 2, H].map((y) => <line key={y} className="spark-grid" x1={0} x2={W} y1={y} y2={y} vectorEffect="non-scaling-stroke" />)}
          {xy.length > 1 && <path className="spark-area" d={`${line}L${last[0]} ${H}L${x0} ${H}Z`} />}
          {xy.length > 1 && <path className="spark-line" d={line} vectorEffect="non-scaling-stroke" />}
          {h != null && <line className="spark-cursor" x1={xy[h][0]} x2={xy[h][0]} y1={0} y2={H} vectorEffect="non-scaling-stroke" />}
        </svg>
        {last && <i className="spark-dot" style={dot(h != null ? xy[h] : last)} />}
        {h != null && <span className="spark-tip" style={{ left: `${hx}%`, translate: hx < 25 ? '0 0' : hx > 75 ? '-100% 0' : '-50% 0' }}>{F.hm(points[h].t)} · {F.str(F.rate(vals[h]))}</span>}
      </div>
      <div className="spark-time" aria-hidden><span>{points.length > 1 ? F.hm(points[0].t) : ''}</span><span>{last ? F.hm(points[points.length - 1].t) : '等待数据…'}</span></div>
    </div>
  );
}

/** 彩色标签（供应商代号、类型、服务编号…）；tip 是悬停说明（全站 TipHost） */
export const Tag = ({ c, children, tip }: { c: TagColor; children: React.ReactNode; tip?: string }) => <span className="tag" data-c={c} data-tip={tip}>{children}</span>;
