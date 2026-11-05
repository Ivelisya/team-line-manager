import * as React from 'react';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';

export interface Option { value: string; label: React.ReactNode; disabled?: boolean }

/** 「options 数组」写法的下拉选择：<OptionSelect value onChange options placeholder />（shadcn Select 的一体化封装，页面里 50 多处用）。
 *  空值用 '' 表示：不带 allowEmpty 时 '' 显示 placeholder（Base UI 里是 value=null）；带 allowEmpty 时多出一个选项，文字是 allowEmpty，选中时触发器显示它（不是 placeholder）——
 *  Base UI 把空串当作「没有值」，所以这一项内部用哨兵值，对外仍是 ''。
 *  值不在选项里时当作空值。
 *  items：Base UI 的 Select 在弹层没挂载时靠它显示选中项的文字（改前 Radix 把全部选项渲染进离屏片段来取）。
 *  memo：选项多的下拉传稳定的 options / onChange 就不随父组件（实时推送）重画 */
const EMPTY = '__empty__';
export const OptionSelect = React.memo(function OptionSelect({ value, onChange, options, placeholder = '请选择', className, sm, disabled, id, 'aria-label': aria, allowEmpty }: {
  value: string; onChange: (v: string) => void; options: Option[]; placeholder?: string; className?: string; sm?: boolean; disabled?: boolean; id?: string; 'aria-label'?: string; allowEmpty?: string;
}) {
  const items = React.useMemo(() => (allowEmpty ? [{ value: EMPTY, label: allowEmpty } as Option, ...options] : options), [allowEmpty, options]);
  // 值不在选项里（URL 里残留的旧参数、已删除的团队、选项还在加载）：显示 placeholder（带 allowEmpty 时显示那一项），不把原始值 / UUID 打出来
  const shown = value !== '' && options.some((o) => o.value === value) ? value : allowEmpty ? EMPTY : null;
  return (
    <Select items={items} value={shown} onValueChange={(v) => onChange(v == null || v === EMPTY ? '' : v)} disabled={disabled}>
      <SelectTrigger id={id} aria-label={aria} size={sm ? 'sm' : 'default'} className={className}>
        <SelectValue placeholder={placeholder} />
      </SelectTrigger>
      <SelectContent>
        {items.map((o) => <SelectItem key={o.value} value={o.value} disabled={o.disabled}>{o.label}</SelectItem>)}
      </SelectContent>
    </Select>
  );
});
