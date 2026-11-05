import * as React from 'react';
import * as TabsPrimitive from '@radix-ui/react-tabs';
import * as DropdownPrimitive from '@radix-ui/react-dropdown-menu';
import * as PopoverPrimitive from '@radix-ui/react-popover';
import * as ToggleGroup from '@radix-ui/react-toggle-group';
import { Check, ChevronDown, ChevronRight } from 'lucide-react';
import { m } from 'motion/react';
import { cn, scrollBehavior } from '@/lib/utils';
import { SPRING } from '@/lib/motion';
import { watchSize } from '@/lib/sharedObserver';

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
/** 页签放不下时（窄屏抽屉）横向滚动：被裁的那一侧渐隐提示还有页签，选中的页签自动滚进可见区 */
export const TabsList = React.forwardRef<React.ElementRef<typeof TabsPrimitive.List>, React.ComponentPropsWithoutRef<typeof TabsPrimitive.List>>(({ className, ...p }, ref) => {
  const el = React.useRef<HTMLDivElement | null>(null);
  const cur = React.useContext(TabsCtx)?.value;
  const [fade, setFade] = React.useState('');
  const setRef = React.useCallback((n: HTMLDivElement | null) => { el.current = n; if (typeof ref === 'function') ref(n); else if (ref) (ref as React.MutableRefObject<HTMLDivElement | null>).current = n; }, [ref]);
  React.useLayoutEffect(() => {
    const n = el.current; if (!n) return;
    if (CSS.supports('animation-timeline: scroll()')) return; // 渐隐由 base.css 的滚动驱动动画给；这里只给不支持的浏览器兜底
    const update = () => { const l = n.scrollLeft > 1; const r = n.scrollLeft + n.clientWidth < n.scrollWidth - 1; setFade(l && r ? 'lr' : l ? 'l' : r ? 'r' : ''); };
    update();
    n.addEventListener('scroll', update, { passive: true });
    const off = watchSize(n, update);
    return () => { n.removeEventListener('scroll', update); off(); };
  }, []);
  React.useEffect(() => {
    const n = el.current; const t = n?.querySelector<HTMLElement>('[data-state="active"]');
    if (!n || !t) return;
    const pad = 28; // 留出渐隐的宽度
    if (t.offsetLeft < n.scrollLeft + pad) n.scrollTo({ left: Math.max(0, t.offsetLeft - pad), behavior: scrollBehavior() });
    else if (t.offsetLeft + t.offsetWidth > n.scrollLeft + n.clientWidth - pad) n.scrollTo({ left: t.offsetLeft + t.offsetWidth - n.clientWidth + pad, behavior: scrollBehavior() });
  }, [cur]);
  return <TabsPrimitive.List ref={setRef} className={cn('tabs-list', className)} data-fade={fade || undefined} {...p} />;
});
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
/** 单选菜单项（外观的深浅色、配色）：读屏读得到选中了哪一项（aria-checked），选中的打勾 */
export const DropdownMenuRadioGroup = DropdownPrimitive.RadioGroup;
export const DropdownMenuRadioItem = React.forwardRef<React.ElementRef<typeof DropdownPrimitive.RadioItem>, React.ComponentPropsWithoutRef<typeof DropdownPrimitive.RadioItem>>(({ className, children, ...p }, ref) => (
  <DropdownPrimitive.RadioItem ref={ref} className={cn('menu-item', className)} {...p}>{children}<DropdownPrimitive.ItemIndicator className="menu-check"><Check /></DropdownPrimitive.ItemIndicator></DropdownPrimitive.RadioItem>
));
DropdownMenuRadioItem.displayName = 'DropdownMenuRadioItem';
export const DropdownMenuLabel = ({ children }: { children: React.ReactNode }) => <DropdownPrimitive.Label className="menu-label">{children}</DropdownPrimitive.Label>;
export const DropdownMenuSub = DropdownPrimitive.Sub;
export const DropdownMenuSubTrigger = React.forwardRef<React.ElementRef<typeof DropdownPrimitive.SubTrigger>, React.ComponentPropsWithoutRef<typeof DropdownPrimitive.SubTrigger>>(({ className, children, ...p }, ref) => (
  <DropdownPrimitive.SubTrigger ref={ref} className={cn('menu-item', className)} {...p}>{children}<ChevronRight className="menu-chev" /></DropdownPrimitive.SubTrigger>
));
DropdownMenuSubTrigger.displayName = 'DropdownMenuSubTrigger';
export const DropdownMenuSubContent = React.forwardRef<React.ElementRef<typeof DropdownPrimitive.SubContent>, React.ComponentPropsWithoutRef<typeof DropdownPrimitive.SubContent>>(({ className, ...p }, ref) => (
  <DropdownPrimitive.Portal><DropdownPrimitive.SubContent ref={ref} collisionPadding={8} className={cn('menu', className)} {...p} /></DropdownPrimitive.Portal>
));
DropdownMenuSubContent.displayName = 'DropdownMenuSubContent';

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
/** disabled：整组置灰（如首页的卡片密度在表格 / 环形视图下），仍占位、不让工具栏跳动 */
type SegProps<T extends string> = { value: T; onChange: (v: T) => void; items: readonly SegItem<T>[]; label: string; sm?: boolean; disabled?: boolean; className?: string };
export const Segmented = React.memo(function Segmented<T extends string>({ value, onChange, items, label, sm, disabled, className }: SegProps<T>) {
  const gid = React.useId();
  return (
    <ToggleGroup.Root type="single" value={value} onValueChange={(v) => v && onChange(v as T)} aria-label={label} disabled={disabled} className={cn('seg', sm && 'seg-sm', className)}>
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

/** 放不下就收进「更多」的分段控件（首页的团队分组）：轨道撑满可用宽度，从左往右能放几项放几项，其余进最右的「更多 ▾」下拉。
 *  选中项一定留在轨道上：它本该在「更多」里时，顶替轨道上最后一个可见项（被顶下来的进下拉）。
 *  每项的自然宽度在一份隐藏的同款副本里量（轨道里的项会被拉伸，量不准），轨道宽度变了、选项文字 / 计数变了才重量 */
export const SegmentedOverflow = React.memo(function SegmentedOverflow<T extends string>({ value, onChange, items, label, className }: Omit<SegProps<T>, 'sm' | 'disabled'>) {
  const gid = React.useId();
  const root = React.useRef<HTMLDivElement>(null);
  const meas = React.useRef<HTMLDivElement>(null);
  const [dim, setDim] = React.useState<{ avail: number; widths: number[]; more: number; gap: number } | null>(null);
  // 文字也算进去（团队改名时 id、台数都没变）；label 不是字符串的（图标等）按值认
  const sig = items.map((it) => `${it.value}:${it.count ?? ''}:${typeof it.label === 'string' || typeof it.label === 'number' ? it.label : ''}`).join('|');
  React.useLayoutEffect(() => {
    const r = root.current; const ms = meas.current;
    if (!r || !ms) return;
    const measure = () => {
      const cs = getComputedStyle(r);
      const avail = r.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
      const els = [...ms.children] as HTMLElement[];
      const widths = els.slice(0, -1).map((e) => e.getBoundingClientRect().width);
      const more = els[els.length - 1].getBoundingClientRect().width;
      const gap = parseFloat(getComputedStyle(ms).columnGap) || 0;
      setDim((o) => (o && o.avail === avail && o.more === more && o.widths.join() === widths.join() ? o : { avail, widths, more, gap }));
    };
    measure();
    return watchSize(r, measure);
  }, [sig]);
  // 算出轨道上放哪些项（按原顺序），其余进「更多」
  const sel = items.findIndex((it) => it.value === value);
  let shown = items.map((_, i) => i);
  if (dim && dim.widths.length === items.length) {
    const sum = (ix: number[]) => ix.reduce((a, i) => a + dim.widths[i], 0) + dim.gap * Math.max(0, ix.length - 1);
    if (sum(shown) > dim.avail + 0.5) {
      let k = items.length - 1;
      const pick = (k: number) => { const ix = shown.slice(0, k); return sel >= k && sel >= 0 ? [...ix.slice(0, Math.max(0, k - 1)), sel] : ix; };
      while (k > 1 && sum(pick(k)) + dim.gap + dim.more > dim.avail) k--;
      shown = pick(k);
    }
  }
  const hidden = items.filter((_, i) => !shown.includes(i));
  return (
    <div className={cn('seg-ovf', className)}>
      <ToggleGroup.Root ref={root} type="single" value={value} onValueChange={(v) => v && onChange(v as T)} aria-label={label} className="seg seg-fill">
        {shown.map((i) => { const it = items[i]; return (
          <ToggleGroup.Item key={it.value} value={it.value} className="seg-item">
            {value === it.value && <m.span aria-hidden className="seg-ind" layoutId={`seg-ind-${gid}`} layoutDependency={value} style={{ borderRadius: 999 }} transition={SPRING} />}
            {it.label}{it.count != null && <span className="seg-count">{it.count}</span>}
          </ToggleGroup.Item>
        ); })}
        {hidden.length > 0 && (
          <DropdownMenu>
            <DropdownMenuTrigger className="seg-item seg-more" aria-label={`更多${label}`}>更多<ChevronDown size={14} /></DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              {hidden.map((it) => (
                <DropdownMenuItem key={it.value} onSelect={() => onChange(it.value)}>
                  <span className="grow">{it.label}</span>{it.count != null && <span className="seg-count">{it.count}</span>}
                </DropdownMenuItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
        )}
      </ToggleGroup.Root>
      <div ref={meas} className="seg seg-measure" aria-hidden>
        {items.map((it) => <span key={it.value} className="seg-item">{it.label}{it.count != null && <span className="seg-count">{it.count}</span>}</span>)}
        <span className="seg-item seg-more">更多<ChevronDown size={14} /></span>
      </div>
    </div>
  );
}) as <T extends string>(p: Omit<SegProps<T>, 'sm' | 'disabled'>) => React.ReactElement;
