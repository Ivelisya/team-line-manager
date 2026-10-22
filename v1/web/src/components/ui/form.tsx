import * as React from 'react';
import * as LabelPrimitive from '@radix-ui/react-label';
import * as CheckboxPrimitive from '@radix-ui/react-checkbox';
import * as SwitchPrimitive from '@radix-ui/react-switch';
import { Check, Info, Search } from 'lucide-react';
import { cn } from '@/lib/utils';

/** 登录用的输入：密码框，或 autoComplete 写明是用户名 / 密码 / 验证码的。其余文本框（团队名、套餐名、搜索…）不让密码管理器挂图标 */
const CREDENTIAL = new Set(['username', 'current-password', 'new-password', 'one-time-code']);
export const Input = React.forwardRef<HTMLInputElement, React.InputHTMLAttributes<HTMLInputElement> & { sm?: boolean }>(({ className, sm, ...p }, ref) => (
  <input ref={ref} className={cn('input', sm && 'input-sm', className)} {...(p.type !== 'password' && !CREDENTIAL.has(p.autoComplete ?? '') ? { 'data-1p-ignore': '', 'data-lpignore': 'true' } : null)} {...p} />
));
Input.displayName = 'Input';

/** 带单位后缀的短输入框（「7 天」「4 小时」）：width 是整个框的宽度，只填一两位数的不用撑满一行。id 等属性落在里面的 input 上（Field 的标签对得上） */
export const UnitInput = React.forwardRef<HTMLInputElement, React.InputHTMLAttributes<HTMLInputElement> & { unit: string; width?: number }>(({ unit, width, ...p }, ref) => (
  <span className="input-unit" style={width ? { width } : undefined}><Input ref={ref} {...p} /><span className="input-unit-u" aria-hidden>{unit}</span></span>
));
UnitInput.displayName = 'UnitInput';

export const Textarea = React.forwardRef<HTMLTextAreaElement, React.TextareaHTMLAttributes<HTMLTextAreaElement>>(({ className, ...p }, ref) => (
  <textarea ref={ref} className={cn('textarea', className)} {...p} />
));
Textarea.displayName = 'Textarea';

export const Label = React.forwardRef<React.ElementRef<typeof LabelPrimitive.Root>, React.ComponentPropsWithoutRef<typeof LabelPrimitive.Root>>(({ className, ...p }, ref) => (
  <LabelPrimitive.Root ref={ref} className={cn('label', className)} {...p} />
));
Label.displayName = 'Label';

/** 「(i)」：悬停 / 聚焦时显示的补充说明。界面上只留必须知道的短字，解释放这里（用户：说明性的小字太多） */
export function InfoTip({ text, className }: { text: string; className?: string }) {
  return <span className={cn('info-tip', className)} tabIndex={0} role="img" aria-label={text} data-tip={text}><Info /></span>;
}

/** 表单项：标签（可带「(i)」补充说明）+ 控件 + 短提示 / 错误。hint 只放一句十来个字的必要提示，长的解释写进 info */
export function Field({ label, info, hint, error, children, className }: { label?: React.ReactNode; info?: string; hint?: React.ReactNode; error?: React.ReactNode; children: React.ReactNode; className?: string }) {
  const id = React.useId();
  return (
    <div className={cn('field', className)}>
      {label && (info ? <span className="field-label"><Label htmlFor={id}>{label}</Label><InfoTip text={info} /></span> : <Label htmlFor={id}>{label}</Label>)}
      {React.isValidElement(children) ? React.cloneElement(children as React.ReactElement<{ id?: string }>, { id }) : children}
      {error ? <div className="hint is-error">{error}</div> : hint ? <div className="hint">{hint}</div> : null}
    </div>
  );
}

export const SearchInput = React.forwardRef<HTMLInputElement, React.InputHTMLAttributes<HTMLInputElement>>(({ className, ...p }, ref) => (
  <span className={cn('search', className)}><Search /><Input ref={ref} type="search" {...p} /></span>
));
SearchInput.displayName = 'SearchInput';

export const Checkbox = React.forwardRef<React.ElementRef<typeof CheckboxPrimitive.Root>, React.ComponentPropsWithoutRef<typeof CheckboxPrimitive.Root>>(({ className, ...p }, ref) => (
  <CheckboxPrimitive.Root ref={ref} className={cn('checkbox', className)} {...p}>
    <CheckboxPrimitive.Indicator><Check /></CheckboxPrimitive.Indicator>
  </CheckboxPrimitive.Root>
));
Checkbox.displayName = 'Checkbox';

export function CheckRow({ checked, onChange, children, disabled, className }: { checked: boolean; onChange: (v: boolean) => void; children: React.ReactNode; disabled?: boolean; className?: string }) {
  const id = React.useId();
  return (
    <label htmlFor={id} className={cn('check-row', className)}>
      <Checkbox id={id} checked={checked} disabled={disabled} onCheckedChange={(v) => onChange(v === true)} />
      <span>{children}</span>
    </label>
  );
}

export const Switch = React.forwardRef<React.ElementRef<typeof SwitchPrimitive.Root>, React.ComponentPropsWithoutRef<typeof SwitchPrimitive.Root>>(({ className, ...p }, ref) => (
  <SwitchPrimitive.Root ref={ref} className={cn('switch', className)} {...p}><SwitchPrimitive.Thumb asChild><span /></SwitchPrimitive.Thumb></SwitchPrimitive.Root>
));
Switch.displayName = 'Switch';
