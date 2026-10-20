import * as React from 'react';
import * as TabsPrimitive from '@radix-ui/react-tabs';
import * as DropdownPrimitive from '@radix-ui/react-dropdown-menu';
import * as TooltipPrimitive from '@radix-ui/react-tooltip';
import * as PopoverPrimitive from '@radix-ui/react-popover';
import * as ToggleGroup from '@radix-ui/react-toggle-group';
import { cn } from '@/lib/utils';

/* ---- Tabs（下划线页签） ---- */
export const Tabs = TabsPrimitive.Root;
export const TabsList = React.forwardRef<React.ElementRef<typeof TabsPrimitive.List>, React.ComponentPropsWithoutRef<typeof TabsPrimitive.List>>(({ className, ...p }, ref) => (
  <TabsPrimitive.List ref={ref} className={cn('tabs-list', className)} {...p} />
));
TabsList.displayName = 'TabsList';
export const TabsTrigger = React.forwardRef<React.ElementRef<typeof TabsPrimitive.Trigger>, React.ComponentPropsWithoutRef<typeof TabsPrimitive.Trigger>>(({ className, ...p }, ref) => (
  <TabsPrimitive.Trigger ref={ref} className={cn('tab', className)} {...p} />
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
export const DropdownMenuLabel = ({ children }: { children: React.ReactNode }) => <DropdownPrimitive.Label className="hint" style={{ padding: '4px 10px' }}>{children}</DropdownPrimitive.Label>;

/* ---- Tooltip ---- */
export const TooltipProvider = ({ children }: { children: React.ReactNode }) => <TooltipPrimitive.Provider delayDuration={250}>{children}</TooltipPrimitive.Provider>;
export function Tip({ text, children, side = 'top' }: { text: React.ReactNode; children: React.ReactElement; side?: 'top' | 'bottom' | 'left' | 'right' }) {
  if (!text) return children;
  return (
    <TooltipPrimitive.Root>
      <TooltipPrimitive.Trigger asChild>{children}</TooltipPrimitive.Trigger>
      <TooltipPrimitive.Portal><TooltipPrimitive.Content side={side} sideOffset={6} className="tooltip">{text}</TooltipPrimitive.Content></TooltipPrimitive.Portal>
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
