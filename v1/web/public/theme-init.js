// 首屏前把主题（light/dark/system）与配色解析成 data-theme / data-palette，避免闪烁。
// 之后跟随系统的深浅色变化由 src/lib/theme.ts 接管（切换时整页同时换色）
// 单独成文件、在 index.html 里同步加载（不是内联脚本）：CSP 的 script-src 只许 'self'
try {
  var t = localStorage.getItem('fleet:theme') || 'system';
  document.documentElement.dataset.theme = t === 'system' ? (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light') : t;
  var p = localStorage.getItem('fleet:palette');
  if (p) document.documentElement.dataset.palette = p;
} catch (e) {}
