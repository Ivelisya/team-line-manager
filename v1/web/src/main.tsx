// 入口只做按路径分流：/c/ 开头是公开的客户页（customerEntry，只带它要用的代码和样式），其余是管理端（adminEntry）。
// 这样客户打开链接不用下载整个管理端。
import { openEarly } from './lib/streamGate';

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
    const { installMock } = await import('@mock/server');
    installMock();
  }
  const customer = location.pathname.startsWith('/c/');
  // 管理端：实时流在下载管理端代码之前就开始连（握手与代码下载、/auth/me、/meta 并行），首屏的机器类查询连上再取、不用连上后再补取一遍（lib/streamGate.ts）。
  // 登录页不连（没登录的握手是 401）
  if (!customer && location.pathname !== '/login') openEarly();
  await (customer ? import('./customerEntry') : import('./adminEntry'));
}
void boot();
