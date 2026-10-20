import { Route, Routes, Navigate } from 'react-router-dom';
import { Shell } from './layout/Shell';
import { HomePage } from './pages/Home';
import { NodesPage } from './pages/Nodes';
import { RelaysPage } from './pages/Relays';
import { ProvisionPage } from './pages/Provision';
import { JobsPage } from './pages/Jobs';
import { CatalogPage } from './pages/Catalog';
import { IncidentsPage } from './pages/Incidents';
import { LoginPage } from './pages/Login';
import { AccessPage } from './pages/Access';

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
        <Route path="catalog" element={<CatalogPage />} />
        <Route path="incidents" element={<IncidentsPage />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  );
}
