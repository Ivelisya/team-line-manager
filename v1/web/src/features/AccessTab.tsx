// 节点抽屉「访问记录」页签：常访问的目标（横条）、来源 top、每小时连接数（柱状）、最近记录。
// 线路机的抽屉汇总它名下所有节点。数据来自 GET /machines/{id}/access，30 秒轮询。
import { Skeleton } from '@/components/ui/skeleton';
import * as React from 'react';
import { Radar } from 'lucide-react';
import { ACCESS_WINDOWS, WINDOW_DAYS, useAccessRetention, useMachineAccess, type AccessEvent, type AccessWindow, type MachineAccess } from '@/lib/access';
import * as F from '@/lib/format';
import { Segmented } from '@/components/ui/overlays';
import { Button } from '@/components/ui/button';
import { InfoTip } from '@/components/ui/info-tip';
import { BarChart } from '@/components/ui/charts/bar-chart';
import { Bar } from '@/components/ui/charts/bar';
import { BarXAxis } from '@/components/ui/charts/bar-x-axis';
import { BarYAxis } from '@/components/ui/charts/bar-y-axis';
import { Grid } from '@/components/ui/charts/grid';
import { YAxis } from '@/components/ui/charts/y-axis';
import { ChartTooltip } from '@/components/ui/charts/tooltip';
import { useChartStable } from '@/components/ui/charts/chart-context';
import { textWidth } from '@/components/ui/charts/text-width';
import { intTicks } from '@/lib/chart';
import { Empty, ErrorNote, Section, SectionHeader } from '@/components/app/primitives';
import { ResultPill, fmtN } from '@/features/AccessBits';
import { ConnCell, IspPlace, SourceProfile } from '@/features/IpIntelBits';
import type { ProfileWindow } from '@/lib/ipintel';
import '@/styles/access.css';

const cut = (s: string, n = 24) => (s.length > n ? `${s.slice(0, n - 1)}…` : s);
// 已转发用主色（条形是「访问了多少次」，不是「成功」的意思），已阻止红
const ROUTED = { label: '已转发', color: 'var(--chart-1)' };
const BLOCKED = { label: '已阻止', color: 'var(--bad)' };
/** 提示框、纵轴刻度、条尾的连接数带单位 */
const times = (v: number) => `${fmtN(v)} 次`;
const yTimes = (v: number) => (v ? `${fmtN(v)} 次` : '0');
const tipRows = (p: Record<string, unknown>) => [{ ...ROUTED, value: times(Number(p.routed)) }, { ...BLOCKED, value: times(Number(p.blocked)) }];

/** 条尾的总次数（每行一个，紧跟在最右一段后面） */
function TailLabels() {
  const { data, barScale, bandWidth, yScale } = useChartStable();
  if (!barScale || !bandWidth) return null;
  return (
    <g aria-hidden="true">
      {data.map((d) => {
        const y = barScale(String(d.host));
        return y == null ? null : <text key={String(d.host)} className="bar-count" x={(yScale(Number(d.total)) ?? 0) + 6} y={y + bandWidth / 2} dominantBaseline="central">{times(Number(d.total))}</text>;
      })}
    </g>
  );
}

/** 横向条形图：每行一个目标，已转发 + 已阻止叠在一起，条尾直接标总次数。limit：只画前几个（截取放在这里，轮询数据没变时 data 引用不变） */
function HostBars({ items, limit }: { items: MachineAccess['top_domains']; limit?: number }) {
  const data = React.useMemo(() => items.slice(0, limit).map((d) => ({ host: d.dest_host, routed: Math.max(0, d.conns - d.blocked), blocked: d.blocked, total: d.conns })), [items, limit]);
  // 右边留给条尾的「N 次」：按最长的那条算宽度（六位数、带千分位也放得下），至少 72
  const tailW = Math.max(72, ...data.map((d) => Math.ceil(textWidth(times(d.total), 11)) + 14));
  return (
    <BarChart data={data} xDataKey="host" orientation="horizontal" stacked aspectRatio="" style={{ height: Math.max(64, data.length * 26 + 8) }} margin={{ top: 0, right: tailW, bottom: 0, left: 176 }} barGap={0.2} animationDuration={0}>
      <Bar dataKey="routed" fill={ROUTED.color} lineCap={3} roundEnd animate={false} fadedOpacity={0.45} />
      <Bar dataKey="blocked" fill={BLOCKED.color} lineCap={3} roundEnd animate={false} fadedOpacity={0.45} />
      <BarYAxis formatLabel={(v) => cut(v, 26)} />
      <TailLabels />
      <ChartTooltip showDatePill={false} showDots={false} showCrosshair={false} damping={0} rows={tipRows} />
    </BarChart>
  );
}
/** IPv4 按数值比（10.0.0.9 在 10.0.0.10 前面），其余（IPv6）按字面 */
const ipKey = (ip: string) => (/^\d+(\.\d+){3}$/.test(ip) ? ip.split('.').reduce((n, x) => n * 256 + Number(x), 0) : null);
const byIp = (a: string, b: string) => { const x = ipKey(a); const y = ipKey(b); return x != null && y != null ? x - y : a.localeCompare(b); };

const p2 = F.pad2;
/** 提示框标题：这一小时的时段，如 10-21 01:00–02:00 */
const hourSpan = (l: string | number) => { const d = new Date(Number(l)); const e = new Date(Number(l) + 3_600_000); return `${p2(d.getMonth() + 1)}-${p2(d.getDate())} ${p2(d.getHours())}:00–${p2(e.getHours())}:00`; };
/** 每小时连接数柱状图 */
function HourlyBars({ points, height = 160 }: { points: MachineAccess['hourly']; height?: number }) {
  const long = points.length > 30;
  const data = React.useMemo(() => points.map((p) => ({ t: new Date(p.hour).getTime(), routed: Math.max(0, p.conns - p.blocked), blocked: p.blocked, total: p.conns })), [points]);
  const ticks = React.useMemo(() => intTicks(Math.max(0, ...data.map((d) => d.routed + d.blocked))), [data]);
  const leftW = Math.max(64, ...ticks.map((t) => Math.ceil(textWidth(yTimes(t), 11)) + 14)); // 「120,000 次」放得下
  const tick = React.useCallback((l: string) => { const d = new Date(Number(l)); return long ? `${p2(d.getMonth() + 1)}-${p2(d.getDate())}` : `${p2(d.getHours())}:00`; }, [long]);
  return (
    <BarChart data={data} xDataKey="t" stacked aspectRatio="" style={{ height }} margin={{ top: 8, right: 10, bottom: 26, left: leftW }} barGap={long ? 0 : 0.1} yDomain={[0, ticks[ticks.length - 1]]} animationDuration={0}>
      <Grid horizontal vertical={false} strokeDasharray="0" rowTickValues={ticks} numTicksRows={4} />
      <Bar dataKey="routed" fill={ROUTED.color} animate={false} lineCap={long ? 1 : 2} roundEnd fadedOpacity={0.45} />
      <Bar dataKey="blocked" fill={BLOCKED.color} animate={false} lineCap={long ? 1 : 2} roundEnd fadedOpacity={0.45} />
      <BarXAxis formatLabel={tick} minLabelGap={long ? 44 : 40} fadeOnHover={false} />
      <YAxis ticks={ticks} formatValue={yTimes} animate={false} />
      <ChartTooltip showDatePill={false} showDots={false} showCrosshair={false} damping={0} rows={tipRows} title={(p) => hourSpan(Number(p.t))} />
    </BarChart>
  );
}

/** 紧凑的最近记录表（抽屉里用）。时间列放得下「10-21 01:02:33」，不出省略号 */
function RecentTable({ items }: { items: AccessEvent[] }) {
  return (
    <div className="table-wrap is-boxed">
      <table className="dt is-one" style={{ minWidth: 520 }}>
        <colgroup><col style={{ width: 140 }} /><col style={{ width: 136 }} /><col /><col style={{ width: 96 }} /></colgroup>
        <thead><tr><th>时间</th><th>来源</th><th>目标</th><th>结果</th></tr></thead>
        <tbody>
          {items.map((e, i) => (
            <tr key={`${e.ts}-${i}`}>
              <td data-tip={F.dtFull(e.ts)}>{F.dt(e.ts)}:{p2(new Date(e.ts).getSeconds())}</td>
              <td>{e.src_ip ?? '—'}</td>
              <td data-tip={e.dest_host}><span className="strong">{e.dest_host}</span>{e.dest_port != null && <span className="t-sub">:{e.dest_port}</span>}</td>
              {/* 胶囊比单元格宽一点点时 .dt 的省略号会接在后面：这一格不省略 */}
              <td style={{ textOverflow: 'clip' }}><ResultPill conns={1} blocked={e.blocked ? 1 : 0} /></td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

// 只收 id 和是否线路机：抽屉的机器详情几乎每条实时推送都在变，传整台机器的话这里（连同图表）每 3 秒重画一次
export const AccessTab = React.memo(function AccessTab({ id, relay }: { id: string; relay: boolean }) {
  const [picked, setW] = React.useState<AccessWindow>('24h');
  // 时间范围不超过小时汇总的保留期（服务端会 400）；保留期调短后选着的退回 24 小时
  const ret = useAccessRetention().data;
  const maxDays = ret?.hourly_days ?? 90; const rawDays = ret?.raw_days ?? 7;
  const items = React.useMemo(() => ACCESS_WINDOWS.filter((x) => WINDOW_DAYS[x.value] <= maxDays).map((x) => ({ value: x.value, label: x.label })), [maxDays]);
  const w = WINDOW_DAYS[picked] <= maxDays ? picked : '24h';
  const [all, setAll] = React.useState(false);
  const q = useMachineAccess(id, w);
  const d = q.data;
  const long = ACCESS_WINDOWS.find((x) => x.value === w)!.long;
  // 时间范围同其它曲线页签：放在页签顶部并吸顶，默认 24 小时（出错时也留着：可以重试，也可以换一段时间）
  const bar = <div className="tab-sticky"><span>时间范围</span><Segmented sm label="时间范围" value={w} onChange={setW} items={items} /></div>;
  const head = <SectionHeader title="常访问的目标" meta={relay ? `汇总名下所有节点 · 最近 ${long}` : `最近 ${long}`} />;
  if (q.isError && !d) return <>{bar}<Section>{head}<ErrorNote error={q.error} onRetry={() => void q.refetch()} /></Section></>;
  if (!d) return <>{bar}<Skeleton className="h-[160px]" /></>;
  const total = d.top_domains.reduce((s, x) => s + x.conns, 0);
  // 来源按连接数，同次数按 IP 数值
  const sources = [...d.top_sources].sort((a, b) => b.conns - a.conns || byIp(a.src_ip, b.src_ip));
  const none = d.top_domains.length === 0 && d.recent.length === 0;
  // IP 库没装时整列查不到：不留两列「—」
  const intel = sources.some((s) => s.intel);
  // 换时间范围、新数据未到时沿用的旧数据调淡（components.css [data-stale]）；段标题和时间范围控件不调淡
  const stale = q.isPlaceholderData;
  return (
    <>
      {bar}
      <Section>
        {head}
        <div data-stale={stale}>
          {none ? (
            <Empty icon={<Radar size={22} />} text={`最近 ${long}没有访问记录`} />
          ) : d.top_domains.length === 0 ? (
            <div className="hint">这段时间没有汇总数据 <InfoTip text="汇总会稍有延迟，下方的「最近记录」是实时的" /></div>
          ) : (
            <>
              <HostBars items={d.top_domains} limit={all ? undefined : 10} />
              <div className="row row-between" style={{ marginTop: 6 }}>
                <span className="hint">共 {fmtN(total)} 次连接（前 {d.top_domains.length} 个目标）</span>
                {d.top_domains.length > 10 && <Button size="sm" variant="ghost" onClick={() => setAll((v) => !v)}>{all ? '收起' : `展开全部 ${d.top_domains.length} 个`}</Button>}
              </div>
            </>
          )}
        </div>
      </Section>
      {!none && (
        <>
          <SourceProfile id={id} window={(w === '7d' ? '7d' : '24h') as ProfileWindow} relay={relay} />
          <Section>
            <SectionHeader title="每小时连接数" meta={w === '1h' ? '最近 24 小时' : `最近 ${long}`} />
            {/* 服务端按小时补零：全是 0 就是没有汇总，不画一张只有 0–4 刻度的空图 */}
            <div data-stale={stale}>{d.hourly.some((p) => p.conns > 0) ? <HourlyBars points={d.hourly} /> : <div className="hint">这段时间没有汇总数据</div>}</div>
          </Section>
          <Section>
            <SectionHeader title="来源地址" meta="按连接数" />
            <div data-stale={stale}>
              {d.top_sources.length ? (
                <div className="table-wrap is-boxed">
                  <table className="dt is-one" style={{ minWidth: (intel ? 560 : 0) + (relay ? 150 : 0) }}>
                    <colgroup><col style={{ width: 150 }} />{relay && <col style={{ width: 120 }} />}{intel && <><col /><col style={{ width: 120 }} /></>}<col style={{ width: 70 }} /><col style={{ width: 110 }} /></colgroup>
                    <thead><tr><th>来源 IP</th>{relay && <th>节点</th>}{intel && <><th>运营商 · 城市</th><th>接入方式</th></>}<th className="r">连接数</th><th>最近</th></tr></thead>
                    <tbody>
                      {sources.map((s) => (
                        <tr key={`${s.node_id}-${s.src_ip}`}>
                          <td>{s.src_ip}</td>{relay && <td data-tip={s.name}>{s.name}</td>}
                          {intel && <><td><IspPlace intel={s.intel} /></td><td><ConnCell intel={s.intel} /></td></>}
                          <td className="r">{fmtN(s.conns)}</td><td>{F.dt(s.last_ts)}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              ) : <div className="hint">没有来源记录</div>}
            </div>
          </Section>
          <Section>
            <SectionHeader title="最近记录" meta="最近 20 条" />
            <div data-stale={stale}>{d.recent.length ? <RecentTable items={d.recent} /> : <div className="hint">最近 {rawDays} 天没有原始记录</div>}</div>
          </Section>
        </>
      )}
    </>
  );
});
