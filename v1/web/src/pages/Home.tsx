// 首页：KPI + 团队筛选 + 视图切换（大卡片/小卡片/紧凑）+ 节点卡片网格。
// 默认排序：线路机 → 团队 → 编号；故障/离线不下沉。其它排序是用户的显式选择。
import * as React from 'react';
import { AlertTriangle, Inbox } from 'lucide-react';
import { useMachines, useOverview, usePlans, useTeams } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import { monthTraffic } from '@/lib/derive';
import type { MachineView, Plan } from '@/lib/types';
import { CountUp, DotCount, Empty, ErrorNote, FilterChip, KpiTile, Num, PageHead } from '@/components/app/primitives';
import { NodeCard, SkeletonCard, type CardView } from '@/components/app/NodeCard';
import { Segmented } from '@/components/ui/overlays';
import { Select } from '@/components/ui/select';

type SortKey = 'default' | 'name' | 'due' | 'traffic' | 'issues';
const SORTS: { value: SortKey; label: string }[] = [
  { value: 'default', label: '默认排序' }, { value: 'name', label: '名称' }, { value: 'due', label: '到期最近' },
  { value: 'traffic', label: '本月流量最多' }, { value: 'issues', label: '异常优先' },
];
const VIEWS: CardView[] = ['large', 'small', 'compact'];
const EMPTY_PLANS: Plan[] = [];
const store = {
  /** 读本地偏好；值不在 allowed 里（旧版本或手改的脏值）一律回退默认 */
  get: <T extends string>(k: string, d: T, allowed: readonly T[]): T => { try { const v = localStorage.getItem(k) as T | null; return v && allowed.includes(v) ? v : d; } catch { return d; } },
  set: (k: string, v: string) => { try { localStorage.setItem(k, v); } catch { /* 忽略 */ } },
};
const RANK: Record<string, number> = { down: 0, degraded: 1, unknown: 2, maintenance: 3, ok: 4, inactive: 5 };

const isIssue = (m: MachineView) => m.health.status === 'down' || m.health.status === 'degraded';

function sortList(list: MachineView[], key: SortKey): MachineView[] {
  if (key === 'default') return list;
  const zh = new Intl.Collator('zh-Hans-CN', { numeric: true });
  const c: Record<Exclude<SortKey, 'default'>, (a: MachineView, b: MachineView) => number> = {
    name: (a, b) => zh.compare(a.name, b.name),
    due: (a, b) => (a.subscription?.due_days ?? 1e9) - (b.subscription?.due_days ?? 1e9),
    traffic: (a, b) => { const x = monthTraffic(a); const y = monthTraffic(b); return y.up + y.down - (x.up + x.down); },
    issues: (a, b) => RANK[a.health.status] - RANK[b.health.status],
  };
  return [...list].sort(c[key]);
}

export function HomePage() {
  const machines = useMachines('live');
  const teams = useTeams();
  const ov = useOverview();
  const plans = usePlans().data ?? EMPTY_PLANS;
  const { id: openId, open } = useDrawer();
  const [view, setView] = React.useState<CardView>(() => store.get('fleet:view', 'large', VIEWS));
  const [sort, setSort] = React.useState<SortKey>(() => store.get('fleet:sort', 'default', SORTS.map((x) => x.value)));
  const [team, setTeam] = React.useState<string>('all');
  const [onlyIssues, setOnlyIssues] = React.useState(false);
  const setV = (v: CardView) => { setView(v); store.set('fleet:view', v); };
  const setS = (v: SortKey) => { setSort(v); store.set('fleet:sort', v); };

  const all = React.useMemo(() => (machines.data ?? []).filter((m) => m.lifecycle !== 'retired'), [machines.data]); // 已退役不进首页
  const counts = React.useMemo(() => ({
    all: all.length, relay: all.filter((m) => m.kind === 'relay').length,
    byTeam: new Map((teams.data ?? []).map((t) => [t.id, all.filter((m) => m.team?.id === t.id).length])),
    issues: all.filter(isIssue).length,
  }), [all, teams.data]);

  const shown = React.useMemo(() => {
    let l = all;
    if (team === 'relay') l = l.filter((m) => m.kind === 'relay');
    else if (team !== 'all') l = l.filter((m) => m.team?.id === team);
    if (onlyIssues) l = l.filter(isIssue);
    return sortList(l, sort);
  }, [all, team, onlyIssues, sort]);

  const o = ov.data;
  const nodes = o?.nodes; const relays = o?.relays;
  const issues = (nodes?.down ?? 0) + (nodes?.degraded ?? 0) + (relays?.down ?? 0) + (relays?.degraded ?? 0);
  const tr = o?.traffic_month;
  const loading = !o;

  return (
    <div className="stack">
      <PageHead title="首页" note={o ? `${nodes!.total} 个节点 · ${relays!.total} 台线路机` : undefined}
        tools={<>
          <Select sm value={sort} onChange={(v) => setS(v as SortKey)} options={SORTS} className="w-[168px]" aria-label="排序" />
          <Segmented label="卡片视图" value={view} onChange={setV} items={[{ value: 'large', label: '大卡片' }, { value: 'small', label: '小卡片' }, { value: 'compact', label: '紧凑' }]} />
        </>} />

      <div className="kpi-grid-wrap">
        <div className="kpi-grid">
          <KpiTile loading={loading} label="节点正常" value={<CountUp value={nodes?.ok} />} unit={`/ ${nodes?.total}`}
            sub={nodes && <><DotCount tone="warn" n={nodes.degraded} label="降级" /><DotCount tone="bad" n={nodes.down} label="故障" /><DotCount tone="neutral" n={nodes.unknown} label="未知" /></>} />
          <KpiTile loading={loading} label="线路机正常" value={<CountUp value={relays?.ok} />} unit={`/ ${relays?.total}`}
            sub={relays && <><DotCount tone="warn" n={relays.degraded} label="降级" /><DotCount tone="bad" n={relays.down} label="故障" /></>} />
          <KpiTile loading={loading} label="故障与降级" value={<CountUp value={issues} />} to="/incidents" sub={issues ? '点击查看告警' : '一切正常'} />
          <KpiTile loading={loading} label="7 天内到期" value={<CountUp value={o?.due_7d} />} to="/nodes" sub="需要续费的机器" />
          <KpiTile loading={loading} label="每月费用" value={o && (o.monthly_cost.length ? o.monthly_cost.map((c) => F.money(c.cents, c.currency)).join(' + ') : F.money(0, 'USD'))} sub={o && o.monthly_cost.length > 1 ? '多币种分别合计' : '在用机器按月折算'} />
          <KpiTile loading={loading} label="本月业务流量" value={tr && <CountUp value={tr.up + tr.down} format={(n) => F.bytes(n).v} />} unit={tr && F.bytes(tr.up + tr.down).u}
            sub={tr && <><span>↓ <Num f={F.bytes(tr.down)} /></span><span>↑ <Num f={F.bytes(tr.up)} /></span></>} />
          <KpiTile loading={loading} label="网卡速率" value={o && <CountUp value={o.net_rx_bps + o.net_tx_bps} format={(n) => F.rate(n).v} />} unit={o && F.rate(o.net_rx_bps + o.net_tx_bps).u}
            sub={o && <><span>↓ <Num f={F.rate(o.net_rx_bps)} /></span><span>↑ <Num f={F.rate(o.net_tx_bps)} /></span></>} />
          <KpiTile loading={loading} label="上架 · 下线中" value={o && <CountUp value={o.provisioning + o.retiring} />} to="/jobs" sub={o && `上架 ${o.provisioning} · 下线 ${o.retiring}`} />
        </div>
      </div>

      <div className="fbar" role="group" aria-label="筛选">
        <span className="fbar-label">分组</span>
        <FilterChip pressed={team === 'all'} onClick={() => setTeam('all')} count={counts.all}>全部</FilterChip>
        <FilterChip pressed={team === 'relay'} onClick={() => setTeam('relay')} count={counts.relay}>线路机</FilterChip>
        {(teams.data ?? []).map((t) => <FilterChip key={t.id} pressed={team === t.id} onClick={() => setTeam(t.id)} count={counts.byTeam.get(t.id) ?? 0}>{t.name}</FilterChip>)}
        <span className="fbar-div" />
        <FilterChip pressed={onlyIssues} onClick={() => setOnlyIssues((v) => !v)} count={counts.issues}><AlertTriangle size={13} />仅看异常</FilterChip>
      </div>

      {machines.isError && <ErrorNote error={machines.error} onRetry={() => void machines.refetch()} />}
      {machines.isLoading ? (
        <div className="cards" data-view={view}>{Array.from({ length: 6 }, (_, i) => <SkeletonCard key={i} view={view} />)}</div>
      ) : shown.length === 0 ? (
        <Empty icon={<Inbox size={28} />} text={all.length ? '没有符合筛选条件的机器' : '还没有机器，去“上架”添加第一台'} />
      ) : (
        <div className="cards" data-view={view}>
          {shown.map((m) => <NodeCard key={m.id} m={m} view={view} current={m.id === openId} plans={plans} onOpen={open} />)}
        </div>
      )}
    </div>
  );
}
