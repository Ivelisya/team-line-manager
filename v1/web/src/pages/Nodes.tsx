// 节点：表格视图。固定列宽；筛选（团队 / 类型 / 线路机 / 状态 / 范围）+ 搜索；点行打开右侧抽屉。
// 默认顺序 = 线路机 → 团队 → 编号（不排序时保持），表头排序是用户的显式选择，可一键恢复；排序记在 URL（?sort=due 升序、?sort=-due 降序，首页“7 天内到期”链到这里）。
// 分页在筛选、排序之后（每页 50）。
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { ChevronRight, RotateCcw, SearchX } from 'lucide-react';
import { useMachines, useTeams } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import { entryText, monthTraffic } from '@/lib/derive';
import { collator, HEALTH_LABEL, KIND_LABEL, useStableOrder } from '@/lib/sort';
import type { MachineView } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { SearchInput } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Segmented, Trunc } from '@/components/ui/overlays';
import { ContextMenu, ContextMenuContent, ContextMenuTrigger } from '@/components/ui/context-menu';
import { SkeletonRows } from '@/components/ui/skeleton';
import { Due, Empty, ErrorNote, HealthPill, KindIcon, Num, PageHead, Panel, Pill, SortHead, type SortState } from '@/components/app/primitives';
import { NodeHover } from '@/components/app/NodeHover';
import { NodeContextItems } from '@/components/app/NodeMenu';
import { Pagination } from '@/components/app/Pagination';
import { usePage } from '@/lib/usePage';

type Scope = 'live' | 'retired';
type Cmp = (a: MachineView, b: MachineView) => number;
/** cmp（升序）+ first（首次点击的方向）：有它们才能点表头排序 */
interface Col { id: string; header: string; size: number; r?: boolean; first?: 'asc' | 'desc'; cmp?: Cmp; cell: (m: MachineView) => React.ReactNode }
const byStr = (f: (m: MachineView) => string) => ({ first: 'asc' as const, cmp: (a: MachineView, b: MachineView) => collator.compare(f(a), f(b)) });
const byNum = (f: (m: MachineView) => number, first: 'asc' | 'desc' = 'desc') => ({ first, cmp: (a: MachineView, b: MachineView) => f(a) - f(b) });

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
  const [sp, setSp] = useSearchParams();
  const rawSort = sp.get('sort');
  const sort = React.useMemo<SortState | null>(() => (rawSort ? { id: rawSort.replace(/^-/, ''), desc: rawSort.startsWith('-') } : null), [rawSort]); // null = 默认顺序
  const setSort = (s: SortState | null) => setSp((p) => { const n = new URLSearchParams(p); if (s) n.set('sort', `${s.desc ? '-' : ''}${s.id}`); else n.delete('sort'); return n; }, { replace: true });

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

  const columns = React.useMemo<Col[]>(() => {
    const retired = scope === 'retired';
    const c: Col[] = [
      { id: 'name', header: '名称', size: retired ? 250 : 190, ...byStr((m) => m.name),
        cell: (m) => <div className="row" style={{ flexWrap: 'nowrap', gap: 8 }}><KindIcon kind={m.kind} /><div style={{ minWidth: 0 }}><NodeHover id={m.id} m={m}><span className="cell-main strong">{m.name}{m.lifecycle === 'retired' && <> <Pill tone="neutral" dot="ring">已退役</Pill></>}</span></NodeHover><span className="sub">{m.ip}</span></div></div> },
      { id: 'kind', header: '类型', size: 84, ...byStr((m) => KIND_LABEL[m.kind]), cell: (m) => KIND_LABEL[m.kind] },
      { id: 'status', header: '状态', size: 96, cell: (m) => <HealthPill m={m} /> },
      { id: 'entry', header: '线路机:端口', size: 150, cell: (m) => <Trunc className="cell-main">{entryText(m)}</Trunc> },
      { id: 'plan', header: '供应商 · 套餐', size: 190,
        cell: (m) => <>{m.plan ? <Trunc className="cell-main">{m.plan.name}</Trunc> : <span className="cell-main t-muted">未设置</span>}<span className="sub">{m.provider?.name ?? '—'}</span></> },
      { id: 'traffic', header: '本月业务流量', size: 120, r: true, ...byNum((m) => { const t = monthTraffic(m); return t.up + t.down; }),
        cell: (m) => { const t = monthTraffic(m); return <><Num f={F.bytes(t.up + t.down)} /><span className="sub">↓ {F.bytes(t.down).v} {F.bytes(t.down).u}</span></>; } },
    ];
    if (retired) {
      c.push({ id: 'retired', header: '退役时间 · 原因', size: 170, ...byStr((m) => m.retired_at ?? ''), cell: (m) => <><span className="cell-main">{F.dtFull(m.retired_at)}</span><span className="sub">{m.lifecycle_reason ?? ''}</span></> });
    } else {
      c.push({ id: 'due', header: '到期', size: 112, r: true, ...byNum((m) => m.subscription?.due_days ?? 1e9, 'asc'),
        cell: (m) => (m.subscription ? <><Due days={m.subscription.due_days} /><span className="sub">{m.subscription.due_date}</span></> : <span className="t-muted">—</span>) });
      c.push({ id: 'cost', header: '费用', size: 92, r: true,
        cell: (m) => m.subscription ? <><span className="num">{F.money(m.subscription.price_cents, m.subscription.currency)}</span><span className="sub">/ {F.cycleLabel(m.subscription.cycle)}</span></> : <span className="t-muted">—</span> });
    }
    c.push({ id: 'chev', header: '', size: 36, cell: () => <ChevronRight size={14} /> });
    return c;
  }, [scope]);

  // 表头排序：稳定排序，相同值保持默认顺序
  const rows = React.useMemo(() => {
    const cmp = columns.find((c) => c.id === sort?.id)?.cmp;
    if (!sort || !cmp) return data;
    const sign = sort.desc ? -1 : 1;
    return [...data].sort((a, b) => sign * cmp(a, b));
  }, [data, columns, sort]);
  const pg = usePage({ total: q.data ? rows.length : undefined, sizes: [25, 50, 100], size: 50, reset: [scope, text, team, kind, relay, status, sort] });
  const stable = useStableOrder(rows, sort?.id === 'traffic' ? JSON.stringify([scope, text, team, kind, relay, status, sort, pg.page, pg.size]) : null);
  const pageRows = React.useMemo(() => stable.slice(pg.offset, pg.offset + pg.size), [stable, pg.offset, pg.size]);
  const total = columns.reduce((s, c) => s + c.size, 0);
  const statusOpts = [...Object.entries(HEALTH_LABEL).filter(([k]) => k !== 'inactive').map(([value, label]) => ({ value, label })), { value: 'provisioning', label: '上架中' }, { value: 'retiring', label: '退役中' }];

  return (
    <div className="stack">
      <PageHead title="节点" note={`${data.length}${data.length !== all.length ? ` / ${all.length}` : ''} 台`}
        tools={<Segmented label="范围" value={scope} onChange={(v) => { setScope(v); setStatus(''); }} items={[{ value: 'live', label: '在用' }, { value: 'retired', label: '已退役' }]} />} />

      <div className="row" role="search" ref={pg.anchor}>
        <SearchInput value={text} onChange={(e) => setText(e.target.value)} placeholder="搜索名称、IP、套餐、服务编号…" aria-label="搜索" className="w-full sm:w-[260px]" />
        <div className="w-[140px]"><Select value={team} onChange={setTeam} allowEmpty="全部团队" placeholder="团队" options={teams.map((t) => ({ value: t.id, label: t.name }))} aria-label="团队" /></div>
        <div className="w-[130px]"><Select value={kind} onChange={setKind} allowEmpty="全部类型" placeholder="类型" options={Object.entries(KIND_LABEL).map(([value, label]) => ({ value, label }))} aria-label="类型" /></div>
        <div className="w-[140px]"><Select value={relay} onChange={setRelay} allowEmpty="全部线路机" placeholder="线路机" options={relays.map((r) => ({ value: r.id, label: r.name }))} aria-label="线路机" /></div>
        {scope === 'live' && <div className="w-[130px]"><Select value={status} onChange={setStatus} allowEmpty="全部状态" placeholder="状态" options={statusOpts} aria-label="状态" /></div>}
        {sort && <Button variant="quiet" size="md" onClick={() => setSort(null)}><RotateCcw />恢复默认顺序</Button>}
      </div>

      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel flush>
        <div className="table-wrap">
          <table className="dt" style={{ minWidth: total }}>
            <colgroup>{columns.map((c) => <col key={c.id} style={{ width: c.size }} />)}</colgroup>
            <SortHead cols={columns} sort={sort} onSort={setSort} />
            <tbody>
              {pageRows.map((m) => (
                <ContextMenu key={m.id}>
                  <ContextMenuTrigger asChild>
                    <tr className={`is-link${m.id === openId ? ' is-current' : ''}${m.lifecycle === 'retired' ? ' is-retired' : ''}`} tabIndex={0}
                      onClick={() => open(m.id)} onKeyDown={(e) => { if (e.key === 'Enter' && e.target === e.currentTarget) open(m.id); }}>
                      {columns.map((c) => <td key={c.id} className={c.id === 'chev' ? 'chev' : c.r ? 'r' : undefined}>{c.cell(m)}</td>)}
                    </tr>
                  </ContextMenuTrigger>
                  <ContextMenuContent><NodeContextItems m={m} /></ContextMenuContent>
                </ContextMenu>
              ))}
            </tbody>
          </table>
        </div>
        {!q.isLoading && !q.isError && rows.length === 0 && <Empty icon={<SearchX size={22} />} text={scope === 'retired' ? '还没有已退役的机器' : '没有符合条件的机器'} hint={all.length && scope === 'live' ? '换个筛选条件试试' : undefined} />}
        {q.isLoading && <SkeletonRows rows={6} />}
        <Pagination pg={pg} unit="台" />
      </Panel>
    </div>
  );
}
