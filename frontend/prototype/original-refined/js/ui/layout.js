// layout.js — 页头 PageHead + 鲜度（§3.3）、面板 / 明细 / 提示条（§3.11）、空 / 加载 / 错误（§3.20）。

import { fmt } from '../../../shared/index.js';
import { icon } from '../icons.js';
import { esc } from './dom.js';
import { button, iconButton } from './controls.js';

// ─── 页头 ───────────────────────────────────────────────────────────────────

/**
 * pageHead({ title, note, tools(html) }) — 全站同一个页头：
 *   [h1 24/600] [附注] ……… [鲜度] [视图或密度分段] [主操作] [次操作] [↻]
 * 静止时无底色，吸顶后才出现半透明画布色 + 模糊 + 底线（watchStuck）。
 */
export function pageHead({ title, note = '', tools = '' }) {
  // 末尾的“更多”（⋯）只在 ≤600 宽、页头里有次要按钮时出现：次要按钮收进菜单，只留主按钮与刷新（craft #8）
  const more = tools ? iconButton({ icon: 'ellipsis', label: '更多操作', size: 'lg', cls: 'page-more', attrs: { 'data-page-more': '', 'aria-haspopup': 'menu', 'aria-expanded': 'false' } }) : '';
  return `<div class="page-sentinel" aria-hidden="true"></div><header class="page-head"><h1 class="page-title">${esc(title)}</h1>${note ? `<span class="page-note">${esc(note)}</span>` : ''}<div class="page-tools">${tools}${more}</div></header>`;
}
/** 吸顶检测：哨兵离开视口（被顶栏盖住）时给页头加 is-stuck。返回解绑函数。 */
export function watchStuck(root) {
  const s = root.querySelector('.page-sentinel'), h = root.querySelector('.page-head');
  if (!s || !h || !globalThis.IntersectionObserver) return () => {};
  const top = parseInt(getComputedStyle(document.documentElement).getPropertyValue('--topbar-h'), 10) || 56;
  const io = new IntersectionObserver(([e]) => h.classList.toggle('is-stuck', !e.isIntersecting), { rootMargin: `-${top + 1}px 0px 0px 0px`, threshold: 0 });
  io.observe(s);
  return () => io.disconnect();
}

// ─── 鲜度 Freshness ─────────────────────────────────────────────────────────

/** 占位：freshness() 放进页头，app 的全局计时器每秒（60 秒后每 30 秒）更新所有 .fresh。 */
export function freshness() {
  // 不进 Tab 序列（它不是控件，craft #38）；数据源明细在悬停提示里
  return '<span class="fresh" data-fresh data-state="fresh" aria-live="off"><span class="dot"></span><span data-f="text">更新于 —</span></span>';
}
/**
 * 计算鲜度文字（§3.3 表）：新鲜 ≤ 期望间隔 × 2 → 更新于 15:22:18；2–5 倍 → 延迟 · 25 秒前；> 5 倍或失败 → 已断开 · 3 分钟前。
 * ageS：距上次实时推送的真实秒数；lastT：上次推送的（模拟）时刻；failed：请求失败。
 */
export function freshnessState({ ageS, lastT, interval = 2, failed = false }) {
  const f = fmt.freshness(ageS, interval);
  const agoWord = ageS == null ? '' : ageS < 60 ? `${Math.max(1, Math.round(ageS))} 秒前` : `${Math.floor(ageS / 60)} 分钟前`;
  if (failed || f.state === 'stale') return { state: 'stale', text: `已断开${agoWord ? ` · ${agoWord}` : ''}` };
  if (f.state === 'delayed') return { state: 'delayed', text: `延迟 · ${agoWord}` };
  return { state: 'fresh', text: `更新于 ${fmt.clockTime(lastT, { seconds: true })}` };
}
/** 把状态写进所有鲜度元素（只写变化的部分）。 */
export function paintFreshness(st, tip) {
  for (const el of document.querySelectorAll('[data-fresh]')) {
    if (el.dataset.state !== st.state) el.dataset.state = st.state;
    const t = el.querySelector('[data-f="text"]');
    if (t.textContent !== st.text) t.textContent = st.text;
    if (tip && el.dataset.tip !== tip) el.dataset.tip = tip;
  }
}

// ─── 面板、明细、提示条 ─────────────────────────────────────────────────────

/** panel({ title, count, sub, tools, body, flush }) — 白卡 + 发丝线 + 圆角 14；表格开头时 flush 贴边。 */
export function panel({ title, count, sub = '', tools = '', body = '', flush = false, cls = '', attrs = '' }) {
  return `<section class="panel${cls ? ' ' + cls : ''}"${attrs}><header class="panel-head"><h2 class="panel-title">${esc(title)}</h2>${count != null ? `<span class="panel-count">${esc(String(count))}</span>` : ''}${sub ? `<span class="panel-sub">${sub}</span>` : ''}<div class="panel-tools">${tools}</div></header><div class="panel-body${flush ? ' is-flush' : ''}">${body}</div></section>`;
}
/** kv([{ label, value(html) }], { cols }) — 标签在上 12/18 --fg-2，值在下 13/20 --fg（首页卡片 2×2 字段同一种写法）。 */
export function kv(items, { cols = 2, big = false } = {}) {
  return `<dl class="kv${big ? ' kv-big' : ''}" style="--kv-cols:${cols}">${items.filter(Boolean).map((it) => `<div class="kv-item"><dt>${esc(it.label)}</dt><dd${it.tip ? ` data-tip="${esc(it.tip)}"` : ''}>${it.value}</dd></div>`).join('')}</dl>`;
}
/** 左标签右值的明细行（主机页签的三组信息）。 */
export function dlRows(items) {
  return `<dl class="dl-rows">${items.filter(Boolean).map((it) => `<div><dt>${esc(it.label)}</dt><dd${it.cls ? ` class="${it.cls}"` : ''}${it.tip ? ` data-tip="${esc(it.tip)}"` : ''}>${it.value}</dd></div>`).join('')}</dl>`;
}
/** 读数块：reading({ label, value(html), sub }) */
export function reading({ label, value, sub = '' }) {
  return `<div class="reading"><div class="reading-label">${esc(label)}</div><div class="reading-value">${value}</div>${sub ? `<div class="reading-sub">${sub}</div>` : '<div class="reading-sub">&nbsp;</div>'}</div>`;
}
/** 提示条：note({ tone, title, body, action(html) }) */
export function note({ tone = 'info', title = '', body = '', action = '' }) {
  const ic = { ok: 'circle-check', warn: 'triangle-alert', bad: 'circle-x', info: 'info' }[tone] ?? 'info';
  return `<div class="note" data-tone="${tone}" role="${tone === 'bad' ? 'alert' : 'status'}">${icon(ic, 16)}<div class="note-body">${title ? `<span class="note-title">${esc(title)}</span>${body ? ' · ' : ''}` : ''}${body}</div>${action ? `<div class="note-action">${action}</div>` : ''}</div>`;
}

// ─── 空、加载、错误 ─────────────────────────────────────────────────────────

/** empty({ icon, text, action:{ label, attrs } }) — 文案统一句式：`没有符合筛选条件的节点 · 清除筛选`。 */
export function empty({ icon: ic = 'inbox', text, action, cls = '' }) {
  return `<div class="empty${cls ? ' ' + cls : ''}">${icon(ic, 20)}<div>${esc(text)}${action ? ` · ${button({ label: action.label, variant: 'link', size: 'sm', attrs: action.attrs ?? {} })}` : ''}</div></div>`;
}
/** 页面失败：CircleX + 标题 + 原因 + 影响 + 重新加载。 */
export function pageError({ title = '页面加载失败', reason = '', impact = '', retryAttrs = { 'data-act': 'reload' } }) {
  return `<div class="empty is-error">${icon('circle-x', 20)}<div class="empty-title">${esc(title)}</div>${reason ? `<div>${esc(reason)}</div>` : ''}${impact ? `<div class="faint">${esc(impact)}</div>` : ''}${button({ label: '重新加载', variant: 'secondary', size: 'md', attrs: retryAttrs })}</div>`;
}
/** 骨架块：skel(width, height) */
export function skel(w = '100%', h = 12, r) {
  return `<span class="skel" style="width:${typeof w === 'number' ? w + 'px' : w};height:${h}px${r ? `;border-radius:${r}` : ''}"></span>`;
}
