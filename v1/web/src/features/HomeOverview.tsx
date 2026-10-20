// 首页上半部分：大圆角浅灰分区（HomeSection）+「监控总览」——问候与一句话状态、8 张 KPI 卡、集群状态面板。
// 集群状态：在线率大读数 + 格条、在线/离线/总计、状态胶囊；实时总带宽 + 上行 / 下行两张小折线（全站速率的客户端环形缓冲，见 lib/traces）。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { Activity, AlertTriangle, ArrowDown, ArrowUp, CalendarClock, CircleDollarSign, CloudUpload, Gauge, LayoutDashboard, Server, TrendingUp, Waypoints } from 'lucide-react';
import type { Overview } from '@/lib/types';
import * as F from '@/lib/format';
import { useMe } from '@/lib/queries';
import { FLEET, useTrace } from '@/lib/traces';
import { cn } from '@/lib/utils';
import { CountUp, DotCount, Num } from '@/components/app/primitives';
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

type Tone = 'ok' | 'teal' | 'bad' | 'amber' | 'gold' | 'sky' | 'violet' | 'blue';
/** KPI 卡：彩色图标 + 标签，大号读数 + 小单位，一行灰色说明 */
function Stat({ icon, tone, label, value, unit, caption, to, loading }: { icon: React.ReactNode; tone: Tone; label: string; value: React.ReactNode; unit?: string; caption?: React.ReactNode; to?: string; loading?: boolean }) {
  const inner = (
    <>
      <span className="stat-label"><span className="stat-ic" data-tone={tone}>{icon}</span>{label}</span>
      <span className="stat-value">{loading ? <span className="skel" style={{ width: 64, height: 20 }} /> : <>{value}{unit && <small>{unit}</small>}</>}</span>
      <span className="stat-cap">{loading ? '' : caption ?? ' '}</span>
    </>
  );
  return to ? <Link className="stat" to={to}>{inner}</Link> : <div className="stat">{inner}</div>;
}

function statusLine(o: Overview | undefined, issues: number): string {
  if (!o) return '正在汇总机器状态…';
  const due = o.due_7d ? `，${o.due_7d} 台 7 天内到期` : '';
  if (issues) return `有 ${issues} 台机器故障或降级${due}，点“故障与降级”查看告警。`;
  return `全部 ${o.nodes.total + o.relays.total} 台机器运行良好${due}，监控数据持续采集中。`;
}

export function OverviewSection({ o, loading }: { o: Overview | undefined; loading: boolean }) {
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
          <Stat loading={loading} tone="bad" icon={<AlertTriangle />} label="故障与降级" value={<CountUp value={o ? issues : undefined} />} unit="台" to="/incidents" caption={o ? (issues ? '点击查看告警' : '一切正常') : undefined} />
          <Stat loading={loading} tone="amber" icon={<CalendarClock />} label="7 天内到期" value={<CountUp value={o?.due_7d} />} unit="台" to="/nodes" caption="需要续费的机器" />
          <Stat loading={loading} tone="gold" icon={<CircleDollarSign />} label="每月费用" value={o ? F.money(cost0?.cents ?? 0, cost0?.currency ?? 'USD') : '—'}
            caption={costMore.length ? `另有 ${costMore.map((c) => F.money(c.cents, c.currency)).join(' + ')}（不同币种分开计）` : '在用机器按月折算'} />
          <Stat loading={loading} tone="sky" icon={<TrendingUp />} label="本月业务流量" value={tr ? <CountUp value={trTotal} format={(n) => F.inScale(n, trScale)} /> : '—'} unit={tr ? trScale.u : undefined}
            caption={tr && <><span>↓ <Num f={F.bytes(tr.down)} /></span><span>↑ <Num f={F.bytes(tr.up)} /></span></>} />
          <Stat loading={loading} tone="violet" icon={<Gauge />} label="网卡速率" value={o ? <CountUp value={rateTotal} format={(n) => F.inScale(n, rateSc)} /> : '—'} unit={o ? rateSc.u : undefined}
            caption={o && <><span>↓ <Num f={F.rate(o.net_rx_bps)} /></span><span>↑ <Num f={F.rate(o.net_tx_bps)} /></span></>} />
          <Stat loading={loading} tone="blue" icon={<CloudUpload />} label="上架中 · 退役中" value={o ? <CountUp value={o.provisioning + o.retiring} /> : '—'} unit="台" to="/jobs" caption={o && `上架 ${o.provisioning} · 退役 ${o.retiring}`} />
        </div>
        <ClusterPanel o={o} />
      </div>
    </HomeSection>
  );
}

const ONLINE_N = 24;
function ClusterPanel({ o }: { o: Overview | undefined }) {
  const trace = useTrace(FLEET);
  const sum = (k: keyof Overview['nodes']) => (o ? o.nodes[k] + o.relays[k] : 0);
  const total = sum('total'); const down = sum('down'); const unknown = sum('unknown'); const degraded = sum('degraded');
  const offline = down + unknown; const online = total - offline;
  const rate = total ? (online / total) * 100 : 0;
  // 格条按比例分段着色：正常（含维护）绿、降级黄、故障红、未知灰
  const colors = React.useMemo(() => {
    const parts: [number, string][] = [[online - degraded, 'var(--ok)'], [degraded, 'var(--warn)'], [down, 'var(--bad)'], [unknown, 'var(--neutral)']];
    return Array.from({ length: ONLINE_N }, (_, i) => {
      let x = ((i + 0.5) / ONLINE_N) * total;
      for (const [n, c] of parts) { if (x < n) return c; x -= n; }
      return 'var(--neutral)';
    });
  }, [online, degraded, down, unknown, total]);
  const healthy = o && !down && !degraded;
  const now = o ? o.net_rx_bps + o.net_tx_bps : 0; const sc = F.rateScale(now);
  return (
    <section className="cluster" aria-label="集群状态">
      <header className="cluster-head">
        <div><h2 className="cluster-title"><Activity />集群状态</h2><p className="cluster-sub">机器在线率与实时网络吞吐</p></div>
        {o && <span className="cpill" data-tone={healthy ? 'ok' : down ? 'bad' : 'warn'}><i />{healthy ? '状态健康' : '有异常'}</span>}
      </header>
      <div className="cluster-box">
        <div className="cluster-row">
          <span className="cluster-big"><span className="num">{o ? <><CountUp value={rate} format={(n) => n.toFixed(rate % 1 ? 1 : 0)} />%</> : '—'}</span><small>在线率</small></span>
          <span className="cluster-kv"><small>离线机器</small><b className="num" data-bad={offline ? '' : undefined}>{o ? offline : '—'} 台</b></span>
        </div>
        <Cells pct={o ? 100 : null} n={ONLINE_N} colors={colors} className="cells-lg" />
        <div className="cluster-foot"><span>在线 <b className="num">{o ? online : '—'}</b> 台{degraded > 0 && <> · 降级 <b className="num">{degraded}</b></>}</span><span>总计 <b className="num">{o ? total : '—'}</b> 台</span></div>
      </div>
      <div className="cluster-box">
        <div className="cluster-row">
          <span className="cluster-big"><span className="num">{o ? <CountUp value={now} format={(n) => F.inScale(n, sc)} /> : '—'}</span><small>{o ? sc.u : ''} 实时总带宽</small></span>
        </div>
        <div className="cluster-charts">
          <div className="cchart" data-dir="up">
            <div className="cchart-head"><span><ArrowUp />上行网络</span><b className="num">↑ <Num f={F.rate(o?.net_tx_bps)} /></b></div>
            <AreaSpark points={trace} pick={(p) => p.tx} tone="up" />
          </div>
          <div className="cchart" data-dir="down">
            <div className="cchart-head"><span><ArrowDown />下行网络</span><b className="num">↓ <Num f={F.rate(o?.net_rx_bps)} /></b></div>
            <AreaSpark points={trace} pick={(p) => p.rx} tone="down" />
          </div>
        </div>
      </div>
    </section>
  );
}
