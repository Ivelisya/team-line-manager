// 抽屉里带图表的三个页签：连接质量 / 流量 / 主机。单独成文件按需加载，recharts 只在打开这些页签时才下载。
// 图表卡、加载框与时间范围也给软路由抽屉的「曲线」页签用（RouterCharts）。
import * as React from 'react';
import { Cpu, Gauge, Globe, HardDrive, MemoryStick, Network, Radar, ServerOff } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import type { UseQueryResult } from '@tanstack/react-query';
import { useAvailability, useSeries } from '@/lib/queries';
import * as F from '@/lib/format';
import { AVAIL_LEGEND, availCells, availTitles, fromLabel, hasHost, monthFlowTip, monthTraffic, resourceMeters, successPct } from '@/lib/derive';
import { Segmented } from '@/components/ui/overlays';
import { Skeleton } from '@/components/ui/skeleton';
import { InfoTip } from '@/components/ui/form';
import { Beat, Empty, ErrorNote, LabelValue, LabelValueGrid, Note, Num, Section, SectionHeader } from '@/components/app/primitives';
import { RANGES, TimeChart } from '@/components/app/TimeChart';
import { EntriesTable, entriesMeta } from './EntriesTable';

/** 抽屉开着时曲线的重取间隔（实时流只推当前读数，不推曲线），随范围放大：范围越长点越粗、取一次越贵
 *  （线路机 24 小时流量曲线一次 130–350 ms 数据库时间，每分钟才多一个点） */
const POLL: Record<string, number> = { '1h': 30_000, '6h': 60_000, '24h': 120_000 };
const poll = (range: string) => POLL[range] ?? 300_000;

// 曲线的线条与格式化放模块常量：抽屉随实时推送重渲染时 TimeChart（memo）的 props 不变，不重画。
// CPU / 内存用首页节点卡的指标色（CPU 蓝、内存紫），上下行与对照序列沿用 --chart-1 / --chart-2
// 只画 TCP：出口验证的耗时是经节点去中控回显服务的完整 HTTPS 往返，不是线路延迟（出口只判通不通，见可用率与入口表）
const L_PROBE = [{ key: 'tcp_ms', label: 'TCP 握手', color: 'var(--chart-1)' }];
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
const yMs = (v: number) => (v ? `${v} ms` : '0');
const yPct = (v: number) => `${Math.round(v)}%`;
const yBytes = (v: number) => (v ? F.str(F.bytes(v)) : '0');
const yRate = (v: number) => (v ? F.str(F.rate(v)) : '0');
const vMs = (_: string, v: number) => `${Math.round(v)} ms`;
const vPct = (_: string, v: number) => `${v.toFixed(1)}%`;
const vBytes = (_: string, v: number) => { const b = F.bytes(v); return `${b.v} ${b.u}`; };
const vRate = (_: string, v: number) => { const r = F.rate(v); return `${r.v} ${r.u}`; };

export function RangeBar({ range, setRange }: { range: string; setRange: (r: string) => void }) {
  return <Segmented sm label="时间范围" value={range} onChange={setRange} items={RANGES} />;
}
/** 抽屉各曲线页签统一的时间范围条：放在页签顶部并吸顶（往下翻几张曲线时也能换），默认 24 小时 */
export const RangeSticky = ({ range, setRange }: { range: string; setRange: (r: string) => void }) => (
  <div className="tab-sticky"><span>时间范围</span><RangeBar range={range} setRange={setRange} /></div>
);

const RANGE_MS: Record<string, number> = { '1h': 3600_000, '6h': 6 * 3600_000, '24h': 24 * 3600_000, '7d': 7 * 86400_000, '30d': 30 * 86400_000 };
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

/** 连接质量、流量、主机页签的默认范围（三个页签一致） */
const DEFAULT_RANGE = '24h';

export function QualityTab({ m }: { m: MachineView }) {
  const [range, setRange] = React.useState(DEFAULT_RANGE);
  const s = useSeries(m.id, 'probe', range, true, poll(range));
  return (
    <>
      <RangeSticky range={range} setRange={setRange} />
      <AvailabilitySection id={m.id} relay={m.kind === 'relay'} />
      <Section>
        {/* 多条入口时曲线是各入口的平均（序列接口不分入口），写明几条、是哪几台线路机 */}
        <ChartCard c="var(--chart-1)" icon={<Radar />} title="TCP 延迟" meta={m.kind !== 'relay' && m.entries.length > 1
          ? <span data-tip={`${m.entries.map((e) => e.relay.name).join('、')} 的平均`}>线路机 → 节点 · {m.entries.length} 条入口平均</span> : '线路机 → 节点'}>
          <ChartFrame q={s} few={fewText(m, range)}>
            <TimeChart full points={s.data?.points} range={s.data?.range ?? range} kind="line" lines={L_PROBE} yFmt={yMs} valueFmt={vMs} clip />
          </ChartFrame>
        </ChartCard>
      </Section>
      <Section>
        <SectionHeader title="各入口当前探测" meta={entriesMeta(m)} />
        <EntriesTable m={m} />
      </Section>
    </>
  );
}

export function TrafficTab({ m }: { m: MachineView }) {
  const [range, setRange] = React.useState(DEFAULT_RANGE);
  const s = useSeries(m.id, 'traffic', range, true, poll(range));
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
      <Section>
        <SectionHeader title="按入口" meta={entriesMeta(m)} />
        <EntriesTable m={m} traffic />
      </Section>
    </>
  );
}

export function HostTab({ m }: { m: MachineView }) {
  const [range, setRange] = React.useState(DEFAULT_RANGE);
  // 家宽代理没有主机；其它机器即使现在没有读数（agent 离线、还没上报），历史曲线照样能看——排查掉线前发生了什么正要看它
  const agentful = m.kind !== 'proxy';
  const s = useSeries(m.id, 'host', range, agentful, poll(range));
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
          ? <ChartCard slim c="var(--m-load)" icon={<Gauge />} title="负载" meta={<InfoTip text={`1 分钟平均负载 ÷ ${cores} 核；超过 100% 表示有任务在排队`} />} value="—" caption={hourly ? '小时汇总不含负载' : '容器内不可用'} />
          : <ChartCard c="var(--m-load)" icon={<Gauge />} title="负载" meta={<InfoTip text={`1 分钟平均负载 ÷ ${cores} 核；超过 100% 表示有任务在排队`} />} value={r ? <Num f={r.load.value} /> : '—'} caption="1 分钟平均"><ChartFrame q={s} few={few} height={140}>{loadPoints && <TimeChart full holeLabel="agent 离线" points={loadPoints} range={s.data?.range ?? range} lines={L_LOAD} yFmt={yPct} valueFmt={vPct} height={140} />}</ChartFrame></ChartCard>)}
        <ChartCard c="var(--chart-1)" icon={<Network />} title="网卡速率" value={h ? <>↓ <Num f={F.rate(h.net_rx_bps)} /> <span className="t-sub">·</span> ↑ <Num f={F.rate(h.net_tx_bps)} /></> : '—'} caption="当前速率"><ChartFrame q={s} few={few} height={160}><TimeChart full holeLabel="agent 离线" points={s.data?.points} range={s.data?.range ?? range} lines={L_NET} yFmt={yRate} valueFmt={vRate} yBase={1000} height={160} clip /></ChartFrame></ChartCard>
      </Section>
    </>
  );
}
