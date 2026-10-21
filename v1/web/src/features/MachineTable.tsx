// 首页“表格”视图：与卡片同一份筛选 + 排序 + 分页后的机器，一行一台，密集只读。
// 表头排序由首页持有（排序要在分页之前做），这里只导出列的比较函数 TABLE_SORTS。点行打开抽屉。
import * as React from 'react';
import type { MachineView } from '@/lib/types';
import type { DrawerTab } from '@/lib/drawer';
import * as F from '@/lib/format';
import { isQuiet, liveRate, monthTraffic, quota, remainingValue, resourceMeters } from '@/lib/derive';
import { collator, KIND_LABEL } from '@/lib/sort';
import { cn } from '@/lib/utils';
import { ContextMenu, ContextMenuContent, ContextMenuTrigger } from '@/components/ui/context-menu';
import { Tip } from '@/components/ui/overlays';
import { Due, KindIcon, Num, SortHead, StatusDot, type SortState } from '@/components/app/primitives';
import { NodeContextItems } from '@/components/app/NodeMenu';
import { Cells, MiniMeter, quotaColors } from '@/components/app/viz';

const Q8 = quotaColors(8);
// 数据过期（isQuiet）的机器读数显示 “—”，排序也按 −1（和速率一样排到最后）
const cpuPct = (m: MachineView) => (!isQuiet(m) && m.host?.cpu_pct != null ? m.host.cpu_pct : -1);
const memPct = (m: MachineView) => (!isQuiet(m) && m.host?.mem_total ? (m.host.mem_used ?? 0) / m.host.mem_total : -1);
const diskPct = (m: MachineView) => (!isQuiet(m) && m.host?.disk_total ? (m.host.disk_used ?? 0) / m.host.disk_total : -1);
/** 可排序列的比较函数（升序）；首次点击的方向见各列的 first */
export const TABLE_SORTS: Record<string, (a: MachineView, b: MachineView) => number> = {
  name: (a, b) => collator.compare(a.name, b.name),
  cpu: (a, b) => cpuPct(a) - cpuPct(b),
  mem: (a, b) => memPct(a) - memPct(b),
  disk: (a, b) => diskPct(a) - diskPct(b),
  up: (a, b) => (liveRate(a)?.tx ?? -1) - (liveRate(b)?.tx ?? -1),
  down: (a, b) => (liveRate(a)?.rx ?? -1) - (liveRate(b)?.rx ?? -1),
  traffic: (a, b) => { const x = monthTraffic(a); const y = monthTraffic(b); return x.up + x.down - (y.up + y.down); },
  quota: (a, b) => (quota(a).rem ?? Infinity) - (quota(b).rem ?? Infinity),
  due: (a, b) => (a.subscription?.due_days ?? 1e9) - (b.subscription?.due_days ?? 1e9),
  value: (a, b) => (remainingValue(a.subscription)?.cents ?? -1) - (remainingValue(b.subscription)?.cents ?? -1),
};
const monthly = (m: MachineView) => {
  const s = m.subscription;
  return s ? F.money(Math.round(s.price_cents / F.CYCLE_MONTHS[s.cycle]), s.currency) : null;
};

/** first：可排序列首次点击的方向（数值先降序；名称、到期先升序） */
interface Col { id: string; header: string; size: number; r?: boolean; first?: 'asc' | 'desc'; cell: (m: MachineView) => React.ReactNode }
const cols: Col[] = [
  { id: 'name', header: '名称', size: 140, first: 'asc',
    cell: (m) => <span className="tname"><StatusDot status={m.health.status} /><KindIcon kind={m.kind} /><Tip text={KIND_LABEL[m.kind]}><span className="cell-main strong">{m.name}</span></Tip></span> },
  { id: 'ip', header: 'IP', size: 120, cell: (m) => <span className="cell-main t-sub">{m.ip}</span> },
  ...(['cpu', 'mem', 'disk'] as const).map((k): Col => ({ id: k, header: k === 'cpu' ? 'CPU' : k === 'mem' ? '内存' : '磁盘', size: 84, first: 'desc',
    cell: (m) => { const r = resourceMeters(m); return r ? <MiniMeter pct={r[k].pct} metric={k} empty={isQuiet(m)} /> : <span className="t-muted">—</span>; } })),
  ...(['up', 'down'] as const).map((k): Col => ({ id: k, header: k === 'up' ? '上行' : '下行', size: 92, r: true, first: 'desc',
    cell: (m) => { const r = liveRate(m); const v = r && (k === 'up' ? r.tx : r.rx); return <span className="trate" data-speed={r ? F.speedOf(v) : undefined}><Num f={F.rate(v)} /></span>; } })),
  { id: 'traffic', header: '本月业务流量', size: 112, r: true, first: 'desc', cell: (m) => { const t = monthTraffic(m); return <Num f={F.bytes(t.up + t.down)} />; } },
  { id: 'quota', header: '剩余流量', size: 106, first: 'desc',
    cell: (m) => { const q = quota(m); return <span className="tquota" title={`本周期已用 ${F.str(F.bytes(q.used))}（${q.start.slice(5)} 起）`}>{q.rem != null && <Cells pct={q.pct} n={8} colors={Q8} />}{q.rem == null ? <span className="t-sub">不限</span> : q.rem > 0 ? <Num f={F.bytes(q.rem)} /> : <span className="due" data-tone="bad">已超限</span>}</span>; } },
  { id: 'due', header: '到期', size: 96, r: true, first: 'asc', cell: (m) => <Due days={m.subscription?.due_days} /> },
  { id: 'value', header: '剩余价值', size: 76, r: true, first: 'desc', cell: (m) => { const v = remainingValue(m.subscription); return v ? <span className="num">{F.money(v.cents, v.currency)}</span> : <span className="t-muted">—</span>; } },
  { id: 'fee', header: '月费', size: 72, r: true, cell: (m) => monthly(m) ?? <span className="t-muted">—</span> },
];
const TOTAL = cols.reduce((s, c) => s + c.size, 0);

export function MachineTable({ rows, sort, onSort, openId, onOpen }: { rows: MachineView[]; sort: SortState | null; onSort: (s: SortState | null) => void; openId: string | null; onOpen: (id: string, tab?: DrawerTab) => void }) {
  return (
    <div className="table-wrap mtable">
      <table className="dt is-one" style={{ minWidth: TOTAL }}>
        <colgroup>{cols.map((c) => <col key={c.id} style={{ width: c.size }} />)}</colgroup>
        <SortHead cols={cols} sort={sort} onSort={onSort} />
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
