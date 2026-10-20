// live-mode.js — DRAFT for PHASE 2: copy to prototype/original-refined/js/live-mode.js (build.sh then ships it).
//
// The one switch between the prototype (snapshot + simulator, everything clickable) and the read-only
// preview on live data (deploy/v2/dist, globalThis.TEAMOPS_LIVE === true).  In prototype mode every export
// is a no-op, so the prototype, its tools and screenshots keep working unchanged.
//
// Pages never decide read-only on their own: they call guard(key, ctx) at the top of a write handler
// (or confirmAction() does it for them), and the notice + deep link come from ACTIONS below.
// Only existing components: toast (info) with a link action, tonePill, openDialog, note — no new styles
// except the one-line .live-badge narrow rule listed in LIVE-MODE.md §10.
import { toast, openDialog, tonePill, button, esc } from './components.js';

export const isLive = () => globalThis.TEAMOPS_LIVE === true;
export const isReadOnly = isLive;
const T = () => globalThis.TEAMOPS ?? null;

// ─── original console URLs (research: upstream-snapshot/komari-admin-live v5 route tree, plugin-live/public) ──

export const OLD = {
  home: '/admin/home', dashboard: '/admin/dashboard', nodes: '/admin/nodes', lines: '/admin/lines',
  behavior: '/admin/behavior', rules: '/admin/behavior?tab=rules', activity: '/admin/activity', audit: '/admin/lines?tab=audit',
  apiKeys: '/admin/api-keys', login: '/admin', logout: '/api/logout',
};
const qs = (o) => new URLSearchParams(Object.entries(o).filter(([, v]) => v != null && v !== '')).toString();
/** Komari uuid = the old console's `asset` id (boot.js knows the mapping; the snapshot does not carry it). */
const assetOf = (node) => (node ? T()?.assetOf?.(node.id ?? node) ?? null : null);
const nodeUrl = (tab) => ({ node }) => { const a = assetOf(node); return a ? `${OLD.nodes}?${qs({ asset: a, tab })}` : OLD.nodes; };
/** Entry actions live on the bound node's drawer (tab=actions) in the old console; relay master / unbound → lines. */
const entryUrl = ({ entry, node, store }) => {
  const n = node ?? (entry?.bound_node ? store?.node?.(entry.bound_node) : null);
  return n && !n.isRelay ? nodeUrl('actions')({ node: n }) : OLD.lines;
};

/** Every write action the UI shows → label for the notice + where it really lives. */
export const ACTIONS = {
  // provisioning (dashboard 新增节点, nodes 新增节点, lines 新增家宽节点 / 新增线路机 all open openProvisionWizard)
  'provision.node': { label: '新增家宽节点', url: () => OLD.lines },
  'provision.relay': { label: '新增线路机', url: () => OLD.lines },
  'provision.enrollExisting': { label: '登记已存在的入口', url: () => OLD.lines },
  // lines page
  'lines.entry.disable': { label: '停用入口', url: entryUrl },
  'lines.entry.enable': { label: '启用入口', url: entryUrl },
  'lines.entry.reset': { label: '重置用量', url: entryUrl },
  'lines.entry.remark': { label: '编辑备注', url: entryUrl },
  'lines.entry.rotate': { label: '更换连接凭据', url: entryUrl },
  'lines.entry.verify': { label: '重新验证链路', url: entryUrl },
  'lines.relay.restartXray': { label: '重启 Xray', url: () => OLD.lines },
  'lines.relay.retryPanel': { label: '重试面板连接', url: () => OLD.lines },
  'lines.relay.logs': { label: '查看 Xray 日志', url: () => OLD.lines },
  // node drawer (管理 / 连接质量 / 费用 tabs)
  'drawer.disable': { label: '停用入口', url: nodeUrl('actions') },
  'drawer.enable': { label: '启用入口', url: nodeUrl('actions') },
  'drawer.reset': { label: '重置用量', url: nodeUrl('actions') },
  'drawer.remark': { label: '编辑备注', url: nodeUrl('actions') },
  'drawer.rotate': { label: '更换连接凭据', url: nodeUrl('actions') },
  'drawer.verify': { label: '重新验证链路', url: nodeUrl('actions') },
  'drawer.monitor': { label: '监控与维护', url: nodeUrl('actions') },
  'drawer.ackAll': { label: '确认故障', url: nodeUrl('actions') },
  'drawer.ack': { label: '标记已知悉', url: nodeUrl('availability') },
  'drawer.editAsset': { label: '编辑资产', url: nodeUrl('billing') },
  'drawer.renew': { label: '登记续费', url: nodeUrl('billing') },
  // palette
  'palette.verify': { label: '重新验证链路', url: nodeUrl('actions') },
  'palette.restartXray': { label: '重启 Xray', url: () => OLD.lines },
  // behavior
  'behavior.event.ack': { label: '标记已查看', url: ({ node }) => `${OLD.behavior}?${qs({ view: 'events', asset: assetOf(node) })}` },
  'behavior.rules.saveDraft': { label: '保存草稿', url: ({ node }) => `${OLD.behavior}?${qs({ tab: 'rules', asset: assetOf(node) })}` },
  'behavior.rules.apply': { label: '应用访问规则', url: ({ node }) => `${OLD.behavior}?${qs({ tab: 'rules', asset: assetOf(node) })}` },
  'behavior.rules.discard': { label: '丢弃全部草稿', url: () => OLD.rules },
  // onboarding (the demo batch is hidden in live mode; these stay reachable only through the page head)
  'onboarding.batch.new': { label: '新建批次', url: () => OLD.lines },
};
/** Read-only destinations the v2 preview does not have (links, not writes). */
export const READ_LINKS = {
  activity: ({ node }) => `${OLD.activity}?${qs({ asset: assetOf(node) })}`,
  audit: () => OLD.audit,
};
/** Sidebar entries that are Komari native pages: in live mode they are plain links to the original. */
export const NATIVE = {
  'native/servers': '/admin/servers', 'native/metrics': '/admin/metrics-overview', 'native/ping': '/admin/ping',
  'native/exec': '/admin/exec', 'native/sessions': '/admin/sessions', 'api-keys': '/admin/api-keys', 'native/terminal': '/terminal',
  'native/notify-0': '/admin/notification/channels', 'native/notify-1': '/admin/notification/general',
  'native/notify-2': '/admin/notification/offline', 'native/notify-3': '/admin/notification/load',
  'native/theme-0': '/admin/themes', 'native/theme-1': '/admin/market/themes',
  'native/plugin-0': '/admin/plugins', 'native/plugin-1': '/admin/market/plugins', 'native/plugin-2': '/admin/plugins/config',
  'native/system-0': '/admin/settings/site', 'native/system-1': '/admin/settings/general', 'native/system-2': '/admin/settings/sign-on',
  'native/system-3': '/admin/settings/xtermjs', 'native/system-4': '/admin/settings/metrics', 'native/system-5': '/admin/logs',
  'native/system-6': '/admin/pprof', 'native/account-0': '/admin/account', 'native/account-1': '/admin/about',
};

export function oldUrl(key, ctx = {}) {
  try { return ACTIONS[key]?.url(ctx) ?? OLD.home; } catch { return OLD.home; }
}
const openOld = (url) => { globalThis.open?.(url, '_blank', 'noopener'); };

/** The single notice every write action shows in live mode. */
export function readOnlyNotice(key, ctx = {}) {
  const a = ACTIONS[key];
  const url = oldUrl(key, ctx);
  return toast({ message: `只读预览：请在原版后台操作${a ? `（${a.label}）` : ''}`, tone: 'info', action: { label: '打开原版', run: () => openOld(url) }, timeout: 8000 });
}
/** At the top of a write handler: `if (guard('lines.entry.enable', { entry, store })) return;` */
export function guard(key, ctx) {
  if (!isReadOnly()) return false;
  readOnlyNotice(key, ctx);
  return true;
}

/** Top-bar badge (existing pill, info tone).  app.js shellHtml: `${liveBadge()}` first in .topbar-right. */
export function liveBadge() {
  if (!isLive()) return '';
  return `<span class="live-badge">${tonePill('只读预览 · 实时数据', 'info', { title: '数据来自运营服务的只读接口，每 10 秒刷新；写操作请在原版后台进行' })}</span>`;
}

/** Login overlay (401 at boot or while polling).  Uses openDialog; no close button: the data stopped. */
let authShown = false;
export function showAuth({ message = '请先登录管理员账户', link = OLD.login } = {}) {
  if (authShown) return;
  authShown = true;
  openDialog({
    title: message,
    size: 'sm',
    body: `<p>${esc('登录状态已失效，实时数据已停止刷新。登录后回到本页即可继续。')}</p>`,
    foot: `${button({ label: '去登录', variant: 'primary', attrs: { 'data-live': 'login' } })}`,
    init(el) { el.addEventListener('click', (e) => { if (e.target.closest('[data-live="login"]')) location.assign(link); }); },
    onClose() { authShown = false; },
  });
}

/** Once after the poller starts (app.js boot): auth overlay, error / recovery notes. */
export function installLiveMode(app) {
  if (!isLive()) return () => {};
  const t = T();
  if (t?.authError) showAuth(t.authError);
  const offs = [
    t?.on?.('auth', (d) => showAuth(d)) ?? (() => {}),
    t?.on?.('error', (d) => { if (d.failing === 3) toast({ message: `实时数据读取失败：${d.message}。页面保留最后一次的数据`, tone: 'bad' }); }) ?? (() => {}),
    t?.on?.('recover', () => toast({ message: '实时数据已恢复', tone: 'ok' })) ?? (() => {}),
  ];
  return () => offs.forEach((f) => f());
}
