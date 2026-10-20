// layers.js — 抽屉 Drawer（§3.16）、对话框 Dialog 与确认分级（§3.17）、Toast（§3.19）。
// 抽屉、对话框、命令面板都盖住整个视口（修 dna-ops M1：原来只盖 iframe）；焦点困在里面，关闭后归还。

import { icon } from '../icons.js';
import { esc, on } from './dom.js';
import { button, iconButton, hydrate } from './controls.js';
import { hideTip, closePopover } from './overlay.js';
import { isReadOnly, readOnlyNotice, guard } from '../live-mode.js';

const layer = () => document.getElementById('layer') ?? document.body;
const FOCUSABLE = 'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function trapFocus(el) {
  return on(el, 'keydown', null, (e) => {
    if (e.key !== 'Tab') return;
    const f = [...el.querySelectorAll(FOCUSABLE)].filter((x) => x.offsetParent !== null);
    if (!f.length) return;
    const first = f[0], last = f[f.length - 1];
    if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
    else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
  });
}
function leave(el, ms = 220) {
  el.classList.add('is-leaving');
  setTimeout(() => el.remove(), matchMedia('(prefers-reduced-motion: reduce)').matches ? 0 : ms);
}

// 模态层打开期间锁住页面滚动：在遮罩上滚轮不会把背后的页面滚走（craft #27）。按层计数，全部关掉才解锁
let locks = 0;
export function lockScroll() {
  if (locks++ > 0) return;
  // 经典滚动条（占宽度）时，锁定前量出它的宽度补成右内边距，内容不左右跳；覆盖式滚动条量出来是 0
  const html = document.documentElement;
  html.style.setProperty('--lock-gutter', `${Math.max(0, innerWidth - html.clientWidth)}px`);
  html.classList.add('is-locked');
}
export function unlockScroll() { locks = Math.max(0, locks - 1); if (!locks) document.documentElement.classList.remove('is-locked'); }

// 叠放栈：Esc 依次关闭 菜单 → 弹层 → 对话框 → 命令面板 → 抽屉（§5.4）
const stack = [];
export function topLayer() { return stack[stack.length - 1] ?? null; }
function push(entry) { stack.push(entry); }
function pop(entry) { const i = stack.indexOf(entry); if (i >= 0) stack.splice(i, 1); }
/** 注册一个自定义层（命令面板等）进 Esc 栈：registerLayer({ close }) → 注销函数。 */
export function registerLayer(entry) { push(entry); return () => pop(entry); }
/** 由 app.js 的 Esc 绑定调用：关闭最上层；返回是否关闭了什么。 */
export function closeTopLayer() { const t = topLayer(); if (!t) return false; t.close(); return true; }

// ─── Drawer ─────────────────────────────────────────────────────────────────

/**
 * openDrawer({ title, pill, meta, tabs:[{value,label}], tab, render(tab, bodyEl, api), onTab, onClose, onNav(dir), label, navLabels, foot })
 * → { el, body, setTab(tab), setHead({title,pill,meta}), setFoot(html), refresh(), close() }
 * 头部：标题 16/600 + 状态 pill ……… 上一个 / 下一个 / 刷新 / 关闭；第二行 12 px 副行；下划线页签。
 * navLabels：上下按钮的说明（默认“上一个（[）/ 下一个（]）”，步骤抽屉是“上一步 / 下一步”）。
 * foot：底部固定按钮条（.drawer-foot，右对齐；空字符串时不占位），例如步骤抽屉的“从这一步重试”。
 */
export function openDrawer(opts) {
  const prevFocus = document.activeElement;
  const scrim = document.createElement('div');
  scrim.className = 'scrim drawer-scrim';
  const el = document.createElement('aside');
  el.className = 'drawer';
  el.setAttribute('role', 'dialog');
  el.setAttribute('aria-modal', 'true');
  el.setAttribute('aria-label', opts.label ?? opts.title ?? '详情');
  el.innerHTML = `
    <header class="drawer-head">
      <div class="drawer-row">
        <h2 class="drawer-title" data-d="title"></h2><span data-d="pill"></span>
        <div class="drawer-tools">
          ${opts.onNav ? iconButton({ icon: 'chevron-up', label: opts.navLabels?.[0] ?? '上一个（[）', attrs: { 'data-d': 'prev' } }) + iconButton({ icon: 'chevron-down', label: opts.navLabels?.[1] ?? '下一个（]）', attrs: { 'data-d': 'next' } }) : ''}
          ${opts.onRefresh ? iconButton({ icon: 'rotate-cw', label: '刷新', attrs: { 'data-d': 'refresh' } }) : ''}
          ${iconButton({ icon: 'x', label: '关闭（Esc）', attrs: { 'data-d': 'close' } })}
        </div>
      </div>
      <div class="drawer-meta" data-d="meta"></div>
      <div data-d="tabs"></div>
    </header>
    <div class="drawer-body" data-d="body" tabindex="-1"></div>
    <footer class="drawer-foot" data-d="foot"></footer>`;
  layer().append(scrim, el);
  const q = (k) => el.querySelector(`[data-d="${k}"]`);
  const body = q('body');
  let tab = opts.tab ?? opts.tabs?.[0]?.value;
  const entry = { close: () => api.close() };
  const api = {
    el, body, get tab() { return tab; },
    setHead({ title, pill, meta } = {}) {
      if (title != null && q('title').textContent !== title) q('title').textContent = title;
      if (pill != null && q('pill').innerHTML !== pill) q('pill').innerHTML = pill;
      if (meta != null && q('meta').innerHTML !== meta) q('meta').innerHTML = meta;
    },
    setTabs(tabsHtml) { q('tabs').innerHTML = tabsHtml; hydrate(q('tabs')); },
    setFoot(html = '') { const f = q('foot'); if (f.__html !== html) { f.innerHTML = html; f.__html = html; } },
    setTab(t, { silent = false } = {}) {
      tab = t;
      const tabsRoot = q('tabs').querySelector('.tabs');
      if (tabsRoot) { for (const b of tabsRoot.querySelectorAll('.tab')) { const on_ = b.dataset.value === t; b.setAttribute('aria-selected', String(on_)); b.tabIndex = on_ ? 0 : -1; } hydrate(q('tabs')); }
      api.refresh();
      body.scrollTop = 0;
      if (!silent) opts.onTab?.(t);
    },
    refresh() { opts.render?.(tab, body, api); hydrate(body); },
    close() {
      if (!el.isConnected) return;
      pop(entry);
      offTrap(); offClick();
      hideTip(); closePopover();
      leave(el); leave(scrim, 180);
      unlockScroll();
      // 焦点回到“当前”对象所在的行或卡片（用 [ ] 切过节点时也回到当前这个），找不到才回到打开前的元素
      const target = opts.returnFocus?.();
      opts.onClose?.();
      const back = target?.isConnected ? target : prevFocus;
      if (back?.isConnected) back.focus?.({ preventScroll: true });
    },
  };
  lockScroll();
  api.setHead(opts);
  if (opts.tabsHtml) api.setTabs(opts.tabsHtml);
  if (opts.foot) api.setFoot(opts.foot);
  const offTrap = trapFocus(el);
  const offClick = on(el, 'click', '[data-d]', (e, b) => {
    const k = b.dataset.d;
    if (k === 'close') api.close();
    else if (k === 'prev') opts.onNav?.(-1);
    else if (k === 'next') opts.onNav?.(1);
    else if (k === 'refresh') opts.onRefresh?.(b);
  });
  el.addEventListener('tab-change', (e) => { if (e.target.closest('[data-d="tabs"]')) api.setTab(e.detail.value); });
  scrim.addEventListener('click', () => api.close());
  push(entry);
  api.refresh();
  requestAnimationFrame(() => (q('tabs').querySelector('.tab[aria-selected="true"]') ?? body).focus({ preventScroll: true }));
  return api;
}

// ─── Dialog ─────────────────────────────────────────────────────────────────

/**
 * openDialog({ title, sub, size:'sm'|'md'|'lg', body(html), foot(html), onClose, init(el, api), guard, focus })
 * → { el, close(), requestClose(), markDirty(), body } — 头部标题 16/600 + 副标题 + 关闭；体滚动；底栏固定、右对齐（取消在左、确认在右）。
 * guard: true → 表单对话框（向导、编辑）：点遮罩不关；填过内容后按 Esc、× 或“取消”先问“放弃已填写的内容？”（craft #2）。
 *   页面在“保存 / 提交”时调 api.close() 直接关闭。
 * focus：首个焦点。默认：[autofocus] → 体里第一个输入框 → 对话框本身（不落到 ×，也就不会自动弹出“关闭”提示，craft #13）；
 *   'cancel' → 底栏的“取消”（破坏性确认默认聚焦取消）。
 */
export function openDialog({ title, sub = '', size = 'sm', body = '', foot = '', onClose, init, label, guard = false, focus } = {}) {
  const prevFocus = document.activeElement;
  const scrim = document.createElement('div');
  scrim.className = 'scrim dialog-scrim';
  const el = document.createElement('div');
  el.className = `dialog${size !== 'sm' ? ` dialog-${size}` : ''}`;
  el.setAttribute('role', 'dialog');
  el.setAttribute('aria-modal', 'true');
  el.setAttribute('aria-label', label ?? title);
  el.tabIndex = -1;
  el.innerHTML = `<header class="dialog-head"><div><h2 class="dialog-title">${esc(title)}</h2>${sub ? `<div class="dialog-sub">${esc(sub)}</div>` : ''}</div>${iconButton({ icon: 'x', label: '关闭', attrs: { 'data-dlg': 'close' } })}</header><div class="dialog-body">${body}</div>${foot ? `<footer class="dialog-foot">${foot}</footer>` : ''}`;
  layer().append(scrim, el);
  const entry = { close: () => api.requestClose() };
  const offTrap = trapFocus(el);
  let dirty = false, asking = false;
  const api = {
    el, body: el.querySelector('.dialog-body'),
    get dirty() { return dirty; },
    markDirty(v = true) { dirty = v; },
    close(result) {
      if (!el.isConnected) return;
      pop(entry); offTrap();
      leave(el, 120); leave(scrim, 180);
      unlockScroll();
      onClose?.(result);
      if (prevFocus?.isConnected) prevFocus.focus?.({ preventScroll: true });
    },
    /** Esc、×、“取消”、遮罩走这里：表单填过内容时先确认。 */
    async requestClose() {
      if (!guard || !dirty) { api.close(); return; }
      if (asking) return;
      asking = true;
      const ok = await confirmAction({ title: '放弃已填写的内容？', body: '关闭后这次填写的内容不会保存。', confirm: '放弃', danger: true, cancelLabel: '继续填写', cancelVariant: 'primary', local: true });
      asking = false;
      if (ok) api.close(); else el.focus({ preventScroll: true });
    },
  };
  el.addEventListener('click', (e) => { if (e.target.closest('[data-dlg="close"], [data-dlg="cancel"]')) api.requestClose(); });
  if (guard) { el.addEventListener('input', () => { dirty = true; }); el.addEventListener('change', () => { dirty = true; }); }
  scrim.addEventListener('click', () => { if (!guard) api.close(); });
  push(entry);
  lockScroll();
  hydrate(el);
  init?.(el, api);
  requestAnimationFrame(() => {
    const first = focus === 'cancel'
      ? el.querySelector('[data-dlg="cancel"]')
      : el.querySelector('[autofocus]') ?? el.querySelector('.dialog-body input:not([type="hidden"]):not([type="checkbox"]):not([disabled]), .dialog-body textarea:not([disabled])');
    (first ?? el).focus({ preventScroll: true });
  });
  return api;
}

/**
 * 确认分级（§3.17）：
 *   L1 confirmAction({ title:'停用入口 线路机2 :20015', body:'该节点的用户会立即断开…', confirm:'停用', danger:true, impact:'影响…' })
 *   L2 再加 typeToConfirm:'团队1-21'：输入对象名后按钮才可用。
 * 返回 Promise<boolean>。L0 不弹框：直接执行 + toast 带撤销（用 toast()）。
 */
export function confirmAction({ title, sub = '', body = '', impact = '', confirm = '确认', danger = false, typeToConfirm = null, cancelLabel = '取消', cancelVariant = 'secondary', action = 'generic', ctx = {}, local = false } = {}) {
  // 实时数据：L1 / L2 都是写操作——没接上的（只读、或随 Xray 迁到新系统的）不弹确认框，给提示并当作取消；
  // 已接上且开了写开关的（action 是 live-mode.js 里 wired 的 key）照常确认。
  // local: true 的确认（关闭对话框前“放弃已填写的内容？”）不是写操作，照常弹出。
  if (!local && guard(action, ctx)) return Promise.resolve(false);
  return new Promise((resolve) => {
    let done = false;
    const typeField = typeToConfirm ? `<label class="field"><span class="field-label">输入 <b>${esc(typeToConfirm)}</b> 以确认</span><input class="input" data-type-confirm autocomplete="off" spellcheck="false"></label>` : '';
    const impactNote = impact ? `<div class="note" data-tone="${danger ? 'bad' : 'warn'}">${icon('triangle-alert', 16)}<div class="note-body">${esc(impact)}</div></div>` : '';
    const api = openDialog({
      title, sub, size: 'sm',
      body: `<p class="dialog-text">${esc(body)}</p>${impactNote}${typeField}`,
      foot: button({ label: cancelLabel, variant: cancelVariant, attrs: { 'data-dlg': 'cancel' } }) + button({ label: confirm, variant: danger ? 'danger' : 'primary', attrs: { 'data-dlg': 'ok', disabled: !!typeToConfirm } }),
      // 破坏性确认默认聚焦“取消”；要输入对象名的 L2 聚焦输入框（craft #13）
      focus: typeToConfirm ? undefined : 'cancel',
      onClose: () => { if (!done) resolve(false); },
      init(el) {
        const ok = el.querySelector('[data-dlg="ok"]');
        const input = el.querySelector('[data-type-confirm]');
        input?.addEventListener('input', () => { ok.disabled = input.value.trim() !== typeToConfirm; });
        ok.addEventListener('click', () => { done = true; resolve(true); api.close(); });
      },
    });
  });
}

// ─── Toast ──────────────────────────────────────────────────────────────────

let toastRoot = null;
/**
 * toast({ message, tone:'ok'|'bad'|'info', action:{ label, run }, timeout })
 * 成功 4 秒消失（悬停暂停）；失败不自动消失（brief E2）；最多叠 3 条。
 */
export function toast({ message, tone = 'ok', action, timeout } = {}) {
  // 安全网：只读预览里任何“原型演示”式的假成功提示都不应出现（漏拦的写操作）→ 换成只读提示
  if (isReadOnly() && /原型/.test(String(message))) return readOnlyNotice('generic');
  if (!toastRoot) { toastRoot = document.createElement('div'); toastRoot.className = 'toasts'; toastRoot.setAttribute('aria-live', 'polite'); layer().appendChild(toastRoot); }
  const ic = tone === 'ok' ? 'circle-check' : tone === 'bad' ? 'circle-x' : 'info';
  const el = document.createElement('div');
  el.className = 'toast';
  el.dataset.tone = tone;
  el.setAttribute('role', tone === 'bad' ? 'alert' : 'status');
  el.innerHTML = `${icon(ic, 16)}<span class="toast-msg">${esc(message)}</span>${action ? button({ label: action.label, variant: 'link', size: 'sm', attrs: { 'data-t': 'act' } }) : ''}${iconButton({ icon: 'x', label: '关闭', size: 'sm', attrs: { 'data-t': 'x' } })}`;
  toastRoot.appendChild(el);
  while (toastRoot.children.length > 3) toastRoot.firstElementChild.remove();
  let ms = timeout ?? (tone === 'bad' ? 0 : 4000);
  let t = null;
  const close = () => { clearTimeout(t); leave(el, 180); };
  const arm = () => { if (ms) t = setTimeout(close, ms); };
  el.addEventListener('pointerenter', () => clearTimeout(t));
  el.addEventListener('pointerleave', arm);
  el.addEventListener('click', (e) => {
    const b = e.target.closest('[data-t]');
    if (!b) return;
    if (b.dataset.t === 'act') action.run?.();
    close();
  });
  arm();
  /** 同一条提示改文字 / 语气（长操作的进度：已提交 → 已生效 / 失败），不另起一条。 */
  const update = ({ message: m, tone: tn = tone, timeout: to } = {}) => {
    if (!el.isConnected) return;
    el.dataset.tone = tn;
    el.setAttribute('role', tn === 'bad' ? 'alert' : 'status');
    const ic2 = tn === 'ok' ? 'circle-check' : tn === 'bad' ? 'circle-x' : 'info';
    el.querySelector('.lucide')?.replaceWith(document.createRange().createContextualFragment(icon(ic2, 16)));
    if (m != null) el.querySelector('.toast-msg').textContent = m;
    clearTimeout(t); t = null;
    ms = to ?? (tn === 'bad' ? 0 : 4000);
    arm();
  };
  return { close, update };
}
