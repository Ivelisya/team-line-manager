// 机器详情抽屉：概览 / 连接质量 / 流量 / 主机 / 费用 / 管理。开关由 URL 的 ?m= 控制。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { Sheet, SheetContent, SheetDescription, SheetTitle } from '@/components/ui/dialog';
import { Tabs, TabsList, TabsTrigger } from '@/components/ui/overlays';
import { ErrorBoundary } from '@/components/app/ErrorBoundary';
import { Skeleton } from '@/components/ui/skeleton';
import type { MachineView } from '@/lib/types';
import { useMachine, useAudit } from '@/lib/queries';
import { DRAWER_TABS, useDrawer, type DrawerTab } from '@/lib/drawer';
import { useHeld } from '@/lib/useHeld';
import * as F from '@/lib/format';
import { KIND_LABEL } from '@/lib/sort';
import { jobKindLabel } from '@/lib/jobs';
import { probeSummary } from '@/lib/derive';
import { Chip, ErrorNote, ReasonList, HealthPill, KindIcon, LabelValue, LabelValueGrid, LifecyclePill, Note, Section, SectionHeader } from '@/components/app/primitives';
import { ManageTab } from './ManageTab';
import { BillingTab } from './BillingTab';
import { EntriesTable } from './EntriesTable';

// 带图表的页签（recharts）按需加载
const QualityTab = React.lazy(() => import('./DrawerCharts').then((m) => ({ default: m.QualityTab })));
const TrafficTab = React.lazy(() => import('./DrawerCharts').then((m) => ({ default: m.TrafficTab })));
const HostTab = React.lazy(() => import('./DrawerCharts').then((m) => ({ default: m.HostTab })));
const AccessTab = React.lazy(() => import('./AccessTab').then((m) => ({ default: m.AccessTab })));

export function NodeDrawer() {
  const { id, tab: liveTab, setTab, close } = useDrawer();
  const q = useMachine(id);
  // 关闭时 id/tab 已从地址里清掉，但抽屉还要播滑出动画：沿用最后一次的机器、页签和 id，别闪成加载骨架或跳回“概览”
  const m = useHeld(id, q.data);
  const heldTab = React.useRef(liveTab);
  const heldId = React.useRef(id);
  if (id) { heldTab.current = liveTab; heldId.current = id; }
  const tab = id ? liveTab : heldTab.current;
  return (
    <Sheet open={!!id} onOpenChange={(o) => !o && close()}>
      <SheetContent aria-describedby={undefined}>
        {/* 抽屉里图表/懒加载块出错只让抽屉显示错误，换机器或页签后自动恢复 */}
        <ErrorBoundary compact resetKey={`${heldId.current}:${tab}`}>
        {!m ? (
          <>
            <div className="drawer-head"><SheetTitle className="drawer-title">{q.isError ? '无法加载机器' : '加载中…'}</SheetTitle></div>
            <div className="drawer-body">{q.isError ? <ErrorNote error={q.error} onRetry={() => void q.refetch()} /> : <Skeleton h={120} />}</div>
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
              <TabsList>{DRAWER_TABS.map((t) => <TabsTrigger key={t.value} value={t.value} noPanel>{t.label}</TabsTrigger>)}</TabsList>
            </Tabs>
            <div className="drawer-body">
              {m.lifecycle === 'retired' && (
                <Note tone="warn"><b>{m.name}</b> 已于 {F.dtFull(m.retired_at)} 退役（IP {m.ip}）。退役机器保留原编号，可能与在用机器同名，请以 IP 和机器 ID 区分。{m.lifecycle_reason && <> 原因：{m.lifecycle_reason}。</>}</Note>
              )}
              {m.lifecycle === 'failed' && (
                <Note tone="bad">
                  上架失败{m.lifecycle_reason && <>：{m.lifecycle_reason}</>}。{m.failed_job && <><Link to={`/jobs/${m.failed_job.id}`} className="btn-link" onClick={close} style={{ color: 'var(--accent)' }}>查看作业</Link>，可在作业页重试；</>}也可以在「管理」里退役，释放 IP 与编号。
                </Note>
              )}
              {m.active_job && (
                <Note tone="info">
                  {jobKindLabel(m.active_job.kind)}进行中。<Link to={`/jobs/${m.active_job.id}`} className="btn-link" onClick={close} style={{ color: 'var(--accent)' }}>查看作业</Link>
                </Note>
              )}
              <div key={tab} className="tab-pane">
                <React.Suspense fallback={<Skeleton h={180} style={{ borderRadius: 'var(--r-float)' }} />}>
                  {tab === 'overview' && <OverviewTab m={m} />}
                  {tab === 'quality' && <QualityTab m={m} />}
                  {tab === 'traffic' && <TrafficTab m={m} />}
                  {tab === 'host' && <HostTab m={m} />}
                  {tab === 'billing' && <BillingTab m={m} />}
                  {tab === 'access' && <AccessTab m={m} />}
                  {tab === 'manage' && <ManageTab m={m} onClosed={close} />}
                </React.Suspense>
              </div>
            </div>
          </>
        )}
        </ErrorBoundary>
      </SheetContent>
    </Sheet>
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
          <ReasonList reasons={m.health.reasons} className="reason-list" />
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
          <LabelValue label="agent">{m.agent ? `${m.agent.version ?? '版本未知'} · ${m.agent.last_report_at ? F.ago(m.agent.last_report_at) : '尚未上报'}` : '无（家宽代理不装 agent）'}</LabelValue>
          <LabelValue label="入口探测">{m.entries.length ? `${ps.tcpOk}/${ps.total} 连通` : '—'}</LabelValue>
          {m.note && <LabelValue label="备注" className="span-2" wrap>{m.note}</LabelValue>}
        </LabelValueGrid>
      </Section>
      {m.relay && (
        <Section>
          <SectionHeader title="应用状态" />
          <LabelValueGrid>
            <LabelValue label="公网地址">{m.relay.public_host}</LabelValue>
            <LabelValue label="端口范围">{m.relay.port_min}–{m.relay.port_max}</LabelValue>
            <LabelValue label="配置代数">{m.relay.generation} · 已应用 {m.relay.applied_generation}</LabelValue>
            <LabelValue label="上次应用">{F.dtFull(m.relay.applied_at)}</LabelValue>
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
        ) : audit.isError ? <ErrorNote error={audit.error} onRetry={() => void audit.refetch()} /> : <div className="hint">{audit.isLoading ? '加载中…' : '暂无记录'}</div>}
      </Section>
    </>
  );
}
