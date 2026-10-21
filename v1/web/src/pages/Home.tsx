// 首页：两个分区。「监控总览」= 问候 + KPI + 集群状态（features/HomeOverview）；
// 「服务器集群」= 分组分段控件 + 三种视图：卡片（大 / 小 / 紧凑三种密度）、表格、环形；都在同一份筛选 + 排序之后分页。
// 默认排序：线路机 → 团队 → 编号；故障/离线不下沉。其它排序是用户的显式选择；表格视图另可点表头排序。
import * as React from 'react';
import { AlertTriangle, CircleGauge, Inbox, LayoutGrid, Server, Table2 } from 'lucide-react';
import { useMachines, useOverview, useTeams } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import { monthTraffic } from '@/lib/derive';
import type { MachineView } from '@/lib/types';
import { Empty, ErrorNote, FilterChip, type SortState } from '@/components/app/primitives';
import { NodeCard, SkeletonCard, type CardView } from '@/components/app/NodeCard';
import { RingCard } from '@/components/app/RingCard';
import { Segmented } from '@/components/ui/overlays';
import { Select } from '@/components/ui/select';
import { Pagination } from '@/components/app/Pagination';
import { HomeSection, OverviewSection } from '@/features/HomeOverview';
import { MachineTable, TABLE_SORTS } from '@/features/MachineTable';
import { usePage } from '@/lib/usePage';
import { collator, useStableOrder } from '@/lib/sort';

type SortKey = 'default' | 'name' | 'due' | 'traffic' | 'issues';
const SORTS: { value: SortKey; label: string }[] = [
  { value: 'default', label: '默认排序' }, { value: 'name', label: '名称' }, { value: 'due', label: '到期最近' },
  { value: 'traffic', label: '本月业务流量最多' }, { value: 'issues', label: '异常优先' },
];
type Layout = 'cards' | 'table' | 'ring';
const LAYOUTS: Layout[] = ['cards', 'table', 'ring'];
const DENSITIES: CardView[] = ['large', 'small', 'compact'];
/** 每页条数：卡片与环形都是 2、3、4、6 的倍数，常见列数下最后一行是满的；中间那档是默认 */
const PAGE_SIZES: Record<CardView | 'table' | 'ring', number[]> = { large: [12, 24, 48], small: [18, 36, 72], compact: [24, 48, 96], table: [25, 50, 100], ring: [24, 48, 96] };
const EMPTY_MACHINES: MachineView[] = [];
const store = {
  /** 读本地偏好；值不在 allowed 里（旧版本或手改的脏值）一律回退默认 */
  get: <T extends string>(k: string, d: T, allowed: readonly T[]): T => { try { const v = localStorage.getItem(k) as T | null; return v && allowed.includes(v) ? v : d; } catch { return d; } },
  set: (k: string, v: string) => { try { localStorage.setItem(k, v); } catch { /* 忽略 */ } },
};
const RANK: Record<string, number> = { down: 0, degraded: 1, unknown: 2, maintenance: 3, ok: 4, inactive: 5 };

const isIssue = (m: MachineView) => m.health.status === 'down' || m.health.status === 'degraded';

function sortList(list: MachineView[], key: SortKey): MachineView[] {
  if (key === 'default') return list;
  const c: Record<Exclude<SortKey, 'default'>, (a: MachineView, b: MachineView) => number> = {
    name: (a, b) => collator.compare(a.name, b.name),
    due: (a, b) => (a.subscription?.due_days ?? 1e9) - (b.subscription?.due_days ?? 1e9),
    traffic: (a, b) => { const x = monthTraffic(a); const y = monthTraffic(b); return y.up + y.down - (x.up + x.down); },
    issues: (a, b) => RANK[a.health.status] - RANK[b.health.status],
  };
  return [...list].sort(c[key]);
}

export function HomePage() {
  const machines = useMachines('live');
  const teams = useTeams();
  const ov = useOverview();
  const { id: openId, open } = useDrawer();
  const [layout, setLayout] = React.useState<Layout>(() => store.get('fleet:layout', 'cards', LAYOUTS));
  const [view, setView] = React.useState<CardView>(() => store.get('fleet:view', 'large', DENSITIES));
  const [sort, setSort] = React.useState<SortKey>(() => store.get('fleet:sort', 'default', SORTS.map((x) => x.value)));
  const [tsort, setTsort] = React.useState<SortState | null>(null); // 表格视图的表头排序（优先于上面的排序）
  const [team, setTeam] = React.useState<string>('all');
  const [onlyIssues, setOnlyIssues] = React.useState(false);
  const setL = (v: Layout) => { setLayout(v); store.set('fleet:layout', v); };
  const setV = (v: CardView) => { setView(v); store.set('fleet:view', v); };
  const setS = (v: SortKey) => { setSort(v); setTsort(null); store.set('fleet:sort', v); };

  const all = machines.data ?? EMPTY_MACHINES; // live 范围已不含已退役
  const counts = React.useMemo(() => ({
    all: all.length, relay: all.filter((m) => m.kind === 'relay').length,
    byTeam: new Map((teams.data ?? []).map((t) => [t.id, all.filter((m) => m.team?.id === t.id).length])),
    issues: all.filter(isIssue).length,
  }), [all, teams.data]);

  const ts = layout === 'table' ? tsort : null;
  const shown = React.useMemo(() => {
    let l = all;
    if (team === 'relay') l = l.filter((m) => m.kind === 'relay');
    else if (team !== 'all') l = l.filter((m) => m.team?.id === team);
    if (onlyIssues) l = l.filter(isIssue);
    l = sortList(l, sort);
    if (ts) { const sign = ts.desc ? -1 : 1; const cmp = TABLE_SORTS[ts.id]; l = [...l].sort((a, b) => sign * cmp(a, b)); } // 稳定排序：相同值保持原顺序
    return l;
  }, [all, team, onlyIssues, sort, ts]);
  // 只渲染当前页：KPI 和分组计数仍按全部机器算（上面的 counts）
  const sizes = PAGE_SIZES[layout === 'cards' ? view : layout];
  const pg = usePage({ total: machines.data ? shown.length : undefined, sizes, size: sizes[1], reset: [team, onlyIssues, sort, layout, view, ts?.id, ts?.desc] });
  const liveSort = !!ts || sort === 'traffic' || sort === 'issues'; // 依据随实时推送变的排序
  const stable = useStableOrder(shown, liveSort ? JSON.stringify([team, onlyIssues, sort, ts, pg.page, pg.size]) : null);
  const page = React.useMemo(() => stable.slice(pg.offset, pg.offset + pg.size), [stable, pg.offset, pg.size]);
  const layoutKey = `${team}|${onlyIssues}|${sort}|${view}`;

  const o = ov.data;
  const groups = [
    { value: 'all', label: '全部', count: counts.all },
    { value: 'relay', label: '线路机', count: counts.relay },
    ...(teams.data ?? []).map((t) => ({ value: t.id, label: t.name, count: counts.byTeam.get(t.id) ?? 0 })),
  ];

  return (
    <div className="stack home">
      {ov.isError && !o && <ErrorNote error={ov.error} onRetry={() => void ov.refetch()} />}
      <OverviewSection o={o} loading={!o && !ov.isError /* 失败时不要一直停在骨架：显示“—”和重试 */} />

      <HomeSection icon={<Server />} label="服务器集群" headRef={pg.anchor}
        title={<h2 className="hsec-title">机器矩阵与实时监控</h2>} sub="实时监控负载、资源占用、网络吞吐与在线状态；点读数或迹线可直接打开历史曲线。"
        actions={<>
          <Select sm value={sort} onChange={(v) => setS(v as SortKey)} options={SORTS} className="w-[150px]" aria-label="排序" />
          <Segmented sm label="视图" value={layout} onChange={setL} items={[
            { value: 'cards', label: <><LayoutGrid size={14} />卡片</> }, { value: 'table', label: <><Table2 size={14} />表格</> }, { value: 'ring', label: <><CircleGauge size={14} />环形</> },
          ]} />
          {layout === 'cards' && <Segmented sm label="卡片密度" value={view} onChange={setV} items={[{ value: 'large', label: '大' }, { value: 'small', label: '小' }, { value: 'compact', label: '紧凑' }]} />}
        </>}>
        <div className="hgroups" role="group" aria-label="筛选">
          <Segmented label="分组" value={team} onChange={setTeam} items={groups} className="seg-fill" />
          <FilterChip pressed={onlyIssues} onClick={() => setOnlyIssues((v) => !v)} count={counts.issues}><AlertTriangle size={13} />仅看异常</FilterChip>
        </div>

        {machines.isError && <ErrorNote error={machines.error} onRetry={() => void machines.refetch()} />}
        {machines.isLoading ? (
          <div className="cards" data-view={view}>{Array.from({ length: 6 }, (_, i) => <SkeletonCard key={i} view={view} />)}</div>
        ) : !machines.data ? null /* 请求失败且没有缓存：上面的错误提示已说明，不要误报“还没有机器” */ : shown.length === 0 ? (
          <Empty icon={<Inbox size={28} />} text={all.length ? '没有符合筛选条件的机器' : '还没有机器，去“上架”添加第一台'} />
        ) : (
          <>
            {/* 每页一个容器（key=页码+视图）：翻页、换视图是整页淡入。卡片页内：筛选/排序/密度变了（layoutKey 变）才量位置、滑到新位置 */}
            {layout === 'cards' && <div key={`c${pg.page}`} className="cards page-in" data-view={view}>
              {page.map((m) => <NodeCard key={m.id} m={m} view={view} current={m.id === openId} onOpen={open} layoutKey={layoutKey} />)}
            </div>}
            {layout === 'ring' && <div key={`r${pg.page}`} className="rings page-in">
              {page.map((m) => <RingCard key={m.id} m={m} current={m.id === openId} onOpen={open} />)}
            </div>}
            {layout === 'table' && <div key="t" className="hpanel page-in"><MachineTable rows={page} sort={tsort} onSort={setTsort} openId={openId} onOpen={open} /></div>}
            <Pagination pg={pg} unit="台" />
          </>
        )}
      </HomeSection>
    </div>
  );
}
