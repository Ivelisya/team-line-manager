// 机器详情抽屉：概览 / 连接质量 / 流量 / 主机 / 费用 / 管理。开关由 URL 的 ?m= 控制。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { Sheet, SheetContent, SheetDescription, SheetTitle } from '@/components/ui/dialog';
import { Tabs, TabsList, TabsTrigger, Trunc } from '@/components/ui/overlays';
import { InfoTip, OptimisticSwitch } from '@/components/ui/form';
import { toast } from '@/components/ui/toast';
import { IconButton } from '@/components/ui/button';
import { ErrorBoundary } from '@/components/app/ErrorBoundary';
import { Gone, isGone } from '@/components/app/Gone';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import type { Incident, MachineView } from '@/lib/types';
import { AnytlsState } from '@/components/app/AnytlsState';
import { RelayProtocolsField } from '@/components/app/RelayProtocols';
import { useMachine, useAudit, useBillingExempt, useMachineIncidents } from '@/lib/queries';
import { AuditList } from '@/components/app/AuditList';
import { DRAWER_TABS, drawerSiblings, useDrawer, type DrawerTab } from '@/lib/drawer';
import { useHeld } from '@/lib/useHeld';
import { useTitleName } from '@/lib/title';
import * as F from '@/lib/format';
import { jobKindLabel } from '@/lib/jobs';
import { errText, kindLabel, linkCounts, meta, probeSummary, reachWho, severityOf, unreachTip } from '@/lib/derive';
import { useReachLabel } from '@/lib/probePoints';
import { CopyButton, ErrorNote, ReasonList, HealthPill, Flag, LabelValue, LabelValueGrid, LifecyclePill, LinkCounts, Note, Pill, Section, SectionHeader } from '@/components/app/primitives';
import { RecentCount, SuggestedCmds } from '@/components/app/IncidentBits';
import { MachineTags } from '@/components/app/MachineTags';
import { ManageTab } from './ManageTab';
import { BillingTab } from './BillingTab';
import { EntriesTable, entriesMeta } from './EntriesTable';

// 带图表的页签（recharts）按需加载
const QualityTab = React.lazy(() => import('./DrawerCharts').then((m) => ({ default: m.QualityTab })));
const TrafficTab = React.lazy(() => import('./DrawerCharts').then((m) => ({ default: m.TrafficTab })));
const HostTab = React.lazy(() => import('./DrawerCharts').then((m) => ({ default: m.HostTab })));
const AccessTab = React.lazy(() => import('./AccessTab').then((m) => ({ default: m.AccessTab })));

export function NodeDrawer() {
  const { id, tab: liveTab, setTab, switchTo, close } = useDrawer();
  const qc = useQueryClient();
  // 首次挂载后空闲时预取图表块（recharts），第一次点图表页签不用等下载
  React.useEffect(() => {
    const t = setTimeout(() => { import('./DrawerCharts').catch(() => { /* 预取失败不要紧，真用到时会再试 */ }); }, 1500);
    return () => clearTimeout(t);
  }, []);
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
            <div className="drawer-head"><SheetTitle className="drawer-title">{q.isError ? (isGone(q.error) ? '机器不存在' : '无法加载机器') : '加载中…'}</SheetTitle></div>
            <div className="drawer-body">{q.isError ? (isGone(q.error) ? <Gone text="这台机器已不存在"><Button size="sm" onClick={close}>关闭</Button></Gone> : <ErrorNote error={q.error} onRetry={() => void q.refetch()} />) : <Skeleton h={120} />}</div>
          </>
        ) : (
          <>
            {/* m-drawer-head：国旗对齐标题那一行（窄屏标签折到第二行时，国旗不跟着落到两行中间） */}
            <header className="drawer-head m-drawer-head" style={{ paddingRight: 56 }}>
              <Flag country={m.country} manual={!!m.country_set} ip={m.ip} />
              <div className="m-head" style={{ flex: 1 }}>
                <div className="m-head-row"><SheetTitle className="drawer-title ellipsis">{m.name}</SheetTitle><HealthPill m={m} /><UnreachPill m={m} />{/* 维护中已由状态胶囊写了，标签里不再重复 */}<MachineTags m={m.maintenance ? { ...m, maintenance: false } : m} /></div>
                <SheetDescription className="drawer-sub ellipsis">{m.ip} · {m.host_machine ? `同机 · ${m.host_machine.name}` : m.plan?.name ?? '未设套餐'}{!m.provider && ' · 未设供应商'}{m.spare_from && ` · 原 ${m.spare_from}`}</SheetDescription>
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
                <Note tone="warn"><b>{m.name}</b> 已于 {F.dtFull(m.retired_at)} 退役{m.lifecycle_reason && <>：{errText(m.lifecycle_reason)}</>} <InfoTip text="清理已完成，很快移入回收站，之后不再出现在面板里。" /></Note>
              )}
              {m.lifecycle === 'failed' && (
                <Note tone="bad">
                  上架失败{m.lifecycle_reason && <>：{errText(m.lifecycle_reason)}</>}。{m.failed_job && <Link to={`/jobs/${m.failed_job.id}`} className="btn btn-link" onClick={close}>查看作业</Link>} <InfoTip text="可在作业页重试；也可以在「管理」里退役，释放 IP 与编号。" />
                </Note>
              )}
              {m.lifecycle === 'retiring' && m.failed_job && !m.active_job && (
                <Note tone="bad">
                  退役失败 · <Link to={`/jobs/${m.failed_job.id}`} className="btn btn-link" onClick={close}>查看作业</Link> <InfoTip text="机器停在「退役中」，在作业页从失败的步骤重试即可完成退役。" />
                </Note>
              )}
              {/* 团队的软路由都看得清出口、却没有一个指着这台（服务端 no_router_exits）：卡片看着像在用，其实没有客户 */}
              {m.no_router_exits && (
                <Note tone="info">没有软路由出口指着这台 <InfoTip text="团队里的软路由都看过了，没有出口指向它，出口可能已改指向别处（改出口指向、替换、手改）。看着在用其实没有客户；不要了可以退役，要接回客户就把出口改指回来。" /></Note>
              )}
              {m.active_job && (
                <Note tone="info">
                  {jobKindLabel(m.active_job.kind)}进行中。<Link to={`/jobs/${m.active_job.id}`} className="btn btn-link" onClick={close}>查看作业</Link>
                </Note>
              )}
              <div key={tab} className="tab-pane">
                <React.Suspense fallback={<Skeleton h={180} className="is-float" />}>
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

/** agent 自报的异常（自 agent 本次启动起累计）：有丢数据、积压、写不进磁盘或应用失败才显示，一行「agent 自报 N 项异常」，明细在 (i) 里。
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
  const all = [...items, ...(applyErr ? [`最近一次应用配置失败：${errText(applyErr)}`] : [])];
  if (!all.length) return null;
  return (
    <div style={{ marginTop: 8 }}>
      <Note tone="warn">agent 自报 {all.length} 项异常 <InfoTip text={`本次启动以来：\n${all.join('\n')}`} /></Note>
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

/** 入口三段连通数（同卡片与线路页，linkCounts）：「入口可达 1/1 · 节点通 1/1 · 出口通 1/1」；没有在用的入口为「—」 */
function LinkRow({ m }: { m: MachineView }) {
  const who = reachWho(useReachLabel());
  const ps = probeSummary(m);
  return ps.all ? <span><LinkCounts c={linkCounts(ps)} who={who} /></span> : <>—</>;
}

/** 节点的入口端口连不上（线路机那一段的问题，告警记在线路机上，节点自己的健康不算它）：抬头在状态旁补一枚「入口可达 x/y」，
 *  不然「正常」与入口表里标红的行各说各的。线路机自己的已经是故障原因，不补 */
function UnreachPill({ m }: { m: MachineView }) {
  const who = reachWho(useReachLabel());
  const ps = probeSummary(m);
  if (m.kind === 'relay' || m.lifecycle !== 'active' || !ps.reachBad) return null;
  return <Pill tone="warn" tip={unreachTip(who, ps.reachBad)}>入口可达 {ps.all - ps.reachBad}/{ps.all}</Pill>;
}

/** 生命周期旁的原因写成一句话：旧版「恢复已退役机器」存的是「已恢复：原因」；进备用（退役）时填的原因加上「退役原因：」（替换、随宿主机退役的本来就是整句） */
function lifecycleWhy(m: MachineView, raw: string) {
  const r = errText(raw);
  const restored = /^已恢复(?:：(.*))?$/.exec(r);
  if (restored) return restored[1] ? `曾从已退役恢复，原因：${restored[1]}` : '曾从已退役恢复';
  return m.spare && !/^(替换下来|随宿主机)/.test(r) ? `退役原因：${r}` : r;
}

/** 向团队收费时算不算这个节点（团队与套餐 → 计费台账）：在用、在团队里（不在备用）的节点才有；旧中控没有 billing_exempt 时不显示 */
function BillingRow({ m }: { m: MachineView }) {
  const set = useBillingExempt();
  if (m.kind === 'relay' || !m.team || m.spare_from || m.lifecycle !== 'active' || m.billing_exempt === undefined) return null;
  const on = !m.billing_exempt;
  return (
    <LabelValue label="计费" wrap>
      <span className="row" style={{ gap: 8, flexWrap: 'nowrap' }}>
        <OptimisticSwitch checked={on} disabled={set.isPending} aria-label="向团队收这个节点的钱"
          onCheckedChange={(v, undo) => set.mutate({ id: m.id, exempt: !v }, { onSuccess: () => toast.ok(v ? `${m.name} 恢复计费` : `${m.name} 设为不计费`), onError: (e) => { undo(); toast.err(e); } })} />
        <span>{on ? '计费' : '不计费'}</span>
        <InfoTip text={'按节点数向团队收费时算不算这个节点（团队与套餐 → 计费台账）\n切换后按离开 / 加入处理：24 小时内有节点加入或离开就算替换，计费不变'} />
      </span>
    </LabelValue>
  );
}

function OverviewTab({ m }: { m: MachineView }) {
  const audit = useAudit(m.id);
  const { open } = useDrawer();
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
          <LabelValue label="类型">{`${kindLabel(m.kind)}${h ? ` · ${meta().ip_node_label}` : ''}`}</LabelValue>
          {m.kind !== 'relay' && <LabelValue label="团队 / 编号">{m.team ? `${m.team.name} / ${m.number != null ? F.pad2(m.number) : '—'}` : '—'}</LabelValue>}
          <LabelValue label="IP"><span className="row" style={{ gap: 4, flexWrap: 'nowrap', minWidth: 0 }}><Trunc className="num">{m.ip}</Trunc><CopyButton text={m.ip} label=" IP" size={13} show /></span></LabelValue>
          {h && <LabelValue label="宿主机"><button type="button" className="btn btn-link" onClick={() => open(h.id)}>{h.name}</button></LabelValue>}
          {/* 1:1 NAT 映射的附加 IP：公网 IP 不在网卡上，ssserver 监听并从这个本地地址出站；线路机连的仍是上面的 IP */}
          {h && m.bind_ip && <LabelValue label="本地地址"><><span className="num">{m.bind_ip}</span> <InfoTip text={`公网 IP 是 1:1 NAT 映射到宿主机这个地址上的：ssserver 监听并从它出站，线路机连的仍是 ${m.ip}`} /></></LabelValue>}
          <LabelValue label="SSH 端口">{`${m.ssh_port}${ofHost}`}</LabelValue>
          <LabelValue label="供应商">{m.provider?.name}</LabelValue>
          <LabelValue label="套餐">{m.plan?.name ?? (h ? '不带套餐' : undefined)}</LabelValue>
          {/* 供应商后台里这台的服务号（DeerVPS 的 SRV-…、ATTVPS 的数字），对账、开工单时用：不是分类，不进标签；搜索照样能搜到 */}
          <LabelValue label="供应商服务编号">{m.service_ref ? <span className="row" style={{ gap: 4, flexWrap: 'nowrap', minWidth: 0 }}><Trunc className="mono">{m.service_ref}</Trunc><CopyButton text={m.service_ref} label="服务编号" size={13} /></span> : null}</LabelValue>
          {/* 已退役、上架失败的原因与时间在顶上的横幅里，这里只放状态（原因放这格会被截断） */}
          {/* 带原因的占满整行、可换行（半格放不下会被截断成「运行中 已恢复：实验：…」） */}
          {(() => { const why = m.lifecycle_reason && m.lifecycle !== 'retired' && m.lifecycle !== 'failed' ? lifecycleWhy(m, m.lifecycle_reason) : null;
            return <LabelValue label="生命周期" className={why ? 'span-2' : undefined} wrap={!!why}><LifecyclePill lc={m.lifecycle} />{why && <span className="t-sub"> {why}</span>}</LabelValue>; })()}
          <LabelValue label="上架时间">{F.dtFull(m.activated_at)}</LabelValue>
          {m.lifecycle === 'retired' ? null : m.agent ? <>
            <LabelValue label="agent 版本">{`${m.agent.version ?? '未知'}${ofHost}`}</LabelValue>
            <LabelValue label="最近上报">{m.agent.last_report_at ? F.ago(m.agent.last_report_at) : '还没上报'}</LabelValue>
          </> : <LabelValue label="agent"><>无 <InfoTip text="家宽代理不装 agent：没有主机读数，连通性靠线路机的探测" /></></LabelValue>}
          {/* 三段分开数（同卡片、线路页）：节点通只数经节点的入口（直出入口没有 TCP 探测），入口可达、出口通数全部在用的入口 */}
          <LabelValue label="连通" className="span-2" wrap><LinkRow m={m} /></LabelValue>
          {m.ip_nodes.length > 0 && (
            <LabelValue label="同机 IP 节点" className="span-2" wrap>
              <div className="stack-sm" style={{ marginTop: 2 }}>{m.ip_nodes.map((x) => <IpNodeLine key={x.id} id={x.id} name={x.name} onOpen={open} />)}</div>
            </LabelValue>
          )}
          {m.note && <LabelValue label="备注" className="span-2" wrap>{m.note}</LabelValue>}
          <BillingRow m={m} />
        </LabelValueGrid>
        <AgentHealthNote m={m} />
      </Section>
      {m.relay && (
        <Section>
          <SectionHeader title="应用状态" />
          <LabelValueGrid>
            <LabelValue label="公网地址">{m.relay.public_host}</LabelValue>
            <LabelValue label="端口范围">{m.relay.port_min}–{m.relay.port_max}</LabelValue>
            {/* 中控上的配置版本与线路机已生效的版本：相等即已同步，不等时写出从哪一版到哪一版 */}
            <LabelValue label="配置">{m.relay.generation === m.relay.applied_generation ? '已应用'
              : <span style={{ color: 'var(--warn-text)' }}>配置未应用（配置版本 {m.relay.generation}，已生效 {m.relay.applied_generation}）</span>}</LabelValue>
            <LabelValue label="上次应用">{F.dtFull(m.relay.applied_at)}</LabelValue>
            <LabelValue label="AnyTLS"><AnytlsState a={m.agent?.anytls} /></LabelValue>
            <LabelValue label="入口协议"><RelayProtocolsField m={m} /></LabelValue>
          </LabelValueGrid>
          {m.relay.apply_error && <div style={{ marginTop: 8 }}><Note tone="bad">{errText(m.relay.apply_error)}</Note></div>}
        </Section>
      )}
      <Section>
        {/* 入口表只在这里放一份（状态、TCP、出口、本月业务流量）；连接质量、流量页签只留计数链回这里。
            线路机的入口多（几十条）：异常的排前面，先露 10 条，表尾展开全部 */}
        <SectionHeader title={m.kind === 'relay' ? '挂载的入口' : '入口'} meta={entriesMeta(m)} />
        <EntriesTable m={m} traffic pick />
      </Section>
      <RecentIncidents m={m} />
      <Section>
        <SectionHeader title="最近操作" />
        {audit.data?.length ? (
          <AuditList items={audit.data} />
        ) : audit.isError ? <ErrorNote error={audit.error} onRetry={() => void audit.refetch()} /> : <div className="hint">{audit.isLoading ? '加载中…' : '暂无记录'}</div>}
      </Section>
    </>
  );
}

/** 最近告警：规则名，右边进行中 / 恢复观察中 / 已恢复；下一行触发、恢复的时刻与持续多久（进行中算到现在），反复的带次数。
 *  排版同「最近操作」（不画圆点），悬停规则名看摘要，已恢复的整条调淡。
 *  恢复观察中：告警还开着，机器的健康原因里却已经没有这条规则了（告警按健康原因开关，规则名就是原因的 key）——
 *  中控要原因连续消失满去抖时长（出口 150 秒、TCP 90 秒、其余 60 秒）才恢复，探测停了时还要等新探测。维护中健康不给原因，不这么判 */
function RecentIncidents({ m }: { m: MachineView }) {
  const q = useMachineIncidents(m.id);
  const judge = m.lifecycle === 'active' && !m.maintenance;
  return (
    <Section>
      <SectionHeader title="最近告警" />
      {q.data?.length ? (
        <ul className="audit-list is-flat">
          {q.data.map((i) => {
            const firing = i.state === 'firing';
            const settling = firing && judge && !m.health.reasons.some((r) => r.key === i.rule);
            return (
              <li key={i.id} data-resolved={firing ? undefined : ''}>
                <div className="audit-body">
                  <div className="audit-head">
                    <span className="audit-what" data-tip={errText(i.summary)}><b>{i.rule_name}</b></span>
                    {settling ? <span className="audit-meta" data-tip="原因已消失，持续一段时间没有再出现就自动恢复">恢复观察中</span>
                      : <span className="audit-meta" data-tone={firing ? severityOf(i.severity).tone : undefined}>{firing ? '进行中' : '已恢复'}</span>}
                  </div>
                  <div className="audit-detail hint">
                    {incidentSpan(i)}
                    {(i.chronic || i.reopen_count > 0) && <> · <RecentCount i={i} /></>}
                  </div>
                  {firing && <SuggestedCmds i={i} />}
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
  if (!i.resolved_at) return `触发 ${from} · 已持续 ${F.span(Date.now() - Date.parse(i.opened_at))}`;
  const to = F.dt(i.resolved_at);
  return `触发 ${from} → 恢复 ${to.slice(0, 5) === from.slice(0, 5) ? to.slice(6) : to} · 持续 ${F.span(Date.parse(i.resolved_at) - Date.parse(i.opened_at))}`;
}

