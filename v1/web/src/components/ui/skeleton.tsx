import { cn } from '@/lib/utils';

/** 骨架占位：统一的脉动块（.skel）。w/h 可以是数字（px）或任意 CSS 长度 */
export function Skeleton({ w = '100%', h = 14, className, style }: { w?: number | string; h?: number | string; className?: string; style?: React.CSSProperties }) {
  return <span className={cn('skel', className)} aria-hidden="true" style={{ width: w, height: h, ...style }} />;
}

/** 列表/表格加载中的几行骨架 */
export function SkeletonRows({ rows = 4, pad = 16 }: { rows?: number; pad?: number }) {
  return (
    <div role="status" aria-label="加载中" style={{ display: 'grid', gap: 12, padding: pad }}>
      {Array.from({ length: rows }, (_, i) => <Skeleton key={i} w={`${88 - ((i * 17) % 40)}%`} h={14} />)}
    </div>
  );
}
