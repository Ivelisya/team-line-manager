import * as React from 'react';
import { Search } from 'lucide-react';
import { cn } from '@/lib/utils';
import { Label } from '@/components/ui/label';
import { InfoTip } from '@/components/ui/info-tip';
import { Field, FieldDescription, FieldError } from '@/components/ui/field';
import { Checkbox } from '@/components/ui/checkbox';
import { Switch } from '@/components/ui/switch';
import { InputGroup, InputGroupAddon, InputGroupInput, InputGroupText } from '@/components/ui/input-group';

// 表单里 shadcn 没有的组合件（基础件各自在 input / textarea / label / field / checkbox / switch / input-group）：
// FormField（标签 + 控件 + 提示 / 错误，id 自动关联）、UnitInput、SearchInput、CheckRow、OptimisticSwitch。

/** 标签 + 可选「(i)」补充说明。
 *  htmlFor 指向一个控件时是 <label>；只给一组控件（分段、单选组、列表）起个标题时不传 htmlFor，渲染成同样外观的 span，不留指不到控件的 label。
 *  「(i)」在这一行里不要自带的左外边距（间距由 gap 给），同改前的 .field-label > .info-tip */
export function LabelRow({ htmlFor, info, children, className }: { htmlFor?: string; info?: string; children: React.ReactNode; className?: string }) {
  const text = htmlFor
    ? <Label htmlFor={htmlFor} className={info ? undefined : className}>{children}</Label>
    : <span data-slot={info ? undefined : 'field-label'} className={cn('inline-flex items-center gap-1 text-label font-medium text-fg-2', !info && className)}>{children}</span>;
  if (!info) return text;
  return <span data-slot="field-label" className={cn('inline-flex min-w-0 items-center gap-1 [&>.info-tip]:m-0', className)}>{text}<InfoTip text={info} /></span>;
}

/** 表单项：标签（可带「(i)」补充说明）+ 控件 + 短提示 / 错误。hint 只放一句十来个字的必要提示，长的解释写进 info。
 *  控件（children 是单个元素时）自动拿到 id，标签 htmlFor 指向它。网格布局（grid + 标签 / 控件 / 提示三行）沿用原样：表单页按它对齐同一行的字段 */
export function FormField({ label, info, hint, error, children, className }: { label?: React.ReactNode; info?: string; hint?: React.ReactNode; error?: React.ReactNode; children: React.ReactNode; className?: string }) {
  const id = React.useId();
  return (
    <Field className={className}>
      {label && <LabelRow htmlFor={id} info={info}>{label}</LabelRow>}
      {React.isValidElement(children) ? React.cloneElement(children as React.ReactElement<{ id?: string }>, { id }) : children}
      {error ? <FieldError>{error}</FieldError> : hint ? <FieldDescription>{hint}</FieldDescription> : null}
    </Field>
  );
}

/** 带单位后缀的输入框（「7 天」「4 小时」）：width 是像素宽，'full' 占满一格（表单网格里与下拉框等宽）；id 等属性落在里面的 input 上（标签对得上） */
export function UnitInput({ unit, width, className, style, ref, ...p }: React.ComponentProps<'input'> & { unit: string; width?: number | 'full' }) {
  return (
    <InputGroup className={cn('max-w-full', className)} style={width ? { width: width === 'full' ? '100%' : width } : undefined}>
      <InputGroupInput ref={ref} className="tabular-nums" style={style} {...p} />
      <InputGroupAddon align="inline-end"><InputGroupText aria-hidden>{unit}</InputGroupText></InputGroupAddon>
    </InputGroup>
  );
}

/** 搜索框：左边放大镜，type=search（原生清除键，样式见 components.css） */
export function SearchInput({ className, ref, ...p }: React.ComponentProps<'input'>) {
  return (
    <InputGroup className={className}>
      <InputGroupAddon><Search /></InputGroupAddon>
      <InputGroupInput ref={ref} type="search" {...p} />
    </InputGroup>
  );
}

/** 整行可点的复选行 / 开关行：点文字也能切换；disabled 时文字一起调淡。switch = 前面是开关（间距略大）；aria-label 给控件一个固定的名字（文字会随状态变时用） */
export function CheckRow({ checked, onChange, children, disabled, className, switch: asSwitch, 'aria-label': ariaLabel }: { checked: boolean; onChange: (v: boolean) => void; children: React.ReactNode; disabled?: boolean; className?: string; switch?: boolean; 'aria-label'?: string }) {
  return (
    <Label className={cn('cursor-pointer gap-2 text-body font-normal text-fg has-data-disabled:cursor-default', asSwitch && 'gap-2.5', className)}>
      {asSwitch
        ? <Switch checked={checked} disabled={disabled} onCheckedChange={(v) => onChange(v)} aria-label={ariaLabel} />
        : <Checkbox checked={checked} disabled={disabled} onCheckedChange={(v) => onChange(v)} aria-label={ariaLabel} />}
      <span className="peer-data-disabled:opacity-(--o-disabled)">{children}</span>
    </Label>
  );
}

/** 值由服务端管的开关（点一下要等请求回来、数据重取才变）：点下立刻按新值显示，不等两次往返。
 *  onCheckedChange 的第二个参数 rollback：请求失败时调它回到服务端的值；成功就不用管——数据一跟上（checked 变了）本地的值自动清掉 */
export function OptimisticSwitch({ checked, onCheckedChange, ...p }: Omit<React.ComponentProps<typeof Switch>, 'checked' | 'onCheckedChange'> & { checked: boolean; onCheckedChange: (v: boolean, rollback: () => void) => void }) {
  const [opt, setOpt] = React.useState<{ v: boolean; base: boolean } | null>(null);
  if (opt && checked !== opt.base) setOpt(null);
  return <Switch checked={opt ? opt.v : checked} onCheckedChange={(v) => { setOpt({ v, base: checked }); onCheckedChange(v, () => setOpt(null)); }} {...p} />;
}
