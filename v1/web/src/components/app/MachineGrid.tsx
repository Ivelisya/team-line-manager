// 机器表格（一行一台）的公共部分：列定义、表头排序的比较函数、行组件与整张表。
// 首页“表格”视图（features/MachineTable）和机器页（pages/Nodes）各自只写自己的列。
import * as React from 'react';
import type { MachineView } from '@/lib/types';
import { isQuiet } from '@/lib/derive';
import { collator } from '@/lib/sort';
import { watchSize } from '@/lib/sharedObserver';
import { cn } from '@/lib/utils';
import { SortHead, type SortState } from './primitives';
import { NodeMenus } from './NodeMenu';

type Cmp = (a: MachineView, b: MachineView) => number;
/** cmp（升序）+ first（首次点击的方向：数值多为先降序，名称、日期先升序）都有才能点表头排序；r：右对齐；
 *  nil：这一行在这列没有值（没有订阅的到期、费用），不论升序降序都排在最后；
 *  drop：容器放不下全部列时可以收起，数字小的先收（没有的列一直显示） */
export interface Col { id: string; header: string; size: number; r?: boolean; first?: 'asc' | 'desc'; cmp?: Cmp; nil?: (m: MachineView) => boolean; drop?: number; cell: (m: MachineView) => React.ReactNode }
export const byStr = (f: (m: MachineView) => string) => ({ first: 'asc' as const, cmp: (a: MachineView, b: MachineView) => collator.compare(f(a), f(b)) });
/** f 返回 null 的行算没有值（nil） */
export const byNum = (f: (m: MachineView) => number | null, first: 'asc' | 'desc' = 'desc') =>
  ({ first, cmp: (a: MachineView, b: MachineView) => (f(a) ?? 0) - (f(b) ?? 0), nil: (m: MachineView) => f(m) == null });

/** 按表头排序：稳定排序，相同值保持原来的（默认）顺序，没有值的行排在最后；没有排序时原样返回 */
export function sortByCol(rows: MachineView[], cols: Col[], sort: SortState | null): MachineView[] {
  const col = cols.find((c) => c.id === sort?.id);
  if (!sort || !col?.cmp) return rows;
  const { cmp, nil } = col;
  const sign = sort.desc ? -1 : 1;
  return [...rows].sort((a, b) => (nil ? Number(nil(a)) - Number(nil(b)) : 0) || sign * cmp(a, b));
}

/** 可以收起的列，按收起的先后（drop 小的先收）；正在按它排序的列不收，排序标记和取消排序的表头一直看得见 */
const droppable = (cols: Col[], sortId: string | undefined) => cols.filter((c) => c.drop != null && c.id !== sortId).sort((a, b) => a.drop! - b.drop!);
/** 按容器宽度要收起几列才放得下，以及收起后放不放得下；全部收起仍放不下就不收，横滑 */
function fitCols(cols: Col[], order: Col[], width: number) {
  let w = cols.reduce((s, c) => s + c.size, 0);
  let n = 0;
  while (w > width && n < order.length) w -= order[n++].size;
  return w > width ? { n: 0, fit: false } : { n, fit: true };
}

/** 整张表：固定列宽（colgroup，最小宽度为显示列的列宽合计），点行 / 回车打开抽屉，右键菜单整个 tbody 共用一份（NodeMenus）。
 *  容器窄时先收起次要列（Col.drop），收完放得下就不横滑；收起几列只在跨过阈值时变，其余宽度变化不重渲染。
 *  dim：哪些行调淡（首页是数据过期的，机器页都不调淡）；须是模块常量，行是 memo 的。
 *  sig：列里不显示实时读数的表（机器页）给一份行签名（模块常量），推送只换了读数、签名没变的行不重渲染。
 *  放得下（不用横滑）时容器带 data-fit，表头吸顶（table.css）；随宽度变化直接改属性 */
export function MachineGrid({ cols, rows, sort, onSort, openId, onOpen, dim = isQuiet, sig, className }: {
  cols: Col[]; rows: MachineView[]; sort: SortState | null; onSort: (s: SortState | null) => void;
  openId: string | null; onOpen: (id: string) => void; dim?: (m: MachineView) => boolean; sig?: (m: MachineView) => string; className?: string;
}) {
  const wrap = React.useRef<HTMLDivElement>(null);
  const [dropped, setDropped] = React.useState(0);
  const order = React.useMemo(() => droppable(cols, sort?.id), [cols, sort?.id]);
  // 挂载、换列时先在绘制前按当前宽度算一次，不先露出全部列和横向滚动条再收；之后的宽度变化交给共享的 ResizeObserver
  React.useLayoutEffect(() => {
    const el = wrap.current!;
    const apply = (w: number) => { const f = fitCols(cols, order, w); el.toggleAttribute('data-fit', f.fit); setDropped(f.n); };
    apply(el.clientWidth);
    return watchSize(el, (e) => apply(e.contentRect.width));
  }, [cols, order]);
  const shown = React.useMemo(() => {
    if (!dropped) return cols;
    const gone = new Set(order.slice(0, dropped).map((c) => c.id));
    return cols.filter((c) => !gone.has(c.id));
  }, [cols, order, dropped]);
  const minWidth = shown.reduce((s, c) => s + c.size, 0);
  // 抽屉关上后焦点回到打开它的那一行（抽屉是非模态的，Radix 不还焦点）；焦点已被放到抽屉外别处的不抢
  const lastOpen = React.useRef(openId);
  React.useEffect(() => {
    const prev = lastOpen.current; lastOpen.current = openId;
    if (!prev || openId) return;
    const a = document.activeElement;
    if (a && a !== document.body && !a.closest('.drawer')) return;
    wrap.current?.querySelector<HTMLElement>(`tr[data-id="${CSS.escape(prev)}"]`)?.focus({ preventScroll: true });
  }, [openId]);
  return (
    <div ref={wrap} className={cn('table-wrap', className)}>
      <table className="dt" style={{ minWidth }}>
        <colgroup>{shown.map((c) => <col key={c.id} style={{ width: c.size }} />)}</colgroup>
        <SortHead cols={shown} sort={sort} onSort={onSort} />
        <NodeMenus rows={rows}><tbody>{rows.map((m) => <Row key={m.id} m={m} current={m.id === openId} onOpen={onOpen} cols={shown} dim={dim} sig={sig} />)}</tbody></NodeMenus>
      </table>
    </div>
  );
}

type RowProps = { m: MachineView; current: boolean; onOpen: (id: string) => void; cols: Col[]; dim: (m: MachineView) => boolean; sig?: (m: MachineView) => string };
/** 一行一个 memo 就挡住了：实时推送只换变了的那台机器的引用，其余行不重渲染（onOpen 来自 useDrawer、cols 是常量或按范围 memo，都稳定）；
 *  给了 sig 时机器换了引用但签名没变也不重渲染 */
const sameRow = (a: RowProps, b: RowProps) => a.current === b.current && a.onOpen === b.onOpen && a.cols === b.cols && a.dim === b.dim && a.sig === b.sig
  && (a.m === b.m || (!!b.sig && b.sig(a.m) === b.sig(b.m)));
/** 右键菜单按 data-id 找到这一行 */
const Row = React.memo(function Row({ m, current, onOpen, cols, dim }: RowProps) {
  return (
    <tr className={cn('is-link', current && 'is-current', dim(m) && 'is-quiet')} tabIndex={0} data-id={m.id}
      onClick={(e) => { if (!(e.target as Element).closest('[data-node-menu]')) onOpen(m.id); }} onKeyDown={(e) => { if (e.key === 'Enter' && e.target === e.currentTarget) onOpen(m.id); }}>
      {cols.map((c) => <td key={c.id} className={c.id === 'chev' ? 'chev' : c.id === 'menu' ? 'c' : c.r ? 'r' : undefined}>{c.cell(m)}</td>)}
    </tr>
  );
}, sameRow);
