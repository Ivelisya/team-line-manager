// 节点：表格视图。固定列宽；筛选（团队 / 类型 / 线路机 / 状态 / 范围）+ 搜索；点行打开右侧抽屉。
// 默认顺序 = 线路机 → 团队 → 编号（不排序时保持），表头排序是用户的显式选择，可一键恢复。
import * as React from 'react';
import { ArrowDownUp, ChevronRight, ChevronDown, ChevronUp, RotateCcw, SearchX } from 'lucide-react';
import { flexRender, getCoreRowModel, getSortedRowModel, useReactTable, type ColumnDef, type SortingState } from '@tanstack/react-table';
import { useMachines, useTeams } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import { entryText, monthTraffic } from '@/lib/derive';
import { HEALTH_LABEL, KIND_LABEL } from '@/lib/sort';
import type { MachineView } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { SearchInput } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Segmented, Trunc } from '@/components/ui/overlays';
import { ContextMenu, ContextMenuContent, ContextMenuTrigger } from '@/components/ui/context-menu';
import { SkeletonRows } from '@/components/ui/skeleton';
import { Empty, ErrorNote, HealthPill, KindIcon, Num, PageHead, Panel, Pill } from '@/components/app/primitives';
import { NodeHover } from '@/components/app/NodeHover';
import { NodeContextItems } from '@/components/app/NodeMenu';

type Scope = 'live' | 'retired';

export function NodesPage() {
  const [scope, setScope] = React.useState<Scope>('live');
  const q = useMachines(scope);
  const teams = useTeams().data ?? [];
  const { id: openId, open } = useDrawer();
  const [text, setText] = React.useState('');
  const [team, setTeam] = React.useState('');
  const [kind, setKind] = React.useState('');
  const [relay, setRelay] = React.useState('');
  const [status, setStatus] = React.useState('');
  const [sorting, setSorting] = React.useState<SortingState>([]);

  const all = q.data ?? [];
  const relays = all.filter((m) => m.kind === 'relay');
  const data = React.useMemo(() => {
    const t = text.trim().toLowerCase();
    return all.filter((m) => {
      if (team && m.team?.id !== team) return false;
      if (kind && m.kind !== kind) return false;
      if (relay && !(m.id === relay || m.entries.some((e) => e.relay.id === relay))) return false;
      if (status && (m.lifecycle === 'active' ? m.health.status : m.lifecycle) !== status) return false;
      if (t && !`${m.name} ${m.ip} ${m.provider?.name ?? ''} ${m.plan?.name ?? ''} ${m.service_ref ?? ''} ${m.note}`.toLowerCase().includes(t)) return false;
      return true;
    });
  }, [all, text, team, kind, relay, status]);

  const columns = React.useMemo<ColumnDef<MachineView>[]>(() => {
    const retired = scope === 'retired';
    const c: ColumnDef<MachineView>[] = [
      { id: 'name', header: '名称', size: retired ? 250 : 190, accessorFn: (m) => m.name, sortingFn: (a, b) => a.original.name.localeCompare(b.original.name, 'zh-Hans-CN', { numeric: true }),
        cell: ({ row: { original: m } }) => <div className="row" style={{ flexWrap: 'nowrap', gap: 8 }}><KindIcon kind={m.kind} /><div style={{ minWidth: 0 }}><NodeHover id={m.id} m={m}><span className="cell-main strong">{m.name}{m.lifecycle === 'retired' && <> <Pill tone="neutral" dot="ring">已退役</Pill></>}</span></NodeHover><span className="sub">{m.ip}</span></div></div> },
      { id: 'kind', header: '类型', size: 84, accessorFn: (m) => m.kind, cell: ({ row }) => KIND_LABEL[row.original.kind] },
      { id: 'status', header: '状态', size: 96, accessorFn: (m) => m.health.status, enableSorting: false, cell: ({ row }) => <HealthPill m={row.original} /> },
      { id: 'entry', header: '线路机:端口', size: 150, accessorFn: (m) => entryText(m), enableSorting: false, cell: ({ row }) => <Trunc className="cell-main">{entryText(row.original)}</Trunc> },
      { id: 'plan', header: '供应商 · 套餐', size: 190, accessorFn: (m) => m.plan?.name ?? '', enableSorting: false,
        cell: ({ row: { original: m } }) => <>{m.plan ? <Trunc className="cell-main">{m.plan.name}</Trunc> : <span className="cell-main t-muted">未设置</span>}<span className="sub">{m.provider?.name ?? '—'}</span></> },
      { id: 'traffic', header: '本月流量', size: 110, meta: { r: true }, accessorFn: (m) => { const t = monthTraffic(m); return t.up + t.down; },
        cell: ({ row: { original: m } }) => { const t = monthTraffic(m); return <><Num f={F.bytes(t.up + t.down)} /><span className="sub">↓ {F.bytes(t.down).v} {F.bytes(t.down).u}</span></>; } },
    ];
    if (retired) {
      c.push({ id: 'retired', header: '退役时间 · 原因', size: 170, accessorFn: (m) => m.retired_at ?? '', cell: ({ row: { original: m } }) => <><span className="cell-main">{F.dtFull(m.retired_at)}</span><span className="sub">{m.lifecycle_reason ?? ''}</span></> });
    } else {
      c.push({ id: 'due', header: '到期', size: 104, meta: { r: true }, accessorFn: (m) => m.subscription?.due_days ?? 1e9,
        cell: ({ row: { original: m } }) => { const d = F.dueFmt(m.subscription?.due_days); return m.subscription ? <><span style={{ color: d.tone === 'bad' ? 'var(--bad-text)' : d.tone === 'warn' ? 'var(--warn-text)' : undefined }}><Num f={d.text} /></span><span className="sub">{m.subscription.due_date}</span></> : <span className="t-muted">—</span>; } });
      c.push({ id: 'cost', header: '费用', size: 92, meta: { r: true }, accessorFn: (m) => m.subscription?.price_cents ?? 0, enableSorting: false,
        cell: ({ row: { original: m } }) => m.subscription ? <><span className="num">{F.money(m.subscription.price_cents, m.subscription.currency)}</span><span className="sub">/ {F.cycleLabel(m.subscription.cycle)}</span></> : <span className="t-muted">—</span> });
    }
    c.push({ id: 'chev', header: '', size: 36, enableSorting: false, cell: () => <ChevronRight size={14} /> });
    return c;
  }, [scope]);

  const table = useReactTable({ data, columns, state: { sorting }, onSortingChange: setSorting, getCoreRowModel: getCoreRowModel(), getSortedRowModel: getSortedRowModel(), enableSortingRemoval: true });
  const total = columns.reduce((s, c) => s + (c.size ?? 100), 0);
  const rows = table.getRowModel().rows;
  const statusOpts = [...Object.entries(HEALTH_LABEL).filter(([k]) => k !== 'inactive').map(([value, label]) => ({ value, label })), { value: 'provisioning', label: '上架中' }, { value: 'retiring', label: '下线中' }];

  return (
    <div className="stack">
      <PageHead title="节点" note={`${data.length}${data.length !== all.length ? ` / ${all.length}` : ''} 台`}
        tools={<Segmented label="范围" value={scope} onChange={(v) => { setScope(v); setStatus(''); }} items={[{ value: 'live', label: '在用' }, { value: 'retired', label: '已退役' }]} />} />

      <div className="row" role="search">
        <SearchInput value={text} onChange={(e) => setText(e.target.value)} placeholder="搜索名称、IP、套餐、服务编号…" aria-label="搜索" className="w-full sm:w-[260px]" />
        <div className="w-[140px]"><Select value={team} onChange={setTeam} allowEmpty="全部团队" placeholder="团队" options={teams.map((t) => ({ value: t.id, label: t.name }))} aria-label="团队" /></div>
        <div className="w-[130px]"><Select value={kind} onChange={setKind} allowEmpty="全部类型" placeholder="类型" options={Object.entries(KIND_LABEL).map(([value, label]) => ({ value, label }))} aria-label="类型" /></div>
        <div className="w-[140px]"><Select value={relay} onChange={setRelay} allowEmpty="全部线路机" placeholder="线路机" options={relays.map((r) => ({ value: r.id, label: r.name }))} aria-label="线路机" /></div>
        {scope === 'live' && <div className="w-[130px]"><Select value={status} onChange={setStatus} allowEmpty="全部状态" placeholder="状态" options={statusOpts} aria-label="状态" /></div>}
        {sorting.length > 0 && <Button variant="quiet" size="md" onClick={() => setSorting([])}><RotateCcw />恢复默认顺序</Button>}
      </div>

      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel flush>
        <div className="table-wrap">
          <table className="dt" style={{ minWidth: total }}>
            <colgroup>{columns.map((c) => <col key={c.id} style={{ width: c.size }} />)}</colgroup>
            <thead>
              {table.getHeaderGroups().map((hg) => (
                <tr key={hg.id}>
                  {hg.headers.map((h) => {
                    const r = (h.column.columnDef.meta as { r?: boolean } | undefined)?.r;
                    const dir = h.column.getIsSorted();
                    return (
                      <th key={h.id} className={r ? 'r' : undefined} aria-sort={dir === 'asc' ? 'ascending' : dir === 'desc' ? 'descending' : undefined}>
                        {h.column.getCanSort() ? (
                          <button type="button" className="th-sort" data-dir={dir || undefined} onClick={h.column.getToggleSortingHandler()}>
                            {flexRender(h.column.columnDef.header, h.getContext())}
                            {dir === 'asc' ? <ChevronUp /> : dir === 'desc' ? <ChevronDown /> : <ArrowDownUp />}
                          </button>
                        ) : flexRender(h.column.columnDef.header, h.getContext())}
                      </th>
                    );
                  })}
                </tr>
              ))}
            </thead>
            <tbody>
              {rows.map((row) => (
                <ContextMenu key={row.id}>
                  <ContextMenuTrigger asChild>
                    <tr className={`is-link${row.original.id === openId ? ' is-current' : ''}${row.original.lifecycle === 'retired' ? ' is-retired' : ''}`} tabIndex={0}
                      onClick={() => open(row.original.id)} onKeyDown={(e) => { if (e.key === 'Enter' && e.target === e.currentTarget) open(row.original.id); }}>
                      {row.getVisibleCells().map((cell) => {
                        const r = (cell.column.columnDef.meta as { r?: boolean } | undefined)?.r;
                        return <td key={cell.id} className={cell.column.id === 'chev' ? 'chev' : r ? 'r' : undefined}>{flexRender(cell.column.columnDef.cell, cell.getContext())}</td>;
                      })}
                    </tr>
                  </ContextMenuTrigger>
                  <ContextMenuContent><NodeContextItems m={row.original} /></ContextMenuContent>
                </ContextMenu>
              ))}
            </tbody>
          </table>
        </div>
        {!q.isLoading && !q.isError && rows.length === 0 && <Empty icon={<SearchX size={22} />} text={scope === 'retired' ? '还没有已退役的机器' : '没有符合条件的机器'} hint={all.length && scope === 'live' ? '换个筛选条件试试' : undefined} />}
        {q.isLoading && <SkeletonRows rows={6} />}
      </Panel>
    </div>
  );
}
