// 机器详情抽屉：概览 / 连接质量 / 流量 / 主机 / 费用 / 管理。开关由 URL 的 ?m= 控制。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { ServerOff } from 'lucide-react';
import { Sheet, SheetContent, SheetDescription, SheetTitle } from '@/components/ui/dialog';
import { Tabs, TabsList, TabsTrigger, Segmented } from '@/components/ui/overlays';
import type { EntryView, MachineView } from '@/lib/types';
import { useMachine, useSeries, useAudit } from '@/lib/queries';
import { DRAWER_TABS, useDrawer, type DrawerTab } from '@/lib/drawer';
import * as F from '@/lib/format';
import { KIND_LABEL } from '@/lib/sort';
import { jobKindLabel } from '@/lib/jobs';
import { hasHost, monthTraffic, probeSummary } from '@/lib/derive';
import { Chip, Empty, entryState, HealthPill, KindIcon, LabelValue, LabelValueGrid, LifecyclePill, Note, Num, Section, SectionHeader } from '@/components/app/primitives';
import { RANGES, TimeChart } from '@/components/app/TimeChart';
import { ManageTab } from './ManageTab';
import { BillingTab } from './BillingTab';
import { AccessTab } from './AccessTab';

export function NodeDrawer() {
  const { id, tab, setTab, close } = useDrawer();
  const q = useMachine(id);
  const m = q.data;
  return (
    <Sheet open={!!id} onOpenChange={(o) => !o && close()}>
      <SheetContent aria-describedby={undefined}>
        {!m ? (
          <>
            <div className="drawer-head"><SheetTitle className="drawer-title">{q.isError ? '无法加载机器' : '加载中…'}</SheetTitle></div>
            <div className="drawer-body">{q.isError ? <Note tone="bad">{(q.error as Error).message}</Note> : <div className="skel" style={{ height: 120 }} />}</div>
          </>
        ) : (
          <>
            <header className="drawer-head" style={{ paddingRight: 56 }}>
              <KindIcon kind={m.kind} />
              <div className="m-head" style={{ flex: 1 }}>
                <div className="m-head-row"><SheetTitle className="drawer-title ellipsis">{m.name}</SheetTitle><HealthPill m={m} />{m.kind !== 'vps' && <Chip variant="neutral">{KIND_LABEL[m.kind]}</Chip>}</div>
                <SheetDescription className="ellipsis" style={{ margin: 0, color: 'var(--fg-2)', fontSize: 12 }}>{m.ip} · {m.provider?.name ?? '未设供应商'} · {m.plan?.name ?? '未设套餐'}</SheetDescription>
              </div>
            </header>
            <Tabs value={tab} onValueChange={(v) => setTab(v as DrawerTab)} className="drawer-tabs">
              <TabsList>{DRAWER_TABS.map((t) => <TabsTrigger key={t.value} value={t.value}>{t.label}</TabsTrigger>)}</TabsList>
            </Tabs>
            <div className="drawer-body">
              {m.lifecycle === 'retired' && (
                <Note tone="warn"><b>{m.name}</b> 已于 {F.dtFull(m.retired_at)} 退役（IP {m.ip}）。退役机器保留原编号，可能与在用机器同名，请以 IP 和机器 ID 区分。{m.lifecycle_reason && <> 原因：{m.lifecycle_reason}。</>}</Note>
              )}
              {m.active_job && (
                <Note tone="info">
                  {jobKindLabel(m.active_job.kind)}进行中。<Link to={`/jobs/${m.active_job.id}`} className="btn-link" onClick={close} style={{ color: 'var(--accent)' }}>查看作业</Link>
                </Note>
              )}
              {tab === 'overview' && <OverviewTab m={m} />}
              {tab === 'quality' && <QualityTab m={m} />}
              {tab === 'traffic' && <TrafficTab m={m} />}
              {tab === 'host' && <HostTab m={m} />}
              {tab === 'billing' && <BillingTab m={m} />}
              {tab === 'access' && <AccessTab m={m} />}
              {tab === 'manage' && <ManageTab m={m} onClosed={close} />}
            </div>
          </>
        )}
      </SheetContent>
    </Sheet>
  );
}

const probeCell = (p: EntryView['tcp']) => (p == null ? <span className="t-muted">—</span> : p.ok ? <Num f={F.ms(p.latency_ms)} /> : <span style={{ color: 'var(--bad-text)' }} title={p.detail ?? undefined}>失败</span>);

export function EntriesTable({ m, traffic }: { m: MachineView; traffic?: boolean }) {
  if (!m.entries.length) return <div className="hint">还没有入口</div>;
  return (
    <div className="table-wrap" style={{ border: '1px solid var(--line)', borderRadius: 'var(--r-float)' }}>
      <table className="dt is-one" style={{ minWidth: 480 }}>
        <colgroup><col style={{ width: 132 }} /><col style={{ width: 96 }} /><col style={{ width: 72 }} /><col style={{ width: 72 }} />{traffic && <col style={{ width: 112 }} />}</colgroup>
        <thead><tr><th>{m.kind === 'relay' ? '端口 · 节点' : '线路机:端口'}</th><th>状态</th><th className="r">TCP</th><th className="r">出口</th>{traffic && <th className="r">本月流量</th>}</tr></thead>
        <tbody>
          {m.entries.map((e) => (
            <tr key={e.id}>
              <td className="strong">{m.kind === 'relay' ? `${e.port} · ${e.node.name}` : `${e.relay.name}:${e.port}`}</td>
              <td>{entryState(e)}</td><td className="r">{probeCell(e.tcp)}</td><td className="r">{probeCell(e.egress)}</td>
              {traffic && <td className="r"><Num f={F.bytes(e.traffic.month_up + e.traffic.month_down)} /></td>}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function OverviewTab({ m }: { m: MachineView }) {
  const audit = useAudit(m.id);
  const ps = probeSummary(m);
  return (
    <>
      {m.health.reasons.length > 0 && (
        <Section>
          <SectionHeader title="状态原因" />
          <ul className="reason-list">{m.health.reasons.map((r) => <li key={r.key}><span className="dot" data-tone={r.severity === 'critical' ? 'bad' : r.severity === 'warning' ? 'warn' : 'info'} />{r.text}</li>)}</ul>
        </Section>
      )}
      <Section>
        <SectionHeader title="基本信息" />
        <LabelValueGrid>
          <LabelValue label="类型">{KIND_LABEL[m.kind]}</LabelValue>
          <LabelValue label="团队 / 编号">{m.team ? `${m.team.name} / ${m.number != null ? F.pad2(m.number) : '—'}` : '—'}</LabelValue>
          <LabelValue label="IP">{m.ip}</LabelValue>
          <LabelValue label="SSH 端口">{m.ssh_port}</LabelValue>
          <LabelValue label="供应商">{m.provider?.name}</LabelValue>
          <LabelValue label="套餐">{m.plan?.name}</LabelValue>
          <LabelValue label="服务编号">{m.service_ref}</LabelValue>
          <LabelValue label="生命周期"><LifecyclePill lc={m.lifecycle} />{m.lifecycle_reason && <span className="t-sub"> {m.lifecycle_reason}</span>}</LabelValue>
          <LabelValue label="上架时间">{F.dtFull(m.activated_at)}</LabelValue>
          {m.retired_at && <LabelValue label="退役时间">{F.dtFull(m.retired_at)}</LabelValue>}
          <LabelValue label="agent">{m.agent ? `${m.agent.version} · ${F.ago(m.agent.last_report_at)}` : '无（家宽代理不装 agent）'}</LabelValue>
          <LabelValue label="入口探测">{m.entries.length ? `${ps.tcpOk}/${ps.total} 连通` : '—'}</LabelValue>
          {m.note && <LabelValue label="备注" className="span-2" wrap>{m.note}</LabelValue>}
        </LabelValueGrid>
      </Section>
      {m.relay && (
        <Section>
          <SectionHeader title="下发状态" />
          <LabelValueGrid>
            <LabelValue label="公网地址">{m.relay.public_host}</LabelValue>
            <LabelValue label="端口范围">{m.relay.port_min}–{m.relay.port_max}</LabelValue>
            <LabelValue label="配置代数 / 已下发">{m.relay.generation} / {m.relay.applied_generation}</LabelValue>
            <LabelValue label="上次下发">{F.dtFull(m.relay.applied_at)}</LabelValue>
          </LabelValueGrid>
          {m.relay.apply_error && <div style={{ marginTop: 8 }}><Note tone="bad">{m.relay.apply_error}</Note></div>}
        </Section>
      )}
      <Section>
        <SectionHeader title={m.kind === 'relay' ? '挂载的入口' : '入口'} meta={`${m.entries.length} 条`} />
        <EntriesTable m={m} />
      </Section>
      <Section>
        <SectionHeader title="最近操作" />
        {audit.data?.length ? (
          <ul className="reason-list">{audit.data.map((a) => <li key={a.id}><span className="dot" data-tone="neutral" /><span><b style={{ fontWeight: 500 }}>{a.title}</b> <span className="t-sub">{a.summary} · {a.actor} · {F.ago(a.ts)}</span></span></li>)}</ul>
        ) : <div className="hint">暂无记录</div>}
      </Section>
    </>
  );
}

function RangeBar({ range, setRange }: { range: string; setRange: (r: string) => void }) {
  return <Segmented sm label="时间范围" value={range} onChange={setRange} items={RANGES.map((r) => ({ value: r.value, label: r.label }))} />;
}

function QualityTab({ m }: { m: MachineView }) {
  const [range, setRange] = React.useState('6h');
  const s = useSeries(m.id, 'probe', range);
  return (
    <>
      <Section>
        <SectionHeader title="延迟" meta="线路机 → 节点 TCP 与出口验证"><RangeBar range={range} setRange={setRange} /></SectionHeader>
        <TimeChart points={s.data?.points} range={range} kind="line"
          lines={[{ key: 'tcp_ms', label: 'TCP 握手', color: 'var(--chart-1)' }, { key: 'egress_ms', label: '出口验证', color: 'var(--chart-2)' }]}
          yFmt={(v) => `${v}`} valueFmt={(_, v) => `${Math.round(v)} ms`} />
        <div className="hint" style={{ marginTop: 4 }}>缺口表示该时刻探测失败。纵轴单位 ms。</div>
      </Section>
      <Section>
        <SectionHeader title="各入口当前探测" />
        <EntriesTable m={m} />
      </Section>
    </>
  );
}

function TrafficTab({ m }: { m: MachineView }) {
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
        <TimeChart points={s.data?.points} range={range}
          lines={[{ key: 'down', label: '↓ 下行', color: 'var(--chart-1)' }, { key: 'up', label: '↑ 上行', color: 'var(--chart-2)' }]}
          yFmt={(v) => { const b = F.bytes(v); return `${b.v}${b.u === 'B' ? '' : b.u[0]}`; }} valueFmt={(_, v) => { const b = F.bytes(v); return `${b.v} ${b.u}`; }} />
      </Section>
      <Section>
        <SectionHeader title="按入口" />
        <EntriesTable m={m} traffic />
      </Section>
    </>
  );
}

function HostTab({ m }: { m: MachineView }) {
  const [range, setRange] = React.useState('6h');
  const host = hasHost(m);
  const s = useSeries(m.id, 'host', range, host);
  if (!host) return <Empty icon={<ServerOff size={28} />} text={m.kind === 'proxy' ? '家宽代理没有主机，不采集 CPU / 内存 / 磁盘 / 网卡指标。' : '暂时没有主机指标'} />;
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
          <LabelValue label="负载 1 分钟"><span className="num">{h.load1.toFixed(2)}</span></LabelValue>
          <LabelValue label="TCP 连接数"><span className="num">{h.tcp_conns}</span></LabelValue>
          <LabelValue label="网卡 ↓ / ↑"><Num f={F.rate(h.net_rx_bps)} /> <span className="t-sub">/</span> <Num f={F.rate(h.net_tx_bps)} /></LabelValue>
        </LabelValueGrid>
      </Section>
      <Section><SectionHeader title="CPU" /><TimeChart points={s.data?.points} range={range} lines={[{ key: 'cpu_pct', label: 'CPU', color: 'var(--chart-1)' }]} yFmt={pct} valueFmt={(_, v) => `${v.toFixed(1)}%`} domain={[0, 100]} height={140} /></Section>
      <Section><SectionHeader title="内存" /><TimeChart points={s.data?.points} range={range} lines={[{ key: 'mem_pct', label: '内存', color: 'var(--chart-1)' }]} yFmt={pct} valueFmt={(_, v) => `${v.toFixed(1)}%`} domain={[0, 100]} height={140} /></Section>
      <Section><SectionHeader title="网卡速率" /><TimeChart points={s.data?.points} range={range} lines={[{ key: 'net_rx_bps', label: '↓ 接收', color: 'var(--chart-1)' }, { key: 'net_tx_bps', label: '↑ 发送', color: 'var(--chart-2)' }]} yFmt={(v) => { const r = F.rate(v); return `${r.v}${r.u[0]}`; }} valueFmt={(_, v) => { const r = F.rate(v); return `${r.v} ${r.u}`; }} height={160} /></Section>
    </>
  );
}
