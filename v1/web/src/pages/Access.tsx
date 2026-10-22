// 访问监控：哪个节点访问了哪些域名 / IP。筛选条（团队 / 节点 / 时间范围 / 搜索 / 目标类型 / 处理结果）即时生效，状态进 URL。
// 页签：域名与 IP / 新出现的目标 / 来源地址 / 节点概览 / 原始记录。数据 30 秒轮询（切走页面暂停）。
// 分页：目标与新目标按 offset 翻页（服务端），来源与节点概览是已取回的列表（前端切页），原始记录按时间游标翻页；每页条数各页签相同。
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { Activity, Radar, Server, ShieldX, Target } from 'lucide-react';
import {
  ACCESS_WINDOWS, EVENTS_PAGE, SOURCES_LIMIT, WINDOW_DAYS, isAccessWindow, useAccessRetention, useAccessEvents, useAccessSources, useAccessSummary, useAccessTargets, useNewTargets,
  type AccessFilter, type AccessNode, type AccessWindow, type TargetSort,
} from '@/lib/access';
import { useMachineBriefs, useTeams } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import { collator } from '@/lib/sort';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { InfoTip, SearchInput } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Segmented, Tabs, TabsList, TabsTrigger } from '@/components/ui/overlays';
import { Empty, ErrorNote, PageHead, Panel, Pill, SortHead, Stat, TableWrap, type SortState } from '@/components/app/primitives';
import { AccessViewSwitch } from '@/components/app/AccessViewSwitch';
import { HostCell, ResultPill, fmtN } from '@/features/AccessBits';
import { CursorPager, Pagination } from '@/components/app/Pagination';
import { scrollBackTo, usePage } from '@/lib/usePage';
import { CONN_FILTER_OPTIONS, ConnCell, IpIntelHint, IpIntelStatusChip, IspPlace, connMatches } from '@/features/IpIntelBits';

type View = 'targets' | 'new' | 'sources' | 'nodes' | 'events';
const VIEWS: { value: View; label: string }[] = [
  { value: 'targets', label: '域名与 IP' }, { value: 'new', label: '新出现的目标' }, { value: 'sources', label: '来源地址' },
  { value: 'nodes', label: '节点概览' }, { value: 'events', label: '原始记录' },
];
const isView = (v: string | null): v is View => VIEWS.some((x) => x.value === v);
const winLabel = (w: AccessWindow) => ACCESS_WINDOWS.find((x) => x.value === w)!.long;
// 选项、每页条数用模块常量：搜索框每敲一个字整页重渲，memo 的下拉 / 分段控件 / 分页器不被新数组击穿
const KIND_OPTS = [{ value: 'domain', label: '域名' }, { value: 'ip', label: 'IP' }];
const ACT_OPTS = [{ value: 'routed', label: '已转发' }, { value: 'blocked', label: '已阻止' }];
const SORT_ITEMS: { value: TargetSort; label: string }[] = [{ value: 'conns', label: '连接数' }, { value: 'last', label: '最近' }, { value: 'first', label: '首次出现' }];
const isTargetSort = (v: string | null): v is TargetSort => SORT_ITEMS.some((x) => x.value === v);
// 每页条数各页签一样（原始记录是服务端固定的一页 EVENTS_PAGE 条）
const SIZES = [50, 100, 200];
const SIZE = 100;
/** 「首次出现」是这个节点第一次访问到它的时间，不受时间范围限制 */
const FIRST_SEEN_TIP = '这个节点第一次访问到它的时间（全部时间，不受时间范围限制）';

export function AccessPage() {
  const [sp, setSp] = useSearchParams();
  const upd = React.useCallback((patch: Record<string, string>) => setSp((p) => {
    const n = new URLSearchParams(p);
    for (const [k, v] of Object.entries(patch)) { if (v) n.set(k, v); else n.delete(k); }
    return n;
  }, { replace: true }), [setSp]);

  // 先取出各个字符串再进依赖：整份 URL 参数（sp）一变就重建 f 的话，打开抽屉（?m=）也会让下面五个 memo 的视图全部重渲染
  const w = sp.get('w');
  // 时间范围跟着保留期走：小时汇总只留 N 天时，超过 N 天的范围不给选（服务端会 400）；地址里带着的退到能选的最长那个
  const retention = useAccessRetention().data;
  const maxDays = retention?.hourly_days ?? 90;
  const windowItems = React.useMemo(() => ACCESS_WINDOWS.filter((x) => WINDOW_DAYS[x.value] <= maxDays).map((x) => ({ value: x.value, label: x.label })), [maxDays]);
  const view = sp.get('view');
  const kind = sp.get('kind');
  const act = sp.get('act');
  const team = sp.get('team') ?? '';
  const node = sp.get('node') ?? '';
  const search = sp.get('q') ?? '';
  const f = React.useMemo<AccessFilter>(() => ({
    window: isAccessWindow(w) ? (WINDOW_DAYS[w] <= maxDays ? w : windowItems[windowItems.length - 1].value) : '24h', team, node, search,
    kind: kind === 'domain' || kind === 'ip' ? kind : '', action: act === 'routed' || act === 'blocked' ? act : '',
  }), [w, team, node, search, kind, act, maxDays, windowItems]);
  const tab: View = isView(view) ? view : 'targets';
  const { open } = useDrawer(); // 一次：各视图的行里不再各自订阅 URL
  // 排序（?sort=）：「域名与 IP」是连接数 / 最近 / 首次出现，「节点概览」是表头排序（-conns）；换页签时清掉
  const sort = sp.get('sort') ?? '';
  const onSort = React.useCallback((v: string) => upd({ sort: v }), [upd]);

  // 搜索框：输入即生效，停手 250ms 后写进 URL
  const [text, setText] = React.useState(f.search);
  // upd 的身份随 URL 变化；放进依赖会让别的筛选一动就重跑这个 effect，把旧文本写回 URL。用 ref 取最新的，只依赖 text。
  const updRef = React.useRef(upd); updRef.current = upd;
  React.useEffect(() => { const t = setTimeout(() => updRef.current({ q: text.trim() }), 250); return () => clearTimeout(t); }, [text]);

  const teams = useTeams().data;
  const teamOpts = React.useMemo(() => (teams ?? []).map((t) => ({ value: t.id, label: t.name })), [teams]);
  // 节点选项与回调保持稳定：搜索框每敲一个字整页重渲时，节点下拉（Select 是 memo）不跟着重画全部选项
  const machines = useMachineBriefs().data;
  const nodeOpts = React.useMemo(() => (machines ?? []).filter((m) => m.kind !== 'relay' && (!f.team || m.team?.id === f.team)).map((m) => ({ value: m.id, label: m.name })), [machines, f.team]);
  const onNode = React.useCallback((v: string) => upd({ node: v }), [upd]);
  const onTeam = React.useCallback((v: string) => upd({ team: v, node: '' }), [upd]);
  const onKind = React.useCallback((v: string) => upd({ kind: v }), [upd]);
  const onAct = React.useCallback((v: string) => upd({ act: v }), [upd]);
  const onWindow = React.useCallback((v: AccessWindow) => upd({ w: v === '24h' ? '' : v }), [upd]);
  const summary = useAccessSummary(f);
  const k = summary.data?.kpi;
  const filtered = !!(f.team || f.node || f.search || f.kind || f.action);
  const clear = React.useCallback(() => { setText(''); upd({ team: '', node: '', q: '', kind: '', act: '', conn: '' }); }, [upd]);
  // 点目标名：搜索框直接换成它（不等防抖）
  const pick = React.useCallback((host: string) => { setText(host); upd({ q: host }); }, [upd]);

  return (
    <div className="stack">
      <PageHead title="访问监控" note={summary.data ? <>统计自 {F.dt(summary.data.window_start)} <InfoTip text="每 30 秒自动刷新" /></> : undefined}
        tools={<><IpIntelStatusChip /><Segmented label="时间范围" value={f.window} onChange={onWindow} items={windowItems} /><AccessViewSwitch value="behavior" /></>} /* 视图切换在最右，与访问规则页同位 */ />

      <div className="row" role="search">
        <SearchInput value={text} onChange={(e) => setText(e.target.value)} placeholder="搜索域名或来源 IP…" aria-label="搜索域名或来源 IP" className="w-full sm:w-[260px]" />
        <div className="w-[140px]"><Select value={f.team} onChange={onTeam} allowEmpty="全部团队" placeholder="团队" options={teamOpts} aria-label="团队" /></div>
        <div className="w-[150px]"><Select value={f.node} onChange={onNode} allowEmpty="全部节点" placeholder="节点" options={nodeOpts} aria-label="节点" /></div>
        <div className="w-[130px]"><Select value={f.kind} onChange={onKind} allowEmpty="全部类型" placeholder="目标类型" options={KIND_OPTS} aria-label="目标类型" /></div>
        <div className="w-[130px]"><Select value={f.action} onChange={onAct} allowEmpty="全部结果" placeholder="处理结果" options={ACT_OPTS} aria-label="处理结果" /></div>
        {filtered && <Button variant="quiet" size="md" onClick={clear}>清除筛选</Button>}
      </div>

      {summary.isError && <ErrNote q={summary} />}
      {/* 换时间范围、筛选后新数字未到时，沿用的旧数字调淡（extras.css [data-stale]）；下面各视图的表格同样 */}
      <div className="stat-grid" data-stale={summary.isPlaceholderData}>
        <Stat tone="blue" icon={<Server />} label="有活动的节点" value={k ? fmtN(k.active_nodes) : '—'} loading={summary.isLoading} caption={`最近 ${winLabel(f.window)}`} />
        <Stat tone="teal" icon={<Activity />} label="连接记录" info="含已阻止的连接" value={k ? fmtN(k.conns) : '—'} loading={summary.isLoading} />
        <Stat tone="violet" icon={<Target />} label="访问目标" info="各节点分别去重后相加：两个节点访问同一个域名算 2 个" value={k ? fmtN(k.targets) : '—'} loading={summary.isLoading} />
        <Stat tone="bad" icon={<ShieldX />} label="已阻止的连接" value={k ? fmtN(k.blocked) : '—'} loading={summary.isLoading} caption={k && k.conns > 0 ? blockedShare(k.blocked, k.conns) : undefined} />
      </div>

      <Tabs value={tab} onValueChange={(v) => upd({ view: v === 'targets' ? '' : v, p: '', sort: '' })}>
        <TabsList>{VIEWS.map((x) => <TabsTrigger key={x.value} value={x.value} noPanel>{x.label}</TabsTrigger>)}</TabsList>
      </Tabs>

      {tab === 'targets' && <TargetsView open={open} f={f} sort={isTargetSort(sort) ? sort : 'conns'} onSort={onSort} filtered={filtered} onClear={clear} onPick={pick} total={summary.data?.kpi.targets} />}
      {tab === 'new' && <NewView open={open} f={f} filtered={filtered} onClear={clear} onPick={pick} />}
      {tab === 'sources' && <SourcesView open={open} f={f} filtered={filtered} onClear={clear} conn={sp.get('conn') ?? ''} upd={upd} />}
      {tab === 'nodes' && <NodesView open={open} f={f} filtered={filtered} onClear={clear} sort={sort} onSort={onSort} />}
      {tab === 'events' && <EventsView open={open} f={f} filtered={filtered} onClear={clear} />}
    </div>
  );
}

/** 空状态：筛选后没有的给「清除筛选」 */
function None({ f, filtered, onClear, what = '访问记录' }: { f: AccessFilter; filtered: boolean; onClear: () => void; what?: string }) {
  return filtered
    ? <Empty icon={<Radar size={22} />} text={`没有符合筛选条件的${what}`}><Button size="sm" variant="quiet" onClick={onClear}>清除筛选</Button></Empty>
    : <Empty icon={<Radar size={22} />} text={`最近 ${winLabel(f.window)}没有${what}`} />;
}
const Loading = () => <div style={{ padding: 16 }}><span className="skel" style={{ width: '60%', height: 14 }} /></div>;
/** 取数出错：带「重试」（同全站的 ErrorNote） */
const ErrNote = ({ q }: { q: { error: unknown; refetch: () => unknown } }) => <ErrorNote error={q.error} onRetry={() => void q.refetch()} />;

/** 节点名：点开抽屉的「访问记录」页签 */
type Open = ReturnType<typeof useDrawer>['open'];
function NodeLink({ id, name, open }: { id: string; name: string; open: Open }) {
  return <button type="button" className="btn btn-link" style={{ maxWidth: '100%' }} onClick={(e) => { e.stopPropagation(); open(id, 'access'); }}><span className="ellipsis">{name}</span></button>;
}

/** total：同一筛选下 summary 的「访问目标」数，正是这张表的总行数。
 *  五个视图都 memo：搜索框打字只改页面里的 text，f 要停手 250ms 写进 URL 后才变，期间不重画表格 */
const TargetsView = React.memo(function TargetsView({ open, f, sort, onSort, filtered, onClear, onPick, total }: { f: AccessFilter; sort: TargetSort; onSort: (s: string) => void; filtered: boolean; onClear: () => void; onPick: (h: string) => void; open: Open; total: number | undefined }) {
  const pg = usePage({ total, sizes: SIZES, size: SIZE, reset: [f, sort] });
  const q = useAccessTargets(f, sort, pg.offset, pg.size);
  const rows = q.data?.items ?? [];
  return (
    <Panel ref={pg.anchor} title="域名与 IP" count={total != null ? `${fmtN(total)} 个` : undefined} sub={<InfoTip text="同一节点的随机子域名已合并为 *.域名；点目标名只看它" />}
      tools={<Segmented sm label="排序" value={sort} onChange={(v) => onSort(v === 'conns' ? '' : v)} items={SORT_ITEMS} />} flush>
      {q.isError && <div style={{ padding: 12 }}><ErrNote q={q} /></div>}
      <TableWrap data-stale={q.isPlaceholderData}>
        <table className="dt" style={{ minWidth: 900 }}>
          <colgroup><col /><col style={{ width: 130 }} /><col style={{ width: 80 }} /><col style={{ width: 96 }} /><col style={{ width: 210 }} /><col style={{ width: 120 }} /><col style={{ width: 104 }} /></colgroup>
          <thead><tr><th>目标</th><th>节点</th><th className="r">来源数</th><th className="r">连接数</th><th>处理结果</th><th><span className="field-label">首次出现<InfoTip text={FIRST_SEEN_TIP} /></span></th><th>最近</th></tr></thead>
          <tbody>
            {rows.map((t) => (
              <tr key={`${t.node_id}|${t.dest_host}`}>
                <td><HostCell host={t.dest_host} kind={t.kind} onPick={onPick} /></td>
                <td><NodeLink id={t.node_id} name={t.name} open={open} /></td>
                <td className="r">{fmtN(t.sources)}</td>
                <td className="r strong">{fmtN(t.conns)}</td>
                <td><ResultPill conns={t.conns} blocked={t.blocked} /></td>
                <td data-tip={F.dtFull(t.first_seen)}>{F.dt(t.first_seen)}</td>
                <td data-tip={F.dtFull(t.last_seen)}>{F.ago(t.last_seen)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </TableWrap>
      {q.isLoading && <Loading />}
      {q.data && rows.length === 0 && <None f={f} filtered={filtered} onClear={onClear} />}
      <Pagination pg={pg} unit="个" />
    </Panel>
  );
});

const NewView = React.memo(function NewView({ f, filtered, onClear, onPick, open }: { f: AccessFilter; filtered: boolean; onClear: () => void; onPick: (h: string) => void; open: Open }) {
  // 页码先于查询算出（偏移量进查询）；总数取上一次的结果。渲染中直接跟上（不等 effect 提交一遍再重渲染）
  const [total, setTotal] = React.useState<number>();
  const pg = usePage({ total, sizes: SIZES, size: SIZE, reset: [f] });
  const q = useNewTargets(f, pg.offset, pg.size);
  if (q.data && q.data.total !== total) setTotal(q.data.total);
  const rows = q.data?.items ?? [];
  return (
    <Panel ref={pg.anchor} title="新出现的目标" count={q.data ? `${fmtN(q.data.total)} 个` : undefined}
      sub={<InfoTip text="该节点第一次访问到的域名 / IP；新目标不等于异常，只作参考" />} flush>
      {q.isError && <div style={{ padding: 12 }}><ErrNote q={q} /></div>}
      <TableWrap data-stale={q.isPlaceholderData}>
        <table className="dt" style={{ minWidth: 760 }}>
          <colgroup><col /><col style={{ width: 130 }} /><col style={{ width: 130 }} /><col style={{ width: 104 }} /><col style={{ width: 96 }} /></colgroup>
          <thead><tr><th>目标</th><th>节点</th><th>首次出现</th><th>最近</th><th className="r">累计连接</th></tr></thead>
          <tbody>
            {rows.map((t) => (
              <tr key={`${t.node_id}|${t.dest_host}`}>
                <td><HostCell host={t.dest_host} kind={t.kind} onPick={onPick} /></td>
                <td><NodeLink id={t.node_id} name={t.name} open={open} /></td>
                <td data-tip={F.dtFull(t.first_seen)}>{F.dt(t.first_seen)}</td>
                <td data-tip={F.dtFull(t.last_seen)}>{F.ago(t.last_seen)}</td>
                <td className="r">{fmtN(t.total)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </TableWrap>
      {q.isLoading && <Loading />}
      {q.data && rows.length === 0 && <None f={f} filtered={filtered} onClear={onClear} what="新出现的目标" />}
      <Pagination pg={pg} unit="个" />
    </Panel>
  );
});

/** 已阻止占比：有阻止但不到 0.1% 时写“不到 0.1%”，不显示成 0.0% */
function blockedShare(blocked: number, conns: number) {
  const p = (blocked / conns) * 100;
  if (blocked > 0 && p < 0.1) return '占不到 0.1%';
  return `占 ${p.toFixed(p < 10 && p % 1 ? 1 : 0)}%`;
}

/** conn：接入方式筛选（?conn=），在取回的来源里筛 */
const SourcesView = React.memo(function SourcesView({ f, filtered, onClear, conn, upd, open }: { f: AccessFilter; filtered: boolean; onClear: () => void; conn: string; upd: (p: Record<string, string>) => void; open: Open }) {
  const q = useAccessSources(f);
  const ct = CONN_FILTER_OPTIONS.some((o) => o.value === conn) ? conn : '';
  const all = q.data?.items ?? [];
  const rows = all.filter((s) => connMatches(s.intel, ct));
  const pg = usePage({ total: q.data ? rows.length : undefined, sizes: SIZES, size: SIZE, reset: [f, ct] });
  // 接口不回总数：取满上限时只知道是连接数最多的前 N 个
  const capped = all.length >= SOURCES_LIMIT;
  const allText = capped ? `前 ${fmtN(SOURCES_LIMIT)} 个` : `${fmtN(all.length)} 个`;
  return (
    <>
      <IpIntelHint />
      <Panel ref={pg.anchor} title="来源地址" count={q.data ? (ct ? `${fmtN(rows.length)} / ${allText}` : allText) : undefined}
        sub={<InfoTip text={`连接到入口的客户端地址，按连接数排序${capped ? `，最多列 ${SOURCES_LIMIT} 个` : ''}；运营商 · 城市 · 接入方式查自中控的离线库，不外发 IP；搜索框在这里匹配来源 IP`} />} flush>
        {q.isError && <div style={{ padding: 12 }}><ErrNote q={q} /></div>}
        <TableWrap data-stale={q.isPlaceholderData}>
          <table className="dt" style={{ minWidth: 820 }}>
            <colgroup><col style={{ width: 150 }} /><col /><col style={{ width: 150 }} /><col style={{ width: 130 }} /><col style={{ width: 90 }} /><col style={{ width: 104 }} /></colgroup>
            <thead>
              <tr>
                <th>来源 IP</th><th>运营商 · 城市</th>
                <th><Select sm value={ct} onChange={(v) => upd({ conn: v })} allowEmpty="全部接入方式" placeholder="接入方式" options={CONN_FILTER_OPTIONS} aria-label="按接入方式筛选" /></th>
                <th>节点</th><th className="r">连接数</th><th>最近</th>
              </tr>
            </thead>
            <tbody>
              {rows.slice(pg.offset, pg.offset + pg.size).map((s) => (
                <tr key={`${s.node_id}|${s.src_ip}`}>
                  <td className="strong">{s.src_ip}</td>
                  <td><IspPlace intel={s.intel} /></td>
                  <td><ConnCell intel={s.intel} /></td>
                  <td><NodeLink id={s.node_id} name={s.name} open={open} /></td>
                  <td className="r">{fmtN(s.conns)}</td>
                  <td data-tip={F.dtFull(s.last_ts)}>{F.ago(s.last_ts)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </TableWrap>
        {q.isLoading && <Loading />}
        {q.data && rows.length === 0 && (ct && all.length > 0
          ? <Empty icon={<Radar size={22} />} text="没有这种接入方式的来源地址"><Button size="sm" variant="quiet" onClick={() => upd({ conn: '' })}>清除接入方式筛选</Button></Empty>
          // 来源按小时汇总时只记了来源 IP，不分目标：搜域名在这里搜不到
          : f.search && !/^[\d.]+$|:/.test(f.search) ? <Empty icon={<Radar size={22} />} text="来源地址只能按 IP 搜索"><Button size="sm" variant="quiet" onClick={onClear}>清除筛选</Button></Empty>
            : <None f={f} filtered={filtered} onClear={onClear} what="来源地址" />)}
        <Pagination pg={pg} unit="个" />
      </Panel>
    </>
  );
});

const NODE_COLS = [
  { id: 'name', header: '节点', first: 'asc' as const }, { id: 'conns', header: '连接数', r: true, first: 'desc' as const },
  { id: 'targets', header: '访问目标', r: true, first: 'desc' as const }, { id: 'domains', header: '域名', r: true, first: 'desc' as const },
  { id: 'sources', header: '来源数', r: true, first: 'desc' as const }, { id: 'blocked', header: '已阻止', r: true, first: 'desc' as const },
  { id: 'last', header: '最近活动', first: 'desc' as const },
];
const NODE_KEY: Record<string, (n: AccessNode) => number | string> = {
  name: (n) => n.name, conns: (n) => n.conns, targets: (n) => n.targets, domains: (n) => n.domains, sources: (n) => n.sources ?? -1, blocked: (n) => n.blocked,
  last: (n) => (n.last_ts ? Date.parse(n.last_ts) : 0),
};
/** sort：?sort= 的原值（「-」开头为降序）；空 = 服务端的顺序 */
const NodesView = React.memo(function NodesView({ f, filtered, onClear, sort: raw, onSort, open }: { f: AccessFilter; filtered: boolean; onClear: () => void; sort: string; onSort: (s: string) => void; open: Open }) {
  const q = useAccessSummary(f);
  const sort = React.useMemo<SortState | null>(() => (NODE_KEY[raw.replace(/^-/, '')] ? { id: raw.replace(/^-/, ''), desc: raw.startsWith('-') } : null), [raw]);
  const rows = React.useMemo(() => {
    const list = q.data?.nodes ?? [];
    const key = sort && NODE_KEY[sort.id];
    if (!key) return list;
    return [...list].sort((a, b) => { const x = key(a); const y = key(b); const c = typeof x === 'number' && typeof y === 'number' ? x - y : collator.compare(String(x), String(y)); return sort.desc ? -c : c; });
  }, [q.data, sort]);
  const pg = usePage({ total: q.data ? rows.length : undefined, sizes: SIZES, size: SIZE, reset: [f, raw] });
  return (
    <Panel ref={pg.anchor} title="节点概览" count={q.data ? `${fmtN(rows.length)} 个节点` : undefined} flush>
      {q.isError && <div style={{ padding: 12 }}><ErrNote q={q} /></div>}
      <TableWrap data-stale={q.isPlaceholderData}>
        <table className="dt" style={{ minWidth: 760 }}>
          <colgroup><col /><col style={{ width: 100 }} /><col style={{ width: 100 }} /><col style={{ width: 90 }} /><col style={{ width: 100 }} /><col style={{ width: 110 }} /><col style={{ width: 130 }} /></colgroup>
          <SortHead cols={NODE_COLS} sort={sort} onSort={(s) => onSort(s ? `${s.desc ? '-' : ''}${s.id}` : '')} />
          <tbody>
            {rows.slice(pg.offset, pg.offset + pg.size).map((n) => (
              <tr key={n.node_id} className="is-link" tabIndex={0} onClick={() => open(n.node_id, 'access')} onKeyDown={(e) => { if (e.key === 'Enter') open(n.node_id, 'access'); }}>
                <td><span className="cell-main"><NodeLink id={n.node_id} name={n.name} open={open} /></span><span className="sub">{n.team ?? '—'}</span></td>
                <td className="r strong">{fmtN(n.conns)}</td>
                <td className="r">{fmtN(n.targets)}</td>
                <td className="r">{fmtN(n.domains)}</td>
                <td className="r">{n.sources == null ? <span className="t-muted" data-tip="按小时汇总的数据不分目标：加了搜索、类型或结果筛选时算不出来源数，切到「1 小时」可以看">—</span> : fmtN(n.sources)}</td>
                <td className="r">{n.blocked > 0 ? <Pill tone="bad">{fmtN(n.blocked)}</Pill> : <span className="t-muted">0</span>}</td>
                <td data-tip={F.dtFull(n.last_ts)}>{F.ago(n.last_ts)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </TableWrap>
      {q.isLoading && <Loading />}
      {q.data && rows.length === 0 && <None f={f} filtered={filtered} onClear={onClear} />}
      <Pagination pg={pg} unit="个" />
    </Panel>
  );
});

const EventsView = React.memo(function EventsView({ f, filtered, onClear, open }: { f: AccessFilter; filtered: boolean; onClear: () => void; open: Open }) {
  const rawDays = useAccessRetention().data?.raw_days ?? 7;
  // 游标栈：cur[i] 是第 i+1 页的 before（上一页的 next_before 原样存，'' = 最新一页）。筛选一变回到最新一页
  const fk = JSON.stringify(f);
  const [st, setSt] = React.useState({ fk, cur: [''] });
  if (st.fk !== fk) setSt({ fk, cur: [''] });
  const cur = st.fk === fk ? st.cur : [''];
  const q = useAccessEvents(f, cur[cur.length - 1]);
  const rows = q.data?.items ?? [];
  const older = q.data?.next_before;
  const top = React.useRef<HTMLElement>(null);
  const go = (next: string[]) => { setSt({ fk, cur: next }); scrollBackTo(top.current); };
  return (
    <Panel ref={top} title="原始记录" sub={<>保留 {rawDays} 天<InfoTip text="最新在前；目标不做合并" /></>} flush>
      {q.isError && <div style={{ padding: 12 }}><ErrNote q={q} /></div>}
      <TableWrap data-stale={q.isPlaceholderData}>
        <table className="dt is-one" style={{ minWidth: 820 }}>
          <colgroup><col style={{ width: 150 }} /><col style={{ width: 130 }} /><col style={{ width: 140 }} /><col /><col style={{ width: 70 }} /><col style={{ width: 96 }} /></colgroup>
          <thead><tr><th>时间</th><th>节点</th><th>来源 IP</th><th>目标</th><th>协议</th><th>结果</th></tr></thead>
          <tbody>
            {rows.map((e, i) => (
              <tr key={`${e.ts}|${e.node_id}|${i}`}>
                <td className="num" data-tip={F.dtFull(e.ts)}>{F.dt(e.ts)}:{F.pad2(new Date(e.ts).getSeconds())}</td>
                <td><NodeLink id={e.node_id} name={e.name} open={open} /></td>
                <td>{e.src_ip ?? '—'}</td>
                <td data-tip={e.dest_host}><span className="strong">{e.dest_host}</span>{e.dest_port != null && <span className="t-sub">:{e.dest_port}</span>}</td>
                <td>{e.network?.toUpperCase() ?? '—'}</td>
                <td>{e.blocked ? <Pill tone="bad">已阻止</Pill> : <Pill tone="ok">已转发</Pill>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </TableWrap>
      {q.isLoading && <Loading />}
      {q.data && rows.length === 0 && <None f={f} filtered={filtered} onClear={onClear} />}
      {/* 新的一页还没到（仍显示着上一页）时不能再往后翻：那时的 next_before 还是上一页的 */}
      <CursorPager page={cur.length} size={EVENTS_PAGE} onNewer={cur.length > 1 ? () => go(cur.slice(0, -1)) : undefined}
        onOlder={older && !q.isPlaceholderData ? () => go([...cur, older]) : undefined} />
    </Panel>
  );
});
