// 机器表格（一行一台）的公共部分：列定义、表头排序的比较函数、行组件与整张表。
// 首页“表格”视图（features/MachineTable）和机器页（pages/Nodes）各自只写自己的列。
import * as React from 'react';
import type { MachineView } from '@/lib/types';
import { isQuiet } from '@/lib/derive';
import { collator } from '@/lib/sort';
import { cn } from '@/lib/utils';
import { SortHead, type SortState } from './primitives';
import { NodeMenus } from './NodeMenu';

type Cmp = (a: MachineView, b: MachineView) => number;
/** cmp（升序）+ first（首次点击的方向：数值多为先降序，名称、日期先升序）都有才能点表头排序；r：右对齐 */
export interface Col { id: string; header: string; size: number; r?: boolean; first?: 'asc' | 'desc'; cmp?: Cmp; cell: (m: MachineView) => React.ReactNode }
export const byStr = (f: (m: MachineView) => string) => ({ first: 'asc' as const, cmp: (a: MachineView, b: MachineView) => collator.compare(f(a), f(b)) });
export const byNum = (f: (m: MachineView) => number, first: 'asc' | 'desc' = 'desc') => ({ first, cmp: (a: MachineView, b: MachineView) => f(a) - f(b) });

/** 按表头排序：稳定排序，相同值保持原来的（默认）顺序；没有排序时原样返回 */
export function sortByCol(rows: MachineView[], cols: Col[], sort: SortState | null): MachineView[] {
  const cmp = cols.find((c) => c.id === sort?.id)?.cmp;
  if (!sort || !cmp) return rows;
  const sign = sort.desc ? -1 : 1;
  return [...rows].sort((a, b) => sign * cmp(a, b));
}

/** 整张表：固定列宽（colgroup，最小宽度为列宽合计），点行 / 回车打开抽屉，右键菜单整个 tbody 共用一份（NodeMenus）。
 *  dim：哪些行调淡（首页是数据过期的，机器页只调淡已退役的）；须是模块常量，行是 memo 的 */
export function MachineGrid({ cols, rows, sort, onSort, openId, onOpen, dim = isQuiet, className }: {
  cols: Col[]; rows: MachineView[]; sort: SortState | null; onSort: (s: SortState | null) => void;
  openId: string | null; onOpen: (id: string) => void; dim?: (m: MachineView) => boolean; className?: string;
}) {
  return (
    <div className={cn('table-wrap', className)}>
      <table className="dt" style={{ minWidth: cols.reduce((s, c) => s + c.size, 0) }}>
        <colgroup>{cols.map((c) => <col key={c.id} style={{ width: c.size }} />)}</colgroup>
        <SortHead cols={cols} sort={sort} onSort={onSort} />
        <NodeMenus rows={rows}><tbody>{rows.map((m) => <Row key={m.id} m={m} current={m.id === openId} onOpen={onOpen} cols={cols} dim={dim} />)}</tbody></NodeMenus>
      </table>
    </div>
  );
}

/** 一行一个 memo：实时推送只换变了的那台机器的引用，其余行不重渲染（onOpen 来自 useDrawer、cols 是常量或按范围 memo，都稳定）。
 *  右键菜单按 data-id 找到这一行 */
const Row = React.memo(function Row({ m, current, onOpen, cols, dim }: { m: MachineView; current: boolean; onOpen: (id: string) => void; cols: Col[]; dim: (m: MachineView) => boolean }) {
  return (
    <tr className={cn('is-link', current && 'is-current', dim(m) && 'is-quiet')} tabIndex={0} data-id={m.id}
      onClick={() => onOpen(m.id)} onKeyDown={(e) => { if (e.key === 'Enter' && e.target === e.currentTarget) onOpen(m.id); }}>
      {cols.map((c) => <td key={c.id} className={c.id === 'chev' ? 'chev' : c.r ? 'r' : undefined}>{c.cell(m)}</td>)}
    </tr>
  );
});
