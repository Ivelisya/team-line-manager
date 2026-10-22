// 首页上半部分：大圆角浅灰分区（HomeSection）+「监控总览」——问候与一句话状态、8 张 KPI 卡、集群状态面板。
// 集群状态：在线率大读数 + 格条、在线/离线/总计、状态胶囊；网卡速率（实时）+ 上行 / 下行两张小折线（全站速率环形缓冲：开场垫近 30 分钟历史，之后实时流喂，见 lib/live）。
// 网卡速率只在集群状态里出现（KPI 原先也有一张，与它重复），KPI 那一格换成流量额度
import * as React from 'react';
import { Link } from 'react-router-dom';
import { Activity, AlertTriangle, ArrowDown, ArrowUp, CalendarClock, CircleDollarSign, CloudUpload, Database, LayoutDashboard, Server, TrendingUp, Waypoints } from 'lucide-react';
import type { Overview } from '@/lib/types';
import * as F from '@/lib/format';
import { useMe, useOfflineNames, useQuotaTight } from '@/lib/queries';
import { openDrawer } from '@/lib/drawer';
import { useFleetSeries } from '@/lib/live';
import { cn } from '@/lib/utils';
import { meta } from '@/lib/derive';
import { CountUp, DotCount, Num, Stat } from '@/components/app/primitives';
import { AreaSpark, Cells } from '@/components/app/viz';

/** 分区：浅灰大圆角容器，左上小胶囊标签（图标 + 文字）、标题、一行说明，右上放操作 */
export function HomeSection({ icon, label, title, sub, actions, children, className, headRef }: {
  icon: React.ReactNode; label: string; title: React.ReactNode; sub?: React.ReactNode; actions?: React.ReactNode; children: React.ReactNode; className?: string; headRef?: (el: HTMLElement | null) => void;
}) {
  return (
    <section className={cn('hsec', className)} aria-label={label}>
      <header className="hsec-head" ref={headRef}>
        <div className="hsec-top"><span className="hsec-badge">{icon}{label}</span>{actions && <div className="hsec-actions">{actions}</div>}</div>
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

/** 问候语下的一行状态：只列要处理的事（故障、上架失败、到期），没有就是「全部正常」 */
function statusLine(o: Overview | undefined, down: number): string {
  if (!o) return '';
  const parts = [
    down && `${down} 台故障`,
    o.failed && `${o.failed} 台上架失败`,
    o.overdue && `${o.overdue} 台已过期`,
    o.due_soon && `${o.due_soon} 台即将到期`,
  ].filter(Boolean);
  const unknown = o.nodes.unknown + o.relays.unknown;
  if (!down && !o.failed) parts.unshift(unknown ? `${unknown} 台状态未知` : '全部正常');
  return parts.join(' · ');
}

/** memo：首页因机器列表推送、筛选、开关抽屉重渲染时，总览只在 overview 数据（metrics 推送单独更新）或额度吃紧的结果变了才重画 */
export const OverviewSection = React.memo(function OverviewSection({ o, loading }: { o: Overview | undefined; loading: boolean }) {
  const me = useMe().data;
  const qt = useQuotaTight().data; const top = qt?.top;
  const nodes = o?.nodes; const relays = o?.relays;
  const down = (nodes?.down ?? 0) + (relays?.down ?? 0);
  const tr = o?.traffic_month;
  const trTotal = tr ? tr.up + tr.down : 0; const trScale = F.bytesScale(trTotal);
  const [cost0, ...costMore] = o?.monthly_cost ?? [];
  return (
    <HomeSection className="hsec-hero" icon={<LayoutDashboard />} label="监控总览"
      title={<h1 className="hero-greet">{F.greeting()}，<span className="hero-name">{me?.username ?? ''}</span></h1>} sub={statusLine(o, down) || undefined}>
      <div className="hero-grid">
        <div className="stat-grid">
          {/* 说明行用全站同一套状态名（降级 / 维护 / 故障 / 未知）；「正常」包括降级与维护（照常服务）这一点放在大数字的悬停说明里 */}
          <Stat loading={loading} tone="ok" icon={<Server />} label="节点正常" to={NODES_LINK} value={<span data-tip="正常 = 总数 − 故障 − 未知：降级、维护照常服务，也算在正常里"><CountUp value={nodes && normal(nodes)} /></span>} unit={nodes ? `/ ${nodes.total}` : undefined}
            caption={nodes && <><DotCount tone="warn" n={nodes.degraded} label="降级" />{nodes.maintenance > 0 && <DotCount tone="neutral" n={nodes.maintenance} label="维护" />}<DotCount tone="bad" n={nodes.down} label="故障" to={DOWN_LINK} />{nodes.unknown > 0 && <DotCount tone="neutral" n={nodes.unknown} label="未知" />}</>} />
          <Stat loading={loading} tone="teal" icon={<Waypoints />} label="线路机正常" to={RELAYS_LINK} value={<span data-tip="正常 = 总数 − 故障 − 未知：降级、维护照常服务，也算在正常里"><CountUp value={relays && normal(relays)} /></span>} unit={relays ? `/ ${relays.total}` : undefined}
            caption={relays && <><DotCount tone="warn" n={relays.degraded} label="降级" />{relays.maintenance > 0 && <DotCount tone="neutral" n={relays.maintenance} label="维护" />}<DotCount tone="bad" n={relays.down} label="故障" to={`${DOWN_LINK}&kind=relay`} />{relays.unknown > 0 && <DotCount tone="neutral" n={relays.unknown} label="未知" />}</>} />
          <Stat loading={loading} tone="bad" icon={<AlertTriangle />} label="故障" value={<CountUp value={o ? down : undefined} />} unit="台" to={DOWN_LINK} caption={o && (o.failed > 0 ? <DotCount tone="bad" n={o.failed} label="上架失败" /> : !down && '一切正常')} />
          <Stat loading={loading} tone="amber" icon={<CalendarClock />} label="即将到期" value={<CountUp value={o?.due_soon} />} unit="台" to="/nodes?due=soon&sort=due" caption={o?.overdue ? `另有 ${o.overdue} 台已过期` : undefined} />
          <Stat loading={loading} tone="gold" icon={<CircleDollarSign />} label="每月费用" to={COST_LINK} value={cost0 ? <span data-tip="在用机器按月折算，不同币种分开计">{F.money(cost0.cents, cost0.currency)}</span> : '—'}
            caption={costMore.length ? `另有 ${costMore.map((c) => F.money(c.cents, c.currency)).join(' + ')}` : undefined} />
          {/* 每条推送都在涨，不滚数（滚到的位数也显示不出来，白白逐帧重画）：同网卡速率，直接到位 */}
          <Stat loading={loading} tone="sky" icon={<TrendingUp />} label="本月业务流量" info="按入口统计；网卡流量在线路机和节点上各算一次，不做合计" to={TRAFFIC_LINK} value={tr ? F.inScale(trTotal, trScale) : '—'} unit={tr ? trScale.u : undefined}
            caption={tr && <><span>下载 <Num f={F.bytes(tr.down)} /></span><span>上传 <Num f={F.bytes(tr.up)} /></span></>} />
          {/* 有吃紧的：点卡片打开最紧那台的流量页签 */}
          <Stat loading={loading} tone="violet" icon={<Database />} label="流量额度吃紧" value={<CountUp value={qt?.n} />} unit={qt ? '台' : undefined} onClick={top ? () => openDrawer(top.id, 'traffic') : undefined}
            caption={qt && (top ? <span className="stat-who">最紧<span className="ellipsis" data-tip={top.name}>{top.name}</span>已用<b className="num">{top.pct}%</b></span> : `都在 ${meta().thresholds.traffic_quota_warn_pct}% 以下`)} />
          <Stat loading={loading} tone="blue" icon={<CloudUpload />} label="上架中 · 退役中" value={o ? <CountUp value={o.provisioning + o.retiring} /> : '—'} unit="台" to="/jobs?view=running" caption={o && <span>上架 <b className="num">{o.provisioning}</b> · 退役 <b className="num">{o.retiring}</b></span>} />
        </div>
        <ClusterPanel o={o} />
      </div>
    </HomeSection>
  );
});

const ONLINE_N = 24;
function ClusterPanel({ o }: { o: Overview | undefined }) {
  const trace = useFleetSeries();
  const offNames = useOfflineNames().data;
  // 在线/离线按 agent 连通（服务端算，同 isQuiet）：有告警但照常上报的机器算在线；降级 / 故障只决定状态胶囊。
  // 只数有自己 agent 的机器：家宽代理没有 agent，同机 IP 节点借宿主机的 agent
  const online = o?.online ?? 0; const offline = o?.offline ?? 0; const total = online + offline;
  const down = o ? o.nodes.down + o.relays.down : 0; const degraded = o ? o.nodes.degraded + o.relays.degraded : 0;
  const rate = total ? (online / total) * 100 : 0;
  // 格条按比例分段着色：在线绿、离线红（末尾几格）；有离线至少 1 格红，不会因为离线占比不到一格就全绿；没有机器全灰
  const colors = React.useMemo(() => {
    const bad = offline ? Math.max(1, Math.round((offline / total) * ONLINE_N)) : 0;
    return Array.from({ length: ONLINE_N }, (_, i) => (!total ? 'var(--neutral)' : i >= ONLINE_N - bad ? 'var(--bad)' : 'var(--ok)'));
  }, [offline, total]);
  // 格条的悬停：在线、离线台数，离线的最多列 5 台、一台一行（名字里的「-」处不会被折成两行）；放在格条下方，不盖住上面的在线率
  const barTip = o ? `在线 ${online} · 离线 ${offline}${offline && offNames?.length ? `\n${offNames.slice(0, 5).join('\n')}${offNames.length > 5 ? `\n等 ${offNames.length} 台` : ''}` : ''}` : undefined;
  // 有离线时格条与「离线机器」可点：去机器页的故障筛选（见 DOWN_LINK）
  const bar = <Cells pct={o ? 100 : null} n={ONLINE_N} colors={colors} className="cells-lg" />;
  const kv = <><small>离线机器</small><b className="num" data-bad={offline ? '' : undefined}>{o ? `${offline} 台` : '—'}</b></>;
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
          <span className="cluster-big"><span className="num">{o ? <><CountUp value={rate} format={(n) => n.toFixed(rate % 1 ? 1 : 0)} />%</> : '—'}</span><small data-tip="只数有自己 agent 的机器：家宽代理没有 agent，不计；同机 IP 节点算在宿主机里">在线率</small></span>
          {offline ? <Link className="cluster-kv" to={DOWN_LINK} data-tip="agent 不再上报">{kv}</Link> : <span className="cluster-kv" data-tip="agent 不再上报">{kv}</span>}
        </div>
        {offline ? <Link to={DOWN_LINK} className="block" data-tip={barTip} data-tip-side="bottom" aria-label={`离线 ${offline} 台`}>{bar}</Link> : <div data-tip={barTip} data-tip-side="bottom">{bar}</div>}
        <div className="cluster-foot"><span>在线 <b className="num">{o ? online : '—'}</b>{o && ' 台'}{degraded > 0 && <> · 降级 <b className="num">{degraded}</b></>}</span><span>总计 <b className="num">{o ? total : '—'}</b>{o && ' 台'}</span></div>
      </div>
      <div className="cluster-box">
        <div className="cluster-row">
          <span className="cluster-big"><span className="num">{o ? F.inScale(now, sc) : '—'}</span><small>{o ? sc.u : ''} 网卡速率</small></span>
        </div>
        <div className="cluster-charts">
          <div className="cchart" data-dir="up">
            <div className="cchart-head"><span><ArrowUp />上行</span><b className="num">↑ <Num f={F.rate(o?.net_tx_bps)} /></b></div>
            <AreaSpark points={trace} pick={(p) => p.tx} tone="up" />
          </div>
          <div className="cchart" data-dir="down">
            <div className="cchart-head"><span><ArrowDown />下行</span><b className="num">↓ <Num f={F.rate(o?.net_rx_bps)} /></b></div>
            <AreaSpark points={trace} pick={(p) => p.rx} tone="down" />
          </div>
        </div>
      </div>
    </section>
  );
}
