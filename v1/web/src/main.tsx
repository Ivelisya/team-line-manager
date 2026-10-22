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
import './styles/palette.css';
// 覆盖层放在所有组件与页面样式之后（同优先级靠文件顺序胜出）；按需加载的 policy.css、terminal.css 不写与它同名的规则
import './styles/overrides.css';
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

// 发版后旧页面去拉已删掉的懒加载块会失败：直接刷新拿新版本（地址栏已是目标页）。10 秒内刷过一次就不再刷，交给 ErrorBoundary 提示
window.addEventListener('vite:preloadError', () => {
  const last = Number(sessionStorage.getItem('chunk-reload') ?? 0);
  if (Date.now() - last < 10_000) return;
  sessionStorage.setItem('chunk-reload', String(Date.now()));
  window.location.reload();
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
