import * as React from 'react';
import { Slot } from '@radix-ui/react-slot';
import { cva, type VariantProps } from 'class-variance-authority';
import { Loader2 } from 'lucide-react';
import { cn } from '@/lib/utils';
import { Tip } from './overlays';

// shadcn 写法（cva + Slot），样式类来自原版 components.css 的 .btn 家族
const buttonVariants = cva('btn', {
  variants: {
    variant: { primary: 'btn-primary', secondary: 'btn-secondary', quiet: 'btn-quiet', text: 'btn-text', link: 'btn-link', danger: 'btn-danger', 'outline-danger': 'btn-outline-danger' },
    size: { lg: '', md: 'btn-md', sm: 'btn-sm' },
  },
  defaultVariants: { variant: 'secondary', size: 'md' },
});

export interface ButtonProps extends React.ButtonHTMLAttributes<HTMLButtonElement>, VariantProps<typeof buttonVariants> {
  asChild?: boolean; loading?: boolean;
}
export const Button = React.forwardRef<HTMLButtonElement, ButtonProps>(({ className, variant, size, asChild, loading, children, disabled, type, ...p }, ref) => {
  const Comp = asChild ? Slot : 'button';
  return (
    <Comp ref={ref} type={asChild ? undefined : type ?? 'button'} className={cn(buttonVariants({ variant, size }), className)} disabled={disabled || loading} aria-busy={loading || undefined} {...p}>
      {asChild ? children : <>{loading && <Loader2 className="spin" />}{children}</>}
    </Comp>
  );
});
Button.displayName = 'Button';

/** 图标按钮：label 同时是无障碍名称和悬停提示（统一的 Tooltip，不用浏览器自带的 title）。tip=false 可关闭提示 */
export const IconButton = React.forwardRef<HTMLButtonElement, React.ButtonHTMLAttributes<HTMLButtonElement> & { label: string; size?: 'sm' | 'md' | 'lg'; tip?: boolean; side?: 'top' | 'bottom' | 'left' | 'right' }>(
  ({ className, label, size = 'md', type, tip = true, side, ...p }, ref) => (
    <Tip text={tip ? label : null} side={side}>
      <button ref={ref} type={type ?? 'button'} aria-label={label} className={cn('icon-btn', size === 'sm' && 'icon-btn-sm', size === 'lg' && 'icon-btn-lg', className)} {...p} />
    </Tip>
  ),
);
IconButton.displayName = 'IconButton';
