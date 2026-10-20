import React from 'react';
import ReactDOM from 'react-dom/client';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter } from 'react-router-dom';
import './styles/tw-base.css';
import './styles/tokens.css';
import './styles/base.css';
import './styles/components.css';
import './styles/cards.css';
import './styles/table.css';
import './styles/pages.css';
import './styles/tw-utilities.css';
import { App } from './App';
import { TooltipProvider } from './components/ui/overlays';

const qc = new QueryClient({
  defaultOptions: { queries: { staleTime: 10_000, retry: 1, refetchOnWindowFocus: true } },
});

async function boot() {
  // VITE_MOCK=1：用浏览器内 mock 代替后端（开发用）
  if (import.meta.env.VITE_MOCK === '1') {
    const { installMock } = await import('./mock/server');
    installMock();
  }
  ReactDOM.createRoot(document.getElementById('root')!).render(
    <React.StrictMode>
      <QueryClientProvider client={qc}>
        <TooltipProvider>
          <BrowserRouter><App /></BrowserRouter>
        </TooltipProvider>
      </QueryClientProvider>
    </React.StrictMode>,
  );
}
void boot();
