import * as React from 'react';
import * as TabsPrimitive from '@radix-ui/react-tabs';
import * as DropdownPrimitive from '@radix-ui/react-dropdown-menu';
import * as TooltipPrimitive from '@radix-ui/react-tooltip';
import * as PopoverPrimitive from '@radix-ui/react-popover';
import * as ToggleGroup from '@radix-ui/react-toggle-group';
import { ChevronRight } from 'lucide-react';
import { cn } from '@/lib/utils';

/* ---- Tabs（下划线页签） ---- */
export const Tabs = TabsPrimitive.Root;
export const TabsList = React.forwardRef<React.ElementRef<typeof TabsPrimitive.List>, React.ComponentPropsWithoutRef<typeof TabsPrimitive.List>>(({ className, ...p }, ref) => (
  <TabsPrimitive.List ref={ref} className={cn('tabs-list', className)} {...p} />
));
TabsList.displayName = 'TabsList';
/** noPanel：这组页签没有配对的 TabsContent（内容由页面自己按 value 渲染）时传，去掉 Radix 默认写上的、指向不存在元素的 aria-controls */
export const TabsTrigger = React.forwardRef<React.ElementRef<typeof TabsPrimitive.Trigger>, React.ComponentPropsWithoutRef<typeof TabsPrimitive.Trigger> & { noPanel?: boolean }>(({ className, noPanel, ...p }, ref) => (
  <TabsPrimitive.Trigger ref={ref} className={cn('tab', className)} {...(noPanel ? { 'aria-controls': undefined } : null)} {...p} />
));
TabsTrigger.displayName = 'TabsTrigger';
export const TabsContent = TabsPrimitive.Content;

/* ---- 下拉菜单 ---- */
export const DropdownMenu = DropdownPrimitive.Root;
export const DropdownMenuTrigger = DropdownPrimitive.Trigger;
export const DropdownMenuContent = React.forwardRef<React.ElementRef<typeof DropdownPrimitive.Content>, React.ComponentPropsWithoutRef<typeof DropdownPrimitive.Content>>(({ className, sideOffset = 4, ...p }, ref) => (
  <DropdownPrimitive.Portal><DropdownPrimitive.Content ref={ref} sideOffset={sideOffset} className={cn('menu', className)} {...p} /></DropdownPrimitive.Portal>
));
DropdownMenuContent.displayName = 'DropdownMenuContent';
export const DropdownMenuItem = React.forwardRef<React.ElementRef<typeof DropdownPrimitive.Item>, React.ComponentPropsWithoutRef<typeof DropdownPrimitive.Item> & { danger?: boolean }>(({ className, danger, ...p }, ref) => (
  <DropdownPrimitive.Item ref={ref} className={cn('menu-item', danger && 'is-danger', className)} {...p} />
));
DropdownMenuItem.displayName = 'DropdownMenuItem';
export const DropdownMenuSeparator = () => <DropdownPrimitive.Separator className="menu-sep" />;
export const DropdownMenuSub = DropdownPrimitive.Sub;
export const DropdownMenuSubTrigger = React.forwardRef<React.ElementRef<typeof DropdownPrimitive.SubTrigger>, React.ComponentPropsWithoutRef<typeof DropdownPrimitive.SubTrigger>>(({ className, children, ...p }, ref) => (
  <DropdownPrimitive.SubTrigger ref={ref} className={cn('menu-item', className)} {...p}>{children}<ChevronRight className="menu-chev" /></DropdownPrimitive.SubTrigger>
));
DropdownMenuSubTrigger.displayName = 'DropdownMenuSubTrigger';
export const DropdownMenuSubContent = React.forwardRef<React.ElementRef<typeof DropdownPrimitive.SubContent>, React.ComponentPropsWithoutRef<typeof DropdownPrimitive.SubContent>>(({ className, ...p }, ref) => (
  <DropdownPrimitive.Portal><DropdownPrimitive.SubContent ref={ref} className={cn('menu', className)} {...p} /></DropdownPrimitive.Portal>
));
DropdownMenuSubContent.displayName = 'DropdownMenuSubContent';

/* ---- Tooltip ---- */
export const TooltipProvider = ({ children }: { children: React.ReactNode }) => <TooltipPrimitive.Provider delayDuration={250}>{children}</TooltipPrimitive.Provider>;
type Side = 'top' | 'bottom' | 'left' | 'right';
export function Tip({ text, children, side = 'top', align }: { text: React.ReactNode; children: React.ReactElement; side?: Side; align?: 'start' | 'center' | 'end' }) {
  if (!text) return children;
  return (
    <TooltipPrimitive.Root>
      <TooltipPrimitive.Trigger asChild>{children}</TooltipPrimitive.Trigger>
      <TooltipPrimitive.Portal><TooltipPrimitive.Content side={side} align={align} sideOffset={6} collisionPadding={8} className="tooltip">{text}</TooltipPrimitive.Content></TooltipPrimitive.Portal>
    </TooltipPrimitive.Root>
  );
}

// 全站共享一个 ResizeObserver（同 lib/useInView.ts）：表格里每个单元格一个 Trunc，不必各建一个观察器
let sharedRO: ResizeObserver | null = null;
const roCallbacks = new WeakMap<Element, () => void>();
function watchSize(el: Element, cb: () => void) {
  sharedRO ??= new ResizeObserver((entries) => { for (const e of entries) roCallbacks.get(e.target)?.(); });
  roCallbacks.set(el, cb);
  sharedRO.observe(el);
  return () => { roCallbacks.delete(el); sharedRO?.unobserve(el); };
}

/** 单行省略文本：只有真的被截断时悬停才出现完整内容的提示 */
export function Trunc({ children, text, className, side = 'top' }: { children: React.ReactNode; text?: string; className?: string; side?: Side }) {
  const ref = React.useRef<HTMLSpanElement>(null);
  const [cut, setCut] = React.useState(false);
  const [open, setOpen] = React.useState(false);
  const [focusable, setFocusable] = React.useState(false);
  const full = text ?? (typeof children === 'string' ? children : '');
  const measure = React.useCallback(() => {
    const el = ref.current;
    const isCut = !!el && el.scrollWidth > el.clientWidth + 1;
    setCut(isCut);
    // 被截断、又不在按钮/链接里时才加入 Tab 顺序；在按钮里的由按钮自己承担焦点
    setFocusable(isCut && !!el && !el.parentElement?.closest('a, button, [tabindex]'));
  }, []);
  React.useEffect(() => {
    const el = ref.current;
    if (!el) return;
    measure();
    return watchSize(el, measure);
  }, [measure, full]);
  return (
    <TooltipPrimitive.Root open={open && cut && !!full} onOpenChange={(o) => { if (o) measure(); setOpen(o); }}>
      <TooltipPrimitive.Trigger asChild>
        <span ref={ref} className={cn('ellipsis block', className)} tabIndex={focusable && full ? 0 : undefined} onPointerEnter={measure} onFocus={measure}>{children}</span>
      </TooltipPrimitive.Trigger>
      <TooltipPrimitive.Portal><TooltipPrimitive.Content side={side} sideOffset={6} collisionPadding={8} className="tooltip">{full}</TooltipPrimitive.Content></TooltipPrimitive.Portal>
    </TooltipPrimitive.Root>
  );
}

/* ---- Popover ---- */
export const Popover = PopoverPrimitive.Root;
export const PopoverTrigger = PopoverPrimitive.Trigger;
export const PopoverContent = React.forwardRef<React.ElementRef<typeof PopoverPrimitive.Content>, React.ComponentPropsWithoutRef<typeof PopoverPrimitive.Content>>(({ className, sideOffset = 6, ...p }, ref) => (
  <PopoverPrimitive.Portal><PopoverPrimitive.Content ref={ref} sideOffset={sideOffset} className={cn('popover', className)} {...p} /></PopoverPrimitive.Portal>
));
PopoverContent.displayName = 'PopoverContent';

/* ---- 分段控件（单选）：大卡片/小卡片/紧凑、团队筛选等 ---- */
export interface SegItem<T extends string> { value: T; label: React.ReactNode; count?: number; disabled?: boolean }
export function Segmented<T extends string>({ value, onChange, items, label, sm, className }: { value: T; onChange: (v: T) => void; items: SegItem<T>[]; label: string; sm?: boolean; className?: string }) {
  return (
    <ToggleGroup.Root type="single" value={value} onValueChange={(v) => v && onChange(v as T)} aria-label={label} className={cn('seg', sm && 'seg-sm', className)}>
      {items.map((it) => (
        <ToggleGroup.Item key={it.value} value={it.value} disabled={it.disabled} className="seg-item">
          {it.label}{it.count != null && <span className="seg-count">{it.count}</span>}
        </ToggleGroup.Item>
      ))}
    </ToggleGroup.Root>
  );
}
