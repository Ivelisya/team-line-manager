// 抽屉里带图表的三个页签：连接质量 / 流量 / 主机。单独成文件按需加载，recharts 只在打开这些页签时才下载。
import * as React from 'react';
import { ServerOff } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import type { UseQueryResult } from '@tanstack/react-query';
import { useSeries } from '@/lib/queries';
import * as F from '@/lib/format';
import { hasHost, monthTraffic } from '@/lib/derive';
import { Segmented } from '@/components/ui/overlays';
import { Skeleton } from '@/components/ui/skeleton';
import { Empty, ErrorNote, LabelValue, LabelValueGrid, Num, Section, SectionHeader } from '@/components/app/primitives';
import { RANGES, TimeChart } from '@/components/app/TimeChart';
import { EntriesTable } from './EntriesTable';

function RangeBar({ range, setRange }: { range: string; setRange: (r: string) => void }) {
  return <Segmented sm label="时间范围" value={range} onChange={setRange} items={RANGES.map((r) => ({ value: r.value, label: r.label }))} />;
}

/** 图表的加载 / 出错状态：出错给重试，不要显示一张空图 */
function ChartFrame({ q, height = 180, children }: { q: UseQueryResult<unknown>; height?: number; children: React.ReactNode }) {
  if (q.isError) return <ErrorNote error={q.error} onRetry={() => void q.refetch()}>曲线加载失败：</ErrorNote>;
  if (q.isLoading) return <Skeleton h={height} style={{ borderRadius: 'var(--r-float)' }} />;
  return <>{children}</>;
}

export function QualityTab({ m }: { m: MachineView }) {
  const [range, setRange] = React.useState('6h');
  const s = useSeries(m.id, 'probe', range);
  return (
    <>
      <Section>
        <SectionHeader title="延迟" meta="线路机 → 节点 TCP 与出口验证"><RangeBar range={range} setRange={setRange} /></SectionHeader>
        <ChartFrame q={s}>
          <TimeChart points={s.data?.points} range={range} kind="line"
            lines={[{ key: 'tcp_ms', label: 'TCP 握手', color: 'var(--chart-1)' }, { key: 'egress_ms', label: '出口验证', color: 'var(--chart-2)' }]}
            yFmt={(v) => `${v}`} valueFmt={(_, v) => `${Math.round(v)} ms`} />
          <div className="hint" style={{ marginTop: 4 }}>缺口表示该时刻探测失败。纵轴单位 ms。</div>
        </ChartFrame>
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
  const s = useSeries(m.id, 'traffic', range);
  const t = monthTraffic(m);
  return (
    <>
      <Section>
        <SectionHeader title="汇总" />
        <LabelValueGrid cols2>
          <LabelValue label="今日 ↓ 下行"><Num f={F.bytes(t.today_down)} /></LabelValue>
          <LabelValue label="今日 ↑ 上行"><Num f={F.bytes(t.today_up)} /></LabelValue>
          <LabelValue label="本月 ↓ 下行"><Num f={F.bytes(t.down)} /></LabelValue>
          <LabelValue label="本月 ↑ 上行"><Num f={F.bytes(t.up)} /></LabelValue>
        </LabelValueGrid>
      </Section>
      <Section>
        <SectionHeader title="业务流量" meta="每个时间桶的字节数"><RangeBar range={range} setRange={setRange} /></SectionHeader>
        <ChartFrame q={s}>
          <TimeChart points={s.data?.points} range={range}
            lines={[{ key: 'down', label: '↓ 下行', color: 'var(--chart-1)' }, { key: 'up', label: '↑ 上行', color: 'var(--chart-2)' }]}
            yFmt={(v) => { const b = F.bytes(v); return `${b.v}${b.u === 'B' ? '' : b.u[0]}`; }} valueFmt={(_, v) => { const b = F.bytes(v); return `${b.v} ${b.u}`; }} />
        </ChartFrame>
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
  const s = useSeries(m.id, 'host', range, host);
  if (!host) return <Empty icon={<ServerOff size={22} />} text={m.kind === 'proxy' ? '家宽代理没有主机，不采集 CPU / 内存 / 磁盘 / 网卡指标。' : '暂时没有主机指标'} />;
  const h = m.host!;
  const pct = (v: number) => `${Math.round(v)}%`;
  return (
    <>
      <Section>
        <SectionHeader title="当前" meta={`更新于 ${F.ago(h.ts)}`}><RangeBar range={range} setRange={setRange} /></SectionHeader>
        <LabelValueGrid>
          <LabelValue label="CPU"><Num f={F.pct(h.cpu_pct, 1)} /></LabelValue>
          <LabelValue label="内存"><Num f={F.bytes(h.mem_used)} /> <span className="t-sub">/ <Num f={F.bytes(h.mem_total)} /></span></LabelValue>
          <LabelValue label="磁盘"><Num f={F.bytes(h.disk_used)} /> <span className="t-sub">/ <Num f={F.bytes(h.disk_total)} /></span></LabelValue>
          <LabelValue label="负载 1 分钟"><span className="num">{h.load1 == null ? '—' : h.load1.toFixed(2)}</span></LabelValue>
          <LabelValue label="TCP 连接数"><span className="num">{h.tcp_conns ?? '—'}</span></LabelValue>
          <LabelValue label="网卡 ↓ / ↑"><Num f={F.rate(h.net_rx_bps)} /> <span className="t-sub">/</span> <Num f={F.rate(h.net_tx_bps)} /></LabelValue>
        </LabelValueGrid>
      </Section>
      <Section><SectionHeader title="CPU" /><ChartFrame q={s} height={140}><TimeChart points={s.data?.points} range={range} lines={[{ key: 'cpu_pct', label: 'CPU', color: 'var(--chart-1)' }]} yFmt={pct} valueFmt={(_, v) => `${v.toFixed(1)}%`} domain={[0, 100]} height={140} /></ChartFrame></Section>
      <Section><SectionHeader title="内存" /><ChartFrame q={s} height={140}><TimeChart points={s.data?.points} range={range} lines={[{ key: 'mem_pct', label: '内存', color: 'var(--chart-1)' }]} yFmt={pct} valueFmt={(_, v) => `${v.toFixed(1)}%`} domain={[0, 100]} height={140} /></ChartFrame></Section>
      <Section><SectionHeader title="网卡速率" /><ChartFrame q={s} height={160}><TimeChart points={s.data?.points} range={range} lines={[{ key: 'net_rx_bps', label: '↓ 接收', color: 'var(--chart-1)' }, { key: 'net_tx_bps', label: '↑ 发送', color: 'var(--chart-2)' }]} yFmt={(v) => { const r = F.rate(v); return `${r.v}${r.u[0]}`; }} valueFmt={(_, v) => { const r = F.rate(v); return `${r.v} ${r.u}`; }} height={160} /></ChartFrame></Section>
    </>
  );
}
