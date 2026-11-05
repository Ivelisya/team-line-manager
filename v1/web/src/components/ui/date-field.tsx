// 日期选择（全站唯一实现，替代原生 <input type="date">）：触发器长得像输入框，值与显示一律 YYYY-MM-DD；
// 弹层是 shadcn Popover（Base UI）+ Calendar（react-day-picker）：周一开头、中文星期与月份。键盘用 react-day-picker 自带的：方向键逐日 / 逐周，
// PageUp / PageDown 换月（加 Shift 换年），Home / End 到周首 / 周尾，Enter / 空格选中，Esc 关闭（焦点回到触发器）。
// 日期按本地日历计算，不经过 Date 的时区换算（用 new Date(年, 月 - 1, 日) 建、用 getFullYear / getMonth / getDate 读）。
// iconOnly：触发器只是一个日历图标按钮（批量网格里配合文本格用，文本格负责键入与整块粘贴）。
import * as React from 'react';
import { Calendar as CalendarIcon } from 'lucide-react';
import { zhCN } from 'react-day-picker/locale/zh-CN';
import { cn } from '@/lib/utils';
import { pad2 } from '@/lib/format';
import { Button } from '@/components/ui/button';
import { Calendar } from '@/components/ui/calendar';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { selectTrigger } from '@/components/ui/select';

const WEEK = ['日', '一', '二', '三', '四', '五', '六']; // getDay() 下标
const ISO = /^\d{4}-\d{2}-\d{2}$/;
const iso = (d: Date) => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
const parse = (s: string) => { const [y, m, d] = s.split('-').map(Number); return new Date(y, m - 1, d); };
const LABELS = { labelPrevious: () => '上个月', labelNext: () => '下个月', labelDayButton: (d: Date) => iso(d), labelGrid: (d: Date) => `${d.getFullYear()} 年 ${d.getMonth() + 1} 月` };
const FORMATTERS = { formatCaption: (d: Date) => `${d.getFullYear()} 年 ${d.getMonth() + 1} 月`, formatWeekdayName: (d: Date) => WEEK[d.getDay()] };

export function DateField({ value, onChange, min, max, id, placeholder = '选择日期', className, iconOnly, disabled, 'aria-label': aria }: {
  value: string; onChange: (v: string) => void; min?: string; max?: string; id?: string; placeholder?: string; className?: string; iconOnly?: boolean; disabled?: boolean; 'aria-label'?: string;
}) {
  const [open, setOpen] = React.useState(false);
  const now = new Date();
  const t = iso(now);
  const sel = ISO.test(value) ? parse(value) : undefined;
  const minD = min && ISO.test(min) ? parse(min) : undefined;
  const maxD = max && ISO.test(max) ? parse(max) : undefined;
  // 打开时的落点夹到 [min, max]：焦点落在禁用的格子上会卡住
  const clamp = (d: Date) => (minD && d < minD ? minD : maxD && d > maxD ? maxD : d);
  const start = clamp(sel ?? parse(t));
  const blocked: React.ComponentProps<typeof Calendar>['disabled'] = [...(minD ? [{ before: minD }] : []), ...(maxD ? [{ after: maxD }] : [])];
  const pick = (d: Date) => { onChange(iso(d)); setOpen(false); };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger id={id} aria-label={aria} disabled={disabled}
        render={iconOnly
          ? (p) => <Button {...p} variant="ghost" size="icon-sm" className={cn('date-pick', className)}><CalendarIcon size={14} /></Button>
          : (p) => <button {...p} data-slot="select-trigger" data-placeholder={value ? undefined : ''} className={cn(selectTrigger, className)}><span className="num min-w-0 flex-1 truncate">{value || placeholder}</span><CalendarIcon /></button>} />
      {/* initialFocus={false}：落点交给日历自己的 autoFocus（选中日；没有选中日就是今天） */}
      <PopoverContent align="start" sideOffset={6} initialFocus={false} className="w-(--pop-w-md) p-2.5">
        <Calendar
          mode="single" required autoFocus
          selected={sel} onSelect={(d) => d && pick(d)}
          defaultMonth={start} startMonth={minD} endMonth={maxD} disabled={blocked}
          weekStartsOn={1} fixedWeeks locale={zhCN} labels={LABELS} formatters={FORMATTERS}
        />
        <div className="mt-2 flex items-center justify-between gap-2 border-t border-line pt-2">
          <Button variant="link" size="inline-sm" disabled={(!!minD && parse(t) < minD) || (!!maxD && parse(t) > maxD)} onClick={() => pick(parse(t))}>今天 {t}</Button>
          {value && <span className="num text-label text-fg-2">已选 {value}</span>}
        </div>
      </PopoverContent>
    </Popover>
  );
}
