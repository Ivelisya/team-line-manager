// 管理端入口（main.tsx 按路径动态加载；/c/ 开头的客户页不走这里，不下载管理端的代码与样式）
import React from 'react';
import ReactDOM from 'react-dom/client';
import { QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter } from 'react-router-dom';
// 样式入口：层的安排、各份样式的先后都在 styles/admin.css 里（覆盖层 overrides.css 的说明也在那里）
import './styles/admin.css';
import { App } from './App';
import { qc } from './queryClient';
import { TooltipProvider } from './components/ui/tooltip';

// 根滚动条槽宽（覆盖式滚动条为 0），弹层锁滚动时按它补回右边距（base.css）。槽宽只随缩放（devicePixelRatio）变、不随窗口尺寸变：
// 只在缩放比变化时重量（拖窗口时不再每帧插节点强制布局）
const measureScrollbar = () => {
  const d = document.createElement('div');
  d.style.cssText = 'position:absolute;top:-200px;width:100px;height:100px;overflow:scroll;scrollbar-width:auto;visibility:hidden';
  document.body.append(d);
  document.documentElement.style.setProperty('--sbw', `${d.offsetWidth - d.clientWidth}px`);
  d.remove();
  matchMedia(`(resolution: ${devicePixelRatio}dppx)`).addEventListener('change', measureScrollbar, { once: true });
};
measureScrollbar();

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={qc}>
      <TooltipProvider>
        <BrowserRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}><App /></BrowserRouter>
      </TooltipProvider>
    </QueryClientProvider>
  </React.StrictMode>,
);
