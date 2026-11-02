// 客户批次时间轴（客户到期页与管理端「团队与套餐 → 计费台账」共用）：每个批次一行——标题行（批次名 · 本期起止 · 还剩几天）、时间条；
// 所有行共用一条横轴（每天一个小刻度、月初写月份）和一条「今天」竖线。
// 只画本期（实际生效的这一期：有效批次是含今天的那一期，已到期的是最后计费的那一期），起点就是现在生效的起始日；
// 首次起算日、续过几次放在时间条的悬停说明里。今天以前实心，今天以后同色虚线；起点实心圆、终点空心圆；起止日期写在条的左上 / 右下。
// 已到期的批次（只在管理端出现）整条灰色。
// 宽度：放得下就铺满，每天至少 MIN_PPD 像素，放不下时整块横向滚动；标题行用 sticky 留在原位不跟着滚（手机上标题照常换行）。
import * as React from 'react';
import { addDays } from '@/lib/format';
import { watchSize } from '@/lib/sharedObserver';
import { cn } from '@/lib/utils';
import type { BatchState } from '@/lib/types';
import '@/styles/batches.css';

export interface TimelinePeriod { start_date: string; end_date: string; months: number; count: number }
export interface TimelineItem { key: string; title: string; state: BatchState; periods: TimelinePeriod[] }

/** 本期：起点最晚的一期（服务端给的各期截止到含今天的那一期 / 最后计费的那一期） */
export const currentPeriod = (periods: TimelinePeriod[]) => periods.reduce((m, p) => (p.start_date > m.start_date ? p : m));

const DAY = 86_400_000;
const dayNum = (s: string) => { const [y, m, d] = s.split('-').map(Number); return Date.UTC(y, m - 1, d) / DAY; };
/** 2026-10-21 → 10/21 */
export const mmdd = (s: string) => `${s.slice(5, 7)}/${s.slice(8, 10)}`;

const MIN_PPD = 5; // 每天至少几像素：再窄日刻度就糊成一片，改为横向滚动
const PAD_L = 10; const PAD_R = 12;
const AXIS_H = 42; // 顶部横轴：今天标签一行 + 月份一行 + 轴线
const ROW_H = 50; // 一条时间条：上面起点日期、中间条、下面日刻度与终点日期
const BAR_Y = 23; const BAR_H = 8; const TICK_Y = 33;

interface Scale { start: string; days: number; ppd: number; W: number; x: (d: string) => number }

/** 横轴范围：各批本期最早的起点（今天更早就从今天）前 2 天 → 最晚的终点（今天更晚就到今天）后 5 天 */
function rangeOf(items: TimelineItem[], today: string) {
  let lo = today; let hi = today;
  for (const b of items) { const p = currentPeriod(b.periods); if (p.start_date < lo) lo = p.start_date; if (p.end_date > hi) hi = p.end_date; }
  const start = addDays(lo, -2);
  return { start, days: dayNum(addDays(hi, 5)) - dayNum(start) };
}

/** 每个月 1 日（不含横轴起点之前的） */
function monthStarts(start: string, days: number): string[] {
  const out: string[] = [];
  let [y, m] = start.split('-').map(Number);
  const end = addDays(start, days);
  for (;;) {
    const d = `${y}-${String(m).padStart(2, '0')}-01`;
    if (d > end) break;
    if (d >= start) out.push(d);
    m += 1; if (m > 12) { m = 1; y += 1; }
  }
  return out;
}

/** 一行的日刻度：一条底线，每天一个短刻度，月初长一些 */
function ticksPath(s: Scale): string {
  const x0 = PAD_L; const x1 = PAD_L + s.days * s.ppd;
  let d = `M${x0} ${TICK_Y}H${x1}`;
  const d0 = dayNum(s.start);
  for (let i = 0; i <= s.days; i++) {
    const day = new Date((d0 + i) * DAY).getUTCDate();
    const x = (PAD_L + i * s.ppd).toFixed(1);
    d += `M${x} ${TICK_Y}v${day === 1 ? 6 : 3}`;
  }
  return d;
}

/** 本期的悬停说明：起止 · 个数；续过的再写首次起算日与续了几次（个数变过的按期列出） */
function periodTip(b: TimelineItem, p: TimelinePeriod) {
  const ps = [...b.periods].sort((x, y) => (x.start_date < y.start_date ? -1 : 1));
  const head = `本期 ${mmdd(p.start_date)} → ${mmdd(p.end_date)} · ${p.count} 个节点`;
  if (ps.length < 2) return head;
  const counts = ps.map((x) => x.count);
  const changed = counts.some((c) => c !== counts[0]) ? `
各期个数：${counts.join(' → ')}` : '';
  return `${head}
${mmdd(ps[0].start_date)} 首次起算，已续 ${ps.length - 1} 次${changed}`;
}

function Track({ b, s, today, ticks }: { b: TimelineItem; s: Scale; today: string; ticks: string }) {
  const t = s.x(today);
  const p = currentPeriod(b.periods);
  const a = s.x(p.start_date); const z = s.x(p.end_date);
  const solid = Math.min(z, t); const dash = Math.max(a, t);
  return (
    <svg className="btl-track" width={s.W} height={ROW_H} role="img" aria-label={`${b.title}：本期 ${p.start_date} 起，${p.end_date} 到期`}>
      <path d={ticks} className="btl-ticks" />
      {/* 今天竖线只画在时间条这一段：贯穿整列会从标题文字上穿过 */}
      <line className="btl-today" x1={t} x2={t} y1={0} y2={ROW_H} />
      <g className="is-a" data-tip={periodTip(b, p)}>
        <rect className="btl-hit" x={a} y={BAR_Y - 9} width={Math.max(0, z - a)} height={18} />
        {solid > a && <rect className="btl-solid" x={a} y={BAR_Y - BAR_H / 2} width={solid - a} height={BAR_H} />}
        {z > dash && <line className="btl-future" x1={dash} x2={z} y1={BAR_Y} y2={BAR_Y} />}
      </g>
      <circle className="btl-start is-a" cx={a} cy={BAR_Y} r={5} />
      <circle className="btl-end is-a" cx={z} cy={BAR_Y} r={4.5} />
      <text className="btl-date" x={Math.max(2, a - 4)} y={11}>{mmdd(p.start_date)}</text>
      <text className="btl-date" x={Math.min(s.W - 2, z + 4)} y={ROW_H - 3} textAnchor="end">{mmdd(p.end_date)}</text>
    </svg>
  );
}

/** 本期还剩几天：终点（下次续费 / 到期日）减今天；今天到期另写 */
function Left({ b, today }: { b: TimelineItem; today: string }) {
  if (b.state === 'expired') return null; // 标题里已写「（已到期）」
  if (b.state === 'today') return <span className="btl-flag">今日到期</span>;
  const n = dayNum(currentPeriod(b.periods).end_date) - dayNum(today);
  return <><span className="t-sub">还剩</span> <b className="num">{n}</b> 天</>;
}

function Axis({ s, today }: { s: Scale; today: string }) {
  const months = monthStarts(s.start, s.days);
  const t = s.x(today);
  const year = today.slice(0, 4);
  const pillW = 72;
  const px = Math.min(Math.max(t - pillW / 2, 0), s.W - pillW);
  return (
    <svg className="btl-axis" width={s.W} height={AXIS_H} aria-hidden>
      <path className="btl-axis-line" d={`M${PAD_L} ${AXIS_H - 1}H${PAD_L + s.days * s.ppd}`} />
      {months.map((d) => {
        const x = s.x(d);
        const [y, m] = d.split('-');
        // 跨年时不在今天这一年的月份写上年份
        return (
          <g key={d}>
            <path className="btl-axis-tick" d={`M${x} ${AXIS_H - 14}V${AXIS_H - 1}`} />
            <text className="btl-month" x={x + 4} y={AXIS_H - 4}>{y !== year ? `${y}年${Number(m)}月` : `${Number(m)}月`}</text>
          </g>
        );
      })}
      <g className="btl-now">
        <rect x={px} y={1} width={pillW} height={18} rx={9} />
        <text x={px + pillW / 2} y={14} textAnchor="middle">{mmdd(today)} 今天</text>
      </g>
    </svg>
  );
}

/** 图例：实心条「已经生效的部分」、虚线「未来的部分」；expired：再加灰色「已到期」 */
export function TimelineLegend({ expired, className }: { expired?: boolean; className?: string }) {
  return (
    <div className={cn('btl btl-legend', className)}>
      <span><svg width="22" height="10" aria-hidden><rect className="btl-solid is-a" x="1" y="1" width="20" height="8" rx="2" /></svg>已经生效的部分</span>
      <span><svg width="22" height="10" aria-hidden><line className="btl-future is-a" x1="1" x2="21" y1="5" y2="5" /></svg>未来的部分</span>
      {expired && <span data-tone="x"><svg width="22" height="10" aria-hidden><rect className="btl-solid is-a" x="1" y="1" width="20" height="8" rx="2" /></svg>已到期</span>}
    </div>
  );
}

const period = (b: TimelineItem) => { const p = currentPeriod(b.periods); return `${mmdd(p.start_date)} → ${mmdd(p.end_date)}`; };

/** 时间轴本体。actions：管理端每行末尾的操作（「⋯」菜单） */
export function BatchTimeline({ items, today, actions }: { items: TimelineItem[]; today: string; actions?: (b: TimelineItem) => React.ReactNode }) {
  const ref = React.useRef<HTMLDivElement>(null);
  const [vw, setVw] = React.useState(0);
  React.useLayoutEffect(() => {
    const el = ref.current!;
    // 取实际宽度向下取整：clientWidth 是四舍五入的，宽度带小数时会比可用宽度多出零点几像素，冒出一条横向滚动条
    const measure = () => setVw(Math.floor(el.getBoundingClientRect().width));
    measure();
    return watchSize(el, measure);
  }, []);

  const { start, days } = React.useMemo(() => rangeOf(items, today), [items, today]);
  const s = React.useMemo<Scale | null>(() => {
    if (!vw) return null;
    const ppd = Math.max(MIN_PPD, (vw - PAD_L - PAD_R) / days);
    const d0 = dayNum(start);
    // 铺满时宽度就是可见宽度（向上取整会多出 1px，出现一条没必要的横向滚动条）
    const W = ppd > MIN_PPD ? vw : Math.ceil(PAD_L + days * ppd + PAD_R);
    return { start, days, ppd, W, x: (d) => PAD_L + (dayNum(d) - d0) * ppd };
  }, [vw, start, days]);
  const ticks = React.useMemo(() => (s ? ticksPath(s) : ''), [s]);

  // 放不下要横滑时（手机），一打开就把「今天」滚到视野里偏右的位置：先看到快到期的部分。只在第一次量好宽度时滚
  const scrolled = React.useRef(false);
  React.useLayoutEffect(() => {
    const el = ref.current;
    if (!s || !el || scrolled.current) return;
    scrolled.current = true;
    if (s.W > vw + 1) el.scrollLeft = Math.max(0, s.x(today) - vw * 0.65);
  }, [s, vw, today]);

  return (
    <div className="btl btl-scroll" ref={ref}>
      {s && (
        <div className="btl-canvas" style={{ width: s.W, '--btl-vw': `${vw}px` } as React.CSSProperties}>
          <Axis s={s} today={today} />
          {items.map((b) => (
            <div key={b.key} className="btl-row" data-state={b.state}>
              <div className="btl-head">
                <div className={cn('btl-line', actions && 'has-act')}>
                  <span className="btl-title">{b.title}</span>
                  <span className="btl-dur"><span className="t-sub">本期</span> <b className="num">{period(b)}</b></span>
                  <span className="btl-nm"><Left b={b} today={today} /></span>
                  {actions && <span className="btl-act">{actions(b)}</span>}
                </div>
              </div>
              <Track b={b} s={s} today={today} ticks={ticks} />
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
