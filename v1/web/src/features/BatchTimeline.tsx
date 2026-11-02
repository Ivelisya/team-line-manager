// 续费日程时间轴（客户页与管理端「团队与套餐 → 计费台账」共用）：按续费日分组，每组一行——标题行（每月几号续费 · 地区 · 几个节点 ·
// 本期起止 · 哪天续费、还有几天）、时间条；所有行共用一条横轴（每天一个小刻度、月初写月份）和一条「今天」竖线。
// 只画本期（实际生效的这一期：在收费的组是含今天的那一期，已停止的是最后收费的那一期），起点就是现在生效的起始日；
// 第一次开始收费的日子、续过几次放在时间条的悬停说明里。今天以前实心，今天以后同色虚线；起点实心圆、终点（续费日）空心圆。
// 已停止收费的组（只在管理端出现）整条灰色。给人读的日期一律「11 月 30 日」（不在今年的带年份），见 cnDate。
// 宽度：放得下就铺满，每天至少 MIN_PPD 像素，放不下时整块横向滚动；标题行用 sticky 留在原位不跟着滚（窄时标题与日期分两行）。
import * as React from 'react';
import { addDays } from '@/lib/format';
import { watchSize } from '@/lib/sharedObserver';
import { cn } from '@/lib/utils';
import type { BatchState } from '@/lib/types';
import '@/styles/batches.css';

export interface TimelinePeriod { start_date: string; end_date: string; months: number; count: number }
/** ends：这一组的节点都已停止收费，本期结束后不再续费（只有管理端知道；客户页按 state = today 判断） */
export interface TimelineItem { key: string; seq: number; region: string; state: BatchState; periods: TimelinePeriod[]; ends?: boolean }

/** 本期：起点最晚的一期（服务端给的各期截止到含今天的那一期 / 最后计费的那一期） */
export const currentPeriod = (periods: TimelinePeriod[]) => periods.reduce((m, p) => (p.start_date > m.start_date ? p : m));

const DAY = 86_400_000;
const dayNum = (s: string) => { const [y, m, d] = s.split('-').map(Number); return Date.UTC(y, m - 1, d) / DAY; };
/** 2026-11-30 →「11 月 30 日」；不在 today 那一年的写「2027 年 1 月 1 日」 */
export const cnDate = (s: string, today: string) => `${s.slice(0, 4) !== today.slice(0, 4) ? `${s.slice(0, 4)} 年 ` : ''}${Number(s.slice(5, 7))} 月 ${Number(s.slice(8, 10))} 日`;
/** 一期 [起点, 终点) 写成「11 月 1 日 – 11 月 30 日」：服务到续费日的前一天 */
export const cnSpan = (start: string, end: string, today: string) => `${cnDate(start, today)} – ${cnDate(addDays(end, -1), today)}`;
/** 两个日子差几天 */
export const daysBetween = (from: string, to: string) => dayNum(to) - dayNum(from);
/** 每月几号续费（按第一次开始收费那天的日子；服务端加月份时没有这一天的月份落到月底）：31 号写「每月月底」，29、30 号带一句悬停说明 */
export function renewDay(start: string): { text: string; tip?: string } {
  const d = Number(start.slice(8, 10));
  if (d === 31) return { text: '每月月底' };
  return { text: `每月 ${d} 日`, tip: d >= 29 ? `没有 ${d} 日的月份（如 2 月），在月底那天续费` : undefined };
}
/** 地区：服务端不知道国家时是「未知」，写成「未标地区」，免得像出错 */
export const regionLabel = (r: string) => (r === '未知' ? '未标地区' : r);

const MIN_PPD = 5; // 每天至少几像素：再窄日刻度就糊成一片，改为横向滚动
const PAD_L = 10; const PAD_R = 12;
const AXIS_H = 42; // 顶部横轴：今天标签一行 + 月份一行 + 轴线
const ROW_H = 50; // 一条时间条：上面起点日期、中间条、下面日刻度与终点日期
const BAR_Y = 23; const BAR_H = 8; const TICK_Y = 33;

interface Scale { start: string; days: number; ppd: number; W: number; x: (d: string) => number }

/** 横轴范围：各组本期最早的起点（今天更早就从今天）前 2 天 → 最晚的终点（今天更晚就到今天）后 5 天 */
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

/** 本期的悬停说明：起止 · 个数；续过的再写第一次开始收费的日子与续了几次（个数变过的按期列出） */
function periodTip(b: TimelineItem, p: TimelinePeriod, today: string) {
  const ps = [...b.periods].sort((x, y) => (x.start_date < y.start_date ? -1 : 1));
  const head = `本期 ${cnSpan(p.start_date, p.end_date, today)} · ${p.count} 个节点`;
  if (ps.length < 2) return head;
  const counts = ps.map((x) => x.count);
  const changed = counts.some((c) => c !== counts[0]) ? `\n每个月的节点数：${counts.join(' → ')}` : '';
  return `${head}\n${cnDate(ps[0].start_date, today)} 开始收费，已续费 ${ps.length - 1} 次${changed}`;
}

/** 这一组还续不续：已停止 / 本期结束后不再续费 / 照常续费 */
const phase = (b: TimelineItem) => (b.state === 'expired' ? 'stopped' : b.ends || b.state === 'today' ? 'ending' : 'renew');

function Track({ b, s, today, ticks }: { b: TimelineItem; s: Scale; today: string; ticks: string }) {
  const t = s.x(today);
  const p = currentPeriod(b.periods);
  const a = s.x(p.start_date); const z = s.x(p.end_date);
  const solid = Math.min(z, t); const dash = Math.max(a, t);
  return (
    <svg className="btl-track" width={s.W} height={ROW_H} role="img" aria-label={`${regionLabel(b.region)}：本期 ${cnSpan(p.start_date, p.end_date, today)}，${cnDate(p.end_date, today)}${phase(b) === 'renew' ? '续费' : '到期'}`}>
      <path d={ticks} className="btl-ticks" />
      {/* 今天竖线只画在时间条这一段：贯穿整列会从标题文字上穿过 */}
      <line className="btl-today" x1={t} x2={t} y1={0} y2={ROW_H} />
      <g className="is-a" data-tip={periodTip(b, p, today)}>
        <rect className="btl-hit" x={a} y={BAR_Y - 9} width={Math.max(0, z - a)} height={18} />
        {solid > a && <rect className="btl-solid" x={a} y={BAR_Y - BAR_H / 2} width={solid - a} height={BAR_H} />}
        {z > dash && <line className="btl-future" x1={dash} x2={z} y1={BAR_Y} y2={BAR_Y} />}
      </g>
      <circle className="btl-start is-a" cx={a} cy={BAR_Y} r={5} />
      <circle className="btl-end is-a" cx={z} cy={BAR_Y} r={4.5} />
      <text className="btl-date" x={Math.max(2, a - 4)} y={11}>{cnDate(p.start_date, today)}</text>
      <text className="btl-date" x={Math.min(s.W - 2, z + 4)} y={ROW_H - 3} textAnchor="end">{cnDate(p.end_date, today)}</text>
    </svg>
  );
}

/** 标题右侧：哪天续费、还有几天；不再续费的写还有几天到期；已停止的不写（标题里已写） */
function Due({ b, today }: { b: TimelineItem; today: string }) {
  const ph = phase(b);
  if (ph === 'stopped') return null;
  if (b.state === 'today') return <span className="btl-flag">今天到期</span>;
  const end = currentPeriod(b.periods).end_date;
  const n = daysBetween(today, end);
  if (ph === 'ending') return <>还有 <b className="num">{n}</b> 天到期</>;
  return <><b className="num">{cnDate(end, today)}</b>续费 <span className="t-sub">· 还有</span> <b className="num">{n}</b> 天</>;
}

function Axis({ s, today }: { s: Scale; today: string }) {
  const months = monthStarts(s.start, s.days);
  const t = s.x(today);
  const year = today.slice(0, 4);
  const pillW = 98;
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
            <text className="btl-month" x={x + 4} y={AXIS_H - 4}>{y !== year ? `${y} 年 ${Number(m)} 月` : `${Number(m)} 月`}</text>
          </g>
        );
      })}
      <g className="btl-now">
        <rect x={px} y={1} width={pillW} height={18} rx={9} />
        <text x={px + pillW / 2} y={14} textAnchor="middle">今天 {cnDate(today, today)}</text>
      </g>
    </svg>
  );
}

/** 图例：实心条「已过去」、虚线「还没到」、空心圆「续费日」；expired：再加灰色「已停止收费」 */
export function TimelineLegend({ expired, className }: { expired?: boolean; className?: string }) {
  return (
    <div className={cn('btl btl-legend', className)}>
      <span><svg width="22" height="10" aria-hidden><rect className="btl-solid is-a" x="1" y="1" width="20" height="8" rx="2" /></svg>已过去</span>
      <span><svg width="22" height="10" aria-hidden><line className="btl-future is-a" x1="1" x2="21" y1="5" y2="5" /></svg>还没到</span>
      <span><svg width="12" height="12" aria-hidden><circle className="btl-end is-a" cx="6" cy="6" r="4" /></svg>续费日</span>
      {expired && <span data-tone="x"><svg width="22" height="10" aria-hidden><rect className="btl-solid is-a" x="1" y="1" width="20" height="8" rx="2" /></svg>已停止收费</span>}
    </div>
  );
}

/** 一行的标题：「每月 1 日续费 · 美区 · 9 个节点」；不再续费、已停止的换掉开头。showSeq：管理端在后面小字写「第01批」，改日子时分得清是哪一组 */
function Title({ b, showSeq }: { b: TimelineItem; showSeq: boolean }) {
  const p = currentPeriod(b.periods);
  const ph = phase(b);
  const r = renewDay(b.periods.reduce((m, x) => (x.start_date < m ? x.start_date : m), p.start_date));
  return (
    <span className="btl-title">
      {ph === 'stopped' ? '已停止收费' : ph === 'ending' ? '不再续费' : <span data-tip={r.tip}>{r.text}续费</span>} · {regionLabel(b.region)} · <span className="num">{p.count}</span> 个节点
      {showSeq && <span className="btl-seq num">第{String(b.seq).padStart(2, '0')}批</span>}
    </span>
  );
}

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
          {items.map((b) => {
            const p = currentPeriod(b.periods);
            return (
              <div key={b.key} className="btl-row" data-state={b.state}>
                <div className="btl-head">
                  <div className={cn('btl-line', actions && 'has-act')}>
                    <Title b={b} showSeq={!!actions} />
                    <span className="btl-meta">
                      <span className="btl-dur"><span className="t-sub">{b.state === 'expired' ? '最后一期' : '本期'}</span> <span className="num">{cnSpan(p.start_date, p.end_date, today)}</span></span>
                      <span className="btl-nm"><Due b={b} today={today} /></span>
                    </span>
                    {actions && <span className="btl-act">{actions(b)}</span>}
                  </div>
                </div>
                <Track b={b} s={s} today={today} ticks={ticks} />
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
