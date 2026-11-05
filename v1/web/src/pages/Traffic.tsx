// 流量：按组织时区的自然日看每天的业务流量（下载 / 上传）。顶部今天 / 昨天 / 近 7 天 / 近 30 天，每日总量柱状图，
// 团队成本（月费、近 30 天流量、USD/GB），「节点 × 日期」明细（热度底色按当前表里最大的一格；右侧合计、日均、月费、USD/GB，闲置的节点标灰胶囊），线路机每天的网卡计费量。数据来自 GET /traffic/daily，5 分钟重取（小时汇总的节奏）。
// 区间（?d=，默认 14 天）与团队（?team=）在地址里；KPI 要近 30 天，取数至少取 30 天，表和图在前端截到所选区间
import * as React from 'react';
import { ArrowDownUp, CalendarDays, CalendarRange, ChartColumnStacked, ChevronDown, ChevronUp, Clock } from 'lucide-react';
import type { DailyBiz, DailyNic, TrafficDaily, TrafficDailyNode, TrafficDailyRelay, TrafficDailyTeam } from '@/lib/types';
import { useTeams, useTrafficDaily } from '@/lib/queries';
import { openDrawer } from '@/lib/drawer';
import { meta, nicBilled, trafficRuleLabel, trafficRuleShort } from '@/lib/derive';
import { collator } from '@/lib/sort';
import { parseSort, useUrlState } from '@/lib/urlState';
import * as F from '@/lib/format';
import { InfoTip } from '@/components/ui/info-tip';
import { OptionSelect } from '@/components/ui/option-select';
import { Segmented } from '@/components/ui/segmented';
import { Skeleton } from '@/components/ui/skeleton';
import { Empty, ErrorNote, Flag, PageHead, Panel, Pill, Stat, TableWrap, type SortState } from '@/components/app/primitives';
import { DailyBars, DailyLegend, md, weekday } from '@/components/app/DailyBars';
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
  const cap = (x: { down: number; up: number } | undefined) => (x ? <><span>↓ 下载 {F.str(F.bytes(x.down))}</span><span>↑ 上传 {F.str(F.bytes(x.up))}</span></> : undefined); // 两段各自不断行（.stat-cap > *），放不下时整段换到下一行
  const stat = (x: { down: number; up: number } | undefined) => (x ? F.bytes(total(x)) : F.NA);

  return (
    <div className="stack">
      <PageHead title="流量" note={<>按{tz}的自然日 <InfoTip text={'业务流量：各节点全部入口的下载加上传，换线前在旧线路机上的那段也算这个节点；直出单列一行\n每个字节只算一次，全部合计 = 各行之和\n今天算到目前为止，每 5 分钟更新'} /></>}
        tools={<>
          <div className="w-[140px]"><OptionSelect value={team} onChange={onTeam} allowEmpty="全部团队" placeholder="团队" options={teamOpts} aria-label="团队" /></div>
          <Segmented label="区间" value={range} onChange={onRange} items={RANGES} />
        </>} />

      {q.isError && !d && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <div className="stat-grid" data-stale={q.isPlaceholderData}>
        <Stat tone="blue" icon={<Clock />} label="今天" info="到目前为止" value={<StatNum f={stat(kpi?.today)} />} loading={!kpi && !q.isError} caption={cap(kpi?.today)} />
        <Stat tone="violet" icon={<CalendarDays />} label="昨天" value={<StatNum f={stat(kpi?.yesterday)} />} loading={!kpi && !q.isError} caption={cap(kpi?.yesterday)} />
        <Stat tone="teal" icon={<CalendarRange />} label="近 7 天" info="含今天" value={<StatNum f={stat(kpi?.d7)} />} loading={!kpi && !q.isError} caption={cap(kpi?.d7)} />
        <Stat tone="sky" icon={<ChartColumnStacked />} label="近 30 天" info="含今天" value={<StatNum f={stat(kpi?.d30)} />} loading={!kpi && !q.isError} caption={cap(kpi?.d30)} />
      </div>

      <Panel title="每日总量" sub={d ? `近 ${days} 天 · 合计 ${F.str(F.bytes(total(sumOf(shown))))} · 日均 ${F.str(F.bytes(total(sumOf(shown)) / days))}` : undefined} tools={d && total(sumOf(shown)) > 0 ? <DailyLegend /> : undefined}>
        {!d ? (q.isError ? <div className="chart-empty" style={{ height: 220 }}>加载失败，暂无数据</div> : <Skeleton className="h-[220px]" />) : total(sumOf(shown)) === 0 ? <div className="chart-empty" style={{ height: 220 }}>近 {days} 天没有业务流量</div>
          : <div data-stale={q.isPlaceholderData}><DailyBars days={shown} height={220} /></div>}
      </Panel>

      {q.isError && !d && <Panel title="团队成本"><Empty icon={<ChartColumnStacked size={22} />} text="加载失败，暂无数据" /></Panel>}
      {d && d.teams.length > 0 && <TeamsTable teams={d.teams} picked={team} stale={q.isPlaceholderData} onPick={onTeam} />}
      <NodesTable nodes={d?.nodes} days={days} idle={d?.idle} spare={d?.teams} loading={q.isLoading} failed={q.isError && !d} stale={q.isPlaceholderData} sortRaw={sp.get('sort')} onSort={onSort} filtered={!!team} />
      {d && d.relays.length > 0 && <RelaysTable relays={d.relays} days={days} stale={q.isPlaceholderData} teamName={team ? teamOpts.find((t) => t.value === team)?.label ?? '所选团队' : null} />}
    </div>
  );
}

/** KPI 的读数：数 + 小单位（Stat 的 unit 只收字符串，这里单位跟着数走） */
const StatNum = ({ f }: { f: F.Fmt }) => <>{f.v}{f.u && <small>{f.u}</small>}</>;

/** 打开到最右（最近几天）。滚到底时固定的第一列会盖住半截列（右对齐的数字丢前几位，39.4 GB 看成 9.4 GB）：
 *  把固定的第一列加宽不到一列宽，让最右的滚动位置正好落在列边上（加在表格里，表头底色、行线照样铺到框边） */
function scrollToEnd(w: HTMLElement) {
  const th0 = w.querySelector<HTMLElement>('thead tr:first-child th');
  if (!th0) return;
  th0.style.minWidth = th0.style.width = '';
  w.scrollLeft = w.scrollWidth;
  if (w.scrollLeft <= 0) return;
  const edge = w.scrollLeft + th0.offsetWidth;
  const cut = Array.from(w.querySelectorAll<HTMLElement>('thead tr:first-child th')).slice(1).find((th) => th.offsetLeft < edge && th.offsetLeft + th.offsetWidth > edge);
  if (!cut) return;
  th0.style.minWidth = th0.style.width = `${th0.offsetWidth + cut.offsetLeft + cut.offsetWidth - edge}px`;
  w.scrollLeft = w.scrollWidth;
}

function useScrollEnd(dep: unknown) {
  const ref = React.useRef<HTMLDivElement>(null);
  // 等两帧：TableWrap 的「放得下」标记（data-fit，overflow 为 clip 时滚不动）由尺寸监听在布局之后才摘掉
  React.useEffect(() => {
    let b = 0;
    let w: HTMLElement | null | undefined;
    // 窗口变了列宽跟着变：还停在最右的重新对齐（人自己滚开了就不动）。在不在最右按滚动时记下的算，缩放后可滚宽度已经变了
    let atEnd = true;
    const onScroll = () => { if (w) atEnd = w.scrollLeft >= w.scrollWidth - w.clientWidth - 2; };
    // 从「放得下」缩到「放不下」时，resize 事件先于尺寸监听摘 data-fit，这时滚不动：等两帧再对齐
    let r1 = 0; let r2 = 0;
    const onResize = () => {
      if (!w || !atEnd) return;
      if (!w.hasAttribute('data-fit')) { scrollToEnd(w); return; }
      cancelAnimationFrame(r1); cancelAnimationFrame(r2);
      r1 = requestAnimationFrame(() => { r2 = requestAnimationFrame(() => { if (w) scrollToEnd(w); }); });
    };
    const a = requestAnimationFrame(() => { b = requestAnimationFrame(() => {
      w = ref.current?.querySelector<HTMLElement>('.table-wrap');
      if (!w) return;
      scrollToEnd(w);
      w.addEventListener('scroll', onScroll, { passive: true });
    }); });
    window.addEventListener('resize', onResize);
    return () => { cancelAnimationFrame(a); cancelAnimationFrame(b); cancelAnimationFrame(r1); cancelAnimationFrame(r2); w?.removeEventListener('scroll', onScroll); window.removeEventListener('resize', onResize); };
  }, [dep]);
  return ref;
}

const DateHead = ({ date, today }: { date: string; today: boolean }) => (
  <th className="r th-date" data-tip={today ? '今天，到目前为止' : undefined}><span>{today ? '今天' : md(date)}</span><small>{weekday(date)}</small></th>
);

/** 区间内的网卡：先加总下行、上行再按规则算计费量（同服务端、额度已用）。按天切片后在前端算：换区间时先用上一份数据（placeholder）切出来，不等重取 */
function nicRange(rule: string, daily: DailyNic[]) {
  const rx = daily.reduce((a, x) => a + x.rx, 0); const tx = daily.reduce((a, x) => a + x.tx, 0);
  return { rule, daily, rx, tx, billed: nicBilled(rule, rx, tx) };
}

type NodeSort = 'name' | 'sum' | 'cost' | 'perGb';
/** 每 GB 业务流量的美元：月费 ÷ 按区间日均折成的 30 天流量（同服务端 traffic::usd_per_gb，区间由页面选，所以在这里按所选天数算）；没月费、没流量为 null */
const perGb = (monthlyCents: number | null, bytes: number, days: number) => (monthlyCents == null || bytes <= 0 ? null : monthlyCents / 100 / ((bytes / days) * 30 / 1e9));

const NodesTable = React.memo(function NodesTable({ nodes, days, idle, spare, loading, failed, stale, sortRaw, onSort, filtered }: {
  nodes: TrafficDailyNode[] | undefined; days: number; idle: TrafficDaily['idle'] | undefined; spare: TrafficDailyTeam[] | undefined; loading: boolean; failed: boolean; stale: boolean; sortRaw: string | null; onSort: (s: string) => void; filtered: boolean;
}) {
  // 不续费的备用机：月费灰字、不进底行合计，也不标闲置（同团队成本表）；被扣住不回收的备用机（n.held）照常续费，不灰、计入底行
  const spareIds = React.useMemo(() => new Set((spare ?? []).filter((t) => t.spare).map((t) => t.id)), [spare]);
  const sort = React.useMemo(() => { const s = parseSort(sortRaw); return s && (['name', 'sum', 'cost', 'perGb'] as string[]).includes(s.id) ? s : null; }, [sortRaw]);
  const rows = React.useMemo(() => {
    const list = (nodes ?? []).map((n) => {
      const daily = n.daily.slice(-days); const sum = total(sumOf(daily));
      const inSpare = !!n.team && spareIds.has(n.team.id); const sp = inSpare && !n.held;
      return { n, daily, sum, sp, inSpare, perGb: sp ? null : perGb(n.monthly_usd_cents, sum, days), nic: n.rule && n.nic_daily ? nicRange(n.rule, n.nic_daily.slice(-days)) : null };
    });
    const s = sort ?? DEFAULT_SORT;
    const key = (r: (typeof list)[number]): number | null => (s.id === 'sum' ? r.sum : s.id === 'cost' ? r.n.monthly_usd_cents : r.perGb);
    return list.sort((a, b) => {
      if (s.id === 'name') { const c = collator.compare(a.n.name, b.n.name); return s.desc ? -c : c; }
      const [x, y] = [key(a), key(b)];
      if (x == null || y == null) return x == null && y == null ? 0 : x == null ? 1 : -1; // 没有的排最后，升降序都一样
      return s.desc ? y - x : x - y;
    });
  }, [nodes, days, sort, spareIds]);
  const dates = rows[0]?.daily.map((x) => x.date) ?? [];
  const max = React.useMemo(() => Math.max(0, ...rows.flatMap((r) => r.daily.map((x) => total(x)))), [rows]);
  const foot = React.useMemo(() => dates.map((_, i) => rows.reduce((a, r) => a + total(r.daily[i]), 0)), [rows, dates]);
  // 底行的月费与 USD/GB 只算有月费、不是不续费的备用机的节点（月费与流量对得上）
  const cost = React.useMemo(() => rows.reduce((a, r) => (r.n.monthly_usd_cents == null || r.sp ? a : { cents: a.cents + r.n.monthly_usd_cents, bytes: a.bytes + r.sum }), { cents: 0, bytes: 0 }), [rows]);
  const wrap = useScrollEnd(`${days}:${dates.length}`);
  const last = dates[dates.length - 1];
  const sumAll = foot.reduce((a, v) => a + v, 0);
  const idleTip = (avg7: number | null) => (idle ? `近 ${idle.days} 天日均 ${F.str(F.bytes(avg7 ?? 0))}，低于 ${idle.gb_per_day} GB` : undefined);
  return (
    <Panel title="节点每日流量" count={nodes ? `${rows.length} 个` : undefined} sub={<InfoTip text={'业务流量：下载加上传；格子底色越深流量越大\nVPS 节点的合计下面写网卡计费量（按这台生效的计费方式，同流量额度）\n已退役的机器这段时间有流量的也列出\n闲置：在用、上架满一周，近一周日均业务流量很低'} />} flush>
      <div ref={wrap}>
        <TableWrap data-stale={stale}>
          <table className="dt is-one dt-heat dt-cost">
            <colgroup><col className="col-node" />{dates.map((x) => <col key={x} className="col-day" />)}<col className="col-sum" /><col className="col-aux" /><col className="col-aux" /><col className="col-aux" /></colgroup>
            <thead><tr>
              <SortTh id="name" label="节点" sort={sort} onSort={onSort} />
              {dates.map((x) => <DateHead key={x} date={x} today={x === last} />)}
              <SortTh id="sum" label={`${days} 天合计`} r sort={sort} onSort={onSort} />
              <th className="r" data-tip={`${days} 天合计 ÷ ${days}`}>日均</th>
              <SortTh id="cost" label="月费" r sort={sort} onSort={onSort} tip={'订阅价按周期折成每月、再折成美元（设置 → 汇率）\n没有价格显示 —'} />
              <SortTh id="perGb" label="USD/GB" r sort={sort} onSort={onSort} tip={`月费 ÷ 按所选 ${days} 天日均折成的 30 天业务流量\n没有月费或没有流量显示 —`} />
            </tr></thead>
            <tbody>
              {rows.map(({ n, daily, sum, nic, sp, inSpare, perGb: pg }) => (
                <tr key={`${n.id}${n.direct ? 'd' : ''}`} className="is-link" tabIndex={0} onClick={() => openDrawer(n.id, 'traffic')} onKeyDown={(e) => { if (e.key === 'Enter') openDrawer(n.id, 'traffic'); }}>
                  <td>
                    <div className="tr-node"><Flag country={n.country} />
                      <div>
                        <span className="cell-main tr-name"><span>{n.name}</span>{n.idle && !inSpare && <Pill className="pill-sm" tip={idleTip(n.avg7)}>闲置</Pill>}</span>
                        <span className="sub">{[n.team?.name ?? (n.direct ? '直出' : '未分团队'), n.lifecycle === 'retired' && '已退役'].filter(Boolean).join(' · ')}</span>
                      </div>
                    </div>
                  </td>
                  {daily.map((x, i) => <td key={x.date} className="r" style={heat(total(x), max)} data-tip={total(x) || nic ? `${md(x.date)} · ↓ 下载 ${F.str(F.bytes(x.down))} · ↑ 上传 ${F.str(F.bytes(x.up))}${nic ? `
网卡计费 ${F.str(F.bytes(nic.daily[i].billed))}（${trafficRuleShort(nic.rule)}）` : ''}` : undefined}>{cell(total(x))}</td>)}
                  {/* 有自己网卡的节点（VPS）副行写网卡计费量（同流量额度的口径） */}
                  <td className="r strong">{cell(sum)}{nic && <span className="sub" data-tip={`网卡计费 · ${trafficRuleLabel(nic.rule)}
${days} 天 ↓ 下行 ${F.str(F.bytes(nic.rx))} · ↑ 上行 ${F.str(F.bytes(nic.tx))}，先加总再按规则算`}>计费 {cell(nic.billed)}</span>}</td>
                  <td className="r">{cell(sum / days)}</td>
                  <td className={sp ? 'r t-muted' : 'r'} data-tip={sp ? '不续费的备用机，不计入合计' : undefined}>{F.money(n.monthly_usd_cents, 'USD')}</td>
                  <td className={sp ? 'r t-muted' : 'r'}>{sp ? '—' : F.usdPerGb(pg)}</td>
                </tr>
              ))}
            </tbody>
            {rows.length > 1 && (
              <tfoot><tr>
                <td>全部</td>
                {foot.map((v, i) => <td key={dates[i]} className="r">{cell(v)}</td>)}
                <td className="r">{cell(sumAll)}</td>
                <td className="r">{cell(sumAll / days)}</td>
                <td className="r">{cost.cents ? F.money(cost.cents, 'USD') : '—'}</td>
                <td className="r">{F.usdPerGb(perGb(cost.cents || null, cost.bytes, days))}</td>
              </tr></tfoot>
            )}
          </table>
        </TableWrap>
      </div>
      {loading && <div style={{ padding: 12 }}><Skeleton className="h-[160px]" /></div>}
      {failed && <Empty icon={<ChartColumnStacked size={22} />} text="加载失败，暂无数据" />}
      {nodes && rows.length === 0 && <Empty icon={<ChartColumnStacked size={22} />} text={filtered ? '这个团队没有节点' : '还没有节点'} />}
    </Panel>
  );
});

/** 团队成本：在用节点的月费、近 30 天业务流量、每 GB 多少美元（不随团队筛选变）；点一行按这个团队筛选，再点取消 */
const TeamsTable = React.memo(function TeamsTable({ teams, picked, stale, onPick }: { teams: TrafficDailyTeam[]; picked: string; stale: boolean; onPick: (id: string) => void }) {
  // 「全部」不含不续费的备用机（等放生）；备用团队里被扣住不回收的几台照常续费，月费、流量、没价格台数算进来（服务端备用团队的月费只按它们算）
  const all = React.useMemo(() => teams.filter((t) => !t.spare || t.held > 0).reduce((a, t) => ({ nodes: a.nodes + (t.spare ? t.held : t.nodes), idle: a.idle + t.idle, cents: a.cents + (t.monthly_usd_cents ?? 0), bytes: a.bytes + (t.spare ? t.held_bytes_30d : t.bytes_30d), unpriced: a.unpriced + t.unpriced }), { nodes: 0, idle: 0, cents: 0, bytes: 0, unpriced: 0 }), [teams]);
  const fee = (cents: number | null, unpriced: number) => <span data-tip={unpriced ? `有 ${unpriced} 台没有价格或没有汇率，没算进月费` : undefined}>{F.money(cents, 'USD')}{unpriced > 0 && '*'}</span>;
  return (
    <Panel title="团队成本" sub={<InfoTip text={'月费：在用节点的订阅价折成美元的每月合计\nUSD/GB：月费 ÷ 近 30 天业务流量，不随上面的区间变\n点一行按这个团队筛选'} />} flush>
      <TableWrap data-stale={stale}>
        <table className="dt is-one dt-sum">
          <thead><tr><th>团队</th><th className="r">在用节点</th><th className="r">闲置</th><th className="r">月费</th><th className="r">近 30 天流量</th><th className="r">USD/GB</th></tr></thead>
          <tbody>
            {teams.map((t) => (
              <tr key={t.id} className="is-link" tabIndex={0} aria-selected={picked === t.id || undefined} onClick={() => onPick(picked === t.id ? '' : t.id)} onKeyDown={(e) => { if (e.key === 'Enter') onPick(picked === t.id ? '' : t.id); }}>
                <td><span className="cell-main">{t.name}</span></td>
                <td className="r" data-tip={t.spare && t.nodes > t.held ? `另 ${t.nodes - t.held} 台不续费的备用机不计` : undefined}>{t.spare ? t.held : t.nodes} 台</td>
                <td className="r">{t.idle && !t.spare ? `${t.idle} 台` : '—'}</td>
                <td className={t.spare && !t.held ? 'r t-muted' : 'r'} data-tip={t.spare ? (t.held ? `其中 ${t.held} 台被扣住不回收（软路由出口还指着 / 同机 IP 节点还在用 / 开着自动续费），照常续费，已算进合计；${t.nodes > t.held ? `其余 ${t.nodes - t.held} 台不续费的备用机不计` : '没有不续费的备用机'}` : '不续费的备用机，不计入合计') : undefined}>{t.spare && !t.held ? '—' : fee(t.monthly_usd_cents, t.unpriced)}</td>
                <td className={t.spare && !t.held ? 'r t-muted' : 'r'} data-tip={t.spare && !t.held ? '不续费的备用机，流量不计入合计' : undefined}>{cell(t.bytes_30d)}</td>
                <td className={t.spare && !t.held ? 'r t-muted' : 'r strong'} data-tip={t.spare ? (t.held ? `只按被扣住不回收的 ${t.held} 台算（月费 ÷ 它们近 30 天的业务流量）` : '不续费的备用机，不算每 GB 成本') : undefined}>{t.spare && !t.held ? '—' : F.usdPerGb(t.usd_per_gb)}</td>
              </tr>
            ))}
          </tbody>
          {teams.filter((t) => !t.spare || t.held > 0).length > 1 && (
            <tfoot><tr>
              <td>全部</td><td className="r">{all.nodes} 台</td><td className="r">{all.idle ? `${all.idle} 台` : '—'}</td>
              <td className="r">{fee(all.cents || null, all.unpriced)}</td><td className="r">{cell(all.bytes)}</td><td className="r">{F.usdPerGb(perGb(all.cents || null, all.bytes, 30))}</td>
            </tr></tfoot>
          )}
        </table>
      </TableWrap>
    </Panel>
  );
});

/** 可排序的表头格（同 SortHead 的按钮与图标）：节点名先升序，数字列先降序；sort 为 null 是默认的合计降序 */
function SortTh({ id, label, r, sort, onSort, tip }: { id: NodeSort; label: string; r?: boolean; sort: SortState | null; onSort: (s: string) => void; tip?: string }) {
  const cur = sort ?? DEFAULT_SORT;
  const dir = cur.id === id ? (cur.desc ? 'desc' : 'asc') : undefined;
  // 名称：升序 → 降序 → 默认；合计：降序（默认）⇄ 升序；月费、USD/GB：降序 → 升序 → 默认
  const next = id === 'name' ? (dir === undefined ? 'name' : dir === 'asc' ? '-name' : '')
    : id === 'sum' ? (dir === 'desc' ? 'sum' : '')
    : dir === undefined ? `-${id}` : dir === 'desc' ? id : '';
  return (
    <th className={r ? 'r' : undefined} aria-sort={dir === 'asc' ? 'ascending' : dir === 'desc' ? 'descending' : undefined} data-tip={tip}>
      <button type="button" className="th-sort" data-dir={dir} onClick={() => onSort(next)}>{label}{dir === 'asc' ? <ChevronUp /> : dir === 'desc' ? <ChevronDown /> : <ArrowDownUp />}</button>
    </th>
  );
}

const RelaysTable = React.memo(function RelaysTable({ relays, days, stale, teamName }: { relays: TrafficDailyRelay[]; days: number; stale: boolean; teamName: string | null }) {
  const rows = React.useMemo(() => relays.map((r) => { const daily = r.daily.slice(-days); return { r, daily, business: r.business.slice(-days), sum: nicRange(r.rule, daily).billed }; }), [relays, days]);
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
${days} 天合计 = 先加总下行、上行，再按规则算`}>{trafficRuleShort(r.rule)}{r.lifecycle === 'retired' ? ' · 已退役' : ''}</span></div>
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
