// 节点卡片：一套外壳、三种视图。每个视图的“槽位”固定（大 10 / 小 7 / 紧凑 6），
// 无论机器是主机、家宽代理还是线路机，同一个槽位都渲染同样高度的内容或占位——配合 cards.css 的 subgrid，同一行的卡片分段永远对齐。
import { memo } from 'react';
import { Activity, ArrowDown, ArrowDownToLine, ArrowUp, ArrowUpFromLine, Calendar, CircleDollarSign, Cpu, Database, Globe, HardDrive, MemoryStick, Radar, RefreshCw, Settings2, PanelRightOpen, ServerOff } from 'lucide-react';
import type { MachineView, Plan } from '@/lib/types';
import * as F from '@/lib/format';
import { beatCells, entryText, hasHost, isQuiet, monthTraffic, probeSummary, quota, resourceMeters, subLabel } from '@/lib/derive';
import { useSeries, usePlans } from '@/lib/queries';
import { useDrawer, useInView } from '@/lib/drawer';
import { KIND_LABEL } from '@/lib/sort';
import { cn } from '@/lib/utils';
import { Beat, Chip, HealthPill, KindIcon, LabelValue, Mark, Meter, Num, Quota } from './primitives';
import { Button, IconButton } from '@/components/ui/button';

export type CardView = 'large' | 'small' | 'compact';
const VIEW_ATTR: Record<CardView, string> = { large: 'large', small: 'small', compact: 'compact' };

function Dot({ m }: { m: MachineView }) {
  const s = m.health.status;
  const tone = s === 'down' ? 'bad' : s === 'degraded' ? 'warn' : s === 'maintenance' ? 'info' : s === 'ok' ? undefined : 'neutral';
  return <span className="dot" data-tone={tone} />;
}

function Head({ m, view, onOpen }: { m: MachineView; view: CardView; onOpen: (tab?: 'overview') => void }) {
  return (
    <header className="nc-head">
      <Dot m={m} /><KindIcon kind={m.kind} />
      <button type="button" className="nc-name" onClick={() => onOpen()} title={`打开 ${m.name} 的详情`}>{m.name}</button>
      {view === 'compact' && <><span className="nc-ipsep">·</span><span className="nc-ip">{m.ip}</span></>}
      <div className="nc-actions"><IconButton label="打开详情" onClick={() => onOpen()}><PanelRightOpen size={16} /></IconButton></div>
    </header>
  );
}
const ipMarks = (m: MachineView) => <span className="nc-marks"><Mark>{m.ip.includes(':') ? 'IPv6' : 'IPv4'}</Mark>{m.ssh_port !== 22 && <Mark title="SSH 端口">SSH {m.ssh_port}</Mark>}</span>;

function Meters({ m, plans, compact }: { m: MachineView; plans: Plan[]; compact?: boolean }) {
  const r = resourceMeters(m, plans);
  if (!r) return <div className="nc-res"><div className="nc-hold"><ServerOff />{m.kind === 'proxy' ? '家宽代理没有主机，无 CPU / 内存指标' : m.lifecycle === 'provisioning' ? '上架完成后开始采集' : '暂无主机指标'}</div></div>;
  const quiet = isQuiet(m);
  return (
    <div className="nc-res">
      <Meter icon={<Cpu />} label="CPU" value={r.cpu.value} pct={r.cpu.pct} sub={compact ? undefined : r.cpu.sub} empty={quiet} />
      <Meter icon={<MemoryStick />} label="内存" value={r.mem.value} pct={r.mem.pct} sub={compact ? undefined : r.mem.sub} empty={quiet} />
      <Meter icon={<HardDrive />} label="磁盘" value={r.disk.value} pct={r.disk.pct} sub={compact ? undefined : r.disk.sub} empty={quiet} />
      <Meter icon={<Activity />} label="负载" value={r.load.value} pct={r.load.pct} sub={compact ? undefined : r.load.sub} empty={quiet} />
    </div>
  );
}

function EntryTable({ m }: { m: MachineView }) {
  const t = monthTraffic(m);
  const row = (label: string, d: number, u: number) => <tr><th>{label}</th><td><Num f={F.bytes(d)} /></td><td><Num f={F.bytes(u)} /></td></tr>;
  return (
    <table className="nc-etable">
      <thead><tr><th>周期</th><th>↓ 下行</th><th>↑ 上行</th></tr></thead>
      <tbody>{row('今日', t.today_down, t.today_up)}{row('本月', t.down, t.up)}</tbody>
    </table>
  );
}

function BeatSlot({ m, enabled }: { m: MachineView; enabled: boolean }) {
  const live = m.lifecycle === 'active';
  const s = useSeries(m.id, 'probe', '1h', enabled && live);
  const ps = probeSummary(m);
  const cap = !live ? <span className="t-muted">{m.lifecycle === 'retired' ? '已退役' : '尚未开始探测'}</span>
    : m.kind === 'relay' ? <>入口连通 <b className="num">{ps.tcpOk}/{ps.total}</b></>
    : <>TCP <b className="num">{ps.tcp != null ? `${ps.tcp} ms` : '—'}</b> · 出口 <b className="num">{ps.egress != null ? `${ps.egress} ms` : '—'}</b></>;
  return <Beat cells={live ? beatCells(s.data?.points) : []} caption={cap} win="近 1 小时" stale={isQuiet(m)} />;
}

function DueVal({ m }: { m: MachineView }) {
  const d = F.dueFmt(m.subscription?.due_days);
  return <span className={cn('nc-run-val', d.tone && `is-${d.tone}`)}><Num f={d.text} /></span>;
}

function TagsList({ m, price = true }: { m: MachineView; price?: boolean }) {
  const p = subLabel(m);
  return (
    <>
      <div className="nc-tags-list">
        {m.provider_code && <Chip title={m.provider?.name}>{m.provider_code}</Chip>}
        {m.kind !== 'vps' && <Chip variant="neutral">{KIND_LABEL[m.kind]}</Chip>}
        {m.service_ref && <Chip>{m.service_ref}</Chip>}
        {m.maintenance && <Chip variant="warn">维护中</Chip>}
      </div>
      {price && p && <span style={{ marginLeft: 'auto' }}><Chip variant="price"><span className="num">{p}</span></Chip></span>}
    </>
  );
}

function Foot({ m, onOpen }: { m: MachineView; onOpen: (tab?: 'manage') => void }) {
  return (
    <footer className="nc-foot">
      <HealthPill m={m} />
      <div className="nc-foot-right">
        <Button variant="text" size="sm" onClick={() => onOpen()}><Settings2 size={14} /><span>详情</span></Button>
        <Button variant="text" size="sm" onClick={() => onOpen('manage')}><span>管理</span></Button>
      </div>
    </footer>
  );
}

function CellsRow({ m }: { m: MachineView }) {
  const due = F.dueFmt(m.subscription?.due_days);
  const t = monthTraffic(m); const ps = probeSummary(m);
  const money = m.subscription ? F.money(m.subscription.price_cents, m.subscription.currency) : '—';
  const h = m.host;
  const dueCell = (
    <div className="nc-cell"><div className="nc-cell-label">到期 · 月费</div>
      <div className="nc-cell-row"><Calendar /><Num f={due.text} /></div>
      <div className="nc-cell-row"><CircleDollarSign /><span className="num">{money}</span></div></div>
  );
  if (h) return (
    <div className="nc-cells3">
      <div className="nc-cell"><div className="nc-cell-label">网卡速率</div>
        <div className="nc-cell-row"><ArrowDownToLine /><Num f={F.rate(h.net_rx_bps)} /></div>
        <div className="nc-cell-row"><ArrowUpFromLine /><Num f={F.rate(h.net_tx_bps)} /></div></div>
      <div className="nc-cell"><div className="nc-cell-label">本月业务</div>
        <div className="nc-cell-row"><ArrowDown /><Num f={F.bytes(t.down)} /></div>
        <div className="nc-cell-row"><ArrowUp /><Num f={F.bytes(t.up)} /></div></div>
      {dueCell}
    </div>
  );
  return (
    <div className="nc-cells3">
      <div className="nc-cell"><div className="nc-cell-label">本月业务</div>
        <div className="nc-cell-row"><ArrowDown /><Num f={F.bytes(t.down)} /></div>
        <div className="nc-cell-row"><ArrowUp /><Num f={F.bytes(t.up)} /></div></div>
      <div className="nc-cell"><div className="nc-cell-label">TCP · 出口</div>
        <div className="nc-cell-row"><Radar /><Num f={F.ms(ps.tcp)} /></div>
        <div className="nc-cell-row"><Globe /><Num f={F.ms(ps.egress)} /></div></div>
      {dueCell}
    </div>
  );
}

function FlowRow({ m }: { m: MachineView }) {
  const h = m.host; const t = monthTraffic(m); const ps = probeSummary(m);
  const col = (title: string, a: React.ReactNode, b: React.ReactNode, ia: React.ReactNode, ib: React.ReactNode) => (
    <div><div className="nc-flow2-title">{title}</div><div className="nc-flow2-row">{ia}{a}</div><div className="nc-flow2-row">{ib}{b}</div></div>
  );
  return (
    <div className="nc-flow2">
      {h ? col('网卡速率', <Num f={F.rate(h.net_rx_bps)} />, <Num f={F.rate(h.net_tx_bps)} />, <ArrowDownToLine />, <ArrowUpFromLine />)
        : col('TCP · 出口', <Num f={F.ms(ps.tcp)} />, <Num f={F.ms(ps.egress)} />, <Radar />, <Globe />)}
      {col('本月业务', <Num f={F.bytes(t.down)} />, <Num f={F.bytes(t.up)} />, <ArrowDown />, <ArrowUp />)}
    </div>
  );
}

function NodeCardImpl({ m, view, current }: { m: MachineView; view: CardView; current?: boolean }) {
  const { open } = useDrawer();
  const plans = usePlans().data ?? [];
  const [ref, seen] = useInView<HTMLElement>();
  const onOpen = (tab?: 'overview' | 'manage') => open(m.id, tab);
  const q = quota(m); const host = hasHost(m);
  const t = monthTraffic(m);
  const quiet = isQuiet(m);

  return (
    <article ref={ref} className={cn('ncard', current && 'is-current', quiet && 'is-quiet')} data-view={VIEW_ATTR[view]} data-id={m.id} data-kind={m.kind} data-health={m.health.status} data-offline={quiet ? '1' : undefined}>
      <Head m={m} view={view} onOpen={onOpen} />

      {view === 'large' && <>
        <div className="nc-addr"><span className="ellipsis">{m.ip}</span>{ipMarks(m)}</div>
        <dl className="nc-fields">
          <LabelValue label="入口">{entryText(m)}</LabelValue>
          <LabelValue label="套餐">{m.plan?.name ?? <span className="t-muted">未设置</span>}</LabelValue>
          <LabelValue label="带宽">{m.plan ? <Num f={F.mbps(m.plan.bandwidth_mbps)} /> : '—'}</LabelValue>
          <LabelValue label="流量上限">{q.limit > 0 ? <Num f={F.gb(q.limit)} /> : '不限'}</LabelValue>
        </dl>
        <Meters m={m} plans={plans} />
        <section className="slot-seg">
          {host ? <>
            <div className="nc-seg-title">网卡</div>
            <div className="nc-nic">
              <div className="nc-rate" title="网卡接收速率"><ArrowDownToLine /><Num f={F.rate(m.host!.net_rx_bps)} /></div>
              <div className="nc-rate" title="网卡发送速率"><ArrowUpFromLine /><Num f={F.rate(m.host!.net_tx_bps)} /></div>
              <div className="nc-total"><span>本月下行</span><Num f={F.bytes(t.down)} /></div>
              <div className="nc-total"><span>本月上行</span><Num f={F.bytes(t.up)} /></div>
            </div>
          </> : <>
            <div className="nc-seg-title"><span>业务流量</span><Chip variant="neutral">{m.kind === 'proxy' ? '无主机' : '无数据'}</Chip></div>
            <EntryTable m={m} />
          </>}
        </section>
        <section className="slot-seg">
          <div className="nc-qhead">
            <span className="nc-qlabel"><Database />流量上限{q.rem != null && <span style={{ fontWeight: 400 }}> {q.rem > 0 ? <>剩余 <Num f={F.bytes(q.rem)} /></> : <span style={{ color: 'var(--bad-text)' }}>已超限</span>}</span>}</span>
            <span className="nc-quse"><b className="num">{F.bytes(q.used).v}</b> <span className="unit">{F.bytes(q.used).u}</span> / {q.limit > 0 ? <Num f={F.gb(q.limit)} /> : '不限'}</span>
          </div>
          <Quota pct={q.pct} />
        </section>
        <section className="slot-seg"><BeatSlot m={m} enabled={seen} /></section>
        <section className="slot-seg nc-run">
          <div className="nc-run-item"><span className="nc-run-label"><RefreshCw />已上架</span><span className="nc-run-val"><Num f={F.since(m.activated_at)} /></span></div>
          <div className="nc-run-item"><span className="nc-run-label"><Calendar />到期</span><DueVal m={m} /></div>
        </section>
        <div className="nc-tags"><TagsList m={m} /></div>
      </>}

      {view === 'small' && <>
        <div className="nc-meta2">
          <div><span>{m.ip}</span>{ipMarks(m)}</div>
          <div><span>{entryText(m)} · {m.plan?.name ?? '未设置套餐'}</span></div>
        </div>
        <section className="slot-seg"><Meters m={m} plans={plans} compact /></section>
        <CellsRow m={m} />
        <section className="slot-seg">
          <div className="nc-qhead" style={{ marginBottom: 6 }}>
            <span className="nc-qlabel"><Database />流量上限 {q.limit > 0 ? <Num f={F.gb(q.limit)} /> : '不限'}</span>
            <span className="nc-quse"><b className="num">{F.bytes(q.used).v}</b> <span className="unit">{F.bytes(q.used).u}</span></span>
          </div>
          <Quota pct={q.pct} />
        </section>
        <section className="slot-seg"><BeatSlot m={m} enabled={seen} /></section>
      </>}

      {view === 'compact' && <>
        <div className="nc-tags"><TagsList m={m} /></div>
        <section className="slot-seg"><Meters m={m} plans={plans} compact /></section>
        <section className="slot-seg"><FlowRow m={m} /></section>
        <section className="slot-seg"><BeatSlot m={m} enabled={seen} /></section>
      </>}

      <Foot m={m} onOpen={onOpen} />
    </article>
  );
}
export const NodeCard = memo(NodeCardImpl);

/** 骨架卡：同样的槽位数，首屏换成真实卡时不跳 */
export function SkeletonCard({ view }: { view: CardView }) {
  const rows = view === 'large' ? 10 : view === 'small' ? 7 : 6;
  return (
    <article className="ncard is-skel" data-view={view} aria-hidden="true">
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className={i === 0 ? 'nc-head' : 'slot-seg'} style={{ minHeight: i === rows - 1 ? 44 : 40 }}><span className="skel" style={{ width: `${40 + ((i * 17) % 40)}%`, height: 12 }} /></div>
      ))}
    </article>
  );
}
