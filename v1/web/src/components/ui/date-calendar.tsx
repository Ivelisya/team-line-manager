// 日期选择的弹层月历（date-field.tsx 懒加载）：react-day-picker + date-fns + 中文 locale 约 85KB，只在第一次打开日期弹层（或指到触发器上）时才下载，
// 抽屉、上架页打开时不背这份体积。星期与月份文字、按钮的读屏文字在这里给。
import * as React from 'react';
import { zhCN } from 'react-day-picker/locale/zh-CN';
import { pad2 } from '@/lib/format';
import { Calendar } from '@/components/ui/calendar';

const WEEK = ['日', '一', '二', '三', '四', '五', '六']; // getDay() 下标
const iso = (d: Date) => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
const LABELS = { labelPrevious: () => '上个月', labelNext: () => '下个月', labelDayButton: (d: Date) => iso(d), labelGrid: (d: Date) => `${d.getFullYear()} 年 ${d.getMonth() + 1} 月` };
const FORMATTERS = { formatCaption: (d: Date) => `${d.getFullYear()} 年 ${d.getMonth() + 1} 月`, formatWeekdayName: (d: Date) => WEEK[d.getDay()] ?? '' };

/** 单选、周一开头、固定 6 行（弹层高度不随月份变）的中文月历 */
export function DateCalendar({ selected, onPick, start, minD, maxD }: { selected: Date | undefined; onPick: (d: Date) => void; start: Date; minD?: Date; maxD?: Date }) {
  const blocked: React.ComponentProps<typeof Calendar>['disabled'] = [...(minD ? [{ before: minD }] : []), ...(maxD ? [{ after: maxD }] : [])];
  return (
    <Calendar
      mode="single" required autoFocus
      selected={selected} onSelect={(d) => d && onPick(d)}
      defaultMonth={start} startMonth={minD} endMonth={maxD} disabled={blocked}
      weekStartsOn={1} fixedWeeks locale={zhCN} labels={LABELS} formatters={FORMATTERS}
    />
  );
}
