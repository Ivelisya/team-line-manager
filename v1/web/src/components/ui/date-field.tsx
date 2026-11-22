// 日期选择（全站唯一实现，替代原生 <input type="date">）：触发器长得像输入框，值与显示一律 YYYY-MM-DD；
// 弹层是 shadcn Popover（Base UI）+ Calendar（react-day-picker）：周一开头、中文星期与月份。键盘用 react-day-picker 自带的：方向键逐日 / 逐周，
// PageUp / PageDown 换月（加 Shift 换年），Home / End 到周首 / 周尾，Enter / 空格选中，Esc 关闭（焦点回到触发器）。
// 日期按本地日历计算，不经过 Date 的时区换算（用 new Date(年, 月 - 1, 日) 建、用 getFullYear / getMonth / getDate 读）。
// iconOnly：触发器只是一个日历图标按钮（批量网格里配合文本格用，文本格负责键入与整块粘贴）。
// 月历本身（react-day-picker + date-fns，约 85KB）在 date-calendar.tsx 里懒加载：指到 / 聚焦触发器时预取，打开时多半已经到了；没到时先放一块与月历同高的占位，弹层不跳。
import * as React from 'react';
import { Calendar as CalendarIcon } from 'lucide-react';
import { cn } from '@/lib/utils';
import { pad2 } from '@/lib/format';
import { Button } from '@/components/ui/button';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { selectTrigger } from '@/components/ui/select';

const loadCalendar = () => import('./date-calendar');
const DateCalendar = React.lazy(() => loadCalendar().then((m) => ({ default: m.DateCalendar })));
const prefetch = () => { loadCalendar().catch(() => { /* 预取失败不要紧，打开时会再试 */ }); };
/** 月历下载期间的占位，与月历同高：标题 --h-md + 间距 6 + 星期行 26 + 固定 6 行 ×（格子 --cell-size = --h-lg + 行距 2） */
const CalendarFallback = () => <div role="status" aria-label="加载中" style={{ height: 'calc(var(--h-md) + 32px + 6 * (var(--h-lg) + 2px))' }} />;

const ISO = /^\d{4}-\d{2}-\d{2}$/;
const iso = (d: Date) => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
// 缺段按 NaN（同 Number(undefined)）
const parse = (s: string) => { const [y = NaN, m = NaN, d = NaN] = s.split('-').map(Number); return new Date(y, m - 1, d); };

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
  const pick = (d: Date) => { onChange(iso(d)); setOpen(false); };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger id={id} aria-label={aria} disabled={disabled} onPointerEnter={prefetch} onFocus={prefetch}
        render={iconOnly
          ? (p) => <Button {...p} variant="ghost" size="icon-sm" className={cn('date-pick', className)}><CalendarIcon size={14} /></Button>
          : (p) => <button {...p} data-slot="select-trigger" data-placeholder={value ? undefined : ''} className={cn(selectTrigger, className)}><span className="num min-w-0 flex-1 truncate">{value || placeholder}</span><CalendarIcon /></button>} />
      {/* initialFocus={false}：落点交给日历自己的 autoFocus（选中日；没有选中日就是今天） */}
      <PopoverContent align="start" sideOffset={6} initialFocus={false} className="w-(--pop-w-md) p-2.5">
        <React.Suspense fallback={<CalendarFallback />}>
          <DateCalendar selected={sel} onPick={pick} start={start} minD={minD} maxD={maxD} />
        </React.Suspense>
        <div className="mt-2 flex items-center justify-between gap-2 border-t border-line pt-2">
          <Button variant="link" size="inline-sm" disabled={(!!minD && parse(t) < minD) || (!!maxD && parse(t) > maxD)} onClick={() => pick(parse(t))}>今天 {t}</Button>
          {value && <span className="num text-label text-fg-2">已选 {value}</span>}
        </div>
      </PopoverContent>
    </Popover>
  );
}
