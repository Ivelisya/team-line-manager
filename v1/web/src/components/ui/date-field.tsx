// 日期选择（全站唯一实现，替代原生 <input type="date">）：触发器长得像输入框，值与显示一律 YYYY-MM-DD；
// 弹层是 Radix Popover + 月历网格。键盘：方向键逐日/逐周，PageUp/PageDown 换月（加 Shift 换年），Home/End 到周首/周尾，
// Enter/空格选中，Esc 关闭（焦点回到触发器）。日期按本地日历计算，不经过 Date 的时区换算。
// iconOnly：触发器只是一个日历图标按钮（批量网格里配合文本格用，文本格负责键入与整块粘贴）。
import * as React from 'react';
import * as PopoverPrimitive from '@radix-ui/react-popover';
import { Calendar, ChevronLeft, ChevronRight } from 'lucide-react';
import { cn } from '@/lib/utils';
import { addDays, addMonths, pad2 } from '@/lib/format';

const WEEK = ['一', '二', '三', '四', '五', '六', '日'];
const ISO = /^\d{4}-\d{2}-\d{2}$/;
const iso = (y: number, m: number, d: number) => `${y}-${pad2(m)}-${pad2(d)}`;
const today = () => { const t = new Date(); return iso(t.getFullYear(), t.getMonth() + 1, t.getDate()); };
const weekday = (s: string) => { const [y, m, d] = s.split('-').map(Number); return (new Date(Date.UTC(y, m - 1, d)).getUTCDay() + 6) % 7; }; // 周一 = 0

/** 某月的 6×7 网格（周一开头），含前后月补位 */
function monthGrid(month: string) {
  const first = `${month}-01`;
  const start = addDays(first, -weekday(first));
  return Array.from({ length: 42 }, (_, i) => addDays(start, i));
}

export function DateField({ value, onChange, min, id, placeholder = '选择日期', className, iconOnly, disabled, 'aria-label': aria }: {
  value: string; onChange: (v: string) => void; min?: string; id?: string; placeholder?: string; className?: string; iconOnly?: boolean; disabled?: boolean; 'aria-label'?: string;
}) {
  const [open, setOpen] = React.useState(false);
  const [focus, setFocus] = React.useState(() => (ISO.test(value) ? value : today()));
  const grid = React.useRef<HTMLDivElement>(null);
  const month = focus.slice(0, 7);
  const days = React.useMemo(() => monthGrid(month), [month]);
  const t = today();

  // 键盘、换月、打开时的落点夹到 min：焦点落在禁用的格子上会卡住（禁用按钮拿不到焦点）
  const clamp = (d: string) => (min && d < min ? min : d);
  const start = clamp(ISO.test(value) ? value : t);
  const focusDay = (d: string) => grid.current?.querySelector<HTMLButtonElement>(`[data-day="${d}"]`)?.focus();
  // 键盘移动、换月之后焦点跟着 focus 走（刚打开时由 onOpenAutoFocus 聚焦：那时弹层内容才挂进 Portal）
  React.useEffect(() => { if (open) focusDay(focus); }, [open, focus]);

  const pick = (d: string) => { if (min && d < min) return; onChange(d); setOpen(false); };
  const onKey = (e: React.KeyboardEvent) => {
    const step: Record<string, () => string> = {
      ArrowLeft: () => addDays(focus, -1), ArrowRight: () => addDays(focus, 1), ArrowUp: () => addDays(focus, -7), ArrowDown: () => addDays(focus, 7),
      Home: () => addDays(focus, -weekday(focus)), End: () => addDays(focus, 6 - weekday(focus)),
      PageUp: () => addMonths(focus, e.shiftKey ? -12 : -1), PageDown: () => addMonths(focus, e.shiftKey ? 12 : 1),
    };
    const f = step[e.key];
    if (f) { e.preventDefault(); setFocus(clamp(f())); }
  };
  const [y, m] = month.split('-').map(Number);

  return (
    <PopoverPrimitive.Root open={open} onOpenChange={(o) => { if (o) setFocus(start); setOpen(o); }}>
      <PopoverPrimitive.Trigger id={id} aria-label={aria} disabled={disabled} className={cn(iconOnly ? 'icon-btn icon-btn-sm' : 'select-trigger datefield', !iconOnly && !value && 'is-empty', className)}>
        {iconOnly ? <Calendar size={14} /> : <><span className="num">{value || placeholder}</span><Calendar /></>}
      </PopoverPrimitive.Trigger>
      <PopoverPrimitive.Portal>
        <PopoverPrimitive.Content className="popover cal" align="start" sideOffset={6} collisionPadding={8} onOpenAutoFocus={(e) => { e.preventDefault(); focusDay(start); }}>
          <div className="cal-head">
            <button type="button" className="icon-btn icon-btn-sm" aria-label="上个月" onClick={() => setFocus(clamp(addMonths(focus, -1)))}><ChevronLeft size={16} /></button>
            <span className="cal-title" aria-live="polite">{y} 年 {m} 月</span>
            <button type="button" className="icon-btn icon-btn-sm" aria-label="下个月" onClick={() => setFocus(addMonths(focus, 1))}><ChevronRight size={16} /></button>
          </div>
          <div className="cal-grid" role="grid" aria-label={`${y} 年 ${m} 月`} ref={grid} onKeyDown={onKey}>
            {WEEK.map((w) => <span key={w} className="cal-wd" role="columnheader">{w}</span>)}
            {days.map((d) => {
              const off = d.slice(0, 7) !== month; const dis = !!min && d < min;
              return (
                <button key={d} type="button" role="gridcell" data-day={d} tabIndex={d === focus ? 0 : -1} disabled={dis}
                  className={cn('cal-day', off && 'is-off', d === t && 'is-today')} aria-selected={d === value} aria-label={d}
                  // 灰色的上 / 下月日期：按下时不先取焦点——取焦点会立刻翻月、格子挪了位置，松开时点不到它，只翻月没选中
                  onMouseDown={off ? (e) => e.preventDefault() : undefined}
                  onClick={() => pick(d)} onFocus={() => { if (d !== focus) setFocus(d); }}>
                  {Number(d.slice(8))}
                </button>
              );
            })}
          </div>
          <div className="cal-foot">
            <button type="button" className="btn btn-link btn-sm" disabled={!!min && t < min} onClick={() => pick(t)}>今天 {t}</button>
            {value && <span className="cal-cur num">已选 {value}</span>}
          </div>
        </PopoverPrimitive.Content>
      </PopoverPrimitive.Portal>
    </PopoverPrimitive.Root>
  );
}
