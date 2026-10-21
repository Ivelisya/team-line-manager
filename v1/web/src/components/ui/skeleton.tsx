import { cn } from '@/lib/utils';

/** 骨架占位：统一的脉动块（.skel）。w/h 可以是数字（px）或任意 CSS 长度 */
export function Skeleton({ w = '100%', h = 14, className, style }: { w?: number | string; h?: number | string; className?: string; style?: React.CSSProperties }) {
  return <span className={cn('skel', className)} aria-hidden="true" style={{ width: w, height: h, ...style }} />;
}

/** 列表/表格加载中的几行骨架：每行与表格行同高、带底线（one = 单行表 --row-1，默认 --row-2），数据到了不跳。
 *  head：骨架替换整张表时（表头也还没渲染）先占一条表头；list：非表格处（树、卡片列表、对话框）用的紧凑样式，不画行线 */
export function SkeletonRows({ rows = 4, one, head, list }: { rows?: number; one?: boolean; head?: boolean; list?: boolean }) {
  if (list) {
    return (
      <div role="status" aria-label="加载中" style={{ display: 'grid', gap: 12, padding: 16 }}>
        {Array.from({ length: rows }, (_, i) => <Skeleton key={i} w={`${88 - ((i * 17) % 40)}%`} h={14} />)}
      </div>
    );
  }
  return (
    <div role="status" aria-label="加载中">
      {head && <div className="skel-head" />}
      {Array.from({ length: rows }, (_, i) => <div key={i} className={cn('skel-row', one && 'is-one')}><Skeleton w={`${88 - ((i * 17) % 40)}%`} h={14} /></div>)}
    </div>
  );
}
