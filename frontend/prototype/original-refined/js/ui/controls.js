// controls.js — 按钮、chip 家族、分段控件、下划线页签、折叠、复制（SPEC §3.5–3.8、§5.7）。
// 渲染函数都返回 HTML 字符串；交互靠 installControls(document) 一次性装好的事件委托，页面不用逐个绑定。

import { fmt } from '../../../shared/index.js';
import { icon } from '../icons.js';
import { esc, attrs, num, on, $$ } from './dom.js';

// ─── 按钮 ────────────────────────────────────────────────────────────────────

/**
 * button({ label, icon, variant, size, caret, attrs })
 * variant: primary | secondary（默认）| quiet | text | link | danger | danger-quiet
 * size: lg（默认 36）| md（32）| sm（28）
 */
export function button({ label = '', icon: ic, iconEnd, variant = 'secondary', size = 'lg', caret = false, cls = '', attrs: a = {} } = {}) {
  const sz = size === 'lg' ? '' : ` btn-${size}`;
  const iconHtml = ic ? icon(ic, 14) : '';
  const end = caret ? icon('chevron-down', 12, 'btn-caret') : iconEnd ? icon(iconEnd, 14) : '';
  return `<button type="button" class="btn btn-${variant}${sz}${cls ? ' ' + cls : ''}"${attrs(a)}>${iconHtml}${label ? `<span>${esc(label)}</span>` : ''}${end}</button>`;
}

/** iconButton({ icon, label（必填，aria-label + tooltip）, size: lg|md|sm, attrs }) */
export function iconButton({ icon: ic, label, size = 'md', cls = '', attrs: a = {} } = {}) {
  const sz = size === 'md' ? '' : ` icon-btn-${size}`;
  const px = size === 'sm' ? 14 : 16;
  return `<button type="button" class="icon-btn${sz}${cls ? ' ' + cls : ''}" aria-label="${esc(label)}" data-tip="${esc(label)}"${attrs(a)}>${icon(ic, px)}</button>`;
}

// ─── chip 家族 ───────────────────────────────────────────────────────────────

/**
 * 状态 pill：statusPill(fmt.health(code, {stale}), { count, text })。
 * 圆点：实心（本色）/ 空心（未知、数据过期）/ 呼吸（维护中、进行中）。
 */
export function statusPill(h, { count, text, attrs: a = {}, cls = '' } = {}) {
  if (!h) return '';
  const tone = h.tone === 'unknown' || h.tone === 'muted' ? 'neutral' : h.tone;
  const dot = h.dot === 'ring' ? 'ring' : h.code === 'maintenance' || h.code === 'running' ? 'pulse' : h.dot === 'none' ? 'none' : 'solid';
  const title = h.modifier ? ` data-tip="${esc(h.modifier)}"` : '';
  return `<span class="pill${cls ? ' ' + cls : ''}" data-tone="${tone}" data-dot="${dot}"${title}${attrs(a)}>${esc(text ?? h.word)}${count != null ? ` <span class="num">${esc(fmt.num(count))}</span>` : ''}</span>`;
}
/** 任意语气的 pill：tonePill('已转发', 'ok')；dot: solid|ring|pulse|none */
export function tonePill(text, tone = 'neutral', { dot = 'solid', title } = {}) {
  return `<span class="pill" data-tone="${tone || 'neutral'}" data-dot="${dot}"${title ? ` data-tip="${esc(title)}"` : ''}>${esc(text)}</span>`;
}
/** 中性实心 chip：`无主机`、`家宽代理`、`未绑定`、`主节点` */
export function neutralChip(text, title) { return `<span class="chip chip-neutral"${title ? ` data-tip="${esc(title)}"` : ''}>${esc(text)}</span>`; }
/** 标签 chip（发丝线描边、灰字） */
export function tagChip(text, title) { return `<span class="chip"${title ? ` data-tip="${esc(title)}"` : ''}>${esc(text)}</span>`; }
/** 价格 chip：CircleDollarSign + $28.88/月（黑字 600） */
export function priceChip(f) {
  if (!f || f.kind === 'missing') return '';
  return `<span class="chip chip-price"${f.exact ? ` data-tip="${esc(f.exact)}"` : ''}>${icon('circle-dollar-sign', 12)}${num(f, { title: false, tone: false })}</span>`;
}
/** 利用率 chip：Gauge + ↓<1% ↑2% */
export function utilChip(pair, title = '带宽利用率（业务，上下行分别计算）') {
  if (!pair) return '';
  return `<span class="chip chip-util" data-tip="${esc(title)}">${icon('gauge', 12)}<span class="num">${esc(pair.text)}</span></span>`;
}
/** 卡片 footer 的带宽 chip（原版“50 Mbps”外形：发丝线描边、灰字、无图标）：bwChip({ text, tip }) */
export function bwChip(m) {
  if (!m) return '';
  return `<span class="chip chip-util" data-tip="${esc(m.tip)}"><span class="num">${esc(m.text)}</span></span>`;
}
/** IP 版本 / 时区 / 原名 标记 */
export function mark(text, { warn = false, title } = {}) { return `<span class="mark${warn ? ' mark-warn' : ''}"${title ? ` data-tip="${esc(title)}"` : ''}>${esc(text)}</span>`; }
/** 键帽 */
export function kbd(text) { return `<span class="kbd">${esc(text)}</span>`; }
/** +N 溢出 chip（点开看全部） */
export function moreChip(n, items = []) { return `<button type="button" class="chip chip-more" data-more="${esc(items.join('\n'))}" aria-label="还有 ${n} 个标签">+${n}</button>`; }
/** 可删 chip（已生效筛选） */
export function removableChip(text, a = {}) { return `<span class="chip">${esc(text)}<button type="button" class="chip-x" aria-label="移除 ${esc(text)}"${attrs(a)}>${icon('x', 12)}</button></span>`; }
/**
 * 筛选 chip：fchip({ label, value, group, count, pressed, flag: html, tip })
 * 选中：软靛蓝底（与侧栏当前项同一种）。计数为 0 时字 --fg-3，仍可点。
 */
export function fchip({ label, value, group, count, pressed = false, flagHtml = '', tip = '', cls = '' }) {
  return `<button type="button" class="fchip${count === 0 ? ' is-zero' : ''}${cls ? ' ' + cls : ''}" aria-pressed="${pressed}" data-fgroup="${esc(group)}" data-fvalue="${esc(value)}"${tip ? ` data-tip="${esc(tip)}"` : ''}>${flagHtml}<span class="fchip-text"><span class="fchip-label">${esc(label)}</span>${count != null ? `<span class="fchip-count">${esc(fmt.num(count))}</span>` : ''}</span></button>`;
}
/** 状态圆点：state on|off|unknown，或 tone */
export function dot({ state, tone, tip } = {}) {
  return `<span class="dot"${state ? ` data-state="${state}"` : ''}${tone ? ` data-tone="${tone}"` : ''}${tip ? ` data-tip="${esc(tip)}"` : ''}></span>`;
}
/** 口径说明按钮 ⓘ：点击出 InfoPopover（overlay.js 里的委托处理 data-info） */
export function infoButton(text, label = '口径说明', { always = false } = {}) {
  return `<button type="button" class="info-btn${always ? ' is-always' : ''}" aria-label="${esc(label)}" data-info="${esc(text)}" aria-expanded="false">${icon('info', 12)}</button>`;
}
/** 可复制的值：点值或图标复制，图标变 Check 1.5 s（§5.7） */
export function copyable(text, { display, cls = '' } = {}) {
  if (!text) return '<span class="t-muted">—</span>';
  return `<button type="button" class="copy${cls ? ' ' + cls : ''}" data-copy="${esc(text)}" aria-label="复制 ${esc(text)}"><span class="copy-text">${esc(display ?? text)}</span>${icon('copy', 14)}</button>`;
}

// ─── 分段控件 Segmented（§3.5） ──────────────────────────────────────────────

/**
 * segmented({ name, items:[{ value, label, count, disabled, dot }], value, size:'md'|'sm', role:'tablist'|'radiogroup', label })
 * 选中项用独立滑块在项之间滑动；← → 移动并选中，Home / End 到两端。
 * 值变化时在根元素上派发 'seg-change'（detail: { name, value }），也可以用 onSegment(root, name, fn)。
 */
export function segmented({ name, items, value, size = 'md', role = 'radiogroup', label = '', cls = '', disableZero = false }) {
  const itemRole = role === 'tablist' ? 'tab' : 'radio';
  const sel = role === 'tablist' ? 'aria-selected' : 'aria-checked';
  const body = items.map((it) => {
    const on_ = it.value === value;
    // disableZero：计数为 0 且没选中的项不可点（点进去是空页，craft #34）
    const dis = it.disabled || (disableZero && it.count === 0 && !on_);
    return `<button type="button" class="seg-item" role="${itemRole}" ${sel}="${on_}" tabindex="${on_ ? 0 : -1}" data-value="${esc(it.value)}"${dis ? ' disabled' : ''}${disableZero ? ' data-disable-zero' : ''}${it.tip ? ` data-tip="${esc(it.tip)}"` : ''}>${it.dot ?? ''}<span class="seg-text">${esc(it.label)}${it.count != null ? `<span class="seg-count">${esc(fmt.num(it.count))}</span>` : ''}</span></button>`;
  }).join('');
  return `<div class="seg${size === 'sm' ? ' seg-sm' : ''}${cls ? ' ' + cls : ''}" role="${role}" data-seg="${esc(name)}"${label ? ` aria-label="${esc(label)}"` : ''}><span class="seg-thumb" aria-hidden="true"></span>${body}</div>`;
}

/** 把滑块移到当前选中项（首次无动画）。 */
export function syncSegmented(root) {
  const thumb = root.querySelector(':scope > .seg-thumb');
  const cur = root.querySelector('.seg-item[aria-selected="true"], .seg-item[aria-checked="true"]');
  if (!thumb) return;
  if (!cur) { thumb.style.width = '0px'; return; }
  const first = !root.classList.contains('is-ready');
  if (first) thumb.style.transition = 'none';
  thumb.style.width = `${cur.offsetWidth}px`;
  thumb.style.transform = `translateX(${cur.offsetLeft}px)`;
  if (first) { root.classList.add('is-ready'); thumb.getBoundingClientRect(); thumb.style.transition = ''; }
}

/** 程序化设置分段值（不派发事件）。 */
export function setSegmented(root, value) {
  const role = root.getAttribute('role');
  const sel = role === 'tablist' ? 'aria-selected' : 'aria-checked';
  for (const b of root.querySelectorAll('.seg-item')) {
    const on_ = b.dataset.value === String(value);
    b.setAttribute(sel, String(on_));
    b.tabIndex = on_ ? 0 : -1;
    if (b.hasAttribute('data-disable-zero')) b.disabled = !on_ && b.querySelector('.seg-count')?.textContent === '0';
  }
  syncSegmented(root);
}
/** 更新分段计数（实时数据变化时只改数字）。counts: { value: n } */
export function setSegmentCounts(root, counts) {
  if (!root) return;
  for (const b of root.querySelectorAll('.seg-item')) {
    const c = b.querySelector('.seg-count');
    const n = counts[b.dataset.value];
    if (c && n != null) { const t = fmt.num(n); if (c.textContent !== t) c.textContent = t; }
    if (b.hasAttribute('data-disable-zero') && n != null) {
      const on_ = b.getAttribute('aria-checked') === 'true' || b.getAttribute('aria-selected') === 'true';
      const dis = n === 0 && !on_;
      if (b.disabled !== dis) b.disabled = dis;
    }
  }
  syncSegmented(root);
}
export function onSegment(root, name, fn) {
  return on(root, 'seg-change', `[data-seg="${name}"]`, (e) => fn(e.detail.value, e));
}

function selectSegItem(item, focus = false) {
  const root = item.closest('.seg');
  if (!root || item.disabled) return;
  const prev = root.querySelector('.seg-item[aria-selected="true"], .seg-item[aria-checked="true"]');
  setSegmented(root, item.dataset.value);
  if (focus) item.focus();
  if (prev !== item) {
    item.scrollIntoView?.({ block: 'nearest', inline: 'nearest' });
    root.dispatchEvent(new CustomEvent('seg-change', { bubbles: true, detail: { name: root.dataset.seg, value: item.dataset.value } }));
  }
}

// ─── 下划线页签 Tabs（§3.6） ─────────────────────────────────────────────────

/** tabs({ name, items:[{ value, label, count, unread }], value }) → 指示条在项之间滑动。 */
export function tabs({ name, items, value, label = '' }) {
  const body = items.map((it) => {
    const on_ = it.value === value;
    return `<button type="button" class="tab" role="tab" aria-selected="${on_}" tabindex="${on_ ? 0 : -1}" data-value="${esc(it.value)}"><span class="tab-text">${esc(it.label)}${it.count != null ? `<span class="tab-count">${esc(fmt.num(it.count))}</span>` : ''}</span>${it.unread ? '<span class="tab-unread" aria-label="有未读"></span>' : ''}</button>`;
  }).join('');
  return `<div class="tabs" role="tablist" data-tabs="${esc(name)}"${label ? ` aria-label="${esc(label)}"` : ''}>${body}<span class="tabs-ink" aria-hidden="true"></span></div>`;
}
export function syncTabs(root) {
  const ink = root.querySelector(':scope > .tabs-ink');
  const cur = root.querySelector('.tab[aria-selected="true"]');
  if (!ink || !cur) return;
  const first = !root.classList.contains('is-ready');
  if (first) ink.style.transition = 'none';
  ink.style.width = `${cur.offsetWidth}px`;
  ink.style.transform = `translateX(${cur.offsetLeft}px)`;
  if (first) { root.classList.add('is-ready'); ink.getBoundingClientRect(); ink.style.transition = ''; }
}
export function setTabs(root, value) {
  for (const b of root.querySelectorAll('.tab')) { const on_ = b.dataset.value === value; b.setAttribute('aria-selected', String(on_)); b.tabIndex = on_ ? 0 : -1; }
  syncTabs(root);
}
function selectTab(item, focus = false) {
  const root = item.closest('.tabs');
  const prev = root.querySelector('.tab[aria-selected="true"]');
  setTabs(root, item.dataset.value);
  if (focus) item.focus();
  if (prev !== item) root.dispatchEvent(new CustomEvent('tab-change', { bubbles: true, detail: { name: root.dataset.tabs, value: item.dataset.value } }));
}

// ─── 折叠 Disclosure（§3.11） ────────────────────────────────────────────────

/** disclosure({ title, meta, body, open }) — 按钮 + 区域；替代浏览器默认 ▶。 */
let discSeq = 0;
export function disclosure({ title, meta = '', body = '', open = false }) {
  const id = `disc-${++discSeq}`;
  return `<button type="button" class="disclosure" aria-expanded="${open}" aria-controls="${id}">${icon('chevron-right', 14)}<span>${esc(title)}</span>${meta ? `<span class="disclosure-meta">${esc(meta)}</span>` : ''}</button><div class="disclosure-body" id="${id}"${open ? '' : ' hidden'}>${body}</div>`;
}

// ─── 统一安装：委托 + 尺寸同步 ───────────────────────────────────────────────

let installed = false;
/** 在 document 上装一次：分段、页签、折叠、复制、+N 标签。 */
export function installControls(root = document) {
  if (installed) return;
  installed = true;
  on(root, 'click', '.seg-item', (e, b) => selectSegItem(b));
  on(root, 'keydown', '.seg-item', (e, b) => {
    const items = $$('.seg-item:not(:disabled)', b.closest('.seg'));
    const i = items.indexOf(b);
    let j = null;
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') j = (i + 1) % items.length;
    else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') j = (i - 1 + items.length) % items.length;
    else if (e.key === 'Home') j = 0;
    else if (e.key === 'End') j = items.length - 1;
    if (j == null) return;
    e.preventDefault();
    selectSegItem(items[j], true);
  });
  on(root, 'click', '.tab', (e, b) => selectTab(b));
  on(root, 'keydown', '.tab', (e, b) => {
    const items = $$('.tab:not(:disabled)', b.closest('.tabs'));
    const i = items.indexOf(b);
    let j = null;
    if (e.key === 'ArrowRight') j = (i + 1) % items.length;
    else if (e.key === 'ArrowLeft') j = (i - 1 + items.length) % items.length;
    else if (e.key === 'Home') j = 0;
    else if (e.key === 'End') j = items.length - 1;
    if (j == null) return;
    e.preventDefault();
    selectTab(items[j], true);
  });
  on(root, 'click', '.disclosure', (e, b) => {
    const open = b.getAttribute('aria-expanded') !== 'true';
    b.setAttribute('aria-expanded', String(open));
    const body = document.getElementById(b.getAttribute('aria-controls'));
    if (body) body.hidden = !open;
  });
  on(root, 'click', '[data-copy]', (e, b) => { e.stopPropagation(); copyText(b.dataset.copy, b); });
  // 尺寸变化后重新定位所有滑块与指示条
  const resync = () => { $$('.seg').forEach(syncSegmented); $$('.tabs').forEach(syncTabs); };
  globalThis.addEventListener('resize', resync);
  document.fonts?.ready?.then(resync);
}

/** 扫描 root 下新渲染的分段 / 页签，定位滑块（渲染后调用一次）；横向放不下的分段、页签、页头工具条加溢出渐隐。 */
export function hydrate(root) {
  root.querySelectorAll('.seg').forEach(syncSegmented);
  root.querySelectorAll('.tabs').forEach(syncTabs);
  root.querySelectorAll('.seg, .tabs, .page-tools').forEach(watchOverflow);
}

// ─── 溢出提示（craft #9）：横向滚动的控件只在溢出的一侧渐隐；选中项滚进可视区 ───
const watched = new WeakSet();
let overflowRO = null;
function paintOverflow(el) {
  const max = el.scrollWidth - el.clientWidth;
  const l = max > 1 && el.scrollLeft > 1, r = max > 1 && el.scrollLeft < max - 1;
  if (el.classList.contains('is-fade-l') !== l) el.classList.toggle('is-fade-l', l);
  if (el.classList.contains('is-fade-r') !== r) el.classList.toggle('is-fade-r', r);
}
function watchOverflow(el) {
  if (watched.has(el)) { paintOverflow(el); return; }
  watched.add(el);
  overflowRO ??= globalThis.ResizeObserver ? new ResizeObserver((es) => es.forEach((e) => paintOverflow(e.target))) : null;
  overflowRO?.observe(el);
  el.addEventListener('scroll', () => paintOverflow(el), { passive: true });
  const cur = el.querySelector('[aria-selected="true"], [aria-checked="true"]');
  if (cur && el.scrollWidth > el.clientWidth + 1) {
    const er = el.getBoundingClientRect(), cr = cur.getBoundingClientRect();
    if (cr.right > er.right) el.scrollLeft += cr.right - er.right + 24;
    else if (cr.left < er.left) el.scrollLeft -= er.left - cr.left + 24;
  }
  paintOverflow(el);
}

// ─── 复制（§5.7） ────────────────────────────────────────────────────────────

let live = null;
function announce(text) {
  if (!live) { live = document.createElement('div'); live.className = 'sr-only'; live.setAttribute('aria-live', 'polite'); document.body.appendChild(live); }
  live.textContent = text;
}
export async function copyText(text, btn) {
  let ok = false;
  try { await navigator.clipboard.writeText(text); ok = true; } catch { ok = false; }
  if (!btn) return ok;
  const svg = btn.querySelector('.lucide');
  if (ok) {
    btn.classList.add('is-copied');
    const prev = svg?.outerHTML;
    if (svg) svg.outerHTML = icon('check', 14);
    btn.dataset.tipNow = '已复制';
    announce(`已复制 ${text}`);
    setTimeout(() => {
      btn.classList.remove('is-copied');
      const s = btn.querySelector('.lucide');
      if (s && prev) s.outerHTML = prev;
      delete btn.dataset.tipNow;
    }, 1500);
  } else {
    const r = document.createRange();
    const t = btn.querySelector('.copy-text') ?? btn;
    r.selectNodeContents(t);
    const sel = getSelection(); sel.removeAllRanges(); sel.addRange(r);
    btn.dataset.tipNow = '复制失败，已选中文字，请按 ⌘C';
    setTimeout(() => delete btn.dataset.tipNow, 2500);
  }
  btn.dispatchEvent(new CustomEvent('tip-refresh', { bubbles: true }));
  return ok;
}
