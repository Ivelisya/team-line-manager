import * as React from 'react';
import * as LabelPrimitive from '@radix-ui/react-label';
import * as CheckboxPrimitive from '@radix-ui/react-checkbox';
import * as SwitchPrimitive from '@radix-ui/react-switch';
import { Check, Search } from 'lucide-react';
import { cn } from '@/lib/utils';

export const Input = React.forwardRef<HTMLInputElement, React.InputHTMLAttributes<HTMLInputElement> & { sm?: boolean }>(({ className, sm, ...p }, ref) => (
  <input ref={ref} className={cn('input', sm && 'input-sm', className)} {...p} />
));
Input.displayName = 'Input';

export const Textarea = React.forwardRef<HTMLTextAreaElement, React.TextareaHTMLAttributes<HTMLTextAreaElement>>(({ className, ...p }, ref) => (
  <textarea ref={ref} className={cn('textarea', className)} {...p} />
));
Textarea.displayName = 'Textarea';

export const Label = React.forwardRef<React.ElementRef<typeof LabelPrimitive.Root>, React.ComponentPropsWithoutRef<typeof LabelPrimitive.Root>>(({ className, ...p }, ref) => (
  <LabelPrimitive.Root ref={ref} className={cn('label', className)} {...p} />
));
Label.displayName = 'Label';

/** 表单项：标签 + 控件 + 说明/错误 */
export function Field({ label, hint, error, children, className }: { label?: React.ReactNode; hint?: React.ReactNode; error?: React.ReactNode; children: React.ReactNode; className?: string }) {
  const id = React.useId();
  return (
    <div className={cn('field', className)}>
      {label && <Label htmlFor={id}>{label}</Label>}
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

export function CheckRow({ checked, onChange, children, disabled }: { checked: boolean; onChange: (v: boolean) => void; children: React.ReactNode; disabled?: boolean }) {
  const id = React.useId();
  return (
    <label htmlFor={id} className="check-row" style={disabled ? { opacity: 0.5 } : undefined}>
      <Checkbox id={id} checked={checked} disabled={disabled} onCheckedChange={(v) => onChange(v === true)} />
      <span>{children}</span>
    </label>
  );
}

export const Switch = React.forwardRef<React.ElementRef<typeof SwitchPrimitive.Root>, React.ComponentPropsWithoutRef<typeof SwitchPrimitive.Root>>(({ className, ...p }, ref) => (
  <SwitchPrimitive.Root ref={ref} className={cn('switch', className)} {...p}><SwitchPrimitive.Thumb asChild><span /></SwitchPrimitive.Thumb></SwitchPrimitive.Root>
));
Switch.displayName = 'Switch';
