// 软路由卡片：外壳与槽位同节点卡（cards.css 的 .ncard + subgrid，大卡片密度），固定 8 个槽位，在线 / 部署中 / 等待接入 / 离线都占同样的行，
// 同一排的卡片分段对齐。还有路由状态 pill、资源格条、部署时间线、行内迷你折线与「查看接入命令」，抽屉也用。
// 卡片随 routers 事件（每轮采集最多一次）重渲染：列表做结构共享，没变的路由引用不变，memo 挡住。
import { memo, useCallback, useMemo, type CSSProperties, type ReactNode } from 'react';
import { ArrowDown, ArrowUp, Cable, Clock, Cpu, Gauge, HardDrive, Loader2, MemoryStick, PanelRightOpen, RefreshCw, Router as RouterIcon, Shuffle, Terminal, Thermometer, Unplug, Users, Waypoints } from 'lucide-react';
import * as F from '@/lib/format';
import { tagColor } from '@/lib/derive';
import { useNow } from '@/lib/useNow';
import { cn } from '@/lib/utils';
import { deadExitNames, deploySteps, routerIssues, routerMeters, showBootstrap, showDeploy, tempTone, uptimeFmt, useRegenBootstrap, useRouter, useRouterMetrics, type RouterSummary, type RouterTab, type RouterView } from '@/lib/routers';
import type { BeatCell } from '@/lib/derive';
import { Button } from '@/components/ui/button';
import { InfoTip } from '@/components/ui/form';
import { toast } from '@/components/ui/toast';
import { Num, Pill } from './primitives';
import { CellMeter, Cells, DotTrace, Tag } from './viz';
import '@/styles/routers.css';

type Open = (tab?: RouterTab) => void;

/** 状态 pill（只靠底色与文字，不带圆点；要醒目的配一个图标）：部署中（转圈）/ 等待接入 / 在线 /
 *  降级（在线但有毛病，悬停点名是哪个出口；同机器的叫法，降级算正常）/ 离线（悬停看连不上的原因，才是故障） */
export function RouterStatePill({ r }: { r: Pick<RouterView, 'id' | 'state' | 'error' | 'summary' | 'deploy'> }) {
  // 不通的出口名只在详情里：降级的才取详情（和抽屉同一份缓存）
  const slow = r.state === 'online' && !!r.summary && r.summary.exits.alive < r.summary.exits.total;
  const names = deadExitNames(useRouter(slow ? r.id : null).data?.detail);
  if (r.deploy?.state === 'running') return <Pill tone="info" dot="pulse">部署中</Pill>;
  if (r.state === 'pending') return <Pill tone="info" tip={r.deploy?.state === 'failed' ? `一键部署失败：${r.deploy.error ?? ''}` : '路由还没连上过中控'}>等待接入</Pill>;
  if (r.state === 'offline') return <Pill tone="bad" tip={r.error ?? undefined} tabIndex={r.error ? 0 : undefined}>离线</Pill>;
  // 在线却带原因：连得上但采集出错，数据是上一次的
  if (r.error) return <Pill tone="warn" tip={`采集出错：${r.error}`} tabIndex={0}>采集出错</Pill>;
  const issues = routerIssues(r.summary, names);
  return issues.length ? <Pill tone="warn" tip={issues.join('\n')} tabIndex={0}>降级</Pill> : <Pill tone="ok">在线</Pill>;
}
/** 卡片描边：离线红、在线但有毛病黄（同机器卡的故障 / 降级） */
export const routerHealth = (r: RouterView) => (r.state === 'offline' ? 'down' : r.state === 'online' && (r.error || routerIssues(r.summary).length) ? 'degraded' : 'ok');

/** 资源四项格条（CPU / 内存 / 存储 / 连接表）：卡片与抽屉「当前」共用 */
export function RouterMeters({ s }: { s: RouterSummary }) {
  const r = routerMeters(s);
  return (
    <div className="nc-res">
      <CellMeter metric="cpu" icon={<Cpu />} label="CPU" value={r.cpu.value} pct={r.cpu.pct} sub={r.cpu.sub} tip={r.cpu.tip} />
      <CellMeter metric="mem" icon={<MemoryStick />} label="内存" value={r.mem.value} pct={r.mem.pct} sub={r.mem.sub} />
      <CellMeter metric="disk" icon={<HardDrive />} label="存储" value={r.disk.value} pct={r.disk.pct} sub={r.disk.sub} tip={r.disk.tip} />
      <CellMeter metric="load" icon={<Gauge />} label="连接表" value={r.ct.value} pct={r.ct.pct} sub={r.ct.sub} tip={r.ct.tip} />
    </div>
  );
}

/** 「查看接入命令」/「改用在路由上执行命令」：原来那条只在生成时显示过一次（中控只存哈希），这里重新生成一条并弹出来，旧的随之作废 */
export function useShowBootstrap() {
  const regen = useRegenBootstrap();
  const run = useCallback((r: Pick<RouterView, 'id' | 'name' | 'last_seen_at'>) => regen.mutateAsync(r.id).then(
    (b) => showBootstrap({ id: r.id, name: r.name, ...b, issued_at: new Date().toISOString(), replaced: r.last_seen_at != null }), toast.err), [regen]);
  return { run, pending: regen.isPending };
}

type TlItem = { key: string; tone: 'done' | 'run' | 'bad' | 'ok'; text: ReactNode; at?: string | number | null; extra?: ReactNode };
const atClock = (t: string | number) => F.clock(typeof t === 'number' ? new Date(t).toISOString() : t);
/** 一键部署的时间线（竖排：圆点 + 连线）：开始 → 走过的步骤 → 当前步骤转圈；失败的那步标红，带原因与两种改法；做完了接一个「完成」。
 *  max：只露最后几项（卡片里） */
export function DeployTimeline({ r, max }: { r: RouterView; max?: number }) {
  const boot = useShowBootstrap();
  const d = r.deploy!;
  const steps = deploySteps(r.id, d);
  const items: TlItem[] = [{ key: 'start', tone: 'done', text: '开始部署', at: d.started_at }];
  steps.forEach((x, i) => items.push({ key: `s${i}`, tone: i < steps.length - 1 || d.state === 'done' ? 'done' : d.state === 'running' ? 'run' : 'bad', text: x.step, at: x.at }));
  if (d.state === 'running' && !steps.length) items.push({ key: 'wait', tone: 'run', text: '准备中' });
  if (d.state === 'failed') {
    if (!steps.length) items.push({ key: 'fail', tone: 'bad', text: '部署' });
    items[items.length - 1].extra = <>
      {d.error && <span className="rt-tl-err">{d.error}</span>}
      <span className="row rt-tl-acts">
        <Button size="sm" loading={boot.pending} onClick={() => void boot.run(r)} data-tip="中控连不上路由（路由在 NAT 后面）时用：生成一条命令，在路由的 SSH 里粘贴执行"><Terminal size={13} />改用在路由上执行命令</Button>
        <Button size="sm" variant="text" onClick={() => showDeploy({ id: r.id, name: r.name })}><RefreshCw size={13} />重新部署</Button>
      </span>
    </>;
  }
  if (d.state === 'done') items.push({ key: 'done', tone: 'ok', text: r.state === 'online' ? '完成，路由已在线' : '完成，等隧道连上中控', at: d.finished_at });
  return (
    <ol className="rt-tl" aria-label="部署进度">
      {(max ? items.slice(-max) : items).map((it) => (
        <li key={it.key} data-tone={it.tone}>
          <i className="rt-tl-dot" aria-hidden>{it.tone === 'run' && <Loader2 className="spin" />}</i>
          <div className="rt-tl-body"><span className="rt-tl-text">{it.text}{it.tone === 'bad' && ' · 失败'}</span>{it.at != null && <span className="rt-tl-at">{atClock(it.at)}</span>}{it.extra}</div>
        </li>
      ))}
    </ol>
  );
}

/** 行内迷你格子（出口近 6 小时的延迟测试、表格行里）：格子语言同曲线页签的「各出口延迟」，一格一段时间，每格悬停看那段的结果 */
export const MiniBeat = memo(function MiniBeat({ cells, titles }: { cells: BeatCell[]; titles: string[] }) {
  return <span className="rt-minibeat" role="img" aria-label="近 6 小时的延迟测试">{cells.map((c, i) => <i key={i} data-t={c ?? undefined} data-tip={titles[i]} />)}</span>;
});

/** 没有读数时（部署中 / 等待接入 / 离线）占掉资源、流量、出口、运行四个槽位（一排里仍与在线的卡对齐），不再排一列「—」：一键部署的时间线（部署中；还没接入的；离线后重新部署失败的）、等路由执行接入命令、离线多久了。不画框（用户：不要每块都是格子） */
function Hold({ r }: { r: RouterView }) {
  const boot = useShowBootstrap();
  const d = r.deploy;
  if (d && (d.state === 'running' || r.state === 'pending' || (d.state === 'failed' && r.state === 'offline'))) return <div className="nc-res rt-span rt-hold-tl"><DeployTimeline r={r} max={3} /></div>;
  if (r.state === 'pending') {
    return (
      <div className="nc-res rt-span"><div className="nc-hold rt-hold">
        <span className="rt-hold-main"><Clock />等路由执行接入命令<InfoTip text="在路由的 SSH 里执行接入命令后，约 30 秒内变为在线" /></span>
        <Button size="sm" loading={boot.pending} onClick={() => void boot.run(r)} data-tip="原来的命令只显示一次：这里会重新生成一条，旧的作废"><Terminal size={13} />查看接入命令</Button>
      </div></div>
    );
  }
  return <div className="nc-res rt-span"><div className="nc-hold rt-hold is-bad"><span className="rt-hold-main"><Unplug />连不上路由 · 最后在线 <Seen at={r.last_seen_at} /></span>{r.error && <span className="rt-hold-err" data-tip={r.error}>{r.error}</span>}</div></div>;
}

/** 「N 秒前」随时间走（卡片 memo 挡住了推送之外的重渲染，自己每 10 秒重画） */
export const Seen = memo(function Seen({ at }: { at: string | null }) {
  const now = useNow(!!at, 10_000);
  return <>{F.ago(at, now)}</>;
});

/** WAN 口速率：方向 + 大号读数（按量级着色）+ 近 15 分钟的点阵迹线（同节点卡；点来自 1 小时曲线，30 秒一个），点击看曲线 */
function WanCol({ dir, bps, values, onOpen }: { dir: 'up' | 'down'; bps: number | null; values: number[]; onOpen: Open }) {
  const up = dir === 'up';
  return (
    <div className="flow-col" data-dir={dir} data-speed={F.speedOf(bps)}>
      <div className="flow-head">
        <span className="flow-label">{up ? <ArrowUp /> : <ArrowDown />}WAN {up ? '上行' : '下行'}</span>
        <span className="flow-rate"><Num f={F.rate(bps)} /></span>
      </div>
      <button type="button" className="flow-trend" data-tip="查看曲线" onClick={() => onOpen('charts')} aria-label={`WAN ${up ? '上行' : '下行'}速率：查看曲线`}><DotTrace values={values} /></button>
    </div>
  );
}
function WanFlow({ id, s, onOpen }: { id: string; s: RouterSummary; onOpen: Open }) {
  const pts = useRouterMetrics(id, '1h').data?.points;
  const rx = useMemo(() => (pts ?? []).map((p) => Number(p.wan_rx_bps) || 0), [pts]);
  const tx = useMemo(() => (pts ?? []).map((p) => Number(p.wan_tx_bps) || 0), [pts]);
  return <><WanCol dir="down" bps={s.wan.rx_bps} values={rx} onOpen={onOpen} /><WanCol dir="up" bps={s.wan.tx_bps} values={tx} onOpen={onOpen} /></>;
}
function RouterCardImpl({ r, current, onOpen: open }: { r: RouterView; current?: boolean; onOpen: (id: string, tab?: RouterTab) => void }) {
  const onOpen = useCallback<Open>((tab) => open(r.id, tab), [open, r.id]);
  // 读数只给在线、且采集成功过的；部署中的那台（重新部署时隧道在重启）也先不显示
  const s = r.state === 'online' && r.deploy?.state !== 'running' ? r.summary : null;
  const oc = s?.openclash;
  const ex = s?.exits;
  const tt = tempTone(s?.temp_c);
  return (
    <article className={cn('ncard', current && 'is-current', !s && 'is-quiet')} data-view="large" data-health={routerHealth(r)} style={{ '--rows': 8 } as CSSProperties}>
      <header className="nc-head">
        <span className="nc-kind" role="img" aria-label="软路由" data-tip="软路由（OpenWrt + OpenClash）"><RouterIcon /></span>
        {/* 名字后面不再挂状态标：状态在底部的 pill 与描边上，等待接入时卡片中间还有说明 */}
        <span className="nc-named"><button type="button" className="nc-name" data-tip={`打开 ${r.name} 的详情`} onClick={() => onOpen()}>{r.name}</button></span>
      </header>

      {/* 地址行：WAN 口地址 + WAN / OpenClash 状态标；还没采集过的写隧道端口 */}
      <div className="nc-addr">
        <span className="ellipsis" data-tip={s ? 'WAN 口地址（路由在客户现场内网里的地址）' : '中控上分给这台路由的反向隧道端口'}>{s ? `WAN ${s.wan.ip ?? '—'}` : `隧道端口 ${r.tunnel_port}`}</span>
        {s && <span className="nc-marks">
          {s.wan.up ? <Tag c="green" tip="WAN 口已连接">WAN 正常</Tag> : <Tag c="orange" tip="WAN 口断开：客户现场断网">WAN 断开</Tag>}
          {!oc!.enabled ? <Tag c="gray" tip="OpenClash 没有启用">OpenClash 未启用</Tag>
            : oc!.running ? <Tag c="violet" tip={`OpenClash ${oc!.version ?? ''} · 模式 ${oc!.mode ?? '—'}`}>OpenClash</Tag>
            : <Tag c="orange" tip="OpenClash 已启用，但核心进程没在运行：客户端此时上不了外网">OpenClash 未运行</Tag>}
        </span>}
      </div>

      {!s ? <Hold r={r} /> : <>
      <RouterMeters s={s} />

      {/* WAN 口速率 + 经 OpenClash 代理的速率（↓ 是到客户端的方向）；点击看曲线 */}
      <section className="slot-seg">
        <div className="flow">
          <WanFlow id={r.id} s={s} onOpen={onOpen} />
          <button type="button" className="flow-foot flow-biz" onClick={() => onOpen('charts')}>
            <span className="flow-foot-label"><Shuffle />OpenClash 代理</span>
            <span>↓ <Num f={F.rate(oc!.down_bps)} /> · ↑ <Num f={F.rate(oc!.up_bps)} /></span><span className="sr-only">（查看曲线页签）</span>
          </button>
        </div>
      </section>

      {/* 出口：一格一个出口，亮的是最近一次延迟测试通过的；参数与面板入口不一致的单独提醒 */}
      <section className="slot-seg">
        <button type="button" className="nc-quota" onClick={() => onOpen('exits')}>
          <span className="nc-qhead">
            <span className="nc-qlabel rt-exits"><Waypoints />出口可用
              <b className={cn('num', ex!.alive < ex!.total && 'is-warn')}>{ex!.alive}/{ex!.total}</b>
            </span>
            {ex!.drift > 0
              ? <span className="nc-quse rt-drift" data-tip="这些出口的 server:port 对上了面板入口，但 uuid / reality 参数和入口现在的不一致（入口轮换过凭据或换过线路机），可能已连不上">{ex!.drift} 个参数与入口不一致</span>
              : <span className="nc-quse" data-tip="server:port 对上了面板入口的出口个数（其余是路由上自己加的，中控不管）">{ex!.managed} 个对应面板入口</span>}
          </span>
          <Cells pct={ex!.total ? (ex!.alive / ex!.total) * 100 : null} n={ex!.total || 18} color="var(--ok)" />
          <span className="sr-only">（查看出口页签）</span>
        </button>
      </section>

      <section className="slot-seg nc-run">
        <div className="nc-run-item"><span className="nc-run-label"><Users />客户端</span><span className="nc-run-val" data-tip="在线的局域网设备（DHCP 租约、邻居表与经过 OpenClash 的来源 IP）"><Num f={{ v: String(s.clients), u: '台' }} /></span></div>
        <div className="nc-run-item"><span className="nc-run-label"><Cable />代理连接</span><span className="nc-run-val" data-tip="OpenClash 当前的连接数"><Num f={{ v: oc!.conns.toLocaleString(), u: '条' }} /></span></div>
        <div className="nc-run-item"><span className="nc-run-label"><Thermometer />温度</span><span className="nc-run-val" data-tone={tt} data-tip="CPU / 主板传感器里最高的读数">{s.temp_c != null ? <Num f={{ v: s.temp_c.toFixed(0), u: '°C' }} /> : <span className="t-muted">—</span>}</span></div>
      </section>

      </>}

      <div className="nc-tags">
        <div className="nc-tags-list">
          {r.team ? <Tag c={tagColor(r.team.name)} tip="团队">{r.team.name}</Tag> : <Tag c="gray">未分团队</Tag>}
          {s && <Tag c="gray" tip="路由已连续运行（重启后从 0 算）">运行 {F.str(uptimeFmt(s.uptime_s))}</Tag>}
        </div>
        <span className={cn('nc-price rt-seen', r.state === 'offline' && 'is-bad')} data-tip={r.state === 'pending' ? undefined : '最近一次采集成功的时刻（每 30 秒一次）'}>
          {r.state === 'pending' ? <>创建于 <Seen at={r.created_at} /></> : r.state === 'offline' ? <>最后在线 <Seen at={r.last_seen_at} /></> : <>更新于 <Seen at={r.last_seen_at} /></>}
        </span>
      </div>

      <footer className="nc-foot">
        <RouterStatePill r={r} />
        <div className="nc-foot-right">
          <Button variant="text" size="sm" onClick={() => onOpen()}><PanelRightOpen size={14} /><span>详情</span></Button>
          <Button variant="text" size="sm" onClick={() => onOpen('exits')}><Waypoints size={14} /><span>出口</span></Button>
        </div>
      </footer>
    </article>
  );
}
export const RouterCard = memo(RouterCardImpl);
