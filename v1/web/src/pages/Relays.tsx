// 线路：每台线路机一张卡——应用状态（配置版本 vs 已生效）+ 入口表（端口、节点、状态、TCP/出口探测、流量）。
// 入口多的线路机（几十条）只先露前 ENTRY_CAP 条，表尾「显示全部 N 条」展开：线路机只有几台，一页里放几个分页器反而难用。
// 有问题的入口（探测不通、应用失败）排在最前、一直露着，不被折叠。点行打开节点抽屉（同机器页），点线路机名打开线路机抽屉。
import * as React from 'react';
import { CheckCircle2, ChevronRight, CircleAlert, Loader2, PackageOpen } from 'lucide-react';
import { useRelays } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import { entryBad, healthLabel, linkCounts, probeSummary, reachWho } from '@/lib/derive';
import { useReachLabel } from '@/lib/probePoints';
import * as F from '@/lib/format';
import type { EntryView, MachineView } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { Tip } from '@/components/ui/overlays';
import { Chip, Due, Empty, ErrorNote, Flag, HealthPill, LinkCounts, Num, PageHead, Pill, TableWrap } from '@/components/app/primitives';
import { NodeHover } from '@/components/app/NodeHover';
import { Waypoints } from 'lucide-react';
import { Skeleton } from '@/components/ui/skeleton';
import { EntryMenuButton, EntryMenus } from '@/components/app/EntryActions';
import { openEvacuate } from '@/lib/nodeActions';
import { movableEntries } from '@/components/app/LifecycleDialogs';
import { ShowAll } from '@/components/app/Pagination';
import { scrollBackTo } from '@/lib/usePage';
import { ENTRY_CAP, RouterMark, activeEntries, entryStatus, probeCell, tcpCell } from '@/features/EntriesTable';
import { useEntryRouters } from '@/lib/routers';
import { HEALTH_TONE } from '@/lib/sort';
import { PlacementPanel } from '@/features/PlacementPanel';
import { scrollBehavior } from '@/lib/utils';

type Open = ReturnType<typeof useDrawer>['open'];
type Users = { id: string; name: string; exit: string }[];

/** 入口一行。memo：实时推送对没变的入口原样返回（live.ts 的 patchEntry），open 是稳定引用，只有变了的那几行重渲染 */
const EntryRow = React.memo(function EntryRow({ e, open, routers }: { e: EntryView; open: Open; routers?: Users }) {
  const who = reachWho(useReachLabel());
  return (
    // 行里的「⋯」由外面的 EntryMenus 接住，不当成点行
    <tr className="is-link" tabIndex={0} onClick={(ev) => { if (!(ev.target as Element).closest('button')) open(e.node.id); }}
      onKeyDown={(ev) => { if (ev.key === 'Enter' && ev.target === ev.currentTarget) open(e.node.id); }}>
      <td className="strong num">{e.port}</td>
      {/* 直出入口的节点就是这台线路机自己 */}
      <td><div className="row" style={{ gap: 0, flexWrap: 'nowrap' }}>
        {e.direct ? <span className="ellipsis" data-tip="从线路机自己的 IP 出去，不经节点">{e.node.name}</span>
          : <NodeHover id={e.node.id} noFocus><span className="ellipsis">{e.node.name}</span></NodeHover>}
        {routers && <RouterMark users={routers} />}
      </div></td>
      <td>{entryStatus(e, who)}</td>
      {/* 停用的入口不再探测：留着的是停用前的旧读数，不显示 */}
      {e.lifecycle === 'disabled' ? <><td className="r t-muted">—</td><td className="r t-muted">—</td></> : <><td className="r">{tcpCell(e)}</td><td className="r">{probeCell(e.egress)}</td></>}
      <td className="r"><Num f={F.bytes(e.traffic.month_up + e.traffic.month_down)} /></td>
      <td className="c"><EntryMenuButton entry={e} /></td>
    </tr>
  );
});

function ApplyStatus({ m }: { m: MachineView }) {
  const r = m.relay; if (!r) return null;
  const pending = r.generation > r.applied_generation;
  const pendingEntries = m.entries.filter((e) => !e.applied && e.lifecycle !== 'disabled').length;
  return (
    <div className="apply-box">
      {/* 带图标的不再画圆点：「配置未应用」原先呼吸点加转圈，两样一起动 */}
      {r.apply_error ? <Pill tone="bad" dot="none"><CircleAlert size={12} />应用失败</Pill>
        : pending ? <Pill tone="info" dot="none"><Loader2 size={12} className="spin" />配置未应用</Pill>
        : <Pill tone="ok" dot="none"><CheckCircle2 size={12} />已应用</Pill>}
      <Tip text="两个数相等表示线路机已同步"><span tabIndex={0}>配置版本 <b className="num" style={{ color: 'var(--fg)' }}>{r.generation}</b> · 已生效 <b className="num" style={{ color: 'var(--fg)' }}>{r.applied_generation}</b></span></Tip>
      {pending && pendingEntries > 0 && <span>{pendingEntries} 条入口应用中</span>}
      <span style={{ marginLeft: 'auto' }} data-tip={r.applied_at ? F.dtFull(r.applied_at) : undefined}>最近应用 {F.dt(r.applied_at)}</span>
      {r.apply_error && <div style={{ flexBasis: '100%', color: 'var(--bad-text)' }}>{r.apply_error}</div>}
    </div>
  );
}

function RelayCard({ m, open, routers }: { m: MachineView; open: Open; routers: Map<string, Users> }) {
  const r = m.relay;
  const dueWarn = !!m.subscription && m.subscription.due_state !== 'ok';
  const [all, setAll] = React.useState(false);
  const card = React.useRef<HTMLElement>(null);
  // 有问题的排前面（稳定：同组内保持原顺序），折叠时也全部露出
  const sorted = React.useMemo(() => [...m.entries.filter(entryBad), ...m.entries.filter((e) => !entryBad(e))], [m.entries]);
  const cap = Math.max(ENTRY_CAP, m.entries.filter(entryBad).length);
  const links = linkCounts(probeSummary(m));
  const who = reachWho(useReachLabel());
  const entries = all ? sorted : sorted.slice(0, cap);
  return (
    <section ref={card} id={`relay-${m.id}`} className="panel relay-card">
      <header className="relay-head">
        <Flag country={m.country} manual={!!m.country_set} ip={m.ip} />
        <button type="button" className="ttl relay-name" onClick={() => open(m.id)}>{m.name}<ChevronRight size={16} aria-hidden /></button>
        <HealthPill m={m} />
        {/* 三段连通数同卡片与抽屉（linkCounts） */}
        {links.reach.total > 0 && <span className="t-sub"><LinkCounts c={links} who={who} /></span>}
        <span className="t-sub">{r?.public_host && r.public_host !== m.ip ? `${r.public_host} · ${m.ip}` : m.ip}</span>
        {m.subscription && <Chip variant={dueWarn ? 'warn' : undefined}><Due days={m.subscription.due_days} state={m.subscription.due_state} tail /></Chip>}
        {m.lifecycle === 'active' && movableEntries(m).length > 0 && <Button size="sm" style={{ marginLeft: 'auto' }} onClick={() => openEvacuate(m.id)}><PackageOpen size={14} />搬空到…</Button>}
      </header>
      <ApplyStatus m={m} />
      {/* 入口的“⋯”菜单与对话框整张卡共用一份 */}
      {/* TableWrap：放得下时表头吸顶，长卡往下滚列名一直在 */}
      <EntryMenus entries={m.entries}><div><TableWrap>
        <table className="dt is-one" style={{ minWidth: 700 }}>
          <colgroup><col style={{ width: 84 }} /><col style={{ width: 160 }} /><col style={{ width: 104 }} /><col style={{ width: 96 }} /><col style={{ width: 96 }} /><col style={{ width: 120 }} /><col style={{ width: 52 }} /></colgroup>
          <thead><tr><th>端口</th><th>节点</th><th>状态</th><th className="r">TCP 延迟</th><th className="r">出口</th><th className="r">本月业务流量</th><th /></tr></thead>
          <tbody>
            {entries.map((e) => <EntryRow key={e.id} e={e} open={open} routers={routers.get(e.id)} />)}
          </tbody>
        </table>
        </TableWrap>
        {m.entries.length === 0 && <Empty text="还没有入口" />}
      </div></EntryMenus>
      {/* 收起后回到这张卡的顶部，不停在下一台线路机中间 */}
      <ShowAll total={m.entries.length} cap={cap} open={all} onToggle={() => { if (all) scrollBackTo(card.current); setAll((v) => !v); }} />
    </section>
  );
}

/** 吸顶跳转条高亮当前看着的那台：各卡与视口上部一条带（顶栏和跳转条之下、到视口 45% 处）相交的第一台。
 *  点了某台就直接高亮它，直到用户自己滚动：最后几台内容短、滚不到顶部，不能让高亮停在别的卡上 */
function useCurrentRelay(idsKey: string) {
  const [cur, setCur] = React.useState<string | null>(null);
  const pinned = React.useRef(false);
  React.useEffect(() => {
    if (!idsKey) return;
    const ids = idsKey.split(','); const seen = new Set<string>();
    const io = new IntersectionObserver((es) => {
      for (const e of es) { const id = e.target.id.slice('relay-'.length); if (e.isIntersecting) seen.add(id); else seen.delete(id); }
      const first = ids.find((id) => seen.has(id)); if (first && !pinned.current) setCur(first);
    }, { rootMargin: '-140px 0px -55% 0px' });
    ids.forEach((id) => { const el = document.getElementById(`relay-${id}`); if (el) io.observe(el); });
    const unpin = () => { pinned.current = false; };
    window.addEventListener('wheel', unpin, { passive: true }); window.addEventListener('touchmove', unpin, { passive: true }); window.addEventListener('keydown', unpin);
    return () => { io.disconnect(); window.removeEventListener('wheel', unpin); window.removeEventListener('touchmove', unpin); window.removeEventListener('keydown', unpin); };
  }, [idsKey]);
  const pick = (id: string) => { pinned.current = true; setCur(id); document.getElementById(`relay-${id}`)?.scrollIntoView({ behavior: scrollBehavior(), block: 'start' }); };
  return [cur, pick] as const;
}

export function RelaysPage() {
  const q = useRelays();
  const { open } = useDrawer();
  const routers = useEntryRouters();
  const list = q.data ?? [];
  const [cur, jump] = useCurrentRelay(list.length > 2 ? list.map((m) => m.id).join() : '');
  return (
    <div className="stack">
      <PageHead title="线路" note={q.data ? <span data-tip={'入口可达、出口通只数在用的入口，停用和待应用的不计；节点通再去掉直出入口（不经节点）'}>{`${list.length} 台线路机 · ${list.reduce((s, m) => s + m.entries.length, 0)} 条入口 · ${list.reduce((s, m) => s + activeEntries(m.entries), 0)} 条在用`}</span> : undefined} />
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {q.isLoading && <Skeleton h={220} />}
      {/* 节点放在哪台线路机最好：换线前先看这里 */}
      {list.length > 1 && <PlacementPanel />}
      {/* 线路机多了竖着排很长：吸顶的一行名字，点一下跳到那张卡；只有故障的写状态 */}
      {list.length > 2 && (
        <nav className="relay-jump" aria-label="线路机">
          {list.map((m) => (
            <button key={m.id} type="button" className="fchip" aria-pressed={cur === m.id} onClick={() => jump(m.id)}>
              {m.name}{HEALTH_TONE[m.health.status] === 'bad' && <span className="relay-jump-bad">{healthLabel(m.health.status)}</span>}
            </button>
          ))}
        </nav>
      )}
      {list.map((m) => <RelayCard key={m.id} m={m} open={open} routers={routers} />)}
      {q.data && list.length === 0 && <Empty icon={<Waypoints size={22} />} text="还没有线路机" hint="去「上架」添加" />}
    </div>
  );
}
