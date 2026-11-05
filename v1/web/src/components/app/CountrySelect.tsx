// 国家选择器：触发器显示国旗 + 中文名（或「自动」那一项），弹出一个可搜索的列表——中文名、英文名、两位代码都能搜到。
// 上架表单、上架表格的「国家」格、机器「管理」页签共用。值是两位代码，空串 = 自动（按 IP 查）
import * as React from 'react';
import { Check, ChevronDown, Globe } from 'lucide-react';
import { COMMON_COUNTRIES, countries, countryName, flagUrl } from '@/lib/flags';
import { cn } from '@/lib/utils';
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from '@/components/ui/command';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { selectTrigger } from '@/components/ui/select';

const FlagImg = ({ cc }: { cc: string }) => {
  const url = flagUrl(cc);
  return url ? <img className="nc-flag" src={url} alt="" loading="lazy" decoding="async" /> : <span className="nc-flag is-none"><Globe /></span>;
};

export function CountrySelect({ id, value, onChange, autoLabel, className, disabled, 'aria-label': aria, onPaste }: {
  id?: string; value: string; onChange: (code: string) => void; autoLabel: string; className?: string; disabled?: boolean; 'aria-label'?: string;
  onPaste?: React.ClipboardEventHandler;
}) {
  const [open, setOpen] = React.useState(false);
  const pick = (v: string) => { setOpen(false); if (v !== value) onChange(v); };
  const row = (c: { code: string; zh: string; en: string }) => (
    <CommandItem key={c.code} value={c.code} keywords={[c.zh, c.en]} onSelect={pick}>
      <FlagImg cc={c.code} /><span className="cmd-main">{c.zh}</span><span className="cmd-aside">{c.code}</span>{c.code === value && <Check className="cs-check" />}
    </CommandItem>
  );
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger disabled={disabled} id={id} data-slot="select-trigger" className={cn(selectTrigger, 'cs-trigger', className)} aria-label={aria} onPaste={onPaste}>
        <span className="cs-value">{value ? <><FlagImg cc={value} />{countryName(value)}</> : <><span className="nc-flag is-none"><Globe /></span>{autoLabel}</>}</span>
        <ChevronDown />
      </PopoverTrigger>
      <PopoverContent solid className="cs-pop p-0" align="start">
        {/* 列表只在打开时建（两百多项），关着不占渲染 */}
        {open && (
          <Command loop>
            <CommandInput placeholder="搜索：中文、英文或代码" />
            <CommandList>
              <CommandEmpty>没有这个国家或地区</CommandEmpty>
              <CommandItem value="auto" keywords={['自动', '按 IP']} onSelect={() => pick('')}>
                <span className="nc-flag is-none"><Globe /></span><span className="cmd-main">{autoLabel}</span>{!value && <Check className="cs-check" />}
              </CommandItem>
              <CommandGroup heading="常用">{countries().filter((c) => COMMON_COUNTRIES.includes(c.code)).sort((a, b) => COMMON_COUNTRIES.indexOf(a.code) - COMMON_COUNTRIES.indexOf(b.code)).map(row)}</CommandGroup>
              <CommandGroup heading="全部">{countries().filter((c) => !COMMON_COUNTRIES.includes(c.code)).map(row)}</CommandGroup>
            </CommandList>
          </Command>
        )}
      </PopoverContent>
    </Popover>
  );
}
