// 首页“表格”视图：与卡片同一份筛选 + 排序 + 分页后的机器，一行一台，密集只读。
// 表头排序由首页持有（排序要在分页之前做），这里只导出按列排序的 sortTable。行、表头与点行打开抽屉见 MachineGrid（与机器页共用）。
import type { MachineView } from '@/lib/types';
import * as F from '@/lib/format';
import { isQuiet, liveRate, monthTraffic, quota, quotaGb, remainingValue, resourceMeters } from '@/lib/derive';
import { Due, HostTag, KindIcon, Num, StatusMark, type SortState } from '@/components/app/primitives';
import { MachineGrid, byNum, byStr, sortByCol, type Col } from '@/components/app/MachineGrid';
import { Cells, MiniMeter, quotaColors } from '@/components/app/viz';

const Q8 = quotaColors(8);
// 数据过期（isQuiet）的机器读数显示 “—”，排序也按 −1（和速率一样排到最后）
const cpuPct = (m: MachineView) => (!isQuiet(m) && m.host?.cpu_pct != null ? m.host.cpu_pct : -1);
const memPct = (m: MachineView) => (!isQuiet(m) && m.host?.mem_total ? (m.host.mem_used ?? 0) / m.host.mem_total : -1);
const diskPct = (m: MachineView) => (!isQuiet(m) && m.host?.disk_total ? (m.host.disk_used ?? 0) / m.host.disk_total : -1);
const PCT = { cpu: cpuPct, mem: memPct, disk: diskPct };
const monthly = (m: MachineView) => {
  const s = m.subscription;
  return s ? F.money(Math.round(s.price_cents / F.CYCLE_MONTHS[s.cycle]), s.currency) : null;
};

// 列宽合计 1066：1426 宽窗口下内容区约 1079，不出横向滚动；更窄时先收起「月费」再收「剩余价值」（918，1267 宽窗口内容区约 921 也放得下）。
// 名称两行（名称 + 小字 IP，同机器页），不再单列 IP；名称列 172 扣掉首列左内边距、状态点、类型标后剩约 106px，「模拟线路机01」这类名字完整显示。
// 同机关系标跟在名称后面，只写「同机」/「+N IP」（宿主机名在悬停说明里），放不下时先省略名称
const cols: Col[] = [
  { id: 'name', header: '名称', size: 172, ...byStr((m) => m.name),
    cell: (m) => <div className="tname"><KindIcon kind={m.kind} /><div>
      <div className="row" style={{ flexWrap: 'nowrap', gap: 6 }}><span className="cell-main strong">{m.name}</span><StatusMark status={m.health.status} lifecycle={m.lifecycle} /><HostTag id={m.id} host={m.host_machine} nodes={m.ip_nodes} short /></div>
      <span className="sub">{m.ip}</span>
    </div></div> },
  ...(['cpu', 'mem', 'disk'] as const).map((k): Col => ({ id: k, header: k === 'cpu' ? 'CPU' : k === 'mem' ? '内存' : '磁盘', size: 82, ...byNum(PCT[k]),
    cell: (m) => { const r = resourceMeters(m); return r ? <MiniMeter pct={r[k].pct} metric={k} empty={isQuiet(m)} /> : <span className="t-muted">—</span>; } })),
  ...(['up', 'down'] as const).map((k): Col => ({ id: k, header: k === 'up' ? '上行' : '下行', size: 92, r: true, ...byNum((m) => (k === 'up' ? liveRate(m)?.tx : liveRate(m)?.rx) ?? -1),
    cell: (m) => { const r = liveRate(m); const v = r && (k === 'up' ? r.tx : r.rx); return <span className="trate" data-speed={r ? F.speedOf(v) : undefined}><Num f={F.rate(v)} /></span>; } })),
  { id: 'traffic', header: '本月业务流量', size: 112, r: true, ...byNum((m) => { const t = monthTraffic(m); return t.up + t.down; }), cell: (m) => { const t = monthTraffic(m); return <Num f={F.bytes(t.up + t.down)} />; } },
  { id: 'quota', header: '剩余流量', size: 106, ...byNum((m) => quota(m).rem ?? Infinity),
    cell: (m) => { const q = quota(m); return <span className="tquota" data-tip={`本周期已用 ${F.str(quotaGb(q.used))}（${q.start.slice(5)} 起）${q.shared ? '，额度与宿主机共用' : ''}`}>{q.rem != null && <Cells pct={q.pct} n={8} colors={Q8} />}{q.rem == null ? <span className="t-sub">{q.shared ? '与宿主机共用' : '不限'}</span> : q.rem > 0 ? <Num f={quotaGb(q.rem)} /> : <span className="due" data-tone="bad">已超限</span>}</span>; } },
  { id: 'due', header: '到期', size: 96, r: true, ...byNum((m) => m.subscription?.due_days ?? 1e9, 'asc'), cell: (m) => (m.subscription ? <Due days={m.subscription.due_days} /> : <span className="t-muted">—</span>) },
  { id: 'value', header: '剩余价值', size: 76, r: true, drop: 2, ...byNum((m) => remainingValue(m.subscription)?.cents ?? -1), cell: (m) => { const v = remainingValue(m.subscription); return v ? <span className="num">{F.money(v.cents, v.currency)}</span> : <span className="t-muted">—</span>; } },
  { id: 'fee', header: '月费', size: 72, r: true, drop: 1, cell: (m) => monthly(m) ?? <span className="t-muted">—</span> },
];
export const sortTable = (rows: MachineView[], sort: SortState | null) => sortByCol(rows, cols, sort);

/** 两行名称：行高用 --row-2，与机器页一致 */
export function MachineTable({ rows, sort, onSort, openId, onOpen }: { rows: MachineView[]; sort: SortState | null; onSort: (s: SortState | null) => void; openId: string | null; onOpen: (id: string) => void }) {
  return <MachineGrid className="mtable" cols={cols} rows={rows} sort={sort} onSort={onSort} openId={openId} onOpen={onOpen} />;
}
