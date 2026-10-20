// 首页“表格”视图：与卡片同一份筛选 + 排序 + 分页后的机器，一行一台，密集只读。
// 表头排序由首页持有（排序要在分页之前做），这里只导出列的比较函数 TABLE_SORTS。点行打开抽屉。
import * as React from 'react';
import { ArrowDownUp, ChevronDown, ChevronUp } from 'lucide-react';
import type { MachineView, Plan } from '@/lib/types';
import type { DrawerTab } from '@/lib/drawer';
import * as F from '@/lib/format';
import { isQuiet, monthTraffic, quota, remainingValue, resourceMeters } from '@/lib/derive';
import { KIND_LABEL } from '@/lib/sort';
import { cn } from '@/lib/utils';
import { ContextMenu, ContextMenuContent, ContextMenuTrigger } from '@/components/ui/context-menu';
import { Tip } from '@/components/ui/overlays';
import { KindIcon, Num } from '@/components/app/primitives';
import { NodeContextItems } from '@/components/app/NodeMenu';
import { StatusDot } from '@/components/app/NodeCard';
import { Cells, quotaColors } from '@/components/app/viz';

const Q8 = quotaColors(8);
export interface TableSort { id: string; desc: boolean }
const memPct = (m: MachineView) => (m.host?.mem_total ? (m.host.mem_used ?? 0) / m.host.mem_total : -1);
const diskPct = (m: MachineView) => (m.host?.disk_total ? (m.host.disk_used ?? 0) / m.host.disk_total : -1);
/** 可排序列：首次点击降序（名称升序），再点反向，第三次回到上面选的排序 */
export const TABLE_SORTS: Record<string, (a: MachineView, b: MachineView) => number> = {
  name: (a, b) => a.name.localeCompare(b.name, 'zh-Hans-CN', { numeric: true }),
  cpu: (a, b) => (a.host?.cpu_pct ?? -1) - (b.host?.cpu_pct ?? -1),
  mem: (a, b) => memPct(a) - memPct(b),
  disk: (a, b) => diskPct(a) - diskPct(b),
  up: (a, b) => (a.host?.net_tx_bps ?? -1) - (b.host?.net_tx_bps ?? -1),
  down: (a, b) => (a.host?.net_rx_bps ?? -1) - (b.host?.net_rx_bps ?? -1),
  traffic: (a, b) => { const x = monthTraffic(a); const y = monthTraffic(b); return x.up + x.down - (y.up + y.down); },
  quota: (a, b) => (quota(a).rem ?? Infinity) - (quota(b).rem ?? Infinity),
  due: (a, b) => (a.subscription?.due_days ?? 1e9) - (b.subscription?.due_days ?? 1e9),
  value: (a, b) => (remainingValue(a)?.cents ?? -1) - (remainingValue(b)?.cents ?? -1),
};
export const nextSort = (s: TableSort | null, id: string): TableSort | null => {
  const descFirst = id !== 'name';
  if (s?.id !== id) return { id, desc: descFirst };
  return s.desc === descFirst ? { id, desc: !descFirst } : null;
};

const pctCell = (pct: number | null, metric: 'cpu' | 'mem' | 'disk', quiet: boolean) => (
  <span className="tmeter" data-metric={metric}><Cells pct={quiet ? null : pct} n={8} color={`var(--m-${metric})`} /><Num f={quiet ? F.NA : F.pct(pct)} /></span>
);
const monthly = (m: MachineView) => {
  const s = m.subscription; const n = s && F.CYCLE_MONTHS[s.cycle];
  return s ? (n ? F.money(Math.round(s.price_cents / n), s.currency) : `${F.money(s.price_cents, s.currency)} 一次性`) : null;
};

interface Col { id: string; header: string; size: number; r?: boolean; sort?: boolean; cell: (m: MachineView) => React.ReactNode }
function columns(plans: Plan[]): Col[] {
  return [
    { id: 'name', header: '名称', size: 140, sort: true,
      cell: (m) => <span className="tname"><StatusDot m={m} /><KindIcon kind={m.kind} /><Tip text={KIND_LABEL[m.kind]}><span className="cell-main strong">{m.name}</span></Tip></span> },
    { id: 'ip', header: 'IP', size: 120, cell: (m) => <span className="cell-main t-sub">{m.ip}</span> },
    ...(['cpu', 'mem', 'disk'] as const).map((k): Col => ({ id: k, header: k === 'cpu' ? 'CPU' : k === 'mem' ? '内存' : '磁盘', size: 84, sort: true,
      cell: (m) => { const r = resourceMeters(m, plans); return r ? pctCell(r[k].pct, k, isQuiet(m)) : <span className="t-muted">—</span>; } })),
    ...(['up', 'down'] as const).map((k): Col => ({ id: k, header: k === 'up' ? '上行' : '下行', size: 92, r: true, sort: true,
      cell: (m) => { const v = k === 'up' ? m.host?.net_tx_bps : m.host?.net_rx_bps; return <span className="trate" data-speed={m.host && !isQuiet(m) ? F.speedOf(v) : undefined}><Num f={F.rate(v)} /></span>; } })),
    { id: 'traffic', header: '本月流量', size: 80, r: true, sort: true, cell: (m) => { const t = monthTraffic(m); return <Num f={F.bytes(t.up + t.down)} />; } },
    { id: 'quota', header: '剩余流量', size: 106, sort: true,
      cell: (m) => { const q = quota(m); return <span className="tquota"><Cells pct={q.pct} n={8} colors={Q8} />{q.rem == null ? <span className="t-sub">不限</span> : q.rem > 0 ? <Num f={F.bytes(q.rem)} /> : <span className="tdue is-bad">已超限</span>}</span>; } },
    { id: 'due', header: '距到期', size: 72, r: true, sort: true,
      cell: (m) => { const d = F.dueFmt(m.subscription?.due_days); return <span className={cn('tdue', d.tone && `is-${d.tone}`)}><Num f={d.text} /></span>; } },
    { id: 'value', header: '剩余价值', size: 76, r: true, sort: true, cell: (m) => { const v = remainingValue(m); return v ? <span className="num">{F.money(v.cents, v.currency)}</span> : <span className="t-muted">—</span>; } },
    { id: 'fee', header: '月费', size: 72, r: true, cell: (m) => monthly(m) ?? <span className="t-muted">—</span> },
  ];
}

export function MachineTable({ rows, plans, sort, onSort, openId, onOpen }: { rows: MachineView[]; plans: Plan[]; sort: TableSort | null; onSort: (s: TableSort | null) => void; openId: string | null; onOpen: (id: string, tab?: DrawerTab) => void }) {
  const cols = React.useMemo(() => columns(plans), [plans]);
  const total = cols.reduce((s, c) => s + c.size, 0);
  return (
    <div className="table-wrap mtable">
      <table className="dt is-one" style={{ minWidth: total }}>
        <colgroup>{cols.map((c) => <col key={c.id} style={{ width: c.size }} />)}</colgroup>
        <thead><tr>{cols.map((c) => {
          const dir = sort?.id === c.id ? (sort.desc ? 'desc' : 'asc') : undefined;
          return (
            <th key={c.id} className={c.r ? 'r' : undefined} aria-sort={dir === 'asc' ? 'ascending' : dir === 'desc' ? 'descending' : undefined}>
              {c.sort ? <button type="button" className="th-sort" data-dir={dir} onClick={() => onSort(nextSort(sort, c.id))}>{c.header}{dir === 'asc' ? <ChevronUp /> : dir === 'desc' ? <ChevronDown /> : <ArrowDownUp />}</button> : c.header}
            </th>
          );
        })}</tr></thead>
        <tbody>{rows.map((m) => (
          <ContextMenu key={m.id}>
            <ContextMenuTrigger asChild>
              <tr className={cn('is-link', m.id === openId && 'is-current', isQuiet(m) && 'is-quiet')} tabIndex={0} data-id={m.id}
                onClick={() => onOpen(m.id)} onKeyDown={(e) => { if (e.key === 'Enter' && e.target === e.currentTarget) onOpen(m.id); }}>
                {cols.map((c) => <td key={c.id} className={c.r ? 'r' : undefined}>{c.cell(m)}</td>)}
              </tr>
            </ContextMenuTrigger>
            <ContextMenuContent><NodeContextItems m={m} /></ContextMenuContent>
          </ContextMenu>
        ))}</tbody>
      </table>
    </div>
  );
}
