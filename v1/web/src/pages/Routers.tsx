// 软路由：客户现场的 OpenWrt + OpenClash，一台一张卡（外壳同首页节点卡）：资源、WAN 与代理速率、出口可用、客户端。
// 中控经反向隧道每 30 秒采集一次，变化随 routers 事件推来；接入有两种方式（一键部署 / 在路由上执行命令），见 RouterDialogs。
import * as React from 'react';
import { Plus, Router as RouterIcon, Shuffle, Users, Waypoints } from 'lucide-react';
import * as F from '@/lib/format';
import { useTeams } from '@/lib/queries';
import { fleetExits, setRouterOrder, useRouterDrawer, useRouters, type RouterView } from '@/lib/routers';
import { Button } from '@/components/ui/button';
import { Segmented } from '@/components/ui/overlays';
import { Skeleton } from '@/components/ui/skeleton';
import { DotCount, Empty, ErrorNote, Num, PageHead, Panel, Stat } from '@/components/app/primitives';
import { RouterCard } from '@/components/app/RouterCard';
import { AddRouterDialog } from '@/features/RouterDialogs';

const NO_TEAM = '__none__';
/** 页头的 KPI：各状态的台数，在线的那些出口、客户端与代理速率的合计（离线的读数是旧的，不算） */
function summarize(list: RouterView[]) {
  const o = { online: 0, offline: 0, pending: 0, alive: 0, total: 0, drift: 0, clients: 0, down_bps: 0, up_bps: 0 };
  for (const r of list) {
    o[r.state]++;
    if (r.state !== 'online' || !r.summary) continue;
    const f = fleetExits(r.summary);
    o.alive += f.alive; o.total += f.total; o.drift += r.summary.exits.drift; o.clients += r.summary.clients;
    o.down_bps += r.summary.openclash.down_bps; o.up_bps += r.summary.openclash.up_bps;
  }
  return o;
}

export function RoutersPage() {
  const q = useRouters();
  const teams = useTeams().data;
  const { id: openId, open } = useRouterDrawer();
  const [team, setTeam] = React.useState('all');
  const [adding, setAdding] = React.useState(false);
  const all = q.data ?? [];
  const sum = React.useMemo(() => summarize(all), [all]);
  // 分组只列有路由的团队（大多数团队没有软路由），另加「未分团队」
  const groups = React.useMemo(() => {
    const n = (id: string | null) => all.filter((r) => (r.team?.id ?? null) === id).length;
    const items = [{ value: 'all', label: '全部', count: all.length }, ...(teams ?? []).filter((t) => n(t.id) > 0).map((t) => ({ value: t.id, label: t.name, count: n(t.id) }))];
    const none = n(null);
    return none ? [...items, { value: NO_TEAM, label: '未分团队', count: none }] : items;
  }, [all, teams]);
  // 当前分组没了（最后一台删掉、改了团队）就回到全部
  const cur = groups.some((g) => g.value === team) ? team : 'all';
  const shown = cur === 'all' ? all : all.filter((r) => (r.team?.id ?? NO_TEAM) === cur);
  // 抽屉的上一台 / 下一台按这一页看到的顺序走；离开页面时清掉
  const order = shown.map((r) => r.id).join();
  React.useEffect(() => { setRouterOrder(order ? order.split(',') : null); }, [order]);
  React.useEffect(() => () => setRouterOrder(null), []);
  const add = <Button onClick={() => setAdding(true)}><Plus />接入软路由</Button>;
  return (
    <div className="stack">
      <PageHead title="软路由" tools={all.length > 0 && add} />
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {/* 总览：同首页的 KPI 卡（在线 / 出口可用 / 客户端 / 代理速率） */}
      {all.length > 0 && (
        <div className="stat-grid">
          <Stat tone="blue" icon={<RouterIcon />} label="软路由在线" value={sum.online} unit={`/ ${all.length}`}
            caption={<><DotCount tone="bad" n={sum.offline} label="离线" />{sum.pending > 0 && <DotCount tone="neutral" n={sum.pending} label="等待接入" />}</>} />
          <Stat tone="teal" icon={<Waypoints />} label="出口可用" value={sum.alive} unit={`/ ${sum.total}`}
            caption={<><DotCount tone="warn" n={sum.total - sum.alive} label="不通" />{sum.drift > 0 && <DotCount tone="warn" n={sum.drift} label="与入口不一致" />}</>} />
          <Stat tone="violet" icon={<Users />} label="客户端在线" value={sum.clients} unit="台" caption="在线的软路由里" />
          <Stat tone="sky" icon={<Shuffle />} label="代理速率" info="在线软路由经 OpenClash 的流量合计" value={F.inScale(sum.down_bps + sum.up_bps, F.rateScale(sum.down_bps + sum.up_bps))} unit={F.rateScale(sum.down_bps + sum.up_bps).u}
            caption={<><span>下载 <Num f={F.rate(sum.down_bps)} /></span><span>上传 <Num f={F.rate(sum.up_bps)} /></span></>} />
        </div>
      )}
      {/* 团队分段按内容宽、靠左（同机器页的分段），不拉满整行 */}
      {groups.length > 2 && <div className="hgroups" role="group" aria-label="筛选"><Segmented label="团队" value={cur} onChange={setTeam} items={groups} /></div>}
      {q.isLoading && <div className="cards" data-view="large">{Array.from({ length: 3 }, (_, i) => <Skeleton className="h-[520px]" style={{ borderRadius: 'var(--r-card)', marginBottom: 'var(--grid-gap)' }} key={i} />)}</div>}
      {q.data && all.length === 0 && (
        // 空状态放在卡片底板上（同作业页）
        <Panel><Empty icon={<RouterIcon size={22} />} text="还没有接入软路由">{add}</Empty></Panel>
      )}
      {shown.length > 0 && <div className="cards page-in" data-view="large">{shown.map((r) => <RouterCard key={r.id} r={r} current={r.id === openId} onOpen={open} />)}</div>}
      <AddRouterDialog open={adding} onOpenChange={setAdding} />
    </div>
  );
}
