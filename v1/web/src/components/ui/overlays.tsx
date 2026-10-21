import * as React from 'react';
import * as TabsPrimitive from '@radix-ui/react-tabs';
import * as DropdownPrimitive from '@radix-ui/react-dropdown-menu';
import * as TooltipPrimitive from '@radix-ui/react-tooltip';
import * as PopoverPrimitive from '@radix-ui/react-popover';
import * as ToggleGroup from '@radix-ui/react-toggle-group';
import { ChevronRight } from 'lucide-react';
import { m } from 'motion/react';
import { cn } from '@/lib/utils';
import { SPRING } from '@/lib/motion';
import { sharedObserver } from '@/lib/sharedObserver';

/* ---- Tabs（下划线页签）：当前页签底下的 2px 下划线是一个共享 layoutId 的 motion 元素，切换时在页签之间滑动 ---- */
const TabsCtx = React.createContext<{ id: string; value: string | undefined } | null>(null);
export const Tabs = React.forwardRef<React.ElementRef<typeof TabsPrimitive.Root>, React.ComponentPropsWithoutRef<typeof TabsPrimitive.Root>>(({ value, defaultValue, onValueChange, ...p }, ref) => {
  const id = React.useId();
  // Radix 不对外暴露当前值：自己跟一份（受控取 value，非受控取内部记录），给触发器判断“我是不是当前页签”
  const [inner, setInner] = React.useState(defaultValue);
  const cur = value ?? inner;
  const ctx = React.useMemo(() => ({ id, value: cur }), [id, cur]);
  return (
    <TabsCtx.Provider value={ctx}>
      <TabsPrimitive.Root ref={ref} value={value} defaultValue={defaultValue} onValueChange={(v) => { setInner(v); onValueChange?.(v); }} {...p} />
    </TabsCtx.Provider>
  );
});
Tabs.displayName = 'Tabs';
export const TabsList = React.forwardRef<React.ElementRef<typeof TabsPrimitive.List>, React.ComponentPropsWithoutRef<typeof TabsPrimitive.List>>(({ className, ...p }, ref) => (
  <TabsPrimitive.List ref={ref} className={cn('tabs-list', className)} {...p} />
));
TabsList.displayName = 'TabsList';
/** noPanel：这组页签没有配对的 TabsContent（内容由页面自己按 value 渲染）时传，去掉 Radix 默认写上的、指向不存在元素的 aria-controls */
export const TabsTrigger = React.forwardRef<React.ElementRef<typeof TabsPrimitive.Trigger>, React.ComponentPropsWithoutRef<typeof TabsPrimitive.Trigger> & { noPanel?: boolean }>(({ className, noPanel, children, ...p }, ref) => {
  const ctx = React.useContext(TabsCtx);
  return (
    <TabsPrimitive.Trigger ref={ref} className={cn('tab', className)} {...(noPanel ? { 'aria-controls': undefined } : null)} {...p}>
      {children}
      {/* layoutDependency：当前页签不变时重渲染不量位置 */}
      {ctx && ctx.value === p.value && <m.span aria-hidden className="tab-ind" layoutId={`tab-ind-${ctx.id}`} layoutDependency={ctx.value} transition={SPRING} />}
    </TabsPrimitive.Trigger>
  );
});
TabsTrigger.displayName = 'TabsTrigger';
export const TabsContent = TabsPrimitive.Content;

/* ---- 下拉菜单 ---- */
export const DropdownMenu = DropdownPrimitive.Root;
export const DropdownMenuTrigger = DropdownPrimitive.Trigger;
// 菜单离视口边至少 8px（collisionPadding）：贴边翻转、可用高度（.menu 的 max-height 读它）都按留白后的视口算
export const DropdownMenuContent = React.forwardRef<React.ElementRef<typeof DropdownPrimitive.Content>, React.ComponentPropsWithoutRef<typeof DropdownPrimitive.Content>>(({ className, sideOffset = 4, ...p }, ref) => (
  <DropdownPrimitive.Portal><DropdownPrimitive.Content ref={ref} sideOffset={sideOffset} collisionPadding={8} className={cn('menu', className)} {...p} /></DropdownPrimitive.Portal>
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
  <DropdownPrimitive.Portal><DropdownPrimitive.SubContent ref={ref} collisionPadding={8} className={cn('menu', className)} {...p} /></DropdownPrimitive.Portal>
));
DropdownMenuSubContent.displayName = 'DropdownMenuSubContent';

/* ---- Tooltip ---- */
export const TooltipProvider = ({ children }: { children: React.ReactNode }) => <TooltipPrimitive.Provider delayDuration={250}>{children}</TooltipPrimitive.Provider>;
type Side = 'top' | 'bottom' | 'left' | 'right';
export function Tip({ text, children, side = 'top' }: { text: React.ReactNode; children: React.ReactElement; side?: Side }) {
  if (!text) return children;
  return (
    <TooltipPrimitive.Root>
      <TooltipPrimitive.Trigger asChild>{children}</TooltipPrimitive.Trigger>
      <TooltipPrimitive.Portal><TooltipPrimitive.Content side={side} sideOffset={6} collisionPadding={8} className="tooltip">{text}</TooltipPrimitive.Content></TooltipPrimitive.Portal>
    </TooltipPrimitive.Root>
  );
}

/** 共享悬停提示：卡片、表格行里成百上千的只读说明不各挂一个 Radix Tooltip 根（每个根带一套 Popper 上下文，随实时推送跟着重渲染），
 *  元素上写 data-tip="文字"，外壳里这一份按悬停 / 键盘聚焦到的元素定位显示。外观、延迟同 Tip：停 250ms 出现，刚有提示开着时直接换过去。
 *  空的 data-tip 当作没有（拼出来的说明可能为空，调用方不用再判） */
const TIP_SEL = '[data-tip]:not([data-tip=""])';
export function TipHost() {
  const [tip, setTip] = React.useState<{ id: number; text: string; r: DOMRect } | null>(null);
  const [open, setOpen] = React.useState(false);
  const hide = React.useRef(() => {});
  React.useEffect(() => {
    let cur: Element | null = null; let timer = 0; let shown = false; let closedAt = 0; let seq = 0;
    const show = (el: HTMLElement) => { shown = true; setTip({ id: ++seq, text: el.dataset.tip ?? '', r: el.getBoundingClientRect() }); setOpen(true); };
    const close = () => {
      clearTimeout(timer); cur = null;
      if (shown) { shown = false; closedAt = performance.now(); setOpen(false); }
    };
    hide.current = close;
    const over = (e: PointerEvent) => {
      if (e.pointerType === 'touch') return;
      const el = (e.target as Element).closest<HTMLElement>(TIP_SEL);
      if (el === cur) return;
      close();
      if (!el) return;
      cur = el;
      if (performance.now() - closedAt < 300) show(el); else timer = window.setTimeout(() => show(el), 250);
    };
    // 键盘聚焦才弹；鼠标按下得到的焦点不算（同 Radix）
    const focus = (e: FocusEvent) => { const el = (e.target as Element).closest<HTMLElement>(TIP_SEL); if (el?.matches(':focus-visible')) { close(); cur = el; show(el); } };
    const leave = (e: PointerEvent) => { if (!e.relatedTarget) close(); }; // 指针离开窗口
    const on: [EventTarget, string, EventListener, boolean][] = [
      [document, 'pointerover', over as EventListener, false], [document, 'pointerout', leave as EventListener, false], [document, 'pointerdown', close, true],
      [document, 'focusin', focus as EventListener, false], [document, 'focusout', close, false], [window, 'scroll', close, true],
    ];
    on.forEach(([t, n, f, c]) => t.addEventListener(n, f, c));
    return () => { clearTimeout(timer); on.forEach(([t, n, f, c]) => t.removeEventListener(n, f, c)); };
  }, []);
  if (!tip) return null;
  // key：换到另一个元素时重新挂载（同 Tip 之间切换：旧的收起、新的立即出现），不在原地改字挪位置
  return (
    <TooltipPrimitive.Root key={tip.id} open={open} onOpenChange={(o) => { if (!o) hide.current(); }}>
      <TooltipPrimitive.Trigger asChild><span className="float-anchor" style={{ left: tip.r.left, top: tip.r.top, width: tip.r.width, height: tip.r.height }} /></TooltipPrimitive.Trigger>
      <TooltipPrimitive.Portal><TooltipPrimitive.Content sideOffset={6} collisionPadding={8} className="tooltip">{tip.text}</TooltipPrimitive.Content></TooltipPrimitive.Portal>
    </TooltipPrimitive.Root>
  );
}

// 全站共享一个 ResizeObserver：表格里每个单元格一个 Trunc，不必各建一个观察器
const watchSize = sharedObserver<ResizeObserverEntry>((cb) => new ResizeObserver(cb));

/** 单行省略文本：只有真的被截断时悬停才出现完整内容的提示 */
export function Trunc({ children, text, className }: { children: React.ReactNode; text?: string; className?: string }) {
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
      <TooltipPrimitive.Portal><TooltipPrimitive.Content sideOffset={6} collisionPadding={8} className="tooltip">{full}</TooltipPrimitive.Content></TooltipPrimitive.Portal>
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
export interface SegItem<T extends string> { value: T; label: React.ReactNode; count?: number }
/** memo：父页面随实时推送重渲染时，items / onChange 稳定（模块常量、useMemo、useCallback）的分段控件不跟着重画 */
type SegProps<T extends string> = { value: T; onChange: (v: T) => void; items: readonly SegItem<T>[]; label: string; sm?: boolean; className?: string };
export const Segmented = React.memo(function Segmented<T extends string>({ value, onChange, items, label, sm, className }: SegProps<T>) {
  const gid = React.useId();
  return (
    <ToggleGroup.Root type="single" value={value} onValueChange={(v) => v && onChange(v as T)} aria-label={label} className={cn('seg', sm && 'seg-sm', className)}>
      {items.map((it) => (
        <ToggleGroup.Item key={it.value} value={it.value} className="seg-item">
          {/* 选中胶囊：共享 layoutId，换项时滑过去；圆角写在 style 里，motion 才会在缩放过程中修正它。
              layoutDependency：选中项不变时父级重渲染（如实时推送）不再量位置 */}
          {value === it.value && <m.span aria-hidden className="seg-ind" layoutId={`seg-ind-${gid}`} layoutDependency={value} style={{ borderRadius: 999 }} transition={SPRING} />}
          {it.label}{it.count != null && <span className="seg-count">{it.count}</span>}
        </ToggleGroup.Item>
      ))}
    </ToggleGroup.Root>
  );
}) as <T extends string>(p: SegProps<T>) => React.ReactElement;
