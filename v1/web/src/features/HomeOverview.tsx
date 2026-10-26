// 首页上半部分：大圆角浅灰分区（HomeSection）+「监控总览」——问候与一句话状态、8 张 KPI 卡、集群状态面板。
// 集群状态：在线率大读数 + 格条、在线/离线/总计、状态胶囊；网卡速率（实时）+ 上行 / 下行两张小折线（全站速率环形缓冲：开场垫近 30 分钟历史，之后实时流喂，见 lib/live）。
// 网卡速率只在集群状态里出现（KPI 原先也有一张，与它重复），KPI 那一格换成流量额度
import * as React from 'react';
import { Link } from 'react-router-dom';
import { Activity, AlertTriangle, ArrowDown, ArrowUp, CalendarClock, ChevronRight, CircleDollarSign, CloudUpload, Database, LayoutDashboard, Router as RouterIcon, Server, TrendingUp, Waypoints } from 'lucide-react';
import type { Overview } from '@/lib/types';
import * as F from '@/lib/format';
import { useMe, useNextDue, useOfflineNames, useQuotaTight } from '@/lib/queries';
import { openDrawer } from '@/lib/drawer';
import { useRouters } from '@/lib/routers';
import { useFleetSeries } from '@/lib/live';
import { cn } from '@/lib/utils';
import { meta, usdTotalTip } from '@/lib/derive';
import { CountUp, DotCount, FromTag, Num, Stat } from '@/components/app/primitives';
import { AreaSpark, Cells } from '@/components/app/viz';
import { InfoTip } from '@/components/ui/form';

/** 分区：浅灰大圆角容器，左上小胶囊标签（图标 + 文字）、标题、一行说明 */
export function HomeSection({ icon, label, title, sub, children, className, headRef }: {
  icon: React.ReactNode; label: string; title: React.ReactNode; sub?: React.ReactNode; children: React.ReactNode; className?: string; headRef?: (el: HTMLElement | null) => void;
}) {
  return (
    <section className={cn('hsec', className)} aria-label={label}>
      <header className="hsec-head" ref={headRef}>
        <div className="hsec-top"><span className="hsec-badge">{icon}{label}</span></div>
        <div className="hsec-titles">{title}{sub && <p className="hsec-sub">{sub}</p>}</div>
      </header>
      {children}
    </section>
  );
}

type Counts = Overview['nodes'];
/** 机器页的故障筛选（status 取健康的 down，同机器页状态下拉）。离线（agent 不再上报）的机器健康都是故障（agent_offline 是严重原因），
 *  机器页没有单独的「离线」筛选，集群状态的离线也链到这里 */
const DOWN_LINK = '/nodes?status=down';
// 其余 KPI 卡点进机器页：节点含 VPS 与家宽代理（机器页的类型筛选一次只选一种），不带类型；排序参数同机器页表头的列 id
const NODES_LINK = '/nodes';
const RELAYS_LINK = '/relays';
const COST_LINK = '/nodes?sort=-cost';
const TRAFFIC_LINK = '/nodes?sort=-traffic';
/** 算正常的：只有故障算异常，降级（照常服务、只是有提醒）与维护都算正常；未知（还判断不了）两头都不算 */
const normal = (c: Counts) => c.total - c.down - c.unknown;
const SPARE_TIP = '闲置备用：备用团队里没有软路由出口指向的机器，等到期放生。不算在节点总数和故障里，也不开告警；月费不在每月费用合计里（另列备用小计）';
const NORMAL_INFO = '正常 = 总数 − 故障 − 未知。降级与维护的机器照常服务，都算正常';

/** 问候语下的一行状态：只说有几类事要处理、是哪几类（具体台数在下面的 KPI 卡里，这里不再重复一遍数字）；没有就是「全部正常」 */
function statusLine(o: Overview | undefined, down: number): string {
  if (!o) return '';
  if (o.settling) return '中控刚启动，各台状态还在汇总…';
  const todo = [
    down && '故障',
    o.failed && '上架失败',
    o.overdue && '已过期',
    o.due_soon && '即将到期',
  ].filter(Boolean);
  const unknown = o.nodes.unknown + o.relays.unknown;
  if (todo.length) return `有 ${todo.length} 件事要处理：${todo.join('、')}${unknown ? ` · ${unknown} 台状态未知` : ''}`;
  return unknown ? `${unknown} 台状态未知` : '全部正常';
}

/** 「正常」卡的说明行：算在正常里的（降级、维护）与另外的（故障、未知）分两组写，每组为 0 的项省略；加起来对得上大数字与总数 */
function HealthCaption({ c, to }: { c: Counts; to: string }) {
  return (
    <>
      {(c.degraded > 0 || c.maintenance > 0) && <span className="stat-grp">含{c.degraded > 0 && <DotCount tone="warn" n={c.degraded} label="降级" />}{c.maintenance > 0 && <DotCount tone="neutral" n={c.maintenance} label="维护" />}</span>}
      {(c.down > 0 || c.unknown > 0) && <span className="stat-grp">另{c.down > 0 && <DotCount tone="bad" n={c.down} label="故障" to={to} />}{c.unknown > 0 && <DotCount tone="neutral" n={c.unknown} label="未知" />}</span>}
    </>
  );
}
/** 「上架中 · 退役中」卡的说明行：上架、退役各几台，上架失败的也在这里（不计在左边的大数字里）；为 0 的省略 */
function ProvisionCaption({ o }: { o: Overview }) {
  if (!o.provisioning && !o.retiring && !o.failed) return <span>没有进行中的</span>;
  return (
    <span className="stat-grp">
      {o.provisioning > 0 && <DotCount tone="neutral" n={o.provisioning} label="上架" />}
      {o.retiring > 0 && <DotCount tone="neutral" n={o.retiring} label="退役" />}
      {o.failed > 0 && <span data-tip="上架失败的不计在左边的台数里，要处理（也算异常）"><DotCount tone="bad" n={o.failed} label="失败" /></span>}
    </span>
  );
}

/** memo：首页因机器列表推送、筛选、开关抽屉重渲染时，总览只在 overview 数据（metrics 推送单独更新）或额度吃紧的结果变了才重画 */
export const OverviewSection = React.memo(function OverviewSection({ o, loading }: { o: Overview | undefined; loading: boolean }) {
  const me = useMe().data;
  const qt = useQuotaTight().data; const top = qt?.top;
  const nd = useNextDue().data;
  const nodes = o?.nodes; const relays = o?.relays;
  // 中控刚启动：健康计数还在变（先出的不准），这三张卡先显示加载态，数据到齐（settling 变 false，随实时推送到）再出数
  const health = loading || !!o?.settling;
  const down = (nodes?.down ?? 0) + (relays?.down ?? 0);
  const tr = o?.traffic_month;
  const trTotal = tr ? tr.up + tr.down : 0; const trScale = F.bytesScale(trTotal);
  // 计费统一按 USD：服务端折好的合计（monthly_usd）；悬停列出各原币种金额与所用汇率，没有汇率的单列在说明行
  const usd = o?.monthly_usd;
  const foreign = usd?.by_currency.filter((p) => p.currency !== 'USD') ?? [];
  const sp = o?.monthly_usd_spare;
  const usdEmpty = !!usd && !usd.by_currency.length && !usd.unconverted.length;
  return (
    <HomeSection className="hsec-hero" icon={<LayoutDashboard />} label="监控总览"
      title={<h1 className="hero-greet">{F.greeting()}，<span className="hero-name">{me?.username ?? ''}</span></h1>} sub={statusLine(o, down) || undefined}>
      <div className="hero-grid">
        <div className="stat-grid">
          {/* 说明行用全站同一套状态名（降级 / 维护 / 故障 / 未知）；「正常」包括降级与维护（照常服务）这一点放在大数字的悬停说明里 */}
          <Stat loading={health} tone="ok" icon={<Server />} label="节点正常" info={`${NORMAL_INFO}${o?.spare ? `\n另有备用 ${o.spare} 台，不计在内：${SPARE_TIP}` : ''}`} to={NODES_LINK} value={<span data-tip="降级和维护的机器照常服务，也算正常"><CountUp value={nodes && normal(nodes)} /></span>} unit={nodes ? `/ ${nodes.total}` : undefined}
            caption={nodes && <HealthCaption c={nodes} to={DOWN_LINK} />} />
          <Stat loading={health} tone="teal" icon={<Waypoints />} label="线路机正常" info={NORMAL_INFO} to={RELAYS_LINK} value={<span data-tip="降级和维护的机器照常服务，也算正常"><CountUp value={relays && normal(relays)} /></span>} unit={relays ? `/ ${relays.total}` : undefined}
            caption={relays && <HealthCaption c={relays} to={`${DOWN_LINK}&kind=relay`} />} />
          <Stat loading={health} tone="bad" icon={<AlertTriangle />} label="故障" value={<CountUp value={o ? down : undefined} />} unit="台" to={DOWN_LINK} caption={o && <><DotCount tone="bad" n={o.nodes.down} label="节点" /><DotCount tone="bad" n={o.relays.down} label="线路机" /></>} />
          <Stat loading={loading} tone="amber" icon={<CalendarClock />} label="即将到期" value={<CountUp value={o?.due_soon} />} unit="台" to="/nodes?due=soon&sort=due" caption={o && nd !== undefined && <>{nd ? <span className="stat-who">最近<span className="ellipsis" data-tip={nd.name}>{nd.name}</span>{nd.days <= 1 ? <b>{nd.days === 0 ? '今天' : '明天'}</b> : <><b className="num">{nd.days}</b>天后</>}</span> : <span>近期没有到期</span>}{o.overdue > 0 && <DotCount tone="bad" n={o.overdue} label="已过期" />}</>} />
          <Stat loading={loading} tone="gold" icon={<CircleDollarSign />} label="每月费用" to={COST_LINK} info={`在用的节点和线路机的月费，统一折成美元；不含备用团队的机器（它们不再续费，另列在说明行）。汇率在「设置 → 汇率」${usd ? `\n${usdTotalTip(usd)}` : ''}`}
            value={!usd || usdEmpty ? '—' : <span data-tip={usdTotalTip(usd)}>{F.money(usd.cents, 'USD')}</span>}
            caption={usd && <>{(usd.unconverted.length ? <span style={{ color: 'var(--warn-text)' }} data-tip="没有这些币种的汇率，未计入合计">未折算 {usd.unconverted.map((c) => F.money(c.cents, c.currency)).join(' + ')}</span>
              : foreign.length ? <span data-tip={usdTotalTip(usd)}>含 {foreign.map((p) => `${F.money(p.cents, p.currency)} ≈ ${F.money(p.usd_cents, 'USD')}`).join(' · ')}</span>
              : <span>在用 <b className="num">{o!.nodes.total + o!.relays.total + (o!.spare ?? 0) - (sp?.count ?? 0)}</b> 台 · 按月折算</span>)}
              {!!sp?.count && <span data-tip={`备用团队的机器等到期放生，不再续费，不在左边的合计里，单独列在这里
${usdTotalTip(sp)}`}>备用 <b className="num">{sp.count}</b> 台 {F.money(sp.cents, 'USD')} 另计</span>}</>} />
          {/* 每条推送都在涨，不滚数（滚到的位数也显示不出来，白白逐帧重画）：同网卡速率，直接到位 */}
          <Stat loading={loading} tone="sky" icon={<TrendingUp />} label="本月业务流量" info={"客户经所有入口的下载加上传。\n单台机器的「本周期已用」按网卡、从账单日算起，数字通常更大。"} to={TRAFFIC_LINK} value={tr ? F.inScaleBytes(trTotal, trScale) : '—'} unit={tr ? trScale.u : undefined}
            caption={tr && <><span>下载 <Num f={F.bytes(tr.down)} /></span><span>上传 <Num f={F.bytes(tr.up)} /></span><FromTag from={tr.counted_from} /></>} />
          {/* 有吃紧的：点卡片打开最紧那台的流量页签 */}
          <Stat loading={loading} tone="violet" icon={<Database />} label="流量额度吃紧" value={<CountUp value={qt?.n} />} unit={qt ? '台' : undefined} onClick={top ? () => openDrawer(top.id, 'traffic') : undefined}
            caption={qt && (top ? <span className="stat-who" data-tip={`${top.name}${top.forecast ? `：${top.forecast}` : `：已用 ${top.pct}%`}`}>最紧<span className="ellipsis">{top.name}</span>{top.full ? <><b className="num">{top.full}</b>满</> : <b className="num">{top.pct}%</b>}</span> : `都在 ${meta().thresholds.traffic_quota_warn_pct}% 以下`)} />
          <Stat loading={loading} tone="blue" icon={<CloudUpload />} label="上架中 · 退役中" value={o ? <CountUp value={o.provisioning + o.retiring} /> : '—'} unit="台" to="/jobs?view=running" caption={o && <ProvisionCaption o={o} />} />
        </div>
        <ClusterPanel o={o} />
      </div>
    </HomeSection>
  );
});

const ONLINE_N = 24;
const OFFLINE_TIP = 'agent 不再上报的机器，一定算故障。点开会看到全部故障，包括入口不通等其他原因';
/** 软路由一行（集群状态底部，点进软路由页）：在线台数、出口可用、客户端。离线的读数是旧的，不算；一台软路由都没有就不出现 */
function RoutersLink() {
  const list = useRouters().data;
  if (!list?.length) return null;
  const on = list.filter((r) => r.state === 'online');
  const ex = on.reduce((a, r) => ({ alive: a.alive + (r.summary?.exits.alive ?? 0), total: a.total + (r.summary?.exits.total ?? 0) }), { alive: 0, total: 0 });
  const bad = list.some((r) => r.state === 'offline');
  return (
    <Link to="/routers" className="cluster-routers" aria-label="软路由">
      <span className="cluster-routers-label"><RouterIcon />软路由</span>
      <span>在线 <b className="num" data-bad={bad ? '' : undefined}>{on.length}/{list.length}</b> · 出口 <b className="num" data-warn={ex.alive < ex.total ? '' : undefined}>{ex.alive}/{ex.total}</b> · 客户端 <b className="num">{on.reduce((n, r) => n + (r.summary?.clients ?? 0), 0)}</b> 台</span>
      <ChevronRight className="cluster-routers-go" aria-hidden />
    </Link>
  );
}

function ClusterPanel({ o }: { o: Overview | undefined }) {
  const trace = useFleetSeries();
  const offNames = useOfflineNames().data;
  // 在线/离线按台机器、按 agent 连通（服务端算，同 isQuiet）：有告警但照常上报的机器算在线；降级 / 故障只决定状态胶囊。
  // 分母 = 应该有 agent 的机器：同机 IP 节点随宿主机的 agent 计入，家宽代理不需要 agent（no_agent），不计
  const online = o?.online ?? 0; const offline = o?.offline ?? 0; const total = online + offline;
  const down = o ? o.nodes.down + o.relays.down : 0; const degraded = o ? o.nodes.degraded + o.relays.degraded : 0;
  const rate = total ? (online / total) * 100 : 0;
  // 三处总数对得上：KPI 的节点 + 线路机 = 在用；agent 在线的分母 = 在用 − 家宽代理；机器矩阵的「全部」= 在用 + 上架中 / 退役中 / 上架失败（备用团队的机器照常算在用）
  const spare = o?.spare ?? 0;
  const active = o ? o.nodes.total + o.relays.total : 0;
  const scope = o ? `分母 = 在用 ${active} 台（节点 ${o.nodes.total} + 线路机 ${o.relays.total}${spare ? `；另有闲置备用 ${spare} 台等放生，不计` : ''}）− 家宽代理 ${o.no_agent} 台（不需要 agent）${o.ip_nodes ? `；同机 IP 节点 ${o.ip_nodes} 台已算在内（跟随宿主机的 agent）` : ''}
机器矩阵的「全部」${active + spare + o.provisioning + o.retiring + o.failed} 台 = 在用 ${active}${spare ? ` + 闲置备用 ${spare}` : ''} + 上架中 ${o.provisioning} + 退役中 ${o.retiring} + 上架失败 ${o.failed}` : '';
  // 格条按比例分段着色：在线绿、离线红（末尾几格）；有离线至少 1 格红，不会因为离线占比不到一格就全绿；没有机器全灰
  const colors = React.useMemo(() => {
    const bad = offline ? Math.max(1, Math.round((offline / total) * ONLINE_N)) : 0;
    return Array.from({ length: ONLINE_N }, (_, i) => (!total ? 'var(--neutral)' : i >= ONLINE_N - bad ? 'var(--bad)' : 'var(--ok)'));
  }, [offline, total]);
  // 格条的悬停：在线、离线台数，离线的最多列 5 台、一台一行（名字里的「-」处不会被折成两行）；放在格条下方，不盖住上面的在线率
  const barTip = o ? `在线 ${online} · 离线 ${offline}${offline && offNames?.length ? `\n离线的机器：\n${offNames.slice(0, 5).join('\n')}${offNames.length > 5 ? `\n另有 ${offNames.length - 5} 台` : ''}` : ''}` : undefined;
  // 有离线时格条与「agent 离线」可点：去机器页的故障筛选（见 DOWN_LINK）
  const bar = <Cells pct={o ? 100 : null} n={ONLINE_N} colors={colors} className="cells-lg" />;
  const kv = <><small>agent 离线</small><b className="num" data-bad={offline ? '' : undefined}>{o ? `${offline} 台` : '—'}</b></>;
  // 只有故障算异常：降级照常服务，胶囊仍是「状态健康」，降级台数写在下面的「在线 · 降级」里
  const now = o ? o.net_rx_bps + o.net_tx_bps : 0; const sc = F.rateScale(now);
  return (
    <section className="cluster" aria-label="集群状态">
      <header className="cluster-head">
        <h2 className="cluster-title"><Activity />集群状态</h2>
        {o && <span className="pill is-outlined" data-tone={down ? 'bad' : 'ok'}>{down ? '有故障' : '状态健康'}</span>}
      </header>
      <div className="cluster-box">
        <div className="cluster-row">
          <span className="cluster-big"><span className="num">{o ? <><CountUp value={rate} format={(n) => n.toFixed(rate % 1 ? 1 : 0)} />%</> : '—'}</span><small data-tip={scope || undefined}>agent 在线率</small></span>
          {offline ? <Link className="cluster-kv" to={DOWN_LINK} data-tip={OFFLINE_TIP}>{kv}</Link> : <span className="cluster-kv" data-tip={OFFLINE_TIP}>{kv}</span>}
        </div>
        {offline ? <Link to={DOWN_LINK} className="block" data-tip={barTip} data-tip-side="bottom" aria-label={`agent 离线 ${offline} 台`}>{bar}</Link> : <div data-tip={barTip} data-tip-side="bottom">{bar}</div>}
        <div className="cluster-foot"><span>agent 在线 <b className="num">{o ? `${online} / ${total}` : '—'}</b>{o && ' 台'}{scope && <InfoTip text={scope} />}</span>{degraded > 0 && <span>降级 <b className="num">{degraded}</b> 台</span>}</div>
      </div>
      <div className="cluster-box">
        <div className="cluster-row">
          <span className="cluster-big"><span className="num">{o ? F.inScale(now, sc) : '—'}</span><small>{o ? sc.u : ''} 网卡速率</small></span>
        </div>
        <div className="cluster-charts">
          <div className="cchart" data-dir="down">
            <div className="cchart-head"><span><ArrowDown />下行</span><b className="num">↓ <Num f={F.rate(o?.net_rx_bps)} /></b></div>
            <AreaSpark points={trace} pick={(p) => p.rx} tone="down" />
          </div>
          <div className="cchart" data-dir="up">
            <div className="cchart-head"><span><ArrowUp />上行</span><b className="num">↑ <Num f={F.rate(o?.net_tx_bps)} /></b></div>
            <AreaSpark points={trace} pick={(p) => p.tx} tone="up" />
          </div>
        </div>
      </div>
      <RoutersLink />
    </section>
  );
}
