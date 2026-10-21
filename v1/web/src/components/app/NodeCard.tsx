// 节点卡片：一套外壳、三种密度（大 / 小 / 紧凑）。每种密度的“槽位”固定（大 10 / 小 7 / 紧凑 6），
// 无论机器是主机、家宽代理还是线路机，同一个槽位都渲染同样高度的内容或占位——配合 cards.css 的 subgrid，同一行的卡片分段永远对齐。
// 视觉语言：资源用分段格条（每个指标一个颜色），实时速率是大号读数 + 点阵迹线（实时流喂的环形缓冲，见 lib/live），
// 剩余流量也是格条（绿→黄→红），心跳条同属“格子”语言。迹线与流量可点，直接打开抽屉的“主机 / 流量”页签看历史曲线。
import { memo, useCallback, useEffect, useMemo, useState, useSyncExternalStore } from 'react';
import { m as mo } from 'motion/react'; // 别名：组件里的 props 叫 m（机器）
import { SPRING } from '@/lib/motion';
import { ArrowDown, ArrowUp, Calendar, CircleDollarSign, Cpu, Database, Gauge, Globe, HardDrive, MemoryStick, MoreHorizontal, Radar, RefreshCw, Settings2, PanelRightOpen, ServerOff, Wallet } from 'lucide-react';
import type { MachineView, ProbeBeats, Subscription } from '@/lib/types';
import * as F from '@/lib/format';
import { beatCells, entryText, hasHost, isQuiet, liveRate, monthTraffic, probeSummary, quota, remainingValue, resourceMeters, subLabel, tagColor } from '@/lib/derive';
import { useProbeBeats } from '@/lib/queries';
import { useLiveSeries } from '@/lib/live';
import { useNow } from '@/lib/useNow';
import { type DrawerTab } from '@/lib/drawer';
import { KIND_LABEL } from '@/lib/sort';
import { cn } from '@/lib/utils';
import { Beat, Due, HealthPill, KindIcon, LabelValue, Mark, Num, StatusDot } from './primitives';
import { CellMeter, Cells, DotTrace, QUOTA_COLORS, Tag } from './viz';
import { Button, IconButton } from '@/components/ui/button';
import { ContextMenu, ContextMenuContent, ContextMenuTrigger } from '@/components/ui/context-menu';
import { DropdownMenu, DropdownMenuContent, DropdownMenuTrigger, Tip } from '@/components/ui/overlays';
import { NodeContextItems, NodeDropdownItems } from './NodeMenu';

export type CardView = 'large' | 'small' | 'compact';
type Open = (tab?: DrawerTab) => void;

// 卡片随实时推送重渲染（m 换了引用）。静态的几块（标题、地址与字段、标签、运行行、footer、心跳条）各自 memo、只收用到的字段，
// 推送只重画读数（资源格条、速率、剩余流量）。

/** 卡片最新的机器：Head 是 memo 的，“⋯”菜单打开期间靠订阅它跟上推送 */
interface Latest { get: () => MachineView; sub: (f: () => void) => () => void }
function useLatest(m: MachineView): Latest {
  const [s] = useState(() => {
    let cur = m; const subs = new Set<() => void>();
    return { get: () => cur, sub: (f: () => void) => { subs.add(f); return () => { subs.delete(f); }; }, set: (v: MachineView) => { cur = v; subs.forEach((f) => f()); } };
  });
  useEffect(() => { s.set(m); }, [s, m]);
  return s;
}

/** 标题：圆点 + 类型标 + 名称 + 右侧操作。“⋯”菜单的内容只在打开时挂载 */
const Head = memo(function Head({ name, kind, ip, status, view, onOpen, menu }: { name: string; kind: MachineView['kind']; ip: string; status: string; view: CardView; onOpen: Open; menu: Latest }) {
  return (
    <header className="nc-head">
      <StatusDot status={status} /><KindIcon kind={kind} />
      <Tip text={`打开 ${name} 的详情`}><button type="button" className="nc-name" onClick={() => onOpen()}>{name}</button></Tip>
      {view === 'compact' && <><span className="nc-ipsep">·</span><span className="nc-ip">{ip}</span></>}
      <div className="nc-actions">
        <IconButton label="打开详情" onClick={() => onOpen()}><PanelRightOpen size={16} /></IconButton>
        <DropdownMenu>
          <DropdownMenuTrigger asChild><IconButton label="更多操作" tip={false}><MoreHorizontal size={16} /></IconButton></DropdownMenuTrigger>
          <DropdownMenuContent align="end"><MenuItems menu={menu} /></DropdownMenuContent>
        </DropdownMenu>
      </div>
    </header>
  );
});
const MenuItems = ({ menu }: { menu: Latest }) => <NodeDropdownItems m={useSyncExternalStore(menu.sub, menu.get)} />;

const IpTags = ({ ip, sshPort }: { ip: string; sshPort: number }) => (
  <span className="nc-marks">{ip.includes(':') ? <Tag c="blue">V6</Tag> : <Tag c="green">V4</Tag>}{sshPort !== 22 && <Mark title="SSH 端口">SSH {sshPort}</Mark>}</span>
);
interface Info { ip: string; sshPort: number; entry: string; plan: MachineView['plan'] }
/** 大卡片：地址行 + 入口 / 套餐 / 套餐带宽 / 流量上限 2×2 */
const LargeInfo = memo(function LargeInfo({ ip, sshPort, entry, plan, limit }: Info & { limit: number }) {
  return <>
    <div className="nc-addr"><span className="ellipsis">{ip}</span><IpTags ip={ip} sshPort={sshPort} /></div>
    <dl className="nc-fields">
      <LabelValue label="入口">{entry}</LabelValue>
      <LabelValue label="套餐">{plan?.name ?? <span className="t-muted">未设置</span>}</LabelValue>
      <LabelValue label="套餐带宽">{plan ? <Num f={F.mbps(plan.bandwidth_mbps)} /> : '—'}</LabelValue>
      <LabelValue label="流量上限"><Num f={F.gb(limit)} /></LabelValue>
    </dl>
  </>;
});
/** 小卡片：地址 + 入口 · 套餐 · 到期两行 */
const SmallInfo = memo(function SmallInfo({ ip, sshPort, entry, plan, dueDays }: Info & { dueDays: number | undefined }) {
  return (
    <div className="nc-meta2">
      <div><span>{ip}</span><IpTags ip={ip} sshPort={sshPort} /></div>
      <div><span>{entry} · {plan?.name ?? '未设置套餐'}</span><span className="nc-due"><Due days={dueDays} tail /></span></div>
    </div>
  );
});

function Meters({ m, compact }: { m: MachineView; compact?: boolean }) {
  const r = resourceMeters(m);
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

/** 网卡速率一列：方向 + 大号读数（按量级着色）、点阵迹线（点击看主机曲线） */
function FlowCol({ dir, rate, values, live, onOpen }: { dir: 'up' | 'down'; rate: number | null; values: number[]; live: boolean; onOpen: Open }) {
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
    </div>
  );
}

function Flow({ m, onOpen, totals }: { m: MachineView; onOpen: Open; totals?: boolean }) {
  const tr = useLiveSeries(m.id);
  const tx = useMemo(() => tr.map((p) => p.tx), [tr]); const rx = useMemo(() => tr.map((p) => p.rx), [tr]); // 迹线没变时 DotTrace（memo）不重画
  const r = liveRate(m); const t = monthTraffic(m);
  return (
    <div className="flow">
      <FlowCol dir="up" rate={r?.tx ?? null} values={tx} live={r != null} onOpen={onOpen} />
      <FlowCol dir="down" rate={r?.rx ?? null} values={rx} live={r != null} onOpen={onOpen} />
      {/* 业务流量按用户视角（下载 = downlink、上传 = uplink），和本机网卡的上行 / 下行不是一回事，单独一行 */}
      {totals && (
        <button type="button" className="flow-foot flow-biz" onClick={() => onOpen('traffic')}>
          <span className="flow-foot-label"><Globe />本月业务</span>
          <span>下载 <Num f={F.bytes(t.down)} /> · 上传 <Num f={F.bytes(t.up)} /></span><span className="sr-only">（查看流量页签）</span>
        </button>
      )}
    </div>
  );
}

/** 无主机（家宽代理）：业务流量小表 */
function EntryTable({ m }: { m: MachineView }) {
  const t = monthTraffic(m);
  const row = (label: string, d: number, u: number) => <tr><th>{label}</th><td><Num f={F.bytes(d)} /></td><td><Num f={F.bytes(u)} /></td></tr>;
  return (
    <table className="nc-etable">
      <thead><tr><th>周期</th><th>↓ 下载</th><th>↑ 上传</th></tr></thead>
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

/** 剩余流量：标题 + 剩余量，右侧本周期已用 / 上限（周期按流量重置日），下面绿→黄→红的格条（不限额度不画）；点击打开流量页签 */
function QuotaSlot({ m, onOpen }: { m: MachineView; onOpen: Open }) {
  const q = quota(m);
  return (
    <button type="button" className="nc-quota" onClick={() => onOpen('traffic')}>
      <span className="nc-qhead">
        <span className="nc-qlabel"><Database />剩余流量
          <b className={cn('num', q.rem === 0 && 'is-bad')}>{q.rem == null ? '不限' : q.rem > 0 ? <Num f={F.bytes(q.rem)} /> : '已超限'}</b>
        </span>
        <span className="nc-quse" title={`周期 ${q.start.slice(5)} 起`}>本周期已用 <Num f={F.bytes(q.used)} />{q.limit > 0 && <> / <Num f={F.gb(q.limit)} /></>}</span>
      </span>
      {q.rem != null && <Cells pct={q.pct} colors={QUOTA_COLORS} />}
      <span className="sr-only">（查看流量页签）</span>
    </button>
  );
}

/** 心跳条：近 1 小时的探测（全部卡片共用一个 /probe-beats 查询，各取自己的那份） */
const BeatSlot = memo(function BeatSlot({ id, kind, lifecycle, quiet, tcpOk, total, tcp, egress }: { id: string; kind: MachineView['kind']; lifecycle: MachineView['lifecycle']; quiet: boolean; tcpOk: number; total: number; tcp: number | null; egress: number | null }) {
  const live = lifecycle === 'active';
  const beats = useProbeBeats(useCallback((d: ProbeBeats) => d[id], [id])).data;
  const cap = !live ? <span className="t-muted">{lifecycle === 'retired' ? '已退役' : '尚未开始探测'}</span>
    : kind === 'relay' ? <>入口连通 <b className="num" style={tcpOk < total ? { color: 'var(--warn-text)' } : undefined}>{tcpOk}/{total}</b></>
    : <>TCP <b><Num f={F.ms(tcp)} /></b> · 出口 <b><Num f={F.ms(egress)} /></b></>;
  return <Beat cells={live ? beatCells(beats) : []} caption={cap} win="近 1 小时" stale={quiet} />;
});

/** 已上架 · 到期 · 剩余价值：彩色读数。已上架按分钟走（memo 挡住推送，自己每分钟重画） */
const RunRow = memo(function RunRow({ activated, sub }: { activated: string | null; sub: Subscription | null }) {
  const now = useNow(true, 60_000);
  const rv = remainingValue(sub);
  return (
    <section className="slot-seg nc-run">
      <div className="nc-run-item"><span className="nc-run-label"><RefreshCw />已上架</span><span className="nc-run-val" data-c="up"><Num f={F.since(activated, now)} /></span></div>
      <div className="nc-run-item"><span className="nc-run-label"><Calendar />到期</span><span className="nc-run-val"><Due days={sub?.due_days} /></span></div>
      <div className="nc-run-item"><span className="nc-run-label"><Wallet />剩余价值</span><span className="nc-run-val">{rv ? <span className="num">{F.money(rv.cents, rv.currency)}</span> : <span className="t-muted">—</span>}</span></div>
    </section>
  );
});

const KIND_TAG = { relay: 'teal', proxy: 'orange', vps: 'violet' } as const;
const Tags = memo(function Tags({ code, provider, kind, service, maintenance, price }: { code: string | null; provider?: string; kind: MachineView['kind']; service: string | null; maintenance: boolean; price: string | null }) {
  return (
    <div className="nc-tags">
      <div className="nc-tags-list">
        {code && <Tag c={tagColor(code)} title={provider}>{code}</Tag>}
        <Tag c={KIND_TAG[kind]}>{KIND_LABEL[kind]}</Tag>
        {service && <Tag c={tagColor(service)}>{service}</Tag>}
        {maintenance && <Tag c="amber">维护中</Tag>}
      </div>
      {price && <span className="nc-price"><CircleDollarSign /><span className="num">{price}</span></span>}
    </div>
  );
});

const Foot = memo(function Foot({ health, lifecycle, maintenance, onOpen }: Pick<MachineView, 'health' | 'lifecycle' | 'maintenance'> & { onOpen: Open }) {
  return (
    <footer className="nc-foot">
      <HealthPill m={{ health, lifecycle, maintenance }} />
      <div className="nc-foot-right">
        <Button variant="text" size="sm" onClick={() => onOpen()}><Settings2 size={14} /><span>详情</span></Button>
        <Button variant="text" size="sm" onClick={() => onOpen('manage')}><span>管理</span></Button>
      </div>
    </footer>
  );
});

/** 卡片只吃稳定的 props（onOpen 回调不随地址变化），配合 memo，抽屉开关时只有变了的那张重渲染。
 *  layoutKey：筛选/排序/视图的签名，变了才量位置做布局动画（layoutDependency），平时重渲染不量 */
function NodeCardImpl({ m, view, current, onOpen: open, layoutKey }: { m: MachineView; view: CardView; current?: boolean; onOpen: (id: string, tab?: DrawerTab) => void; layoutKey: string }) {
  const onOpen = useCallback<Open>((tab) => open(m.id, tab), [open, m.id]);
  const menu = useLatest(m);
  const host = hasHost(m);
  const quiet = isQuiet(m);
  const info: Info = { ip: m.ip, sshPort: m.ssh_port, entry: entryText(m), plan: m.plan };
  const tags = <Tags code={m.provider_code} provider={m.provider?.name} kind={m.kind} service={m.service_ref} maintenance={m.maintenance} price={subLabel(m)} />;
  const beat = <section className="slot-seg"><BeatSlot id={m.id} kind={m.kind} lifecycle={m.lifecycle} quiet={quiet} {...probeSummary(m)} /></section>;

  return (
    <ContextMenu>
    <ContextMenuTrigger asChild>
    <mo.article layout="position" layoutDependency={layoutKey} initial={{ opacity: 0 }} animate={{ opacity: 1 }} transition={{ layout: SPRING, opacity: { duration: 0.18 } }} className={cn('ncard', current && 'is-current', quiet && 'is-quiet')} data-view={view} data-id={m.id} data-kind={m.kind} data-health={m.health.status} data-offline={quiet ? '1' : undefined}>
      <Head name={m.name} kind={m.kind} ip={m.ip} status={m.health.status} view={view} onOpen={onOpen} menu={menu} />

      {view === 'large' && <>
        <LargeInfo {...info} limit={quota(m).limit} />
        <Meters m={m} />
        <section className="slot-seg">{host ? <Flow m={m} onOpen={onOpen} totals /> : <><div className="nc-seg-title"><span>业务流量</span><Tag c="gray">{m.kind === 'proxy' ? '无主机' : '无数据'}</Tag></div><EntryTable m={m} /></>}</section>
        <section className="slot-seg"><QuotaSlot m={m} onOpen={onOpen} /></section>
        {beat}
        <RunRow activated={m.activated_at} sub={m.subscription} />
        {tags}
      </>}

      {view === 'small' && <>
        <SmallInfo {...info} dueDays={m.subscription?.due_days} />
        <section className="slot-seg"><Meters m={m} compact /></section>
        <section className="slot-seg">{host ? <Flow m={m} onOpen={onOpen} /> : <FlowRow m={m} />}</section>
        <section className="slot-seg"><QuotaSlot m={m} onOpen={onOpen} /></section>
        {beat}
      </>}

      {view === 'compact' && <>
        {tags}
        <section className="slot-seg"><Meters m={m} compact /></section>
        <section className="slot-seg">{host ? <Flow m={m} onOpen={onOpen} /> : <FlowRow m={m} />}</section>
        {beat}
      </>}

      <Foot health={m.health} lifecycle={m.lifecycle} maintenance={m.maintenance} onOpen={onOpen} />
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
