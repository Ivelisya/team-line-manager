import * as React from 'react';
import * as HoverPrimitive from '@radix-ui/react-hover-card';
import { cn } from '@/lib/utils';

// 悬浮卡片：停留 350ms 后出现，移开 120ms 后收起；外观是浮层 .popover 加宽一点。
export function HoverCard({ openDelay = 350, closeDelay = 120, ...p }: React.ComponentProps<typeof HoverPrimitive.Root>) {
  return <HoverPrimitive.Root openDelay={openDelay} closeDelay={closeDelay} {...p} />;
}
export const HoverCardTrigger = HoverPrimitive.Trigger;
export const HoverCardContent = React.forwardRef<React.ElementRef<typeof HoverPrimitive.Content>, React.ComponentPropsWithoutRef<typeof HoverPrimitive.Content>>(({ className, sideOffset = 8, align = 'start', ...p }, ref) => (
  <HoverPrimitive.Portal><HoverPrimitive.Content ref={ref} sideOffset={sideOffset} align={align} collisionPadding={12} className={cn('popover hover-card', className)} {...p} /></HoverPrimitive.Portal>
));
HoverCardContent.displayName = 'HoverCardContent';
