// 抽屉里带图表的三个页签：连接质量 / 流量 / 主机。单独成文件按需加载，recharts 只在打开这些页签时才下载。
// 图表卡、加载框与时间范围也给软路由抽屉的「曲线」页签用（RouterCharts）。
import * as React from 'react';
import { ChevronRight, Cpu, Gauge, Globe, HardDrive, MemoryStick, Network, Radar, ServerOff } from 'lucide-react';
import type { MachineView, SeriesPoint } from '@/lib/types';
import { usePingSeries } from '@/lib/ping';
import { useReachQuality, type ReachWindowStat } from '@/lib/probePoints';
import type { UseQueryResult } from '@tanstack/react-query';
import { useAvailability, useSeries } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import { AVAIL_LEGEND, availCells, availTitles, fromLabel, hasHost, monthFlowTip, monthTraffic, resourceMeters, successPct } from '@/lib/derive';
import { Segmented } from '@/components/ui/overlays';
import { Skeleton } from '@/components/ui/skeleton';
import { InfoTip } from '@/components/ui/form';
import { Beat, Empty, ErrorNote, LabelValue, LabelValueGrid, Note, Num, Section, SectionHeader, TableWrap } from '@/components/app/primitives';
import { RANGES, TimeChart } from '@/components/app/TimeChart';
import { chartPoll, RANGE_MS, vMs, vPct, vRate, yMs, yPct, yRate } from '@/lib/chart';
import { entriesMeta } from './EntriesTable';
import { Button } from '@/components/ui/button';


// 曲线的线条与格式化放模块常量：抽屉随实时推送重渲染时 TimeChart（memo）的 props 不变，不重画。
// CPU / 内存用首页节点卡的指标色（CPU 蓝、内存紫），上下行与对照序列沿用 --chart-1 / --chart-2
// 只画 TCP：出口验证的耗时是经节点去中控回显服务的完整 HTTPS 往返，不是线路延迟（出口只判通不通，见可用率与入口表）
const L_PROBE = [{ key: 'tcp_ms', label: 'TCP 延迟', color: 'var(--chart-1)' }];
const L_TRAFFIC = [{ key: 'down', label: '↓ 下载', color: 'var(--chart-1)' }, { key: 'up', label: '↑ 上传', color: 'var(--chart-2)' }];
const L_CPU = [{ key: 'cpu_pct', label: 'CPU', color: 'var(--m-cpu)' }];
// 按小时的点（7 天 / 30 天）是一小时的平均，短时冲高被摊平：同画每小时的峰值，色取 CPU 蓝的浅色
const L_CPU_HOURLY = [{ key: 'cpu_pct', label: '平均', color: 'var(--m-cpu)' }, { key: 'cpu_max', label: '峰值', color: 'color-mix(in oklab, var(--m-cpu) 45%, transparent)' }];
const L_MEM = [{ key: 'mem_pct', label: '内存', color: 'var(--m-mem)' }];
const L_DISK = [{ key: 'disk_pct', label: '磁盘', color: 'var(--m-disk)' }];
// 负载同卡片：1 分钟平均负载按核数折成百分比（load_pct 由 HostTab 从 load1 算出）；超过 100% 表示排队，纵轴不封顶
const L_LOAD = [{ key: 'load_pct', label: '负载', color: 'var(--m-load)' }];
const L_NET = [{ key: 'net_rx_bps', label: '↓ 下行', color: 'var(--chart-1)' }, { key: 'net_tx_bps', label: '↑ 上行', color: 'var(--chart-2)' }];
const PCT_DOMAIN: [number, number] = [0, 100];
const yBytes = (v: number) => (v ? F.str(F.bytes(v)) : '0');
const vBytes = (_: string, v: number) => { const b = F.bytes(v); return `${b.v} ${b.u}`; };

export function RangeBar({ range, setRange }: { range: string; setRange: (r: string) => void }) {
  return <Segmented sm label="时间范围" value={range} onChange={setRange} items={RANGES} />;
}
/** 抽屉各曲线页签统一的时间范围条：放在页签顶部并吸顶（往下翻几张曲线时也能换），默认 24 小时；三个页签共用一个范围（?range=，见 lib/drawer） */
export const RangeSticky = ({ range, setRange }: { range: string; setRange: (r: string) => void }) => (
  <div className="tab-sticky"><span>时间范围</span><RangeBar range={range} setRange={setRange} /></div>
);

/** 点不到两个、画不成线时的说明：还在上架、或在所选时间范围内才上架的，是数据还在积累；其余（离线、已退役）是这段时间没有数据 */
const fewText = (m: MachineView, range: string) =>
  m.lifecycle === 'provisioning' || (m.activated_at != null && Date.now() - Date.parse(m.activated_at) < (RANGE_MS[range] ?? 0)) ? '刚上架，数据积累中' : '这段时间没有数据';

/** 图表的加载 / 出错状态：出错给重试，不要显示一张空图；换时间范围、新数据未到时沿用的旧曲线调淡。
 *  点不到两个时只剩一副空坐标轴：同全为 0 的空状态，占住图高、写一句说明（few） */
export function ChartFrame<T extends { points: readonly unknown[] }>({ q, few, height = 180, children }: { q: UseQueryResult<T>; few: string; height?: number; children: React.ReactNode }) {
  // 轮询中偶尔一次取不到：留着上一份曲线（下一轮会再取），只有一份都没有时才显示错误
  if (q.isError && !q.data) return <ErrorNote error={q.error} onRetry={() => void q.refetch()}>曲线加载失败：</ErrorNote>;
  if (q.isLoading) return <Skeleton h={height} className="is-float" />;
  return (
    <div className="chart-frame" data-stale={q.isPlaceholderData} aria-busy={q.isPlaceholderData || undefined}>
      {(q.data?.points.length ?? 0) < 2 ? <div className="chart-empty" style={{ height }}>{few}</div> : children}
    </div>
  );
}

/** 图表卡：一张曲线一张卡，边框、底色与顶部色条取指标色（--cc）。卡头左边图标 + 标题，右边当前值 + 一行小字；
 *  没有当前值的图右边放时间范围（tools） */
export function ChartCard({ c, icon, title, meta, value, caption, tools, slim, children }: { c: string; icon: React.ReactNode; title: string; meta?: React.ReactNode; value?: React.ReactNode; caption?: string; tools?: React.ReactNode; slim?: boolean; children?: React.ReactNode }) {
  return (
    <div className={slim ? 'chart-card is-slim' : 'chart-card'} style={{ '--cc': c } as React.CSSProperties}>
      <div className="chart-card-head">
        <span className="chart-card-title">{icon}{title}{meta && <span className="chart-card-meta">{meta}</span>}</span>
        {value != null && <span className="chart-card-val"><b>{value}</b>{caption && <small>{caption}</small>}</span>}
        {tools}
      </div>
      {children}
    </div>
  );
}

const AVAIL_DAYS = 30;
const AVAIL_TIP = `近 ${AVAIL_DAYS} 天全部探测的成功率：TCP 是线路机到节点的 TCP 握手，出口是经入口访问外网的验证`;
/** 近 30 天可用率：每天一格（旧→新，最后一格是今天），按 TCP 与出口里较差的一种定色，悬停看当天的成功率与次数；
 *  格条上方左边是整段的成功率、右边是时间窗（同卡片心跳条「近 1 小时」的写法）。
 *  线路机这里是名下所有入口（TCP 与出口）的合计，标题写明；卡片心跳格是中控 → 线路机，口径不同。
 *  只依赖机器 id 与是否线路机：实时推送重渲染页签时不跟着重画 */
const AvailabilitySection = React.memo(function AvailabilitySection({ id, relay }: { id: string; relay: boolean }) {
  const q = useAvailability(id, AVAIL_DAYS);
  const d = q.data;
  const tip = relay ? `${AVAIL_TIP}；这里是挂在这台线路机上所有入口的合计` : AVAIL_TIP;
  const caption = !d ? '加载中…' : d.tcp_pct == null && d.egress_pct == null ? '没有探测'
    : <span data-tip={tip}>TCP <b>{successPct(d.tcp_pct)}</b> · 出口 <b>{successPct(d.egress_pct)}</b></span>;
  return (
    <Section>
      <SectionHeader title={relay ? '名下入口可用率' : '可用率'} />
      {q.isError && !q.data ? <ErrorNote error={q.error} onRetry={() => void q.refetch()}>可用率加载失败：</ErrorNote>
        : <Beat n={AVAIL_DAYS} cells={d ? availCells(d.items) : []} titles={d && availTitles(d.items)} caption={caption} win={`近 ${AVAIL_DAYS} 天 · 每天一格`} legend={AVAIL_LEGEND} stale={!d} />}
    </Section>
  );
});

const REACH_WINDOWS = [['24h', '近 24 小时'], ['7d', '近 7 天'], ['30d', '近 30 天']] as const;
const reachCell = (w: ReachWindowStat | null) => (!w || !w.total ? '—'
  : <span data-tip={`${w.ok}/${w.total} 次连上${w.median_ms == null ? '' : ` · 延迟中位 ${Math.round(w.median_ms)} ms`}`}>
    <b className="num">{successPct((100 * w.ok) / w.total)}</b>{w.avg_ms != null && <span className="t-sub"> · {Math.round(w.avg_ms)} ms</span>}</span>);
/** 线路机的客户入口连通性：各路来源（中控、各探测点）连它客户入口的成功率与平均建连延迟（中控只判通不通），来自小时汇总。
 *  卡片心跳格只看主来源近 1 小时；这里给长期的、各路并排的（对比香港与中控、事后核对投诉） */
const ReachQualitySection = React.memo(function ReachQualitySection({ id }: { id: string }) {
  const q = useReachQuality(id);
  const s = q.data?.sources ?? [];
  return (
    <Section>
      <SectionHeader title={<>入口连通性<InfoTip text={'中控和各探测点连这台线路机入口的成功率和平均延迟\n探测点自己网络不通的那段不算；中控只看通不通，没有延迟'} /></>} />
      {q.isError && !q.data ? <ErrorNote error={q.error} onRetry={() => void q.refetch()}>连通性加载失败：</ErrorNote>
        : q.isLoading ? <Skeleton h={72} />
        : !s.length ? <div className="hint">还没有探测记录</div>
        : (
          <TableWrap>
            <table className="dt is-one" style={{ minWidth: 440 }}>
              <thead><tr><th>来源</th>{REACH_WINDOWS.map(([k, l]) => <th key={k}>{l}</th>)}</tr></thead>
              <tbody>
                {s.map((x) => (
                  <tr key={x.source}>
                    <td><span className="cell-main">{x.label}</span>{x.primary && <span className="sub">主来源</span>}</td>
                    {REACH_WINDOWS.map(([k]) => <td key={k}>{reachCell(x.windows[k])}</td>)}
                  </tr>
                ))}
              </tbody>
            </table>
          </TableWrap>
        )}
    </Section>
  );
});

const PING_COLORS = ['var(--chart-1)', 'var(--chart-2)', 'var(--chart-3)', 'var(--m-cpu)', 'var(--m-mem)', 'var(--m-disk)', 'var(--m-load)', 'var(--fg-3)'];
/** 线路机的三网 PING 曲线：延迟（各目标一条线，1h–24h 每分钟的中位数，7d / 30d 每小时平均）与丢包率。
 *  同一轮各目标的时刻相同，按时刻合成一组点（s<槽位> 延迟、l<槽位> 丢包） */
const PingSection = React.memo(function PingSection({ id, range }: { id: string; range: string }) {
  const q = usePingSeries(id, range, chartPoll(range));
  const { points, ms, loss } = React.useMemo(() => {
    const by = new Map<string, SeriesPoint>();
    for (const s of q.data?.series ?? []) for (const p of s.points) {
      const o = by.get(p.ts) ?? { ts: p.ts };
      o[`s${s.slot}`] = p.rtt_ms; o[`l${s.slot}`] = p.loss_pct;
      by.set(p.ts, o);
    }
    const ser = q.data?.series ?? [];
    return {
      points: [...by.values()].sort((a, b) => a.ts.localeCompare(b.ts)),
      ms: ser.map((s) => ({ key: `s${s.slot}`, label: s.name, color: PING_COLORS[s.slot % PING_COLORS.length] })),
      loss: ser.map((s) => ({ key: `l${s.slot}`, label: s.name, color: PING_COLORS[s.slot % PING_COLORS.length] })),
    };
  }, [q.data]);
  if (q.data && !q.data.series.length) return null;
  const pq = { ...q, data: q.data && { points } } as UseQueryResult<{ points: SeriesPoint[] }>;
  return (
    <Section>
      <ChartCard c="var(--chart-1)" icon={<Radar />} title="三网 PING 延迟" meta="线路机 → 各目标 · 每分钟 5 次的中位数">
        <ChartFrame q={pq} few="这段时间没有数据"><TimeChart full points={points} range={range} kind="line" lines={ms} yFmt={yMs} valueFmt={vMs} clip /></ChartFrame>
      </ChartCard>
      <ChartCard c="var(--chart-2)" icon={<Radar />} title="三网 PING 丢包" meta="每分钟 5 次里没通的比例">
        <ChartFrame q={pq} few="这段时间没有数据"><TimeChart full points={points} range={range} kind="line" lines={loss} yFmt={yPct} valueFmt={vPct} domain={PCT_DOMAIN} clip /></ChartFrame>
      </ChartCard>
    </Section>
  );
});

export function QualityTab({ m }: { m: MachineView }) {
  const { range, setRange } = useDrawer();
  const s = useSeries(m.id, 'probe', range, true, chartPoll(range));
  return (
    <>
      {/* 可用率（近 30 天）与入口连通性（24 小时 / 7 天 / 30 天）是固定时间段，不跟时间范围走：放在时间范围条上面，下面的曲线才归它管 */}
      <AvailabilitySection id={m.id} relay={m.kind === 'relay'} />
      {m.kind === 'relay' && <ReachQualitySection id={m.id} />}
      <RangeSticky range={range} setRange={setRange} />
      {m.kind === 'relay' && <PingSection id={m.id} range={range} />}
      <Section>
        {/* 多条入口时曲线是各入口的平均（序列接口不分入口），写明几条、是哪几台线路机 */}
        <ChartCard c="var(--chart-1)" icon={<Radar />} title="TCP 延迟" meta={m.kind !== 'relay' && m.entries.length > 1
          ? <span data-tip={`${m.entries.map((e) => e.relay.name).join('、')} 的平均`}>线路机 → 节点 · {m.entries.length} 条入口平均</span> : '线路机 → 节点'}>
          <ChartFrame q={s} few={fewText(m, range)}>
            <TimeChart full points={s.data?.points} range={s.data?.range ?? range} kind="line" lines={L_PROBE} yFmt={yMs} valueFmt={vMs} clip />
          </ChartFrame>
        </ChartCard>
      </Section>
      <EntriesLink m={m} />
    </>
  );
}

/** 连接质量、流量页签不再各放一张入口表（列和概览的几乎一样）：只留一行计数，点「查看」回概览的入口表（含每条的 TCP、出口与本月业务流量） */
function EntriesLink({ m }: { m: MachineView }) {
  const { setTab } = useDrawer();
  if (!m.entries.length) return null;
  return (
    <Section>
      <SectionHeader title="入口" meta={entriesMeta(m)}><Button variant="quiet" size="sm" onClick={() => setTab('overview')}>查看<ChevronRight size={14} /></Button></SectionHeader>
    </Section>
  );
}

export function TrafficTab({ m }: { m: MachineView }) {
  const { range, setRange } = useDrawer();
  const s = useSeries(m.id, 'traffic', range, true, chartPoll(range));
  const t = monthTraffic(m); const nic = m.nic; const from = m.month_counted_from;
  return (
    <>
      <RangeSticky range={range} setRange={setRange} />
      {/* 数据不完整时「X 起」只在这里写一次（短句 + (i) 全句），下面各数字后面不再重复 */}
      {from && <div className="hint">流量数据从 {fromLabel(from)} 起 <InfoTip text={monthFlowTip(m)} /></div>}
      {/* 有 agent 的机器分两组：网卡（供应商计费口径，下行 / 上行）与业务（Xray 入口，下载 / 上传）；家宽代理、同机 IP 节点只有业务一组 */}
      {nic && (
        <Section>
          <SectionHeader title="网卡（供应商计费口径）" />
          <LabelValueGrid cols2>
            <LabelValue label="今日 ↓ 下行"><Num f={F.bytes(nic.today_rx)} /></LabelValue>
            <LabelValue label="今日 ↑ 上行"><Num f={F.bytes(nic.today_tx)} /></LabelValue>
            <LabelValue label="本月 ↓ 下行"><Num f={F.bytes(nic.month_rx)} /></LabelValue>
            <LabelValue label="本月 ↑ 上行"><Num f={F.bytes(nic.month_tx)} /></LabelValue>
          </LabelValueGrid>
        </Section>
      )}
      <Section>
        <SectionHeader title={<>业务（Xray 入口）{m.host_machine && <> <InfoTip text="网卡与宿主机共用，这里是经它的业务流量。" /></>}</>} />
        <LabelValueGrid cols2>
          <LabelValue label="今日 ↓ 下载"><Num f={F.bytes(t.today_down)} /></LabelValue>
          <LabelValue label="今日 ↑ 上传"><Num f={F.bytes(t.today_up)} /></LabelValue>
          <LabelValue label="本月 ↓ 下载"><Num f={F.bytes(t.down)} /></LabelValue>
          <LabelValue label="本月 ↑ 上传"><Num f={F.bytes(t.up)} /></LabelValue>
        </LabelValueGrid>
      </Section>
      <Section>
        <ChartCard c="var(--chart-1)" icon={<Globe />} title="业务流量" meta={<InfoTip text={s.data?.resolution === '1h' ? '每个点是这一小时的流量' : '每个点是这一分钟的流量'} />}>
          <ChartFrame q={s} few={fewText(m, range)}>
            <TimeChart full points={s.data?.points} range={s.data?.range ?? range} lines={L_TRAFFIC} yFmt={yBytes} valueFmt={vBytes} yBase={1000} empty="这段时间没有业务流量" clip />
          </ChartFrame>
        </ChartCard>
      </Section>
      <EntriesLink m={m} />
    </>
  );
}

export function HostTab({ m }: { m: MachineView }) {
  const { range, setRange } = useDrawer();
  // 家宽代理没有主机；其它机器即使现在没有读数（agent 离线、还没上报），历史曲线照样能看——排查掉线前发生了什么正要看它
  const agentful = m.kind !== 'proxy';
  const s = useSeries(m.id, 'host', range, agentful, chartPoll(range));
  const cores = m.agent?.cores ?? null;
  // 负载曲线：load1 按核数折成百分比（同卡片）；核数不知道时没有这条
  const loadPoints = React.useMemo(() => (cores && s.data?.points.some((p) => typeof p.load1 === 'number') ? s.data.points.map((p) => ({ ...p, load_pct: typeof p.load1 === 'number' ? (p.load1 / cores) * 100 : null })) : null), [s.data, cores]);
  if (!agentful) return <Empty icon={<ServerOff size={22} />} text="家宽代理没有主机指标" />;
  const h = hasHost(m) ? m.host! : null;
  const r = h ? resourceMeters(m)! : null;
  const few = fewText(m, range);
  const hourly = s.data?.resolution === '1h';
  return (
    <>
      <RangeSticky range={range} setRange={setRange} />
      <Section>
        <SectionHeader title="当前" meta={h ? `更新于 ${F.ago(h.ts)}` : m.agent?.last_report_at ? `最后上报于 ${F.ago(m.agent.last_report_at)}` : '还没上报过'} />
        {h && r ? (
          <LabelValueGrid>
            <LabelValue label="内存"><Num f={F.mem(h.mem_used)} /> <span className="t-sub">/ <Num f={F.mem(h.mem_total)} /></span></LabelValue>
            <LabelValue label="磁盘"><Num f={F.mem(h.disk_used)} /> <span className="t-sub">/ <Num f={F.mem(h.disk_total)} /></span></LabelValue>
            {/* 同卡片的负载格条：百分比一行，「1 分钟 0.13 · 1 核」放第二行 */}
            <LabelValue label="负载" wrap>{h.load1 == null ? <span className="t-sub">{r.load.sub}</span>
              : <><Num f={r.load.value} /><div className="t-sub">{r.load.sub}</div></>}</LabelValue>
            <LabelValue label="TCP 连接数"><span className="num">{h.tcp_conns ?? '—'}</span></LabelValue>
          </LabelValueGrid>
        ) : <div className="hint">agent 离线，没有当前读数</div>}
        {m.host_machine && <div style={{ marginTop: 8 }}><Note>这是宿主机「{m.host_machine.name}」的读数 <InfoTip text="同机 IP 节点和宿主机是同一台机器，CPU、内存、磁盘与网卡都共用。" /></Note></div>}
      </Section>
      {/* 图表卡同属一段，之间不再画分隔线 */}
      <Section className="chart-stack">
        <ChartCard c="var(--m-cpu)" icon={<Cpu />} title="CPU" meta={hourly ? '每小时平均与峰值' : undefined} value={h ? <Num f={F.pct(h.cpu_pct, 1)} /> : '—'} caption="使用率"><ChartFrame q={s} few={few} height={140}><TimeChart full holeLabel="agent 离线" points={s.data?.points} range={s.data?.range ?? range} lines={hourly ? L_CPU_HOURLY : L_CPU} yFmt={yPct} valueFmt={vPct} domain={PCT_DOMAIN} height={140} /></ChartFrame></ChartCard>
        <ChartCard c="var(--m-mem)" icon={<MemoryStick />} title="内存" value={r ? <Num f={F.pct(r.mem.pct, 1)} /> : '—'} caption="使用率"><ChartFrame q={s} few={few} height={140}><TimeChart full holeLabel="agent 离线" points={s.data?.points} range={s.data?.range ?? range} lines={L_MEM} yFmt={yPct} valueFmt={vPct} domain={PCT_DOMAIN} height={140} /></ChartFrame></ChartCard>
        <ChartCard c="var(--m-disk)" icon={<HardDrive />} title="磁盘" value={r ? <Num f={F.pct(r.disk.pct, 1)} /> : '—'} caption="使用率"><ChartFrame q={s} few={few} height={140}><TimeChart full holeLabel="agent 离线" points={s.data?.points} range={s.data?.range ?? range} lines={L_DISK} yFmt={yPct} valueFmt={vPct} domain={PCT_DOMAIN} height={140} /></ChartFrame></ChartCard>
        {/* 容器里拿不到负载（或小时汇总不含）：图卡缩成一行，不占一整张图的高度 */}
        {cores != null && (s.data && !loadPoints
          ? <ChartCard slim c="var(--m-load)" icon={<Gauge />} title="负载" meta={<InfoTip text={`1 分钟平均负载除以 ${cores} 核。超过 100% 说明有进程在排队`} />} value="—" caption={hourly ? '小时汇总不含负载' : '容器里读不到负载'} />
          : <ChartCard c="var(--m-load)" icon={<Gauge />} title="负载" meta={<InfoTip text={`1 分钟平均负载除以 ${cores} 核。超过 100% 说明有进程在排队`} />} value={r ? <Num f={r.load.value} /> : '—'} caption="1 分钟平均"><ChartFrame q={s} few={few} height={140}>{loadPoints && <TimeChart full holeLabel="agent 离线" points={loadPoints} range={s.data?.range ?? range} lines={L_LOAD} yFmt={yPct} valueFmt={vPct} height={140} />}</ChartFrame></ChartCard>)}
        <ChartCard c="var(--chart-1)" icon={<Network />} title="网卡速率" value={h ? <>↓ <Num f={F.rate(h.net_rx_bps)} /> <span className="t-sub">·</span> ↑ <Num f={F.rate(h.net_tx_bps)} /></> : '—'} caption="当前速率"><ChartFrame q={s} few={few} height={160}><TimeChart full holeLabel="agent 离线" points={s.data?.points} range={s.data?.range ?? range} lines={L_NET} yFmt={yRate} valueFmt={vRate} yBase={1000} height={160} clip /></ChartFrame></ChartCard>
      </Section>
    </>
  );
}
