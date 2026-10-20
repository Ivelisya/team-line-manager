// flow.js — 流程类通用组件（上架流水线先用，别的页面也可以用）：
//   单选卡片 ChoiceCards、分段进度条 StepBar、时间线 Timeline、日志块 LogView、带单位输入 Affix、只写密码框 SecretInput。
// 外形都沿用现有语言：白底发丝线、圆角 10、12/13 px 字、状态色只用于图标与细线（SPEC §3.9、§3.11、§3.14）。

import { esc, attrs, on } from './dom.js';
import { icon } from '../icons.js';
import { stepIcon } from './table.js';

// ─── 单选卡片（§3.9 单选：16 圆，选中 1 px --accent 边 + 6 px 实心点） ───────

/**
 * choiceCards({ name, items:[{ value, label, desc, icon, disabled }], value, cols, label })
 * 原生 radio（视觉隐藏）+ 卡片外形：键盘 ← → 切换、Tab 进出与原生单选一致；change 事件照常冒泡。
 */
export function choiceCards({ name, items, value, cols = 3, label = '' }) {
  const body = items.map((it) => `<label class="choice"${it.disabled ? ' aria-disabled="true"' : ''}>
      <input type="radio" class="sr-only" name="${esc(name)}" value="${esc(it.value)}"${it.value === value ? ' checked' : ''}${it.disabled ? ' disabled' : ''}>
      <span class="choice-radio" aria-hidden="true"></span>
      <span class="choice-title">${it.icon ? icon(it.icon, 14) : ''}${esc(it.label)}</span>
      ${it.desc ? `<span class="choice-desc">${esc(it.desc)}</span>` : ''}
    </label>`).join('');
  return `<div class="choices" role="radiogroup"${label ? ` aria-label="${esc(label)}"` : ''} style="--cols:${Number(cols)}">${body}</div>`;
}

// ─── 分段进度条（历史批次“步骤进度”：每步一段，2 px，段间距 2） ─────────────

const SEG_TONE = { succeeded: 'ok', failed: 'bad', blocked: 'bad', running: 'info', waiting_input: 'warn', waiting_gate: 'info', rolled_back: 'warn' };
/**
 * stepBar(statuses, { text, label }) — statuses 是步骤状态数组（succeeded / failed / running / skipped / queued …）；
 * 成功 --ok、失败 --bad、进行中 --info、等待 --warn、未执行（排队、跳过）--line。右侧 `8/8`（Inter 12 --fg-2）。
 */
export function stepBar(statuses, { text = '', label = '步骤进度' } = {}) {
  const segs = statuses.map((s) => `<i${SEG_TONE[s] ? ` data-t="${SEG_TONE[s]}"` : ''}></i>`).join('');
  return `<span class="stepbar" role="img" aria-label="${esc(`${label} ${text}`)}"><span class="stepbar-track">${segs}</span>${text ? `<span class="stepbar-text num">${esc(text)}</span>` : ''}</span>`;
}

// ─── 时间线（步骤抽屉“时间线”页签；线路页接入任务的步骤列表也可以用） ─────

/**
 * timeline(items) — items: [{ status, title, meta, desc, current, attrs }]
 * 每项：状态图标 14（与 StepCell 同一套）+ 标题 13/500 + 右侧 12 px 元信息（耗时）+ 下方 12 px 说明；
 * 左侧 1 px 竖线把各项连起来。有 attrs（如 data-step）时整项是按钮；current 时底色 --selected。
 */
export function timeline(items, { label = '时间线' } = {}) {
  return `<ol class="timeline" aria-label="${esc(label)}">${items.map((it) => {
    const tag = it.attrs ? 'button' : 'div';
    const inner = `<span class="tl-icon" data-step="${esc(it.status)}">${stepIcon(it.status, 14)}</span><span class="tl-title">${it.title}</span><span class="tl-meta">${it.meta ?? ''}</span>${it.desc ? `<span class="tl-desc">${it.desc}</span>` : ''}`;
    return `<li class="tl-item"><${tag} class="tl-row"${tag === 'button' ? ' type="button"' : ''}${it.current ? ' aria-current="step"' : ''}${attrs(it.attrs ?? {})}>${inner}</${tag}></li>`;
  }).join('')}</ol>`;
}

// ─── 日志块（等宽 12/20、--sunken、--r-float；跟随末尾、搜索高亮） ────────────

/** logLines(lines, { query }) — lines: [{ t:'03:12', text, tone:'warn'|'bad'|'ok' }] → 行 HTML；query 命中处包 <mark>。 */
export function logLines(lines, { query = '' } = {}) {
  const q = query.trim().toLowerCase();
  const hl = (s) => {
    if (!q) return esc(s);
    const low = s.toLowerCase();
    let out = '', i = 0, j;
    while ((j = low.indexOf(q, i)) >= 0) { out += esc(s.slice(i, j)) + `<mark class="log-hit">${esc(s.slice(j, j + q.length))}</mark>`; i = j + q.length; }
    return out + esc(s.slice(i));
  };
  return lines.map((l) => `<div class="log-line"${l.tone ? ` data-tone="${esc(l.tone)}"` : ''}><span class="log-time">${esc(l.t ?? '')}</span><span class="log-text">${hl(l.text ?? '')}</span></div>`).join('');
}
/**
 * logView({ lines, query, empty, label, height }) → `<div class="log" role="log">…</div>`
 * height：可见高度（默认 360，超出滚动）。空日志显示一句 12 px 灰字。
 */
export function logView({ lines = [], query = '', empty = '没有日志', label = '日志', height } = {}) {
  const body = lines.length ? logLines(lines, { query }) : `<div class="log-empty">${esc(empty)}</div>`;
  return `<div class="log" role="log" tabindex="0" aria-label="${esc(label)}"${height ? ` style="--log-h:${Number(height)}px"` : ''}>${body}</div>`;
}
/**
 * followLog(logEl, { onFollowChange }) — 跟随末尾：追加内容后自动滚到底；用户向上滚动时自动停止跟随，滚回底部时恢复。
 * 返回 { follow, setFollow(v), append(lines, query), scrollEnd(), dispose() }。
 */
export function followLog(logEl, { follow = true, onFollowChange } = {}) {
  let f = follow;
  let programmatic = false;
  const atEnd = () => logEl.scrollHeight - logEl.scrollTop - logEl.clientHeight < 8;
  const scrollEnd = () => { programmatic = true; logEl.scrollTop = logEl.scrollHeight; requestAnimationFrame(() => { programmatic = false; }); };
  const off = on(logEl, 'scroll', null, () => {
    if (programmatic) return;
    const nf = atEnd();
    if (nf !== f) { f = nf; onFollowChange?.(f); }
  });
  if (f) scrollEnd();
  return {
    get follow() { return f; },
    setFollow(v) { f = v; if (v) scrollEnd(); },
    append(lines, query = '') {
      if (!lines.length) return;
      logEl.querySelector('.log-empty')?.remove();
      logEl.insertAdjacentHTML('beforeend', logLines(lines, { query }));
      if (f) scrollEnd();
    },
    scrollEnd,
    dispose: off,
  };
}

// ─── 带单位输入、只写密码框（§3.9） ─────────────────────────────────────────

/** affix(inputHtml, unit) → 单位放在框内右侧（12 --fg-2）：affix('<input class="input" …>', '秒') */
export function affix(inputHtml, unit) {
  return `<span class="affix">${inputHtml}<span class="affix-unit">${esc(unit)}</span></span>`;
}
/**
 * secretInput({ name, width, size, attrs }) — type=password，占位“只写，不回显”；不自动填充、不拼写检查。
 * 提交后由页面换成只读文字 `已提供 · 未验证`（neutralChip）。
 */
export function secretInput({ name = 'secret', width, size = 'lg', label = '密码', attrs: a = {} } = {}) {
  return `<input class="input${size === 'sm' ? ' input-sm' : ''}" type="password" name="${esc(name)}" placeholder="只写，不回显" autocomplete="new-password" spellcheck="false" aria-label="${esc(label)}"${width ? ` style="width:${Number(width)}px"` : ''}${attrs(a)}>`;
}
