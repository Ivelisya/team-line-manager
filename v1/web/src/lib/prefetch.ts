// 跳转前预取：侧栏链接被指针停住（约 60ms，划过不算）、获得焦点、按下时，先把目标页的代码和它首屏的主查询取起来。
// 每次接口约 150ms 往返，点下去时多半已经回来了，新页面挂上就是数据、不先闪骨架。懒加载页面的 import 也从这里拿（App.tsx 的 React.lazy 共用，同一个模块只下载一次）。
// 只预取不带参数的默认视图（侧栏链接就是这样）；已经新鲜的查询 prefetchQuery 不会再取。
import type { QueryClient } from '@tanstack/react-query';
import { plansQuery, relaysQuery, trafficDailyQuery } from './queries';
import { routersQuery } from './routers';

export const pageLoaders = {
  relays: () => import('@/pages/Relays'),
  routers: () => import('@/pages/Routers'),
  provision: () => import('@/pages/Provision'),
  jobs: () => import('@/pages/Jobs'),
  catalog: () => import('@/pages/Catalog'),
  incidents: () => import('@/pages/Incidents'),
  settings: () => import('@/pages/Settings'),
  traffic: () => import('@/pages/Traffic'),
  access: () => import('@/pages/Access'),
  policy: () => import('@/pages/Policy'),
  audit: () => import('@/pages/Audit'),
};

/** 路由第一段 → 页面代码与首屏主查询（首页、机器页常驻主包，机器列表一直在缓存里） */
const ROUTES: Record<string, { load?: () => Promise<unknown>; queries?: (qc: QueryClient) => Promise<unknown>[] }> = {
  nodes: { queries: (qc) => [qc.prefetchQuery(plansQuery)] },
  relays: { load: pageLoaders.relays, queries: (qc) => [qc.prefetchQuery(relaysQuery)] },
  routers: { load: pageLoaders.routers, queries: (qc) => [qc.prefetchQuery(routersQuery)] },
  provision: { load: pageLoaders.provision, queries: (qc) => [qc.prefetchQuery(plansQuery)] },
  jobs: { load: pageLoaders.jobs },
  // 流量页：默认 14 天，KPI 要近 30 天，取数按 30 天（同 TrafficPage）
  traffic: { load: pageLoaders.traffic, queries: (qc) => [qc.prefetchQuery(trafficDailyQuery(30, ''))] },
  access: { load: pageLoaders.access },
  providers: { load: pageLoaders.catalog },
  teams: { load: pageLoaders.catalog },
  incidents: { load: pageLoaders.incidents },
  audit: { load: pageLoaders.audit },
  settings: { load: pageLoaders.settings },
};

function prefetchRoute(qc: QueryClient, to: string) {
  const r = ROUTES[to.split('?')[0]!.split('/')[1] ?? ''];
  if (!r) return;
  // 预取失败不要紧：真进页面时会再取、再下载（失败的提示也在那时出）
  r.load?.().catch(() => {});
  r.queries?.(qc).forEach((p) => p.catch(() => {}));
}

/** 链接上的预取事件：指针停住 60ms 才算（划过侧栏不预取），聚焦、按下立刻 */
export function prefetchHandlers(qc: QueryClient, to: string) {
  let t: ReturnType<typeof setTimeout> | undefined;
  const now = () => { clearTimeout(t); prefetchRoute(qc, to); };
  return {
    onPointerEnter: () => { clearTimeout(t); t = setTimeout(now, 60); },
    onPointerLeave: () => clearTimeout(t),
    onFocus: now,
    onPointerDown: now,
  };
}
