// 访问监控：哪个节点访问了哪些域名 / IP。筛选条（团队 / 节点 / 时间范围 / 搜索 / 目标类型 / 处理结果）即时生效，状态进 URL。
// 页签：域名与 IP / 新出现的目标 / 来源地址 / 节点概览 / 原始记录。数据 30 秒轮询（切走页面暂停）。
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { Radar } from 'lucide-react';
import {
  ACCESS_WINDOWS, isAccessWindow, useAccessEvents, useAccessSources, useAccessSummary, useAccessTargets, useNewTargets,
  type AccessFilter, type AccessWindow, type TargetSort,
} from '@/lib/access';
import { useMachines, useTeams } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { SearchInput } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Segmented, Tabs, TabsList, TabsTrigger } from '@/components/ui/overlays';
import { Empty, KpiTile, Note, PageHead, Panel, Pill } from '@/components/app/primitives';
import { HostCell, ResultPill, fmtN } from '@/features/AccessTab';

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
  const [sort, setSort] = React.useState<TargetSort>('conns');

  // 搜索框：输入即生效，停手 250ms 后写进 URL
  const [text, setText] = React.useState(f.search);
  React.useEffect(() => { const t = setTimeout(() => upd({ q: text.trim() }), 250); return () => clearTimeout(t); }, [text, upd]);

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
        tools={<Segmented label="时间范围" value={f.window} onChange={(v) => upd({ w: v === '24h' ? '' : v })} items={ACCESS_WINDOWS.map((x) => ({ value: x.value, label: x.label }))} />} />

      <div className="row" role="search">
        <SearchInput value={text} onChange={(e) => setText(e.target.value)} placeholder="搜索域名或来源 IP…" aria-label="搜索域名或来源 IP" className="w-full sm:w-[260px]" />
        <div className="w-[140px]"><Select value={f.team} onChange={(v) => upd({ team: v, node: '' })} allowEmpty="全部团队" placeholder="团队" options={teams.map((t) => ({ value: t.id, label: t.name }))} aria-label="团队" /></div>
        <div className="w-[150px]"><Select value={f.node} onChange={(v) => upd({ node: v })} allowEmpty="全部节点" placeholder="节点" options={nodes.map((m) => ({ value: m.id, label: m.name }))} aria-label="节点" /></div>
        <div className="w-[130px]"><Select value={f.kind} onChange={(v) => upd({ kind: v })} allowEmpty="全部类型" placeholder="目标类型" options={[{ value: 'domain', label: '域名' }, { value: 'ip', label: 'IP' }]} aria-label="目标类型" /></div>
        <div className="w-[130px]"><Select value={f.action} onChange={(v) => upd({ act: v })} allowEmpty="全部结果" placeholder="处理结果" options={[{ value: 'routed', label: '已转发' }, { value: 'blocked', label: '已阻止' }]} aria-label="处理结果" /></div>
        {filtered && <Button variant="quiet" size="md" onClick={clear}>清除筛选</Button>}
      </div>

      {summary.isError && <Note tone="bad">{(summary.error as Error).message}</Note>}
      <div className="kpi-grid-wrap">
        <div className="kpi-grid">
          <KpiTile label="有活动的节点" value={k ? fmtN(k.active_nodes) : '—'} loading={summary.isLoading} sub={`最近 ${winLabel(f.window)}`} />
          <KpiTile label="连接记录" value={k ? fmtN(k.conns) : '—'} loading={summary.isLoading} sub="含已阻止的连接" />
          <KpiTile label="访问目标" value={k ? fmtN(k.targets) : '—'} loading={summary.isLoading} sub="各节点分别去重后相加" />
          <KpiTile label="已阻止的连接" value={k ? fmtN(k.blocked) : '—'} loading={summary.isLoading} sub={k && k.conns > 0 ? `占 ${((k.blocked / k.conns) * 100).toFixed(k.blocked / k.conns < 0.1 ? 1 : 0)}%` : ' '} />
        </div>
      </div>

      <Tabs value={tab} onValueChange={(v) => upd({ view: v === 'targets' ? '' : v })}>
        <TabsList>{VIEWS.map((x) => <TabsTrigger key={x.value} value={x.value}>{x.label}</TabsTrigger>)}</TabsList>
      </Tabs>

      {tab === 'targets' && <TargetsView f={f} sort={sort} setSort={setSort} filtered={filtered} />}
      {tab === 'new' && <NewView f={f} filtered={filtered} />}
      {tab === 'sources' && <SourcesView f={f} filtered={filtered} />}
      {tab === 'nodes' && <NodesView f={f} filtered={filtered} />}
      {tab === 'events' && <EventsView f={f} filtered={filtered} />}
    </div>
  );
}

function None({ f, filtered, what = '访问记录' }: { f: AccessFilter; filtered: boolean; what?: string }) {
  return <Empty icon={<Radar size={28} />} text={filtered ? `没有符合筛选条件的${what}` : `最近 ${winLabel(f.window)}没有${what}（需要线路机开启访问日志、且有真实客户端流量）`} />;
}
const Loading = () => <div style={{ padding: 16 }}><span className="skel" style={{ width: '60%', height: 14 }} /></div>;
const ErrNote = ({ e }: { e: unknown }) => <Note tone="bad">{(e as Error).message}</Note>;

/** 节点名：点开抽屉的「访问记录」页签 */
function NodeLink({ id, name }: { id: string; name: string }) {
  const { open } = useDrawer();
  return <button type="button" className="btn btn-link" style={{ maxWidth: '100%' }} onClick={(e) => { e.stopPropagation(); open(id, 'access'); }}><span className="ellipsis">{name}</span></button>;
}

function LoadMore({ has, busy, onClick }: { has: boolean; busy: boolean; onClick: () => void }) {
  if (!has) return null;
  return <div className="dt-foot" style={{ justifyContent: 'center' }}><Button size="sm" loading={busy} onClick={onClick}>加载更多</Button></div>;
}

function TargetsView({ f, sort, setSort, filtered }: { f: AccessFilter; sort: TargetSort; setSort: (s: TargetSort) => void; filtered: boolean }) {
  const q = useAccessTargets(f, sort);
  const rows = q.data?.pages.flatMap((p) => p.items) ?? [];
  return (
    <Panel title="域名与 IP" count={q.data ? `${rows.length}${q.hasNextPage ? '+' : ''} 个` : undefined} sub="同一节点的随机子域名已合并为 *.域名"
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
                <td><NodeLink id={t.node_id} name={t.name} /></td>
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
      <LoadMore has={!!q.hasNextPage} busy={q.isFetchingNextPage} onClick={() => void q.fetchNextPage()} />
    </Panel>
  );
}

function NewView({ f, filtered }: { f: AccessFilter; filtered: boolean }) {
  const q = useNewTargets(f);
  const rows = q.data?.items ?? [];
  return (
    <Panel title="新出现的目标" count={q.data ? `${rows.length}${q.data.next_offset != null ? '+' : ''} 个` : undefined}
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
                <td><NodeLink id={t.node_id} name={t.name} /></td>
                <td>{F.dtFull(t.first_seen)}<span className="sub">{F.ago(t.first_seen)}</span></td>
                <td>{F.ago(t.last_seen)}</td>
                <td className="r">{fmtN(t.total)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {q.isLoading && <Loading />}
      {q.data && rows.length === 0 && <None f={f} filtered={filtered} what="新出现的目标" />}
    </Panel>
  );
}

function SourcesView({ f, filtered }: { f: AccessFilter; filtered: boolean }) {
  const q = useAccessSources(f);
  const rows = q.data?.items ?? [];
  return (
    <Panel title="来源地址" count={q.data ? `${rows.length} 个` : undefined} sub="连接到入口的客户端地址，按连接数排序；搜索框在这里匹配来源 IP" flush>
      {q.isError && <div style={{ padding: 12 }}><ErrNote e={q.error} /></div>}
      <div className="table-wrap">
        <table className="dt is-one" style={{ minWidth: 560 }}>
          <colgroup><col /><col style={{ width: 150 }} /><col style={{ width: 110 }} /><col style={{ width: 130 }} /></colgroup>
          <thead><tr><th>来源 IP</th><th>节点</th><th className="r">连接数</th><th>最近</th></tr></thead>
          <tbody>
            {rows.map((s) => (
              <tr key={`${s.node_id}|${s.src_ip}`}>
                <td><span className="mono strong">{s.src_ip}</span></td>
                <td><NodeLink id={s.node_id} name={s.name} /></td>
                <td className="r">{fmtN(s.conns)}</td>
                <td title={F.dtFull(s.last_ts)}>{F.ago(s.last_ts)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {q.isLoading && <Loading />}
      {q.data && rows.length === 0 && <None f={f} filtered={filtered} what="来源地址" />}
    </Panel>
  );
}

function NodesView({ f, filtered }: { f: AccessFilter; filtered: boolean }) {
  const q = useAccessSummary(f);
  const { open } = useDrawer();
  const rows = q.data?.nodes ?? [];
  return (
    <Panel title="节点概览" count={q.data ? `${rows.length} 个节点` : undefined} sub="点一行打开该节点的访问记录" flush>
      {q.isError && <div style={{ padding: 12 }}><ErrNote e={q.error} /></div>}
      <div className="table-wrap">
        <table className="dt" style={{ minWidth: 760 }}>
          <colgroup><col style={{ width: 170 }} /><col style={{ width: 100 }} /><col style={{ width: 100 }} /><col style={{ width: 90 }} /><col style={{ width: 100 }} /><col style={{ width: 110 }} /><col /></colgroup>
          <thead><tr><th>节点</th><th className="r">连接数</th><th className="r">访问目标</th><th className="r">域名</th><th className="r">来源数</th><th className="r">已阻止</th><th>最近活动</th></tr></thead>
          <tbody>
            {rows.map((n) => (
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
    </Panel>
  );
}

function EventsView({ f, filtered }: { f: AccessFilter; filtered: boolean }) {
  const q = useAccessEvents(f);
  const rows = q.data?.pages.flatMap((p) => p.items) ?? [];
  return (
    <Panel title="原始记录" count={q.data ? `${rows.length}${q.hasNextPage ? '+' : ''} 条` : undefined} sub="最新在前；原始记录只保留 7 天，目标不做合并" flush>
      {q.isError && <div style={{ padding: 12 }}><ErrNote e={q.error} /></div>}
      <div className="table-wrap">
        <table className="dt is-one" style={{ minWidth: 820 }}>
          <colgroup><col style={{ width: 150 }} /><col style={{ width: 130 }} /><col style={{ width: 140 }} /><col /><col style={{ width: 70 }} /><col style={{ width: 96 }} /></colgroup>
          <thead><tr><th>时间</th><th>节点</th><th>来源 IP</th><th>目标</th><th>协议</th><th>结果</th></tr></thead>
          <tbody>
            {rows.map((e, i) => (
              <tr key={`${e.ts}|${e.node_id}|${i}`}>
                <td className="num">{F.dtFull(e.ts)}:{F.pad2(new Date(e.ts).getSeconds())}</td>
                <td><NodeLink id={e.node_id} name={e.name} /></td>
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
      <LoadMore has={!!q.hasNextPage} busy={q.isFetchingNextPage} onClick={() => void q.fetchNextPage()} />
    </Panel>
  );
}
