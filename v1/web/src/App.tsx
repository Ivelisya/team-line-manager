import * as React from 'react';
import { Link, Route, Routes } from 'react-router';
import { Compass, House } from 'lucide-react';
import { Shell } from './layout/Shell';
import { HomePage } from './pages/Home';
import { NodesPage } from './pages/Nodes';
import { LoginPage } from './pages/Login';
import { Empty } from './components/app/primitives';
import { Button } from './components/ui/button';
import { pageLoaders as L } from './lib/prefetch';

// 首页 / 节点 / 登录常驻主包；其余页面按需加载（Shell 里有 Suspense）。import 在 lib/prefetch（侧栏悬停预取共用）
const lazyPage = <K extends string>(load: () => Promise<Record<K, React.ComponentType>>, name: K) =>
  React.lazy(() => load().then((m) => ({ default: m[name] })));
const RelaysPage = lazyPage(L.relays, 'RelaysPage');
const RoutersPage = lazyPage(L.routers, 'RoutersPage');
const ProvisionPage = lazyPage(L.provision, 'ProvisionPage');
const JobsPage = lazyPage(L.jobs, 'JobsPage');
const CatalogPage = lazyPage(L.catalog, 'CatalogPage');
const TeamsPage = lazyPage(L.catalog, 'TeamsPage');
const ProvidersPage = lazyPage(L.catalog, 'ProvidersPage');
const IncidentsPage = lazyPage(L.incidents, 'IncidentsPage');
const SettingsPage = lazyPage(L.settings, 'SettingsPage');
const TrafficPage = lazyPage(L.traffic, 'TrafficPage');
const AccessPage = lazyPage(L.access, 'AccessPage');
const PolicyPage = lazyPage(L.policy, 'PolicyPage');
const AuditPage = lazyPage(L.audit, 'AuditPage');

/** 没有这个页面（手输错的地址、旧书签）：说清楚，给一个回首页的按钮，不静默跳回首页 */
function NotFound() {
  return <div className="grid min-h-[60vh] place-content-center"><Empty icon={<Compass size={28} />} text="没有这个页面"><Button render={<Link to="/" />}><House />回到首页</Button></Empty></div>;
}

export function App() {
  return (
    <Routes>
      <Route path="/login" element={<LoginPage />} />
      <Route element={<Shell />}>
        <Route index element={<HomePage />} />
        <Route path="nodes" element={<NodesPage />} />
        <Route path="relays" element={<RelaysPage />} />
        <Route path="routers" element={<RoutersPage />} />
        <Route path="provision" element={<ProvisionPage />} />
        <Route path="jobs" element={<JobsPage />} />
        <Route path="jobs/:id" element={<JobsPage />} />
        <Route path="traffic" element={<TrafficPage />} />
        <Route path="access" element={<AccessPage />} />
        <Route path="access/policy" element={<PolicyPage />} />
        <Route path="catalog" element={<CatalogPage />} />
        <Route path="teams" element={<TeamsPage />} />
        <Route path="providers" element={<ProvidersPage />} />
        <Route path="incidents" element={<IncidentsPage />} />
        <Route path="audit" element={<AuditPage />} />
        <Route path="settings" element={<SettingsPage />} />
        <Route path="*" element={<NotFound />} />
      </Route>
    </Routes>
  );
}
