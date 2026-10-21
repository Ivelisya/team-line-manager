import * as React from 'react';
import * as DialogPrimitive from '@radix-ui/react-dialog';
import { X } from 'lucide-react';
import { cn } from '@/lib/utils';
import { durMs } from '@/lib/motion';
import { IconButton } from './button';

export const Dialog = DialogPrimitive.Root;

type OutsideHandler = React.ComponentPropsWithoutRef<typeof DialogPrimitive.Content>['onPointerDownOutside'];
/** 点通知（toast，挂在弹层外）不算点外面：关掉一条通知不该把对话框、抽屉一起关掉 */
const keepOnToast = (cb: OutsideHandler): OutsideHandler => (e) => {
  if ((e.target as Element | null)?.closest?.('.toasts')) e.preventDefault();
  cb?.(e);
};

type InteractHandler = React.ComponentPropsWithoutRef<typeof DialogPrimitive.Content>['onInteractOutside'];
/** 点遮罩想关、却被调用方拦下（有未保存的改动、新密钥还没复制）：面板轻轻回弹一下（1 → 1.02 → 1，--dur-2 + --ease-out），
 *  告诉用户“点到了，但不会关”，不再毫无反应。只认调用方拦下的主键点击——Radix 自己拦的右键、焦点移出和点通知（keepOnToast）在调用方之前就已拦下。
 *  减弱动效时 --dur-2 为 0，不弹。缩放用独立的 scale 属性，与居中的 translate、进出场的 transform 互不干扰 */
const bounceWhenKept = (cb: InteractHandler, panel: React.RefObject<HTMLDivElement | null>): InteractHandler => (e) => {
  const before = e.defaultPrevented;
  cb?.(e);
  if (before || !e.defaultPrevented || e.detail.originalEvent.type !== 'pointerdown') return;
  const ms = durMs('--dur-2');
  if (ms > 0) panel.current?.animate([{ scale: '1' }, { scale: '1.02' }, { scale: '1' }], { duration: ms, easing: getComputedStyle(document.documentElement).getPropertyValue('--ease-out').trim() });
};

// 毛玻璃遮罩（对话框、命令面板）开着时在 <html> 上挂 data-overlay，extras.css 据此停掉下面一层（页面、抽屉）的循环动画和读数过渡，
// KPI 数字也直接到位（useCountUp）。以前写成 body:has(.overlay[data-state="open"]) …：
// :has 的参数里有 [data-state]，悬停提示、菜单开合一改 data-state，浏览器就要把整页几千个元素重新匹配一遍。
// 计数而不是开关：对话框叠对话框、关一个开一个交替时不会被提前清掉。遮罩播退场动画期间仍算开着（模糊还在）。
// 节点抽屉的遮罩只压暗不模糊，不挂这个开关
let shown = 0;
/** 对话框、命令面板的遮罩：Radix 只在打开和退场动画期间挂载它，挂着就登记。ref 要交给 Overlay，Presence 靠它等退场动画播完 */
export const Scrim = React.forwardRef<HTMLDivElement, { className: string }>(({ className }, ref) => {
  React.useLayoutEffect(() => {
    if (shown++ === 0) document.documentElement.dataset.overlay = '';
    return () => { if (--shown === 0) delete document.documentElement.dataset.overlay; };
  }, []);
  return <DialogPrimitive.Overlay ref={ref} className={className} />;
});
Scrim.displayName = 'Scrim';

/** 对话框：size sm 440 / md 660 / lg 760（原版对话框三档） */
export const DialogContent = React.forwardRef<React.ElementRef<typeof DialogPrimitive.Content>, React.ComponentPropsWithoutRef<typeof DialogPrimitive.Content> & { size?: 'sm' | 'md' | 'lg' }>(
  ({ className, size = 'sm', children, onPointerDownOutside, onInteractOutside, ...p }, ref) => {
    const panel = React.useRef<HTMLDivElement>(null);
    React.useImperativeHandle(ref, () => panel.current!, []);
    return (
      <DialogPrimitive.Portal>
        <Scrim className="overlay" />
        <DialogPrimitive.Content ref={panel} className={cn('dialog', size === 'md' && 'dialog-md', size === 'lg' && 'dialog-lg', className)}
          onPointerDownOutside={keepOnToast(onPointerDownOutside)} onInteractOutside={bounceWhenKept(onInteractOutside, panel)} {...p}>
          {children}
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    );
  },
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
      <DialogPrimitive.Overlay className="drawer-overlay" />
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
