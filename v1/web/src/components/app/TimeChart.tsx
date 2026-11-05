// 时序小图：细线 + 淡填充（原版详情图的语言），画法用 bklit（src/components/ui/charts）。颜色由调用方给：图表 token --chart-1 / --chart-2 / --chart-3，或首页的指标色 --m-cpu / --m-mem。
// memo：抽屉随实时推送重渲染时，曲线点没变（lines / 格式化函数由调用方放在模块常量里）就不重画
import { memo, useCallback, useMemo, useState } from 'react';
import { curveMonotoneX } from 'd3-shape';
import { scaleTime } from 'd3-scale';
import { useReducedMotion } from 'motion/react';
import type { SeriesPoint } from '@/lib/types';
import { RANGE_MS, autoTicks, intTicks, niceCeil, pctTicks, valueTicks } from '@/lib/chart';
import { dt, hm } from '@/lib/format';
import { Area, AreaChart } from '@/components/ui/charts/area-chart';
import { Line, LineChart } from '@/components/ui/charts/line-chart';
import { Grid } from '@/components/ui/charts/grid';
import { XAxis } from '@/components/ui/charts/x-axis';
import { YAxis } from '@/components/ui/charts/y-axis';
import { ReferenceArea } from '@/components/ui/charts/reference-area';
import { ChartTooltip } from '@/components/ui/charts/tooltip';
import { useChartStable } from '@/components/ui/charts/chart-context';
import { ChartLegend } from '@/components/ui/charts/legend-toggle';
import '@/styles/charts.css';

export const RANGES = [{ value: '1h', label: '1 小时' }, { value: '6h', label: '6 小时' }, { value: '24h', label: '24 小时' }, { value: '7d', label: '7 天' }, { value: '30d', label: '30 天' }] as const;

/** 按天的标日期；按小时的标时刻，落在零点的标日期（一两天的曲线跨日，不会出现两个一样的 06:00 分不清哪天） */
const xFmt = (range: string) => (t: string | number) => {
  const d = new Date(t);
  return range === '7d' || range === '30d' || (d.getHours() === 0 && d.getMinutes() === 0) ? dt(t).slice(0, 5) : hm(t);
};

const MIN = 60_000; const HOUR = 60 * MIN; const DAY = 24 * HOUR;
const TICK_STEP: Record<string, number> = { '1h': 10 * MIN, '6h': HOUR, '24h': 3 * HOUR, '7d': DAY, '30d': 5 * DAY };
/** 刻度步长与格式按数据实际跨度取，不按所选范围：刚上架或早已离线的机器看「7 天」，数据只有一天多，按 7 天只标日期会标出一排相同的 10-20 */
const spanRange = (span: number) => (span <= HOUR ? '1h' : span <= 6 * HOUR ? '6h' : span <= 2 * DAY ? '24h' : span <= 7 * DAY ? '7d' : '30d');
/** 横轴刻度落在本地时间的整十分 / 整点 / 零点上（图表库自己取的会是 13:46、16:40 这种） */
function timeTicks(min: number, max: number, range: string): number[] | undefined {
  const step = TICK_STEP[range];
  if (!step || !(max > min)) return undefined;
  const d = new Date(min); d.setHours(0, 0, 0, 0);
  const out: number[] = [];
  for (let t = d.getTime() + Math.ceil((min - d.getTime()) / step) * step; t <= max; t += step) out.push(t);
  return out.length >= 2 ? out : undefined; // 数据比一个步长还短（刚上架的机器看 7 天）：由调用处改用 d3 的时间刻度兜底
}

/** 没有数据的时段（所选范围左边还没上架、中间离线、右边停了上报）每隔一段垫一个空点（只有时间）：悬停那里时没有提示
 *  （这一行没有任何序列值，提示不出来），不会吸到很远的那个真实点上；曲线也在空段断开，不把两头连成一条斜线。
 *  「空段」= 比常见点距的 3 倍还长；垫点的间隔不小于常见点距、整个范围最多约 240 个 */
function withGaps<T extends { t: number }>(pts: T[], span: [number, number] | null): (T | { t: number })[] {
  if (pts.length < 2 || !span) return pts;
  const diffs = pts.slice(1).map((p, i) => p.t - pts[i].t).sort((a, b) => a - b);
  const typical = diffs[diffs.length >> 1];
  if (!(typical > 0)) return pts;
  const step = Math.max(typical, (span[1] - span[0]) / 240);
  const out: (T | { t: number })[] = [];
  // (from, to) 之间隔 step 垫点，两头各让出半步
  const fill = (from: number, to: number) => { if (to - from > 3 * typical) for (let t = from + step / 2; t < to - step / 2; t += step) out.push({ t }); };
  fill(span[0] - step / 2, pts[0].t);
  pts.forEach((p, i) => { if (i) fill(pts[i - 1].t, p.t); out.push(p); });
  fill(pts[pts.length - 1].t, span[1] + step / 2);
  return out;
}

/** 常见点距（相邻点间隔的中位数）；比它的 3 倍还长的空档算「没有数据」 */
function typicalStep(pts: { t: number }[]): number {
  if (pts.length < 2) return 0;
  const diffs = pts.slice(1).map((p, i) => p.t - pts[i].t).sort((a, b) => a - b);
  return diffs[diffs.length >> 1];
}
/** 第 q 分位数（0–1） */
const quantile = (sorted: number[], q: number) => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * q))];
const MAX_OUT = 40;
export interface Line { key: string; label: string; color: string }
/** 面积填充的顶端不透明度：第一条 0.1 左右的观感（带向下渐隐，顶端取高一点），其余更淡 */
const FILL_TOP = [0.22, 0.09];
/** 截顶的点另存真实值，悬停读它 */
const rawKey = (k: string) => `${k}$raw`;
const REVEAL_MS = 600; // 首次显示 / 换时间范围时从左往右展开一次；同范围的刷新不播
const TIP_SPRING = { stiffness: 1500, damping: 90 }; // 十字线、悬停点几乎没有惯性（点少的小图才会走弹簧，密集的曲线本来就是直接跳）

/** 超出纵轴上限的点：画在顶边，用小三角标出（悬停仍是真实值） */
function OutMarks({ marks }: { marks: { t: number; color: string }[] }) {
  const { xScale } = useChartStable();
  return (
    <g aria-hidden="true">
      {marks.map((o, i) => {
        const x = xScale(new Date(o.t));
        return x == null ? null : <path key={i} d={`M${x - 4} 6L${x + 4} 6L${x} 0Z`} fill={o.color} />;
      })}
    </g>
  );
}

/** yBase：数值是速率（1000 进位）或字节（1024 进位）时给出，纵轴按该进位取整档刻度；百分比等固定范围的用 domain。
 *  empty：每条曲线全为 0（或缺测）时不画图，原处显示这句话（否则纵轴只剩一组 0–4 的空刻度）。
 *  full：横轴取所选范围（到现在为止的 1 小时 … 30 天）；数据从范围中间才开始时，横轴从第一条数据画起，
 *  范围里更早的那一段压成左边一条窄窄的斜纹（暂无数据），不把两天的数据挤在 30 天的右边一小条。
 *  数据中间或末尾的空档（离线、停止上报）同样画斜纹，holeLabel 写那是什么（默认「暂无数据」，主机页签是「agent 离线」）。
 *  clip：纵轴按 p99（至少让最大的一个点可截）截断——少数尖峰不再把整张图压扁；超出的点画在顶边并标小三角（悬停仍是真实值） */
export const TimeChart = memo(function TimeChart({ points, lines, range, kind = 'area', yFmt, valueFmt, domain, yBase, pct, height = 180, yWidth = 60, empty, full, clip, holeLabel = '暂无数据' }: {
  points: SeriesPoint[] | undefined; lines: Line[]; range: string; kind?: 'area' | 'line'; yFmt: (v: number) => string; valueFmt?: (k: string, v: number) => string;
  domain?: [number | 'auto', number | 'auto']; yBase?: 1000 | 1024; pct?: boolean; height?: number; yWidth?: number; empty?: string; full?: boolean; clip?: boolean; holeLabel?: string;
}) {
  // 图例点掉的曲线：不画，纵轴刻度也只按还显示的曲线取；最后一条点不掉（全关了只剩空坐标轴）
  const [hidden, setHidden] = useState<ReadonlySet<string>>(() => new Set());
  const toggle = useCallback((k: string) => setHidden((h) => {
    const n = new Set(h);
    if (!n.delete(k)) { if (n.size >= lines.length - 1) return h; n.add(k); }
    return n;
  }), [lines.length]);
  const shown = useMemo(() => lines.filter((l) => !hidden.has(l.key)), [lines, hidden]);
  const real = useMemo(() => (points ?? []).map((p) => ({ ...p, t: new Date(p.ts).getTime() })), [points]);
  // 横轴：full 时是 [现在 − 所选范围, 现在]（数据取回时算一次，不随每次重渲染挪动），数据晚于范围起点时改从第一条数据（减去左边那条斜纹）画起；否则数据首尾。
  // lead：左边斜纹的时长（0 = 没有）；first：第一条数据的时刻
  const { span, lead, first, typical } = useMemo(() => {
    const typical = typicalStep(real); const first = real[0]?.t ?? 0;
    if (full && RANGE_MS[range]) {
      const end = Math.max(Date.now(), real[real.length - 1]?.t ?? 0); const start = end - RANGE_MS[range];
      const lead = real.length > 1 && first - start > 3 * typical ? Math.min(first - start, (end - first) * 0.16) : 0;
      return { span: [lead ? first - lead : start, end] as [number, number], lead, first, typical };
    }
    return { span: real.length > 1 ? [first, real[real.length - 1].t] as [number, number] : null, lead: 0, first, typical };
  }, [real, full, range]);
  const data = useMemo(() => withGaps(real, span), [real, span]);
  // 数据里的空档（斜纹）：左边的 lead，中间的、末尾的（比常见点距的 3 倍还长）
  const holes = useMemo(() => {
    const out: { a: number; b: number; label: string }[] = [];
    if (!span || !(typical > 0)) return out;
    if (lead) out.push({ a: span[0], b: first, label: '暂无数据' });
    real.forEach((p, i) => { if (i && p.t - real[i - 1].t > 3 * typical) out.push({ a: real[i - 1].t, b: p.t, label: holeLabel }); });
    const last = real[real.length - 1].t;
    if (full && span[1] - last > 3 * typical) out.push({ a: last, b: span[1], label: holeLabel });
    return out;
  }, [span, lead, first, typical, real, full, holeLabel]);
  const xRange = full && !lead ? range : span ? spanRange(span[1] - span[0] - lead) : range;
  // 数据很短（刚上架）时 timeTicks 给不出刻度：用 d3 的本地时间刻度取 3–5 个，仍走 xFmt（不落到图表库默认的 en-US「Nov 5」）；只有一个点时标它自己
  const xTicks = useMemo(() => {
    if (!span) return real.length ? [real[0].t] : undefined;
    const t = timeTicks(span[0] + lead, span[1], xRange);
    if (t) return t;
    const d = scaleTime().domain([new Date(span[0] + lead), new Date(span[1])]).ticks(4).map((x) => x.getTime());
    return d.length ? d : [span[0] + lead, span[1]];
  }, [span, lead, xRange, real]);
  const xFormat = useMemo(() => xFmt(xRange), [xRange]);
  // 纵轴：clip 时按 p99 截断（最大值不到 p99 的 1.3 倍，没有值得截的尖峰，照常按最大值）
  const y = useMemo(() => {
    const vals: number[] = [];
    for (const d of data) for (const l of shown) { const v = (d as Record<string, unknown>)[l.key]; if (typeof v === 'number' && v > 0) vals.push(v); }
    const max = vals.length ? Math.max(...vals) : 0;
    // 上限取 p99（先去掉最大的一个点再取：点少时 p99 就是最大值，一个尖峰也得能截）
    const p99 = vals.length > 1 ? quantile([...vals].sort((a, b) => a - b).slice(0, -1), 0.99) : vals[0] ?? 0;
    const cut = !!clip && p99 > 0 && max > p99 * 1.3;
    const top = cut ? p99 : max;
    let ticks = yBase ? valueTicks(top, yBase) : pct && top > 0 ? pctTicks(top) : clip && top > 0 ? intTicks(top) : undefined;
    let ceil = ticks ? ticks[ticks.length - 1] : cut ? niceCeil(top) : undefined;
    // 固定范围（百分比 0–100）：上下限就是范围，刻度均分 4 段
    if (ceil == null && typeof domain?.[0] === 'number' && typeof domain[1] === 'number') {
      const [lo, hi] = domain; ceil = hi; ticks = [0, 1, 2, 3, 4].map((i) => lo + ((hi - lo) * i) / 4);
    }
    // 没给进位 / 范围的图（连接数、温度、出口延迟 …）：自己算 4 段刻度，同一份交给 Grid 与 YAxis，上限就是最后一个刻度；全为 0 时 0 与 1
    if (ceil == null) {
      ticks = autoTicks(top, vals.every(Number.isInteger)); ceil = ticks[ticks.length - 1];
    }
    return { ticks, ceil, floor: typeof domain?.[0] === 'number' ? domain[0] : 0, cut };
  }, [data, shown, yBase, pct, clip, domain]);
  // 画的行：每行只带 t 和还显示的曲线值（没有值的键不写，bklit 据此断线）；超出上限的值压到上限，真实值另存，悬停读真实值
  const { rows, out } = useMemo(() => {
    const out: { t: number; color: string }[] = [];
    const rows = data.map((d) => {
      const src = d as Record<string, unknown>; const row: Record<string, unknown> = { t: (d as { t: number }).t };
      for (const l of shown) {
        const v = src[l.key];
        if (typeof v !== 'number') continue;
        if (y.cut && y.ceil != null && v > y.ceil) { row[l.key] = y.ceil; row[rawKey(l.key)] = v; if (out.length < MAX_OUT) out.push({ t: row.t as number, color: l.color }); } else row[l.key] = v;
      }
      return row;
    });
    return { rows, out };
  }, [data, shown, y]);
  const none = useMemo(() => !!empty && data.every((d) => lines.every((l) => !(d as Record<string, unknown>)[l.key])), [data, lines, empty]);
  const reduce = useReducedMotion();
  const tipRows = useCallback((p: Record<string, unknown>) => shown.map((l) => {
    const v = (p[rawKey(l.key)] ?? p[l.key]) as number | undefined;
    return { color: l.color, label: l.label, value: typeof v !== 'number' ? '—' : valueFmt ? valueFmt(l.key, v) : String(v) };
  }), [shown, valueFmt]);
  const tipTitle = useCallback((p: Record<string, unknown>) => dt(p.t as number), []);
  if (none) return <div className="chart-empty" style={{ height }}>{empty}</div>;
  const labelW = Math.max(0, ...(y.ticks ?? []).map((v) => yFmt(v).length)) * 6.4 + 14; // 刻度文字 11px：数字 / 字母 ≈ 6.4px 一个
  const margin = { top: 8, right: 10, bottom: 26, left: Math.max(yWidth, Math.ceil(labelW)) };
  const xDomain = span ? ([new Date(span[0]), new Date(span[1])] as [Date, Date]) : undefined;
  // 数组而不是 <>…</>：AreaChart 只在直接子元素里找 Area 来登记序列（纵轴取值、提示）
  const children = [
    <Grid key="grid" horizontal vertical={false} strokeDasharray="0" rowTickValues={y.ticks} numTicksRows={4} />,
    ...holes.map((h, i) => (
      <ReferenceArea key={`hole${i}`} x1={h.a} x2={h.b} pattern="diagonal" patternColor="color-mix(in oklab, var(--chart-axis) 30%, transparent)" patternScale={1.2} patternStrokeWidth={1.5}
        stroke="transparent" fadeEdgesLength={14} label={h.label} labelColor="var(--chart-axis)" />
    )),
    ...shown.map((l) => kind === 'area'
      ? <Area key={l.key} dataKey={l.key} fill={l.color} stroke={l.color} strokeWidth={1.5} fillOpacity={FILL_TOP[Math.min(lines.indexOf(l), 1)]} curve={curveMonotoneX} showHighlight={false} />
      : <Line key={l.key} dataKey={l.key} stroke={l.color} strokeWidth={1.5} curve={curveMonotoneX} animate={false} fadeEdges={false} showHighlight={false} />),
    out.length > 0 && <OutMarks key="out" marks={out} />,
    <XAxis key="x" tickMode="domain" ticks={xTicks} formatTick={xFormat} animate={false} />,
    <YAxis key="y" ticks={y.ticks} formatValue={yFmt} animate={false} />,
    <ChartTooltip key="tip" showDatePill={false} showDots dotSize={4} dotHaloScale={2} dotHaloOpacity={0.2} damping={0} springConfig={TIP_SPRING}  rows={tipRows} title={tipTitle} />,
  ];
  const common = {
    data: rows, xDataKey: 't', margin, aspectRatio: '', style: { height }, xDomain, yDomain: y.ceil != null ? ([y.floor, y.ceil] as [number, number]) : undefined,
    decimate: false, yDomainTween: false, animationDuration: reduce ? 0 : REVEAL_MS, revealSignature: range,
  };
  return (
    <div className="tc">
      {lines.length > 1 && <ChartLegend lines={lines} hidden={hidden} onToggle={toggle} />}
      {kind === 'area' ? <AreaChart {...common}>{children}</AreaChart> : <LineChart {...common}>{children}</LineChart>}
    </div>
  );
});
