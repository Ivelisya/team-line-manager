// 分页器（唯一实现），样式参照 brocade 的目录分页：一条底栏——左边「25–48 / 161 台 · 每页 [24]」，
// 右边「第 [2] / 7 页」+「上一页 / 下一页」。配 lib/usePage.ts 使用。
// 放在面板里是表尾（顶线 + 浅底），放在卡片网格下面是一条独立的圆角条（同筛选条）。
// 另有两个小件：CursorPager（按时间游标的「较新 / 较早」翻页）、ShowAll（短表格只露前几行，「显示全部 N 条」展开）。
import { memo, useCallback, useMemo } from 'react';
import { ChevronLeft, ChevronRight } from 'lucide-react';
import type { PageState } from '@/lib/usePage';
import { OptionSelect } from '@/components/ui/option-select';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';

const n = (x: number) => x.toLocaleString('zh-CN');

/** compact：窄栏里用（作业列表），只留范围和上一页 / 下一页。
 *  memo：pg 由 usePage 按值缓存，页面随推送重渲染时分页器和里面两个下拉（Select 也是 memo，选项、回调都稳定）不跟着重画 */
export const Pagination = memo(function Pagination({ pg, unit = '项', compact, className }: { pg: PageState; unit?: string; compact?: boolean; className?: string }) {
  const { page, pageCount, total, size, sizes, defaultSize, offset, setPage, setSize } = pg;
  const pages = useMemo(() => Array.from({ length: pageCount }, (_, i) => ({ value: String(i + 1), label: String(i + 1) })), [pageCount]);
  const sizeOpts = useMemo(() => sizes.map((n) => ({ value: String(n), label: String(n) })), [sizes]);
  const onSize = useCallback((v: string) => setSize(Number(v)), [setSize]);
  const onPage = useCallback((v: string) => setPage(Number(v)), [setPage]);
  if (total == null || (total <= size && size <= defaultSize)) return null; // 默认条数一页就放得下：不出分页器（调大过条数的仍显示，好调回来）
  return (
    <nav className={cn('pager', className)} aria-label="分页">
      <span className="pager-range" aria-live="polite">
        <span className="num">{total ? `${n(offset + 1)}–${n(Math.min(offset + size, total))}` : '0'}</span><span className="pager-of">/</span><span className="num">{n(total)}</span> {unit}
      </span>
      {!compact && sizes.length > 1 && (
        <span className="pager-field">每页
          <OptionSelect sm value={String(size)} onChange={onSize} options={sizeOpts} aria-label="每页条数" className="w-16 font-num" />
        </span>
      )}
      <span className="pager-nav">
        {!compact && pageCount > 1 && (
          <span className="pager-field">第
            <OptionSelect sm value={String(page)} onChange={onPage} options={pages} aria-label="页码" className="w-16 font-num" />
            / <span className="num">{n(pageCount)}</span> 页
          </span>
        )}
        <Button variant="outline" size="sm" className="min-w-18" disabled={page <= 1} onClick={() => setPage(page - 1)}><ChevronLeft />上一页</Button>
        <Button variant="outline" size="sm" className="min-w-18" disabled={page >= pageCount} onClick={() => setPage(page + 1)}>下一页<ChevronRight /></Button>
      </span>
    </nav>
  );
});

/** 时间游标翻页：没有总数，只能往前（较新）/ 往后（较早）走；按钮写法同分页器（上一页 = 较新） */
export function CursorPager({ page, size, unit = '条', loading = false, onNewer, onOlder }: { page: number; size: number; unit?: string; loading?: boolean; onNewer?: () => void; onOlder?: () => void }) {
  if (!onNewer && !onOlder) return null;
  return (
    <nav className="pager" aria-label="分页">
      <span className="pager-range">第 <span className="num">{n(page)}</span> 页 · 每页 <span className="num">{size}</span> {unit}</span>
      <span className="pager-nav">
        <Button variant="outline" size="sm" className="min-w-18" disabled={!onNewer || loading} onClick={onNewer}><ChevronLeft />上一页</Button>
        <Button variant="outline" size="sm" className="min-w-18" disabled={!onOlder || loading} loading={loading} onClick={onOlder}>下一页<ChevronRight /></Button>
      </span>
    </nav>
  );
}

/** 表格只先露前几行：超出时在表尾给「显示全部 N 条 / 收起」 */
export function ShowAll({ total, cap, open, onToggle, unit = '条' }: { total: number; cap: number; open: boolean; onToggle: () => void; unit?: string }) {
  if (total <= cap) return null;
  return (
    <div className="dt-foot is-center">
      <Button size="sm" variant="ghost" aria-expanded={open} onClick={onToggle}>{open ? '收起' : `显示全部 ${total} ${unit}`}</Button>
    </div>
  );
}
