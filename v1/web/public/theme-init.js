// 首屏前把主题（light/dark/system）与配色解析成 data-theme / data-palette，避免闪烁。
// 之后跟随系统的深浅色变化由 src/lib/theme.ts 接管（切换时整页同时换色）
// 单独成文件、在 index.html 里同步加载（不是内联脚本）：CSP 的 script-src 只许 'self'
// 客户页 /c/ 一律浅色白底：不读管理端存的主题与配色（本机登录过管理端的人打开客户页也不会是深色）；客户页也不引 lib/theme，不随系统深浅色变
var cx = location.pathname.indexOf('/c/') === 0;
try {
  if (cx) {
    document.documentElement.dataset.theme = 'light';
  } else {
    var t = localStorage.getItem('fleet:theme') || 'system';
    document.documentElement.dataset.theme = t === 'system' ? (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light') : t;
    var p = localStorage.getItem('fleet:palette');
    if (p) document.documentElement.dataset.palette = p;
  }
} catch (e) {}
// 首屏就是白底（html.is-cx，样式在 base.css）和中性标题，不先闪一下管理端的底色与「团队线路管理」
if (cx) {
  document.documentElement.classList.add('is-cx');
  document.title = '节点到期时间轴';
}
