// app.js — 外壳（顶栏 + 侧栏）、hash 路由、主题、全局快捷键、命令面板、鲜度计时器、节点抽屉装配（SPEC §3.1–3.3、§5、§6.0）。
// 页面模块放在 js/pages/<name>.js，默认导出 { title, mount(ctx) → { update(params), unmount(), nodeOrder?(), onDrawer?(id) } }。
// 页面不存在时显示“原型未包含此页”的空状态，不跳走。

import { fmt, loadSnapshot, createLive, createKeys, paletteItems, comboKeys } from '../../shared/index.js';
import {
  icon, esc, storage, on, $, $$, installControls, installOverlays, installBeats, hydrate,
  openMenu, closeTopLayer, topLayer, registerLayer, openDialog, toast, createCommandPalette,
  freshnessState, paintFreshness, pageHead, freshness, empty, pageError, kpiTile, kbd, tableSkeleton,
  segmented, button, iconButton, rafBatch,
} from './components.js';
import { applyPlans, allPlans } from './plans.js';
import { openNodeDrawer, closeNodeDrawer, drawerNodeId } from './node-drawer.js';
import { skeletonCard } from './node-card.js';
import { PAGE_MODULES } from './pages/index.js';
import { HEAD_TOOLS } from './page-tools.js';
import { isLive, guard, liveBadge, showAuth, installLiveMode } from './live-mode.js';
import { logout } from './login.js';
import { fleetAvailable, fleetGet, onFleet } from './fleet.js';

// ─── 导航（顶层顺序、名称与原版完全一致；新增的“上架流水线”放在“运维”分组第一项，SPEC §9 Q6 备选） ───

const NAV_FULL = [
  { id: 'home', label: '首页', icon: 'house' },
  { id: 'dashboard', label: '运营总览', icon: 'layout-dashboard' },
  { id: 'nodes', label: '节点', icon: 'server' },
  { id: 'lines', label: '线路', icon: 'network' },
  { id: 'behavior', label: '节点行为', icon: 'activity' },
  { group: 'ops', label: '运维', icon: 'wrench', items: [
    { id: 'onboarding', label: '上架流水线', icon: 'package-plus' },
    // 新后台（fleetd）的两页放在“运维”里，顶层与原版保持一致（fidelity #8 同一原则）；顶栏的事项计数直达事项与告警
    { id: 'alerts', label: '事项与告警', icon: 'triangle-alert' },
    { id: 'plans', label: '套餐与配额', icon: 'layers' },
    { id: 'native/servers', label: '主机配置', icon: 'server-cog' },
    { id: 'native/metrics', label: '主机统计', icon: 'chart-no-axes-column' },
    { id: 'native/ping', label: '延迟监测', icon: 'radar' },
    { id: 'native/exec', label: '远程执行', icon: 'code' },
    { id: 'native/sessions', label: '会话管理', icon: 'users' },
    { id: 'api-keys', label: 'API 密钥', icon: 'key-round' },
    { id: 'native/terminal', label: '工作台', icon: 'square-terminal' },
  ] },
  { group: 'notify', label: '通知', icon: 'bell', items: [{ id: 'notify', label: '告警通知' }, ...['通知渠道', '通用', '离线通知', '负载通知'].map((l, i) => ({ id: `native/notify-${i}`, label: l }))] },
  { group: 'theme', label: '主题', icon: 'palette', items: ['主题管理', '主题市场'].map((l, i) => ({ id: `native/theme-${i}`, label: l })) },
  { group: 'plugin', label: '插件', icon: 'blocks', items: ['插件管理', '插件市场', '插件配置'].map((l, i) => ({ id: `native/plugin-${i}`, label: l })) },
  { group: 'system', label: '系统', icon: 'settings', items: ['站点', '通用', '登录', 'XtermJS', '数据库', '日志', '性能分析'].map((l, i) => ({ id: `native/system-${i}`, label: l })) },
  // 账户：新后台账户（登录、密码、两步验证、会话）在前；原版 Komari 的两页在旧系统下线前保留
  { group: 'account', label: '账户', icon: 'circle-user', items: [{ id: 'account', label: '账户与安全', v2: true }, ...['账户', '关于'].map((l, i) => ({ id: `native/account-${i}`, label: l }))] },
];
const PAGES = new Set(['home', 'dashboard', 'nodes', 'lines', 'behavior', 'onboarding', 'alerts', 'notify', 'plans', 'api-keys', 'account']);
/**
 * 实时构建（生产）：原版 Komari 已下线——它的原生页（主机配置、主题、插件、系统、原版账户……）一律不出现；
 * 拿掉后只剩一项的分组摊平成顶层项（账户 → 账户与安全，通知 → 告警通知）。原型保留完整的原版侧栏。
 */
function liveNav(nav) {
  const out = [];
  for (const x of nav) {
    if (!x.items) { out.push(x); continue; }
    const items = x.items.filter((s) => !s.id.startsWith('native/'));
    if (!items.length) continue;
    if (items.length === 1) out.push({ ...items[0], icon: items[0].icon ?? x.icon });
    else out.push({ ...x, items });
  }
  return out;
}
const NAV = isLive() ? liveNav(NAV_FULL) : NAV_FULL;
const NAV_FLAT = NAV_FULL.flatMap((x) => (x.items ? x.items : [x]));
const TITLES = Object.fromEntries(NAV_FLAT.map((x) => [x.id, x.label]));

// ─── 主题（light / dark / system，localStorage refined:theme） ──────────────

const THEME_KEY = 'refined:theme';
const themeIcon = (t) => (t === 'dark' ? 'moon' : t === 'light' ? 'sun' : 'monitor');
function applyTheme(t) {
  if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t;
  else delete document.documentElement.dataset.theme;
  storage.set(THEME_KEY, t);
  const b = $('[data-top="theme"]');
  if (b) { b.innerHTML = icon(themeIcon(t), 16); b.dataset.tip = `外观：${{ light: '浅色', dark: '深色', system: '跟随系统' }[t]}`; }
}
const PALETTE_KEY = 'refined:palette';
function applyPalette(v) {
  if (v === 'indigo') document.documentElement.dataset.palette = 'indigo';
  else delete document.documentElement.dataset.palette;
  storage.set(PALETTE_KEY, v === 'indigo' ? 'indigo' : 'plum');
}
// 只用原版靛蓝（粉紫先不做，token 块保留不启用）
applyPalette('indigo');
const effectiveDark = () => document.documentElement.dataset.theme === 'dark' || (!document.documentElement.dataset.theme && matchMedia('(prefers-color-scheme: dark)').matches);

// ─── 外壳 ───────────────────────────────────────────────────────────────────

function shellHtml(navState) {
  // 只读预览：原生页（Komari 后台）直接链接原版地址，不再显示“原型未包含此页”
  const href = (x) => `#/${x.id}`;
  const item = (x, sub = false) => `<li><a class="nav-item${sub && !x.icon ? ' no-icon' : ''}" href="${esc(href(x))}" data-nav="${x.id}">${x.icon ? icon(x.icon, 16) : ''}<span class="nav-label">${esc(x.label)}</span></a></li>`;
  const nav = NAV.map((x) => {
    if (!x.group) return item(x);
    const open = !!navState.open?.[x.group];
    return `<li><button type="button" class="nav-item nav-group" data-group="${x.group}" aria-expanded="${open}" aria-controls="nav-${x.group}">${icon(x.icon, 16)}<span class="nav-label">${esc(x.label)}</span>${icon('chevron-down', 14, 'nav-chevron')}</button>
      <ul class="nav-sub" id="nav-${x.group}"${open ? '' : ' hidden'}>${x.items.map((s) => item(s, true)).join('')}</ul></li>`;
  }).join('');
  return `<button type="button" class="skip-link" data-top="skip">跳到内容</button><header class="topbar">
      <button type="button" class="icon-btn" data-top="nav" aria-label="收起 / 展开侧栏" data-tip="收起 / 展开侧栏">${icon('menu', 16)}</button>
      <a class="brand" href="#/home">团队线路管理</a>
      <div class="topbar-right">
        <span data-top-alerts hidden></span>
        ${liveBadge()}
        <button type="button" class="top-btn" data-top="search" aria-label="搜索（${comboKeys('mod+k')[0].join('')}）" data-tip="搜索节点、IP、入口（${comboKeys('mod+k')[0].join('')}）">${icon('search', 16)}</button>
        <button type="button" class="top-btn" data-top="theme" aria-label="外观" aria-haspopup="menu" aria-expanded="false"></button>
        <button type="button" class="top-btn" data-top="accent" aria-label="强调色" data-tip="强调色" aria-haspopup="menu" aria-expanded="false">${icon('droplet', 16)}</button>
        <button type="button" class="top-btn" data-top="lang" aria-label="语言" data-tip="语言" aria-haspopup="menu" aria-expanded="false">${icon('languages', 16)}</button>
        <button type="button" class="top-btn is-muted" data-top="logout" aria-label="退出登录" data-tip="退出登录">${icon('log-out', 16)}</button>
      </div>
    </header>
    <nav class="sidebar" aria-label="主导航"><ul class="nav-list">${nav}</ul></nav>
    <div class="nav-scrim" data-top="scrim"></div>
    <main class="main" id="main"><div class="content" id="content"></div><div class="content" id="demo" hidden></div></main>`;
}

// ─── 启动 ───────────────────────────────────────────────────────────────────

const appEl = document.getElementById('app');
const navState = storage.get('refined:nav', { collapsed: false, open: {} });
appEl.innerHTML = shellHtml(navState);
appEl.dataset.nav = navState.collapsed && innerWidth > 760 ? 'closed' : 'open';
applyTheme(storage.get(THEME_KEY, 'system'));
installControls(document);
installOverlays(document);
installBeats(document);

const contentEl = document.getElementById('content');
const mainEl = document.getElementById('main');
queueMicrotask(() => markNav(parseHash().path));

// 首屏骨架：外壳立即出现，内容区是与真实布局等高的骨架（§3.20，不再整页转圈）
{
  const path0 = parseHash().path;
  const view0 = storage.get('refined:home:view', 'large');
  const kpis = `<section class="kpi-grid">${Array.from({ length: 4 }, () => kpiTile({ key: 'x', label: ' ', loading: true })).join('')}</section>`;
  const body = path0 === 'home'
    ? `<div class="home-bar"><span class="skel" style="width:330px;height:36px;border-radius:999px"></span><span class="skel" style="width:96px;height:36px"></span></div>
       <div class="fbar"><span class="skel" style="width:40%;height:32px;border-radius:999px"></span></div>
       <div class="cards" data-view="${esc(view0)}">${Array.from({ length: 6 }, () => skeletonCard(view0)).join('')}</div>`
    : path0 === 'dashboard'
      // 运营总览：团队表 3 行 + 两栏（排行 5 行 │ 需处理）+ 190 高图表，与真实布局等高（§3.20）
      ? `<section class="panel"><div class="panel-head"><span class="skel" style="width:48px;height:14px"></span></div>${tableSkeleton(7, 3)}</section>
         <div class="cols-2 dash-pair">${['96px', '64px'].map((w) => `<section class="panel"><div class="panel-head"><span class="skel" style="width:${w};height:14px"></span></div>${tableSkeleton(4, 5, { head: false })}</section>`).join('')}</div>
         <section class="panel"><div class="panel-head"><span class="skel" style="width:120px;height:14px"></span></div><div class="panel-body"><span class="skel" style="height:190px"></span></div></section>`
      : path0 === 'lines'
        // 线路：控制条 + 线路机面板（名称行、描述行、六个读数、入口表 8 行），没有 KPI 行，与真实布局等高（§3.20）
        ? `<div class="lines-bar"><span class="skel" style="width:380px;height:36px;border-radius:999px;max-width:100%"></span><span class="skel" style="width:360px;height:36px;max-width:100%"></span></div>
           <section class="panel relay"><div class="relay-head"><span class="skel" style="width:360px;height:22px;max-width:100%"></span><span class="skel" style="width:480px;height:18px;max-width:100%"></span></div><div class="relay-metrics">${'<span class="skel" style="height:48px"></span>'.repeat(6)}</div><div class="relay-sec"><div class="entry-head"><span class="skel" style="width:420px;height:32px;max-width:100%"></span></div>${tableSkeleton(10, 8)}</div></section>`
        : path0 === 'onboarding'
          // 上架流水线：批次卡（标题 + 两行元信息 + 进度）→ 4 张 KPI → 合批 3 行 → 矩阵 8 行，与真实布局等高（§3.20）
          ? `<section class="panel"><div class="panel-head"><span class="skel" style="width:360px;height:22px;max-width:100%"></span></div><div class="panel-body"><span class="skel" style="width:70%;height:12px;margin:3px 0 9px"></span><span class="skel" style="width:55%;height:12px;margin-bottom:16px"></span><span class="skel" style="width:360px;height:16px;max-width:100%"></span></div></section>
             ${kpis}
             <section class="panel"><div class="panel-head"><span class="skel" style="width:96px;height:14px"></span></div>${tableSkeleton(4, 3)}</section>
             <section class="panel"><div class="panel-head"><span class="skel" style="width:96px;height:14px"></span></div>${tableSkeleton(10, 8)}</section>`
          : path0 === 'nodes'
            // 节点：面板头工具条两行（状态分段 + 团队分段 / 搜索 + 三个下拉）+ 11 列表格 8 行，与真实布局等高（§3.20）
            ? `<section class="panel"><div class="toolbar"><div class="toolbar-row"><span class="skel" style="width:420px;height:36px;border-radius:999px;max-width:100%"></span><span class="toolbar-end"><span class="skel" style="width:320px;height:36px;border-radius:999px;max-width:100%"></span></span></div><div class="toolbar-row"><span class="skel" style="width:320px;height:36px;max-width:100%"></span><span class="skel" style="width:270px;height:36px;max-width:100%"></span></div></div>${tableSkeleton(11, 8)}</section>`
            : path0 === 'behavior'
              // 节点行为：页面分段 → 筛选条 → KPI → 页签 + 表（与真实页面同序，KPI 不再先占第一行）
              ? `<div class="row"><span class="skel" style="width:168px;height:36px;border-radius:999px"></span></div>
                 <div class="fform"><span class="skel" style="width:60%;height:36px"></span></div>
                 ${kpis}
                 <section class="panel"><div class="tabs" style="padding:0 var(--card-pad);height:41px;align-items:center"><span class="skel" style="width:320px;height:14px"></span></div>${tableSkeleton(6, 8)}</section>`
              : `<section class="panel"><div class="panel-head"><span class="skel" style="width:120px;height:14px"></span></div>${tableSkeleton(7, 8)}</section>`;
  // 页头工具与真实页面同一份（page-tools.js），骨架换成真实页面时“更新于”不再被挤走（craft #6）
  const tools = HEAD_TOOLS[path0]?.({ density: storage.get('refined:density', 'comfortable'), view: view0 }) ?? freshness();
  contentEl.innerHTML = `<div class="page">${pageHead({ title: TITLES[path0] ?? '首页', tools })}${['lines', 'onboarding', 'behavior', 'notify', 'api-keys'].includes(path0) ? '' : kpis}${body}</div>`;
  hydrate(contentEl);
}

const keys = createKeys();
let store, live, page = null, pageName = null, pageSeq = 0;
let drawerPushed = false, closingByRoute = false;

const app = {
  get store() { return store; },
  get live() { return live; },
  keys,
  toast,
  /** 当前 hash 的查询参数（URLSearchParams）。 */
  params: () => parseHash().params,
  route: () => parseHash().path,
  /** 合并参数写回 URL（null 删除）；replace 默认 true。触发当前页 update(params)。 */
  setParams(obj, { replace = true } = {}) {
    const { path, params } = parseHash();
    for (const [k, v] of Object.entries(obj)) { if (v == null || v === '') params.delete(k); else params.set(k, v); }
    writeHash(path, params, replace);
    route();
  },
  navigate(hash) { location.hash = hash.replace(/^#?/, '#'); },
  /** 在当前页打开节点抽屉（pushState：浏览器后退 = 关抽屉，§5.5）。 */
  openNode(id, tab = 'overview') { app.setDrawerParams(id, tab); },
  setDrawerParams(id, tab, { replace } = {}) {
    const { path, params } = parseHash();
    const wasOpen = params.has('node');
    params.set('node', id);
    if (tab && tab !== 'overview') params.set('tab', tab); else params.delete('tab');
    const rep = replace ?? wasOpen;
    if (!rep) drawerPushed = true;
    writeHash(path, params, rep);
    route();
  },
  closeDrawerParams() {
    if (closingByRoute) return;
    const { path, params } = parseHash();
    if (!params.has('node')) return;
    if (drawerPushed) { drawerPushed = false; history.back(); return; }
    params.delete('node'); params.delete('tab');
    writeHash(path, params, true);
  },
  drawerOpen: () => !!drawerNodeId(),
  drawerNodeId,
  /** 抽屉关闭时把焦点还给当前节点所在的行或卡片（用 [ ] 切过节点时也是当前这一个，craft #27）。 */
  focusTargetFor(id) {
    if (!id) return null;
    const q = CSS.escape(id);
    return contentEl.querySelector(`tr[data-row="${q}"][tabindex], tr[data-node="${q}"][tabindex], .ncard[data-id="${q}"] .ncard-name`);
  },
  nodeOrder: () => page?.api?.nodeOrder?.() ?? store.nodes().map((n) => n.id),
  onDrawerChange: (id) => page?.api?.onDrawer?.(id),
  /** 运营页密度（宽松 / 紧凑，localStorage refined:density），写在 <main data-density>。 */
  density: () => storage.get('refined:density', 'comfortable'),
  setDensity(v) { storage.set('refined:density', v); applyDensity(); page?.api?.onDensity?.(app.density()); },
};
globalThis.__app = app; // 调试用

function applyDensity() {
  if (page?.module?.density) mainEl.dataset.density = app.density() === 'compact' ? 'compact' : 'comfortable';
  else delete mainEl.dataset.density;
}

function parseHash() {
  const h = location.hash.replace(/^#\/?/, '');
  const [p, q = ''] = h.split('?');
  return { path: p || 'home', params: new URLSearchParams(q) };
}
function writeHash(path, params, replace) {
  // 可读的 URL：只转义会破坏解析的字符（% & = # + ? 空格），中文、逗号、冒号原样保留
  const enc = (s) => String(s).replace(/[%&=#+? ]/g, (c) => encodeURIComponent(c));
  const q = [...params].map(([k, v]) => `${enc(k)}=${enc(v)}`).join('&');
  const url = `#/${path}${q ? `?${q}` : ''}`;
  if (url === location.hash) return;
  if (replace) history.replaceState(history.state, '', url); else history.pushState(null, '', url);
}

// ─── 滚动位置：记在当前历史条目里，后退回到上一页时恢复（craft #11） ────────
if ('scrollRestoration' in history) history.scrollRestoration = 'manual';
let scrollSaveT = null;
addEventListener('scroll', () => {
  clearTimeout(scrollSaveT);
  scrollSaveT = setTimeout(() => { try { history.replaceState({ ...(history.state ?? {}), scrollY: Math.round(scrollY) }, '', location.href); } catch { /* 忽略 */ } }, 150);
}, { passive: true });
/** 页面挂载后恢复滚动：内容是异步画出来的（首页卡片在下一帧），等页面够高再滚，最多等 1.5 秒。 */
function restoreScroll(y, seq) {
  const t0 = performance.now();
  const step = () => {
    if (seq !== pageSeq) return;
    const max = document.documentElement.scrollHeight - innerHeight;
    if (max >= y - 1 || performance.now() - t0 > 1500) { window.scrollTo(0, Math.min(y, Math.max(0, max))); return; }
    requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
}

// ─── 路由 ───────────────────────────────────────────────────────────────────

async function route() {
  if (!store) return;
  const { path, params } = parseHash();
  // 通知里的深链（fleetd 告警 link）：#/lines/r1 → 线路页该线路机；#/nodes/t1-01 → 节点抽屉
  const deep = /^(lines|nodes)\/([^/?#]+)$/.exec(path);
  if (deep) { history.replaceState(null, '', deep[1] === 'lines' ? `#/lines?relay=${encodeURIComponent(deep[2])}` : `#/nodes?node=${encodeURIComponent(deep[2])}`); return route(); }
  markNav(path);
  document.title = `${TITLES[path] ?? '团队线路管理'} · 团队线路管理`;
  if (path !== pageName) {
    const seq = ++pageSeq;
    try { page?.api?.unmount?.(); } catch (e) { console.error(e); }
    page = null; pageName = path;
    closeNodeDrawerQuiet();
    contentEl.innerHTML = '';
    let mod = null;
    if (PAGE_MODULES[path]) {
      try { mod = (await PAGE_MODULES[path]()).default; } catch (e) { console.error(e); mod = null; }
    }
    if (seq !== pageSeq) return;
    if (!mod) mod = placeholderPage(path);
    const el = document.createElement('div');
    contentEl.replaceChildren(el);
    page = { module: mod, api: null };
    applyDensity();
    try { page.api = mod.mount({ el, app, params }) ?? {}; } catch (e) {
      console.error(e);
      el.innerHTML = pageError({ title: '页面加载失败', reason: String(e.message ?? e), impact: '其它页面不受影响' });
    }
    hydrate(el);
    const y = history.state?.scrollY;
    if (y > 0) restoreScroll(y, seq); else window.scrollTo(0, 0);
  } else {
    page?.api?.update?.(params);
  }
  syncDrawer(params);
  renderDemo(params);
}
/** 侧栏当前项（软靛蓝胶囊）；子项所在分组自动展开。 */
function markNav(path) {
  for (const a of $$('[data-nav]')) { if (a.dataset.nav === path) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current'); }
  const grp = NAV.find((x) => x.items?.some((s) => s.id === path));
  if (grp) setGroup(grp.group, true);
}
function closeNodeDrawerQuiet() { closingByRoute = true; closeNodeDrawer(); closingByRoute = false; }
function syncDrawer(params) {
  const id = params.get('node');
  if (id && store.node(id)) openNodeDrawer(app, id, params.get('tab') ?? 'overview');
  else if (drawerNodeId()) { drawerPushed = false; closeNodeDrawerQuiet(); }
}
function placeholderPage(path) {
  const title = TITLES[path] ?? '页面';
  return {
    title,
    mount({ el }) {
      // 实时：原版 Komari 的原生页已随旧系统下线（旧书签打开时给说明）；原型：未包含此页
      const text = isLive() ? (path.startsWith('native/') ? '这个页面属于已下线的原版后台' : '没有这个页面') : PAGES.has(path) ? `原型未包含此页（${title} 由其它页面构建者补上）` : '原型未包含此页';
      el.innerHTML = `<div class="page">${pageHead({ title, tools: freshness() })}<div class="placeholder">${empty({ icon: 'inbox', text, action: { label: '回到首页', attrs: { 'data-nav-home': '' } } })}</div></div>`;
      const offs = [on(el, 'click', '[data-nav-home]', () => app.navigate('#/home'))];
      return { unmount: () => offs.forEach((f) => f()) };
    },
  };
}

// ─── 演示条（?demo=1）：回放真实时刻、触发一次状态变化（§6.2） ─────────────

function renderDemo(params) {
  const box = document.getElementById('demo');
  // 只读预览：演示条（回放、造状态）会改写实时状态，不出现
  if (params.get('demo') !== '1' || isLive()) { box.hidden = true; box.innerHTML = ''; return; }
  if (!box.hidden && box.innerHTML) return;
  box.hidden = false;
  const reps = store.replays();
  const label = (r) => {
    const names = r.nodes.map((x) => store.node(x.node)?.name).filter(Boolean);
    const who = names.length > 2 ? `${names.slice(0, 2).join('、')} 等 ${names.length} 台` : names.join('、');
    return `${fmt.stamp(r.at).text} ${who} ${fmt.health(r.nodes[0]?.status).word}`;
  };
  box.innerHTML = `<div class="demo-bar">回放真实时刻：${reps.map((r, i) => `<button type="button" class="btn btn-link btn-sm" data-replay="${i}">${esc(label(r))}</button>`).join(' │ ')} │ <button type="button" class="btn btn-link btn-sm" data-replay="clear">恢复</button> │ <button type="button" class="btn btn-link btn-sm" data-trigger>触发一次状态变化</button></div>`;
  box.onclick = (e) => {
    const r = e.target.closest('[data-replay]');
    if (r) { if (r.dataset.replay === 'clear') store.clearReplay(); else store.applyReplay(Number(r.dataset.replay)); return; }
    if (e.target.closest('[data-trigger]')) live.triggerEvent();
  };
}

// ─── 顶栏、侧栏交互 ─────────────────────────────────────────────────────────

function setGroup(g, open) {
  const b = $(`.nav-group[data-group="${g}"]`);
  if (!b) return;
  b.setAttribute('aria-expanded', String(open));
  document.getElementById(`nav-${g}`).hidden = !open;
  navState.open = { ...navState.open, [g]: open };
  storage.set('refined:nav', navState);
}
// 只认侧栏分组按钮：表格分组行 tr[data-group] 也在 appEl 里，不能被当成侧栏分组
on(appEl, 'click', '.nav-group[data-group]', (e, b) => setGroup(b.dataset.group, b.getAttribute('aria-expanded') !== 'true'));
// 手机侧栏：只有点了真正的页面链接才收起（点分组只是展开子项，craft #3）。
// 注意 #app 自己也带 data-nav 属性，选择器必须限定在 a.nav-item 上，否则任何点击都会把抽屉关掉。
on(appEl, 'click', 'a.nav-item[data-nav]', () => { if (appEl.dataset.nav === 'open-mobile') closeMobileNav(false); });
const sidebarEl = appEl.querySelector('.sidebar');
let offMobileNav = null;
function openMobileNav() {
  appEl.dataset.nav = 'open-mobile';
  offMobileNav?.();
  offMobileNav = registerLayer({ close: () => closeMobileNav(true) }); // Esc 关闭（进 Esc 栈）
  requestAnimationFrame(() => (sidebarEl.querySelector('[aria-current="page"]') ?? sidebarEl.querySelector('.nav-item'))?.focus({ preventScroll: true }));
}
function closeMobileNav(returnFocus = true) {
  if (appEl.dataset.nav !== 'open-mobile') return;
  appEl.dataset.nav = 'open';
  const off = offMobileNav; offMobileNav = null; off?.();
  if (returnFocus) $('[data-top="nav"]')?.focus({ preventScroll: true });
}
// 手机侧栏打开时 Tab 只在侧栏里循环
sidebarEl.addEventListener('keydown', (e) => {
  if (e.key !== 'Tab' || appEl.dataset.nav !== 'open-mobile') return;
  const f = [...sidebarEl.querySelectorAll('a.nav-item, button.nav-item')].filter((x) => x.offsetParent !== null);
  if (!f.length) return;
  if (e.shiftKey && document.activeElement === f[0]) { e.preventDefault(); f[f.length - 1].focus(); }
  else if (!e.shiftKey && document.activeElement === f[f.length - 1]) { e.preventDefault(); f[0].focus(); }
});
addEventListener('resize', () => { if (innerWidth > 760 && appEl.dataset.nav === 'open-mobile') closeMobileNav(false); });
on(appEl, 'click', '[data-top]', (e, b) => {
  const k = b.dataset.top;
  if (k === 'nav') {
    if (innerWidth <= 760) { if (appEl.dataset.nav === 'open-mobile') closeMobileNav(true); else openMobileNav(); return; }
    const closed = appEl.dataset.nav !== 'closed';
    appEl.dataset.nav = closed ? 'closed' : 'open';
    navState.collapsed = closed; storage.set('refined:nav', navState);
    setTimeout(() => globalThis.dispatchEvent(new Event('resize')), 240);
  } else if (k === 'scrim') closeMobileNav(true);
  else if (k === 'search') palette.open();
  else if (k === 'skip') { const c = contentEl.querySelector('.page-title') ?? contentEl; c.tabIndex = -1; c.focus(); }
  else if (k === 'theme') {
    const cur = storage.get(THEME_KEY, 'system');
    openMenu(b, {
      label: '外观', minWidth: 160,
      items: [{ group: '外观' }, { value: 'light', label: '浅色', icon: 'sun', checked: cur === 'light' }, { value: 'dark', label: '深色', icon: 'moon', checked: cur === 'dark' }, { value: 'system', label: '跟随系统', icon: 'monitor', checked: cur === 'system' }],
      onSelect: (v) => applyTheme(v),
    });
  } else if (k === 'accent') {
    // 调色板：只有原版靛蓝（粉紫的 token 块留在 original/tokens.css，不启用）
    const cur = document.documentElement.dataset.palette === 'indigo' ? 'indigo' : 'plum';
    const item = (v, l) => ({ value: v, label: l, checked: cur === v, end: `<span class="swatch" data-palette="${v}" aria-hidden="true"></span>${cur === v ? icon('check', 14) : ''}` });
    const all = [['indigo', '靛蓝']]; // 只有原版靛蓝（owner 2026-09-24）
    openMenu(b, { label: '强调色', minWidth: 180, items: [{ group: '强调色' }, ...all.map(([v, l]) => item(v, l))], onSelect: (v) => applyPalette(v) });
  } else if (k === 'lang') {
    openMenu(b, { label: '语言', minWidth: 180, items: [{ group: '语言' }, { value: 'zh_CN', label: '简体中文', checked: true }], onSelect: () => {} });
  } else if (k === 'logout') {
    if (isLive()) logout(); // 新后台：POST /auth/logout，回到 v2 登录页
    else toast({ message: '原型不会真的退出登录', tone: 'info' });
  }
});
// 窄屏页头的“更多”（⋯）：把被收起的次要操作列成菜单，选中后点原按钮（craft #8）
on(appEl, 'click', '[data-page-more]', (e, b) => {
  const list = [...b.closest('.page-tools').querySelectorAll(':scope > .btn:not(.btn-primary)')];
  if (!list.length) return;
  openMenu(b, {
    label: '更多操作', minWidth: 180,
    items: list.map((x, i) => ({ value: String(i), label: x.textContent.trim(), icon: [...(x.querySelector('.lucide')?.classList ?? [])].find((c) => c.startsWith('i-'))?.slice(2) })),
    onSelect: (v) => list[Number(v)]?.click(),
  });
});

// ─── 长表吊顶（craft #4）：表格放得下时 .table-wrap 改成 overflow-x: clip（不形成滚动容器），表头就能贴着页头吊顶；
// 放不下（窄屏、步骤 16 列）时保持容器内横向滚动。表格重画、实时补丁、窗口变化后按帧合并重算一次 ───
const syncWraps = rafBatch(() => {
  const ph = contentEl.querySelector('.page > .page-head');
  const sticky = ph && getComputedStyle(ph).position === 'sticky';
  document.documentElement.style.setProperty('--pagehead-h', `${sticky ? ph.offsetHeight : 0}px`);
  for (const w of [...contentEl.querySelectorAll('.table-wrap'), ...document.querySelectorAll('#layer .table-wrap')]) {
    const t = w.firstElementChild;
    if (!t) continue;
    const fits = t.offsetWidth <= w.clientWidth + 1;
    if (w.classList.contains('is-clip') !== fits) w.classList.toggle('is-clip', fits);
  }
});
new MutationObserver(syncWraps).observe(contentEl, { childList: true, subtree: true });
new MutationObserver(syncWraps).observe(document.getElementById('layer'), { childList: true, subtree: true }); // 对话框、抽屉里的表
addEventListener('resize', syncWraps);
/**
 * 纵向滚轮穿过只横向滚动的表格容器：表比容器宽时 .table-wrap 是 overflow-x:auto 的滚动容器，Chrome 会把整次滚轮
 * 手势（尤其触控板带一点横向分量时）锁在它身上，外面的对话框体 / 抽屉体 / 页面就滚不动。纵向为主、而它自己不能纵向滚时，
 * 把 deltaY 交给最近的能纵向滚的祖先（背后页面被锁时不交给窗口）。
 */
document.addEventListener('wheel', (e) => {
  const wrap = e.target.closest?.('.table-wrap');
  if (!wrap || e.ctrlKey || Math.abs(e.deltaY) <= Math.abs(e.deltaX)) return;
  if (wrap.scrollHeight > wrap.clientHeight + 1) return; // 它自己能纵向滚：交给浏览器
  let sc = wrap.parentElement;
  while (sc && sc !== document.body) {
    const oy = getComputedStyle(sc).overflowY;
    if ((oy === 'auto' || oy === 'scroll') && sc.scrollHeight > sc.clientHeight + 1) break;
    sc = sc.parentElement;
  }
  if (!sc || sc === document.body) {
    if (document.documentElement.classList.contains('is-locked')) return;
    sc = document.scrollingElement;
  }
  const dy = e.deltaMode === 1 ? e.deltaY * 16 : e.deltaMode === 2 ? e.deltaY * sc.clientHeight : e.deltaY;
  e.preventDefault();
  sc.scrollBy({ top: dy, left: 0, behavior: 'instant' });
}, { passive: false, capture: true });
globalThis.addEventListener('hashchange', () => route());

// ─── 命令面板 ───────────────────────────────────────────────────────────────

// hash 参数只转义会破坏解析的字符（与 writeHash 同一规则），中文、冒号原样保留
const hashEnc = (v) => String(v).replace(/[%&=#+? ]/g, (c) => encodeURIComponent(c));
const JUMPS = [['home', 'g h'], ['dashboard', 'g o'], ['nodes', 'g n'], ['lines', 'g r'], ['behavior', 'g b'], ['onboarding', 'g p'], ['alerts', 'g a']];
const palette = createCommandPalette({
  registerLayer,
  groups: ['跳转', '节点', '线路机', '入口', '套餐', '操作'],
  items: () => {
    if (!store) return [];
    const jumps = JUMPS.map(([id, combo]) => ({ id: `go:${id}`, group: '跳转', title: TITLES[id], subtitle: '', keywords: [id], icon: NAV_FLAT.find((x) => x.id === id)?.icon, end: comboKeys(combo).map((k) => k.join('')).join(' '), run: () => app.navigate(`#/${id}`) }));
    const base = paletteItems(store, (kind, id) => {
      if (kind === 'node') app.openNode(id);
      else if (kind === 'relay') app.navigate(`#/lines?relay=${hashEnc(id)}`);
      else if (kind === 'entry') app.navigate(`#/lines?entry=${hashEnc(id)}`); // 线路页定位到那一行并展开（§5.4）
    }).map((it) => {
      if (it.id.startsWith('entry:')) return { ...it, group: '入口' }; // 入口单独成组（原来混在“线路机”组里，craft #29）
      if (!it.id.startsWith('node:')) return it;
      const n = store.node(it.id.slice(5));
      return { ...it, subtitle: [n.ip, n.team, n.entry_label, n.plan?.name].filter(Boolean).join(' · '), keywords: [...it.keywords, n.plan?.name ?? ''].filter(Boolean) };
    });
    const plans = allPlans(store).map((p) => ({ id: `plan:${p.id}`, group: '套餐', title: p.name, subtitle: `${store.nodes().filter((n) => n.plan_id === p.id).length} 台 · ${p.specs?.summary ?? ''}`, keywords: [p.provider, p.id].filter(Boolean), icon: 'layers', run: () => app.navigate(`#/nodes?plan=${encodeURIComponent(p.id)}`) }));
    const cur = drawerNodeId() ? store.node(drawerNodeId()) : null;
    const actions = [
      { id: 'act:theme', group: '操作', title: '切换浅色 / 深色', keywords: ['主题', '外观', '深色', '浅色', '暗色', '亮色', 'theme', 'dark'], icon: 'sun', end: comboKeys('mod+shift+l')[0].join(''), run: toggleTheme },
      { id: 'act:density', group: '操作', title: '切换宽松 / 紧凑', subtitle: page?.module?.density ? `当前${app.density() === 'compact' ? '紧凑' : '宽松'}` : '只对运营页生效', keywords: ['密度', '紧凑', '宽松', 'density'], icon: 'list', end: comboKeys('mod+shift+d')[0].join(''), run: () => { if (page?.module?.density) app.setDensity(app.density() === 'compact' ? 'comfortable' : 'compact'); else toast({ message: '首页用“大卡片 / 小卡片 / 紧凑”切换视图；密度只对运营页生效', tone: 'info' }); } },
      { id: 'act:help', group: '操作', title: '快捷键帮助', keywords: ['快捷键', '帮助', 'help'], icon: 'command', end: '?', run: showHelp },
      ...(cur ? [
        { id: 'act:copy-ip', group: '操作', title: `复制 ${cur.name} 的 IP`, subtitle: cur.ip, keywords: ['复制', 'IP', 'copy'], icon: 'copy', run: () => navigator.clipboard?.writeText(cur.ip).then(() => toast({ message: `已复制 ${cur.ip}` }), () => toast({ message: '复制失败', tone: 'bad' })) },
        { id: 'act:verify', group: '操作', title: `重新验证 ${cur.name} 的链路`, keywords: ['验证', '链路', '出口'], icon: 'refresh-cw', run: () => guard('palette.verify', { node: cur }) || toast({ message: `${cur.name} 链路验证已开始（原型演示）`, tone: 'info' }) },
        { id: 'act:restart', group: '操作', title: `重启 ${cur.relay ?? '线路机'} 的 Xray`, icon: 'power', danger: true, run: () => {
          if (guard('palette.restartXray', { node: cur })) return;
          // 实时（新后台已开放）：到线路页对应的线路机卡片上确认（确认对话框在那里，带入口数与连接数）
          if (isLive()) { const r = store.relays().find((x) => x.name === cur.relay || x.id === cur.id); app.navigate(r ? `#/lines?relay=${encodeURIComponent(r.id)}` : '#/lines'); toast({ message: `在 ${r?.name ?? '线路机'} 的卡片上点“重启 Xray”确认`, tone: 'info' }); return; }
          toast({ message: '会打开确认对话框（原型演示）', tone: 'info' });
        } },
      ] : []),
    ];
    return [...jumps, ...base, ...plans, ...actions];
  },
});

// ─── 快捷键（§5.4） ─────────────────────────────────────────────────────────

function toggleTheme() { applyTheme(effectiveDark() ? 'light' : 'dark'); }
function showHelp() {
  const groups = keys.list();
  const body = `<div class="keys-help">${groups.map((g) => `<div><div class="keys-group-title">${esc(g.group)}</div>${g.items.map((it) => `<div class="keys-row"><span>${esc(it.desc)}</span><span class="keys-combo">${it.keys.map((steps) => steps.map((ks) => ks.map((k) => kbd(k)).join('')).join(' 然后 ')).join(' / ')}</span></div>`).join('')}</div>`).join('')}</div>`;
  openDialog({ title: '快捷键', size: 'md', body });
}
keys.bindAll([
  { combo: 'mod+k', run: () => (palette.isOpen ? palette.close() : palette.open()), desc: '命令面板', group: '全局', allowInInput: true },
  { combo: '/', run: () => { const s = $('[data-page-search]'); if (s) s.focus(); else palette.open(); }, desc: '聚焦搜索', group: '全局' },
  { combo: '?', run: showHelp, desc: '快捷键帮助', group: '全局' },
  { combo: 'Escape', run: () => { if (!closeTopLayer()) return false; }, desc: '关闭最上层（菜单 → 弹层 → 对话框 → 命令面板 → 抽屉）', group: '全局' },
  { combo: 'mod+shift+l', run: toggleTheme, desc: '切换浅色 / 深色', group: '外观', allowInInput: true },
  { combo: 'mod+shift+d', run: () => { if (page?.module?.density) app.setDensity(app.density() === 'compact' ? 'comfortable' : 'compact'); }, desc: '切换宽松 / 紧凑（运营页）', group: '外观' },
  ...JUMPS.map(([id, combo]) => ({ combo, run: () => app.navigate(`#/${id}`), desc: `去${TITLES[id]}`, group: '跳转' })),
  { combo: '[', run: () => $('.drawer [data-d="prev"]')?.click(), desc: '上一个节点', group: '抽屉', when: () => !!drawerNodeId() },
  { combo: ']', run: () => $('.drawer [data-d="next"]')?.click(), desc: '下一个节点', group: '抽屉', when: () => !!drawerNodeId() },
]);
keys.start(document);
// g 序列提示条（左下角）
let hintEl = null;
keys.onPending(({ pending }) => {
  if (pending === 'G') {
    hintEl ??= document.createElement('div');
    hintEl.className = 'keyhint';
    hintEl.innerHTML = `${kbd('G')} 然后 ${JUMPS.map(([id, c]) => `${kbd(c.split(' ')[1].toUpperCase())} ${esc(TITLES[id])}`).join(' · ')}`;
    document.getElementById('layer').appendChild(hintEl);
  } else hintEl?.remove();
});

// ─── 鲜度：全页共用一个计时器（60 秒内每秒，之后每 30 秒，§5.3 规则 7） ─────

let failed = false;
function freshTick() {
  if (!live) return;
  const age = live.age();
  const src = live.sources();
  // 轮询器每 live.intervalS 秒（10）刷新一次，模拟器每 2 秒；连续 3 次读取失败算断开（数据保留，按鲜度规则变灰）
  failed = (live.status?.().failing ?? 0) >= 3;
  const st = freshnessState({ ageS: age, lastT: src.komari, interval: live.intervalS ?? 2, failed });
  const tip = `数据源最后更新（北京时间）\nKomari 实时　${fmt.clockTime(src.komari, { seconds: true })}\n线路机采集　${fmt.clockTime(src.xray, { seconds: true })}\n监控评估　　${fmt.clockTime(src.monitor, { seconds: true })}\n线路机面板　${fmt.clockTime(src.panel, { seconds: true })}${live.status ? `\n实时轮询　　每 ${live.intervalS ?? 10} 秒 · 已请求 ${live.status().requests} 次` : ''}`;
  paintFreshness(st, tip);
}

// ─── 顶栏事项计数（fleetd 在时）：进行中且未确认的事项；0 时不显示，严重为红、否则黄；点了去事项与告警 ───

function installAlertPill() {
  const box = document.querySelector('[data-top-alerts]');
  if (!box || !fleetAvailable()) return;
  const paint = async () => {
    let c = null;
    try { c = (await fleetGet('/alerts', { state: 'unacked', limit: 1 }))?.counts ?? null; } catch { c = null; }
    const firing = Math.max(0, (c?.firing ?? 0) - (c?.acked ?? 0));
    box.hidden = !firing;
    if (!firing) { box.innerHTML = ''; return; }
    const tone = (c.critical ?? 0) > 0 ? 'bad' : 'warn';
    // 窄屏只留图标 + 数字（顶栏 390 宽时不溢出）
    box.innerHTML = `<a class="pill alert-pill" data-tone="${tone}" data-dot="none" href="#/alerts" aria-label="${esc(`事项 ${firing} 个`)}" data-tip="${esc(`进行中且未确认的事项 ${firing} 个（严重 ${c.critical ?? 0} · 警告 ${c.warning ?? 0}）`)}">${icon('triangle-alert', 12)}<span class="alert-pill-word">事项</span><span class="num">${esc(fmt.num(firing))}</span></a>`;
  };
  paint();
  onFleet(({ resource }) => { if (resource === 'alerts') paint(); if (resource === 'fleet-down') { box.hidden = true; box.innerHTML = ''; } });
}

/**
 * 实时数据加载后按后台收侧栏：旧系统下线（legacy_source = native）后，原版 Komari 的原生页不存在了——整项拿掉，
 * 空了的分组也拿掉；新后台没有账户接口时拿掉“账户与安全”。原型模式不动。
 */
function pruneNav() {
  if (!isLive()) return;
  const gone = (id) => (globalThis.TEAMOPS?.decommissioned && id.startsWith('native/')) || (id === 'account' && !globalThis.TEAMOPS?.fleet?.has?.('auth'));
  for (const a of document.querySelectorAll('.nav-item[data-nav]')) if (gone(a.dataset.nav)) a.closest('li')?.remove();
  for (const ul of document.querySelectorAll('.nav-sub')) if (!ul.querySelector('li')) ul.closest('li')?.remove();
}

// ─── 数据 ───────────────────────────────────────────────────────────────────

/**
 * 启动看门狗：15 秒还没拿到首屏数据 → 内容区换成明确的错误（在等哪些接口）+ 重新加载，不再无限骨架。
 * 数据后来到了照常渲染（route() 会覆盖内容区）。
 */
const BOOT_WATCHDOG_MS = 15000;
function bootWatchdog() {
  return setTimeout(() => {
    if (document.documentElement.dataset.ready === '1' || document.querySelector('.login-screen')) return;
    const src = globalThis.TEAMOPS?.source;
    const waiting = src ? [...src.inflight.keys()].map((u) => { try { return new URL(u, location.href).pathname.replace(/^\/v2\/api\/(legacy|v1)/, '').replace(/^\/api\/team-ops/, ''); } catch { return u; } }) : [];
    contentEl.innerHTML = `<div class="page">${pageHead({ title: '团队线路管理' })}<div class="placeholder">${pageError({
      title: '数据加载超时',
      reason: isLive() ? `${BOOT_WATCHDOG_MS / 1000} 秒内没有读到运营数据${waiting.length ? `；还在等：${waiting.slice(0, 4).join('、')}` : ''}` : '快照读取超时',
      impact: '后台可能正忙或暂时不可用；页面会在数据到达后自动显示，也可以重新加载',
    })}</div></div>`;
    paintFreshness({ state: 'stale', text: '加载超时' });
    on(contentEl, 'click', '[data-act="reload"]', () => location.reload());
  }, BOOT_WATCHDOG_MS);
}

(async function boot() {
  const watchdog = bootWatchdog();
  performance.mark?.('teamops:boot');
  try {
    store = await loadSnapshot();
    performance.mark?.('teamops:snapshot');
    await applyPlans(store);
    performance.mark?.('teamops:plans');
    clearTimeout(watchdog);
  } catch (e) {
    clearTimeout(watchdog);
    if (e?.auth) { showAuth(e); paintFreshness({ state: 'stale', text: '已断开' }); return; } // 只读预览：未登录 / 会话过期 → 登录对话框，内容区保留骨架
    console.error(e);
    contentEl.innerHTML = `<div class="page">${pageHead({ title: '团队线路管理' })}<div class="placeholder">${isLive()
      ? pageError({ title: '页面加载失败', reason: `读取实时数据失败：${e.message}`, impact: '运营服务暂时不可用，稍后重新加载' })
      : pageError({ title: '页面加载失败', reason: `读取 data/snapshot.json 失败：${e.message}`, impact: '所有页面都依赖这份快照' })}</div></div>`;
    on(contentEl, 'click', '[data-act="reload"]', () => location.reload());
    return;
  }
  live = createLive(store).start();
  pruneNav();
  installLiveMode(app);
  installAlertPill();
  setInterval(freshTick, 1000);
  store.on('change', () => freshTick());
  if (!location.hash) history.replaceState(null, '', '#/home');
  await route();
  freshTick();
  document.documentElement.dataset.ready = '1';
  performance.mark?.('teamops:ready');
})();
