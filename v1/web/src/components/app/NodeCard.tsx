// 节点卡片：一套外壳、三种密度（大 / 小 / 紧凑）。每种密度的“槽位”固定（大 10 / 小 7 / 紧凑 6），
// 无论机器是主机、家宽代理还是线路机，同一个槽位都渲染同样高度的内容或占位——配合 cards.css 的 subgrid，同一行的卡片分段永远对齐。
// 视觉语言：资源用分段格条（每个指标一个颜色），实时速率是大号读数 + 点阵迹线（实时流喂的环形缓冲，见 lib/live），
// 剩余流量也是格条（绿→黄→红），心跳条同属“格子”语言。迹线与流量可点，直接打开抽屉的“主机 / 流量”页签看历史曲线。
import { memo } from 'react';
import { m as mo } from 'motion/react'; // 别名：组件里的 props 叫 m（机器）
import { SPRING } from '@/lib/motion';
import { ArrowDown, ArrowUp, Calendar, CircleDollarSign, Cpu, Database, Gauge, Globe, HardDrive, MemoryStick, MoreHorizontal, Radar, RefreshCw, Settings2, PanelRightOpen, ServerOff, Wallet } from 'lucide-react';
import type { MachineView, Plan } from '@/lib/types';
import * as F from '@/lib/format';
import { beatCells, entryText, hasHost, isQuiet, liveRate, monthTraffic, probeSummary, quota, remainingValue, resourceMeters, subLabel, tagColor } from '@/lib/derive';
import { useSeries } from '@/lib/queries';
import { useLiveSeries } from '@/lib/live';
import { type DrawerTab } from '@/lib/drawer';
import { useInView } from '@/lib/useInView';
import { KIND_LABEL } from '@/lib/sort';
import { cn } from '@/lib/utils';
import { Beat, HealthPill, KindIcon, LabelValue, Mark, Num } from './primitives';
import { CellMeter, Cells, DotTrace, QUOTA_COLORS, Tag } from './viz';
import { Button, IconButton } from '@/components/ui/button';
import { ContextMenu, ContextMenuContent, ContextMenuTrigger } from '@/components/ui/context-menu';
import { DropdownMenu, DropdownMenuContent, DropdownMenuTrigger, Tip } from '@/components/ui/overlays';
import { NodeContextItems, NodeDropdownItems } from './NodeMenu';

export type CardView = 'large' | 'small' | 'compact';
type Open = (tab?: DrawerTab) => void;

export function StatusDot({ m }: { m: MachineView }) {
  const s = m.health.status;
  const tone = s === 'down' ? 'bad' : s === 'degraded' ? 'warn' : s === 'maintenance' ? 'info' : s === 'ok' ? undefined : 'neutral';
  return <span className="dot" data-tone={tone} />;
}

function Head({ m, view, onOpen }: { m: MachineView; view: CardView; onOpen: Open }) {
  return (
    <header className="nc-head">
      <StatusDot m={m} /><KindIcon kind={m.kind} />
      <Tip text={`打开 ${m.name} 的详情`}><button type="button" className="nc-name" onClick={() => onOpen()}>{m.name}</button></Tip>
      {view === 'compact' && <><span className="nc-ipsep">·</span><span className="nc-ip">{m.ip}</span></>}
      <div className="nc-actions">
        <IconButton label="打开详情" onClick={() => onOpen()}><PanelRightOpen size={16} /></IconButton>
        <DropdownMenu>
          <DropdownMenuTrigger asChild><IconButton label="更多操作" tip={false}><MoreHorizontal size={16} /></IconButton></DropdownMenuTrigger>
          <DropdownMenuContent align="end"><NodeDropdownItems m={m} /></DropdownMenuContent>
        </DropdownMenu>
      </div>
    </header>
  );
}
export const IpTags = ({ m }: { m: MachineView }) => (
  <span className="nc-marks">{m.ip.includes(':') ? <Tag c="blue">V6</Tag> : <Tag c="green">V4</Tag>}{m.ssh_port !== 22 && <Mark title="SSH 端口">SSH {m.ssh_port}</Mark>}</span>
);

function Meters({ m, plans, compact }: { m: MachineView; plans: Plan[]; compact?: boolean }) {
  const r = resourceMeters(m, plans);
  if (!r) return <div className="nc-res"><div className="nc-hold"><ServerOff />{m.kind === 'proxy' ? '家宽代理没有主机，无 CPU / 内存指标' : m.lifecycle === 'provisioning' ? '上架完成后开始采集' : '暂无主机指标'}</div></div>;
  const quiet = isQuiet(m);
  return (
    <div className="nc-res">
      <CellMeter metric="cpu" icon={<Cpu />} label="CPU" value={r.cpu.value} pct={r.cpu.pct} sub={compact ? undefined : r.cpu.sub} empty={quiet} />
      <CellMeter metric="mem" icon={<MemoryStick />} label="内存" value={r.mem.value} pct={r.mem.pct} sub={compact ? undefined : r.mem.sub} empty={quiet} />
      <CellMeter metric="disk" icon={<HardDrive />} label="磁盘" value={r.disk.value} pct={r.disk.pct} sub={compact ? undefined : r.disk.sub} empty={quiet} />
      <CellMeter metric="load" icon={<Gauge />} label="负载" value={r.load.value} pct={r.load.pct} sub={compact ? undefined : r.load.sub} empty={quiet} title={r.load.hint} />
    </div>
  );
}

/** 实时速率一列：方向 + 大号读数（按量级着色）、点阵迹线（点击看主机曲线）、本月累计（点击看流量页签） */
function FlowCol({ dir, rate, values, total, live, onOpen }: { dir: 'up' | 'down'; rate: number | null; values: number[]; total?: number; live: boolean; onOpen: Open }) {
  const up = dir === 'up';
  return (
    <div className="flow-col" data-dir={dir} data-speed={live ? F.speedOf(rate) : undefined}>
      <div className="flow-head">
        <span className="flow-label">{up ? <ArrowUp /> : <ArrowDown />}{up ? '上行' : '下行'}</span>
        <span className="flow-rate"><Num f={F.rate(rate)} /></span>
      </div>
      <Tip text="查看主机曲线"><button type="button" className="flow-trend" onClick={() => onOpen('host')} aria-label={`${up ? '上行' : '下行'}速率：查看主机曲线`}>
        <DotTrace values={values} />
        <span className="flow-live" data-live={live ? '' : undefined}><i />{live ? '实时' : '离线'}</span>
      </button></Tip>
      {total != null && (
        <button type="button" className="flow-foot" onClick={() => onOpen('traffic')} aria-label={`本月${up ? '出站' : '入站'}：查看流量页签`}>
          <span className="flow-foot-label"><Globe />本月{up ? '出站' : '入站'}</span><Num f={F.bytes(total)} />
        </button>
      )}
    </div>
  );
}

function Flow({ m, onOpen, totals }: { m: MachineView; onOpen: Open; totals?: boolean }) {
  const tr = useLiveSeries(m.id);
  const r = liveRate(m); const t = monthTraffic(m);
  return (
    <div className="flow">
      <FlowCol dir="up" rate={r?.tx ?? null} values={tr.map((p) => p.tx)} total={totals ? t.up : undefined} live={r != null} onOpen={onOpen} />
      <FlowCol dir="down" rate={r?.rx ?? null} values={tr.map((p) => p.rx)} total={totals ? t.down : undefined} live={r != null} onOpen={onOpen} />
    </div>
  );
}

/** 无主机（家宽代理）：业务流量小表 */
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
/** 无主机（小卡片 / 紧凑）：本月业务 + 探测延迟两列 */
function FlowRow({ m }: { m: MachineView }) {
  const t = monthTraffic(m); const ps = probeSummary(m);
  const col = (title: string, a: React.ReactNode, b: React.ReactNode, ia: React.ReactNode, ib: React.ReactNode) => (
    <div><div className="nc-flow2-title">{title}</div><div className="nc-flow2-row">{ia}{a}</div><div className="nc-flow2-row">{ib}{b}</div></div>
  );
  return (
    <div className="nc-flow2">
      {col('本月业务', <Num f={F.bytes(t.down)} />, <Num f={F.bytes(t.up)} />, <ArrowDown />, <ArrowUp />)}
      {col('TCP · 出口', <Num f={F.ms(ps.tcp)} />, <Num f={F.ms(ps.egress)} />, <Radar />, <Globe />)}
    </div>
  );
}

/** 剩余流量：标题 + 剩余量，右侧已用 / 上限，下面绿→黄→红的格条（不限额度不画）；点击打开流量页签 */
function QuotaSlot({ m, onOpen }: { m: MachineView; onOpen: Open }) {
  const q = quota(m); const used = F.bytes(q.used);
  return (
    <button type="button" className="nc-quota" onClick={() => onOpen('traffic')} aria-label="剩余流量：查看流量页签">
      <span className="nc-qhead">
        <span className="nc-qlabel"><Database />剩余流量
          <b className={cn('num', q.rem === 0 && 'is-bad')}>{q.rem == null ? '不限' : q.rem > 0 ? <Num f={F.bytes(q.rem)} /> : '已超限'}</b>
        </span>
        <span className="nc-quse">已用 <span className="num">{used.v} {used.u}</span>{q.limit > 0 && <> / <Num f={F.gb(q.limit)} /></>}</span>
      </span>
      {q.rem != null && <Cells pct={q.pct} colors={QUOTA_COLORS} />}
    </button>
  );
}

function BeatSlot({ m, enabled }: { m: MachineView; enabled: boolean }) {
  const live = m.lifecycle === 'active';
  const s = useSeries(m.id, 'probe', '1h', enabled && live, 120_000);
  const ps = probeSummary(m);
  const cap = !live ? <span className="t-muted">{m.lifecycle === 'retired' ? '已退役' : '尚未开始探测'}</span>
    : m.kind === 'relay' ? <>入口连通 <b className="num" style={ps.tcpOk < ps.total ? { color: 'var(--warn-text)' } : undefined}>{ps.tcpOk}/{ps.total}</b></>
    : <>TCP <b className="num">{ps.tcp != null ? `${ps.tcp} ms` : '—'}</b> · 出口 <b className="num">{ps.egress != null ? `${ps.egress} ms` : '—'}</b></>;
  return <Beat cells={live ? beatCells(s.data?.points) : []} caption={cap} win="近 1 小时" stale={isQuiet(m)} />;
}

const dueTone = (m: MachineView) => { const t = F.dueFmt(m.subscription?.due_days).tone; return t || (m.subscription ? 'ok' : ''); };

/** 已上架 · 到期 · 剩余价值：彩色读数 */
function RunRow({ m }: { m: MachineView }) {
  const rv = remainingValue(m);
  return (
    <section className="slot-seg nc-run">
      <div className="nc-run-item"><span className="nc-run-label"><RefreshCw />已上架</span><span className="nc-run-val" data-c="up"><Num f={F.since(m.activated_at)} /></span></div>
      <div className="nc-run-item"><span className="nc-run-label"><Calendar />距到期</span><span className="nc-run-val" data-c={dueTone(m)}><Num f={F.dueFmt(m.subscription?.due_days).text} /></span></div>
      <div className="nc-run-item"><span className="nc-run-label"><Wallet />剩余价值</span><span className="nc-run-val">{rv ? <span className="num">{F.money(rv.cents, rv.currency)}</span> : <span className="t-muted">—</span>}</span></div>
    </section>
  );
}

const KIND_TAG = { relay: 'teal', proxy: 'orange', vps: 'violet' } as const;
function Tags({ m, price = true }: { m: MachineView; price?: boolean }) {
  const p = subLabel(m);
  return (
    <>
      <div className="nc-tags-list">
        {m.provider_code && <Tag c={tagColor(m.provider_code)} title={m.provider?.name}>{m.provider_code}</Tag>}
        <Tag c={KIND_TAG[m.kind]}>{KIND_LABEL[m.kind]}</Tag>
        {m.service_ref && <Tag c={tagColor(m.service_ref)}>{m.service_ref}</Tag>}
        {m.maintenance && <Tag c="amber">维护中</Tag>}
      </div>
      {price && p && <span className="nc-price"><CircleDollarSign /><span className="num">{p}</span></span>}
    </>
  );
}

function Foot({ m, onOpen }: { m: MachineView; onOpen: Open }) {
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

/** 卡片只吃稳定的 props（plans 引用、onOpen 回调不随地址变化），配合 memo，抽屉开关或 tick 时只有变了的那张重渲染。
 *  layoutKey：筛选/排序/视图的签名，变了才量位置做布局动画（layoutDependency），平时重渲染不量 */
function NodeCardImpl({ m, view, current, plans, onOpen: open, layoutKey }: { m: MachineView; view: CardView; current?: boolean; plans: Plan[]; onOpen: (id: string, tab?: DrawerTab) => void; layoutKey: string }) {
  const [ref, visible] = useInView<HTMLElement>(); // 心跳条只在卡片可见时轮询，滚出视口就停
  const onOpen: Open = (tab) => open(m.id, tab);
  const host = hasHost(m);
  const quiet = isQuiet(m);
  const due = F.dueFmt(m.subscription?.due_days);

  return (
    <ContextMenu>
    <ContextMenuTrigger asChild>
    <mo.article ref={ref} layout="position" layoutDependency={layoutKey} initial={{ opacity: 0 }} animate={{ opacity: 1 }} transition={{ layout: SPRING, opacity: { duration: 0.18 } }} className={cn('ncard', current && 'is-current', quiet && 'is-quiet')} data-view={view} data-id={m.id} data-kind={m.kind} data-health={m.health.status} data-offline={quiet ? '1' : undefined}>
      <Head m={m} view={view} onOpen={onOpen} />

      {view === 'large' && <>
        <div className="nc-addr"><span className="ellipsis">{m.ip}</span><IpTags m={m} /></div>
        <dl className="nc-fields">
          <LabelValue label="入口">{entryText(m)}</LabelValue>
          <LabelValue label="套餐">{m.plan?.name ?? <span className="t-muted">未设置</span>}</LabelValue>
          <LabelValue label="带宽">{m.plan ? <Num f={F.mbps(m.plan.bandwidth_mbps)} /> : '—'}</LabelValue>
          <LabelValue label="流量上限">{quota(m).limit > 0 ? <Num f={F.gb(quota(m).limit)} /> : '不限'}</LabelValue>
        </dl>
        <Meters m={m} plans={plans} />
        <section className="slot-seg">{host ? <Flow m={m} onOpen={onOpen} totals /> : <><div className="nc-seg-title"><span>业务流量</span><Tag c="gray">{m.kind === 'proxy' ? '无主机' : '无数据'}</Tag></div><EntryTable m={m} /></>}</section>
        <section className="slot-seg"><QuotaSlot m={m} onOpen={onOpen} /></section>
        <section className="slot-seg"><BeatSlot m={m} enabled={visible} /></section>
        <RunRow m={m} />
        <div className="nc-tags"><Tags m={m} /></div>
      </>}

      {view === 'small' && <>
        <div className="nc-meta2">
          <div><span>{m.ip}</span><IpTags m={m} /></div>
          <div><span>{entryText(m)} · {m.plan?.name ?? '未设置套餐'}</span><span className="nc-due" data-c={dueTone(m)}>距到期 <Num f={due.text} /></span></div>
        </div>
        <section className="slot-seg"><Meters m={m} plans={plans} compact /></section>
        <section className="slot-seg">{host ? <Flow m={m} onOpen={onOpen} /> : <FlowRow m={m} />}</section>
        <section className="slot-seg"><QuotaSlot m={m} onOpen={onOpen} /></section>
        <section className="slot-seg"><BeatSlot m={m} enabled={visible} /></section>
      </>}

      {view === 'compact' && <>
        <div className="nc-tags"><Tags m={m} /></div>
        <section className="slot-seg"><Meters m={m} plans={plans} compact /></section>
        <section className="slot-seg">{host ? <Flow m={m} onOpen={onOpen} /> : <FlowRow m={m} />}</section>
        <section className="slot-seg"><BeatSlot m={m} enabled={visible} /></section>
      </>}

      <Foot m={m} onOpen={onOpen} />
    </mo.article>
    </ContextMenuTrigger>
    <ContextMenuContent><NodeContextItems m={m} /></ContextMenuContent>
    </ContextMenu>
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
