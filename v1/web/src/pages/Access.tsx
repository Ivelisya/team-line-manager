// 访问监控：哪个节点访问了哪些域名 / IP。筛选条（团队 / 节点 / 时间范围 / 搜索 / 目标类型 / 处理结果）即时生效，状态进 URL。
// 页签：域名与 IP / 新出现的目标 / 来源地址 / 节点概览 / 原始记录。数据 30 秒轮询（切走页面暂停）。
// 分页：目标与新目标按 offset 翻页（服务端），来源与节点概览是已取回的列表（前端切页），原始记录按时间游标「较新 / 较早」。
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { Activity, Radar, Server, ShieldX, Target } from 'lucide-react';
import {
  ACCESS_WINDOWS, EVENTS_PAGE, isAccessWindow, useAccessEvents, useAccessSources, useAccessSummary, useAccessTargets, useNewTargets,
  type AccessFilter, type AccessWindow, type TargetSort,
} from '@/lib/access';
import { useMachines, useTeams } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { SearchInput } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Segmented, Tabs, TabsList, TabsTrigger } from '@/components/ui/overlays';
import { Empty, Note, PageHead, Panel, Pill, Stat } from '@/components/app/primitives';
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

export function AccessPage() {
  const [sp, setSp] = useSearchParams();
  const upd = React.useCallback((patch: Record<string, string>) => setSp((p) => {
    const n = new URLSearchParams(p);
    for (const [k, v] of Object.entries(patch)) { if (v) n.set(k, v); else n.delete(k); }
    return n;
  }, { replace: true }), [setSp]);

  const w = sp.get('w');
  const view = sp.get('view');
  const kind = sp.get('kind');
  const act = sp.get('act');
  const f = React.useMemo<AccessFilter>(() => ({
    window: isAccessWindow(w) ? w : '24h', team: sp.get('team') ?? '', node: sp.get('node') ?? '', search: sp.get('q') ?? '',
    kind: kind === 'domain' || kind === 'ip' ? kind : '', action: act === 'routed' || act === 'blocked' ? act : '',
  }), [w, sp, kind, act]);
  const tab: View = isView(view) ? view : 'targets';
  const { open } = useDrawer(); // 一次：各视图的行里不再各自订阅 URL
  const [sort, setSort] = React.useState<TargetSort>('conns');

  // 搜索框：输入即生效，停手 250ms 后写进 URL
  const [text, setText] = React.useState(f.search);
  // upd 的身份随 URL 变化；放进依赖会让别的筛选一动就重跑这个 effect，把旧文本写回 URL。用 ref 取最新的，只依赖 text。
  const updRef = React.useRef(upd); updRef.current = upd;
  React.useEffect(() => { const t = setTimeout(() => updRef.current({ q: text.trim() }), 250); return () => clearTimeout(t); }, [text]);

  const teams = useTeams().data ?? [];
  const machines = useMachines('live').data ?? [];
  const nodes = machines.filter((m) => m.kind !== 'relay' && (!f.team || m.team?.id === f.team));
  const summary = useAccessSummary(f);
  const k = summary.data?.kpi;
  const filtered = !!(f.team || f.node || f.search || f.kind || f.action);
  const clear = () => { setText(''); upd({ team: '', node: '', q: '', kind: '', act: '' }); };

  return (
    <div className="stack">
      <PageHead title="访问监控" note={`每 30 秒刷新${summary.data ? ` · 统计自 ${F.dt(summary.data.window_start)}` : ''}`}
        tools={<><IpIntelStatusChip /><AccessViewSwitch value="behavior" /><Segmented label="时间范围" value={f.window} onChange={(v) => upd({ w: v === '24h' ? '' : v })} items={ACCESS_WINDOWS.map((x) => ({ value: x.value, label: x.label }))} /></>} />

      <div className="row" role="search">
        <SearchInput value={text} onChange={(e) => setText(e.target.value)} placeholder="搜索域名或来源 IP…" aria-label="搜索域名或来源 IP" className="w-full sm:w-[260px]" />
        <div className="w-[140px]"><Select value={f.team} onChange={(v) => upd({ team: v, node: '' })} allowEmpty="全部团队" placeholder="团队" options={teams.map((t) => ({ value: t.id, label: t.name }))} aria-label="团队" /></div>
        <div className="w-[150px]"><Select value={f.node} onChange={(v) => upd({ node: v })} allowEmpty="全部节点" placeholder="节点" options={nodes.map((m) => ({ value: m.id, label: m.name }))} aria-label="节点" /></div>
        <div className="w-[130px]"><Select value={f.kind} onChange={(v) => upd({ kind: v })} allowEmpty="全部类型" placeholder="目标类型" options={[{ value: 'domain', label: '域名' }, { value: 'ip', label: 'IP' }]} aria-label="目标类型" /></div>
        <div className="w-[130px]"><Select value={f.action} onChange={(v) => upd({ act: v })} allowEmpty="全部结果" placeholder="处理结果" options={[{ value: 'routed', label: '已转发' }, { value: 'blocked', label: '已阻止' }]} aria-label="处理结果" /></div>
        {filtered && <Button variant="quiet" size="md" onClick={clear}>清除筛选</Button>}
      </div>

      {summary.isError && <Note tone="bad">{(summary.error as Error).message}</Note>}
      <div className="stat-grid">
        <Stat tone="blue" icon={<Server />} label="有活动的节点" value={k ? fmtN(k.active_nodes) : '—'} loading={summary.isLoading} caption={`最近 ${winLabel(f.window)}`} />
        <Stat tone="teal" icon={<Activity />} label="连接记录" value={k ? fmtN(k.conns) : '—'} loading={summary.isLoading} caption="含已阻止的连接" />
        <Stat tone="violet" icon={<Target />} label="访问目标" value={k ? fmtN(k.targets) : '—'} loading={summary.isLoading} caption="各节点分别去重后相加" />
        <Stat tone="bad" icon={<ShieldX />} label="已阻止的连接" value={k ? fmtN(k.blocked) : '—'} loading={summary.isLoading} caption={k && k.conns > 0 ? blockedShare(k.blocked, k.conns) : undefined} />
      </div>

      <Tabs value={tab} onValueChange={(v) => upd({ view: v === 'targets' ? '' : v, p: '' })}>
        <TabsList>{VIEWS.map((x) => <TabsTrigger key={x.value} value={x.value} noPanel>{x.label}</TabsTrigger>)}</TabsList>
      </Tabs>

      {tab === 'targets' && <TargetsView open={open} f={f} sort={sort} setSort={setSort} filtered={filtered} total={summary.data?.kpi.targets} />}
      {tab === 'new' && <NewView open={open} f={f} filtered={filtered} />}
      {tab === 'sources' && <SourcesView open={open} f={f} filtered={filtered} />}
      {tab === 'nodes' && <NodesView open={open} f={f} filtered={filtered} />}
      {tab === 'events' && <EventsView open={open} f={f} filtered={filtered} />}
    </div>
  );
}

function None({ f, filtered, what = '访问记录' }: { f: AccessFilter; filtered: boolean; what?: string }) {
  return <Empty icon={<Radar size={28} />} text={filtered ? `没有符合筛选条件的${what}` : `最近 ${winLabel(f.window)}没有${what}（需要线路机开启访问日志、且有真实客户端流量）`} />;
}
const Loading = () => <div style={{ padding: 16 }}><span className="skel" style={{ width: '60%', height: 14 }} /></div>;
const ErrNote = ({ e }: { e: unknown }) => <Note tone="bad">{(e as Error).message}</Note>;

/** 节点名：点开抽屉的「访问记录」页签 */
type Open = ReturnType<typeof useDrawer>['open'];
function NodeLink({ id, name, open }: { id: string; name: string; open: Open }) {
  return <button type="button" className="btn btn-link" style={{ maxWidth: '100%' }} onClick={(e) => { e.stopPropagation(); open(id, 'access'); }}><span className="ellipsis">{name}</span></button>;
}

/** total：同一筛选下 summary 的「访问目标」数，正是这张表的总行数 */
function TargetsView({ open, f, sort, setSort, filtered, total }: { f: AccessFilter; sort: TargetSort; setSort: (s: TargetSort) => void; filtered: boolean; open: Open; total: number | undefined }) {
  const pg = usePage({ total, sizes: [50, 100, 200], size: 100, reset: [f, sort] });
  const q = useAccessTargets(f, sort, pg.offset, pg.size);
  const rows = q.data?.items ?? [];
  return (
    <Panel ref={pg.anchor} title="域名与 IP" count={total != null ? `${fmtN(total)} 个` : undefined} sub="同一节点的随机子域名已合并为 *.域名"
      tools={<Segmented sm label="排序" value={sort} onChange={setSort} items={[{ value: 'conns', label: '连接数' }, { value: 'last', label: '最近' }, { value: 'first', label: '首次' }]} />} flush>
      {q.isError && <div style={{ padding: 12 }}><ErrNote e={q.error} /></div>}
      <div className="table-wrap">
        <table className="dt" style={{ minWidth: 900 }}>
          <colgroup><col /><col style={{ width: 130 }} /><col style={{ width: 80 }} /><col style={{ width: 96 }} /><col style={{ width: 210 }} /><col style={{ width: 104 }} /><col style={{ width: 104 }} /></colgroup>
          <thead><tr><th>目标</th><th>节点</th><th className="r">来源数</th><th className="r">连接数</th><th>处理结果</th><th>首次</th><th>最近</th></tr></thead>
          <tbody>
            {rows.map((t) => (
              <tr key={`${t.node_id}|${t.dest_host}`}>
                <td><HostCell host={t.dest_host} kind={t.kind} /></td>
                <td><NodeLink id={t.node_id} name={t.name} open={open} /></td>
                <td className="r">{fmtN(t.sources)}</td>
                <td className="r strong">{fmtN(t.conns)}</td>
                <td><ResultPill conns={t.conns} blocked={t.blocked} /></td>
                <td title={F.dtFull(t.first_seen)}>{F.dt(t.first_seen)}</td>
                <td title={F.dtFull(t.last_seen)}>{F.ago(t.last_seen)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {q.isLoading && <Loading />}
      {q.data && rows.length === 0 && <None f={f} filtered={filtered} />}
      <Pagination pg={pg} unit="个" />
    </Panel>
  );
}

function NewView({ f, filtered, open }: { f: AccessFilter; filtered: boolean; open: Open }) {
  // 页码先于查询算出（偏移量进查询）；总数取上一次的结果
  const [total, setTotal] = React.useState<number>();
  const pg = usePage({ total, sizes: [50, 100, 200], size: 100, reset: [f] });
  const q = useNewTargets(f, pg.offset, pg.size);
  React.useEffect(() => { if (q.data) setTotal(q.data.total); }, [q.data]);
  const rows = q.data?.items ?? [];
  return (
    <Panel ref={pg.anchor} title="新出现的目标" count={q.data ? `${fmtN(q.data.total)} 个` : undefined}
      sub="该节点第一次访问到的域名 / IP；新目标不等于异常，只作参考" flush>
      {q.isError && <div style={{ padding: 12 }}><ErrNote e={q.error} /></div>}
      <div className="table-wrap">
        <table className="dt" style={{ minWidth: 760 }}>
          <colgroup><col /><col style={{ width: 130 }} /><col style={{ width: 130 }} /><col style={{ width: 104 }} /><col style={{ width: 96 }} /></colgroup>
          <thead><tr><th>目标</th><th>节点</th><th>首次出现</th><th>最近</th><th className="r">累计连接</th></tr></thead>
          <tbody>
            {rows.map((t) => (
              <tr key={`${t.node_id}|${t.dest_host}`}>
                <td><HostCell host={t.dest_host} kind={t.kind} /></td>
                <td><NodeLink id={t.node_id} name={t.name} open={open} /></td>
                <td title={F.dtFull(t.first_seen)}>{F.dt(t.first_seen)}<span className="sub">{F.ago(t.first_seen)}</span></td>
                <td>{F.ago(t.last_seen)}</td>
                <td className="r">{fmtN(t.total)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {q.isLoading && <Loading />}
      {q.data && rows.length === 0 && <None f={f} filtered={filtered} what="新出现的目标" />}
      <Pagination pg={pg} unit="个" />
    </Panel>
  );
}

/** 已阻止占比：有阻止但不到 0.1% 时写“不到 0.1%”，不显示成 0.0% */
function blockedShare(blocked: number, conns: number) {
  const p = (blocked / conns) * 100;
  if (blocked > 0 && p < 0.1) return '占不到 0.1%';
  return `占 ${p.toFixed(p < 10 && p % 1 ? 1 : 0)}%`;
}

function SourcesView({ f, filtered, open }: { f: AccessFilter; filtered: boolean; open: Open }) {
  const q = useAccessSources(f);
  const [ct, setCt] = React.useState('');
  const all = q.data?.items ?? [];
  const rows = all.filter((s) => connMatches(s.intel, ct));
  const pg = usePage({ total: q.data ? rows.length : undefined, sizes: [50, 100], size: 50, reset: [f, ct] });
  return (
    <>
      <IpIntelHint />
      <Panel ref={pg.anchor} title="来源地址" count={q.data ? `${rows.length}${ct ? ` / ${all.length}` : ''} 个` : undefined}
        sub="连接到入口的客户端地址，按连接数排序，附运营商 · 城市 · 接入方式（离线库，不外发 IP）；搜索框在这里匹配来源 IP" flush>
        {q.isError && <div style={{ padding: 12 }}><ErrNote e={q.error} /></div>}
        <div className="table-wrap">
          <table className="dt" style={{ minWidth: 820 }}>
            <colgroup><col style={{ width: 150 }} /><col /><col style={{ width: 150 }} /><col style={{ width: 130 }} /><col style={{ width: 90 }} /><col style={{ width: 104 }} /></colgroup>
            <thead>
              <tr>
                <th>来源 IP</th><th>运营商 · 城市</th>
                <th><Select sm value={ct} onChange={setCt} allowEmpty="接入方式" placeholder="接入方式" options={CONN_FILTER_OPTIONS} aria-label="按接入方式筛选" /></th>
                <th>节点</th><th className="r">连接数</th><th>最近</th>
              </tr>
            </thead>
            <tbody>
              {rows.slice(pg.offset, pg.offset + pg.size).map((s) => (
                <tr key={`${s.node_id}|${s.src_ip}`}>
                  <td><span className="mono strong">{s.src_ip}</span></td>
                  <td><IspPlace intel={s.intel} /></td>
                  <td><ConnCell intel={s.intel} /></td>
                  <td><NodeLink id={s.node_id} name={s.name} open={open} /></td>
                  <td className="r">{fmtN(s.conns)}</td>
                  <td title={F.dtFull(s.last_ts)}>{F.ago(s.last_ts)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        {q.isLoading && <Loading />}
        {q.data && rows.length === 0 && (ct && all.length > 0
          ? <Empty icon={<Radar size={28} />} text="没有符合该接入方式的来源地址" hint={<Button size="sm" variant="quiet" onClick={() => setCt('')}>清除接入方式筛选</Button>} />
          : <None f={f} filtered={filtered} what="来源地址" />)}
        <Pagination pg={pg} unit="个" />
      </Panel>
    </>
  );
}

function NodesView({ f, filtered, open }: { f: AccessFilter; filtered: boolean; open: Open }) {
  const q = useAccessSummary(f);
  const rows = q.data?.nodes ?? [];
  const pg = usePage({ total: q.data ? rows.length : undefined, sizes: [50, 100], size: 50, reset: [f] });
  return (
    <Panel ref={pg.anchor} title="节点概览" count={q.data ? `${rows.length} 个节点` : undefined} sub="点一行打开该节点的访问记录" flush>
      {q.isError && <div style={{ padding: 12 }}><ErrNote e={q.error} /></div>}
      <div className="table-wrap">
        <table className="dt" style={{ minWidth: 760 }}>
          <colgroup><col style={{ width: 170 }} /><col style={{ width: 100 }} /><col style={{ width: 100 }} /><col style={{ width: 90 }} /><col style={{ width: 100 }} /><col style={{ width: 110 }} /><col /></colgroup>
          <thead><tr><th>节点</th><th className="r">连接数</th><th className="r">访问目标</th><th className="r">域名</th><th className="r">来源数</th><th className="r">已阻止</th><th>最近活动</th></tr></thead>
          <tbody>
            {rows.slice(pg.offset, pg.offset + pg.size).map((n) => (
              <tr key={n.node_id} className="is-link" tabIndex={0} onClick={() => open(n.node_id, 'access')} onKeyDown={(e) => { if (e.key === 'Enter') open(n.node_id, 'access'); }}>
                <td><span className="cell-main strong">{n.name}</span><span className="sub">{n.team ?? '—'}</span></td>
                <td className="r strong">{fmtN(n.conns)}</td>
                <td className="r">{fmtN(n.targets)}</td>
                <td className="r">{fmtN(n.domains)}</td>
                <td className="r">{fmtN(n.sources)}</td>
                <td className="r">{n.blocked > 0 ? <Pill tone="bad">{fmtN(n.blocked)}</Pill> : <span className="t-muted">0</span>}</td>
                <td title={F.dtFull(n.last_ts)}>{F.ago(n.last_ts)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {q.isLoading && <Loading />}
      {q.data && rows.length === 0 && <None f={f} filtered={filtered} />}
      <Pagination pg={pg} unit="个" />
    </Panel>
  );
}

function EventsView({ f, filtered, open }: { f: AccessFilter; filtered: boolean; open: Open }) {
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
    <Panel ref={top} title="原始记录" sub="最新在前；原始记录只保留 7 天，目标不做合并" flush>
      {q.isError && <div style={{ padding: 12 }}><ErrNote e={q.error} /></div>}
      <div className="table-wrap">
        <table className="dt is-one" style={{ minWidth: 820 }}>
          <colgroup><col style={{ width: 150 }} /><col style={{ width: 130 }} /><col style={{ width: 140 }} /><col /><col style={{ width: 70 }} /><col style={{ width: 96 }} /></colgroup>
          <thead><tr><th>时间</th><th>节点</th><th>来源 IP</th><th>目标</th><th>协议</th><th>结果</th></tr></thead>
          <tbody>
            {rows.map((e, i) => (
              <tr key={`${e.ts}|${e.node_id}|${i}`}>
                <td className="num" title={F.dtFull(e.ts)}>{F.dt(e.ts)}:{F.pad2(new Date(e.ts).getSeconds())}</td>
                <td><NodeLink id={e.node_id} name={e.name} open={open} /></td>
                <td><span className="mono">{e.src_ip ?? '—'}</span></td>
                <td title={e.dest_host}><span className="strong">{e.dest_host}</span>{e.dest_port != null && <span className="t-sub">:{e.dest_port}</span>}</td>
                <td>{e.network?.toUpperCase() ?? '—'}</td>
                <td>{e.blocked ? <Pill tone="bad">已阻止</Pill> : <Pill tone="ok">已转发</Pill>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {q.isLoading && <Loading />}
      {q.data && rows.length === 0 && <None f={f} filtered={filtered} />}
      {/* 新的一页还没到（仍显示着上一页）时不能再往前翻：那时的 next_before 还是上一页的 */}
      <CursorPager page={cur.length} size={EVENTS_PAGE} onNewer={cur.length > 1 ? () => go(cur.slice(0, -1)) : undefined}
        onOlder={older && !q.isPlaceholderData ? () => go([...cur, older]) : undefined} />
    </Panel>
  );
}
