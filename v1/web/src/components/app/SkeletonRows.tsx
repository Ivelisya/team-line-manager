import { cn } from '@/lib/utils';
import { Skeleton } from '@/components/ui/skeleton';

/** 列表 / 表格加载中的几行骨架：每行与表格行同高、带底线（one = 单行表 --row-1，默认 --row-2），数据到了不跳。
 *  head：骨架替换整张表时（表头也还没渲染）先占一条表头；list：非表格处（树、卡片列表、对话框）用的紧凑样式，不画行线 */
export function SkeletonRows({ rows = 4, one, head, list }: { rows?: number; one?: boolean; head?: boolean; list?: boolean }) {
  const bar = (i: number) => <Skeleton key={i} className="h-3.5" style={{ width: `${88 - ((i * 17) % 40)}%` }} />;
  if (list) {
    return <div role="status" aria-label="加载中" className="grid gap-3 p-4">{Array.from({ length: rows }, (_, i) => bar(i))}</div>;
  }
  return (
    <div role="status" aria-label="加载中">
      {head && <div className="h-(--thead-h) border-b border-line bg-sunken" />}
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className={cn('box-border flex items-center border-b border-line px-(--cell-edge) last:border-b-0', one ? 'h-(--row-1)' : 'h-(--row-2)')}>{bar(i)}</div>
      ))}
    </div>
  );
}
