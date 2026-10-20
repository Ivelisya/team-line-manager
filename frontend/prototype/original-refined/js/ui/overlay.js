// overlay.js — 浮层（SPEC §3.18）：提示 Tooltip、弹层 Popover、菜单 Menu、口径说明 InfoPopover。
// 外观同一种：--raised 底、发丝线、圆角 10、--shadow-float；进入上移 3 px + 淡入。
//   任何元素加 data-tip="文字" 就有 tooltip（延迟 400 ms；data-tip-fast 为 80 ms，给心跳格等数据小格用）。
//   任何按钮加 data-info="口径文字" 就是 ⓘ 口径说明（点击出 280 宽弹层，键盘与触屏可用）。

import { icon } from '../icons.js';
import { esc, on } from './dom.js';

const layer = () => document.getElementById('layer') ?? document.body;

// ─── Tooltip ────────────────────────────────────────────────────────────────

let tipEl = null, tipFor = null, tipTimer = null;
function tipText(el) { return el.dataset.tipNow ?? el.dataset.tip ?? ''; }
function placeTip(el) {
  if (!tipEl) return;
  const r = el.getBoundingClientRect();
  const t = tipEl.getBoundingClientRect();
  let top = r.top - t.height - 7;
  if (top < 60) top = r.bottom + 7;
  let left = r.left + r.width / 2 - t.width / 2;
  left = Math.max(8, Math.min(left, innerWidth - t.width - 8));
  tipEl.style.top = `${Math.round(top)}px`;
  tipEl.style.left = `${Math.round(left)}px`;
}
/** 立即显示 tooltip（键盘聚焦、心跳格用）。 */
export function showTip(el, text = tipText(el)) {
  clearTimeout(tipTimer);
  if (!text) return hideTip();
  // 锚点的弹层 / 菜单已经打开：不再叠一个提示（craft #10）
  if (el.getAttribute('aria-expanded') === 'true' || (openPop && openPop.anchor === el)) return hideTip();
  if (!tipEl) { tipEl = document.createElement('div'); tipEl.className = 'tip'; tipEl.setAttribute('role', 'tooltip'); layer().appendChild(tipEl); }
  if (tipFor !== el) { tipEl.style.animation = 'none'; tipEl.getBoundingClientRect(); tipEl.style.animation = ''; }
  tipFor = el;
  if (tipEl.textContent !== text) tipEl.textContent = text;
  placeTip(el);
}
export function hideTip() { clearTimeout(tipTimer); tipTimer = null; tipFor = null; if (tipEl) { tipEl.remove(); tipEl = null; } }
/** 数据变化后，若 tooltip 正显示在 el 上，原地更新内容（§5.3 规则 5：不关闭、不跳位）。 */
export function refreshTip(el) { if (tipFor && (el === tipFor || el.contains(tipFor))) showTip(tipFor); }

// ─── Popover ────────────────────────────────────────────────────────────────

let openPop = null;
/**
 * openPopover(anchor, { html, width, placement:'bottom-start'|'bottom-end'|'bottom', cls, role, onClose, focus })
 * → { el, update(html), close(), reposition(), pinned }
 * 同一时间只开一个弹层；Esc、点外部、页面滚动关闭；关闭后焦点回到 anchor（键盘打开时）。
 */
/** 锚点的“身份”：它身上的 data-* 属性（不含 data-tip）。页面重画后按身份找回同一个按钮（菜单跟着新按钮定位、关闭后焦点回到它）。 */
function anchorKey(a) {
  if (!a?.attributes) return '';
  return [...a.attributes].filter((x) => x.name.startsWith('data-') && !x.name.startsWith('data-tip')).map((x) => `[${x.name}="${CSS.escape(x.value)}"]`).join('');
}
export function openPopover(anchor, { html = '', width, placement = 'bottom-start', cls = '', role = 'dialog', label = '', onClose, focus = false } = {}) {
  closePopover();
  hideTip();
  const key = anchorKey(anchor);
  const resolve = () => {
    if (api.anchor?.isConnected || !key) return api.anchor;
    const found = document.querySelectorAll(key);
    const next = found.length === 1 ? found[0] : null; // 身份不唯一（例如每张卡都有的 data-act="today"）就不猜
    if (next) { api.anchor = next; next.setAttribute('aria-expanded', 'true'); ro?.disconnect(); ro?.observe(next); }
    return api.anchor;
  };
  const el = document.createElement('div');
  el.className = `pop${cls ? ' ' + cls : ''}`;
  el.setAttribute('role', role);
  if (label) el.setAttribute('aria-label', label);
  if (width) el.style.width = `${width}px`;
  el.innerHTML = html;
  layer().appendChild(el);
  anchor?.setAttribute('aria-expanded', 'true');
  const api = {
    el, anchor, pinned: false,
    update(h) { if (el.innerHTML !== h) el.innerHTML = h; api.reposition(); },
    reposition() {
      const anchor = resolve();
      if (!anchor?.isConnected) return;
      const r = anchor.getBoundingClientRect();
      const p = el.getBoundingClientRect();
      let top = r.bottom + 6;
      if (top + p.height > innerHeight - 8 && r.top - p.height - 6 > 60) top = r.top - p.height - 6;
      let left = placement === 'bottom-end' ? r.right - p.width : placement === 'bottom' ? r.left + r.width / 2 - p.width / 2 : r.left;
      left = Math.max(8, Math.min(left, innerWidth - p.width - 8));
      el.style.top = `${Math.round(top)}px`;
      el.style.left = `${Math.round(left)}px`;
    },
    close(returnFocus = false) {
      if (openPop !== api) return;
      openPop = null;
      ro?.disconnect();
      const a = resolve();
      a?.setAttribute('aria-expanded', 'false');
      el.remove();
      onClose?.();
      if (returnFocus) a?.focus?.({ preventScroll: true });
    },
  };
  // 锚点尺寸变化（多选后按钮文字变长）→ 跟着重新定位（craft #12）
  const ro = globalThis.ResizeObserver ? new ResizeObserver(() => api.reposition()) : null;
  if (anchor) ro?.observe(anchor);
  api.reposition();
  openPop = api;
  if (focus) (el.querySelector('[autofocus], button, [tabindex="0"], a') ?? el).focus?.();
  return api;
}
export function closePopover(returnFocus = false) { openPop?.close(returnFocus); }
export const currentPopover = () => openPop;

/**
 * 悬停预览 + 点击固定的弹层（今日流量弹层用，§3.18）。
 * hoverPopover(anchor, { render(api) → html, width, delay:150 })：精确指针悬停 150 ms 出现；点击固定；
 * 指针离开锚点和弹层 150 ms 后收起（未固定时）。返回 dispose()。
 */
export function hoverPopover(anchor, { render, width = 248, delay = 150, placement = 'bottom-end', label = '' }) {
  let t = null, leaveT = null, api = null;
  const fine = globalThis.matchMedia?.('(hover: hover) and (pointer: fine)').matches;
  const open = (pin) => {
    if (api && openPop === api) { if (pin) api.pinned = true; return; }
    api = openPopover(anchor, { html: '', width, placement, label, onClose: () => { api = null; } });
    api.pinned = !!pin;
    api.el.addEventListener('pointerenter', () => clearTimeout(leaveT));
    api.el.addEventListener('pointerleave', () => scheduleLeave());
    render(api);
  };
  const scheduleLeave = () => { clearTimeout(leaveT); leaveT = setTimeout(() => { if (api && !api.pinned) api.close(); }, 150); };
  const offs = [
    on(anchor, 'pointerenter', null, () => { if (!fine) return; clearTimeout(leaveT); t = setTimeout(() => open(false), delay); }),
    on(anchor, 'pointerleave', null, () => { clearTimeout(t); scheduleLeave(); }),
    on(anchor, 'click', null, (e) => { e.stopPropagation(); clearTimeout(t); if (api && api.pinned) { api.close(); return; } open(true); }),
  ];
  return () => offs.forEach((f) => f());
}

// ─── Menu ───────────────────────────────────────────────────────────────────

/**
 * openMenu(anchor, { items, onSelect(value, item), placement, minWidth, multi })
 * items: { group:'排序' } | { sep:true } | { value, label, icon, checked, end, count, danger, disabled }
 * 当前项右侧 Check（或 end 里自定义的方向箭头）。↑↓ 移动、Enter 选择、Esc 关闭。
 * multi: true → 多选菜单（节点页“套餐 ▾”“线路机 ▾”）：带 checked 的项前面是 16 px 勾选框，点击只切换、不关闭，
 *   onSelect(value, item, checked) 每次切换都会调用；没有 checked 的项（如“清除”）照常选择并关闭。count 显示在右侧。
 */
export function openMenu(anchor, { items, onSelect, placement = 'bottom-end', minWidth = 200, label = '', multi = false }) {
  const body = items.map((it, i) => {
    if (it.group) return `<div class="menu-group" role="presentation">${esc(it.group)}</div>`;
    if (it.sep) return '<div class="menu-sep" role="separator"></div>';
    const isCheck = multi && it.checked != null;
    const end = it.end ?? (it.count != null ? `<span class="num">${esc(String(it.count))}</span>` : !multi && it.checked ? icon('check', 14) : '');
    const role = isCheck ? 'menuitemcheckbox' : `menuitem${it.checked != null ? 'radio' : ''}`;
    const box = isCheck ? `<span class="menu-check" aria-hidden="true">${icon('check', 12)}</span>` : '';
    return `<button type="button" class="menu-item${it.danger ? ' is-danger' : ''}${it.count === 0 ? ' is-zero' : ''}" role="${role}"${it.checked != null ? ` aria-checked="${!!it.checked}"` : ''} data-i="${i}"${it.disabled ? ' disabled' : ''}>${box}${it.icon ? icon(it.icon, 14) : ''}<span>${esc(it.label)}</span>${end ? `<span class="menu-item-end">${end}</span>` : ''}</button>`;
  }).join('');
  const api = openPopover(anchor, { html: body, placement, cls: 'menu', role: 'menu', label });
  api.el.style.minWidth = `${minWidth}px`;
  api.reposition();
  const buttons = [...api.el.querySelectorAll('.menu-item:not(:disabled)')];
  const start = buttons.find((b) => b.getAttribute('aria-checked') === 'true') ?? buttons[0];
  start?.focus();
  api.el.addEventListener('keydown', (e) => {
    const i = buttons.indexOf(document.activeElement);
    if (e.key === 'ArrowDown') { e.preventDefault(); buttons[(i + 1) % buttons.length]?.focus(); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); buttons[(i - 1 + buttons.length) % buttons.length]?.focus(); }
    else if (e.key === 'Home') { e.preventDefault(); buttons[0]?.focus(); }
    else if (e.key === 'End') { e.preventDefault(); buttons[buttons.length - 1]?.focus(); }
    else if (e.key === 'Tab') { api.close(); }
  });
  api.el.addEventListener('click', (e) => {
    const b = e.target.closest('.menu-item');
    if (!b) return;
    const it = items[Number(b.dataset.i)];
    if (multi && it.checked != null) {
      it.checked = !it.checked;
      b.setAttribute('aria-checked', String(it.checked));
      onSelect?.(it.value, it, it.checked);
      requestAnimationFrame(() => api.reposition()); // 页面可能重画了触发按钮：找回新按钮并对齐
      return;
    }
    api.close(true);
    onSelect?.(it.value, it);
  });
  return api;
}

// ─── 安装：tooltip 委托、口径说明、+N、全局关闭 ─────────────────────────────

let installed = false;
export function installOverlays(root = document) {
  if (installed) return;
  installed = true;
  // tooltip：悬停
  on(root, 'pointerover', '[data-tip], [data-tip-now]', (e, el) => {
    if (e.pointerType === 'touch') return;
    if (tipFor === el) return;
    if (openPop && openPop.anchor === el) return;
    clearTimeout(tipTimer);
    const fast = el.hasAttribute('data-tip-fast') || !!tipFor;
    tipTimer = setTimeout(() => showTip(el), fast ? 80 : 400);
  });
  on(root, 'pointerout', '[data-tip], [data-tip-now]', (e, el) => {
    if (el.contains(e.relatedTarget)) return;
    clearTimeout(tipTimer);
    if (tipFor === el) hideTip();
  });
  // tooltip：键盘聚焦（只在 :focus-visible 时）
  on(root, 'focusin', '[data-tip]', (e, el) => { if (el.matches(':focus-visible')) showTip(el); });
  on(root, 'focusout', '[data-tip]', (e, el) => { if (tipFor === el) hideTip(); });
  on(root, 'tip-refresh', '[data-tip], [data-tip-now]', (e, el) => { if (tipFor === el || el.matches(':hover')) showTip(el); });
  // 触屏：点按出 tooltip（§5.9）
  on(root, 'click', '[data-tip-tap]', (e, el) => { if (tipFor === el) hideTip(); else showTip(el); });

  // ⓘ 口径说明
  on(root, 'click', '[data-info]', (e, b) => {
    e.stopPropagation();
    if (openPop?.anchor === b) { closePopover(); return; }
    openPopover(b, { html: `<div class="pop-body">${esc(b.dataset.info)}</div>`, width: 280, placement: 'bottom-start', label: '口径说明' });
  });
  // +N 标签：弹出完整列表
  on(root, 'click', '[data-more]', (e, b) => {
    e.stopPropagation();
    if (openPop?.anchor === b) { closePopover(); return; }
    const chips = b.dataset.more.split('\n').filter(Boolean).map((t) => `<span class="chip">${esc(t)}</span>`).join(' ');
    openPopover(b, { html: `<div class="pop-title">全部标签</div><div class="chip-wrap">${chips}</div>`, width: 240, placement: 'bottom-start' });
  });

  // 点外部、Esc、滚动关闭弹层；Esc 同时收起 tooltip
  document.addEventListener('pointerdown', (e) => {
    if (openPop && !openPop.el.contains(e.target) && !openPop.anchor?.contains(e.target)) closePopover();
  }, true);
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    if (tipFor) hideTip();
    if (openPop) { e.stopPropagation(); e.preventDefault(); closePopover(true); }
  }, true);
  addEventListener('scroll', (e) => {
    if (tipFor) hideTip();
    if (openPop && !(e.target instanceof Node && openPop.el.contains(e.target))) closePopover();
  }, true);
  addEventListener('resize', () => { hideTip(); openPop?.reposition(); });
}
