// 节点：表格视图。固定列宽（窄时先收起类型、费用列）；筛选（团队 / 类型 / 线路机 / 状态 / 范围）+ 搜索；点行打开右侧抽屉。
// 默认顺序 = 线路机 → 团队 → 编号（不排序时保持），表头排序是用户的显式选择，可一键恢复；排序记在 URL（?sort=due 升序、?sort=-due 降序，首页“即将到期”链到这里），
// 范围也是（?scope=recycle 回收站，缺省在用；回收站见 RecycleBin.tsx）。筛选同样记在 URL，别的页面直接链到筛好的列表：
// ?team=<团队 id>&kind=<类型>&relay=<线路机 id>&status=<状态>&q=<搜索>&due=soon（即将到期与已过期，不含备用团队，同首页 KPI 的 due_state 口径）&plan=<套餐 id>（团队与套餐页的台数链过来）。
// 分页在筛选、排序之后（每页 50）。
import * as React from 'react';
import { ChevronRight, RotateCcw, SearchX, X } from 'lucide-react';
import { useMachines, usePlans, useTeams } from '@/lib/queries';
import { setDrawerOrder, useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import { feeView, healthLabel, isIdleSpare, kindLabel, lifecycleLabel, monthFlow, statusOf, statusRank, trafficTotal } from '@/lib/derive';
import { HEALTH_TONE, KIND_ORDER, useStableOrder } from '@/lib/sort';
import type { MachineView } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { SearchInput } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Segmented, Trunc } from '@/components/ui/overlays';
import { SkeletonRows } from '@/components/ui/skeleton';
import { Due, DueDate, Empty, ErrorNote, Flag, HealthPill, HostTag, MonthFlowCell, Num, PageHead, Panel } from '@/components/app/primitives';
import { NodeHover } from '@/components/app/NodeHover';
import { MachineGrid, byNum, byStr, sortByCol, type Col } from '@/components/app/MachineGrid';
import { Pagination } from '@/components/app/Pagination';
import { RecycleBin } from './RecycleBin';
import { usePage } from '@/lib/usePage';
import { NODE_FILTERS, useUrlSearch, useUrlSort, useUrlState } from '@/lib/urlState';

type Scope = 'live' | 'recycle';
/** 不调淡任何行：机器页看的是资产信息，离线机器的这些字段照常可读（首页表格调淡的是读数过期的） */
const never = () => false;
// 筛选下拉的固定选项与空列表兜底用模块常量：页面随实时推送重渲染时，memo 的下拉（Select）不被新数组击穿。
// 选项的中文名来自 /meta（模块加载时还没取回），第一次用时算好留着
const EMPTY: MachineView[] = [];
const PAGE_SIZES = [25, 50, 100];
type Opt = { value: string; label: string };
let opts: { kind: Opt[]; status: Opt[] } | null = null;
const filterOpts = () => (opts ??= {
  kind: KIND_ORDER.map((value) => ({ value, label: kindLabel(value) })),
  status: [...Object.keys(HEALTH_TONE).filter((k) => k !== 'inactive').map((value) => ({ value, label: healthLabel(value) })),
    ...(['provisioning', 'failed', 'retiring'] as const).map((value) => ({ value, label: lifecycleLabel(value) }))],
});
const SCOPES: { value: Scope; label: string }[] = [{ value: 'live', label: '在用' }, { value: 'recycle', label: '回收站' }];
const entryOf = (e: MachineView['entries'][number]) => `${e.relay.name}:${e.port}`;
/** 行签名：这页的列只显示资产信息（流量按显示精度），不显示实时读数，推送只改读数时行不重渲染（MachineGrid sig）。
 *  列里显示的东西都要在这里——每加一列、列里多显示一个字段，都要同步改这里；悬浮卡不传 m，打开时自己取这台的最新数据 */
const rowSig = (m: MachineView) => {
  const f = monthFlow(m); const s = m.subscription;
  return [m.name, m.ip, m.country, m.country_set, m.kind, m.lifecycle, m.maintenance, m.health.status, m.health.reasons.map((r) => `${r.severity}${r.text}`).join(),
    m.host_machine?.id, m.host_machine?.name, m.ip_nodes.map((x) => `${x.id}:${x.name}`).join(), m.entries.map(entryOf).join(), m.plan?.name, m.provider?.name, f.kind, F.str(F.bytes(f.total)), F.str(F.bytes(f.down)), F.str(F.bytes(f.up)), f.from,
    s?.due_days, s?.due_state, s?.due_date, s?.price_cents, s?.monthly_cents, s?.monthly_usd_cents, s?.currency, s?.cycle, m.spare_from].join('\u0001');
};

/** 入口列：线路机写入口条数；节点写第一条「线路机:端口」，其余几条收成「+N」小标签，悬停列出 */
function EntryCell({ m }: { m: MachineView }) {
  if (m.kind === 'relay') return <span className="cell-main">{m.entries.length} 条入口</span>;
  const [first, ...rest] = m.entries;
  if (!first) return <span className="t-muted">—</span>;
  return (
    <div className="row" style={{ flexWrap: 'nowrap', gap: 6 }}>
      <Trunc className="cell-main">{entryOf(first)}</Trunc>
      {rest.length > 0 && <span className="tag" style={{ flex: 'none' }} data-tip={`其余入口\n${rest.map(entryOf).join('\n')}`}>+{rest.length}</span>}
    </div>
  );
}

export function NodesPage() {
  const [sp, upd] = useUrlState(); // upd 身份稳定，筛选回调跟着稳定（下拉是 memo 的）
  // 范围同排序记在 URL（?scope=recycle），刷新、后退或从链接打开都停在原来的范围；两个范围的筛选互不相干，切换时清掉
  const scope: Scope = sp.get('scope') === 'recycle' ? 'recycle' : 'live';
  const onScope = React.useCallback((v: Scope) => upd({ scope: v === 'recycle' ? v : '', ...Object.fromEntries([...NODE_FILTERS, 'source', 'p', 'sort'].map((k) => [k, ''])) }), [upd]);
  const tools = <Segmented label="范围" value={scope} onChange={onScope} items={SCOPES} />;
  return scope === 'recycle' ? <RecycleBin sp={sp} upd={upd} tools={tools} /> : <LiveNodes sp={sp} upd={upd} tools={tools} />;
}

function LiveNodes({ sp, upd, tools }: { sp: URLSearchParams; upd: (patch: Record<string, string>) => void; tools: React.ReactNode }) {
  const q = useMachines();
  const teams = useTeams().data;
  const teamOpts = React.useMemo(() => (teams ?? []).map((t) => ({ value: t.id, label: t.name })), [teams]);
  const { id: openId, open } = useDrawer();
  // 输入框是本地状态、停手后写回 ?q=（直接绑地址参数时快速输入会丢字）；筛选按地址里的 q
  const text = sp.get('q') ?? '';
  const [input, setInput] = useUrlSearch(sp, upd);
  const team = sp.get('team') ?? '';
  const kind = sp.get('kind') ?? '';
  const relay = sp.get('relay') ?? '';
  const status = sp.get('status') ?? '';
  const dueSoon = sp.get('due') === 'soon';
  const plan = sp.get('plan') ?? '';
  const planName = usePlans().data?.find((p) => p.id === plan)?.name;
  const setTeam = React.useCallback((v: string) => upd({ team: v }), [upd]);
  const setKind = React.useCallback((v: string) => upd({ kind: v }), [upd]);
  const setRelay = React.useCallback((v: string) => upd({ relay: v }), [upd]);
  const setStatus = React.useCallback((v: string) => upd({ status: v }), [upd]);
  const filtered = !!(text || team || kind || relay || status || dueSoon || plan);
  const clearFilters = () => { setInput(''); upd(Object.fromEntries(NODE_FILTERS.map((k) => [k, '']))); };
  const [urlSort, setSort] = useUrlSort(); // null = 默认顺序
  // 只看即将到期时，没选排序就按到期日升序（最急的在前）
  const sort = React.useMemo(() => urlSort ?? (dueSoon ? { id: 'due', desc: false } : null), [urlSort, dueSoon]);

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
      // 「故障」与首页 KPI 同口径：闲置备用（等到期放生的）不算，台数才对得上
      if (status && (statusOf(m) !== status || (status === 'down' && isIdleSpare(m)))) return false;
      if (dueSoon && (m.spare || !m.subscription || m.subscription.due_state === 'ok')) return false;
      if (plan && m.plan?.id !== plan) return false;
      if (t && !`${m.name} ${m.ip} ${m.provider?.name ?? ''} ${m.plan?.name ?? ''} ${m.service_ref ?? ''} ${m.note}`.toLowerCase().includes(t)) return false;
      return true;
    });
  }, [all, text, team, kind, relay, status, dueSoon, plan]);

  const columns = React.useMemo<Col[]>(() => {
    const c: Col[] = [
      // 同机关系标跟在名称后面（在悬浮卡的触发区外），只写「同机」/「+N IP」，宿主机名在悬停说明里；备用里的机器在名称后面标原名
      { id: 'name', header: '名称', size: 184, ...byStr((m) => m.name),
        cell: (m) => <div className="row" style={{ flexWrap: 'nowrap', gap: 8 }}><Flag country={m.country} manual={!!m.country_set} ip={m.ip} /><div style={{ minWidth: 0 }}>
          <div className="row" style={{ flexWrap: 'nowrap', gap: 6 }}><NodeHover id={m.id} noFocus><span className="cell-main strong">{m.name}</span></NodeHover><HostTag id={m.id} host={m.host_machine} nodes={m.ip_nodes} short />{m.spare_from && <span className="tag tag-from" data-tip={`原名 ${m.spare_from}：退役或被替换前的名字`}>原 {m.spare_from}</span>}</div>
          <span className="sub" data-tip={m.ip}>{m.ip}</span>
        </div></div> },
      { id: 'kind', header: '类型', size: 84, drop: 1, ...byStr((m) => kindLabel(m.kind)), cell: (m) => kindLabel(m.kind) },
      { id: 'status', header: '状态', size: 110, ...byNum(statusRank, 'asc'), cell: (m) => <HealthPill m={m} /> },
      { id: 'entry', header: '入口', size: 150, cell: (m) => <EntryCell m={m} /> },
      { id: 'plan', header: '套餐 · 供应商', size: 176, drop: 3,
        cell: (m) => <>{m.plan ? <Trunc className="cell-main">{m.plan.name}</Trunc> : <span className="cell-main t-muted">{m.host_machine ? '不带套餐' : '未设置'}</span>}<span className="sub" data-tip={m.provider?.name}>{m.provider?.name ?? '—'}</span></> },
      { id: 'traffic', header: '本月流量', size: 144, r: true, ...byNum(trafficTotal),
        cell: (m) => <MonthFlowCell m={m} /> },
      // 没有订阅的（无值）不论升降序都排在最后
      { id: 'due', header: '到期', size: 140, r: true, ...byNum((m) => m.subscription?.due_days ?? null, 'asc'),
        cell: (m) => (m.subscription ? <><Due days={m.subscription.due_days} state={m.subscription.due_state} /><span className="sub"><DueDate sub={m.subscription} /></span></> : <span className="t-muted">—</span>) },
      // 计费统一按 USD（feeView）：排序按服务端折好的月费；原币种不是 USD 或不是按月付的，副行写原价（悬停有汇率）。
      // 没有汇率的写原币种月费、副行标未折算，排在最后
      { id: 'cost', header: '费用', size: 104, r: true, drop: 2, ...byNum((m) => m.subscription?.monthly_usd_cents ?? null),
        cell: (m) => { const s = m.subscription; if (!s) return <span className="t-muted">—</span>;
          const v = feeView(s);
          return <div data-tip={v.tip ?? undefined}><Num f={{ v: v.main, u: v.unit }} />{v.sub && <span className="sub">{v.sub}</span>}</div>; } },
      { id: 'chev', header: '', size: 36, cell: () => <ChevronRight size={14} /> },
    ];
    return c;
  }, []);

  // 表头排序：稳定排序，相同值保持默认顺序
  const rows = React.useMemo(() => sortByCol(data, columns, sort), [data, columns, sort]);
  const pg = usePage({ total: q.data ? rows.length : undefined, sizes: PAGE_SIZES, size: 50, reset: [text, team, kind, relay, status, dueSoon, plan, sort] });
  const stable = useStableOrder(rows, sort?.id === 'traffic' ? JSON.stringify([text, team, kind, relay, status, dueSoon, plan, sort, pg.page, pg.size]) : null);
  const pageRows = React.useMemo(() => stable.slice(pg.offset, pg.offset + pg.size), [stable, pg.offset, pg.size]);
  React.useEffect(() => { setDrawerOrder(stable.map((m) => m.id)); }, [stable]);
  React.useEffect(() => () => setDrawerOrder(null), []);

  return (
    <div className="stack">
      <PageHead title="机器" note={`${data.length}${data.length !== all.length ? ` / ${all.length}` : ''} 台`}
        tools={tools} />

      <div className="row" role="search" ref={pg.anchor}>
        <SearchInput value={input} onChange={(e) => setInput(e.target.value)} placeholder="搜索名称、IP、套餐、服务编号…" aria-label="搜索" className="w-full sm:w-[260px]" />
        <div className="w-[140px]"><Select value={team} onChange={setTeam} allowEmpty="全部团队" placeholder="团队" options={teamOpts} aria-label="团队" /></div>
        <div className="w-[130px]"><Select value={kind} onChange={setKind} allowEmpty="全部类型" placeholder="类型" options={filterOpts().kind} aria-label="类型" /></div>
        <div className="w-[140px]"><Select value={relay} onChange={setRelay} allowEmpty="全部线路机" placeholder="线路机" options={relayOpts} aria-label="线路机" /></div>
        <div className="w-[130px]"><Select value={status} onChange={setStatus} allowEmpty="全部状态" placeholder="状态" options={filterOpts().status} aria-label="状态" /></div>
        {dueSoon && <button type="button" className="fchip" aria-pressed="true" aria-label="取消「即将到期」筛选" onClick={() => upd({ due: '' })}>即将到期<X size={14} /></button>}
        {plan && <button type="button" className="fchip" aria-pressed="true" aria-label="取消套餐筛选" onClick={() => upd({ plan: '' })}>套餐{planName && ` ${planName}`}<X size={14} /></button>}
        {/* 两个按钮只要有一个在就都占着位置（另一个淡出隐藏）：点掉一个时另一个不平移到鼠标下面 */}
        {(filtered || urlSort) && <>
          <Button variant="quiet" size="md" className="btn-slot" data-off={filtered ? undefined : ''} onClick={clearFilters}><X />清除筛选</Button>
          <Button variant="quiet" size="md" className="btn-slot" data-off={urlSort ? undefined : ''} onClick={() => setSort(null)}><RotateCcw />恢复默认顺序</Button>
        </>}
      </div>

      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel flush>
        <MachineGrid cols={columns} rows={pageRows} sort={sort} onSort={setSort} openId={openId} onOpen={open} dim={never} sig={rowSig} />
        {!q.isLoading && !q.isError && rows.length === 0 && (
          <Empty icon={<SearchX size={22} />} text="没有符合条件的机器">
            {filtered && all.length > 0 && <Button size="sm" onClick={clearFilters}>清除筛选</Button>}
          </Empty>
        )}
        {q.isLoading && <SkeletonRows rows={6} />}
        <Pagination pg={pg} unit="台" />
      </Panel>
    </div>
  );
}
