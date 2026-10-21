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

/** 对话框：size sm 440 / md 660 / lg 760（原版对话框三档） */
export const DialogContent = React.forwardRef<React.ElementRef<typeof DialogPrimitive.Content>, React.ComponentPropsWithoutRef<typeof DialogPrimitive.Content> & { size?: 'sm' | 'md' | 'lg' }>(
  ({ className, size = 'sm', children, onPointerDownOutside, ...p }, ref) => (
    <DialogPrimitive.Portal>
      <DialogPrimitive.Overlay className="overlay" />
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
