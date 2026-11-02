import * as React from 'react';
import { Link, Route, Routes } from 'react-router-dom';
import { Compass, House } from 'lucide-react';
import { Shell } from './layout/Shell';
import { HomePage } from './pages/Home';
import { NodesPage } from './pages/Nodes';
import { LoginPage } from './pages/Login';
import { Empty } from './components/app/primitives';
import { Button } from './components/ui/button';

// 首页 / 节点 / 登录常驻主包；其余页面按需加载（Shell 里有 Suspense）
const lazyPage = <K extends string>(load: () => Promise<Record<K, React.ComponentType>>, name: K) =>
  React.lazy(() => load().then((m) => ({ default: m[name] })));
const RelaysPage = lazyPage(() => import('./pages/Relays'), 'RelaysPage');
const RoutersPage = lazyPage(() => import('./pages/Routers'), 'RoutersPage');
const ProvisionPage = lazyPage(() => import('./pages/Provision'), 'ProvisionPage');
const JobsPage = lazyPage(() => import('./pages/Jobs'), 'JobsPage');
const CatalogPage = lazyPage(() => import('./pages/Catalog'), 'CatalogPage');
const IncidentsPage = lazyPage(() => import('./pages/Incidents'), 'IncidentsPage');
const SettingsPage = lazyPage(() => import('./pages/Settings'), 'SettingsPage');
const TrafficPage = lazyPage(() => import('./pages/Traffic'), 'TrafficPage');
const AccessPage = lazyPage(() => import('./pages/Access'), 'AccessPage');
const PolicyPage = lazyPage(() => import('./pages/Policy'), 'PolicyPage');
const AuditPage = lazyPage(() => import('./pages/Audit'), 'AuditPage');

/** 没有这个页面（手输错的地址、旧书签）：说清楚，给一个回首页的按钮，不静默跳回首页 */
function NotFound() {
  return <Empty icon={<Compass size={28} />} text="没有这个页面"><Button asChild variant="primary"><Link to="/"><House />回到首页</Link></Button></Empty>;
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
        <Route path="incidents" element={<IncidentsPage />} />
        <Route path="audit" element={<AuditPage />} />
        <Route path="settings" element={<SettingsPage />} />
        <Route path="*" element={<NotFound />} />
      </Route>
    </Routes>
  );
}
