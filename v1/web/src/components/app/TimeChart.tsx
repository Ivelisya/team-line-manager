// 时序小图：细线 + 淡填充（原版详情图的语言）。颜色由调用方给：图表 token --chart-1 / --chart-2 / --chart-3，或首页的指标色 --m-cpu / --m-mem。
// memo：抽屉随实时推送重渲染时，曲线点没变（lines / 格式化函数由调用方放在模块常量里）就不重画
import { memo, useCallback, useMemo, useState } from 'react';
import type { SeriesPoint } from '@/lib/types';
import { pad2 as p2 } from '@/lib/format';
import { ChartContainer, ChartLegend, chartTooltip, Recharts, axisProps, type ChartConfig } from '@/components/ui/chart';

const { AreaChart, Area, LineChart, Line, XAxis, YAxis, CartesianGrid } = Recharts;
export const RANGES = [{ value: '1h', label: '1 小时' }, { value: '6h', label: '6 小时' }, { value: '24h', label: '24 小时' }, { value: '7d', label: '7 天' }, { value: '30d', label: '30 天' }] as const;

/** 按天的标日期；按小时的标时刻，落在零点的标日期（一两天的曲线跨日，不会出现两个一样的 06:00 分不清哪天） */
const xFmt = (range: string) => (t: string | number) => {
  const d = new Date(t);
  const date = `${p2(d.getMonth() + 1)}-${p2(d.getDate())}`;
  return range === '7d' || range === '30d' || (d.getHours() === 0 && d.getMinutes() === 0) ? date : `${p2(d.getHours())}:${p2(d.getMinutes())}`;
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

export interface Line { key: string; label: string; color: string }
/** 悬停点：描边取卡片底色（recharts 默认白描边，暗色下是一圈白边） */
const ACTIVE_DOT = { r: 3.5, strokeWidth: 2, stroke: 'var(--surface)' };
/** yBase：数值是速率（1000 进位）或字节（1024 进位）时给出，纵轴按该进位取整档刻度；百分比等固定范围的用 domain。
 *  empty：每条曲线全为 0（或缺测）时不画图，原处显示这句话（否则纵轴只剩一组 0–4 的空刻度）。
 *  full：横轴铺满所选范围（到现在为止的 1 小时 … 30 天），数据不够时左边留空，不把两天的数据拉满 30 天；不给时按数据首尾 */
export const TimeChart = memo(function TimeChart({ points, lines, range, kind = 'area', yFmt, valueFmt, domain, yBase, height = 180, empty, full }: {
  points: SeriesPoint[] | undefined; lines: Line[]; range: string; kind?: 'area' | 'line'; yFmt: (v: number) => string; valueFmt?: (k: string, v: number) => string;
  domain?: [number | 'auto', number | 'auto']; yBase?: 1000 | 1024; height?: number; empty?: string; full?: boolean;
}) {
  const config = useMemo<ChartConfig>(() => Object.fromEntries(lines.map((l) => [l.key, { label: l.label, color: l.color }])), [lines]);
  // 图例点掉的曲线：不画，纵轴刻度也只按还显示的曲线取；最后一条点不掉（全关了只剩空坐标轴）
  const [hidden, setHidden] = useState<ReadonlySet<string>>(() => new Set());
  const toggle = useCallback((k: string) => setHidden((h) => {
    const n = new Set(h);
    if (!n.delete(k)) { if (n.size >= lines.length - 1) return h; n.add(k); }
    return n;
  }), [lines.length]);
  const data = useMemo(() => (points ?? []).map((p) => ({ ...p, t: new Date(p.ts).getTime() })), [points]);
  // 横轴范围：full 时是 [现在 − 所选范围, 现在]（数据取回时算一次，不随每次重渲染挪动），否则数据首尾
  const span = useMemo<[number, number] | null>(() => {
    if (full && RANGE_MS[range]) { const end = Math.max(Date.now(), data[data.length - 1]?.t ?? 0); return [end - RANGE_MS[range], end]; }
    return data.length > 1 ? [data[0].t, data[data.length - 1].t] : null;
  }, [data, full, range]);
  const xRange = full ? range : span ? spanRange(span[1] - span[0]) : range;
  const xTicks = useMemo(() => (span ? timeTicks(span[0], span[1], xRange) : undefined), [span, xRange]);
  const yTicks = useMemo(() => {
    if (!yBase) return undefined;
    let max = 0;
    for (const d of data) for (const l of lines) { if (hidden.has(l.key)) continue; const v = (d as Record<string, unknown>)[l.key]; if (typeof v === 'number' && v > max) max = v; }
    return valueTicks(max, yBase);
  }, [data, lines, yBase, hidden]);
  const none = useMemo(() => !!empty && data.every((d) => lines.every((l) => !(d as Record<string, unknown>)[l.key])), [data, lines, empty]);
  if (none) return <div className="chart-empty" style={{ height }}>{empty}</div>;
  const common = {
    data, margin: { top: 6, right: 8, bottom: 0, left: 0 },
  };
  const axes = (
    <>
      <CartesianGrid vertical={false} stroke="var(--chart-grid)" />
      <XAxis dataKey="t" type="number" scale="time" domain={span ?? ['dataMin', 'dataMax']} allowDataOverflow ticks={xTicks} tickFormatter={xFmt(xRange)} {...axisProps} minTickGap={32} />
      <YAxis width={60} tickFormatter={yFmt} {...axisProps} interval={0} ticks={yTicks} domain={yTicks ? [0, yTicks[yTicks.length - 1]] : domain ?? [0, 'auto']} />
      {chartTooltip(config, (l) => { const d = new Date(Number(l)); return `${p2(d.getMonth() + 1)}-${p2(d.getDate())} ${p2(d.getHours())}:${p2(d.getMinutes())}`; }, valueFmt)}
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
