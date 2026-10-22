// 抽屉里带图表的三个页签：连接质量 / 流量 / 主机。单独成文件按需加载，recharts 只在打开这些页签时才下载。
import * as React from 'react';
import { Cpu, Globe, MemoryStick, Network, Radar, ServerOff } from 'lucide-react';
import type { MachineView, SeriesResp } from '@/lib/types';
import type { UseQueryResult } from '@tanstack/react-query';
import { useAvailability, useSeries } from '@/lib/queries';
import * as F from '@/lib/format';
import { availCells, availTitles, hasHost, monthTraffic, resourceMeters, successPct } from '@/lib/derive';
import { Segmented } from '@/components/ui/overlays';
import { Skeleton } from '@/components/ui/skeleton';
import { Beat, Empty, ErrorNote, LabelValue, LabelValueGrid, Note, Num, Section, SectionHeader } from '@/components/app/primitives';
import { RANGES, TimeChart } from '@/components/app/TimeChart';
import { EntriesTable } from './EntriesTable';

/** 抽屉开着时曲线的重取间隔（实时流只推当前读数，不推曲线），随范围放大：范围越长点越粗、取一次越贵
 *  （线路机 24 小时流量曲线一次 130–350 ms 数据库时间，每分钟才多一个点） */
const POLL: Record<string, number> = { '1h': 30_000, '6h': 60_000, '24h': 120_000 };
const poll = (range: string) => POLL[range] ?? 300_000;

// 曲线的线条与格式化放模块常量：抽屉随实时推送重渲染时 TimeChart（memo）的 props 不变，不重画。
// CPU / 内存用首页节点卡的指标色（CPU 蓝、内存紫），上下行与对照序列沿用 --chart-1 / --chart-2
const L_PROBE = [{ key: 'tcp_ms', label: 'TCP 握手', color: 'var(--chart-1)' }, { key: 'egress_ms', label: '出口验证', color: 'var(--chart-2)' }];
const L_TRAFFIC = [{ key: 'down', label: '↓ 下载', color: 'var(--chart-1)' }, { key: 'up', label: '↑ 上传', color: 'var(--chart-2)' }];
const L_CPU = [{ key: 'cpu_pct', label: 'CPU', color: 'var(--m-cpu)' }];
// 按小时的点（7 天 / 30 天）是一小时的平均，短时冲高被摊平：同画每小时的峰值，色取 CPU 蓝的浅色
const L_CPU_HOURLY = [{ key: 'cpu_pct', label: '平均', color: 'var(--m-cpu)' }, { key: 'cpu_max', label: '峰值', color: 'color-mix(in oklab, var(--m-cpu) 45%, transparent)' }];
const L_MEM = [{ key: 'mem_pct', label: '内存', color: 'var(--m-mem)' }];
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

function RangeBar({ range, setRange }: { range: string; setRange: (r: string) => void }) {
  return <Segmented sm label="时间范围" value={range} onChange={setRange} items={RANGES} />;
}

const RANGE_MS: Record<string, number> = { '1h': 3600_000, '6h': 6 * 3600_000, '24h': 24 * 3600_000, '7d': 7 * 86400_000, '30d': 30 * 86400_000 };
/** 点不到两个、画不成线时的说明：还在上架、或在所选时间范围内才上架的，是数据还在积累；其余（离线、已退役）是这段时间没有数据 */
const fewText = (m: MachineView, range: string) =>
  m.lifecycle === 'provisioning' || (m.activated_at != null && Date.now() - Date.parse(m.activated_at) < (RANGE_MS[range] ?? 0)) ? '刚上架，数据积累中' : '这段时间没有数据';

/** 图表的加载 / 出错状态：出错给重试，不要显示一张空图；换时间范围、新数据未到时沿用的旧曲线调淡。
 *  点不到两个时只剩一副空坐标轴：同全为 0 的空状态，占住图高、写一句说明（few） */
function ChartFrame({ q, few, height = 180, children }: { q: UseQueryResult<SeriesResp>; few: string; height?: number; children: React.ReactNode }) {
  if (q.isError) return <ErrorNote error={q.error} onRetry={() => void q.refetch()}>曲线加载失败：</ErrorNote>;
  if (q.isLoading) return <Skeleton h={height} style={{ borderRadius: 'var(--r-float)' }} />;
  return (
    <div className="chart-frame" data-stale={q.isPlaceholderData} aria-busy={q.isPlaceholderData || undefined}>
      {(q.data?.points.length ?? 0) < 2 ? <div className="chart-empty" style={{ height }}>{few}</div> : children}
    </div>
  );
}

/** 图表卡：一张曲线一张卡，边框、底色与顶部色条取指标色（--cc）。卡头左边图标 + 标题，右边当前值 + 一行小字；
 *  没有当前值的图右边放时间范围（tools） */
function ChartCard({ c, icon, title, meta, value, caption, tools, children }: { c: string; icon: React.ReactNode; title: string; meta?: string; value?: React.ReactNode; caption?: string; tools?: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="chart-card" style={{ '--cc': c } as React.CSSProperties}>
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
 *  格条上方左边是整段的成功率、右边是时间窗（同卡片上「入口连通 · 近 1 小时」的写法）。
 *  只依赖机器 id 与是否线路机：实时推送重渲染页签时不跟着重画 */
const AvailabilitySection = React.memo(function AvailabilitySection({ id, relay }: { id: string; relay: boolean }) {
  const q = useAvailability(id, AVAIL_DAYS);
  const d = q.data;
  const tip = relay ? `${AVAIL_TIP}；这里是挂在这台线路机上所有入口的合计` : AVAIL_TIP;
  const caption = !d ? '加载中…' : d.tcp_pct == null && d.egress_pct == null ? '没有探测'
    : <span data-tip={tip}>TCP <b>{successPct(d.tcp_pct)}</b> · 出口 <b>{successPct(d.egress_pct)}</b></span>;
  return (
    <Section>
      <SectionHeader title="可用率" />
      {q.isError ? <ErrorNote error={q.error} onRetry={() => void q.refetch()}>可用率加载失败：</ErrorNote>
        : <Beat n={AVAIL_DAYS} cells={d ? availCells(d.items) : []} titles={d && availTitles(d.items)} caption={caption} win={`近 ${AVAIL_DAYS} 天 · 每天一格`} stale={!d} />}
    </Section>
  );
});

export function QualityTab({ m }: { m: MachineView }) {
  const [range, setRange] = React.useState('6h');
  const s = useSeries(m.id, 'probe', range, true, poll(range));
  return (
    <>
      <AvailabilitySection id={m.id} relay={m.kind === 'relay'} />
      <Section>
        <ChartCard c="var(--chart-1)" icon={<Radar />} title="延迟" meta="线路机 → 节点 TCP 与出口验证" tools={<RangeBar range={range} setRange={setRange} />}>
          <ChartFrame q={s} few={fewText(m, range)}>
            <TimeChart points={s.data?.points} range={s.data?.range ?? range} kind="line" lines={L_PROBE} yFmt={yMs} valueFmt={vMs} />
            <div className="hint" style={{ marginTop: 4 }}>缺口表示该时刻探测失败。</div>
          </ChartFrame>
        </ChartCard>
      </Section>
      <Section>
        <SectionHeader title="各入口当前探测" />
        <EntriesTable m={m} />
      </Section>
    </>
  );
}

export function TrafficTab({ m }: { m: MachineView }) {
  const [range, setRange] = React.useState('24h');
  const s = useSeries(m.id, 'traffic', range, true, poll(range));
  const t = monthTraffic(m);
  return (
    <>
      <Section>
        <SectionHeader title="汇总" />
        <LabelValueGrid cols2>
          <LabelValue label="今日 ↓ 下载"><Num f={F.bytes(t.today_down)} /></LabelValue>
          <LabelValue label="今日 ↑ 上传"><Num f={F.bytes(t.today_up)} /></LabelValue>
          <LabelValue label="本月 ↓ 下载"><Num f={F.bytes(t.down)} /></LabelValue>
          <LabelValue label="本月 ↑ 上传"><Num f={F.bytes(t.up)} /></LabelValue>
        </LabelValueGrid>
      </Section>
      <Section>
        <ChartCard c="var(--chart-1)" icon={<Globe />} title="业务流量" meta={s.data?.resolution === '1h' ? '每小时的流量' : '每分钟的流量'} tools={<RangeBar range={range} setRange={setRange} />}>
          <ChartFrame q={s} few={fewText(m, range)}>
            <TimeChart points={s.data?.points} range={s.data?.range ?? range} lines={L_TRAFFIC} yFmt={yBytes} valueFmt={vBytes} yBase={1024} empty="这段时间没有业务流量" />
          </ChartFrame>
        </ChartCard>
      </Section>
      <Section>
        <SectionHeader title="按入口" />
        <EntriesTable m={m} traffic />
      </Section>
    </>
  );
}

export function HostTab({ m }: { m: MachineView }) {
  const [range, setRange] = React.useState('6h');
  // 家宽代理没有主机；其它机器即使现在没有读数（agent 离线、还没上报），历史曲线照样能看——排查掉线前发生了什么正要看它
  const agentful = m.kind !== 'proxy';
  const s = useSeries(m.id, 'host', range, agentful, poll(range));
  if (!agentful) return <Empty icon={<ServerOff size={22} />} text="家宽代理没有主机，不采集 CPU / 内存 / 磁盘 / 网卡指标。" />;
  const h = hasHost(m) ? m.host! : null;
  const r = h ? resourceMeters(m)! : null;
  const few = fewText(m, range);
  const hourly = s.data?.resolution === '1h';
  return (
    <>
      <Section>
        <SectionHeader title="当前" meta={h ? `更新于 ${F.ago(h.ts)}` : m.agent?.last_report_at ? `最后上报于 ${F.ago(m.agent.last_report_at)}` : '还没上报过'}><RangeBar range={range} setRange={setRange} /></SectionHeader>
        {h && r ? (
          <LabelValueGrid>
            <LabelValue label="内存"><Num f={F.bytes(h.mem_used)} /> <span className="t-sub">/ <Num f={F.bytes(h.mem_total)} /></span></LabelValue>
            <LabelValue label="磁盘"><Num f={F.bytes(h.disk_used)} /> <span className="t-sub">/ <Num f={F.bytes(h.disk_total)} /></span></LabelValue>
            <LabelValue label="负载"><Num f={r.load.value} /> <span className="t-sub">{r.load.sub}</span></LabelValue>
            <LabelValue label="TCP 连接数"><span className="num">{h.tcp_conns ?? '—'}</span></LabelValue>
          </LabelValueGrid>
        ) : <div className="hint">agent 离线或还没上报过，没有当前读数；下面是所选时间范围内的历史曲线。</div>}
        {m.host_machine && <div style={{ marginTop: 8 }}><Note>这是宿主机「{m.host_machine.name}」的读数：同机 IP 节点和宿主机是同一台机器，CPU、内存、磁盘与网卡都共用。</Note></div>}
      </Section>
      {/* 三张图表卡同属一段，之间不再画分隔线 */}
      <Section className="chart-stack">
        <ChartCard c="var(--m-cpu)" icon={<Cpu />} title="CPU" meta={hourly ? '每小时平均与峰值' : undefined} value={h ? <Num f={F.pct(h.cpu_pct, 1)} /> : '—'} caption="使用率"><ChartFrame q={s} few={few} height={140}><TimeChart points={s.data?.points} range={s.data?.range ?? range} lines={hourly ? L_CPU_HOURLY : L_CPU} yFmt={yPct} valueFmt={vPct} domain={PCT_DOMAIN} height={140} /></ChartFrame></ChartCard>
        <ChartCard c="var(--m-mem)" icon={<MemoryStick />} title="内存" value={r ? <Num f={F.pct(r.mem.pct, 1)} /> : '—'} caption="使用率"><ChartFrame q={s} few={few} height={140}><TimeChart points={s.data?.points} range={s.data?.range ?? range} lines={L_MEM} yFmt={yPct} valueFmt={vPct} domain={PCT_DOMAIN} height={140} /></ChartFrame></ChartCard>
        <ChartCard c="var(--chart-1)" icon={<Network />} title="网卡速率" value={h ? <>↓ <Num f={F.rate(h.net_rx_bps)} /> <span className="t-sub">·</span> ↑ <Num f={F.rate(h.net_tx_bps)} /></> : '—'} caption="当前速率"><ChartFrame q={s} few={few} height={160}><TimeChart points={s.data?.points} range={s.data?.range ?? range} lines={L_NET} yFmt={yRate} valueFmt={vRate} yBase={1000} height={160} /></ChartFrame></ChartCard>
      </Section>
    </>
  );
}
