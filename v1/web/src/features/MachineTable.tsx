// 首页“表格”视图：与卡片同一份筛选 + 排序 + 分页后的机器，一行一台，密集只读。
// 排序由首页持有（排序要在分页之前做，卡片 / 环形视图也按这几列的比较函数排），这里导出按列排序的 sortTable。行、表头与点行打开抽屉见 MachineGrid（与机器页共用）。
import { MoreHorizontal } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import * as F from '@/lib/format';
import { cycleLabel, isQuiet, liveRate, monthlyHint, quota, quotaGb, quotaTip, quotaUsedLabel, remainingValue, resourceMeters, statusRank, trafficTotal } from '@/lib/derive';
import { Due, Flag, HealthPill, HostTag, MonthFlowCell, Num, type SortState } from '@/components/app/primitives';
import { MachineGrid, byNum, byStr, sortByCol, type Col } from '@/components/app/MachineGrid';
import { Cells, MiniMeter, quotaColors } from '@/components/app/viz';
import { IconButton } from '@/components/ui/button';

const Q8 = quotaColors(8);
// 数据过期（isQuiet）的机器读数显示 “—”；排序时没有值的（null）不论升降都排在最后（MachineGrid 的 nil）
const cpuPct = (m: MachineView) => (!isQuiet(m) && m.host?.cpu_pct != null ? m.host.cpu_pct : null);
const memPct = (m: MachineView) => (!isQuiet(m) && m.host?.mem_total ? (m.host.mem_used ?? 0) / m.host.mem_total : null);
const diskPct = (m: MachineView) => (!isQuiet(m) && m.host?.disk_total ? (m.host.disk_used ?? 0) / m.host.disk_total : null);
const PCT = { cpu: cpuPct, mem: memPct, disk: diskPct };

// 列宽合计 1226：1426 宽窗口下内容区约 1079，先收起「月费」「剩余价值」（1078）；更窄时再收「磁盘」「内存」（1267 宽窗口内容区约 921 放得下 914）。
// 名称两行（名称 + 小字 IP，同机器页），不再单列 IP；状态单独一列（名称后面不再标）。同机关系标跟在名称后面，只写「同机」/「+N IP」（宿主机名在悬停说明里），
// 放不下时先省略名称。行尾「⋯」与右键是同一份菜单（MachineGrid 外面的 NodeMenus）
const cols: Col[] = [
  { id: 'name', header: '名称', size: 172, ...byStr((m) => m.name),
    cell: (m) => <div className="tname"><Flag country={m.country} manual={!!m.country_set} ip={m.ip} /><div>
      <div className="row" style={{ flexWrap: 'nowrap', gap: 6 }}><span className="cell-main strong">{m.name}</span><HostTag id={m.id} host={m.host_machine} nodes={m.ip_nodes} short /></div>
      <span className="sub">{m.ip}</span>
    </div></div> },
  { id: 'status', header: '状态', size: 88, ...byNum(statusRank, 'asc'), cell: (m) => <HealthPill m={m} /> },
  ...(['cpu', 'mem', 'disk'] as const).map((k): Col => ({ id: k, header: k === 'cpu' ? 'CPU' : k === 'mem' ? '内存' : '磁盘', size: 82, drop: k === 'disk' ? 3 : k === 'mem' ? 4 : undefined, ...byNum(PCT[k]),
    cell: (m) => { const r = resourceMeters(m); return r ? <MiniMeter pct={r[k].pct} metric={k} empty={isQuiet(m)} /> : <span className="t-muted">—</span>; } })),
  ...(['up', 'down'] as const).map((k): Col => ({ id: k, header: k === 'up' ? '上行' : '下行', size: 92, r: true, ...byNum((m) => (k === 'up' ? liveRate(m)?.tx : liveRate(m)?.rx) ?? null),
    cell: (m) => { const r = liveRate(m); const v = r && (k === 'up' ? r.tx : r.rx); return <span className="trate" data-speed={r ? F.speedOf(v) : undefined}><Num f={F.rate(v)} /></span>; } })),
  { id: 'traffic', header: '本月流量', size: 144, r: true, ...byNum(trafficTotal), cell: (m) => <MonthFlowCell m={m} /> },
  // 不限额度、与宿主机共用的没有「剩余」可比，排在最后
  { id: 'quota', header: '剩余流量', size: 106, ...byNum((m) => quota(m).rem),
    cell: (m) => { const q = quota(m); return <span className="tquota" data-tip={q.rem == null ? (q.shared ? '额度与宿主机共用' : undefined) : `${quotaUsedLabel(q)} ${F.str(quotaGb(q.used))}\n${quotaTip(m)}`}>{q.rem != null && <Cells pct={q.pct} n={8} colors={Q8} />}{q.rem == null ? <span className="t-sub">{q.shared ? '与宿主机共用' : '不限'}</span> : q.rem > 0 ? <Num f={quotaGb(q.rem)} /> : <span className="due" data-tone="bad">已超限</span>}</span>; } },
  { id: 'due', header: '到期', size: 96, r: true, ...byNum((m) => m.subscription?.due_days ?? null, 'asc'), cell: (m) => (m.subscription ? <Due days={m.subscription.due_days} state={m.subscription.due_state} /> : <span className="t-muted">—</span>) },
  { id: 'value', header: '剩余价值', size: 76, r: true, drop: 2, ...byNum((m) => remainingValue(m.subscription)?.cents ?? null), cell: (m) => { const v = remainingValue(m.subscription); return v ? <span className="num">{F.money(v.cents, v.currency)}</span> : <span className="t-muted">—</span>; } },
  // 服务端折好的月费（不同币种直接按数值比）；季付、年付的悬停写原价
  { id: 'fee', header: '月费', size: 72, r: true, drop: 1, ...byNum((m) => m.subscription?.monthly_cents ?? null),
    cell: (m) => { const s = m.subscription; return s ? <span className="num" data-tip={monthlyHint(s) ? `原价 ${F.money(s.price_cents, s.currency)}/${cycleLabel(s.cycle)}` : undefined}>{F.money(s.monthly_cents, s.currency)}</span> : <span className="t-muted">—</span>; } },
  { id: 'menu', header: '', size: 40, cell: () => <IconButton label="更多操作" tip={false} data-node-menu="" aria-haspopup="menu"><MoreHorizontal size={16} /></IconButton> },
];
export const sortTable = (rows: MachineView[], sort: SortState | null) => sortByCol(rows, cols, sort);

/** 两行名称：行高用 --row-2，与机器页一致 */
export function MachineTable({ rows, sort, onSort, openId, onOpen }: { rows: MachineView[]; sort: SortState | null; onSort: (s: SortState | null) => void; openId: string | null; onOpen: (id: string) => void }) {
  return <MachineGrid className="mtable" cols={cols} rows={rows} sort={sort} onSort={onSort} openId={openId} onOpen={onOpen} />;
}
