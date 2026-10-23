// 软路由详情抽屉：概览 / 出口 / 客户端 / 曲线。开关由 URL 的 ?r=（页签 rtab）控制，与机器抽屉（?m=）分开；出口对应的入口点开时换成机器抽屉，后退回来。
// 容器按信息的性质选（用户：不要每块都是格子）：总览数字不带框、设置类信息用分组列表（行间细线）、出口关系画成「出口 → 线路机 → 节点」、部署进度是时间线。
import * as React from 'react';
import { CircleX, MoreHorizontal, PencilLine, RefreshCw, Router as RouterIcon, SquareTerminal, Terminal, Trash2, TriangleAlert, XCircle } from 'lucide-react';
import { Sheet, SheetContent, SheetDescription, SheetTitle } from '@/components/ui/dialog';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger, Tabs, TabsList, TabsTrigger, Trunc } from '@/components/ui/overlays';
import { IconButton } from '@/components/ui/button';
import { Field, InfoTip, Input, Textarea } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Skeleton } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ErrorBoundary } from '@/components/app/ErrorBoundary';
import { ConfirmDialog, FormDialog, TypedConfirmDialog } from '@/components/app/dialogs';
import { ErrorNote, Note, Num, Pill, Section, SectionHeader, SeverityMark } from '@/components/app/primitives';
import { Tag } from '@/components/app/viz';
import { DeployTimeline, MiniBeat, RouterMeters, RouterStatePill, Seen, useShowBootstrap } from '@/components/app/RouterCard';
import { useMachineBriefs, useTeams } from '@/lib/queries';
import { deadExitNames, exitBeats, exitUsable, ROUTER_TABS, routerIssues, showDeploy, showRemoved, tempTone, uptimeFmt, useDeleteRouter, usePatchRouter, useRouter, useRouterDrawer, useRouterExitMetrics, useRouters, type RouterClient, type RouterDetail, type RouterExit, type RouterTab, type RouterView } from '@/lib/routers';
import { tagColor } from '@/lib/derive';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { openTerminal } from '@/lib/terminal';
import { useTitleName } from '@/lib/title';
import * as F from '@/lib/format';

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
  const bodyRef = React.useRef<HTMLDivElement>(null);
  React.useLayoutEffect(() => { if (bodyRef.current) bodyRef.current.scrollTop = 0; }, [r?.id, tab]);
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
              <Head r={r} detail={detail} onClosed={close} />
              <Tabs value={tab} onValueChange={(v) => setTab(v as RouterTab)} className="drawer-tabs">
                <TabsList>{ROUTER_TABS.map((t) => <TabsTrigger key={t.value} value={t.value} noPanel>{t.label}</TabsTrigger>)}</TabsList>
              </Tabs>
              <div className="drawer-body" ref={bodyRef}>
                {q.isError && !q.data && <ErrorNote error={q.error} onRetry={() => void q.refetch()}>详情加载失败：</ErrorNote>}
                <div key={tab} className="tab-pane">
                  <React.Suspense fallback={<Skeleton h={180} className="is-float" />}>
                    {tab === 'overview' && <OverviewTab r={r} detail={detail} />}
                    {tab === 'exits' && <Needs r={r} detail={detail}>{(d) => <ExitsTab id={r.id} d={d} />}</Needs>}
                    {tab === 'clients' && <Needs r={r} detail={detail}>{(d) => <ClientsTab d={d} />}</Needs>}
                    {tab === 'charts' && <ChartsTab r={r} />}
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
  if (!detail) return <div className="hint">{r.state === 'pending' ? '接入后才有数据' : '还没采集成功过'}</div>;
  return <>{children(detail)}</>;
}

/** 头部：类型标 + 名称 + 状态 + 团队，副标题 WAN 与系统；右边「⋯」放管理操作 */
function Head({ r, detail, onClosed }: { r: RouterView; detail: RouterDetail | null | undefined; onClosed: () => void }) {
  const { open, tab } = useRouterDrawer();
  const [edit, setEdit] = React.useState(false);
  const [askRegen, setAskRegen] = React.useState(false);
  const [askDel, setAskDel] = React.useState(false);
  const boot = useShowBootstrap();
  const del = useDeleteRouter();
  const sub = [r.summary?.wan.ip && `WAN ${r.summary.wan.ip}`, detail?.system.os, `隧道端口 ${r.tunnel_port}`].filter(Boolean).join(' · ');
  return (
    <header className="drawer-head" style={{ paddingRight: 56 }}>
      <span className="nc-kind" role="img" aria-label="软路由"><RouterIcon /></span>
      <div className="m-head" style={{ flex: 1 }}>
        <div className="m-head-row"><SheetTitle className="drawer-title ellipsis">{r.name}</SheetTitle><RouterStatePill r={r} />{r.team && <Tag c={tagColor(r.team.name)} tip="团队">{r.team.name}</Tag>}</div>
        <SheetDescription className="drawer-sub ellipsis">{sub}</SheetDescription>
      </div>
      <div className="drawer-nav">
        <DropdownMenu>
          <DropdownMenuTrigger asChild><IconButton label="管理" tip={false} data-tip="管理：改名、终端、重新接入、删除"><MoreHorizontal size={16} /></IconButton></DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem onSelect={() => setEdit(true)}><PencilLine />改名 / 团队 / 备注…</DropdownMenuItem>
            <DropdownMenuItem onSelect={() => showDeploy({ id: r.id, name: r.name })}><RefreshCw />重新部署（填账号密码）…</DropdownMenuItem>
            {/* 还没连上过的没有隧道可断，直接生成；连上过的先确认（旧隧道立刻失效） */}
            <DropdownMenuItem onSelect={() => (r.last_seen_at ? setAskRegen(true) : void boot.run(r))}><Terminal />{r.state === 'pending' ? '查看接入命令' : '重新生成接入命令…'}</DropdownMenuItem>
            {/* 打开终端先收起抽屉：终端窗口的层级在抽屉遮罩之下，开着抽屉会被盖住 */}
            {r.state !== 'pending' && <DropdownMenuItem onSelect={() => { onClosed(); openTerminal({ kind: 'router', id: r.id, name: r.name }, () => open(r.id, tab)); }}><SquareTerminal />打开终端</DropdownMenuItem>}
            <DropdownMenuSeparator />
            <DropdownMenuItem danger onSelect={() => setAskDel(true)}><Trash2 />删除…</DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
      <EditDialog open={edit} onOpenChange={setEdit} r={r} />
      <ConfirmDialog open={askRegen} onOpenChange={setAskRegen} title={`重新生成「${r.name}」的接入命令？`} confirmLabel="重新生成" danger
        description={<>隧道会立即断开<InfoTip text="会换一把隧道密钥：路由上现在的 fleet-tunnel 立刻连不上中控，直到在路由上执行新命令。不影响 OpenClash 和客户端上网，只是这段时间看不到它的状态" /></>}
        onConfirm={() => boot.run(r)} />
      <TypedConfirmDialog open={askDel} onOpenChange={setAskDel} title={`删除软路由「${r.name}」？`} confirmLabel="删除" danger expected={r.name} prompt="请输入名称"
        description={<>只删采集历史<InfoTip text="撤销它的隧道公钥、删掉采集的历史；OpenClash、原来的 router-tunnel 照常。删除后会给出在路由上停掉 fleet-tunnel 的命令" /></>}
        onConfirm={async () => {
          await del.mutateAsync(r.id); onClosed();
          // 从没连上过、也没一键部署过的：路由上没装 fleet-tunnel，不用给清理命令
          if (r.last_seen_at || r.deploy) showRemoved(r.name); else toast.ok(`已删除「${r.name}」`);
        }} />
    </header>
  );
}

/** 改名 / 团队 / 备注：只提交改了的项。每次打开重新挂载，从当前值开始 */
const EditDialog = resetOnOpen(function EditDialog({ open, onOpenChange, r }: { open: boolean; onOpenChange: (o: boolean) => void; r: RouterView }) {
  const patch = usePatchRouter();
  const teams = useTeams().data;
  const opts = React.useMemo(() => (teams ?? []).map((t) => ({ value: t.id, label: t.name })), [teams]);
  const [name, setName] = React.useState(r.name); const [team, setTeam] = React.useState(r.team?.id ?? ''); const [note, setNote] = React.useState(r.note);
  const body = { ...(name.trim() !== r.name ? { name: name.trim() } : {}), ...(team !== (r.team?.id ?? '') ? { team_id: team || null } : {}), ...(note.trim() !== r.note ? { note: note.trim() } : {}) };
  const dirty = Object.keys(body).length > 0;
  return (
    <FormDialog open={open} onOpenChange={onOpenChange} title={`编辑「${r.name}」`} submitLabel="保存" busy={patch.isPending} valid={dirty && !!name.trim()} dirty={dirty}
      onSubmit={() => patch.mutate({ id: r.id, ...body }, { onSuccess: () => { toast.ok('已保存'); onOpenChange(false); }, onError: toast.err })}>
      <Field label="名称"><Input value={name} onChange={(e) => setName(e.target.value)} autoFocus autoComplete="off" /></Field>
      <Field label="团队"><Select value={team} onChange={setTeam} options={opts} allowEmpty="不分团队" placeholder="不分团队" /></Field>
      <Field label="备注"><Textarea rows={3} value={note} onChange={(e) => setNote(e.target.value)} /></Field>
    </FormDialog>
  );
});

// ---- 分组列表（同 iOS 设置）：一组一个底，行间细线，标签在左、值在右 ----
const Group = ({ title, info, foot, children }: { title?: string; info?: string; foot?: React.ReactNode; children: React.ReactNode }) => (
  <section className="rt-gsec">{title && <h3 className="rt-group-title">{title}{info && <InfoTip text={info} />}</h3>}<ul className="rt-group">{children}</ul>{foot && <div className="rt-group-foot hint">{foot}</div>}</section>
);
const Row = ({ label, children, tip }: { label: React.ReactNode; children: React.ReactNode; tip?: string }) => (
  <li className="rt-row"><span className="rt-row-label" data-tip={tip}>{label}</span><span className="rt-row-value">{children ?? '—'}</span></li>
);

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
  return (
    <div className="rt-groups">
      {r.state === 'offline' && <Note tone="bad">连不上路由（最后在线 <Seen at={r.last_seen_at} />）{r.error && <>：<span className="mono">{r.error}</span></>}<InfoTip text="隧道断了时 OpenClash 与客户端上网照常，只是这里看不到它的状态" /></Note>}
      {r.state === 'online' && r.error && <Note tone="warn">连得上，但采集出错：<span className="mono">{r.error}</span>{detail?.collected_at && <>（下面是 <Seen at={detail.collected_at} /> 的数据）</>}<InfoTip text="隧道是通的，只是采集脚本出错、输出读不了或超时；不算离线" /></Note>}
      {oc?.config_error && <Note tone="warn">OpenClash 的配置读不了：<span className="mono">{oc.config_error}</span>{oc.config_stale ? '，出口按上次读到的配置对应面板入口' : '，出口对不上面板入口'}<InfoTip text="替换节点、换线路机时还读不了，作业会失败、不改这台路由" /></Note>}
      {showDeployTl && <Group title="一键部署"><li className="rt-row rt-row-tl"><DeployTimeline r={r} /></li></Group>}
      {/* 总览：大号的出口可用数 + 一句话，不带框 */}
      {s && (
        <div className="rt-hero">
          <div className="rt-hero-main"><b className="rt-hero-num num">{s.exits.alive}<small>/{s.exits.total}</small></b><span className="rt-hero-unit">个出口可用</span></div>
          <p className="rt-hero-sum">
            {s.clients} 台客户端在线 · OpenClash {s.openclash.conns.toLocaleString()} 条连接 · WAN ↓ {F.str(F.rate(s.wan.rx_bps))} ↑ {F.str(F.rate(s.wan.tx_bps))} · 已运行 {F.str(uptimeFmt(s.uptime_s))}
            {s.exits.drift > 0 && <> · <span className="rt-warn">{s.exits.drift} 个出口参数与面板入口不一致</span></>}
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
      )}
      {s && <RouterMeters s={s} />}
      <Group title="基本信息">
        <Row label="团队">{r.team?.name ?? <span className="t-muted">不分团队</span>}</Row>
        <Row label="隧道端口" tip="中控上分给这台的反向隧道端口（路由连上中控后，中控经它登录路由采集）"><span className="num">{r.tunnel_port}</span></Row>
        <Row label="创建时间">{F.dtFull(r.created_at)}</Row>
        <Row label={r.state === 'offline' ? '最后在线' : '最近采集'} tip="每 30 秒采集一次">{r.last_seen_at ? <>{F.dt(r.last_seen_at)} <span className="t-sub">· <Seen at={r.last_seen_at} /></span></> : '还没有'}</Row>
        {r.note && <Row label="备注"><span className="rt-row-wrap">{r.note}</span></Row>}
      </Group>
      {sys && (
        <Group title="系统" foot={sys.clock_skew_s != null && Math.abs(sys.clock_skew_s) > 5 ? <span className="rt-warn">路由时钟{skewWord(sys.clock_skew_s)}，出口可能连不上<InfoTip text="路由的时钟和中控差太多：TLS 与 reality 出口对时间敏感，会连不上" /></span> : undefined}>
          <Row label="系统">{sys.os}</Row>
          <Row label="内核"><span className="mono">{sys.kernel}</span></Row>
          <Row label="CPU">{sys.cpu_model} · {sys.cores} 核</Row>
          <Row label="负载" tip="1 / 5 / 15 分钟平均负载；百分比 = 1 分钟负载 ÷ 核数（同机器卡片），超过 100% 表示排队">
            <Num f={F.pct((sys.load[0] / sys.cores) * 100)} /> <span className="t-sub">· {sys.load.map((x) => x.toFixed(2)).join(' / ')}</span>
          </Row>
          <Row label="进程">{sys.procs} 个</Row>
          <Row label="运行时长"><Num f={uptimeFmt(s?.uptime_s)} /></Row>
          <Row label="温度"><span className="rt-temp" data-tone={tempTone(s?.temp_c)}>{s?.temp_c != null ? `${s.temp_c.toFixed(0)} °C` : '没有传感器'}</span></Row>
          <Row label="时钟偏差" tip="和中控比：中控的时间减路由的时间，正数是路由慢、负数是路由快">{sys.clock_skew_s != null ? skewWord(sys.clock_skew_s) : '—'}</Row>
        </Group>
      )}
      {detail && (
        <Group title="网络接口" info="速率是两次采集（30 秒）之间的平均；累计从路由开机算">
          {detail.interfaces.map((i) => (
            <li key={i.name} className="rt-row rt-iface">
              <span className="rt-row-label"><b>{i.name}</b><span className="t-sub"> · {i.device} · {i.proto}</span>{!i.up && <span className="rt-alive" data-tone="bad"><XCircle />断开</span>}</span>
              <span className="rt-row-value">
                <span className="num">{i.ip ?? <span className="t-muted">没有地址</span>}</span>
                <span className="rt-iface-sub">↓ <Num f={F.rate(i.rx_bps)} /> ↑ <Num f={F.rate(i.tx_bps)} /> · 累计 ↓ {F.str(F.bytes(i.rx_bytes))} ↑ {F.str(F.bytes(i.tx_bytes))}{i.uptime_s != null && <> · 连上 {F.str(uptimeFmt(i.uptime_s))}</>}</span>
              </span>
            </li>
          ))}
        </Group>
      )}
      {oc && s && (
        <Group title="OpenClash">
          <Row label="状态">{!oc.enabled ? <Pill>未启用</Pill> : oc.running ? <Pill tone="ok">运行中</Pill> : <Pill tone="bad" tip="已启用但核心进程（clash）没在运行：客户端此时上不了外网"><CircleX size={12} />核心没在运行</Pill>}</Row>
          <Row label="核心版本"><span className="mono">{oc.version ?? '—'}</span></Row>
          <Row label="模式">{oc.mode}</Row>
          <Row label="配置文件"><Trunc className="mono">{oc.config_path ?? '—'}</Trunc></Row>
          <Row label="配置修改于">{F.dtFull(oc.config_mtime)}</Row>
          {oc.delay_error && <Row label="延迟测试" tip="每 5 分钟测一次；没测成时出口的延迟沿用上一次的结果，不算不通"><span className="rt-warn">最近一次没测成：{oc.delay_error}</span></Row>}
          <Row label="核心内存" tip="clash 进程的常驻内存（VmRSS）"><Num f={F.mem(sys?.clash_rss)} /></Row>
          <Row label="代理速率" tip="经 OpenClash 的流量；↓ 是到客户端的方向">↓ <Num f={F.rate(s.openclash.down_bps)} /> ↑ <Num f={F.rate(s.openclash.up_bps)} /></Row>
          <Row label="连接">{s.openclash.conns.toLocaleString()} 条</Row>
        </Group>
      )}
      {detail === null && r.state === 'pending' && !d && <div className="hint">接入后才有数据</div>}
    </div>
  );
}

// ---- 出口 ----
const SYNC: Record<RouterExit['sync'], { tone: 'ok' | 'warn' | 'neutral'; label: string; tip: string }> = {
  ok: { tone: 'ok', label: '一致', tip: '参数（uuid、reality 公钥等）与面板入口现在的一致' },
  drift: { tone: 'warn', label: '不一致', tip: 'server:port 对上了面板入口，但 uuid / reality 参数和入口现在的不一致：入口轮换过凭据或换过线路机，路由上的这个出口可能已连不上' },
  unmanaged: { tone: 'neutral', label: '不受管', tip: '没对上任何面板入口（server:port 不是线路机的入口端口）：是路由上自己加的，中控不管它' },
};
/** 不一致的说明：对上的入口已停用时直接写这个（凭据没变，是线路机上没有这个入站了） */
const syncTip = (e: RouterExit) => (e.sync === 'drift' && e.entry?.lifecycle === 'disabled' ? '对应入口已停用：线路机上没有这个入站了，路由上的这个出口连不上' : SYNC[e.sync].tip);
/** 只测不通 1 次的不算不可用（同告警，连续两次才算）：标黄 */
const exitTone = (e: RouterExit) => (!exitUsable(e) ? 'bad' : e.alive === false || e.sync === 'drift' ? 'warn' : e.sync === 'unmanaged' ? 'neutral' : 'ok');
const exitTip = (e: RouterExit) => [`${e.name}（${e.type}）→ ${e.server}:${e.port}`, e.alive === false ? `连续 ${e.fail_streak} 次延迟测试不通` : e.delay_ms != null ? `延迟 ${e.delay_ms} ms` : '还没测过延迟', e.sync !== 'ok' && syncTip(e)].filter(Boolean).join('\n');
/** 面板入口名「团队1-02 · 实验线路机:20000」拆成节点与线路机:端口两行 */
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
      <p className="rt-lead">{d.exits.length} 个出口 · <b>{alive}</b> 个可用 · {managed} 个对应面板入口{drift > 0 && <> · <span className="rt-warn">{drift} 个参数不一致</span></>}<InfoTip text="延迟每 5 分钟测一次（经出口访问 generate_204，每批 6 个出口；没通的当场单独再测一次，再不通才算这一轮不通）；连续两轮测不通才算不可用" /></p>
      <Section>
        <SectionHeader title="出口 → 节点" />
        <Topology exits={d.exits} onOpen={openMachine} />
      </Section>
      <Section>
        <SectionHeader title="明细" meta={`${d.exits.length} 个`} />
        <div className="table-wrap">
          <table className="dt rt-exit-tbl" style={{ minWidth: 708 }}>
            <colgroup><col style={{ width: 90 }} /><col style={{ width: 150 }} /><col style={{ width: 74 }} /><col style={{ width: 84 }} /><col style={{ width: 90 }} /><col style={{ width: 48 }} /><col style={{ width: 104 }} /><col style={{ width: 68 }} /></colgroup>
            <thead><tr><th>出口</th><th>对应面板入口</th><th>参数<InfoTip text={(['drift', 'unmanaged'] as const).map((k) => `${SYNC[k].label}：${SYNC[k].tip}`).join('\n')} /></th><th className="r">延迟</th><th>近 6 小时</th><th className="r">连接</th><th className="r">↓ / ↑ 速率</th><th className="r">客户端</th></tr></thead>
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

/** 出口 → 节点：按线路机分组，线路机是组标题，一个出口一行「出口 ── 节点」，一一对应；没对上面板入口的单列一组「不受管」。
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
      <p className="rt-lead"><b>{online}</b> 台在线 · 共 {list.length} 台<InfoTip text="来自 DHCP 租约、邻居表与经过 OpenClash 的来源 IP（旁路由没有租约，只认得 IP）" /></p>
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
