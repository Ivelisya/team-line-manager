// 时序小图：细线 + 淡填充（原版详情图的语言）。颜色由调用方给：图表 token --chart-1 / --chart-2 / --chart-3，或首页的指标色 --m-cpu / --m-mem。
// memo：抽屉随实时推送重渲染时，曲线点没变（lines / 格式化函数由调用方放在模块常量里）就不重画
import { memo, useCallback, useId, useMemo, useState } from 'react';
import type { SeriesPoint } from '@/lib/types';
import { dt, hm } from '@/lib/format';
import { ChartContainer, ChartLegend, chartTooltip, Recharts, axisProps, type ChartConfig } from '@/components/ui/chart';

const { AreaChart, Area, LineChart, Line, XAxis, YAxis, CartesianGrid, ReferenceArea, ReferenceDot } = Recharts;
export const RANGES = [{ value: '1h', label: '1 小时' }, { value: '6h', label: '6 小时' }, { value: '24h', label: '24 小时' }, { value: '7d', label: '7 天' }, { value: '30d', label: '30 天' }] as const;

/** 按天的标日期；按小时的标时刻，落在零点的标日期（一两天的曲线跨日，不会出现两个一样的 06:00 分不清哪天） */
const xFmt = (range: string) => (t: string | number) => {
  const d = new Date(t);
  return range === '7d' || range === '30d' || (d.getHours() === 0 && d.getMinutes() === 0) ? dt(t).slice(0, 5) : hm(t);
};

const MIN = 60_000; const HOUR = 60 * MIN; const DAY = 24 * HOUR;
const RANGE_MS: Record<string, number> = { '1h': HOUR, '6h': 6 * HOUR, '24h': DAY, '7d': 7 * DAY, '30d': 30 * DAY };
const TICK_STEP: Record<string, number> = { '1h': 10 * MIN, '6h': HOUR, '24h': 3 * HOUR, '7d': DAY, '30d': 5 * DAY };
/** 刻度步长与格式按数据实际跨度取，不按所选范围：刚上架或早已离线的机器看「7 天」，数据只有一天多，按 7 天只标日期会标出一排相同的 10-20 */
const spanRange = (span: number) => (span <= HOUR ? '1h' : span <= 6 * HOUR ? '6h' : span <= 2 * DAY ? '24h' : span <= 7 * DAY ? '7d' : '30d');
/** 横轴刻度落在本地时间的整十分 / 整点 / 零点上（recharts 自己取的是 13:46、16:40 这种） */
function timeTicks(min: number, max: number, range: string): number[] | undefined {
  const step = TICK_STEP[range];
  if (!step || !(max > min)) return undefined;
  const d = new Date(min); d.setHours(0, 0, 0, 0);
  const out: number[] = [];
  for (let t = d.getTime() + Math.ceil((min - d.getTime()) / step) * step; t <= max; t += step) out.push(t);
  return out.length >= 2 ? out : undefined; // 数据比一个步长还短（刚上架的机器看 7 天）：交给 recharts 自己取
}

/** 纵轴刻度：从 0 起 4 段。速率（1000 进位）每段取 1/2/2.5/5×10^k，字节（1024 进位）每段取 2 的幂，
 *  读出来是 250 Mbps、256 MiB 这样的整数，不会出现按十进制取整再换算成 MiB 的 143M、286M */
function valueTicks(max: number, base: 1000 | 1024): number[] | undefined {
  if (!(max > 0)) return undefined;
  const unit = base ** Math.max(0, Math.floor(Math.log(max) / Math.log(base)));
  const raw = max / unit / 4;
  let step: number;
  if (base === 1024) step = 2 ** Math.ceil(Math.log2(raw));
  else { const e = 10 ** Math.floor(Math.log10(raw)); step = ([1, 2, 2.5, 5, 10].find((n) => n * e >= raw) ?? 10) * e; }
  if (unit === 1) step = Math.max(1, step); // 最小单位（B / bps）不分小数格，否则标签取整后重复
  return [0, 1, 2, 3, 4].map((i) => i * step * unit);
}

/** 没有数据的时段（所选范围左边还没上架、中间离线、右边停了上报）每隔一段垫一个空点（只有时间）：悬停那里时没有提示
 *  （recharts 滤掉空值后提示是空的），不会吸到很远的那个真实点上；曲线也在空段断开，不把两头连成一条斜线。
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
/** 纵轴上限取整档（1 / 1.5 / 2 / 2.5 / 3 / 4 / 5 / 6 / 8 / 10 × 10^k），不指定进位的图（延迟 ms）用 */
function niceCeil(v: number): number {
  const e = 10 ** Math.floor(Math.log10(v));
  return ([1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10].find((n) => n * e >= v) ?? 10) * e;
}
/** 超出纵轴上限的点画在顶边，用小三角标出 */
const OUT_TRI = (color: string) => ({ cx, cy }: { cx?: number; cy?: number }) => (cx == null || cy == null ? <g /> : <path d={`M${cx - 4} ${cy + 6}L${cx + 4} ${cy + 6}L${cx} ${cy}Z`} fill={color} />);
const MAX_OUT = 40;
/** 整数刻度（延迟 ms）：从 0 起 4 段，每段取整档的整数（不出现 0.85 / 1.7 / 2.55 ms） */
const intTicks = (max: number) => { const step = Math.max(1, niceCeil(max / 4)); return [0, 1, 2, 3, 4].map((i) => i * step); };

/** 百分比刻度（没有固定上限的百分比，如负载）：从 0 起 4 段，每段取 5 / 10 / 20 / 25 / 50 / 100 … 这样的整档，不出现 7% / 14% / 21% */
const pctTicks = (max: number) => { const step = [5, 10, 20, 25, 50, 100, 200, 500, 1000, 2000, 5000].find((s) => s * 4 >= max) ?? Math.ceil(max / 4); return [0, 1, 2, 3, 4].map((i) => i * step); };

export interface Line { key: string; label: string; color: string }
/** 悬停点：描边取卡片底色（recharts 默认白描边，暗色下是一圈白边） */
const ACTIVE_DOT = { r: 3.5, strokeWidth: 2, stroke: 'var(--surface)' };
/** yBase：数值是速率（1000 进位）或字节（1024 进位）时给出，纵轴按该进位取整档刻度；百分比等固定范围的用 domain。
 *  empty：每条曲线全为 0（或缺测）时不画图，原处显示这句话（否则纵轴只剩一组 0–4 的空刻度）。
 *  full：横轴取所选范围（到现在为止的 1 小时 … 30 天）；数据从范围中间才开始时，横轴从第一条数据画起，
 *  范围里更早的那一段压成左边一条窄窄的斜纹（暂无数据），不把两天的数据挤在 30 天的右边一小条。
 *  数据中间或末尾的空档（离线、停止上报）同样画斜纹，holeLabel 写那是什么（默认「暂无数据」，主机页签是「agent 离线」）。
 *  clip：纵轴按 p99（至少让最大的一个点可截）截断——少数尖峰不再把整张图压扁；超出的点画在顶边并标小三角（悬停仍是真实值） */
export const TimeChart = memo(function TimeChart({ points, lines, range, kind = 'area', yFmt, valueFmt, domain, yBase, pct, height = 180, empty, full, clip, holeLabel = '暂无数据' }: {
  points: SeriesPoint[] | undefined; lines: Line[]; range: string; kind?: 'area' | 'line'; yFmt: (v: number) => string; valueFmt?: (k: string, v: number) => string;
  domain?: [number | 'auto', number | 'auto']; yBase?: 1000 | 1024; pct?: boolean; height?: number; empty?: string; full?: boolean; clip?: boolean; holeLabel?: string;
}) {
  const config = useMemo<ChartConfig>(() => Object.fromEntries(lines.map((l) => [l.key, { label: l.label, color: l.color }])), [lines]);
  const hatch = useId().replace(/:/g, '');
  // 图例点掉的曲线：不画，纵轴刻度也只按还显示的曲线取；最后一条点不掉（全关了只剩空坐标轴）
  const [hidden, setHidden] = useState<ReadonlySet<string>>(() => new Set());
  const toggle = useCallback((k: string) => setHidden((h) => {
    const n = new Set(h);
    if (!n.delete(k)) { if (n.size >= lines.length - 1) return h; n.add(k); }
    return n;
  }), [lines.length]);
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
  const xTicks = useMemo(() => (span ? timeTicks(span[0] + lead, span[1], xRange) : undefined), [span, lead, xRange]);
  // 纵轴：clip 时按 p99 截断（最大值不到 p99 的 1.3 倍，没有值得截的尖峰，照常按最大值）
  const y = useMemo(() => {
    const vals: number[] = [];
    for (const d of data) for (const l of lines) { if (hidden.has(l.key)) continue; const v = (d as Record<string, unknown>)[l.key]; if (typeof v === 'number' && v > 0) vals.push(v); }
    const max = vals.length ? Math.max(...vals) : 0;
    // 上限取 p99（先去掉最大的一个点再取：点少时 p99 就是最大值，一个尖峰也得能截）
    const p99 = vals.length > 1 ? quantile([...vals].sort((a, b) => a - b).slice(0, -1), 0.99) : vals[0] ?? 0;
    const cut = !!clip && p99 > 0 && max > p99 * 1.3;
    const top = cut ? p99 : max;
    const ticks = yBase ? valueTicks(top, yBase) : pct && top > 0 ? pctTicks(top) : clip && top > 0 ? intTicks(top) : undefined;
    const ceil = ticks ? ticks[ticks.length - 1] : cut ? niceCeil(top) : undefined;
    const out: { t: number; v: number; key: string }[] = [];
    if (cut && ceil != null) for (const d of data) for (const l of lines) { if (hidden.has(l.key)) continue; const v = (d as Record<string, unknown>)[l.key]; if (typeof v === 'number' && v > ceil) out.push({ t: (d as { t: number }).t, v, key: l.key }); }
    return { ticks, ceil, out: out.slice(0, MAX_OUT) };
  }, [data, lines, yBase, pct, hidden, clip]);
  const none = useMemo(() => !!empty && data.every((d) => lines.every((l) => !(d as Record<string, unknown>)[l.key])), [data, lines, empty]);
  if (none) return <div className="chart-empty" style={{ height }}>{empty}</div>;
  const common = {
    data, margin: { top: 6, right: 8, bottom: 0, left: 0 },
  };
  const wide = (a: number, b: number) => span != null && (b - a) / (span[1] - span[0]) > 0.1; // 斜纹宽到放得下字才写
  const axes = (
    <>
      <defs>
        <pattern id={hatch} width="7" height="7" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><line x1="0" y1="0" x2="0" y2="7" stroke="var(--chart-axis)" strokeOpacity="0.28" strokeWidth="2" /></pattern>
      </defs>
      <CartesianGrid vertical={false} stroke="var(--chart-grid)" />
      {holes.map((h) => <ReferenceArea key={h.a} x1={h.a} x2={h.b} fill={`url(#${hatch})`} fillOpacity={1} stroke="none" ifOverflow="hidden" label={wide(h.a, h.b) ? { value: h.label, position: 'center', fill: 'var(--chart-axis)', fontSize: 11 } : undefined} />)}
      <XAxis dataKey="t" type="number" scale="time" domain={span ?? ['dataMin', 'dataMax']} allowDataOverflow ticks={xTicks} tickFormatter={xFmt(xRange)} {...axisProps} minTickGap={32} />
      <YAxis width={60} tickFormatter={yFmt} {...axisProps} interval={0} ticks={y.ticks} allowDataOverflow={y.out.length > 0} domain={y.ceil != null ? [0, y.ceil] : domain ?? [0, 'auto']} />
      {chartTooltip(config, (l) => dt(Number(l)), valueFmt)}
      {y.out.map((o) => <ReferenceDot key={`${o.key}${o.t}`} x={o.t} y={y.ceil!} r={0} shape={OUT_TRI(`var(--color-${o.key})`)} ifOverflow="visible" />)}
    </>
  );
  return (
    <div style={{ display: 'grid', gap: 6 }}>
      {lines.length > 1 && <ChartLegend config={config} hidden={hidden} onToggle={toggle} />}
      <ChartContainer config={config} height={height}>
        {kind === 'area' ? (
          <AreaChart {...common}>
            {axes}
            {lines.map((l, i) => (
              <Area key={l.key} dataKey={l.key} type="monotone" stroke={`var(--color-${l.key})`} strokeWidth={1.5} fill={`var(--color-${l.key})`} fillOpacity={i === 0 ? 0.1 : 0.04} hide={hidden.has(l.key)} dot={false} activeDot={ACTIVE_DOT} isAnimationActive={false} />
            ))}
          </AreaChart>
        ) : (
          <LineChart {...common}>
            {axes}
            {lines.map((l) => <Line key={l.key} dataKey={l.key} type="monotone" stroke={`var(--color-${l.key})`} strokeWidth={1.5} hide={hidden.has(l.key)} dot={false} activeDot={ACTIVE_DOT} isAnimationActive={false} />)}
          </LineChart>
        )}
      </ChartContainer>
    </div>
  );
});
