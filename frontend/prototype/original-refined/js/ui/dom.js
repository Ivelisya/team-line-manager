// dom.js — 所有组件共用的小工具：转义、Fmt → HTML、事件委托、安全存储、语气类名。
// 页面代码只通过这些函数把 fmt 的结果变成 HTML，不自己拼单位、不直接写颜色。

import { fmt } from '../../../shared/index.js';

export const esc = fmt.esc;

/** Query helpers. */
export const $ = (sel, root = document) => root.querySelector(sel);
export const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

/**
 * 事件委托：on(root, 'click', '[data-act]', (e, el) => …) → 解绑函数。
 * el 是匹配 selector 的最近祖先。
 */
export function on(root, type, selector, handler, opts) {
  const fn = (e) => {
    const el = selector ? e.target.closest?.(selector) : root;
    if (!el || (selector && !root.contains(el))) return;
    handler(e, el);
  };
  root.addEventListener(type, fn, opts);
  return () => root.removeEventListener(type, fn, opts);
}

/** 字符串 → DocumentFragment 的第一个元素。 */
export function el(html) {
  const t = document.createElement('template');
  t.innerHTML = html.trim();
  return t.content.firstElementChild;
}

/** HTML 属性串：attrs({ 'data-id': 'x', disabled: true, hidden: false }) → ' data-id="x" disabled'。 */
export function attrs(o = {}) {
  let s = '';
  for (const [k, v] of Object.entries(o)) {
    if (v === false || v == null) continue;
    s += v === true ? ` ${k}` : ` ${k}="${esc(v)}"`;
  }
  return s;
}

/** fmt 语气 → CSS 类（字色）：ok warn bad info muted secondary unknown。 */
export function toneClass(tone) { return tone ? `t-${tone}` : ''; }

/**
 * Fmt → HTML。数值（含前缀 $、<、↓）一律包在 .num 里，单位包在 .unit 里（12 px --fg-2），
 * 缺测、未设置等占位词按 kind 自动退一级颜色（SPEC §4.1 F-12/F-13）。
 *   num(fmt.rate(79000))            → <span class="num">79.0</span> <span class="unit">Kbps</span>
 *   num(fmt.money(2888,{perMonth}))  → <span class="num">$28.88</span><span class="unit">/月</span>
 * opts.plain: true → 单位不缩小（表格、弹层里的整串数值）；opts.cls 追加到外层。
 */
export function num(f, opts = {}) {
  if (f == null) return '<span class="num t-muted">—</span>';
  if (!(f instanceof fmt.Fmt)) return esc(f);
  const kindCls = { missing: 't-muted', unset: 't-muted', na: 't-muted', stale: 't-muted', pending: 't-warn', unverified: 't-warn', error: 't-bad', unlimited: 't-secondary' }[f.kind] ?? '';
  const tone = opts.tone === false ? '' : (f.kind === 'value' || f.kind === 'zero') && f.tone ? toneClass(f.tone) : '';
  const cls = [kindCls, tone, opts.cls].filter(Boolean).join(' ');
  const title = f.exact && opts.title !== false ? ` data-tip="${esc(f.exact)}"` : '';
  const isWord = !f.unit && !f.prefix && !f.suffix && !/\d/.test(f.num ?? f.text);
  if (isWord) return `<span class="${cls || 'txt'}"${title}>${esc(f.text)}</span>`;
  const unitCls = opts.plain ? 'unit unit-plain' : 'unit';
  // 文字前缀（“已过期 ”、“覆盖 ”、“负载 ”）按单位的样式排，符号前缀（$ < ↓ −）跟数字走
  const wordPrefix = /[一-鿿]/.test(f.prefix ?? '');
  const numPart = wordPrefix
    ? `<span class="${unitCls}">${esc(f.prefix.trim())}</span> <span class="num">${esc(f.num)}</span>`
    : `<span class="num">${esc(f.prefix)}${esc(f.num)}</span>`;
  const unitPart = f.unit ? `${f.sep ? ' ' : ''}<span class="${unitCls}">${esc(f.unit)}</span>` : '';
  const suffix = f.suffix ? `<span class="${unitCls}">${esc(f.suffix)}</span>` : '';
  return `<span class="val${cls ? ' ' + cls : ''}"${title}>${numPart}${unitPart}${suffix}</span>`;
}

/** 纯文本 Fmt（不拆单位），带 exact tooltip。 */
export function txt(f, cls = '') {
  if (f == null) return '<span class="t-muted">—</span>';
  const kindCls = { missing: 't-muted', unset: 't-muted', na: 't-muted', stale: 't-muted', pending: 't-warn', unverified: 't-warn', error: 't-bad', unlimited: 't-secondary' }[f.kind] ?? '';
  const tone = f.tone && (f.kind === 'value' || f.kind === 'zero' || f.kind === 'word') ? toneClass(f.tone) : '';
  const c = [kindCls, tone, cls].filter(Boolean).join(' ');
  return `<span${c ? ` class="${c}"` : ''}${f.exact ? ` data-tip="${esc(f.exact)}"` : ''}>${esc(f.text)}</span>`;
}

/**
 * 安全的 localStorage（隐私窗口、被禁用时不抛错，返回默认值）。
 * store.get('refined:theme', 'system')；值按 JSON 存。
 */
export const storage = {
  get(key, def = null) {
    try { const v = globalThis.localStorage?.getItem(key); return v == null ? def : JSON.parse(v); } catch { return def; }
  },
  set(key, value) {
    try { globalThis.localStorage?.setItem(key, JSON.stringify(value)); } catch { /* 忽略：只是便利项 */ }
  },
  remove(key) { try { globalThis.localStorage?.removeItem(key); } catch { /* 忽略 */ } },
};

/** 防抖。 */
export function debounce(fn, ms) {
  let t = null;
  const d = (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); };
  d.cancel = () => clearTimeout(t);
  return d;
}

/** requestAnimationFrame 合批。 */
export function rafBatch(fn) {
  let queued = false;
  return () => { if (queued) return; queued = true; requestAnimationFrame(() => { queued = false; fn(); }); };
}

/** 读取当前实际主题（light / dark）。 */
export function currentScheme() {
  const t = document.documentElement.dataset.theme;
  if (t === 'dark' || t === 'light') return t;
  return globalThis.matchMedia?.('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
}

/** prefers-reduced-motion */
export const reducedMotion = () => !!globalThis.matchMedia?.('(prefers-reduced-motion: reduce)').matches;

/** 国旗：21×14，来自首页原版 assets/flags。 */
export function flag(code, { w = 21, h = 14 } = {}) {
  if (!code || !/^[A-Z]{2}$/.test(String(code))) return ''; // 只认两位国家代码（路径里不能出现别的）
  const base = new URL('../../assets/flags/', import.meta.url).href;
  return `<img class="flag" src="${base}${esc(code)}.svg" width="${w}" height="${h}" alt="${esc(code)}" loading="lazy" decoding="async">`;
}
