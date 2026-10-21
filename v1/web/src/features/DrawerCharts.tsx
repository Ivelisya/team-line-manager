// 抽屉里带图表的三个页签：连接质量 / 流量 / 主机。单独成文件按需加载，recharts 只在打开这些页签时才下载。
import * as React from 'react';
import { Cpu, Globe, MemoryStick, Network, Radar, ServerOff } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import type { UseQueryResult } from '@tanstack/react-query';
import { useSeries } from '@/lib/queries';
import * as F from '@/lib/format';
import { hasHost, monthTraffic, resourceMeters } from '@/lib/derive';
import { Segmented } from '@/components/ui/overlays';
import { Skeleton } from '@/components/ui/skeleton';
import { Empty, ErrorNote, LabelValue, LabelValueGrid, Num, Section, SectionHeader } from '@/components/app/primitives';
import { RANGES, TimeChart } from '@/components/app/TimeChart';
import { EntriesTable } from './EntriesTable';

/** 抽屉开着时曲线每 30 秒重取（实时流只推当前读数，不推曲线） */
const POLL = 30_000;

// 曲线的线条与格式化放模块常量：抽屉随实时推送重渲染时 TimeChart（memo）的 props 不变，不重画。
// CPU / 内存用首页节点卡的指标色（CPU 蓝、内存紫），上下行与对照序列沿用 --chart-1 / --chart-2
const L_PROBE = [{ key: 'tcp_ms', label: 'TCP 握手', color: 'var(--chart-1)' }, { key: 'egress_ms', label: '出口验证', color: 'var(--chart-2)' }];
const L_TRAFFIC = [{ key: 'down', label: '↓ 下载', color: 'var(--chart-1)' }, { key: 'up', label: '↑ 上传', color: 'var(--chart-2)' }];
const L_CPU = [{ key: 'cpu_pct', label: 'CPU', color: 'var(--m-cpu)' }];
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
  return <Segmented sm label="时间范围" value={range} onChange={setRange} items={RANGES.map((r) => ({ value: r.value, label: r.label }))} />;
}

/** 图表的加载 / 出错状态：出错给重试，不要显示一张空图；换时间范围、新数据未到时沿用的旧曲线调淡 */
function ChartFrame({ q, height = 180, children }: { q: UseQueryResult<unknown>; height?: number; children: React.ReactNode }) {
  if (q.isError) return <ErrorNote error={q.error} onRetry={() => void q.refetch()}>曲线加载失败：</ErrorNote>;
  if (q.isLoading) return <Skeleton h={height} style={{ borderRadius: 'var(--r-float)' }} />;
  return <div className="chart-frame" data-stale={q.isPlaceholderData || undefined} aria-busy={q.isPlaceholderData || undefined}>{children}</div>;
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

export function QualityTab({ m }: { m: MachineView }) {
  const [range, setRange] = React.useState('6h');
  const s = useSeries(m.id, 'probe', range, true, POLL);
  return (
    <>
      <Section>
        <ChartCard c="var(--chart-1)" icon={<Radar />} title="延迟" meta="线路机 → 节点 TCP 与出口验证" tools={<RangeBar range={range} setRange={setRange} />}>
          <ChartFrame q={s}>
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
  const s = useSeries(m.id, 'traffic', range, true, POLL);
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
          <ChartFrame q={s}>
            <TimeChart points={s.data?.points} range={s.data?.range ?? range} lines={L_TRAFFIC} yFmt={yBytes} valueFmt={vBytes} yBase={1024} />
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
  const host = hasHost(m);
  const s = useSeries(m.id, 'host', range, host, POLL);
  if (!host) return <Empty icon={<ServerOff size={22} />} text={m.kind === 'proxy' ? '家宽代理没有主机，不采集 CPU / 内存 / 磁盘 / 网卡指标。' : '暂时没有主机指标'} />;
  const h = m.host!;
  const r = resourceMeters(m)!;
  return (
    <>
      <Section>
        <SectionHeader title="当前" meta={`更新于 ${F.ago(h.ts)}`}><RangeBar range={range} setRange={setRange} /></SectionHeader>
        <LabelValueGrid>
          <LabelValue label="内存"><Num f={F.bytes(h.mem_used)} /> <span className="t-sub">/ <Num f={F.bytes(h.mem_total)} /></span></LabelValue>
          <LabelValue label="磁盘"><Num f={F.bytes(h.disk_used)} /> <span className="t-sub">/ <Num f={F.bytes(h.disk_total)} /></span></LabelValue>
          <LabelValue label="负载"><Num f={r.load.value} /> <span className="t-sub">{r.load.sub}</span></LabelValue>
          <LabelValue label="TCP 连接数"><span className="num">{h.tcp_conns ?? '—'}</span></LabelValue>
        </LabelValueGrid>
      </Section>
      {/* 三张图表卡同属一段，之间不再画分隔线 */}
      <Section className="chart-stack">
        <ChartCard c="var(--m-cpu)" icon={<Cpu />} title="CPU" value={<Num f={F.pct(h.cpu_pct, 1)} />} caption="使用率"><ChartFrame q={s} height={140}><TimeChart points={s.data?.points} range={s.data?.range ?? range} lines={L_CPU} yFmt={yPct} valueFmt={vPct} domain={PCT_DOMAIN} height={140} /></ChartFrame></ChartCard>
        <ChartCard c="var(--m-mem)" icon={<MemoryStick />} title="内存" value={<Num f={F.pct(r.mem.pct, 1)} />} caption="使用率"><ChartFrame q={s} height={140}><TimeChart points={s.data?.points} range={s.data?.range ?? range} lines={L_MEM} yFmt={yPct} valueFmt={vPct} domain={PCT_DOMAIN} height={140} /></ChartFrame></ChartCard>
        <ChartCard c="var(--chart-1)" icon={<Network />} title="网卡速率" value={<>↓ <Num f={F.rate(h.net_rx_bps)} /> <span className="t-sub">·</span> ↑ <Num f={F.rate(h.net_tx_bps)} /></>} caption="当前速率"><ChartFrame q={s} height={160}><TimeChart points={s.data?.points} range={s.data?.range ?? range} lines={L_NET} yFmt={yRate} valueFmt={vRate} yBase={1000} height={160} /></ChartFrame></ChartCard>
      </Section>
    </>
  );
}
