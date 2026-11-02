// 入口只做按路径分流：/c/ 开头是公开的客户页（customerEntry，只带它要用的代码和样式），其余是管理端（adminEntry）。
// 这样客户打开链接不用下载整个管理端。

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
  await (location.pathname.startsWith('/c/') ? import('./customerEntry') : import('./adminEntry'));
}
void boot();
