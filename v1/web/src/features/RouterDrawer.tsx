// 软路由详情抽屉：概览 / 出口 / 客户端 / 曲线 / 管理。开关由 URL 的 ?r=（页签 rtab）控制，与机器抽屉（?m=）分开；出口对应的入口点开时换成机器抽屉，后退回来。
// 骨架同机器抽屉：头部是名称 + 状态 + 上一台 / 下一台，基本信息是「标签在上、值在下」的网格，操作都在最后的「管理」页签（改名是页签里的表单）。
// 总览数字不带框，出口关系画成「出口 → 线路机 → 节点」，部署进度是时间线。
import * as React from 'react';
import { ChevronLeft, ChevronRight, CircleX, Router as RouterIcon, TriangleAlert, XCircle } from 'lucide-react';
import { Sheet, SheetContent, SheetDescription, SheetTitle } from '@/components/ui/dialog';
import { Tabs, TabsList, TabsTrigger, Trunc } from '@/components/ui/overlays';
import { Button, IconButton } from '@/components/ui/button';
import { Field, InfoTip, Input, Textarea } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Skeleton } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ErrorBoundary } from '@/components/app/ErrorBoundary';
import { ConfirmDialog, TypedConfirmDialog } from '@/components/app/dialogs';
import { CopyButton, ErrorNote, LabelValue, LabelValueGrid, Note, Num, Pill, Section, SectionHeader, SeverityMark, TableWrap } from '@/components/app/primitives';
import { Tag } from '@/components/app/viz';
import { DeployTimeline, MiniBeat, RouterMeters, RouterStatePill, Seen, useShowBootstrap } from '@/components/app/RouterCard';
import { useEndSubjectMaint, useMachineBriefs, useTeams } from '@/lib/queries';
import { MaintenanceDialog } from '@/components/app/MaintenanceDialog';
import { deadExitNames, exitBeats, exitUsable, ROUTER_TABS, routerIssues, routerSiblings, showDeploy, showRemoved, tempTone, uptimeFmt, useDeleteRouter, usePatchRouter, useRouter, useRouterDrawer, useRouterExitMetrics, useRouters, type RouterClient, type RouterDetail, type RouterExit, type RouterTab, type RouterView } from '@/lib/routers';
import { tagColor } from '@/lib/derive';
import { useHeld } from '@/lib/useHeld';
import { openTerminal } from '@/lib/terminal';
import { useTitleName } from '@/lib/title';
import * as F from '@/lib/format';
import { ApiError } from '@/lib/api';

const ChartsTab = React.lazy(() => import('./RouterCharts').then((m) => ({ default: m.RouterChartsTab })));

export function RouterDrawer() {
  const { id, tab: liveTab, setTab, close } = useRouterDrawer();
  const q = useRouter(id);
  // 详情没回来前先用列表里那份（头部、状态马上就有）；关闭动画期间两份都沿用最后一次的，不闪骨架
  const full = useHeld(id, q.data);
  const brief = useHeld(id, useRouters().data?.find((x) => x.id === id));
  const r: RouterView | undefined = full ?? brief;
  const detail = full?.detail; // undefined = 还在取；null = 没采集成功过
  const heldTab = React.useRef(liveTab);
  if (id) heldTab.current = liveTab;
  const tab = id ? liveTab : heldTab.current;
  // 换路由或页签：正文回到顶部，当前页签滚进可见区（窄屏页签条横向滚动），同机器抽屉
  const bodyRef = React.useRef<HTMLDivElement>(null);
  const tabsRef = React.useRef<HTMLDivElement>(null);
  React.useLayoutEffect(() => {
    if (bodyRef.current) bodyRef.current.scrollTop = 0;
    tabsRef.current?.querySelector('[data-state="active"]')?.scrollIntoView({ inline: 'nearest', block: 'nearest' });
  }, [r?.id, tab]);
  useTitleName(id ? r?.name : undefined);
  return (
    <Sheet open={!!id} onOpenChange={(o) => !o && close()}>
      <SheetContent aria-describedby={undefined}>
        <ErrorBoundary compact resetKey={`${r?.id}:${tab}`}>
          {!r ? (
            <>
              <div className="drawer-head"><SheetTitle className="drawer-title">{q.isError ? '无法加载软路由' : '加载中…'}</SheetTitle></div>
              <div className="drawer-body">{q.isError ? <ErrorNote error={q.error} onRetry={() => void q.refetch()} /> : <Skeleton h={120} />}</div>
            </>
          ) : (
            <>
              <Head r={r} detail={detail} />
              <Tabs value={tab} onValueChange={(v) => setTab(v as RouterTab)} className="drawer-tabs">
                <TabsList ref={tabsRef}>{ROUTER_TABS.map((t) => <TabsTrigger key={t.value} value={t.value} noPanel>{t.label}</TabsTrigger>)}</TabsList>
              </Tabs>
              <div className="drawer-body" ref={bodyRef}>
                {q.isError && !q.data && <ErrorNote error={q.error} onRetry={() => void q.refetch()}>详情加载失败：</ErrorNote>}
                <div key={tab} className="tab-pane">
                  <React.Suspense fallback={<Skeleton h={180} className="is-float" />}>
                    {tab === 'overview' && <OverviewTab r={r} detail={detail} />}
                    {tab === 'exits' && <Needs r={r} detail={detail}>{(d) => <ExitsTab id={r.id} d={d} />}</Needs>}
                    {tab === 'clients' && <Needs r={r} detail={detail}>{(d) => <ClientsTab d={d} />}</Needs>}
                    {tab === 'charts' && <ChartsTab r={r} />}
                    {tab === 'manage' && <ManageTab r={r} onClosed={close} />}
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

/** 出口、客户端只在采集成功过之后才有 */
function Needs({ r, detail, children }: { r: RouterView; detail: RouterDetail | null | undefined; children: (d: RouterDetail) => React.ReactNode }) {
  if (detail === undefined) return <Skeleton h={180} className="is-float" />;
  if (!detail) return <div className="hint">{r.state === 'pending' ? '接入后才有数据' : '还没有数据'}</div>;
  return <>{children(detail)}</>;
}

/** 输入框、对话框里不抢 [ ] 键（同全局快捷键） */
const typingIn = (t: EventTarget | null) => t instanceof HTMLElement && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName));

/** 头部（同机器抽屉）：类型图标 + 名称 + 状态 + 团队，副标题 WAN 与系统；右边上一台 / 下一台（快捷键 [ ]），顺序是软路由页当前看到的 */
function Head({ r, detail }: { r: RouterView; detail: RouterDetail | null | undefined }) {
  const { switchTo } = useRouterDrawer();
  const sib = routerSiblings(useRouters().data, r.id);
  const ref = React.useRef(sib); ref.current = sib;
  React.useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.key !== '[' && e.key !== ']') || e.metaKey || e.ctrlKey || e.altKey || typingIn(e.target) || document.querySelector('.dialog, .cmd-dialog')) return;
      const to = ref.current && (e.key === ']' ? ref.current.next : ref.current.prev);
      if (!to) return;
      e.preventDefault();
      switchTo(to);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [switchTo]);
  const sub = [r.summary?.wan.ip && `WAN ${r.summary.wan.ip}`, detail?.system.os, `隧道端口 ${r.tunnel_port}`].filter(Boolean).join(' · ');
  return (
    <header className="drawer-head m-drawer-head" style={{ paddingRight: 56 }}>
      <span className="nc-kind" role="img" aria-label="软路由"><RouterIcon /></span>
      <div className="m-head" style={{ flex: 1 }}>
        <div className="m-head-row"><SheetTitle className="drawer-title ellipsis">{r.name}</SheetTitle><RouterStatePill r={r} />{r.team && <Tag c={tagColor(r.team.name)} tip="团队">{r.team.name}</Tag>}</div>
        <SheetDescription className="drawer-sub ellipsis">{sub}</SheetDescription>
      </div>
      {sib && (
        <div className="drawer-nav">
          <IconButton label="上一台" tip={false} data-tip="上一台（快捷键 [）" aria-keyshortcuts="[" disabled={!sib.prev} onClick={() => sib.prev && switchTo(sib.prev)}><ChevronLeft size={16} /></IconButton>
          <IconButton label="下一台" tip={false} data-tip="下一台（快捷键 ]）" aria-keyshortcuts="]" disabled={!sib.next} onClick={() => sib.next && switchTo(sib.next)}><ChevronRight size={16} /></IconButton>
        </div>
      )}
    </header>
  );
}

/** 管理页签（同机器抽屉的「管理」）：改名 / 团队 / 备注的表单，接入与终端一组，维护窗口一组，删除单成一组（危险） */
function ManageTab({ r, onClosed }: { r: RouterView; onClosed: () => void }) {
  return (
    <>
      <RenameSection r={r} />
      <AccessSection r={r} onClosed={onClosed} />
      <MaintSection r={r} />
      <DeleteSection r={r} onClosed={onClosed} />
    </>
  );
}

/** 名称 / 团队 / 备注：只提交改了的项。实时推送改了服务端的值时，只同步用户没动过的字段（同机器的改名表单），换了路由全部重置 */
function RenameSection({ r }: { r: RouterView }) {
  const patch = usePatchRouter();
  const teams = useTeams().data;
  const opts = React.useMemo(() => (teams ?? []).map((t) => ({ value: t.id, label: t.name })), [teams]);
  const srv = { id: r.id, name: r.name, team: r.team?.id ?? '', note: r.note };
  const [name, setName] = React.useState(srv.name); const [team, setTeam] = React.useState(srv.team); const [note, setNote] = React.useState(srv.note);
  const last = React.useRef(srv);
  React.useEffect(() => {
    const prev = last.current; last.current = srv;
    const keep = (v: string, was: string, now: string) => (prev.id === srv.id && v !== was ? v : now);
    setName((v) => keep(v, prev.name, srv.name)); setTeam((v) => keep(v, prev.team, srv.team)); setNote((v) => keep(v, prev.note, srv.note));
  }, [srv.id, srv.name, srv.team, srv.note]); // eslint-disable-line react-hooks/exhaustive-deps
  const body = { ...(name.trim() !== r.name ? { name: name.trim() } : {}), ...(team !== (r.team?.id ?? '') ? { team_id: team || null } : {}), ...(note.trim() !== r.note ? { note: note.trim() } : {}) };
  const dirty = Object.keys(body).length > 0;
  const valid = dirty && !!name.trim();
  const save = () => patch.mutate({ id: r.id, ...body }, { onSuccess: () => toast.ok('已保存'), onError: toast.err });
  return (
    <Section>
      <SectionHeader title={<>改名 / 团队 / 备注 <InfoTip text="只改面板里的显示，不影响路由和客户端上网" /></>} />
      <form className="form-grid" onSubmit={(e) => { e.preventDefault(); if (valid && !patch.isPending) save(); }}>
        <Field label="名称" error={!name.trim() ? '名称不能为空' : undefined}><Input value={name} onChange={(e) => setName(e.target.value)} autoComplete="off" aria-invalid={!name.trim()} /></Field>
        <Field label="团队"><Select value={team} onChange={setTeam} options={opts} allowEmpty="不分团队" placeholder="不分团队" /></Field>
        <Field label="备注" className="span-2"><Textarea rows={2} value={note} onChange={(e) => setNote(e.target.value)} /></Field>
        <div className="span-2 row" style={{ justifyContent: 'flex-end' }}>
          <Button type="submit" variant="primary" disabled={!valid || patch.isPending} loading={patch.isPending}>保存</Button>
        </div>
      </form>
    </Section>
  );
}

/** 接入命令、重新部署、终端：每项一行（标题 + 灰色副行，右边按钮），同机器管理页签的分组列表 */
function AccessSection({ r, onClosed }: { r: RouterView; onClosed: () => void }) {
  const { open } = useRouterDrawer();
  const boot = useShowBootstrap();
  const [askRegen, setAskRegen] = React.useState(false);
  const pending = r.state === 'pending';
  return (
    <Section>
      <div className="mgroup">
        <div className="manage-row">
          <div><div className="t">接入命令</div><div className="hint">{pending ? '在路由上执行接入命令，约半分钟后上线' : '中控连不上路由时，到路由上执行命令重新接入'}</div></div>
          {/* 还没连上过的没有隧道可断，直接生成；连上过的先确认（旧隧道立刻失效） */}
          <Button variant="secondary" loading={boot.pending} onClick={() => (r.last_seen_at ? setAskRegen(true) : void boot.run(r))}>{pending ? '查看接入命令' : '重新生成…'}</Button>
        </div>
        <div className="manage-row">
          <div><div className="t">一键部署</div><div className="hint">中控通过 SSH 登录路由重新安装，需要账号密码</div></div>
          <Button variant="secondary" onClick={() => showDeploy({ id: r.id, name: r.name })}>重新部署…</Button>
        </div>
        {!pending && (
          <div className="manage-row">
            <div><div className="t">终端</div><div className="hint">经隧道登录路由</div></div>
            {/* 打开终端先收起抽屉：终端窗口的层级在抽屉遮罩之下，开着抽屉会被盖住；关掉终端回到这一页 */}
            <Button variant="secondary" onClick={() => { onClosed(); openTerminal({ kind: 'router', id: r.id, name: r.name }, () => open(r.id, 'manage')); }}>打开终端</Button>
          </div>
        )}
      </div>
      <ConfirmDialog open={askRegen} onOpenChange={setAskRegen} title={`重新生成接入命令 · ${r.name}`} confirmLabel="重新生成" danger
        description={<>隧道会立即断开<InfoTip text={'旧命令会立即失效，路由在执行新命令前连不上中控\n不影响客户端上网，只是这段时间看不到路由状态'} /></>}
        onConfirm={() => boot.run(r)} />
    </Section>
  );
}

/** 维护窗口（同机器管理页签的那一行，对话框也是同一个）：客户现场计划断电、换宽带前开，期间不发告警、开着的保持；维护中可以提前结束 */
function MaintSection({ r }: { r: RouterView }) {
  const [open, setOpen] = React.useState(false);
  const end = useEndSubjectMaint();
  return (
    <Section>
      <div className="mgroup">
        <div className="manage-row">
          <div><div className="t">维护窗口 <InfoTip text={'客户现场断电、换宽带前开\n期间不发告警，开着的告警保持到维护结束，卡片上显示「维护」'} /></div>{r.maintenance && <div className="hint">维护中</div>}</div>
          {r.maintenance
            ? <Button variant="secondary" loading={end.isPending} onClick={() => end.mutate({ kind: 'router', id: r.id }, { onSuccess: () => toast.ok(`${r.name} 已结束维护`), onError: toast.err })}>结束维护</Button>
            : <Button variant="secondary" onClick={() => setOpen(true)}>维护窗口…</Button>}
        </div>
      </div>
      <MaintenanceDialog m={r} kind="router" open={open} onOpenChange={setOpen} />
    </Section>
  );
}

function DeleteSection({ r, onClosed }: { r: RouterView; onClosed: () => void }) {
  const del = useDeleteRouter();
  const [ask, setAsk] = React.useState(false);
  // 删除被 409 拦下（有作业改过它的出口、没确认改回）：对话框里留着原因，确认按钮换成强制删除，已输入的名称清空、要重新输入
  const [force, setForce] = React.useState(false);
  return (
    <Section>
      <div className="mgroup is-danger">
        <div className="manage-row">
          <div><div className="t">删除</div><div className="hint">只删面板里的记录，路由本身不受影响 <InfoTip text={'会撤销隧道授权并删除历史数据\n删除后会给出一条在路由上执行的清理命令'} /></div></div>
          <Button variant="outline-danger" onClick={() => setAsk(true)}>删除…</Button>
        </div>
      </div>
      <TypedConfirmDialog open={ask} onOpenChange={(o) => { setAsk(o); if (!o) setForce(false); }} title={`删除软路由「${r.name}」？`} confirmLabel={force ? '强制删除' : '删除'} danger expected={r.name} prompt="请输入名称"
        description="只删面板里的记录，路由本身不受影响" retype={force}
        onConfirm={async () => {
          let jobs: string[] = [];
          try { jobs = (await del.mutateAsync({ id: r.id, force })).jobs ?? []; } catch (e) { if (e instanceof ApiError && e.status === 409) setForce(true); throw e; }
          onClosed();
          // 强制删除摘掉了哪些作业：它们改过的这台路由的出口不会再有人改回
          if (jobs.length) toast.show({ tone: 'info', title: `已从 ${jobs.length} 个作业里摘掉「${r.name}」`, text: `作业 ${jobs.join('、')} 改过的出口不会再改回，需要自己处理`, duration: 15_000 });
          // 从没连上过、也没一键部署过的：路由上没装 fleet-tunnel，不用给清理命令
          if (r.last_seen_at || r.deploy) showRemoved(r.name); else toast.ok(`已删除「${r.name}」`);
        }} />
    </Section>
  );
}

/** 时钟偏差（clock_skew_s = 中控 − 路由，正数 = 路由慢；同机器的 clock_skew_ms）说成「慢 3 秒」「快 2 秒」 */
const skewWord = (s: number) => (Math.round(s) === 0 ? '一致' : `${s > 0 ? '慢' : '快'} ${Math.abs(Math.round(s))} 秒`);

function OverviewTab({ r, detail }: { r: RouterView; detail: RouterDetail | null | undefined }) {
  const s = r.summary;
  const { setTab } = useRouterDrawer();
  const issues = r.state === 'online' ? routerIssues(s, deadExitNames(detail)) : [];
  const d = r.deploy;
  // 部署时间线：在跑、失败了，或刚做完（10 分钟内）才放
  const showDeployTl = d && (d.state !== 'done' || (d.finished_at != null && Date.now() - Date.parse(d.finished_at) < 600_000));
  const sys = detail?.system;
  const oc = detail?.openclash;
  // 离线时下面的读数是掉线前最后一次读到的：弱化显示并标明是什么时候的
  const stale = r.state === 'offline' && !!(s || detail);
  const staleCls = stale ? 'rt-stale' : undefined;
  const skewed = sys?.clock_skew_s != null && Math.abs(sys.clock_skew_s) > 5;
  return (
    <>
      {r.state === 'offline' && <Note tone="bad">连不上路由{r.error && <>：<span className="mono">{r.error}</span></>}<InfoTip text="隧道断开不影响客户端上网，只是这里看不到状态" /></Note>}
      {r.state === 'online' && r.error && <Note tone="warn">这次没读到路由状态：<span className="mono">{r.error}</span>{detail?.collected_at && <>（下面是 <Seen at={detail.collected_at} />的数据）</>}<InfoTip text="路由连得上，不算离线" /></Note>}
      {oc?.config_error && <Note tone="warn">读不到 OpenClash 配置：<span className="mono">{oc.config_error}</span>{oc.config_stale ? '。出口按上次读到的配置对应入口' : '。出口暂时对不上入口'}<InfoTip text="这时替换节点、换线路机会失败，不会改这台路由" /></Note>}
      {showDeployTl && <Section><SectionHeader title="一键部署" /><DeployTimeline r={r} /></Section>}
      {/* 总览：大号的出口可用数 + 一句话，不带框；下面是资源四项 */}
      {stale && <p className="rt-stale-tag">以下是上次读到的数据（<Seen at={detail?.collected_at ?? r.last_seen_at} />）</p>}
      {s && (
        <Section className={staleCls}>
          <div className="rt-hero">
            <div className="rt-hero-main"><b className="rt-hero-num num">{s.exits.alive}<small>/{s.exits.total}</small></b><span className="rt-hero-unit">个出口可用</span></div>
            <p className="rt-hero-sum">
              {s.clients} 台客户端在线 · OpenClash {s.openclash.conns.toLocaleString()} 条连接 · WAN ↓ {F.str(F.rate(s.wan.rx_bps))} ↑ {F.str(F.rate(s.wan.tx_bps))} · 已运行 {F.str(uptimeFmt(s.uptime_s))}
              {s.exits.drift > 0 && <> · <span className="rt-warn">{s.exits.drift} 个出口与入口不一致</span></>}
            </p>
            {issues.length > 0 && (
              <ul className="reason-list">
                {issues.map((text) => (
                  <li key={text}><SeverityMark severity="warning" />{text}
                    {text.includes('延迟测试不通') && <> <button type="button" className="btn btn-link btn-sm" style={{ height: 'auto', padding: 0 }} onClick={() => setTab('exits')}>查看出口</button></>}
                  </li>
                ))}
              </ul>
            )}
          </div>
          <div className="rt-hero-meters"><RouterMeters s={s} /></div>
        </Section>
      )}
      <Section>
        <SectionHeader title="基本信息" />
        <LabelValueGrid>
          <LabelValue label="团队">{r.team?.name ?? <span className="t-muted">不分团队</span>}</LabelValue>
          {s?.wan.ip && <LabelValue label="WAN 地址"><span className="row" style={{ gap: 4, flexWrap: 'nowrap', minWidth: 0 }}><Trunc className="num">{s.wan.ip}</Trunc><CopyButton text={s.wan.ip} label="WAN 地址" size={13} show /></span></LabelValue>}
          <LabelValue label="隧道端口"><><span className="num">{r.tunnel_port}</span> <InfoTip text="中控分给这台路由的隧道端口，中控通过它连到路由" /></></LabelValue>
          <LabelValue label="创建时间">{F.dtFull(r.created_at)}</LabelValue>
          <LabelValue label={r.state === 'offline' ? '最后在线' : '最近更新'}>{r.last_seen_at ? <>{F.dt(r.last_seen_at)} <span className="t-sub">· <Seen at={r.last_seen_at} /></span></> : '还没连上过'}</LabelValue>
          {r.note && <LabelValue label="备注" className="span-2" wrap>{r.note}</LabelValue>}
        </LabelValueGrid>
      </Section>
      {sys && (
        <Section className={staleCls}>
          <SectionHeader title="系统" />
          <LabelValueGrid>
            <LabelValue label="系统">{sys.os}</LabelValue>
            <LabelValue label="内核"><Trunc className="mono">{sys.kernel}</Trunc></LabelValue>
            <LabelValue label="CPU" wrap>{sys.cpu_model} · {sys.cores} 核</LabelValue>
            {/* 同机器主机页签的负载：百分比一行，三个平均负载放第二行 */}
            <LabelValue label="负载" wrap><><Num f={F.pct((sys.load[0] / sys.cores) * 100)} /> <InfoTip text="1、5、15 分钟平均负载。百分比是 1 分钟负载除以核数，超过 100% 说明在排队" /><div className="t-sub">{sys.load.map((x) => x.toFixed(2)).join(' / ')}</div></></LabelValue>
            <LabelValue label="进程">{`${sys.procs} 个`}</LabelValue>
            <LabelValue label="运行时长"><Num f={uptimeFmt(s?.uptime_s)} /></LabelValue>
            <LabelValue label="温度"><span className="rt-temp" data-tone={tempTone(s?.temp_c)}>{s?.temp_c != null ? `${s.temp_c.toFixed(0)} °C` : '没有传感器'}</span></LabelValue>
            {/* 差得多（超过 5 秒）时标黄并说明后果，不另起一条提示 */}
            <LabelValue label="时钟">{sys.clock_skew_s != null ? <><span className={skewed ? 'rt-warn' : undefined}>{Math.round(sys.clock_skew_s) === 0 ? '和中控一致' : `比中控${skewWord(sys.clock_skew_s)}`}</span>{skewed && <> <InfoTip text="和中控差太多，加密连接会失败，出口可能连不上" /></>}</> : null}</LabelValue>
          </LabelValueGrid>
        </Section>
      )}
      {detail && detail.interfaces.length > 0 && (
        <Section className={staleCls}>
          <SectionHeader title={<>网络接口<InfoTip text="速率是最近一小段时间的平均，累计从开机起算" /></>} />
          <TableWrap>
            <table className="dt" style={{ minWidth: 520 }}>
              <colgroup><col style={{ width: 170 }} /><col style={{ width: 130 }} /><col /><col /></colgroup>
              <thead><tr><th>接口</th><th>地址</th><th className="r">速率</th><th className="r">累计</th></tr></thead>
              <tbody>
                {detail.interfaces.map((i) => (
                  <tr key={i.name}>
                    <td><span className="cell-main strong">{i.name}{!i.up && <> <span className="rt-alive" data-tone="bad"><XCircle />断开</span></>}</span><span className="sub">{i.device} · {i.proto}{i.uptime_s != null && ` · 连上 ${F.str(uptimeFmt(i.uptime_s))}`}</span></td>
                    <td className="num">{i.ip ?? <span className="t-muted">没有地址</span>}</td>
                    <td className="r"><span className="num">↓ <Num f={F.rate(i.rx_bps)} /></span><span className="sub">↑ <Num f={F.rate(i.tx_bps)} /></span></td>
                    <td className="r"><span className="num">↓ {F.str(F.bytes(i.rx_bytes))}</span><span className="sub">↑ {F.str(F.bytes(i.tx_bytes))}</span></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </TableWrap>
        </Section>
      )}
      {oc && s && (
        <Section className={staleCls}>
          <SectionHeader title="OpenClash" />
          <LabelValueGrid>
            <LabelValue label="状态">{!oc.enabled ? <Pill>未启用</Pill> : oc.running ? <Pill tone="ok">运行中</Pill> : <Pill tone="bad" tip="已启用但没在运行，客户端暂时上不了外网"><CircleX size={12} />没在运行</Pill>}</LabelValue>
            <LabelValue label="核心版本"><Trunc className="mono">{oc.version ?? '—'}</Trunc></LabelValue>
            <LabelValue label="模式">{oc.mode}</LabelValue>
            <LabelValue label="核心内存"><Num f={F.mem(sys?.clash_rss)} /></LabelValue>
            <LabelValue label="代理速率" wrap><>↓ <Num f={F.rate(s.openclash.down_bps)} /> ↑ <Num f={F.rate(s.openclash.up_bps)} /> <InfoTip text="经 OpenClash 的流量，↓ 是到客户端的方向" /></></LabelValue>
            <LabelValue label="连接">{`${s.openclash.conns.toLocaleString()} 条`}</LabelValue>
            <LabelValue label="配置修改于">{F.dtFull(oc.config_mtime)}</LabelValue>
            <LabelValue label="配置文件" className="span-2"><Trunc className="mono">{oc.config_path ?? '—'}</Trunc></LabelValue>
            {oc.delay_error && <LabelValue label="延迟测试" className="span-2" wrap><span className="rt-warn">上次没测成：{oc.delay_error}</span> <InfoTip text="没测成时沿用上次的延迟，不算不通" /></LabelValue>}
          </LabelValueGrid>
        </Section>
      )}
      {detail === null && r.state === 'pending' && !d && <div className="hint">接入后才有数据</div>}
    </>
  );
}

// ---- 出口 ----
const SYNC: Record<RouterExit['sync'], { tone: 'ok' | 'warn' | 'neutral'; label: string; tip: string }> = {
  ok: { tone: 'ok', label: '一致', tip: '凭据和入口一致' },
  drift: { tone: 'warn', label: '不一致', tip: '地址和端口对得上入口，但凭据变了。入口换过凭据或线路机，这个出口可能连不上' },
  unmanaged: { tone: 'neutral', label: '不受管', tip: '没对上任何入口，是路由上自己加的，中控不管' },
};
/** 不一致的说明：对上的入口已停用时直接写这个（凭据没变，是线路机上没有这个入站了） */
const syncTip = (e: RouterExit) => (e.sync === 'drift' && e.entry?.lifecycle === 'disabled' ? '对应的入口已停用，这个出口连不上' : SYNC[e.sync].tip);
/** 只测不通 1 次的不算不可用（同告警，连续两次才算）：标黄 */
const exitTone = (e: RouterExit) => (!exitUsable(e) ? 'bad' : e.alive === false || e.sync === 'drift' ? 'warn' : e.sync === 'unmanaged' ? 'neutral' : 'ok');
const exitTip = (e: RouterExit) => [`${e.name}（${e.type}）→ ${e.server}:${e.port}`, e.alive === false ? `连续 ${e.fail_streak} 次延迟测试不通` : e.delay_ms != null ? `延迟 ${e.delay_ms} ms` : '还没测过延迟', e.sync !== 'ok' && syncTip(e)].filter(Boolean).join('\n');
/** 入口名「团队1-02 · 实验线路机:20000」拆成节点与线路机:端口两行 */
const splitEntry = (name: string) => { const i = name.indexOf(' · '); return i < 0 ? [name, ''] : [name.slice(0, i), name.slice(i + 3)]; };
const BEAT_RANGE = '6h';

function ExitsTab({ id, d }: { id: string; d: RouterDetail }) {
  const { openMachine } = useRouterDrawer();
  const m = useExitBeats(id);
  const alive = d.exits.filter(exitUsable).length;
  const managed = d.exits.filter((e) => e.entry).length;
  const drift = d.exits.filter((e) => e.sync === 'drift').length;
  return (
    <>
      <p className="rt-lead">{d.exits.length} 个出口 · <b>{alive}</b> 个可用 · {managed} 个对应入口{drift > 0 && <> · <span className="rt-warn">{drift} 个不一致</span></>}<InfoTip text="定期测一次延迟。连续两轮不通才算不可用，只不通一次不告警" /></p>
      <Section>
        <SectionHeader title="出口 → 节点" />
        <Topology exits={d.exits} onOpen={openMachine} />
      </Section>
      <Section>
        <SectionHeader title="明细" meta={`${d.exits.length} 个`} />
        <div className="table-wrap">
          <table className="dt rt-exit-tbl" style={{ minWidth: 708 }}>
            <colgroup><col style={{ width: 90 }} /><col style={{ width: 150 }} /><col style={{ width: 74 }} /><col style={{ width: 84 }} /><col style={{ width: 90 }} /><col style={{ width: 48 }} /><col style={{ width: 104 }} /><col style={{ width: 68 }} /></colgroup>
            <thead><tr><th>出口</th><th>对应入口</th><th>参数<InfoTip text={(['drift', 'unmanaged'] as const).map((k) => `${SYNC[k].label}：${SYNC[k].tip}`).join('\n')} /></th><th className="r">延迟</th><th>近 6 小时</th><th className="r">连接</th><th className="r">↓ / ↑ 速率</th><th className="r">客户端</th></tr></thead>
            <tbody>{d.exits.map((e) => <ExitRow key={e.name} e={e} beats={m.get(e.name)} onOpen={openMachine} />)}</tbody>
          </table>
        </div>
      </Section>
    </>
  );
}

/** 各出口近 6 小时的延迟测试（行内迷你格子）：name → 12 格（每格 30 分钟） */
function useExitBeats(id: string) {
  const q = useRouterExitMetrics(id, BEAT_RANGE);
  return React.useMemo(() => new Map((q.data?.exits ?? []).map((x) => [x.name, exitBeats(x.points, BEAT_RANGE, 12, Date.now())])), [q.data]);
}

/** 一行。memo：routers 事件重取详情后，没变的出口结构共享、引用不变（延迟格子另由曲线轮询换） */
const ExitRow = React.memo(function ExitRow({ e, beats, onOpen }: { e: RouterExit; beats: ReturnType<typeof exitBeats> | undefined; onOpen: (id: string) => void }) {
  const s = SYNC[e.sync];
  const [node, via] = e.entry ? splitEntry(e.entry.name) : ['', ''];
  return (
    <tr>
      <td><span className="cell-main strong" data-tip={`${e.name}（${e.type}）→ ${e.server}:${e.port}`}>{e.name}</span><span className="sub">{e.type}</span></td>
      <td>{e.entry ? <><button type="button" className="btn btn-link" style={{ height: 'auto', padding: 0, maxWidth: '100%' }} data-tip="打开这个节点的详情" onClick={() => onOpen(e.entry!.node_id)}><span className="ellipsis">{node}</span></button>{via && <span className="sub ellipsis" data-tip={e.entry.name}>{via}</span>}</> : <span className="t-muted">—</span>}</td>
      {/* 一致的不标（一列「一致」只是噪音），只标不一致 / 不受管 */}
      <td>{e.sync !== 'ok' && <Pill tone={s.tone} tip={syncTip(e)}>{s.label}</Pill>}</td>
      <td className="r">
        {e.alive === false && exitUsable(e) ? <span className="rt-alive rt-warn" data-tip="只不通 1 次，还算可用；连续两次不通才告警">不通 1 次</span>
          : e.alive === false ? <span className="rt-alive" data-tone="bad"><XCircle />不通</span>
          : e.alive ? <Num f={F.ms(e.delay_ms)} /> : <span className="t-muted" data-tip="还没测过">—</span>}
        {e.delay_at && <span className="sub">{F.ago(e.delay_at)}</span>}
      </td>
      <td>{beats ? <MiniBeat cells={beats.cells} titles={beats.titles} /> : <span className="t-muted">—</span>}</td>
      <td className="r num">{e.conns}</td>
      <td className="r"><span className="num">↓ {F.str(F.rate(e.down_bps))}</span><span className="sub">↑ {F.str(F.rate(e.up_bps))}</span></td>
      <td className="r num">{e.clients}</td>
    </tr>
  );
});

/** 出口 → 节点：按线路机分组，线路机是组标题，一个出口一行「出口 ── 节点」，一一对应；没对上入口的单列一组「不受管」。
 *  不通的出口红、只不通 1 次或参数不一致的黄，连线跟着变成虚线。线路机、节点名取机器列表缓存（没取过时用入口名） */
function Topology({ exits, onOpen }: { exits: RouterExit[]; onOpen: (id: string) => void }) {
  const machines = useMachineBriefs().data;
  const name = React.useMemo(() => new Map((machines ?? []).map((m) => [m.id, m.name])), [machines]);
  const groups = React.useMemo(() => {
    const by = new Map<string, RouterExit[]>();
    for (const e of exits) { const k = e.entry?.relay_id ?? ''; by.set(k, [...(by.get(k) ?? []), e]); }
    return [...by].sort(([a], [b]) => (a === '' ? 1 : b === '' ? -1 : 0));
  }, [exits]);
  return (
    <div className="table-wrap">
      <div className="rt-topo">
        {groups.map(([relay, list]) => (
          <div key={relay || 'none'} className="rt-topo-group">
            <div className="rt-topo-title">
              {relay ? <><button type="button" className="btn btn-link" style={{ height: 'auto', padding: 0 }} data-tip="打开这台线路机的详情" onClick={() => onOpen(relay)}>{name.get(relay) ?? '线路机'}</button><small>{list.length} 个出口</small></>
                : <>不受管（路由自配）<small>{list.length} 个出口</small></>}
            </div>
            {list.map((e) => (
              <React.Fragment key={e.name}>
                <span className="rt-topo-pill rt-topo-exit" data-tone={exitTone(e)} data-tip={exitTip(e)}>
                  {exitTone(e) === 'bad' ? <XCircle /> : exitTone(e) === 'warn' ? <TriangleAlert /> : null}<span className="ellipsis">{e.name}</span>
                  <small className="num">{!exitUsable(e) ? '不通' : e.alive === false ? '刚才不通 1 次' : e.delay_ms != null ? `${e.delay_ms} ms` : ''}</small>
                </span>
                <i className="rt-topo-link" data-tone={exitTone(e)} />
                {relay ? (
                  <button type="button" className="rt-topo-pill rt-topo-node" data-tip={e.entry!.name} onClick={() => onOpen(e.entry!.node_id)}>
                    <span className="ellipsis">{name.get(e.entry!.node_id) ?? e.entry!.name}</span>
                  </button>
                ) : <span className="rt-topo-raw mono">{e.server}:{e.port}</span>}
              </React.Fragment>
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}

// ---- 客户端 ----
const byClient = (a: RouterClient, b: RouterClient) => Number(b.online) - Number(a.online) || b.conns - a.conns;
function ClientsTab({ d }: { d: RouterDetail }) {
  const list = React.useMemo(() => [...d.clients].sort(byClient), [d.clients]);
  const online = list.filter((c) => c.online).length;
  const offline = online < list.length;
  return (
    <>
      <p className="rt-lead"><b>{online}</b> 台在线 · 共 {list.length} 台<InfoTip text="包括 DHCP 分配的设备，以及经过 OpenClash 的设备（旁路由下的设备只能认出 IP）" /></p>
      <div className="table-wrap">
        <table className="dt" style={{ minWidth: offline ? 700 : 630 }}>
          <colgroup><col style={{ width: 230 }} />{offline && <col style={{ width: 70 }} />}<col style={{ width: 70 }} /><col style={{ width: 104 }} /><col /></colgroup>
          <thead><tr><th>设备</th>{offline && <th>状态</th>}<th className="r">连接</th><th className="r">↓ / ↑ 速率</th><th>经过的出口（连接数）</th></tr></thead>
          <tbody>{list.map((c) => <ClientRow key={c.ip} c={c} offline={offline} />)}</tbody>
        </table>
      </div>
    </>
  );
}
const ClientRow = React.memo(function ClientRow({ c, offline }: { c: RouterClient; offline: boolean }) {
  const via = [...c.exits].sort((a, b) => b.conns - a.conns).map((x) => `${x.name} ${x.conns} 条`).join(' · ');
  // 认不出主机名的设备拿 IP 当名字（旁路由没有租约），正文色，不灰
  return (
    <tr className={c.online ? undefined : 'is-quiet'}>
      <td><span className="cell-main strong">{c.hostname ?? c.ip}</span>{(c.hostname || c.mac) && <span className="sub mono">{c.hostname && c.ip}{c.mac && `${c.hostname ? ' · ' : ''}${c.mac}`}</span>}</td>
      {offline && <td>{!c.online && <Pill>离线</Pill>}</td>}
      <td className="r num">{c.conns}</td>
      <td className="r">{c.online ? <><span className="num">↓ {F.str(F.rate(c.down_bps))}</span><span className="sub">↑ {F.str(F.rate(c.up_bps))}</span></> : <span className="t-muted">—</span>}</td>
      <td>{via ? <Trunc className="cell-main">{via}</Trunc> : <span className="t-muted">没有经过 OpenClash 的连接</span>}</td>
    </tr>
  );
});
