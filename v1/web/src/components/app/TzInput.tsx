// 时区选择（订阅的账单时区、供应商时区共用）：触发器显示「洛杉矶（UTC−7）」，弹出可搜索的列表——中文城市名、IANA 名、UTC 偏移都能搜到。
// 常用一组（/meta 的常用时区 + 当前值），全部一组（浏览器认得的 IANA 时区）。emptyLabel：允许留空（如「跟随组织时区」），列表最前多一项
import * as React from 'react';
import { Check, ChevronDown } from 'lucide-react';
import { prefixScore } from '@/lib/match';
import { tzLabel, tzOptions, type TzOption } from '@/lib/tz';
import { cn } from '@/lib/utils';
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from '@/components/ui/command';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/overlays';

/** 搜索打分：城市名 / 时区名里某个词以它开头的在前，其次是连续子串（lib/match）；搜的空格当下划线（「los an」→ Los_Angeles） */
const tzFilter = (v: string, s: string, kw?: string[]) => prefixScore([v, ...(kw ?? [])], s.trim().toLowerCase().replace(/\s+/g, '_'));

export function TzInput({ id, value, onChange, emptyLabel, placeholder = '选择时区', 'aria-label': ariaLabel }: {
  id?: string; value: string; onChange: (v: string) => void; emptyLabel?: string; placeholder?: string; 'aria-label'?: string;
}) {
  const [open, setOpen] = React.useState(false);
  const pick = (v: string) => { setOpen(false); if (v !== value) onChange(v); };
  const row = (o: TzOption) => (
    <CommandItem key={o.value} value={o.value} keywords={o.keywords} onSelect={pick}>
      <span className="cmd-main">{o.label}</span>{o.value === value && <Check className="cs-check" />}
    </CommandItem>
  );
  // 列表只在打开时建（四百多项），关着不占渲染
  const opts = open ? tzOptions(value || undefined) : null;
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button type="button" id={id} className="select-trigger cs-trigger" aria-label={ariaLabel}>
          <span className={cn('cs-value ellipsis', !value && !emptyLabel && 't-muted')}>{value ? tzLabel(value) : emptyLabel ?? placeholder}</span>
          <ChevronDown />
        </button>
      </PopoverTrigger>
      <PopoverContent className="cs-pop is-wide" align="start">
        {opts && (
          <Command loop filter={tzFilter}>
            <CommandInput placeholder="搜索：城市、时区名或 UTC+8" />
            <CommandList>
              <CommandEmpty>没有这个时区</CommandEmpty>
              {emptyLabel && (
                <CommandItem value="__empty__" keywords={[emptyLabel]} onSelect={() => pick('')}>
                  <span className="cmd-main">{emptyLabel}</span>{!value && <Check className="cs-check" />}
                </CommandItem>
              )}
              <CommandGroup heading="常用">{opts.common.map(row)}</CommandGroup>
              <CommandGroup heading="全部">{opts.all.map(row)}</CommandGroup>
            </CommandList>
          </Command>
        )}
      </PopoverContent>
    </Popover>
  );
}
