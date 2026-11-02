// 计费台账（管理端「团队与套餐 → 计费台账」）与客户页 /c/:token 共用的小件：中文日期、这一期的区间、月份切换。
// 口径在服务端（batches.rs）：每个节点从开始收费日起每满一个月收一次（一期），按月看 = 每个节点起点在这个月的那一期。
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { IconButton } from '@/components/ui/button';
import '@/styles/billing.css';

/** 2026-11-30 →「11 月 30 日」；不在 today 那一年的写「2027 年 1 月 1 日」 */
export const cnDate = (s: string, today: string) => `${s.slice(0, 4) !== today.slice(0, 4) ? `${s.slice(0, 4)} 年 ` : ''}${Number(s.slice(5, 7))} 月 ${Number(s.slice(8, 10))} 日`;
/** 一期写成「10 月 30 日 – 11 月 30 日到期」：起点是购买（开始收费）那天，终点是到期日（同供应商写的到期日，下一期从这天起） */
export const cnSpan = (start: string, end: string, today: string) => `${cnDate(start, today)} – ${cnDate(end, today)}到期`;
/** 2026-11 →「11 月」；不在 today 那一年的写「2027 年 1 月」 */
export const cnMonth = (m: string, today: string) => `${m.slice(0, 4) !== today.slice(0, 4) ? `${m.slice(0, 4)} 年 ` : ''}${Number(m.slice(5, 7))} 月`;
/** 地区：服务端不知道国家时是「未知」，写成「未标地区」，免得像出错 */
export const regionLabel = (r: string) => (r === '未知' ? '未标地区' : r);

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
