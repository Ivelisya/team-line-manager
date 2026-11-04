// 计费台账（管理端「团队 → 台账」）与客户页 /c/:token 共用的日期、月份与状态展示。
// 口径在服务端（batches.rs）：每个节点从开始收费日起每满一个月收一次（一期），按月看 = 每个节点起点在这个月的那一期。
import type { ReactNode } from 'react';
import type { BillingMonthNode } from '@/lib/types';
import { ArrowDownRight, ChevronLeft, ChevronRight } from 'lucide-react';
import { IconButton } from '@/components/ui/button';
import '@/styles/billing.css';

/** 2026-11-02 →「11 月 02 日」；跨年写「2027 年 01 月 01 日」，月份与日期均补齐两位。 */
export const cnDate = (s: string, today: string) => `${s.slice(0, 4) !== today.slice(0, 4) ? `${s.slice(0, 4)} 年 ` : ''}${s.slice(5, 7)} 月 ${s.slice(8, 10)} 日`;
/** 一期的日期区间：终点是到期日，下一期从这天起。 */
export const cnSpan = (start: string, end: string, today: string) => `${cnDate(start, today)} – ${cnDate(end, today)}`;
/** 2026-11 →「11 月」；不在 today 那一年的写「2027 年 1 月」 */
export const cnMonth = (m: string, today: string) => `${m.slice(0, 4) !== today.slice(0, 4) ? `${m.slice(0, 4)} 年 ` : ''}${Number(m.slice(5, 7))} 月`;
/** 地区：服务端不知道国家时是「未知」，写成「未标地区」，免得像出错 */
export const regionLabel = (r: string) => (r === '未知' ? '未标地区' : r);

export function OutstandingHeading() {
  return <div className="bm-outstanding-title"><h2 className="panel-title">待支付</h2><p className="bm-billing-basis">按照节点生效日期收费</p></div>;
}

/** 月份切换「‹ 2026 年 11 月 ›」：只在 months（服务端给的可以翻看的月份）里翻 */
export function MonthSwitch({ month, months, onChange }: { month: string; months: string[]; onChange: (m: string) => void }) {
  const i = months.indexOf(month);
  return (
    <span className="bm-switch">
      <IconButton size="sm" label="上个月" disabled={i <= 0} onClick={() => onChange(months[i - 1])}><ChevronLeft size={16} /></IconButton>
      <span className="num bm-switch-label" aria-live="polite">{`${month.slice(0, 4)} 年 ${Number(month.slice(5, 7))} 月`}</span>
      <IconButton size="sm" label="下个月" disabled={i < 0 || i >= months.length - 1} onClick={() => onChange(months[i + 1])}><ChevronRight size={16} /></IconButton>
    </span>
  );
}

export function MonthSummary({ counts, selected, today, onSelect }: {
  counts: Map<string, number>; selected?: string; today: string; onSelect: (month: string) => void;
}) {
  return <ul className="bm-month-counts">
    {[...counts].sort(([a], [b]) => a.localeCompare(b)).map(([month, count]) => {
      const label = cnMonth(month, today);
      return <li key={month}><button type="button" className="bm-month-choice"
        aria-current={month === selected ? 'true' : undefined}
        aria-label={`查看${label}明细，${count} 个节点待支付`} onClick={() => onSelect(month)}>
        <span className="bm-choice-date">{label}<ArrowDownRight size={14} aria-hidden="true" /></span>
        <span className="bm-choice-count"><strong>{count}</strong> 个节点</span>
      </button></li>;
    })}
  </ul>;
}

const PERIOD_LABEL = { active: '生效中', expired: '已到期', upcoming: '待续费' };

export function PeriodLegend({ includeUpcoming = true }: { includeUpcoming?: boolean }) {
  return <div className="bm-legend" aria-label="本期状态">
    {Object.entries(PERIOD_LABEL).filter(([state]) => includeUpcoming || state !== 'upcoming').map(([state, label]) => <span key={state}><i className="bm-dot" data-period={state} aria-hidden="true" />{label}</span>)}
  </div>;
}

type RowNode = Pick<BillingMonthNode, 'name' | 'region' | 'period_start' | 'period_end' | 'manual' | 'ended'>;

export function BillingNodeRow({ n, today, side, nameContent }: {
  n: RowNode; today: string; side: ReactNode; nameContent?: ReactNode;
}) {
  const period = n.period_start > today ? 'upcoming' : today >= n.period_end ? 'expired' : 'active';
  const label = PERIOD_LABEL[period];
  const remaining = period === 'active' ? Math.round((Date.parse(n.period_end) - Date.parse(today)) / 86_400_000) : null;
  return <li className="bm-row">
    <div className="bm-main">
      <span className="bm-name">
        <span className="bm-dot" data-period={period} role="img" aria-label={label} data-tip={label} />
        {nameContent ?? n.name}
        {n.manual && <span className="bm-tag">手动添加</span>}
        {n.ended && <span className="bm-tag">已停止收费</span>}
      </span>
      <span className="bm-sub">{regionLabel(n.region)} · <span className="num">{cnSpan(n.period_start, n.period_end, today)}</span>{remaining !== null && <> · <span className="num">还有 {remaining} 天到期</span></>}</span>
    </div>
    {side != null && <span className="bm-side">{side}</span>}
  </li>;
}
