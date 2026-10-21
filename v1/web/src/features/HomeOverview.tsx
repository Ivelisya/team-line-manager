// 首页上半部分：大圆角浅灰分区（HomeSection）+「监控总览」——问候与一句话状态、8 张 KPI 卡、集群状态面板。
// 集群状态：在线率大读数 + 格条、在线/离线/总计、状态胶囊；网卡速率（实时）+ 上行 / 下行两张小折线（全站速率环形缓冲：开场垫近 30 分钟历史，之后实时流喂，见 lib/live）。
import * as React from 'react';
import { Activity, AlertTriangle, ArrowDown, ArrowUp, CalendarClock, CircleDollarSign, CloudUpload, Gauge, LayoutDashboard, Server, TrendingUp, Waypoints } from 'lucide-react';
import type { Overview } from '@/lib/types';
import * as F from '@/lib/format';
import { useMe } from '@/lib/queries';
import { useFleetSeries } from '@/lib/live';
import { cn } from '@/lib/utils';
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

function statusLine(o: Overview | undefined, issues: number): string {
  if (!o) return '正在汇总机器状态…';
  const due = o.due_7d ? `，${o.due_7d} 台 7 天内到期` : '';
  const fail = o.failed ? `，${o.failed} 台上架失败` : '';
  if (issues) return `有 ${issues} 台机器故障或降级${fail}${due}，点“故障与降级”查看告警。`;
  if (o.failed) return `${o.failed} 台机器上架失败${due}，可在作业页重试或在详情里退役。`;
  return `全部 ${o.nodes.total + o.relays.total} 台机器运行良好${due}，监控数据持续采集中。`;
}

/** memo：首页因机器列表推送、筛选、开关抽屉重渲染时，总览只在 overview 数据变了才重画（它的数据由 metrics 推送单独更新） */
export const OverviewSection = React.memo(function OverviewSection({ o, loading }: { o: Overview | undefined; loading: boolean }) {
  const me = useMe().data;
  const nodes = o?.nodes; const relays = o?.relays;
  const issues = (nodes?.down ?? 0) + (nodes?.degraded ?? 0) + (relays?.down ?? 0) + (relays?.degraded ?? 0);
  const tr = o?.traffic_month;
  const trTotal = tr ? tr.up + tr.down : 0; const trScale = F.bytesScale(trTotal);
  const rateTotal = o ? o.net_rx_bps + o.net_tx_bps : 0; const rateSc = F.rateScale(rateTotal);
  const [cost0, ...costMore] = o?.monthly_cost ?? [];
  return (
    <HomeSection className="hsec-hero" icon={<LayoutDashboard />} label="监控总览"
      title={<h1 className="hero-greet">{F.greeting()}，<span className="hero-name">{me?.username ?? ''}</span></h1>} sub={statusLine(o, issues)}>
      <div className="hero-grid">
        <div className="stat-grid">
          <Stat loading={loading} tone="ok" icon={<Server />} label="节点正常" value={<CountUp value={nodes?.ok} />} unit={nodes ? `/ ${nodes.total}` : undefined}
            caption={nodes && <><DotCount tone="warn" n={nodes.degraded} label="降级" /><DotCount tone="bad" n={nodes.down} label="故障" /><DotCount tone="neutral" n={nodes.unknown} label="未知" /></>} />
          <Stat loading={loading} tone="teal" icon={<Waypoints />} label="线路机正常" value={<CountUp value={relays?.ok} />} unit={relays ? `/ ${relays.total}` : undefined}
            caption={relays && <><DotCount tone="warn" n={relays.degraded} label="降级" /><DotCount tone="bad" n={relays.down} label="故障" /></>} />
          <Stat loading={loading} tone="bad" icon={<AlertTriangle />} label="故障与降级" value={<CountUp value={o ? issues : undefined} />} unit="台" to="/incidents" caption={o && <>{issues ? '点击查看告警' : !o.failed && '一切正常'}{o.failed > 0 && <DotCount tone="bad" n={o.failed} label="上架失败" />}</>} />
          <Stat loading={loading} tone="amber" icon={<CalendarClock />} label="7 天内到期" value={<CountUp value={o?.due_7d} />} unit="台" to="/nodes?sort=due" caption="需要续费的机器" />
          <Stat loading={loading} tone="gold" icon={<CircleDollarSign />} label="每月费用" value={cost0 ? F.money(cost0.cents, cost0.currency) : '—'}
            caption={costMore.length ? `另有 ${costMore.map((c) => F.money(c.cents, c.currency)).join(' + ')}（不同币种分开计）` : '在用机器按月折算'} />
          <Stat loading={loading} tone="sky" icon={<TrendingUp />} label="本月业务流量" value={tr ? <CountUp value={trTotal} format={(n) => F.inScale(n, trScale)} /> : '—'} unit={tr ? trScale.u : undefined}
            caption={tr && <><span>下载 <Num f={F.bytes(tr.down)} /></span><span>上传 <Num f={F.bytes(tr.up)} /></span></>} />
          <Stat loading={loading} tone="violet" icon={<Gauge />} label="网卡速率" value={o ? <CountUp value={rateTotal} format={(n) => F.inScale(n, rateSc)} /> : '—'} unit={o ? rateSc.u : undefined}
            caption={o && <><span>下行 <Num f={F.rate(o.net_rx_bps)} /></span><span>上行 <Num f={F.rate(o.net_tx_bps)} /></span></>} />
          <Stat loading={loading} tone="blue" icon={<CloudUpload />} label="上架中 · 退役中" value={o ? <CountUp value={o.provisioning + o.retiring} /> : '—'} unit="台" to="/jobs" caption={o && `上架 ${o.provisioning} · 退役 ${o.retiring}`} />
        </div>
        <ClusterPanel o={o} />
      </div>
    </HomeSection>
  );
});

const ONLINE_N = 24;
function ClusterPanel({ o }: { o: Overview | undefined }) {
  const trace = useFleetSeries();
  // 在线/离线按 agent 连通（服务端算，同 isQuiet）：有告警但照常上报的机器算在线；降级 / 故障只决定状态胶囊
  const online = o?.online ?? 0; const offline = o?.offline ?? 0; const total = online + offline;
  const down = o ? o.nodes.down + o.relays.down : 0; const degraded = o ? o.nodes.degraded + o.relays.degraded : 0;
  const rate = total ? (online / total) * 100 : 0;
  // 格条按比例分段着色：在线绿、离线红；没有机器全灰
  const colors = React.useMemo(() => Array.from({ length: ONLINE_N }, (_, i) => (!total ? 'var(--neutral)' : ((i + 0.5) / ONLINE_N) * total < online ? 'var(--ok)' : 'var(--bad)')), [online, total]);
  const healthy = o && !down && !degraded;
  const now = o ? o.net_rx_bps + o.net_tx_bps : 0; const sc = F.rateScale(now);
  return (
    <section className="cluster" aria-label="集群状态">
      <header className="cluster-head">
        <div><h2 className="cluster-title"><Activity />集群状态</h2><p className="cluster-sub">机器在线率与网卡速率</p></div>
        {o && <span className="cpill" data-tone={healthy ? 'ok' : down ? 'bad' : 'warn'}><i />{healthy ? '状态健康' : '有异常'}</span>}
      </header>
      <div className="cluster-box">
        <div className="cluster-row">
          <span className="cluster-big"><span className="num">{o ? <><CountUp value={rate} format={(n) => n.toFixed(rate % 1 ? 1 : 0)} />%</> : '—'}</span><small>在线率</small></span>
          <span className="cluster-kv"><small>离线机器</small><b className="num" data-bad={offline ? '' : undefined}>{o ? `${offline} 台` : '—'}</b></span>
        </div>
        <Cells pct={o ? 100 : null} n={ONLINE_N} colors={colors} className="cells-lg" />
        <div className="cluster-foot"><span>在线 <b className="num">{o ? online : '—'}</b>{o && ' 台'}{degraded > 0 && <> · 降级 <b className="num">{degraded}</b></>}</span><span>总计 <b className="num">{o ? total : '—'}</b>{o && ' 台'}</span></div>
      </div>
      <div className="cluster-box">
        <div className="cluster-row">
          <span className="cluster-big"><span className="num">{o ? <CountUp value={now} format={(n) => F.inScale(n, sc)} /> : '—'}</span><small>{o ? sc.u : ''} 网卡速率（实时）</small></span>
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
