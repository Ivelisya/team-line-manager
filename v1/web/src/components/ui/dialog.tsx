import * as React from 'react';
import * as DialogPrimitive from '@radix-ui/react-dialog';
import { X } from 'lucide-react';
import { cn } from '@/lib/utils';
import { IconButton } from './button';

export const Dialog = DialogPrimitive.Root;

type OutsideHandler = React.ComponentPropsWithoutRef<typeof DialogPrimitive.Content>['onPointerDownOutside'];
/** 点通知（toast，挂在弹层外）不算点外面：关掉一条通知不该把对话框、抽屉一起关掉 */
const keepOnToast = (cb: OutsideHandler): OutsideHandler => (e) => {
  if ((e.target as Element | null)?.closest?.('.toasts')) e.preventDefault();
  cb?.(e);
};

// 毛玻璃遮罩开着时在 <html> 上挂 data-overlay（有对话框 / 命令面板为 "dialog"，只有抽屉为 "drawer"），
// extras.css 据此停掉下面一层的循环动画和读数过渡。以前写成 body:has(.overlay[data-state="open"]) …：
// :has 的参数里有 [data-state]，悬停提示、菜单开合一改 data-state，浏览器就要把整页几千个元素重新匹配一遍。
// 计数而不是开关：对话框叠在抽屉上、关一个开一个交替时不会被提前清掉。遮罩播退场动画期间仍算开着（模糊还在）
const shown = { dialog: 0, drawer: 0 };
const syncFlag = () => {
  const d = document.documentElement.dataset;
  if (shown.dialog) d.overlay = 'dialog'; else if (shown.drawer) d.overlay = 'drawer'; else delete d.overlay;
};
/** 遮罩：Radix 只在打开和退场动画期间挂载它，挂着就登记。ref 要交给 Overlay，Presence 靠它等退场动画播完 */
export const Scrim = React.forwardRef<HTMLDivElement, { className: string; kind: keyof typeof shown }>(({ className, kind }, ref) => {
  React.useLayoutEffect(() => {
    shown[kind]++; syncFlag();
    return () => { shown[kind]--; syncFlag(); };
  }, [kind]);
  return <DialogPrimitive.Overlay ref={ref} className={className} />;
});
Scrim.displayName = 'Scrim';

/** 对话框：size sm 440 / md 660 / lg 760（原版对话框三档） */
export const DialogContent = React.forwardRef<React.ElementRef<typeof DialogPrimitive.Content>, React.ComponentPropsWithoutRef<typeof DialogPrimitive.Content> & { size?: 'sm' | 'md' | 'lg' }>(
  ({ className, size = 'sm', children, onPointerDownOutside, ...p }, ref) => (
    <DialogPrimitive.Portal>
      <Scrim className="overlay" kind="dialog" />
      <DialogPrimitive.Content ref={ref} className={cn('dialog', size === 'md' && 'dialog-md', size === 'lg' && 'dialog-lg', className)} onPointerDownOutside={keepOnToast(onPointerDownOutside)} {...p}>
        {children}
      </DialogPrimitive.Content>
    </DialogPrimitive.Portal>
  ),
);
DialogContent.displayName = 'DialogContent';

export const DialogTitle = React.forwardRef<React.ElementRef<typeof DialogPrimitive.Title>, React.ComponentPropsWithoutRef<typeof DialogPrimitive.Title>>(({ className, ...p }, ref) => (
  <DialogPrimitive.Title ref={ref} className={cn('dialog-title', className)} {...p} />
));
DialogTitle.displayName = 'DialogTitle';
export const DialogDescription = React.forwardRef<React.ElementRef<typeof DialogPrimitive.Description>, React.ComponentPropsWithoutRef<typeof DialogPrimitive.Description>>(({ className, ...p }, ref) => (
  <DialogPrimitive.Description ref={ref} className={cn('dialog-desc', className)} {...p} />
));
DialogDescription.displayName = 'DialogDescription';
/** 标题 + 说明的头部块（标题与说明之间固定的小间距） */
export const DialogHeader = ({ className, ...p }: React.HTMLAttributes<HTMLDivElement>) => <div className={cn('dialog-head', className)} {...p} />;
export const DialogFooter = ({ className, ...p }: React.HTMLAttributes<HTMLDivElement>) => <div className={cn('dialog-foot', className)} {...p} />;

/** 右侧抽屉（shadcn Sheet 的写法：Dialog + 右侧定位） */
export const Sheet = DialogPrimitive.Root;
export const SheetContent = React.forwardRef<React.ElementRef<typeof DialogPrimitive.Content>, React.ComponentPropsWithoutRef<typeof DialogPrimitive.Content> & { hideClose?: boolean }>(
  ({ className, children, hideClose, onPointerDownOutside, ...p }, ref) => (
    <DialogPrimitive.Portal>
      <Scrim className="drawer-overlay" kind="drawer" />
      <DialogPrimitive.Content ref={ref} className={cn('drawer', className)} onOpenAutoFocus={(e) => e.preventDefault()} onPointerDownOutside={keepOnToast(onPointerDownOutside)} {...p}>
        {children}
        {!hideClose && (
          <DialogPrimitive.Close asChild>
            <IconButton label="关闭" className="" style={{ position: 'absolute', top: 12, right: 12 }}><X size={16} /></IconButton>
          </DialogPrimitive.Close>
        )}
      </DialogPrimitive.Content>
    </DialogPrimitive.Portal>
  ),
);
SheetContent.displayName = 'SheetContent';
export const SheetTitle = DialogTitle;
export const SheetDescription = DialogDescription;
