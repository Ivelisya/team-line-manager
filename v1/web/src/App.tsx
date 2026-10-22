import * as React from 'react';
import { Route, Routes, Navigate } from 'react-router-dom';
import { Shell } from './layout/Shell';
import { HomePage } from './pages/Home';
import { NodesPage } from './pages/Nodes';
import { LoginPage } from './pages/Login';

// 首页 / 节点 / 登录常驻主包；其余页面按需加载（Shell 里有 Suspense）
const lazyPage = <K extends string>(load: () => Promise<Record<K, React.ComponentType>>, name: K) =>
  React.lazy(() => load().then((m) => ({ default: m[name] })));
const RelaysPage = lazyPage(() => import('./pages/Relays'), 'RelaysPage');
const ProvisionPage = lazyPage(() => import('./pages/Provision'), 'ProvisionPage');
const JobsPage = lazyPage(() => import('./pages/Jobs'), 'JobsPage');
const CatalogPage = lazyPage(() => import('./pages/Catalog'), 'CatalogPage');
const IncidentsPage = lazyPage(() => import('./pages/Incidents'), 'IncidentsPage');
const SettingsPage = lazyPage(() => import('./pages/Settings'), 'SettingsPage');
const AccessPage = lazyPage(() => import('./pages/Access'), 'AccessPage');
const PolicyPage = lazyPage(() => import('./pages/Policy'), 'PolicyPage');
const AuditPage = lazyPage(() => import('./pages/Audit'), 'AuditPage');

export function App() {
  return (
    <Routes>
      <Route path="/login" element={<LoginPage />} />
      <Route element={<Shell />}>
        <Route index element={<HomePage />} />
        <Route path="nodes" element={<NodesPage />} />
        <Route path="relays" element={<RelaysPage />} />
        <Route path="provision" element={<ProvisionPage />} />
        <Route path="jobs" element={<JobsPage />} />
        <Route path="jobs/:id" element={<JobsPage />} />
        <Route path="access" element={<AccessPage />} />
        <Route path="access/policy" element={<PolicyPage />} />
        <Route path="catalog" element={<CatalogPage />} />
        <Route path="incidents" element={<IncidentsPage />} />
        <Route path="audit" element={<AuditPage />} />
        <Route path="settings" element={<SettingsPage />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  );
}
