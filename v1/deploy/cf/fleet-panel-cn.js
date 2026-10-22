// Cloudflare Worker「fleet-panel-cn」，路由 cn.kismet613.com/*（kismet613.com 区），见 docs/OPS.md §11。
// cn.kismet613.com 是灰云 A 记录，指向优选的 Cloudflare IP，大陆用户落到更近的 CF 机房；这里把请求原样转给橙云的
// panel.kismet613.com（同一个 zone，源站收到的 CF-Connecting-IP 仍是用户真实 IP），WebSocket 升级请求同样透传。
// 改了要在 CF 控制台「Workers 和 Pages → fleet-panel-cn」里重新部署（兼容日期 2025-06-01，模块语法）。
export default {
  async fetch(req) {
    const u = new URL(req.url);
    if (u.protocol === 'http:') {
      u.protocol = 'https:';
      return Response.redirect(u.toString(), 301);
    }
    u.hostname = 'panel.kismet613.com';
    return fetch(new Request(u, req));
  },
};
