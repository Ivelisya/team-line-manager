import * as React from 'react';
import * as SelectPrimitive from '@radix-ui/react-select';
import { Check, ChevronDown } from 'lucide-react';
import { cn } from '@/lib/utils';

export interface Option { value: string; label: React.ReactNode; disabled?: boolean }

/** shadcn Select 的一体化封装：<Select value onChange options placeholder />。空值用 '' 表示（内部转成哨兵）。 */
const EMPTY = '__empty__';
export function Select({ value, onChange, options, placeholder = '请选择', className, sm, disabled, id, 'aria-label': aria, allowEmpty }: {
  value: string; onChange: (v: string) => void; options: Option[]; placeholder?: string; className?: string; sm?: boolean; disabled?: boolean; id?: string; 'aria-label'?: string; allowEmpty?: string;
}) {
  return (
    <SelectPrimitive.Root value={value === '' ? (allowEmpty ? EMPTY : undefined) : value} onValueChange={(v) => onChange(v === EMPTY ? '' : v)} disabled={disabled}>
      <SelectPrimitive.Trigger id={id} aria-label={aria} className={cn('select-trigger', sm && 'input-sm', className)}>
        <SelectPrimitive.Value placeholder={placeholder} />
        <SelectPrimitive.Icon asChild><ChevronDown /></SelectPrimitive.Icon>
      </SelectPrimitive.Trigger>
      <SelectPrimitive.Portal>
        <SelectPrimitive.Content className="select-content" position="popper" sideOffset={4}>
          <SelectPrimitive.Viewport style={{ maxHeight: 300, overflowY: 'auto' }}>
            {allowEmpty && (
              <SelectPrimitive.Item value={EMPTY} className="select-item"><SelectPrimitive.ItemText>{allowEmpty}</SelectPrimitive.ItemText></SelectPrimitive.Item>
            )}
            {options.map((o) => (
              <SelectPrimitive.Item key={o.value} value={o.value} disabled={o.disabled} className="select-item">
                <SelectPrimitive.ItemText>{o.label}</SelectPrimitive.ItemText>
                <SelectPrimitive.ItemIndicator><Check /></SelectPrimitive.ItemIndicator>
              </SelectPrimitive.Item>
            ))}
          </SelectPrimitive.Viewport>
        </SelectPrimitive.Content>
      </SelectPrimitive.Portal>
    </SelectPrimitive.Root>
  );
}
