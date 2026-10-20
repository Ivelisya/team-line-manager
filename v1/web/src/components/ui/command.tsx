import * as React from 'react';
import * as DialogPrimitive from '@radix-ui/react-dialog';
import { Command as Cmdk } from 'cmdk';
import { Search } from 'lucide-react';
import { cn } from '@/lib/utils';

// shadcn 的 Command 写法：cmdk 提供过滤/键盘导航，外观全部走 .cmd-* 类（令牌来自 tokens.css）。
export const Command = React.forwardRef<React.ElementRef<typeof Cmdk>, React.ComponentPropsWithoutRef<typeof Cmdk>>(({ className, ...p }, ref) => (
  <Cmdk ref={ref} className={cn('cmd', className)} {...p} />
));
Command.displayName = 'Command';

/** 命令面板对话框：遮罩 + 顶部居中的面板（宽 --palette-w），层级 --z-palette */
export function CommandDialog({ open, onOpenChange, title, children }: { open: boolean; onOpenChange: (o: boolean) => void; title: string; children: React.ReactNode }) {
  return (
    <DialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay className="overlay cmd-overlay" />
        <DialogPrimitive.Content className="cmd-dialog" aria-describedby={undefined}>
          <DialogPrimitive.Title className="sr-only">{title}</DialogPrimitive.Title>
          {children}
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}

export const CommandInput = React.forwardRef<React.ElementRef<typeof Cmdk.Input>, React.ComponentPropsWithoutRef<typeof Cmdk.Input>>(({ className, ...p }, ref) => (
  <div className="cmd-input-wrap">
    <Search size={16} />
    <Cmdk.Input ref={ref} className={cn('cmd-input', className)} {...p} />
  </div>
));
CommandInput.displayName = 'CommandInput';

export const CommandList = React.forwardRef<React.ElementRef<typeof Cmdk.List>, React.ComponentPropsWithoutRef<typeof Cmdk.List>>(({ className, ...p }, ref) => (
  <Cmdk.List ref={ref} className={cn('cmd-list', className)} {...p} />
));
CommandList.displayName = 'CommandList';

export const CommandEmpty = React.forwardRef<React.ElementRef<typeof Cmdk.Empty>, React.ComponentPropsWithoutRef<typeof Cmdk.Empty>>(({ className, ...p }, ref) => (
  <Cmdk.Empty ref={ref} className={cn('cmd-empty', className)} {...p} />
));
CommandEmpty.displayName = 'CommandEmpty';

export const CommandGroup = React.forwardRef<React.ElementRef<typeof Cmdk.Group>, React.ComponentPropsWithoutRef<typeof Cmdk.Group>>(({ className, ...p }, ref) => (
  <Cmdk.Group ref={ref} className={cn('cmd-group', className)} {...p} />
));
CommandGroup.displayName = 'CommandGroup';

export const CommandItem = React.forwardRef<React.ElementRef<typeof Cmdk.Item>, React.ComponentPropsWithoutRef<typeof Cmdk.Item>>(({ className, ...p }, ref) => (
  <Cmdk.Item ref={ref} className={cn('cmd-item', className)} {...p} />
));
CommandItem.displayName = 'CommandItem';

