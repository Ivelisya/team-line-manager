// 机器详情抽屉：概览 / 连接质量 / 流量 / 主机 / 费用 / 管理。开关由 URL 的 ?m= 控制。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { Sheet, SheetContent, SheetDescription, SheetTitle } from '@/components/ui/dialog';
import { Tabs, TabsList, TabsTrigger, Tip } from '@/components/ui/overlays';
import { IconButton } from '@/components/ui/button';
import { ErrorBoundary } from '@/components/app/ErrorBoundary';
import { Skeleton } from '@/components/ui/skeleton';
import type { AuditEntry, MachineView } from '@/lib/types';
import { useMachine, useAudit } from '@/lib/queries';
import { DRAWER_TABS, drawerSiblings, useDrawer, type DrawerTab } from '@/lib/drawer';
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
  // 标签页标题换成这台机器的名字，关上抽屉恢复
  const titleName = id ? m?.name : undefined;
  React.useEffect(() => {
    if (!titleName) return;
    const prev = document.title;
    document.title = `${titleName} · 团队线路管理`;
    return () => { document.title = prev; };
  }, [titleName]);
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
              <KindIcon kind={m.kind} />
              <div className="m-head" style={{ flex: 1 }}>
                <div className="m-head-row"><SheetTitle className="drawer-title ellipsis">{m.name}</SheetTitle><HealthPill m={m} />{m.kind !== 'vps' && <Chip variant="neutral">{KIND_LABEL[m.kind]}</Chip>}</div>
                <SheetDescription className="ellipsis" style={{ margin: 0, color: 'var(--fg-2)', fontSize: 12 }}>{m.ip} · {m.provider?.name ?? '未设供应商'} · {m.host_machine ? `同机 · ${m.host_machine.name}` : m.plan?.name ?? '未设套餐'}</SheetDescription>
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

/** agent 自报的异常（自 agent 本次启动起累计）：有丢数据、积压或应用失败才显示。
 *  访问日志解析不了 / 被丢弃的行也列在这里：Xray 日志格式一变，访问监控会静默归零，这是唯一能看到的地方。
 *  线路机的应用失败已在「应用状态」里显示，相同的不重复 */
function AgentHealthNote({ m }: { m: MachineView }) {
  const h = m.agent?.health;
  if (!h) return null;
  const items = [
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
          <LabelValue label="服务编号">{m.service_ref}</LabelValue>
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
      <Section>
        <SectionHeader title="最近操作" />
        {audit.data?.length ? (
          <ul className="audit-list">
            {audit.data.map((a) => {
              const p = auditParts(a);
              return (
                <li key={a.id}>
                  <span className="dot" data-tone="neutral" />
                  <div className="audit-body">
                    <div className="audit-head">
                      <span className="audit-what"><b>{a.title}</b>{p.target && <> {p.target}</>}</span>
                      <span className="audit-meta">{a.actor} · <span data-tip={F.dtFull(a.ts)}>{F.ago(a.ts)}</span></span>
                    </div>
                    {p.rest && <div className="audit-detail mono" data-tip={p.rest}>{p.rest}</div>}
                  </div>
                </li>
              );
            })}
          </ul>
        ) : audit.isError ? <ErrorNote error={audit.error} onRetry={() => void audit.refetch()} /> : <div className="hint">{audit.isLoading ? '加载中…' : '暂无记录'}</div>}
      </Section>
    </>
  );
}

/** 一条操作记录拆成对象与详情：摘要以记录里的显示名开头时（上架、执行命令、换线路机…），名字接在动作后面，其余是详情。
 *  名字保持记录时的样子，之后改过名也不跟着变 */
function auditParts(a: AuditEntry): { target: string; rest: string } {
  const name = (a.detail as { name?: unknown } | null)?.name;
  if (typeof name !== 'string' || !name) return { target: '', rest: a.summary };
  if (a.summary === name) return { target: name, rest: '' };
  if (a.summary.startsWith(`${name} · `)) return { target: name, rest: a.summary.slice(name.length + 3) };
  return { target: '', rest: a.summary };
}
