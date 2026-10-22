// 节点：表格视图。固定列宽（窄时先收起类型、费用列）；筛选（团队 / 类型 / 线路机 / 状态 / 范围）+ 搜索；点行打开右侧抽屉。
// 默认顺序 = 线路机 → 团队 → 编号（不排序时保持），表头排序是用户的显式选择，可一键恢复；排序记在 URL（?sort=due 升序、?sort=-due 降序，首页“7 天内到期”链到这里），
// 范围也是（?scope=retired 已退役，缺省在用）。
// 分页在筛选、排序之后（每页 50）。
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { ChevronRight, RotateCcw, SearchX } from 'lucide-react';
import { useMachines, useTeams } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import { entryText, monthTraffic } from '@/lib/derive';
import { HEALTH_LABEL, KIND_LABEL, useStableOrder } from '@/lib/sort';
import type { MachineView } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { SearchInput } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Segmented, Trunc } from '@/components/ui/overlays';
import { SkeletonRows } from '@/components/ui/skeleton';
import { Due, Empty, ErrorNote, Flag, HealthPill, HostTag, Num, PageHead, Panel, Pill, type SortState } from '@/components/app/primitives';
import { NodeHover } from '@/components/app/NodeHover';
import { MachineGrid, byNum, byStr, sortByCol, type Col } from '@/components/app/MachineGrid';
import { Pagination } from '@/components/app/Pagination';
import { usePage } from '@/lib/usePage';

type Scope = 'live' | 'retired';
/** 只调淡已退役的行：机器页看的是资产信息，离线机器的这些字段照常可读（首页表格调淡的是读数过期的） */
const isRetired = (m: MachineView) => m.lifecycle === 'retired';
// 筛选下拉的固定选项与空列表兜底用模块常量：页面随实时推送重渲染时，memo 的下拉（Select）不被新数组击穿
const EMPTY: MachineView[] = [];
const PAGE_SIZES = [25, 50, 100];
const KIND_OPTS = Object.entries(KIND_LABEL).map(([value, label]) => ({ value, label }));
const STATUS_OPTS = [...Object.entries(HEALTH_LABEL).filter(([k]) => k !== 'inactive').map(([value, label]) => ({ value, label })), { value: 'provisioning', label: '上架中' }, { value: 'failed', label: '上架失败' }, { value: 'retiring', label: '退役中' }];
const SCOPES: { value: Scope; label: string }[] = [{ value: 'live', label: '在用' }, { value: 'retired', label: '已退役' }];
/** 行签名：这页的列只显示资产信息（流量按显示精度），不显示实时读数，推送只改读数时行不重渲染（MachineGrid sig）。
 *  列里显示的东西都要在这里——每加一列、列里多显示一个字段，都要同步改这里；悬浮卡不传 m，打开时自己取这台的最新数据 */
const rowSig = (m: MachineView) => {
  const t = monthTraffic(m); const s = m.subscription;
  return [m.name, m.ip, m.kind, m.lifecycle, m.maintenance, m.health.status, m.health.reasons.map((r) => `${r.severity}${r.text}`).join(),
    m.host_machine?.id, m.host_machine?.name, m.ip_nodes.map((x) => `${x.id}:${x.name}`).join(), entryText(m), m.plan?.name, m.provider?.name, F.str(F.bytes(t.up + t.down)), F.str(F.bytes(t.down)),
    s?.due_days, s?.due_date, s?.price_cents, s?.currency, s?.cycle, m.retired_at, m.lifecycle_reason].join('\u0001');
};

export function NodesPage() {
  const [sp, setSp] = useSearchParams();
  // 范围同排序记在 URL（?scope=retired），刷新、后退或从链接打开都停在原来的范围
  const scope: Scope = sp.get('scope') === 'retired' ? 'retired' : 'live';
  const q = useMachines(scope);
  const teams = useTeams().data;
  const teamOpts = React.useMemo(() => (teams ?? []).map((t) => ({ value: t.id, label: t.name })), [teams]);
  const { id: openId, open } = useDrawer();
  const [text, setText] = React.useState('');
  const [team, setTeam] = React.useState('');
  const [kind, setKind] = React.useState('');
  const [relay, setRelay] = React.useState('');
  const [status, setStatus] = React.useState('');
  const rawSort = sp.get('sort');
  const sort = React.useMemo<SortState | null>(() => (rawSort ? { id: rawSort.replace(/^-/, ''), desc: rawSort.startsWith('-') } : null), [rawSort]); // null = 默认顺序
  const setSort = (s: SortState | null) => setSp((p) => { const n = new URLSearchParams(p); if (s) n.set('sort', `${s.desc ? '-' : ''}${s.id}`); else n.delete('sort'); return n; }, { replace: true });
  const onScope = React.useCallback((v: Scope) => {
    setSp((p) => { const n = new URLSearchParams(p); if (v === 'retired') n.set('scope', v); else n.delete('scope'); return n; }, { replace: true });
    setStatus('');
  }, [setSp]);

  const all = q.data ?? EMPTY;
  // 线路机选项只随名单变：实时推送每次都换机器列表，线路机和名字没变时选项引用不变
  const relays = all.filter((m) => m.kind === 'relay');
  const relaySig = relays.map((r) => `${r.id}:${r.name}`).join();
  const relayOpts = React.useMemo(() => relays.map((r) => ({ value: r.id, label: r.name })), [relaySig]); // eslint-disable-line react-hooks/exhaustive-deps
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
      // 同机关系标跟在名称后面（在悬浮卡的触发区外），只写「同机」/「+N IP」，宿主机名在悬停说明里
      { id: 'name', header: '名称', size: retired ? 250 : 180, ...byStr((m) => m.name),
        cell: (m) => <div className="row" style={{ flexWrap: 'nowrap', gap: 8 }}><Flag country={m.country} manual={!!m.country_set} ip={m.ip} /><div style={{ minWidth: 0 }}>
          <div className="row" style={{ flexWrap: 'nowrap', gap: 6 }}><NodeHover id={m.id} noFocus><span className="cell-main strong">{m.name}{m.lifecycle === 'retired' && <> <Pill tone="neutral">已退役</Pill></>}</span></NodeHover><HostTag id={m.id} host={m.host_machine} nodes={m.ip_nodes} short /></div>
          <span className="sub">{m.ip}</span>
        </div></div> },
      { id: 'kind', header: '类型', size: 84, drop: 1, ...byStr((m) => KIND_LABEL[m.kind]), cell: (m) => KIND_LABEL[m.kind] },
      { id: 'status', header: '状态', size: 96, cell: (m) => <HealthPill m={m} /> },
      { id: 'entry', header: '线路机:端口', size: 140, cell: (m) => <Trunc className="cell-main">{entryText(m)}</Trunc> },
      { id: 'plan', header: '供应商 · 套餐', size: 176, drop: 3,
        cell: (m) => <>{m.plan ? <Trunc className="cell-main">{m.plan.name}</Trunc> : <span className="cell-main t-muted">{m.host_machine ? '不带套餐' : '未设置'}</span>}<span className="sub">{m.provider?.name ?? '—'}</span></> },
      { id: 'traffic', header: '本月业务流量', size: 112, r: true, ...byNum((m) => { const t = monthTraffic(m); return t.up + t.down; }),
        cell: (m) => { const t = monthTraffic(m); return <><Num f={F.bytes(t.up + t.down)} /><span className="sub">下载 {F.bytes(t.down).v} {F.bytes(t.down).u}</span></>; } },
    ];
    if (retired) {
      c.push({ id: 'retired', header: '退役时间 · 原因', size: 170, ...byStr((m) => m.retired_at ?? ''), cell: (m) => <><span className="cell-main">{F.dtFull(m.retired_at)}</span><span className="sub">{m.lifecycle_reason ?? ''}</span></> });
    } else {
      c.push({ id: 'due', header: '到期', size: 112, r: true, ...byNum((m) => m.subscription?.due_days ?? 1e9, 'asc'),
        cell: (m) => (m.subscription ? <><Due days={m.subscription.due_days} /><span className="sub">{m.subscription.due_date}</span></> : <span className="t-muted">—</span>) });
      c.push({ id: 'cost', header: '费用', size: 92, r: true, drop: 2,
        cell: (m) => m.subscription ? <><span className="num">{F.money(m.subscription.price_cents, m.subscription.currency)}</span><span className="sub">/ {F.cycleLabel(m.subscription.cycle)}</span></> : <span className="t-muted">—</span> });
    }
    c.push({ id: 'chev', header: '', size: 36, cell: () => <ChevronRight size={14} /> });
    return c;
  }, [scope]);

  // 表头排序：稳定排序，相同值保持默认顺序
  const rows = React.useMemo(() => sortByCol(data, columns, sort), [data, columns, sort]);
  const pg = usePage({ total: q.data ? rows.length : undefined, sizes: PAGE_SIZES, size: 50, reset: [scope, text, team, kind, relay, status, sort] });
  const stable = useStableOrder(rows, sort?.id === 'traffic' ? JSON.stringify([scope, text, team, kind, relay, status, sort, pg.page, pg.size]) : null);
  const pageRows = React.useMemo(() => stable.slice(pg.offset, pg.offset + pg.size), [stable, pg.offset, pg.size]);

  return (
    <div className="stack">
      <PageHead title="机器" note={`${data.length}${data.length !== all.length ? ` / ${all.length}` : ''} 台`}
        tools={<Segmented label="范围" value={scope} onChange={onScope} items={SCOPES} />} />

      <div className="row" role="search" ref={pg.anchor}>
        <SearchInput value={text} onChange={(e) => setText(e.target.value)} placeholder="搜索名称、IP、套餐、服务编号…" aria-label="搜索" className="w-full sm:w-[260px]" />
        <div className="w-[140px]"><Select value={team} onChange={setTeam} allowEmpty="全部团队" placeholder="团队" options={teamOpts} aria-label="团队" /></div>
        <div className="w-[130px]"><Select value={kind} onChange={setKind} allowEmpty="全部类型" placeholder="类型" options={KIND_OPTS} aria-label="类型" /></div>
        <div className="w-[140px]"><Select value={relay} onChange={setRelay} allowEmpty="全部线路机" placeholder="线路机" options={relayOpts} aria-label="线路机" /></div>
        {scope === 'live' && <div className="w-[130px]"><Select value={status} onChange={setStatus} allowEmpty="全部状态" placeholder="状态" options={STATUS_OPTS} aria-label="状态" /></div>}
        {sort && <Button variant="quiet" size="md" onClick={() => setSort(null)}><RotateCcw />恢复默认顺序</Button>}
      </div>

      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel flush>
        <MachineGrid cols={columns} rows={pageRows} sort={sort} onSort={setSort} openId={openId} onOpen={open} dim={isRetired} sig={rowSig} />
        {!q.isLoading && !q.isError && rows.length === 0 && <Empty icon={<SearchX size={22} />} text={scope === 'retired' && !all.length ? '还没有已退役的机器' : '没有符合条件的机器'} hint={all.length ? '换个筛选条件试试' : undefined} />}
        {q.isLoading && <SkeletonRows rows={6} />}
        <Pagination pg={pg} unit="台" />
      </Panel>
    </div>
  );
}
