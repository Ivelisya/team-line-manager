// live-mode.js — 只读预览（实时数据）开关，见 deploy/v2/LIVE-MODE.md。
//
// The one switch between the prototype (snapshot + simulator, everything clickable) and the read-only
// preview on live data (deploy/v2/dist, globalThis.TEAMOPS_LIVE === true).  In prototype mode every export
// is a no-op, so the prototype, its tools and screenshots keep working unchanged.
//
// Pages never decide read-only on their own: they call guard(key, ctx) at the top of a write handler
// (or confirmAction() does it for them), and the notice + deep link come from ACTIONS below.
// Only existing components: toast (info) with a link action, the pill, openDialog, note — the only new CSS
// is the badge's narrow-screen rule in css/shell.css.
// 直接从 ui/* 导入（不经 components.js）：ui/layers.js 的 confirmAction / toast 反过来引用本模块，
// 两边都只在调用时用到对方，模块循环是安全的。
import { toast, openDialog } from './ui/layers.js';
import { button } from './ui/controls.js';
import { esc } from './ui/dom.js';
import { showLogin, fleetLogin } from './login.js';

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

/**
 * Every write action the UI shows → label for the notice + where it really lives.
 * cls 'a' = only the team-ops database (asset edit, renewal, incident / behavior ack, node profile …);
 * cls 'b' = touches a relay (entries, Xray, access rules, provisioning): the relays' Xray moved to the new system, so these
 * go to fleetd's native API (fleet/API.md §9.1, async ops; live-ops.js) — `op` names the fleetd capability; they work once
 * fleetd has it (TEAMOPS.fleet.has(op)), else the notice says the new backend has not opened it yet.  No `op` = no API
 * planned (`note` says why).
 * wired: true = class a implemented (live-writes.js: fleetd native when it has it, else the legacy endpoint); active only
 * when TEAMOPS_WRITES is on.
 * LIVE-MODE.md §13 has the table with endpoints and payloads.
 */
export const ACTIONS = {
  generic: { label: '', url: () => OLD.home },
  // provisioning (dashboard 新增节点, nodes 新增节点, lines 新增家宽节点 / 新增线路机 all open openProvisionWizard)
  'provision.node': { label: '新增家宽节点', url: () => OLD.lines, cls: 'b', op: 'onboard' },
  'provision.relay': { label: '新增线路机', url: () => OLD.lines, cls: 'b', note: '新线路机由 fleet-cutover 接入，不走网页' },
  'provision.enrollExisting': { label: '登记已存在的入口', url: () => OLD.lines, cls: 'b', note: '已存在的入口由新系统收编（adopted），不用手动登记' },
  // lines page
  'lines.entry.disable': { label: '停用入口', url: entryUrl, cls: 'b', op: 'entries' },
  'lines.entry.enable': { label: '启用入口', url: entryUrl, cls: 'b', op: 'entries' },
  'lines.entry.reset': { label: '重置用量', url: entryUrl, cls: 'b', op: 'entries' },
  'lines.entry.remark': { label: '编辑备注', url: entryUrl, cls: 'b', op: 'entries' },
  'lines.entry.remarkSave': { label: '保存备注', url: entryUrl, cls: 'b', op: 'entries' },
  'lines.entry.rotate': { label: '更换连接凭据', url: entryUrl, cls: 'b', op: 'entries' },
  'lines.entry.verify': { label: '重新验证链路', url: entryUrl, cls: 'b', note: '新后台每分钟自动验证链路，结果在连接质量里' },
  'lines.relay.restartXray': { label: '重启 Xray', url: () => OLD.lines, cls: 'b', op: 'restart_xray' },
  'lines.relay.retryPanel': { label: '重试面板连接', url: () => OLD.lines, cls: 'b', note: '3x-ui 面板已退役，线路机由新系统管理' },
  'lines.relay.logs': { label: '查看 Xray 日志', url: () => OLD.lines, cls: 'b', note: '新后台暂不提供 Xray 日志；访问记录在节点行为里' },
  // node drawer (管理 / 连接质量 / 费用 tabs)
  'drawer.disable': { label: '停用入口', url: nodeUrl('actions'), cls: 'b', op: 'entries' },
  'drawer.enable': { label: '启用入口', url: nodeUrl('actions'), cls: 'b', op: 'entries' },
  'drawer.reset': { label: '重置用量', url: nodeUrl('actions'), cls: 'b', op: 'entries' },
  'drawer.remark': { label: '编辑备注', url: nodeUrl('actions'), cls: 'b', op: 'entries' },
  'drawer.rotate': { label: '更换连接凭据', url: nodeUrl('actions'), cls: 'b', op: 'entries' },
  'drawer.verify': { label: '重新验证链路', url: nodeUrl('actions'), cls: 'b', note: '新后台每分钟自动验证链路，结果在连接质量里' },
  'drawer.monitor': { label: '监控与维护', url: nodeUrl('actions'), cls: 'a', wired: true },
  'drawer.ackAll': { label: '确认故障', url: nodeUrl('actions'), cls: 'a', wired: true },
  'drawer.ack': { label: '标记已知悉', url: nodeUrl('availability'), cls: 'a', wired: true },
  'drawer.editAsset': { label: '编辑资产', url: nodeUrl('billing'), cls: 'a', wired: true },
  'drawer.renew': { label: '登记续费', url: nodeUrl('billing'), cls: 'a', wired: true },
  // palette
  'palette.verify': { label: '重新验证链路', url: nodeUrl('actions'), cls: 'b', note: '新后台每分钟自动验证链路，结果在连接质量里' },
  'palette.restartXray': { label: '重启 Xray', url: () => OLD.lines, cls: 'b', op: 'restart_xray' },
  // behavior
  'behavior.event.ack': { label: '标记已查看', url: ({ node }) => `${OLD.behavior}?${qs({ view: 'events', asset: assetOf(node) })}`, cls: 'a', wired: true },
  'behavior.rules.saveDraft': { label: '保存草稿', url: ({ node }) => `${OLD.behavior}?${qs({ tab: 'rules', asset: assetOf(node) })}`, cls: 'b', op: 'policies' },
  'behavior.rules.apply': { label: '应用访问规则', url: ({ node }) => `${OLD.behavior}?${qs({ tab: 'rules', asset: assetOf(node) })}`, cls: 'b', op: 'policies' },
  'behavior.rules.discard': { label: '丢弃全部草稿', url: () => OLD.rules, cls: 'b', op: 'policies' },
  'behavior.rules.rollback': { label: '回滚到这个版本', url: () => OLD.rules, cls: 'b', op: 'policies.rollback' },
  // onboarding (the demo batch is hidden in live mode; these stay reachable only through the page head)
  'onboarding.batch.new': { label: '新建批次', url: () => OLD.lines, cls: 'b', op: 'onboard' },
  'onboarding.batch.pause': { label: '暂停批次', url: () => OLD.lines, cls: 'b', op: 'onboard' },
  'onboarding.batch.cancelUnstarted': { label: '取消未开始的行', url: () => OLD.lines, cls: 'b', op: 'onboard' },
  // API 密钥（旧 /admin/api-keys，类 a：只动 team-ops 数据库）
  'apikeys.create': { label: '新建密钥', url: () => OLD.apiKeys, cls: 'a', wired: true },
  'apikeys.revoke': { label: '撤销密钥', url: () => OLD.apiKeys, cls: 'a', wired: true },
  // fleetd 自己的写（事项与告警）：旧后台没有对应页面
  'alerts.ack': { label: '确认事项', url: () => OLD.home, cls: 'a', wired: true, noOld: true },
  'alerts.silence': { label: '新建静默', url: () => OLD.home, cls: 'a', wired: true, noOld: true },
  'alerts.maintenance': { label: '新建维护窗口', url: () => OLD.home, cls: 'a', wired: true, noOld: true },
  'alerts.deleteWindow': { label: '结束静默 / 维护', url: () => OLD.home, cls: 'a', wired: true, noOld: true },
  'onboarding.row': { label: '上架步骤操作', url: () => OLD.lines, cls: 'b', op: 'onboard' },
  // 账户与安全（新后台账户，fleet/API.md §8）：只动登录者自己的账户
  'account.password': { label: '修改密码', url: () => OLD.home, cls: 'a', wired: true, noOld: true },
  'account.totp': { label: '两步验证', url: () => OLD.home, cls: 'a', wired: true, noOld: true },
  'account.sessionRevoke': { label: '退出会话', url: () => OLD.home, cls: 'a', wired: true, noOld: true },
};
/** Read-only destinations the v2 preview does not have (links, not writes). */
export const READ_LINKS = {
  activity: ({ node }) => `${OLD.activity}?${qs({ asset: assetOf(node) })}`,
  audit: () => OLD.audit,
};
/** Sidebar entries that are Komari native pages: in live mode they are plain links to the original. */
export const NATIVE = {
  'native/servers': '/admin/servers', 'native/metrics': '/admin/metrics-overview', 'native/ping': '/admin/ping',
  'native/exec': '/admin/exec', 'native/sessions': '/admin/sessions', 'native/terminal': '/terminal',
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
  if (a?.cls === 'b') return movedNotice(key);
  void ctx; // 原版后台已下线：不再给“打开原版”的链接
  return toast({ message: `只读预览：这个构建没有开启写操作${a?.label ? `（${a.label}）` : ''}`, tone: 'info', timeout: 8000 });
}
/** Class b not available here: the new backend has not opened it yet (op), or there is no such operation any more (note). */
export function movedNotice(key) {
  const a = ACTIONS[key];
  const why = a?.note ?? (a?.op && !writesEnabled() ? '这个构建没有开启写操作' : '新后台尚未开放此操作，开放后在这里直接进行');
  return toast({ message: `${a?.label ? `${a.label}：` : ''}${why}`, tone: 'info', timeout: 8000 });
}
export const writesEnabled = () => isLive() && globalThis.TEAMOPS_WRITES === true;
/** fleetd routes this capability (fleet/API.md; /system features or version) — never probed with a request. */
export const fleetHas = (feature) => !!T()?.fleet?.has?.(feature);
/**
 * At the top of a write handler: `if (guard('lines.entry.enable', { entry, store })) return;`
 * true = handled here (notice shown), false = go on.  Prototype mode: always false.  Live mode: class b → MOVED
 * notice; class a wired with TEAMOPS_WRITES on → false (the handler posts to the legacy endpoint); otherwise the
 * read-only notice with the link to the original page.
 */
export function guard(key, ctx) {
  if (!isLive()) return false;
  if (isWired(key)) return false;
  readOnlyNotice(key, ctx);
  return true;
}
/**
 * Wired and enabled: the handler should do the real request (live-writes.js / live-ops.js / live-onboard.js) instead of
 * the prototype demo.  Class a: wired + write build.  Class b: write build + fleetd has the capability.
 */
export function isWired(key) {
  if (!isLive() || !writesEnabled()) return false;
  const a = ACTIONS[key];
  if (!a) return false;
  if (a.cls === 'a') return !!a.wired;
  return !!a.op && fleetHas(a.op);
}

/** Top-bar badge (existing pill, info tone).  app.js shellHtml: `${liveBadge()}` first in .topbar-right. */
export function liveBadge() {
  if (!isLive()) return '';
  if (writesEnabled()) {
    const tipW = '数据来自运营服务，每 10 秒刷新；资产、续费、故障与行为确认、监控设置可以在这里修改，入口与线路机操作随 Xray 迁到新系统';
    return `<span class="pill live-badge" data-tone="info" data-dot="solid" data-tip="${esc(tipW)}" tabindex="0" aria-label="实时数据"><span class="live-badge-long">实时数据</span><span class="live-badge-short">实时</span></span>`;
  }
  const tip = '数据来自运营服务的只读接口，每 10 秒刷新；新增、停用、续费等写操作请在原版后台进行';
  return `<span class="pill live-badge" data-tone="info" data-dot="solid" data-tip="${esc(tip)}" tabindex="0" aria-label="只读预览，实时数据"><span class="live-badge-long">只读预览 · 实时数据</span><span class="live-badge-short">只读</span></span>`;
}

/** Login overlay (401 at boot or while polling).  Uses openDialog; no close button: the data stopped. */
let authShown = false;
export function showAuth({ message = '请先登录管理员账户', link = OLD.login, expired } = {}) {
  // 新后台在这个域名上：用 v2 自己的登录页（fleetd 账户，可选两步验证）；旧后台时仍去原版登录
  if (fleetLogin()) { showLogin({ expired: expired ?? !!T()?.poller }); return; }
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
