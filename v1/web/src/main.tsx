import React from 'react';
import ReactDOM from 'react-dom/client';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter } from 'react-router-dom';
import './styles/tw-base.css';
import './styles/tokens.css';
import './styles/theme.css';
import './styles/base.css';
import './styles/components.css';
import './styles/home.css';
import './styles/cards.css';
import './styles/table.css';
import './styles/pages.css';
import './styles/extras.css';
import './styles/routers.css';
import './styles/tw-utilities.css';
import { App } from './App';
import { ApiError } from './lib/api';
import { TooltipProvider } from './components/ui/overlays';

const qc = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 10_000,
      // 4xx 是确定的结果（未登录、不存在、参数不对），重试没有意义；网络错误和 5xx 重试一次
      retry: (n, err) => !(err instanceof ApiError && err.status >= 400 && err.status < 500) && n < 1,
      refetchOnWindowFocus: true,
    },
  },
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
          <BrowserRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}><App /></BrowserRouter>
        </TooltipProvider>
      </QueryClientProvider>
    </React.StrictMode>,
  );
}
void boot();
