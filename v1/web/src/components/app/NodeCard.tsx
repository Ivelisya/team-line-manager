// 节点卡片：一套外壳、三种密度（详细 / 标准 / 紧凑，内部值 large / small / compact）。每种密度的“槽位”固定（详细 10 / 标准 7 / 紧凑 6），
// 无论机器是主机、家宽代理还是线路机，同一个槽位都渲染同样高度的内容或占位——配合 cards.css 的 subgrid，同一行的卡片分段永远对齐。
// 视觉语言：资源用分段格条（每个指标一个颜色），实时速率是大号读数 + 点阵迹线（实时流喂的环形缓冲，见 lib/live），
// 剩余流量也是格条（绿→黄→红），心跳条同属“格子”语言。读数、迹线、流量、心跳条都可点，直接打开抽屉对应的页签看历史曲线。
// 家宽代理没有主机：资源格条的位置放本月业务流量，速率的位置放探测延迟。
import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { flushSync } from 'react-dom';
import { m as mo } from 'motion/react'; // 别名：组件里的 props 叫 m（机器）
import { SPRING } from '@/lib/motion';
import { ArrowDown, ArrowDownUp, ArrowUp, Calendar, CircleDollarSign, Cpu, Database, Gauge, Globe, HardDrive, MemoryStick, MoreHorizontal, Radar, RefreshCw, Settings2, PanelRightOpen, ServerOff, Wallet } from 'lucide-react';
import type { DueState, MachineView, ProbeBeats, Subscription } from '@/lib/types';
import * as F from '@/lib/format';
import { BEAT_LEGEND, REVERSE_TIP, beatCells, beatRate, beatTitles, dueTone, entryText, errText, hasHost, hostRel, isQuiet, limitGb, liveRate, monthFlow, monthFlowTip, monthTraffic, priceHint, probeSummary, quota, quotaForecast, quotaGb, quotaTip, quotaUsedLabel, remainingValue, resourceMeters, subLabel, trafficTotal, beatLegendRelay, linkCounts, reachWho, reachWin, relayRttMs, reasonText, unreachTip, LINK_LABEL } from '@/lib/derive';
import { useProbeBeats } from '@/lib/queries';
import { useReachLabel } from '@/lib/probePoints';
import { useLiveSeries } from '@/lib/live';
import { useNow } from '@/lib/useNow';
import { sharedObserver, watchSize } from '@/lib/sharedObserver';
import { type DrawerTab } from '@/lib/drawer';
import { cn } from '@/lib/utils';
import { Beat, Due, Flag, FromTag, HealthPill, LabelValue, LinkCounts, Mark, Num, Pill, recycleNote, type RecycleNote } from './primitives';
import { MachineTag, machineTagItems } from './MachineTags';
import { PingRows } from './PingRows';
import { CellMeter, Cells, DotTrace, QUOTA_COLORS, Tag } from './viz';
import { Button, IconButton } from '@/components/ui/button';

export type CardView = 'large' | 'small' | 'compact';
/** 首页按这些排序时，卡片上露出排序依据（详细卡、标准卡本来就显示到期的不重复） */
export type CardShow = 'due' | 'traffic';
type Open = (tab?: DrawerTab) => void;
type Paused = { readonly current: boolean };
/** 「仅看异常」里恢复正常、还留在原位的那张卡（首页传 recovered）的「已恢复」标记的说明，环形卡共用 */
export const RECOVERED_TIP = '开着「仅看异常」时，恢复正常的留在原位；关掉筛选或换分组后不再列出';

// 卡片随实时推送重渲染（m 换了引用）。静态的几块（标题、地址与字段、标签、运行行、footer、心跳条）各自 memo、只收用到的字段，
// 推送只重画读数（资源格条、速率、剩余流量）。滚出视口（含上下 200px 余量）的卡冻结在最后一份数据上，回到视口时换成最新的（见 NodeCardImpl）。
// 卡片里不挂 Radix 根：右键菜单与“⋯”下拉由外面的 NodeMenus 整个网格共用一份（按 data-id / data-node-menu 找到这张卡），
// 只读的悬停说明写 data-tip，由全站一份 TipHost 显示。

/** 标题：国旗 + 名称 + 右侧操作（状态只看底部的胶囊，名称旁不再重复标）。紧凑卡的 IP 在名称下面一行小字（同表格名称列） */
const Head = memo(function Head({ name, from, country, manual, ip, view, onOpen }: { name: string; from: string | null; country: string | null | undefined; manual: boolean; ip: string; view: CardView; onOpen: Open }) {
  const btn = <button type="button" className="nc-name" data-tip={from ? `打开「${name}」（原 ${from}）的详情` : `打开「${name}」的详情`} onClick={() => onOpen()}>{name}</button>;
  return (
    <header className="nc-head">
      <Flag country={country} manual={manual} ip={ip} />
      {view === 'compact' ? <div className="nc-title">{btn}<span className="nc-ip" data-tip={ip}>{ip}</span></div> : <>{btn}{from && <span className="nc-from">原 {from}</span>}</>}
      {/* 打开详情：点名字或底部「详情」，标题行只留「⋯」 */}
      <div className="nc-actions">
        <IconButton label="更多操作" tip={false} data-node-menu="" aria-haspopup="menu"><MoreHorizontal size={16} /></IconButton>
      </div>
    </header>
  );
});

/** IP 后面的标：IP 版本、非默认的 SSH 端口（同机关系在标签行，见 MachineTags） */
const IpTags = ({ ip, sshPort }: Pick<Info, 'ip' | 'sshPort'>) => (
  <span className="nc-marks">{ip.includes(':') ? <Tag c="blue">IPv6</Tag> : <Tag c="green">IPv4</Tag>}{sshPort !== 22 && <Mark tip="SSH 端口">SSH {sshPort}</Mark>}</span>
);
interface Info { id: string; ip: string; sshPort: number; entry: string; plan: MachineView['plan']; host: MachineView['host_machine']; nodes: MachineView['ip_nodes'] }
/** 详细卡：地址行 + 入口 / 套餐 / 套餐带宽 / 流量上限 2×2。同机 IP 节点不带套餐，自己没设上限时额度与宿主机共用 */
const LargeInfo = memo(function LargeInfo({ entry, plan, limit, unset, ...a }: Info & { limit: number; unset: boolean }) {
  return <>
    <div className="nc-addr"><span className="ellipsis" data-tip={a.ip}>{a.ip}</span><IpTags {...a} /></div>
    <dl className="nc-fields">
      <LabelValue plain label="入口">{entry}</LabelValue>
      <LabelValue plain label="套餐">{plan?.name ?? <span className="t-muted">{a.host ? '不带套餐' : '未设置'}</span>}</LabelValue>
      <LabelValue plain label="套餐带宽">{plan ? <Num f={F.mbps(plan.bandwidth_mbps)} /> : '—'}</LabelValue>
      <LabelValue plain label="流量上限">{a.host && !(limit > 0) ? '与宿主机共用' : unset ? '未设置' : <Num f={limitGb(limit)} />}</LabelValue>
    </dl>
  </>;
});
/** 标准卡：地址 + 入口 · 套餐 · 到期两行（长了省略，悬停看全文） */
const SmallInfo = memo(function SmallInfo({ entry, plan, dueDays, dueState, recycle, ...a }: Info & { dueDays: number | undefined; dueState: DueState | undefined; recycle: RecycleNote | null }) {
  const meta = `${entry} · ${plan?.name ?? (a.host ? '不带套餐' : '未设置套餐')}`;
  return (
    <div className="nc-meta2">
      <div><span data-tip={a.ip}>{a.ip}</span><IpTags {...a} /></div>
      <div><span data-tip={meta}>{meta}</span><span className="nc-due">{recycle && <span className="nc-recycle" data-tip={recycle.tip}>{recycle.text}</span>}<Due days={dueDays} state={dueState} tail /></span></div>
    </div>
  );
});

/** 资源格条的位置没有读数时的占位（同高）：agent 离线写离线了多久（每分钟走一次），还在上架、还没上报过各一句 */
function Hold({ m }: { m: MachineView }) {
  const now = useNow(true, 60_000);
  const off = m.agent && !m.agent.online;
  const text = off ? (m.agent!.last_report_at ? `agent 离线 ${F.span(now - Date.parse(m.agent!.last_report_at))}` : 'agent 还没上报') : m.lifecycle === 'provisioning' ? '上架完成后才有数据' : '暂无主机数据';
  return <div className="nc-res"><div className="nc-hold"><ServerOff />{text}</div></div>;
}

/** CPU / 内存 / 磁盘 / 负载四条格条：整块可点，打开抽屉的主机页签。数据过期（agent 离线）时换成占位，不显示旧读数 */
function Meters({ m, compact, onOpen }: { m: MachineView; compact?: boolean; onOpen: Open }) {
  const r = resourceMeters(m);
  if (!r || isQuiet(m)) return <Hold m={m} />;
  return (
    <button type="button" className="nc-res nc-click" onClick={() => onOpen('host')}>
      <CellMeter metric="cpu" icon={<Cpu />} label="CPU" value={r.cpu.value} pct={r.cpu.pct} sub={compact ? undefined : r.cpu.sub} />
      <CellMeter metric="mem" icon={<MemoryStick />} label="内存" value={r.mem.value} pct={r.mem.pct} sub={compact ? undefined : r.mem.sub} />
      <CellMeter metric="disk" icon={<HardDrive />} label="磁盘" value={r.disk.value} pct={r.disk.pct} sub={compact ? undefined : r.disk.sub} />
      <CellMeter metric="load" icon={<Gauge />} label="负载" value={r.load.value} pct={r.load.pct} sub={compact ? undefined : r.load.sub} tip={r.load.hint} />
      <span className="sr-only">（查看主机曲线）</span>
    </button>
  );
}

/** 网卡速率一列：方向 + 大号读数（按量级着色）、点阵迹线（点击看主机曲线）。「实时」标两列各一个（保持两列等宽对称）；
 *  不配小圆点，只在悬停卡片时轻轻呼吸（cards.css），平时一页几十张卡不跑无限动画 */
function FlowCol({ dir, rate, values, dots, live, bw, onOpen }: { dir: 'up' | 'down'; rate: number | null; values: number[]; dots?: number; live: boolean; bw: number | null | undefined; onOpen: Open }) {
  const up = dir === 'up';
  return (
    <div className="flow-col" data-dir={dir} data-speed={live && rate != null ? F.speedOf(rate, bw) : undefined}>
      <div className="flow-head">
        <span className="flow-label">{up ? <ArrowUp /> : <ArrowDown />}{up ? '上行' : '下行'}</span>
        <span className="flow-rate"><Num f={F.rate(rate)} /></span>
      </div>
      <button type="button" className="flow-trend" data-tip="查看主机曲线" onClick={() => onOpen('host')} aria-label={`${up ? '上行' : '下行'}速率：查看主机曲线`}>
        <DotTrace values={values} n={dots} />
        {/* 离线不再标：资源位的占位块已写明原因和时长，读数只留「—」 */}
        <span className="flow-live" data-live={live && rate != null ? '' : undefined}>{!live ? '' : rate == null ? '无读数' : '实时'}</span>
      </button>
    </div>
  );
}

/** compact：紧凑卡的流量列窄（约 120px），迹线只画最近 20 个点，30 个点挤在一起看不清 */
function Flow({ m, onOpen, totals, compact, paused }: { m: MachineView; onOpen: Open; totals?: boolean; compact?: boolean; paused: Paused }) {
  const tr = useLiveSeries(m.id, paused);
  const tx = useMemo(() => tr.map((p) => p.tx), [tr]); const rx = useMemo(() => tr.map((p) => p.rx), [tr]); // 迹线没变时 DotTrace（memo）不重画
  const r = liveRate(m); const t = monthFlow(m); const bw = m.plan?.bandwidth_mbps;
  return (
    <div className="flow">
      <FlowCol dir="down" rate={r?.rx ?? null} values={rx} dots={compact ? 20 : undefined} live={r != null} bw={bw} onOpen={onOpen} />
      <FlowCol dir="up" rate={r?.tx ?? null} values={tx} dots={compact ? 20 : undefined} live={r != null} bw={bw} onOpen={onOpen} />
      {/* 本月流量单独一行：有 agent 的是网卡口径（下行 / 上行，计费口径）；同机 IP 节点没有自己的网卡，是业务流量（用户视角的下载 / 上传）。两种不混在一行 */}
      {totals && (
        <button type="button" className="flow-foot flow-month" data-tip={monthFlowTip(m)} onClick={() => onOpen('traffic')}>
          {t.kind === 'nic' ? <span className="flow-foot-label"><ArrowDownUp />本月</span> : <span className="flow-foot-label"><Globe />本月业务</span>}
          <span className="flow-vals">{t.kind === 'nic' ? <>↓ 下行 <Num f={F.bytes(t.down)} /> · ↑ 上行 <Num f={F.bytes(t.up)} /></> : <>下载 <Num f={F.bytes(t.down)} /> · 上传 <Num f={F.bytes(t.up)} /></>}<FromTag from={t.from} sep /></span><span className="sr-only">（查看流量页签）</span>
        </button>
      )}
    </div>
  );
}

/** 两列大号读数（家宽代理：本月业务流量、探测延迟），外观同网卡速率；整块可点，打开对应页签 */
function Pair({ cols, tab, label, tip, onOpen }: { cols: { icon: React.ReactNode; label: string; value: React.ReactNode; sub?: React.ReactNode }[]; tab: DrawerTab; label: string; tip?: string; onOpen: Open }) {
  return (
    <button type="button" className="flow nc-pair nc-click" data-tip={tip} onClick={() => onOpen(tab)}>
      {cols.map((c) => (
        <span key={c.label} className="flow-col">
          <span className="flow-head"><span className="flow-label">{c.icon}{c.label}</span><span className="flow-rate">{c.value}</span></span>
          {c.sub && <span className="flow-sub">{c.sub}</span>}
        </span>
      ))}
      <span className="sr-only">（{label}）</span>
    </button>
  );
}
/** 家宽代理：本月业务流量（下载 / 上传），副行今日；不完整时今日后面跟灰字「10-22 起」 */
function BizPair({ m, onOpen, today }: { m: MachineView; onOpen: Open; today?: boolean }) {
  const t = monthTraffic(m);
  return <Pair tab="traffic" label="查看流量页签" onOpen={onOpen} tip={monthFlowTip(m)} cols={[
    { icon: <ArrowDown />, label: '本月下载', value: <Num f={F.bytes(t.down)} />, sub: today && <>今日 <Num f={F.bytes(t.today_down)} /><FromTag from={m.month_counted_from} sep /></> },
    { icon: <ArrowUp />, label: '本月上传', value: <Num f={F.bytes(t.up)} />, sub: today && <>今日 <Num f={F.bytes(t.today_up)} /></> },
  ]} />;
}
const FAIL = <span style={{ color: 'var(--bad-text)' }}>失败</span>;
/** TCP 延迟读数（线路机 → 节点，穿透入口是节点 → 线路机，近 1 小时中位数）：全部失败显示「失败」（同抽屉入口表），没有探测是「—」 */
const probeMs = (ms: number | null, fail: boolean) => (fail ? FAIL : <Num f={F.ms(ms)} />);
/** 出口：只看通不通（出口验证的耗时是经节点去中控的，不当延迟读数） */
const egressText = (ok: boolean, fail: boolean) => (fail ? FAIL : ok ? '正常' : '—');
/** 家宽代理：TCP 延迟与出口通不通 */
function ProbePair({ m, onOpen }: { m: MachineView; onOpen: Open }) {
  const ps = probeSummary(m);
  return <Pair tab="quality" label="查看连接质量页签" onOpen={onOpen} cols={[
    { icon: <Radar />, label: 'TCP 延迟', value: <span data-tip={ps.tcpTip}>{probeMs(ps.tcp, ps.tcpFail)}</span> },
    { icon: <Globe />, label: '出口', value: egressText(ps.egressOk, ps.egressFail) },
  ]} />;
}

/** 剩余流量：标题 + 剩余量，右侧本周期已用 / 上限（滚动 30 天的是近 30 天已用；周期起止、计量规则、数据不完整的说明在悬停里），
 *  下面绿→黄→红的格条；点击打开费用页签（额度、周期、计量规则都在那里）。不限流量的左边写本周期已用、右边「流量不限」（详细卡字段里已有「流量上限 不限」，右边不再写），不画格条，一行垂直居中。
 *  同机 IP 节点自己没设上限：额度与宿主机共用（算在宿主机的已用里） */
function QuotaSlot({ m, onOpen, large }: { m: MachineView; onOpen: Open; large?: boolean }) {
  const q = quota(m);
  const fc = quotaForecast(m);
  // 没设套餐 ≠ 不限：不画额度，只写一句「未设置」
  if (q.unset) return <div className="nc-quota is-unl"><span className="nc-qhead"><span className="nc-qlabel t-muted"><Database />流量额度未设置</span></span></div>;
  return (
    <button type="button" className={cn('nc-quota', q.unlimited && 'is-unl')} onClick={() => onOpen('billing')}>
      <span className="nc-qhead">
        {q.unlimited ? <>
          <span className="nc-qlabel"><Database />{quotaUsedLabel(q)}<b className="num"><Num f={quotaGb(q.used)} /></b></span>
          {/* 详细卡的字段里已经写了「流量上限 不限」，这里不再重复 */}
          {!large && <span className="nc-quse" data-tip={quotaTip(m)}>流量不限</span>}
        </> : (
          <span className="nc-qlabel"><Database />剩余流量
            <b className={cn('num', q.rem === 0 && 'is-bad')}>{q.rem == null ? '与宿主机共用' : q.rem > 0 ? <Num f={quotaGb(q.rem)} /> : '已超限'}</b>
          </span>
        )}
        {q.limit > 0 && <span className="nc-quse" data-over={q.pct != null && q.pct >= 100 ? '' : undefined} data-tip={`${quotaUsedLabel(q)} ${F.str(quotaGb(q.used))} / ${F.str(limitGb(q.limit))}\n${quotaTip(m)}`}>{quotaUsedLabel(q)} <Num f={quotaGb(q.used)} /> / <Num f={limitGb(q.limit)} /><FromTag from={q.from} start={q.start} sep /></span>}
      </span>
      {q.rem != null && <Cells pct={q.pct} colors={QUOTA_COLORS} />}
      {fc && <span className="nc-qfc">{fc}</span>}
      <span className="sr-only">（查看费用页签）</span>
    </button>
  );
}

const NO_ENTRY_TIP = '探测连的是线路机上客户用的入口端口\n这台还没有入口，分到节点后自动开始探测\n线路本身通不通看下面的三网 PING';
/** 心跳条：近 1 小时的探测（全部卡片共用一个 /probe-beats 查询，各取自己的那份）；整条可点，打开连接质量页签。
 *  格子：节点是线路机 → 节点的 TCP 建连（线路机的 agent 探）；线路机是主探测来源 → 线路机的连通性（中控，或靠近客户的探测点；
 *  说明文字随来源变，探测点另有建连延迟：标题后面写近 1 小时的中位延迟）。
 *  标题：故障的写第一条健康原因（同状态胶囊的悬停），线路机写「入口可达 x/y」（同抽屉，linkCounts；节点通、出口通不全通时跟在后面），
 *  家宽代理写成功率（延迟已在上面），其余写 TCP 延迟 · 出口通不通（有入口的线路机端口连不上时在最前面写一段「入口可达 x/y」：它比延迟更要紧，紧凑密度下一行放不下时被截掉的该是后面的 TCP / 出口，否则卡片全绿、看不出客户连不上）。节点自己的 agent 离线不影响它的心跳格：探测不是它自己做的，照常上色 */
const BeatSlot = memo(function BeatSlot({ id, kind, lifecycle, reason, reasonKey, onOpen, tcp, tcpTip, egressOk, tcpFail, egressFail, all, off, total, tcpOk, egressBad, reachBad, reverse }: { id: string; kind: MachineView['kind']; lifecycle: MachineView['lifecycle']; reason: string | undefined; reasonKey: string | undefined; onOpen: Open } & ReturnType<typeof probeSummary>) {
  const live = lifecycle === 'active';
  const src = useReachLabel();
  const bq = useProbeBeats(useCallback((d: ProbeBeats) => d[id], [id]));
  const beats = bq.data;
  const rtt = kind === 'relay' && live ? relayRttMs(beats) : null;
  const who = reachWho(src);
  const links = linkCounts({ all, reachBad, tcpOk, total, egressBad });
  // 入口连不上（relay_unreachable）时原因就是「入口可达」那个数，不再另写一遍；整台连不上、但还没有哪个端口满 3 次时（轮流检查）照写原因。
  // 别的原因写在前面；延迟放进悬停
  const reachReason = reasonKey === 'relay_unreachable' && reachBad > 0;
  // 节点通 / 出口通不是全通时才跟在「入口可达」后面（LinkCounts only），这时一行放不下延迟：延迟只进悬停；三段都写时连时间窗前面的来源也让给它（来源在悬停里），只剩「近 1 小时」
  const bad = (['node', 'egress'] as const).filter((k) => links[k].total > 0 && links[k].ok < links[k].total).length;
  const extra = bad > 0;
  // 三段都写时标题可能放不下（紧凑密度会截掉节点通 / 出口通）：悬停第一行写全三段；原因里的逐端口错误合并成「N 个端口…」
  const full = extra ? `${(['reach', 'node', 'egress'] as const).filter((k) => links[k].total > 0).map((k) => `${LINK_LABEL[k]} ${links[k].ok}/${links[k].total}`).join(' · ')}\n` : '';
  const relayCounts = <span data-tip={`${full}${reachReason ? `${reasonText(reason!)}\n` : ''}下面的格子是${who}连线路机入口的结果${off ? `\n停用的 ${off} 条入口不计` : ''}${rtt != null && (reason || extra) ? `\n${src?.label ?? '探测点'}连到这台线路机的延迟 ${Math.round(rtt)} ms（近 1 小时中位数）` : ''}`}><LinkCounts c={links} who={who} only={['reach']} /></span>;
  const rttTag = rtt == null || extra ? null : <> · <span data-tip={`${src?.label ?? '探测点'}连到这台线路机的延迟，近 1 小时中位数`}>延迟 <b className="num">{Math.round(rtt)} ms</b></span></>;
  // 节点自己通、但有入口的线路机端口连不上：卡片是全绿的，客户却连不上，在最前面补一段「入口可达 x/y」（同抽屉标题旁的黄胶囊；放最前是因为紧凑密度下一行放不下，被截掉的该是后面的延迟）
  const unreach = kind !== 'relay' && live && reachBad > 0
    ? <><span data-tip={unreachTip(who, reachBad)}>{LINK_LABEL.reach} <b className="num" style={{ color: 'var(--bad-text)' }}>{all - reachBad}/{all}</b></span> · </> : null;
  const rate = kind === 'proxy' && live ? beatRate(beats) : null;
  const empty = live && bq.isSuccess && !beats?.some((b) => b[0] > 0); // 整条都没有探测：标题写明、格子画虚线，不是 20 格灰块
  const cap = !live ? <span className="t-muted">{lifecycle === 'retired' ? '已退役' : '还没开始探测'}</span>
    : kind === 'relay' && reason ? (reachReason ? relayCounts : <><span className="nc-reason" data-tip={reason}>{reasonKey === 'relay_unreachable' ? `${who}连不上入口` : reason}</span>{all > 0 && <> · {relayCounts}</>}</>)
    : reason ? <span className="nc-reason" data-tip={reason}>{reason}</span>
    : kind === 'relay' && all === 0 ? <span className="t-muted" data-tip={NO_ENTRY_TIP}>还没有入口，不用探测</span>
    : empty ? <span className="t-muted">暂无探测</span>
    : kind === 'relay' ? <>{relayCounts}{rttTag}</>
    : kind === 'proxy' ? <>{unreach}探测成功 <b className="num">{rate == null ? '—' : `${Math.round(rate)}%`}</b></>
    // 入口全是穿透的节点：TCP 是节点 → 线路机的（旧 agent 没有，显示 —），格子是经入口的出口探测；混着普通入口的在后面标出含几条穿透
    : total === 0 && reverse > 0 ? <>{unreach}<span data-tip={`${tcpTip}\n${REVERSE_TIP}\n下面的格子是经入口的出口探测`}>TCP <b>{probeMs(tcp, tcpFail)}</b>（穿透）</span> · 出口 <b>{egressText(egressOk, egressFail)}</b></>
    : <>{unreach}<span data-tip={tcpTip}>TCP <b>{probeMs(tcp, tcpFail)}</b></span>{reverse > 0 && <span data-tip={REVERSE_TIP}>（含 {reverse} 条穿透）</span>} · 出口 <b>{egressText(egressOk, egressFail)}</b></>;
  // 时段按这份数据取回的时刻对齐（轮询间隔 2 分钟，用当前时刻在跨格时会错一格）
  const titles = useMemo(() => (live ? beatTitles(beats, bq.dataUpdatedAt) : undefined), [live, beats, bq.dataUpdatedAt]);
  return (
    <button type="button" className="nc-beat nc-click" onClick={() => onOpen('quality')}>
      <Beat cells={live ? beatCells(beats, kind === 'relay') : []} titles={titles} caption={cap} win={kind === 'relay' && bad < 2 ? reachWin(src) : '近 1 小时'} empty={empty} legend={kind === 'relay' ? beatLegendRelay(src) : BEAT_LEGEND} />
      <span className="sr-only">（查看连接质量页签）</span>
    </button>
  );
});

/** 已上架 · 到期 · 剩余价值：彩色读数。已上架按分钟走（memo 挡住推送，自己每分钟重画） */
const RunRow = memo(function RunRow({ activated, sub, recycle }: { activated: string | null; sub: Subscription | null; recycle: RecycleNote | null }) {
  const now = useNow(true, 60_000);
  const rv = remainingValue(sub);
  return (
    <section className="slot-seg nc-run">
      <div className="nc-run-item"><span className="nc-run-label"><RefreshCw />已上架</span><span className="nc-run-val" data-c="up"><Num f={F.since(activated, now)} /></span></div>
      <div className="nc-run-item"><span className="nc-run-label"><Calendar />到期</span><span className="nc-run-val"><Due days={sub?.due_days} state={sub?.due_state} /></span>{recycle && <span className="nc-run-note" data-tip={recycle.tip}>{recycle.text}</span>}</div>
      <div className="nc-run-item"><span className="nc-run-label"><Wallet />剩余价值</span><span className="nc-run-val">{rv ? <span className="num">{F.money(rv.cents, rv.currency)}</span> : <span className="t-muted">—</span>}</span></div>
    </section>
  );
});

// 标签行收起几枚要排版后才数得出。一页几十张卡各在自己的 layout effect 里「读位置 → 写平移」，读写交替，每张卡都逼浏览器重排一次；
// 改成登记进队列，同一轮提交（或同一轮 ResizeObserver 回调）里的全部卡片在微任务里先统一读、再统一写：整页只排一次版。
// 微任务在这一轮提交之后、绘制之前跑，收起数用 flushSync 同步重渲染，「+N」不会晚一帧冒出来
// 列表元素在冲刷时才取（list()）：排进队列到微任务之间卡片可能已卸载（换密度、换分组、翻页），ref 已是 null，跳过它；
// 没有标签的卡（没设供应商的 VPS 节点）列表是空的：收起数记 0（原来有收起的，标签变成 0 枚时「+N」要去掉）。原来排队时就把元素存下、冲刷时不查，卸载的那张会抛 TypeError，
// 同一批里排在它后面的卡片都没数，「+N」停在旧值
type TagsJob = { list: () => HTMLDivElement | null; more: () => HTMLSpanElement | null; cut: number; setCut: (n: number) => void };
const tagsQueue = new Set<TagsJob>();
function flushTags() {
  const jobs = [...tagsQueue]; tagsQueue.clear();
  // 读：每张卡放不下的枚数，和「+N」该往左挪多少（最后一枚露出的标签右边到列表右边的空白）
  const reads = jobs.flatMap((j) => {
    const el = j.list();
    if (!el?.isConnected) return [];
    if (!el.firstElementChild) return [{ j, n: 0, dx: 0 }];
    const top = (el.firstElementChild as HTMLElement).offsetTop;
    let n = 0;
    for (const t of el.children) if ((t as HTMLElement).offsetTop > top) n++;
    const last = el.children[el.children.length - 1 - n];
    return [{ j, n, dx: last.getBoundingClientRect().right - el.getBoundingClientRect().right }];
  });
  // 写：收起数变了的重渲染（之后的 layout effect 会再排一轮队、量出新的平移）；没变的直接写平移
  const changed = reads.filter((r) => r.n !== r.j.cut);
  for (const r of reads) { const more = r.j.more(); if (more && r.n === r.j.cut) more.style.translate = `${r.dx}px`; }
  if (changed.length) flushSync(() => changed.forEach((r) => r.j.setCut(r.n)));
}
function queueTags(j: TagsJob) {
  if (!tagsQueue.size) queueMicrotask(flushTags);
  tagsQueue.add(j);
}

/** 标签行：同一套标签与顺序（见 MachineTags：供应商 → 类型 → 同机关系 → 维护中），三种密度都一样。只排一行，放不下的整枚收起（折到看不见的第二行，cards.css），末尾「+N」，悬停列出收起的那几枚。
 *  挂载、标签变了、「+N」出现或变了（列表随之变窄）、卡片变宽变窄时重数（排进上面的批量队列）；推送不改这几个字段，memo 挡住，不重数。
 *  列表按剩下的宽度伸满，收起的标签让出的空白留在它末尾：「+N」往左平移过去、紧跟最后一枚露出的标签（价格仍靠右）。只是平移，不改排版，不会再触发重数。
 *  价格不是按月付的，悬停给月均 */
const Tags = memo(function Tags({ code, provider, kind, maintenance, price, priceTip, id, host, nodes }: {
  code: string | null; provider: MachineView['provider']; kind: MachineView['kind']; maintenance: boolean; price: string | null; priceTip: string | null;
  id: string; host: MachineView['host_machine']; nodes: MachineView['ip_nodes'];
}) {
  const items = machineTagItems({ provider, provider_code: code, kind, maintenance, host_machine: host, ip_nodes: nodes });
  const list = useRef<HTMLDivElement>(null);
  const more = useRef<HTMLSpanElement>(null);
  const [cut, setCut] = useState(0);
  const job = useRef<TagsJob | null>(null);
  job.current ??= { list: () => list.current, more: () => more.current, cut: 0, setCut };
  const count = useCallback(() => { if (list.current) queueTags(job.current!); }, []);
  useLayoutEffect(() => { job.current!.cut = cut; count(); }, [count, code, provider, kind, maintenance, price, host, nodes, cut]);
  useEffect(() => watchSize(list.current!.parentElement!, count), [count]); // 看整行：行宽只随卡片变，「+N」进出不会让它再触发
  return (
    <div className="nc-tags">
      {/* 同机关系标被收起时不进 Tab 顺序：聚焦会把裁掉的第二行滚出来 */}
      <div className="nc-tags-list" ref={list}>{items.map((t, i) => <MachineTag key={t.key} t={t} m={{ id, host_machine: host, ip_nodes: nodes }} tabIndex={'c' in t || i < items.length - cut ? undefined : -1} />)}</div>
      {/* 收起的标签仍在读屏顺序里，「+N」只给眼睛看 */}
      {cut > 0 && <span ref={more} className="tag" data-c="gray" aria-hidden data-tip={items.slice(-cut).map((t) => ('c' in t ? t.tip : hostRel(host, nodes)?.tip ?? '')).join('\n')}>+{cut}</span>}
      {price && <span className="nc-price" data-tip={priceTip ?? undefined}><CircleDollarSign /><span className="num">{price}</span></span>}
    </div>
  );
});

/** footer：状态胶囊（健康原因在它的悬停里）、「仅看异常」里已恢复的标记、按到期 / 流量排序时的排序依据、详情与管理 */
const Foot = memo(function Foot({ health, lifecycle, maintenance, recovered, show, showTone, showTip, onOpen }: Pick<MachineView, 'health' | 'lifecycle' | 'maintenance'> & { recovered?: boolean; show?: string; showTone?: string; showTip?: string; onOpen: Open }) {
  return (
    <footer className="nc-foot">
      <span className="nc-foot-left"><HealthPill m={{ health, lifecycle, maintenance }} />{recovered && <Pill tone="neutral" dot="none" tip={RECOVERED_TIP}>已恢复</Pill>}{show && <span className="nc-show" data-tone={showTone || undefined} data-tip={showTip}>{show}</span>}</span>
      <div className="nc-foot-right">
        <Button variant="text" size="sm" onClick={() => onOpen()}><PanelRightOpen size={14} /><span>详情</span></Button>
        <Button variant="text" size="sm" onClick={() => onOpen('manage')}><Settings2 size={14} /><span>管理</span></Button>
      </div>
    </footer>
  );
});
/** footer 里的排序依据（文字）：详细卡、标准卡本来就有到期的不写；详细卡有本月流量的不写 */
function showOf(m: MachineView, show: CardShow | undefined, view: CardView): { text?: string; tone?: string; tip?: string } {
  if (show === 'due' && view === 'compact') {
    const s = m.subscription; if (!s) return { text: '没有到期日' };
    const d = F.dueFmt(s.due_days, dueTone(s.due_state));
    return { text: `${d.pre}${d.f ? F.str(d.f) : ''}${d.post}${d.past ? '' : '到期'}`, tone: d.tone };
  }
  if (show === 'traffic' && !(view === 'large' && hasHost(m))) return { text: `本月流量 ${F.str(F.bytes(trafficTotal(m)))}`, tip: monthFlowTip(m) };
  return {};
}

// 视口（上下各留 200px，滚动时先一步换上最新）之外的卡片冻结：推送照常改 m，但卡片主体收到的还是滚出去时的那一份（memo 挡住整张卡），
// 回到视口时换成最新的。一页 96 张紧凑卡，一条推送原来要把视口外的几十张一起重画
const watchNear = sharedObserver<IntersectionObserverEntry>((cb) => new IntersectionObserver(cb, { rootMargin: '200px 0px' }));

type CardProps = { m: MachineView; view: CardView; current?: boolean; onOpen: (id: string, tab?: DrawerTab) => void; layoutKey: string | null; show?: CardShow; recovered?: boolean };
/** 卡片只吃稳定的 props（onOpen 回调不随地址变化），配合 memo，抽屉开关时只有变了的那张重渲染。
 *  layoutKey：筛选/排序/视图的签名，变了才量位置做布局动画（layoutDependency），平时重渲染不量；null = 不做布局动画（卡片太多时由首页传）。
 *  出现时的淡入走 CSS（cards.css），不等 motion 特性包：特性包到位之前 initial 的透明度会一直停在 0 */
function NodeCardImpl(p: CardProps) {
  const ref = useRef<HTMLElement>(null);
  const [near, setNear] = useState(true);
  const paused = useRef(false);
  useEffect(() => watchNear(ref.current!, (e) => { paused.current = !e.isIntersecting; setNear(e.isIntersecting); }), []);
  const held = useRef(p.m);
  if (near) held.current = p.m;
  return <CardBody {...p} m={held.current} cardRef={ref} paused={paused} />;
}

const CardBody = memo(function CardBody({ m, view, current, onOpen: open, layoutKey, show, recovered, cardRef, paused }: CardProps & { cardRef: React.RefObject<HTMLElement | null>; paused: Paused }) {
  const onOpen = useCallback<Open>((tab) => open(m.id, tab), [open, m.id]);
  const proxy = m.kind === 'proxy';
  const quiet = isQuiet(m);
  const info: Info = { id: m.id, ip: m.ip, sshPort: m.ssh_port, entry: entryText(m), plan: m.plan, host: m.host_machine, nodes: m.ip_nodes };
  const tags = <Tags code={m.provider_code} provider={m.provider} kind={m.kind} maintenance={m.maintenance} price={subLabel(m)} priceTip={priceHint(m.subscription)} id={m.id} host={m.host_machine} nodes={m.ip_nodes} />;
  // 探测行的标题只讲探测：agent 离线已由资源位的占位块和状态标说过，这里跳过它，取下一条原因（没有就照常写读数）
  const r0 = m.health.reasons.find((r) => r.key !== 'agent_offline');
  const reason = m.lifecycle === 'active' && m.health.status === 'down' && r0 ? errText(r0.text) : undefined;
  // 线路机：心跳格（主探测来源 → 客户入口，可用性）下面接三网 PING（线路机往外，线路质量）
  const beat = <section className="slot-seg"><BeatSlot id={m.id} kind={m.kind} lifecycle={m.lifecycle} reason={reason} reasonKey={r0?.key} onOpen={onOpen} {...probeSummary(m)} />{m.kind === 'relay' && m.lifecycle === 'active' && <PingRows id={m.id} onOpen={onOpen} />}</section>;
  const sv = showOf(m, show, view);
  const res = proxy ? <BizPair m={m} onOpen={onOpen} today={view === 'large'} /> : <Meters m={m} compact={view !== 'large'} onOpen={onOpen} />;
  // 还没有主机读数的（上架中、没上报过）照样是速率两列：读数「—」
  const flow = proxy ? <ProbePair m={m} onOpen={onOpen} /> : <Flow m={m} onOpen={onOpen} totals={view === 'large'} compact={view === 'compact'} paused={paused} />;

  return (
    <mo.article ref={cardRef} layout={layoutKey == null ? false : 'position'} layoutDependency={layoutKey} transition={SPRING} className={cn('ncard', current && 'is-current', quiet && 'is-quiet')} data-view={view} data-id={m.id} data-kind={m.kind} data-health={m.health.status} data-offline={quiet ? '1' : undefined}>
      <Head name={m.name} from={m.spare_from} country={m.country} manual={!!m.country_set} ip={m.ip} view={view} onOpen={onOpen} />

      {view === 'large' && <>
        <LargeInfo {...info} limit={quota(m).limit} unset={quota(m).unset} />
        {proxy ? <div className="nc-res nc-res-pair">{res}</div> : res}
        <section className="slot-seg">{flow}</section>
        <section className="slot-seg"><QuotaSlot m={m} onOpen={onOpen} large /></section>
        {beat}
        <RunRow activated={m.activated_at} sub={m.subscription} recycle={recycleNote(m)} />
        {tags}
      </>}

      {view === 'small' && <>
        <SmallInfo {...info} dueDays={m.subscription?.due_days} dueState={m.subscription?.due_state} recycle={recycleNote(m)} />
        <section className="slot-seg">{res}</section>
        <section className="slot-seg">{flow}</section>
        <section className="slot-seg"><QuotaSlot m={m} onOpen={onOpen} /></section>
        {beat}
      </>}

      {view === 'compact' && <>
        {tags}
        <section className="slot-seg">{res}</section>
        <section className="slot-seg">{flow}</section>
        {beat}
      </>}

      <Foot health={m.health} lifecycle={m.lifecycle} maintenance={m.maintenance} recovered={recovered} show={sv.text} showTone={sv.tone} showTip={sv.tip} onOpen={onOpen} />
    </mo.article>
  );
});
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
