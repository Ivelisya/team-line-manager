import * as React from 'react';
import * as ContextPrimitive from '@radix-ui/react-context-menu';
import { ChevronRight } from 'lucide-react';
import { cn } from '@/lib/utils';

// 右键菜单：外观与下拉菜单共用 .menu / .menu-item（components.css），同一份菜单定义可以同时渲染成两种。离视口边留 8px 同下拉（overlays.tsx）。
export const ContextMenu = ContextPrimitive.Root;
export const ContextMenuTrigger = ContextPrimitive.Trigger;
export const ContextMenuContent = React.forwardRef<React.ElementRef<typeof ContextPrimitive.Content>, React.ComponentPropsWithoutRef<typeof ContextPrimitive.Content>>(({ className, ...p }, ref) => (
  <ContextPrimitive.Portal><ContextPrimitive.Content ref={ref} collisionPadding={8} className={cn('menu', className)} {...p} /></ContextPrimitive.Portal>
));
ContextMenuContent.displayName = 'ContextMenuContent';
export const ContextMenuItem = React.forwardRef<React.ElementRef<typeof ContextPrimitive.Item>, React.ComponentPropsWithoutRef<typeof ContextPrimitive.Item> & { danger?: boolean }>(({ className, danger, ...p }, ref) => (
  <ContextPrimitive.Item ref={ref} className={cn('menu-item', danger && 'is-danger', className)} {...p} />
));
ContextMenuItem.displayName = 'ContextMenuItem';
export const ContextMenuSeparator = () => <ContextPrimitive.Separator className="menu-sep" />;
export const ContextMenuSub = ContextPrimitive.Sub;
export const ContextMenuSubTrigger = React.forwardRef<React.ElementRef<typeof ContextPrimitive.SubTrigger>, React.ComponentPropsWithoutRef<typeof ContextPrimitive.SubTrigger>>(({ className, children, ...p }, ref) => (
  <ContextPrimitive.SubTrigger ref={ref} className={cn('menu-item', className)} {...p}>{children}<ChevronRight className="menu-chev" /></ContextPrimitive.SubTrigger>
));
ContextMenuSubTrigger.displayName = 'ContextMenuSubTrigger';
export const ContextMenuSubContent = React.forwardRef<React.ElementRef<typeof ContextPrimitive.SubContent>, React.ComponentPropsWithoutRef<typeof ContextPrimitive.SubContent>>(({ className, ...p }, ref) => (
  <ContextPrimitive.Portal><ContextPrimitive.SubContent ref={ref} collisionPadding={8} className={cn('menu', className)} {...p} /></ContextPrimitive.Portal>
));
ContextMenuSubContent.displayName = 'ContextMenuSubContent';
