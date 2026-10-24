// 流量：按组织时区的自然日看每天的业务流量（下载 / 上传）。顶部今天 / 昨天 / 近 7 天 / 近 30 天，每日总量柱状图，
// 「节点 × 日期」明细（热度底色按当前表里最大的一格），线路机每天的网卡计费量。数据来自 GET /traffic/daily，5 分钟重取（小时汇总的节奏）。
// 区间（?d=，默认 14 天）与团队（?team=）在地址里；KPI 要近 30 天，取数至少取 30 天，表和图在前端截到所选区间
import * as React from 'react';
import { ArrowDownUp, CalendarDays, CalendarRange, ChartColumnStacked, ChevronDown, ChevronUp, Clock } from 'lucide-react';
import type { DailyBiz, TrafficDailyNode, TrafficDailyRelay } from '@/lib/types';
import { useTeams, useTrafficDaily } from '@/lib/queries';
import { openDrawer } from '@/lib/drawer';
import { meta, nicBilled, trafficRuleLabel } from '@/lib/derive';
import { collator } from '@/lib/sort';
import { parseSort, useUrlState } from '@/lib/urlState';
import * as F from '@/lib/format';
import { InfoTip } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Segmented } from '@/components/ui/overlays';
import { Skeleton } from '@/components/ui/skeleton';
import { Empty, ErrorNote, Flag, PageHead, Panel, Stat, TableWrap, type SortState } from '@/components/app/primitives';
import { DailyBars, md, weekday } from '@/components/app/DailyBars';
import '@/styles/traffic.css';

const RANGES = [{ value: '7', label: '7 天' }, { value: '14', label: '14 天' }, { value: '30', label: '30 天' }, { value: '90', label: '90 天' }];
const DEFAULT_RANGE = '14';
const sumOf = (d: DailyBiz[]) => d.reduce((a, x) => ({ down: a.down + x.down, up: a.up + x.up }), { down: 0, up: 0 });
const total = (x: { down: number; up: number }) => x.down + x.up;
/** 格子里的流量：一个数 + 单位，0 写「—」 */
const cell = (v: number) => (v > 0 ? F.str(F.bytes(v)) : '—');
/** 热度底色：按当前表里最大的一格，0 不上色 */
const heat = (v: number, max: number) => (v > 0 && max > 0 ? { background: `color-mix(in oklab, var(--chart-1) ${Math.round(5 + 25 * (v / max))}%, transparent)` } : undefined);
const DEFAULT_SORT: SortState = { id: 'sum', desc: true };

export function TrafficPage() {
  const [sp, upd] = useUrlState();
  const rawRange = sp.get('d');
  const range = RANGES.some((r) => r.value === rawRange) ? rawRange! : DEFAULT_RANGE;
  const days = Number(range);
  const team = sp.get('team') ?? '';
  const q = useTrafficDaily(Math.max(days, 30), team);
  const d = q.data;
  const teams = useTeams().data;
  const teamOpts = React.useMemo(() => (teams ?? []).map((t) => ({ value: t.id, label: t.name })), [teams]);
  const onTeam = React.useCallback((v: string) => upd({ team: v }), [upd]);
  const onRange = React.useCallback((v: string) => upd({ d: v === DEFAULT_RANGE ? '' : v }), [upd]);
  const onSort = React.useCallback((s: string) => upd({ sort: s }), [upd]);

  // KPI：今天、昨天、近 7 天、近 30 天（取回的至少 30 天）
  const kpi = React.useMemo(() => {
    if (!d) return null;
    const t = d.total; const n = t.length;
    return { today: t[n - 1], yesterday: t[n - 2], d7: sumOf(t.slice(-7)), d30: sumOf(t.slice(-30)) };
  }, [d]);
  const shown = React.useMemo(() => d?.total.slice(-days) ?? [], [d, days]);
  const tz = meta().org_timezone_label;
  const cap = (x: { down: number; up: number } | undefined) => (x ? <>↓ 下载 {F.str(F.bytes(x.down))} · ↑ 上传 {F.str(F.bytes(x.up))}</> : undefined);
  const stat = (x: { down: number; up: number } | undefined) => (x ? F.bytes(total(x)) : F.NA);

  return (
    <div className="stack">
      <PageHead title="流量" note={<>按{tz}的自然日 <InfoTip text={'业务流量：各节点全部入口的下载加上传，换线前在旧线路机上的那段也算这个节点；直出单列一行\n每个字节只算一次，全部合计 = 各行之和\n今天算到目前为止，每 5 分钟更新'} /></>}
        tools={<>
          <div className="w-[140px]"><Select value={team} onChange={onTeam} allowEmpty="全部团队" placeholder="团队" options={teamOpts} aria-label="团队" /></div>
          <Segmented label="区间" value={range} onChange={onRange} items={RANGES} />
        </>} />

      {q.isError && !d && <ErrorNote error={q.error} onRetry={() => void q.refetch()}>流量加载失败：</ErrorNote>}
      <div className="stat-grid" data-stale={q.isPlaceholderData}>
        <Stat tone="blue" icon={<Clock />} label="今天" info="到目前为止" value={<StatNum f={stat(kpi?.today)} />} loading={!kpi} caption={cap(kpi?.today)} />
        <Stat tone="violet" icon={<CalendarDays />} label="昨天" value={<StatNum f={stat(kpi?.yesterday)} />} loading={!kpi} caption={cap(kpi?.yesterday)} />
        <Stat tone="teal" icon={<CalendarRange />} label="近 7 天" info="含今天" value={<StatNum f={stat(kpi?.d7)} />} loading={!kpi} caption={cap(kpi?.d7)} />
        <Stat tone="sky" icon={<ChartColumnStacked />} label="近 30 天" info="含今天" value={<StatNum f={stat(kpi?.d30)} />} loading={!kpi} caption={cap(kpi?.d30)} />
      </div>

      <Panel title="每日总量" sub={d ? `近 ${days} 天 · 合计 ${F.str(F.bytes(total(sumOf(shown))))} · 日均 ${F.str(F.bytes(total(sumOf(shown)) / days))}` : undefined}>
        {!d ? <Skeleton h={220} /> : total(sumOf(shown)) === 0 ? <div className="chart-empty" style={{ height: 220 }}>近 {days} 天没有业务流量</div>
          : <div data-stale={q.isPlaceholderData}><DailyBars days={shown} height={220} /></div>}
      </Panel>

      <NodesTable nodes={d?.nodes} days={days} loading={q.isLoading} stale={q.isPlaceholderData} sortRaw={sp.get('sort')} onSort={onSort} filtered={!!team} />
      {d && d.relays.length > 0 && <RelaysTable relays={d.relays} days={days} stale={q.isPlaceholderData} teamName={team ? teamOpts.find((t) => t.value === team)?.label ?? '所选团队' : null} />}
    </div>
  );
}

/** KPI 的读数：数 + 小单位（Stat 的 unit 只收字符串，这里单位跟着数走） */
const StatNum = ({ f }: { f: F.Fmt }) => <>{f.v}{f.u && <small>{f.u}</small>}</>;

/** 打开到最右（最近几天） */
function useScrollEnd(dep: unknown) {
  const ref = React.useRef<HTMLDivElement>(null);
  // 等两帧：TableWrap 的「放得下」标记（data-fit，overflow 为 clip 时滚不动）由尺寸监听在布局之后才摘掉
  React.useEffect(() => {
    let b = 0;
    const a = requestAnimationFrame(() => { b = requestAnimationFrame(() => { const w = ref.current?.querySelector('.table-wrap'); if (w) w.scrollLeft = w.scrollWidth; }); });
    return () => { cancelAnimationFrame(a); cancelAnimationFrame(b); };
  }, [dep]);
  return ref;
}

const DateHead = ({ date, today }: { date: string; today: boolean }) => (
  <th className="r th-date" data-tip={today ? '今天，到目前为止' : undefined}><span>{today ? '今天' : md(date)}</span><small>{weekday(date)}</small></th>
);

const NodesTable = React.memo(function NodesTable({ nodes, days, loading, stale, sortRaw, onSort, filtered }: {
  nodes: TrafficDailyNode[] | undefined; days: number; loading: boolean; stale: boolean; sortRaw: string | null; onSort: (s: string) => void; filtered: boolean;
}) {
  const sort = React.useMemo(() => { const s = parseSort(sortRaw); return s && (s.id === 'name' || s.id === 'sum') ? s : null; }, [sortRaw]);
  const rows = React.useMemo(() => {
    const list = (nodes ?? []).map((n) => { const daily = n.daily.slice(-days); return { n, daily, sum: total(sumOf(daily)) }; });
    const s = sort ?? DEFAULT_SORT;
    return list.sort((a, b) => { const c = s.id === 'name' ? collator.compare(a.n.name, b.n.name) : a.sum - b.sum; return s.desc ? -c : c; });
  }, [nodes, days, sort]);
  const dates = rows[0]?.daily.map((x) => x.date) ?? [];
  const max = React.useMemo(() => Math.max(0, ...rows.flatMap((r) => r.daily.map((x) => total(x)))), [rows]);
  const foot = React.useMemo(() => dates.map((_, i) => rows.reduce((a, r) => a + total(r.daily[i]), 0)), [rows, dates]);
  const wrap = useScrollEnd(`${days}:${dates.length}`);
  const last = dates[dates.length - 1];
  return (
    <Panel title="节点每日流量" count={nodes ? `${rows.length} 个` : undefined} sub={<InfoTip text={'下载加上传；格子底色越深流量越大\n已退役的机器这段时间有流量的也列出'} />} flush>
      <div ref={wrap}>
        <TableWrap data-stale={stale}>
          <table className="dt is-one dt-heat">
            <colgroup><col className="col-node" />{dates.map((x) => <col key={x} className="col-day" />)}<col className="col-sum" /></colgroup>
            <thead><tr>
              <SortTh id="name" label="节点" sort={sort} onSort={onSort} />
              {dates.map((x) => <DateHead key={x} date={x} today={x === last} />)}
              <SortTh id="sum" label={`${days} 天合计`} r sort={sort} onSort={onSort} />
            </tr></thead>
            <tbody>
              {rows.map(({ n, daily, sum }) => (
                <tr key={`${n.id}${n.direct ? 'd' : ''}`} className="is-link" tabIndex={0} onClick={() => openDrawer(n.id, 'traffic')} onKeyDown={(e) => { if (e.key === 'Enter') openDrawer(n.id, 'traffic'); }}>
                  <td>
                    <div className="tr-node"><Flag country={n.country} />
                      <div><span className="cell-main">{n.name}</span><span className="sub">{[n.team?.name ?? (n.direct ? '直出' : '未分团队'), n.lifecycle === 'retired' && '已退役'].filter(Boolean).join(' · ')}</span></div>
                    </div>
                  </td>
                  {daily.map((x) => <td key={x.date} className="r" style={heat(total(x), max)} data-tip={total(x) ? `${md(x.date)} · ↓ 下载 ${F.str(F.bytes(x.down))} · ↑ 上传 ${F.str(F.bytes(x.up))}` : undefined}>{cell(total(x))}</td>)}
                  <td className="r strong">{cell(sum)}<span className="sub">日均 {cell(sum / days)}</span></td>
                </tr>
              ))}
            </tbody>
            {rows.length > 1 && (
              <tfoot><tr>
                <td>全部</td>
                {foot.map((v, i) => <td key={dates[i]} className="r">{cell(v)}</td>)}
                <td className="r">{cell(foot.reduce((a, v) => a + v, 0))}</td>
              </tr></tfoot>
            )}
          </table>
        </TableWrap>
      </div>
      {loading && <div style={{ padding: 12 }}><Skeleton h={160} /></div>}
      {nodes && rows.length === 0 && <Empty icon={<ChartColumnStacked size={22} />} text={filtered ? '这个团队没有节点' : '还没有节点'} />}
    </Panel>
  );
});

/** 可排序的表头格（同 SortHead 的按钮与图标）：节点名先升序，合计先降序；sort 为 null 是默认的合计降序 */
function SortTh({ id, label, r, sort, onSort }: { id: 'name' | 'sum'; label: string; r?: boolean; sort: SortState | null; onSort: (s: string) => void }) {
  const cur = sort ?? DEFAULT_SORT;
  const dir = cur.id === id ? (cur.desc ? 'desc' : 'asc') : undefined;
  // 名称：升序 → 降序 → 默认；合计：降序（默认）⇄ 升序
  const next = id === 'name' ? (dir === undefined ? 'name' : dir === 'asc' ? '-name' : '') : dir === 'desc' ? 'sum' : '';
  return (
    <th className={r ? 'r' : undefined} aria-sort={dir === 'asc' ? 'ascending' : dir === 'desc' ? 'descending' : undefined}>
      <button type="button" className="th-sort" data-dir={dir} onClick={() => onSort(next)}>{label}{dir === 'asc' ? <ChevronUp /> : dir === 'desc' ? <ChevronDown /> : <ArrowDownUp />}</button>
    </th>
  );
}

const RelaysTable = React.memo(function RelaysTable({ relays, days, stale, teamName }: { relays: TrafficDailyRelay[]; days: number; stale: boolean; teamName: string | null }) {
  const rows = React.useMemo(() => relays.map((r) => { const daily = r.daily.slice(-days); const s = daily.reduce((a, x) => ({ rx: a.rx + x.rx, tx: a.tx + x.tx }), { rx: 0, tx: 0 }); return { r, daily, business: r.business.slice(-days), sum: nicBilled(r.rule, s.rx, s.tx) }; }), [relays, days]);
  const dates = rows[0]?.daily.map((x) => x.date) ?? [];
  const max = React.useMemo(() => Math.max(0, ...rows.flatMap((r) => r.daily.map((x) => x.billed))), [rows]);
  const wrap = useScrollEnd(`${days}:${dates.length}`);
  const last = dates[dates.length - 1];
  return (
    <Panel title="线路机每日网卡计费量" sub={<>{teamName && <span className="t-muted">整台线路机，不分团队</span>}<InfoTip text={'网卡口径：下行 = 流进线路机，上行 = 流出；每天按生效的计费方式算一次，合计先加总下行、上行再算\n悬停格子看下行、上行和经过它的业务流量'} /></>} flush>
      <div ref={wrap}>
        <TableWrap data-stale={stale}>
          <table className="dt is-one dt-heat">
            <colgroup><col className="col-node" />{dates.map((x) => <col key={x} className="col-day" />)}<col className="col-sum" /></colgroup>
            <thead><tr><th>线路机</th>{dates.map((x) => <DateHead key={x} date={x} today={x === last} />)}<th className="r">{days} 天合计</th></tr></thead>
            <tbody>
              {rows.map(({ r, daily, business, sum }) => (
                <tr key={r.id} className="is-link" tabIndex={0} onClick={() => openDrawer(r.id, 'traffic')} onKeyDown={(e) => { if (e.key === 'Enter') openDrawer(r.id, 'traffic'); }}>
                  <td>
                    <div className="tr-node"><Flag country={r.country} />
                      <div><span className="cell-main">{r.name}</span><span className="sub" data-tip={`${trafficRuleLabel(r.rule)}
${days} 天合计 = 先加总下行、上行，再按规则算`}>{trafficRuleLabel(r.rule)}{r.lifecycle === 'retired' ? ' · 已退役' : ''}</span></div>
                    </div>
                  </td>
                  {daily.map((x, i) => <td key={x.date} className="r" style={heat(x.billed, max)} data-tip={`${md(x.date)} · ↓ 下行 ${F.str(F.bytes(x.rx))} · ↑ 上行 ${F.str(F.bytes(x.tx))}\n计费 ${F.str(F.bytes(x.billed))} · ${teamName ? `其中${teamName}经过` : '经过'}的业务流量 ${F.str(F.bytes(total(business[i])))}`}>{cell(x.billed)}</td>)}
                  <td className="r strong">{cell(sum)}<span className="sub">日均 {cell(sum / days)}</span></td>
                </tr>
              ))}
            </tbody>
          </table>
        </TableWrap>
      </div>
    </Panel>
  );
});
