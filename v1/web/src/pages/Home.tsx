// 首页：两个分区。「监控总览」= 问候 + KPI + 集群状态（features/HomeOverview）；
// 「服务器集群」= 分组分段控件 + 三种视图：卡片（详细 / 标准 / 紧凑三种密度）、表格、环形；都在同一份筛选 + 排序之后分页。
// 排序只有一份：工具栏的下拉与表格表头改的是同一个状态（下拉列常用的几种，表头点出来的其它排序在下拉里显示「自定义」），卡片 / 环形视图同样按它排。
// 默认排序：线路机 → 团队 → 编号；故障/离线不下沉。分组、排序、仅看异常、视图、密度都写在地址里（刷新、分享后不变），没写时用上次的选择（本地偏好），视图、密度、排序随即补进地址。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { AlertTriangle, CircleGauge, CloudUpload, Inbox, LayoutGrid, Server, Table2 } from 'lucide-react';
import { useMachines, useOverview, useProbeBeatsPoll, useTeams } from '@/lib/queries';
import { usePingBeatsPoll } from '@/lib/ping';
import { setDrawerOrder, useDrawer } from '@/lib/drawer';
import { isIdleSpare } from '@/lib/derive';
import { prefOf, prefs } from '@/lib/prefs';
import { parseSort, sortParam, useUrlState } from '@/lib/urlState';
import type { MachineView } from '@/lib/types';
import { Empty, ErrorNote, FilterChip, type SortState } from '@/components/app/primitives';
import { NodeCard, SkeletonCard, type CardShow, type CardView } from '@/components/app/NodeCard';
import { NodeMenus } from '@/components/app/NodeMenu';
import { RingCard } from '@/components/app/RingCard';
import { Segmented, SegmentedOverflow } from '@/components/ui/overlays';
import { Select } from '@/components/ui/select';
import { Button } from '@/components/ui/button';
import { Pagination } from '@/components/app/Pagination';
import { HomeSection, OverviewSection } from '@/features/HomeOverview';
import { MachineTable, sortTable } from '@/features/MachineTable';
import { usePage } from '@/lib/usePage';
import { useStableOrder } from '@/lib/sort';
import { watchSize } from '@/lib/sharedObserver';

/** 排序写成「列 id」，降序前面加「-」（同机器页的 ?sort=）；下拉里的几种常用排序。default = 默认顺序 */
const SORTS = [
  { value: 'default', label: '默认排序' }, { value: 'name', label: '名称' }, { value: 'due', label: '到期最近' },
  { value: '-traffic', label: '本月流量最多' }, { value: 'status', label: '异常优先' },
];
const SORTS_CUSTOM = [...SORTS, { value: 'custom', label: '自定义' }];
const homeSort = (s: string) => (s === 'default' ? null : parseSort(s));
const sortStr = (s: SortState | null) => sortParam(s) || 'default';
/** 随实时推送变的排序依据（读数、流量、额度、状态）：显示顺序要稳住（useStableOrder），换页、换排序才重排 */
const STATIC_SORT = new Set(['name', 'due', 'value', 'fee']);
type Layout = 'cards' | 'table' | 'ring';
const LAYOUTS: Layout[] = ['cards', 'table', 'ring'];
const DENSITIES: CardView[] = ['large', 'small', 'compact'];
// 分段控件的选项用模块常量：首页随实时推送重渲染时，memo 的分段控件不跟着重画。密度按实际效果命名
const DENSITY_ITEMS: { value: CardView; label: string }[] = [{ value: 'large', label: '详细' }, { value: 'small', label: '标准' }, { value: 'compact', label: '紧凑' }];
const LAYOUT_ITEMS: { value: Layout; label: React.ReactNode }[] = [
  { value: 'cards', label: <><LayoutGrid size={14} />卡片</> }, { value: 'table', label: <><Table2 size={14} />表格</> }, { value: 'ring', label: <><CircleGauge size={14} />环形</> },
];
/** 一页超过这么多张卡就不做筛选 / 排序 / 换密度时的弹簧重排：几十张卡同时量位置、逐帧变换，比直接换位更卡 */
const LAYOUT_ANIM_MAX = 24;
/** 每页条数：卡片与环形都是 2、3、4、6 的倍数，常见列数下最后一行是满的；中间那档是默认 */
const PAGE_SIZES: Record<CardView | 'table' | 'ring', number[]> = { large: [12, 24, 48], small: [18, 36, 72], compact: [24, 48, 96], table: [25, 50, 100], ring: [24, 48, 96] };
const EMPTY_MACHINES: MachineView[] = [];
// 本地偏好的键（没写在地址里时用上次的选择）
const P_LAYOUT = 'fleet:layout'; const P_VIEW = 'fleet:view'; const P_SORT = 'fleet:home-sort';

/** 异常：故障、上架失败（上架失败的健康是 inactive，单看健康会漏掉）。降级照常服务、只是有提醒，算正常（同总览的「正常」）；
 *  闲置备用（没有出口指着、没有在用 IP 节点）不通是预期，不算（同总览的故障计数） */
const isIssue = (m: MachineView) => m.lifecycle === 'failed' || (m.health.status === 'down' && !isIdleSpare(m));

/** 吸顶条是否正吸在顶栏下面：哨兵放在条的原位（高 0），滚到顶栏线以上就是吸住了（吸住后才铺毛玻璃和底线）。
 *  按滚动事件（每帧一次）量哨兵的位置，不用 IntersectionObserver：一下子滚过很远（锚点、刷新后恢复滚动位置）时哨兵从视口下方直接跳到上方，观察器不会触发。
 *  同时把吸顶部分的高度写成 --hbar-h，表格视图的吸顶表头据此排在它下面（宽屏吸整条；窄屏 .hbar 是 display: contents，只吸团队页签那一行） */
function useStuckBar() {
  const sentinel = React.useRef<HTMLDivElement>(null);
  const bar = React.useRef<HTMLDivElement>(null);
  const [stuck, setStuck] = React.useState(false);
  React.useEffect(() => {
    const top = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--topbar-h')) || 56;
    // 条的原位比哨兵低 8px（条上下各伸出 8px 内边距，见 home.css），条的上沿碰到顶栏下沿时才算吸住
    let raf = 0;
    const check = () => { raf = 0; setStuck(sentinel.current!.getBoundingClientRect().top < top - 8); };
    const onScroll = () => { raf ||= requestAnimationFrame(check); };
    check();
    addEventListener('scroll', onScroll, { passive: true });
    const b = bar.current!;
    const measure = () => {
      const el = getComputedStyle(b).display === 'contents' ? b.firstElementChild as HTMLElement : b;
      b.parentElement!.style.setProperty('--hbar-h', `${el.offsetHeight}px`);
    };
    measure();
    const off = [watchSize(b, measure), watchSize(b.firstElementChild!, measure)];
    return () => { removeEventListener('scroll', onScroll); cancelAnimationFrame(raf); off.forEach((f) => f()); };
  }, []);
  return { sentinel, bar, stuck };
}

export function HomePage() {
  const machines = useMachines();
  const teams = useTeams();
  const ov = useOverview();
  const { id: openId, open } = useDrawer();
  const [sp, upd] = useUrlState();
  // 本地偏好只在打开首页时读一次 localStorage（实时推送每次重渲染都读没必要）；之后改选择时 setL / setV / setSort 同步改这份，
  // 地址参数被清掉（再点一次侧栏的首页）时退回的是最近的选择
  const [saved] = React.useState(() => ({ layout: prefOf(P_LAYOUT, 'cards', LAYOUTS), view: prefOf(P_VIEW, 'large', DENSITIES), sort: prefs.get(P_SORT) ?? 'default' }));
  const layout = LAYOUTS.includes(sp.get('view') as Layout) ? sp.get('view') as Layout : saved.layout;
  const view = DENSITIES.includes(sp.get('density') as CardView) ? sp.get('density') as CardView : saved.view;
  const rawSort = sp.get('sort') ?? saved.sort;
  const sort = React.useMemo(() => homeSort(rawSort), [rawSort]);
  // 分组：线路机一组用 kind=relay，团队用 team=<id>（同机器页的筛选参数名）
  const team = sp.get('kind') === 'relay' ? 'relay' : sp.get('team') ?? 'all';
  const onlyIssues = sp.get('issues') === '1';
  // 写地址的同时记成本地偏好（下次没带参数打开时用）
  const setL = React.useCallback((v: Layout) => { prefs.set(P_LAYOUT, v); saved.layout = v; upd({ view: v }); }, [upd, saved]);
  const setV = React.useCallback((v: CardView) => { prefs.set(P_VIEW, v); saved.view = v; upd({ density: v }); }, [upd, saved]);
  const setSort = React.useCallback((s: SortState | null) => { const v = sortStr(s); prefs.set(P_SORT, v); saved.sort = v; upd({ sort: v }); }, [upd, saved]);
  const pickSort = React.useCallback((v: string) => { if (v !== 'custom') setSort(homeSort(v)); }, [setSort]);
  // 视图、密度、排序没写在地址里时按本机偏好：缺哪个补哪个（首次打开、点侧栏回到首页；替换当前记录），复制地址分享出去，对方看到的和这里一样。
  // 分组与仅看异常不记本机偏好，没写就是「全部」「关」，人人一样，不用补
  const missing = (['view', 'density', 'sort'] as const).filter((k) => !sp.has(k)).join(' ');
  React.useEffect(() => {
    if (!missing) return;
    const cur = { view: layout, density: view, sort: rawSort };
    upd(Object.fromEntries(missing.split(' ').map((k) => [k, cur[k as keyof typeof cur]])));
  }, [missing, layout, view, rawSort, upd]);
  useProbeBeatsPoll(layout === 'cards'); // 卡片心跳条每 2 分钟刷新：全页一个轮询
  usePingBeatsPoll(layout === 'cards'); // 线路机卡片的三网 PING 每分钟刷新：同上

  // 实时推送每条都换一份机器列表：按可打断的优先级重渲染（用户的点击、输入先处理，推送晚一点跟上）
  const all = React.useDeferredValue(machines.data ?? EMPTY_MACHINES); // 列表不含已回收的
  // 首份列表刚到、推迟的那一轮还没渲染：仍按加载中处理（不闪「还没有机器」，也不把地址里的页码夹回第 1 页）
  const pending = all === EMPTY_MACHINES && !!machines.data?.length;
  const counts = React.useMemo(() => ({
    all: all.length, relay: all.filter((m) => m.kind === 'relay').length,
    byTeam: new Map((teams.data ?? []).map((t) => [t.id, all.filter((m) => m.team?.id === t.id).length])),
  }), [all, teams.data]);
  // 当前分组：「仅看异常」的计数与筛选都从它出发
  const grouped = React.useMemo(() => (team === 'all' ? all : team === 'relay' ? all.filter((m) => m.kind === 'relay') : all.filter((m) => m.team?.id === team)), [all, team]);
  const issueCount = React.useMemo(() => grouped.filter(isIssue).length, [grouped]);
  // 悬停说明：异常 = 故障 + 上架失败（闲置备用的故障不算），拆开写，两项相加等于胶囊上的数
  const issueTip = React.useMemo(() => { const f = grouped.filter((m) => m.lifecycle === 'failed').length; return `故障 ${issueCount - f} + 上架失败 ${f}`; }, [grouped, issueCount]);

  // 「仅看异常」开着时成员不随实时推送移出（像邮件的「未读」筛选）：恢复正常的那台留在原位、照常显示正常状态，
  // 鼠标下的卡不会被换走、页面不跳。命中过的记在这里（筛选时只加不删，重复加无妨）；切换分组、开关筛选时换一个空的
  const seen = React.useRef({ key: '', ids: new Set<string>() });
  const seenKey = `${team}|${onlyIssues}`;
  if (seen.current.key !== seenKey) seen.current = { key: seenKey, ids: new Set() };
  const pickTeam = React.useCallback((v: string) => upd(v === 'relay' ? { kind: 'relay', team: '' } : { kind: '', team: v === 'all' ? '' : v }), [upd]);
  const toggleIssues = () => upd({ issues: onlyIssues ? '' : '1' });
  const shown = React.useMemo(() => {
    let l = grouped;
    if (onlyIssues) { const ids = seen.current.ids; l = l.filter((m) => (isIssue(m) ? !!ids.add(m.id) : ids.has(m.id))); }
    return sortTable(l, sort); // 稳定排序：相同值保持默认顺序
  }, [grouped, onlyIssues, sort]);
  // 只渲染当前页：KPI 和分组计数按全部机器算（上面的 counts）
  const sizes = PAGE_SIZES[layout === 'cards' ? view : layout];
  const pg = usePage({ total: machines.data && !pending ? shown.length : undefined, sizes, size: sizes[1], reset: [team, onlyIssues, rawSort, layout, view] });
  const liveSort = !!sort && !STATIC_SORT.has(sort.id);
  const stable = useStableOrder(shown, liveSort ? JSON.stringify([team, onlyIssues, rawSort, pg.page, pg.size]) : null);
  const page = React.useMemo(() => stable.slice(pg.offset, pg.offset + pg.size), [stable, pg.offset, pg.size]);
  const layoutKey = page.length > LAYOUT_ANIM_MAX ? null : `${team}|${onlyIssues}|${rawSort}|${view}`;
  // 抽屉的上一台 / 下一台按这里的顺序走（整份筛选结果，不只当前页）；离开首页还给默认顺序
  React.useEffect(() => { setDrawerOrder(stable.map((m) => m.id)); }, [stable]);
  React.useEffect(() => () => setDrawerOrder(null), []);
  // 按到期、本月流量排序时，卡片上露出排序依据
  const show: CardShow | undefined = sort?.id === 'due' ? 'due' : sort?.id === 'traffic' ? 'traffic' : undefined;

  const o = ov.data;
  // 分组选项只随计数变：实时推送每次都换机器列表（counts 跟着重算），台数没变时选项引用不变，分组控件不重画。
  // 0 台的团队不列（选着的那个除外，免得选中项凭空消失）
  const countSig = `${counts.all}|${counts.relay}|${[...counts.byTeam.values()].join()}|${team}`;
  const groups = React.useMemo(() => [
    { value: 'all', label: '全部', count: counts.all },
    ...(counts.relay || team === 'relay' ? [{ value: 'relay', label: '线路机', count: counts.relay }] : []),
    ...(teams.data ?? []).filter((t) => (counts.byTeam.get(t.id) ?? 0) > 0 || t.id === team).map((t) => ({ value: t.id, label: t.name, count: counts.byTeam.get(t.id) ?? 0 })),
  ], [countSig, teams.data]); // eslint-disable-line react-hooks/exhaustive-deps
  const sortValue = SORTS.some((x) => x.value === rawSort) ? rawSort : 'custom';
  const sb = useStuckBar();

  return (
    <div className="stack home">
      {ov.isError && !o && <ErrorNote error={ov.error} onRetry={() => void ov.refetch()} />}
      <OverviewSection o={o} loading={!o && !ov.isError /* 失败时不要一直停在骨架：显示“—”和重试 */} />

      <HomeSection icon={<Server />} label="服务器集群" headRef={pg.anchor}
        title={<h2 className="hsec-title">机器矩阵与实时监控</h2>}
        >
        {/* 吸顶条：团队页签 + 仅看异常 | 排序、密度、视图。滚过去后吸在顶栏下面（毛玻璃 + 底线）；窄屏只吸团队页签那一行。视图切换固定在最右；密度只对卡片视图有意义，其它视图下不显示 */}
        <div className="hbar-sentinel" ref={sb.sentinel} aria-hidden />
        <div className="hbar" ref={sb.bar} data-stuck={sb.stuck ? '' : undefined}>
          <div className="hgroups" role="group" aria-label="筛选">
            <SegmentedOverflow label="分组" value={team} onChange={pickTeam} items={groups} />
            <FilterChip pressed={onlyIssues} onClick={toggleIssues} count={issueCount} tip={issueTip}><AlertTriangle size={13} />仅看异常</FilterChip>
          </div>
          <div className="hctrls">
            <Select sm value={sortValue} onChange={pickSort} options={sortValue === 'custom' ? SORTS_CUSTOM : SORTS} className="w-[150px]" aria-label="排序" />
            {layout === 'cards' && <Segmented sm label="卡片密度" value={view} onChange={setV} items={DENSITY_ITEMS} />}
            <Segmented sm label="视图" value={layout} onChange={setL} items={LAYOUT_ITEMS} />
          </div>
        </div>

        {machines.isError && <ErrorNote error={machines.error} onRetry={() => void machines.refetch()} />}
        {machines.isLoading || pending ? (
          <div className="cards" data-view={view}>{Array.from({ length: 6 }, (_, i) => <SkeletonCard key={i} view={view} />)}</div>
        ) : !machines.data ? null /* 请求失败且没有缓存：上面的错误提示已说明，不要误报“还没有机器” */ : shown.length === 0 ? (
          all.length ? <Empty icon={<Inbox size={28} />} text="没有符合筛选条件的机器" />
            : <Empty icon={<Inbox size={28} />} text="还没有机器"><Button asChild variant="primary"><Link to="/provision"><CloudUpload />上架</Link></Button></Empty>
        ) : (
          <>
            {/* 每页一个容器（key=页码）：翻页、切换卡片 / 环形 / 表格时整块淡入（换卡片密度不重挂、不淡入）。卡片页内：筛选/排序/密度变了（layoutKey 变）才量位置、滑到新位置。
                右键与“⋯”菜单整页共用一份（NodeMenus） */}
            {layout === 'cards' && <NodeMenus rows={page}><div key={`c${pg.page}`} className="cards page-in" data-view={view}>
              {page.map((m) => <NodeCard key={m.id} m={m} view={view} current={m.id === openId} onOpen={open} layoutKey={layoutKey} show={show} recovered={onlyIssues && !isIssue(m)} />)}
            </div></NodeMenus>}
            {layout === 'ring' && <NodeMenus rows={page}><div key={`r${pg.page}`} className="rings page-in">
              {page.map((m) => <RingCard key={m.id} m={m} current={m.id === openId} onOpen={open} show={show} recovered={onlyIssues && !isIssue(m)} />)}
            </div></NodeMenus>}
            {layout === 'table' && <div key="t" className="panel page-in"><MachineTable rows={page} sort={sort} onSort={setSort} openId={openId} onOpen={open} /></div>}
            <Pagination pg={pg} unit="台" />
          </>
        )}
      </HomeSection>
    </div>
  );
}
