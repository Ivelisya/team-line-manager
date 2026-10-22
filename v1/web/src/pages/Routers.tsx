// 软路由：客户现场的 OpenWrt + OpenClash，一台一张卡（外壳同首页节点卡）：资源、WAN 与代理速率、出口可用、客户端。
// 中控经反向隧道每 30 秒采集一次，变化随 routers 事件推来；接入有两种方式（一键部署 / 在路由上执行命令），见 RouterDialogs。
import * as React from 'react';
import { Plus, Router as RouterIcon } from 'lucide-react';
import { useTeams } from '@/lib/queries';
import { useRouterDrawer, useRouters, type RouterView } from '@/lib/routers';
import { Button } from '@/components/ui/button';
import { Segmented } from '@/components/ui/overlays';
import { Skeleton } from '@/components/ui/skeleton';
import { Empty, ErrorNote, PageHead } from '@/components/app/primitives';
import { RouterCard } from '@/components/app/RouterCard';
import { AddRouterDialog } from '@/features/RouterDialogs';

const NO_TEAM = '__none__';
/** 页头那句话：各状态的台数，在线的那些出口与客户端的合计（离线的读数是旧的，不算） */
function summarize(list: RouterView[]) {
  const o = { online: 0, offline: 0, pending: 0, alive: 0, total: 0, drift: 0, clients: 0 };
  for (const r of list) {
    o[r.state]++;
    if (r.state !== 'online' || !r.summary) continue;
    o.alive += r.summary.exits.alive; o.total += r.summary.exits.total; o.drift += r.summary.exits.drift; o.clients += r.summary.clients;
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
  const add = <Button variant="primary" onClick={() => setAdding(true)}><Plus />接入软路由</Button>;
  return (
    <div className="stack">
      <PageHead title="软路由" tools={all.length > 0 && add} />
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {/* 总览：大号台数 + 一句话，不带框 */}
      {all.length > 0 && (
        <div className="rt-hero">
          <div className="rt-hero-main"><b className="rt-hero-num num">{all.length}</b><span className="rt-hero-unit">台软路由</span></div>
          <p className="rt-hero-sum">
            {sum.online} 台在线{sum.offline > 0 && <> · <span className="rt-bad">{sum.offline} 台离线</span></>}{sum.pending > 0 && <> · {sum.pending} 台等待接入</>}
            {sum.total > 0 && <> · 出口 <b className={sum.alive < sum.total ? 'rt-warn' : undefined}>{sum.alive}/{sum.total}</b> 可用 · 客户端 {sum.clients} 台</>}
            {sum.drift > 0 && <> · <span className="rt-warn">{sum.drift} 个出口参数与面板入口不一致</span></>}
          </p>
        </div>
      )}
      {groups.length > 2 && <div className="hgroups" role="group" aria-label="筛选"><Segmented label="团队" value={cur} onChange={setTeam} items={groups} className="seg-fill" /></div>}
      {q.isLoading && <div className="cards" data-view="large">{Array.from({ length: 3 }, (_, i) => <Skeleton key={i} h={520} style={{ borderRadius: 'var(--r-card)', marginBottom: 'var(--grid-gap)' }} />)}</div>}
      {q.data && all.length === 0 && (
        <Empty icon={<RouterIcon size={22} />} text="还没有接入软路由">{add}</Empty>
      )}
      {shown.length > 0 && <div className="cards page-in" data-view="large">{shown.map((r) => <RouterCard key={r.id} r={r} current={r.id === openId} onOpen={open} />)}</div>}
      <AddRouterDialog open={adding} onOpenChange={setAdding} />
    </div>
  );
}
