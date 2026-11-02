// 首屏前把主题（light/dark/system）与配色解析成 data-theme / data-palette，避免闪烁。
// 之后跟随系统的深浅色变化由 src/lib/theme.ts 接管（切换时整页同时换色）
// 单独成文件、在 index.html 里同步加载（不是内联脚本）：CSP 的 script-src 只许 'self'
try {
  var t = localStorage.getItem('fleet:theme') || 'system';
  document.documentElement.dataset.theme = t === 'system' ? (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light') : t;
  var p = localStorage.getItem('fleet:palette');
  if (p) document.documentElement.dataset.palette = p;
} catch (e) {}
// 客户页 /c/：首屏就是白底（html.is-cx，样式在 base.css）和中性标题，不先闪一下管理端的底色与「团队线路管理」
if (location.pathname.indexOf('/c/') === 0) {
  document.documentElement.classList.add('is-cx');
  document.title = '节点到期时间轴';
}
