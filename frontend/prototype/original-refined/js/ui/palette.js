// palette.js — 命令面板 ⌘K（SPEC §3.21、§5.4）。逻辑用 shared/keys.js createPalette，这里只负责外观与键盘。
// 外观和全站同一种浮层语言：--raised、圆角 14、发丝线、--shadow-float；当前项软靛蓝（与侧栏当前项同一种）。

import { createPalette } from '../../../shared/index.js';
import { icon } from '../icons.js';
import { esc } from './dom.js';
import { kbd } from './controls.js';
import { hideTip, closePopover } from './overlay.js';
import { lockScroll, unlockScroll } from './layers.js';

const layer = () => document.getElementById('layer') ?? document.body;
const STATUS_TONE = { ok: null, degraded: 'warn', fail: 'bad', maintenance: 'info', unknown: 'unknown' };

/**
 * createCommandPalette({ items: () => [...], groups }) → { open(), close(), isOpen }
 * items 形如 keys.js：{ id, group, title, subtitle, keywords[], status, run(), danger, confirm, end }
 */
export function createCommandPalette({ items, groups = ['跳转', '节点', '线路机', '套餐', '操作'], registerLayer }) {
  const pal = createPalette({ items, groups, recentKey: 'refined:palette:recent' });
  let root = null, scrim = null, input = null, list = null, prevFocus = null, unregister = null;

  function render() {
    if (!list) return;
    const grouped = pal.grouped();
    const active = pal.active();
    const armed = pal.armed();
    if (!grouped.length) {
      // 空结果：不再指向不存在的选项；很长的查询只回显前 20 个字（craft #1、#29）
      const q = pal.query();
      list.innerHTML = `<div class="palette-empty">没有匹配“${esc(q.length > 20 ? `${q.slice(0, 20)}…` : q)}”的结果</div>`;
      input.removeAttribute('aria-activedescendant');
      return;
    }
    list.innerHTML = grouped.map((g) => `<div class="palette-group" role="presentation">${esc(g.group)}</div>${g.items.map((r) => {
      const it = r.item;
      const tone = it.status != null ? STATUS_TONE[it.status] : undefined;
      const dotHtml = it.status != null ? `<span class="dot"${tone ? ` data-tone="${tone}"` : ''}></span>` : it.icon ? icon(it.icon, 14) : '';
      const isArmed = armed === it.id;
      const end = isArmed ? '再按一次 ↵ 确认' : it.end ?? (r.index === active ? '↵' : '');
      // 副文字里的匹配也高亮（例如搜端口 20015 命中入口副行）
      return `<div class="palette-item${r.index === active ? ' is-active' : ''}${isArmed ? ' is-armed' : ''}" role="option" aria-selected="${r.index === active}" id="pal-${r.index}" data-i="${r.index}">${dotHtml}<span class="palette-title">${highlight(it.title, r.field === 'title' ? r.ranges : [])}</span>${it.subtitle ? `<span class="palette-sub">${highlight(it.subtitle, r.field === 'subtitle' ? r.subtitleRanges : [])}</span>` : ''}<span class="palette-end">${esc(end)}</span></div>`;
    }).join('')}`).join('');
    input.setAttribute('aria-activedescendant', `pal-${active}`);
    list.querySelector('.is-active')?.scrollIntoView({ block: 'nearest' });
  }
  function highlight(text, ranges) {
    if (!ranges?.length) return esc(text);
    let out = '', i = 0;
    for (const [a, b] of ranges) { out += esc(text.slice(i, a)) + `<mark>${esc(text.slice(a, b))}</mark>`; i = b; }
    return out + esc(text.slice(i));
  }
  function open() {
    if (root) { input.focus(); return; }
    hideTip(); closePopover();
    prevFocus = document.activeElement;
    pal.open();
    scrim = document.createElement('div');
    scrim.className = 'scrim palette-scrim';
    root = document.createElement('div');
    root.className = 'palette';
    root.setAttribute('role', 'dialog');
    root.setAttribute('aria-modal', 'true');
    root.setAttribute('aria-label', '命令面板');
    root.innerHTML = `<div class="palette-input">${icon('search', 16)}<input type="text" placeholder="搜索节点、IP、入口，或输入命令" aria-label="搜索" role="combobox" aria-expanded="true" aria-controls="pal-list" autocomplete="off" spellcheck="false">${kbd('esc')}</div><div class="palette-list" id="pal-list" role="listbox" tabindex="-1"></div><div class="palette-foot"><span>${kbd('↑')}${kbd('↓')} 选择</span><span>${kbd('↵')} 打开</span><span>${kbd('esc')} 关闭</span></div>`;
    layer().append(scrim, root);
    input = root.querySelector('input');
    list = root.querySelector('.palette-list');
    input.addEventListener('input', () => { pal.setQuery(input.value); render(); });
    input.addEventListener('keydown', (e) => {
      if (e.isComposing || e.keyCode === 229) return;
      if (e.key === 'ArrowDown' || (e.ctrlKey && e.key === 'n')) { e.preventDefault(); e.stopPropagation(); pal.move(1); render(); }
      else if (e.key === 'ArrowUp' || (e.ctrlKey && e.key === 'p')) { e.preventDefault(); e.stopPropagation(); pal.move(-1); render(); }
      else if (e.key === 'Enter') { e.preventDefault(); e.stopPropagation(); runActive(); }
      else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); close(); }
    });
    // 焦点困在面板里（aria-modal）：Tab / Shift+Tab 当作 ↓ / ↑，焦点始终留在输入框（craft #1）
    root.addEventListener('keydown', (e) => {
      if (e.key !== 'Tab') return;
      e.preventDefault(); e.stopPropagation();
      if (document.activeElement !== input) input.focus();
      pal.move(e.shiftKey ? -1 : 1); render();
    });
    root.addEventListener('pointerdown', (e) => { if (!e.target.closest('input')) e.preventDefault(); }); // 点列表不把焦点从输入框拿走
    list.addEventListener('pointermove', (e) => { const it = e.target.closest('.palette-item'); if (it && Number(it.dataset.i) !== pal.active()) { pal.setActive(Number(it.dataset.i)); render(); } });
    list.addEventListener('click', (e) => { const it = e.target.closest('.palette-item'); if (it) { pal.setActive(Number(it.dataset.i)); runActive(); } });
    scrim.addEventListener('click', close);
    unregister = registerLayer?.({ close });
    lockScroll();
    render();
    input.focus();
  }
  function runActive() {
    const r = pal.run();
    if (r.needsConfirm) { render(); return; }
    if (r.ran) close({ keepFocus: true });
  }
  function close({ keepFocus = false } = {}) {
    if (!root) return;
    pal.close();
    root.remove(); scrim.remove();
    root = scrim = input = list = null;
    unregister?.();
    unlockScroll();
    if (!keepFocus && prevFocus?.isConnected) prevFocus.focus?.();
  }
  return { open, close, get isOpen() { return !!root; }, invalidate: () => pal.invalidate() };
}
