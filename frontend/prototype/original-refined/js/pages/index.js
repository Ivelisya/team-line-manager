// pages/index.js — 页面注册表。新页面在这里加一行（懒加载），路由 #/<key> 就会挂载它；
// 没有登记的页面显示“原型未包含此页”的空状态（不去请求不存在的文件，控制台保持干净）。
export const PAGE_MODULES = {
  home: () => import('./home.js'),
  dashboard: () => import('./dashboard.js'),
  nodes: () => import('./nodes.js'),
  lines: () => import('./lines.js'),
  behavior: () => import('./behavior.js'),
  onboarding: () => import('./onboarding.js'),
  alerts: () => import('./alerts.js'),
  notify: () => import('./notify.js'),
  plans: () => import('./plans.js'),
  'api-keys': () => import('./api-keys.js'),
  account: () => import('./account.js'),
};
