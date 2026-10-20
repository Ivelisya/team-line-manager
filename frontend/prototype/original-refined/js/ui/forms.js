// forms.js — 表单控件的渲染助手（SPEC §3.9）：下拉 select、搜索框 searchBox，以及搜索框的统一行为 onSearch。
// 外观全部来自 components.css 的 .select / .search / .input；这里只负责拼 HTML 和事件。
//   下拉用原生 <select>（appearance:none + ChevronDown），键盘、读屏、手机选择器都是系统的。
//   搜索框：左侧 Search 图标；空时右侧键帽 `/`，有内容时换成清除按钮；输入防抖、Enter 立即、Esc 先清空再失焦。

import { icon } from '../icons.js';
import { esc, attrs, on, debounce } from './dom.js';
import { kbd } from './controls.js';

/**
 * select({ name, value, options, label, width, disabled, tip, attrs })
 *   options: [{ value, label }] 或分组 [{ group: '团队1', options: [{ value, label }] }]
 *   width：像素宽（写成 --w，窄屏由容器规则覆盖）；tip：包裹层的 tooltip（禁用时说明原因）。
 * 值变化时派发原生 change 事件，页面用 onSelect(root, name, fn) 接。
 */
export function select({ name, value = '', options = [], label = '', width, disabled = false, tip = '', attrs: a = {} }) {
  const opt = (o) => `<option value="${esc(o.value)}"${String(o.value) === String(value) ? ' selected' : ''}${o.disabled ? ' disabled' : ''}>${esc(o.label)}</option>`;
  const body = options.map((o) => (o.group ? `<optgroup label="${esc(o.group)}">${o.options.map(opt).join('')}</optgroup>` : opt(o))).join('');
  const style = width ? ` style="--w:${typeof width === 'number' ? `${width}px` : /^[\d.]+(px|%|em|rem|ch)?$/.test(String(width)) ? esc(width) : 'auto'}"` : '';
  return `<span class="select-wrap"${tip ? ` data-tip="${esc(tip)}"` : ''}><select class="select" data-select="${esc(name)}"${label ? ` aria-label="${esc(label)}"` : ''}${style}${disabled ? ' disabled' : ''}${attrs(a)}>${body}</select>${icon('chevron-down', 14)}</span>`;
}
/** 监听某个下拉：onSelect(root, 'team', (value) => …) → 解绑函数。 */
export function onSelect(root, name, fn) {
  return on(root, 'change', `[data-select="${name}"]`, (e, el) => fn(el.value, e));
}

/**
 * searchBox({ name, value, placeholder, label, pageSearch })
 * pageSearch: true → 带 data-page-search，全局 `/` 快捷键会聚焦它（一页只放一个）。
 */
export function searchBox({ name = 'q', value = '', placeholder = '搜索', label, pageSearch = true, width } = {}) {
  const style = width ? ` style="max-width:${typeof width === 'number' ? `${width}px` : /^[\d.]+(px|%|em|rem|ch)?$/.test(String(width)) ? esc(width) : 'auto'}"` : '';
  return `<label class="search" data-search="${esc(name)}"${style}>${icon('search', 14)}<input class="input" type="text" enterkeyhint="search" autocomplete="off" spellcheck="false" value="${esc(value)}" placeholder="${esc(placeholder)}" aria-label="${esc(label ?? placeholder)}"${pageSearch ? ' data-page-search' : ''}>${searchEnd(!!value)}</label>`;
}
function searchEnd(hasValue) {
  return hasValue
    ? `<button type="button" class="icon-btn icon-btn-sm" data-search-clear aria-label="清除搜索" data-tip="清除（Esc）">${icon('x', 14)}</button>`
    : kbd('/');
}
function syncEnd(box, hasValue) {
  const cur = box.querySelector('[data-search-clear], .kbd');
  const isClear = cur?.hasAttribute('data-search-clear');
  if (isClear === hasValue) return;
  const t = document.createElement('template');
  t.innerHTML = searchEnd(hasValue);
  if (cur) cur.replaceWith(t.content.firstElementChild); else box.appendChild(t.content.firstElementChild);
}
/** 程序化设置搜索框的值（URL 变化时回填），不触发回调。 */
export function setSearch(root, name, value) {
  const box = root.querySelector(`[data-search="${name}"]`);
  const input = box?.querySelector('input');
  if (!input || input.value === value) return;
  input.value = value;
  syncEnd(box, !!value);
}

/**
 * onSearch(root, name, fn, { delay }) — 逐键防抖（默认 400 ms）；Enter 立即；Esc：有内容先清空，再按失焦；
 * 清除按钮清空并把焦点留在框里。fn(value) 只在值真的变化时调用。返回解绑函数。
 */
export function onSearch(root, name, fn, { delay = 400 } = {}) {
  let last = root.querySelector(`[data-search="${name}"] input`)?.value ?? '';
  const emit = (v) => { if (v === last) return; last = v; fn(v); };
  const later = debounce((v) => emit(v), delay);
  const sel = `[data-search="${name}"]`;
  const offs = [
    on(root, 'input', `${sel} input`, (e, input) => { syncEnd(input.closest(sel), !!input.value); later(input.value.trim()); }),
    on(root, 'keydown', `${sel} input`, (e, input) => {
      if (e.isComposing) return;
      if (e.key === 'Enter') { later.cancel(); emit(input.value.trim()); }
      else if (e.key === 'Escape') {
        e.stopPropagation();
        if (input.value) { input.value = ''; syncEnd(input.closest(sel), false); later.cancel(); emit(''); } else input.blur();
      }
    }),
    on(root, 'click', `${sel} [data-search-clear]`, (e, b) => {
      e.preventDefault();
      const box = b.closest(sel);
      const input = box.querySelector('input');
      input.value = '';
      syncEnd(box, false);
      later.cancel();
      emit('');
      input.focus();
    }),
  ];
  return () => { later.cancel(); offs.forEach((f) => f()); };
}
