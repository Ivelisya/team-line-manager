// 机器详情抽屉：概览 / 连接质量 / 流量 / 主机 / 费用 / 管理。开关由 URL 的 ?m= 控制。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { Sheet, SheetContent, SheetDescription, SheetTitle } from '@/components/ui/dialog';
import { Tabs, TabsList, TabsTrigger, Tip, Trunc } from '@/components/ui/overlays';
import { IconButton } from '@/components/ui/button';
import { ErrorBoundary } from '@/components/app/ErrorBoundary';
import { Skeleton } from '@/components/ui/skeleton';
import type { Incident, MachineView } from '@/lib/types';
import { useMachine, useAudit, useMachineIncidents } from '@/lib/queries';
import { AuditList } from '@/components/app/AuditList';
import { DRAWER_TABS, drawerSiblings, useDrawer, type DrawerTab } from '@/lib/drawer';
import { useHeld } from '@/lib/useHeld';
import { useTitleName } from '@/lib/title';
import * as F from '@/lib/format';
import { KIND_LABEL } from '@/lib/sort';
import { jobKindLabel } from '@/lib/jobs';
import { probeSummary, severityOf } from '@/lib/derive';
import { CopyButton, ErrorNote, ReasonList, HealthPill, Flag, LabelValue, LabelValueGrid, LifecyclePill, Note, Section, SectionHeader, SeverityDot } from '@/components/app/primitives';
import { MachineTags } from '@/components/app/MachineTags';
import { ManageTab } from './ManageTab';
import { BillingTab } from './BillingTab';
import { EntriesTable } from './EntriesTable';

// 带图表的页签（recharts）按需加载
const QualityTab = React.lazy(() => import('./DrawerCharts').then((m) => ({ default: m.QualityTab })));
const TrafficTab = React.lazy(() => import('./DrawerCharts').then((m) => ({ default: m.TrafficTab })));
const HostTab = React.lazy(() => import('./DrawerCharts').then((m) => ({ default: m.HostTab })));
const AccessTab = React.lazy(() => import('./AccessTab').then((m) => ({ default: m.AccessTab })));

export function NodeDrawer() {
  const { id, tab: liveTab, setTab, switchTo, close } = useDrawer();
  const qc = useQueryClient();
  const q = useMachine(id);
  // 关闭时 id/tab 已从地址里清掉，但抽屉还要播滑出动画：沿用最后一次的机器、页签和 id，别闪成加载骨架或跳回“概览”
  const m = useHeld(id, q.data);
  const heldTab = React.useRef(liveTab);
  const heldId = React.useRef(id);
  if (id) { heldTab.current = liveTab; heldId.current = id; }
  const tab = id ? liveTab : heldTab.current;
  // 换机器或页签：正文回到顶部，当前页签滚进可见区（窄屏页签条横向滚动）。用沿用的 id：关闭动画期间不跳
  const bodyRef = React.useRef<HTMLDivElement>(null);
  const tabsRef = React.useRef<HTMLDivElement>(null);
  const shownId = m ? heldId.current : null;
  React.useLayoutEffect(() => {
    if (bodyRef.current) bodyRef.current.scrollTop = 0;
    tabsRef.current?.querySelector('[data-state="active"]')?.scrollIntoView({ inline: 'nearest', block: 'nearest' });
  }, [shownId, tab]);
  // 标签页标题带上这台机器的名字（关闭动画期间已经去掉），拼接见 lib/title
  useTitleName(id ? m?.name : undefined);
  // 上一台 / 下一台：同 [ ] 键，用沿用的 id，关闭动画期间按钮不消失
  const sib = drawerSiblings(qc, shownId);
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
              <Flag country={m.country} ip={m.ip} />
              <div className="m-head" style={{ flex: 1 }}>
                <div className="m-head-row"><SheetTitle className="drawer-title ellipsis">{m.name}</SheetTitle><HealthPill m={m} /><MachineTags m={m} /></div>
                <SheetDescription className="ellipsis" style={{ margin: 0, color: 'var(--fg-2)', fontSize: 12 }}>{m.ip} · {m.host_machine ? `同机 · ${m.host_machine.name}` : m.plan?.name ?? '未设套餐'}{!m.provider && ' · 未设供应商'}</SheetDescription>
              </div>
              {sib && (
                <div className="drawer-nav">
                  <IconButton label="上一台" tip={false} data-tip="上一台（快捷键 [）" aria-keyshortcuts="[" disabled={!sib.prev} onClick={() => sib.prev && switchTo(sib.prev)}><ChevronLeft size={16} /></IconButton>
                  <IconButton label="下一台" tip={false} data-tip="下一台（快捷键 ]）" aria-keyshortcuts="]" disabled={!sib.next} onClick={() => sib.next && switchTo(sib.next)}><ChevronRight size={16} /></IconButton>
                </div>
              )}
            </header>
            <Tabs value={tab} onValueChange={(v) => setTab(v as DrawerTab)} className="drawer-tabs">
              <TabsList ref={tabsRef}>{DRAWER_TABS.map((t) => <TabsTrigger key={t.value} value={t.value} noPanel>{t.label}</TabsTrigger>)}</TabsList>
            </Tabs>
            <div className="drawer-body" ref={bodyRef}>
              {m.lifecycle === 'retired' && (
                <Note tone="warn"><b>{m.name}</b> 已于 {F.dtFull(m.retired_at)} 退役（IP {m.ip}）。退役机器保留原编号，可能与在用机器同名，请以 IP 和机器 ID 区分。{m.lifecycle_reason && <> 原因：{m.lifecycle_reason}。</>}</Note>
              )}
              {m.lifecycle === 'failed' && (
                <Note tone="bad">
                  上架失败{m.lifecycle_reason && <>：{m.lifecycle_reason}</>}。{m.failed_job && <><Link to={`/jobs/${m.failed_job.id}`} className="btn btn-link" onClick={close}>查看作业</Link>，可在作业页重试；</>}也可以在「管理」里退役，释放 IP 与编号。
                </Note>
              )}
              {m.lifecycle === 'retiring' && m.failed_job && !m.active_job && (
                <Note tone="bad">
                  退役失败 · <Link to={`/jobs/${m.failed_job.id}`} className="btn btn-link" onClick={close}>查看作业</Link>。机器停在「退役中」，在作业页从失败的步骤重试即可完成退役。
                </Note>
              )}
              {m.active_job && (
                <Note tone="info">
                  {jobKindLabel(m.active_job.kind)}进行中。<Link to={`/jobs/${m.active_job.id}`} className="btn btn-link" onClick={close}>查看作业</Link>
                </Note>
              )}
              <div key={tab} className="tab-pane">
                <React.Suspense fallback={<Skeleton h={180} style={{ borderRadius: 'var(--r-float)' }} />}>
                  {tab === 'overview' && <OverviewTab m={m} />}
                  {tab === 'quality' && <QualityTab m={m} />}
                  {tab === 'traffic' && <TrafficTab m={m} />}
                  {tab === 'host' && <HostTab m={m} />}
                  {tab === 'billing' && <BillingTab m={m} />}
                  {tab === 'access' && <AccessTab id={m.id} relay={m.kind === 'relay'} />}
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

/** agent 自报的异常（自 agent 本次启动起累计）：有丢数据、积压、写不进磁盘或应用失败才显示。
 *  访问日志解析不了 / 被丢弃的行也列在这里：Xray 日志格式一变，访问监控会静默归零，这是唯一能看到的地方。
 *  线路机的应用失败已在「应用状态」里显示，相同的不重复 */
function AgentHealthNote({ m }: { m: MachineView }) {
  const h = m.agent?.health;
  if (!h) return null;
  const items = [
    h.disk_write_error && `报告写不进磁盘、正从内存直接上报（${h.disk_write_error}）`,
    h.queue_len > 3 && `${h.queue_len} 份报告积压待发`,
    h.queue_dropped > 0 && `${h.queue_dropped} 份报告因离线队列超限被丢弃`,
    h.queue_dead > 0 && `${h.queue_dead} 份报告被中控拒收`,
    h.access_unparsed > 0 && `访问日志有 ${h.access_unparsed} 行解析不了`,
    h.access_dropped_lines > 0 && `${h.access_dropped_lines} 行访问日志被丢弃（超长、含非法字符或不完整）`,
    h.access_rotation_gaps + h.access_truncations > 0 && `访问日志有 ${h.access_rotation_gaps + h.access_truncations} 段漏读`,
  ].filter(Boolean);
  const applyErr = h.last_apply_error && h.last_apply_error !== m.relay?.apply_error ? h.last_apply_error : null;
  if (!items.length && !applyErr) return null;
  return (
    <div style={{ marginTop: 8 }}>
      <Note tone="warn">agent 自报（本次启动以来）：{items.join('，')}{items.length > 0 && applyErr && '；'}{applyErr && <>最近一次应用配置失败：{applyErr}</>}</Note>
    </div>
  );
}

/** 宿主机概览里的一个同机 IP 节点：名称（点开它的抽屉）、IP、状态。用机器列表缓存里的那份，没有时单取这台 */
function IpNodeLine({ id, name, onOpen }: { id: string; name: string; onOpen: (id: string) => void }) {
  const n = useMachine(id).data;
  return (
    <div className="row" style={{ gap: 8, flexWrap: 'nowrap' }}>
      <button type="button" className="btn btn-link" onClick={() => onOpen(id)}>{name}</button>
      {n && <><span className="t-sub num">{n.ip}</span><HealthPill m={n} /></>}
    </div>
  );
}

function OverviewTab({ m }: { m: MachineView }) {
  const audit = useAudit(m.id);
  const { open } = useDrawer();
  const ps = probeSummary(m);
  // 同机 IP 节点：SSH 端口、agent 都是宿主机的
  const h = m.host_machine;
  const ofHost = h ? '（宿主机的）' : '';
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
          <LabelValue label="类型">{`${KIND_LABEL[m.kind]}${h ? ' · 同机 IP 节点' : ''}`}</LabelValue>
          <LabelValue label="团队 / 编号">{m.team ? `${m.team.name} / ${m.number != null ? F.pad2(m.number) : '—'}` : '—'}</LabelValue>
          <LabelValue label="IP">{m.ip}</LabelValue>
          {h && <LabelValue label="宿主机"><button type="button" className="btn btn-link" onClick={() => open(h.id)}>{h.name}</button></LabelValue>}
          <LabelValue label="SSH 端口">{`${m.ssh_port}${ofHost}`}</LabelValue>
          <LabelValue label="供应商">{m.provider?.name}</LabelValue>
          <LabelValue label="套餐">{m.plan?.name ?? (h ? '不带套餐' : undefined)}</LabelValue>
          {/* 供应商后台里这台的服务号（DeerVPS 的 SRV-…、ATTVPS 的数字），对账、开工单时用：不是分类，不进标签；搜索照样能搜到 */}
          <LabelValue label="供应商服务编号">{m.service_ref ? <span className="row" style={{ gap: 4, flexWrap: 'nowrap', minWidth: 0 }}><Trunc className="mono">{m.service_ref}</Trunc><CopyButton text={m.service_ref} label="服务编号" size={13} /></span> : null}</LabelValue>
          <LabelValue label="生命周期"><LifecyclePill lc={m.lifecycle} />{m.lifecycle_reason && <span className="t-sub"> {m.lifecycle_reason}</span>}</LabelValue>
          <LabelValue label="上架时间">{F.dtFull(m.activated_at)}</LabelValue>
          {m.retired_at && <LabelValue label="退役时间">{F.dtFull(m.retired_at)}</LabelValue>}
          <LabelValue label="agent">{m.agent ? `${m.agent.version ?? '版本未知'} · ${m.agent.last_report_at ? F.ago(m.agent.last_report_at) : '尚未上报'}${ofHost}` : '无（家宽代理不装 agent）'}</LabelValue>
          {/* 只数经节点的入口：直出入口没有 TCP 探测 */}
          <LabelValue label="入口探测">{ps.total ? `${ps.tcpOk}/${ps.total} 连通` : '—'}</LabelValue>
          {m.ip_nodes.length > 0 && (
            <LabelValue label="同机 IP 节点" className="span-2" wrap>
              <div className="stack-sm" style={{ marginTop: 2 }}>{m.ip_nodes.map((x) => <IpNodeLine key={x.id} id={x.id} name={x.name} onOpen={open} />)}</div>
            </LabelValue>
          )}
          {m.note && <LabelValue label="备注" className="span-2" wrap>{m.note}</LabelValue>}
        </LabelValueGrid>
        <AgentHealthNote m={m} />
      </Section>
      {m.relay && (
        <Section>
          <SectionHeader title="应用状态" />
          <LabelValueGrid>
            <LabelValue label="公网地址">{m.relay.public_host}</LabelValue>
            <LabelValue label="端口范围">{m.relay.port_min}–{m.relay.port_max}</LabelValue>
            <LabelValue label="配置版本"><Tip text="两个数相等表示线路机已同步"><span tabIndex={0}>{m.relay.generation} · 已生效 {m.relay.applied_generation}</span></Tip></LabelValue>
            <LabelValue label="上次应用">{F.dtFull(m.relay.applied_at)}</LabelValue>
          </LabelValueGrid>
          {m.relay.apply_error && <div style={{ marginTop: 8 }}><Note tone="bad">{m.relay.apply_error}</Note></div>}
        </Section>
      )}
      <Section>
        <SectionHeader title={m.kind === 'relay' ? '挂载的入口' : '入口'} meta={`${m.entries.length} 条`} />
        <EntriesTable m={m} />
      </Section>
      <RecentIncidents id={m.id} />
      <Section>
        <SectionHeader title="最近操作" />
        {audit.data?.length ? (
          <AuditList items={audit.data} />
        ) : audit.isError ? <ErrorNote error={audit.error} onRetry={() => void audit.refetch()} /> : <div className="hint">{audit.isLoading ? '加载中…' : '暂无记录'}</div>}
      </Section>
    </>
  );
}

/** 最近告警：级别点 + 规则名，右边进行中 / 已恢复；下一行触发、恢复的时刻与持续多久（进行中算到现在），反复的带次数。
 *  排版同「最近操作」，悬停规则名看摘要，已恢复的级别点调淡 */
function RecentIncidents({ id }: { id: string }) {
  const q = useMachineIncidents(id);
  return (
    <Section>
      <SectionHeader title="最近告警" />
      {q.data?.length ? (
        <ul className="audit-list">
          {q.data.map((i) => {
            const firing = i.state === 'firing';
            return (
              <li key={i.id} data-resolved={firing ? undefined : ''}>
                <SeverityDot severity={i.severity} />
                <div className="audit-body">
                  <div className="audit-head">
                    <span className="audit-what" data-tip={i.summary}><b>{i.rule_name}</b></span>
                    <span className="audit-meta" data-tone={firing ? severityOf(i.severity).tone : undefined}>{firing ? '进行中' : '已恢复'}</span>
                  </div>
                  <div className="audit-detail hint">
                    {incidentSpan(i)}
                    {i.reopen_count > 0 && <> · <span data-tip={`恢复后 30 分钟内又复发 ${i.reopen_count} 次，合并为这一条`}>反复 {i.reopen_count + 1} 次</span></>}
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
      ) : q.isError ? <ErrorNote error={q.error} onRetry={() => void q.refetch()} /> : <div className="hint">{q.isLoading ? '加载中…' : '最近没有告警'}</div>}
    </Section>
  );
}

/** 「触发 10-22 05:12 → 恢复 05:40 · 持续 28 分钟」（恢复在同一天只写时刻）；进行中「触发 10-22 05:12 · 已持续 28 分钟」 */
function incidentSpan(i: Incident): string {
  const from = F.dt(i.opened_at);
  if (!i.resolved_at) return `触发 ${from} · 已持续 ${F.str(F.since(i.opened_at))}`;
  const to = F.dt(i.resolved_at);
  return `触发 ${from} → 恢复 ${to.slice(0, 5) === from.slice(0, 5) ? to.slice(6) : to} · 持续 ${F.str(F.since(i.opened_at, Date.parse(i.resolved_at)))}`;
}

