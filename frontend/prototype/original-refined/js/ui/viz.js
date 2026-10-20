// viz.js — 读数与数据图形：KPI 卡（§3.4）、进度线 / 额度条（§3.13）、心跳条 / 30 天条（§3.14）、图表（§3.15）。
// 读数语法全站一致：灰 12 px 标签 + 黑 13 px/600 数值 + 灰 12 px 单位 + 2 px 细进度线 + 灰 11 px 副行（ID-10）。

import { fmt, charts } from '../../../shared/index.js';
import { icon } from '../icons.js';
import { esc, num, on } from './dom.js';
import { infoButton, iconButton, dot } from './controls.js';
import { showTip, hideTip, refreshTip } from './overlay.js';
import { latencyF } from '../node-model.js';

// ─── KPI 卡 ─────────────────────────────────────────────────────────────────

/** 副行：可带一个 ≤420 宽时换上的短写法（“↓42.2 ↑41.4 Mbps”），两种都保持一行（fidelity #15）。 */
const subHtml = (sub, subShort) => (subShort ? `<span class="kpi-sub-long">${sub}</span><span class="kpi-sub-short">${subShort}</span>` : sub);

/**
 * kpiTile({ key, label, info, infoAlways, value(html), sub(html), subShort(html), action:{ icon, label, href }, filtered, clickable, pressed, loading })
 * info 的 ⓘ 平时隐藏、悬停卡片时出现；口径容易误解的几张卡传 infoAlways: true 常驻（fidelity #13）。
 * 右上角动作（action）只用于“跳到别的页面”；在本页筛选的卡整卡可点（clickable + pressed，consistency #13）。
 * 外形照首页 .overview-card：标签 12 / 主值 27 Inter 半粗 + 灰小单位 / 副行 12；高 106，无阴影。
 * 主值永远是 --fg，不按状态染色；状态放副行（带彩色圆点的计数）。
 * 实时更新：patchKpi(root, key, { value, sub }) 只改变化的部分。
 */
export function kpiTile({ key, label, info, infoAlways = false, value = '', sub = '', subShort = '', action, filtered = false, clickable = false, pressed = false, loading = false }) {
  const tag = clickable ? 'button' : 'article';
  const act = action ? (action.href
    ? `<a class="icon-btn kpi-action" href="${esc(/^#\//.test(String(action.href)) ? action.href : '#/home')}" aria-label="${esc(action.label)}" data-tip="${esc(action.label)}">${icon(action.icon, 16)}</a>`
    : iconButton({ icon: action.icon, label: action.label, size: 'md', cls: 'kpi-action', attrs: action.attrs ?? {} })) : '';
  const infoHtml = info ? infoButton(info, `${label}：口径说明`, { always: infoAlways }) : '';
  sub = subHtml(sub, subShort);
  const labelHtml = loading && !label.trim() ? '<span class="skel" style="width:64px;height:10px"></span>' : `<span>${esc(label)}</span>`;
  if (clickable && (info || action)) {
    // 可点 + ⓘ（或右上动作）：按钮不能套按钮，整卡是 article，点击区是铺满的透明按钮 .kpi-hit，ⓘ 与动作浮在它上面
    return `<article class="kpi is-clickable${loading ? ' is-loading' : ''}" data-kpi="${esc(key)}">
    <button type="button" class="kpi-hit" aria-pressed="${pressed}" aria-label="${esc(label)}：只看这些"></button>
    <div class="kpi-label">${labelHtml}${filtered ? '<span class="kpi-filtered">· 已筛选</span>' : ''}${infoHtml}</div>
    <div class="kpi-value" data-k="value">${loading ? '' : value}</div>
    <div class="kpi-sub" data-k="sub">${sub}</div>${act}
  </article>`;
  }
  return `<${tag} class="kpi${loading ? ' is-loading' : ''}" data-kpi="${esc(key)}"${clickable ? ` type="button" aria-pressed="${pressed}"` : ''}>
    <div class="kpi-label">${labelHtml}${filtered ? '<span class="kpi-filtered">· 已筛选</span>' : ''}${infoHtml}</div>
    <div class="kpi-value" data-k="value">${loading ? '' : value}</div>
    <div class="kpi-sub" data-k="sub">${sub}</div>${act}
  </${tag}>`;
}
/** 主值 HTML：kpiValue(fmt.rate(x)) → 83.7 + 灰色 Mbps；kpiValue('41', '/ 41') */
export function kpiValue(f, unitText) {
  if (typeof f === 'string') return `<span class="num">${esc(f)}</span>${unitText ? `<span class="unit">${esc(unitText)}</span>` : ''}`;
  if (!f || f.kind === 'missing') return '<span class="t-muted">—</span>';
  return `<span class="num">${esc(f.prefix)}${esc(f.num)}</span>${f.unit ? `<span class="unit">${esc(f.unit)}</span>` : ''}${f.suffix ? `<span class="unit">${esc(f.suffix)}</span>` : ''}`;
}
/** 副行里的状态计数：countDots([['故障', 1, 'bad'], ['降级', 0, 'warn']]) → 非零的带彩色圆点。 */
export function countDots(items, { hideZero = false } = {}) {
  return items.filter(([, n]) => !hideZero || n).map(([w, n, tone]) => `<span>${n ? dot({ tone }) : ''}${esc(w)} <span class="num">${esc(fmt.num(n))}</span></span>`).join(' · ');
}
export function patchKpi(root, key, { value, sub, subShort } = {}) {
  const k = root.querySelector(`[data-kpi="${key}"]`);
  if (!k) return;
  const v = k.querySelector('[data-k="value"]'), s = k.querySelector('[data-k="sub"]');
  if (value != null && v.innerHTML !== value) v.innerHTML = value;
  if (sub != null) { const h = subHtml(sub, subShort); if (s.__html !== h) { s.innerHTML = h; s.__html = h; } }
  k.classList.remove('is-loading');
}

// ─── 进度线 Meter ────────────────────────────────────────────────────────────

/**
 * meter({ icon, label, value(Fmt), pct, tone, sub, tip, slot })
 * pct 0–100 决定填充宽度；tone warn / bad（fmt.meterTone）只改填充色，数值文字不变色。
 * slot：给实时更新用的键（data-s），见 patchMeter。
 */
export function meter({ icon: ic, label, value, pct = 0, tone = null, sub = '', tip = '', slot = '', stale = false }) {
  return `<div class="meter${stale ? ' is-stale' : ''}"${tone ? ` data-tone="${tone}"` : ''}${slot ? ` data-s="${esc(slot)}"` : ''}${tip ? ` data-tip="${esc(tip)}"` : ''}>
    <div class="meter-head"><span class="meter-label">${ic ? icon(ic, 13) : ''}${esc(label)}</span><span class="meter-value" data-m="v">${meterValue(value)}</span></div>
    <div class="meter-track"><span class="meter-fill" data-m="f" style="width:${clampPct(pct)}%"></span></div>
    ${sub != null && sub !== false ? `<div class="meter-sub" data-m="s">${sub}</div>` : ''}
  </div>`;
}
function meterValue(f) {
  if (!f || f.kind === 'missing') return '<span class="t-muted">—</span>';
  return `<span class="num">${esc(f.prefix)}${esc(f.num)}</span>${f.unit ? `<span class="unit">${esc(f.unit)}</span>` : ''}`;
}
const clampPct = (p) => (Number.isFinite(p) ? Math.max(0, Math.min(100, p)) : 0).toFixed(2);
/** 实时更新一条进度线：只写变化的部分（宽度走 --dur-bar 过渡）。 */
export function patchMeter(el, { value, pct, tone, sub, tip }) {
  if (!el) return;
  const v = el.querySelector('[data-m="v"]'), f = el.querySelector('[data-m="f"]'), s = el.querySelector('[data-m="s"]');
  const vh = meterValue(value);
  if (v && v.innerHTML !== vh) v.innerHTML = vh;
  const w = `${clampPct(pct)}%`;
  if (f && f.style.width !== w) f.style.width = w;
  if ((el.dataset.tone ?? null) !== (tone ?? null)) { if (tone) el.dataset.tone = tone; else delete el.dataset.tone; }
  if (s && sub != null && s.innerHTML !== sub) s.innerHTML = sub;
  if (tip != null && el.dataset.tip !== tip) { el.dataset.tip = tip; refreshTip(el); }
}

// ─── 额度条 Quota ────────────────────────────────────────────────────────────

/** quota({ pct, tone }) — 6 px 胶囊；≥90% warn、≥100% bad；无上限时空轨道。 */
export function quota({ pct = 0, tone = null, slot = '' } = {}) {
  return `<div class="quota"${tone ? ` data-tone="${tone}"` : ''}${slot ? ` data-s="${esc(slot)}"` : ''}><span class="quota-fill" style="width:${clampPct(pct)}%"></span></div>`;
}
export function quotaTone(pct) { return pct == null ? null : pct >= 100 ? 'bad' : pct >= 90 ? 'warn' : null; }
export function patchQuota(el, { pct, tone }) {
  if (!el) return;
  const f = el.querySelector('.quota-fill');
  const w = `${clampPct(pct)}%`;
  if (f.style.width !== w) f.style.width = w;
  if ((el.dataset.tone ?? null) !== (tone ?? null)) { if (tone) el.dataset.tone = tone; else delete el.dataset.tone; }
}

// ─── 心跳条 LatencyStrip（§3.14） ────────────────────────────────────────────

const WIN_WORD = (s) => (s ? `近 ${Math.max(1, Math.round(s / 60))} 分钟` : '');
/**
 * beatModel(node) → { cap(html), win, cells:[{ t, tip }], stale } — 一种组件三种数据状态：
 *   有 TCP 样本：延迟 12 ms · 丢包 0%，20 格按 TCP 阈值着色；
 *   无样本有链路证据：20 格空，说明行换成 出口验证 309 ms · 上游 TCP 13 ms；
 *   都没有：延迟 — · 丢包 — + 20 个空格。
 */
export function beatModel(n) {
  const L = n.latency ?? {};
  const st = L.strip_tcp;
  const limits = n.limits ?? {};
  if (st?.ms?.length) {
    const cells = st.ms.map((v, i) => {
      const lost = st.lost?.[i];
      const t = lost ? 'bad' : v == null ? null : fmt.latencyTone(v, 'tcp', limits);
      return { t, tip: charts.latencyTip(st, i) };
    });
    while (cells.length < 20) cells.unshift({ t: null, tip: '无样本' });
    const med = fmt.latency(st.median_ms), loss = fmt.pctLoss(st.loss_pct);
    return {
      cap: `延迟 <b>${esc(med.text)}</b> · 丢包 <b>${esc(loss.text)}</b>`,
      win: WIN_WORD(st.window_s ?? st.ms.length * st.interval_s),
      tip: `TCP 探测每 ${st.interval_s} 秒一次 · ${charts.latencyCaption(st)}`,
      cells: cells.slice(-20),
    };
  }
  const empty = Array.from({ length: 20 }, () => ({ t: null, tip: '无样本' }));
  if (L.egress_ms != null || L.tcp_ms != null) {
    const eg = latencyF(n, 'egress'), tcp = latencyF(n, 'tcp');
    return { cap: `出口验证 <b>${esc(eg.text)}</b> · 上游 TCP <b>${esc(tcp.text)}</b>`, win: '无心跳样本', tip: '没有 TCP 心跳样本，显示链路证据', cells: empty };
  }
  // 线路机与节点同一种写法：有主节点端口的 TCP 心跳就画，没有样本时是缺测“—”（不再写“不适用”）
  return { cap: '延迟 <b class="t-muted">—</b> · 丢包 <b class="t-muted">—</b>', win: '', tip: '没有延迟样本', cells: empty };
}
/** latencyStrip(node, { slot }) → 心跳条 HTML（说明行 + 20 格）。 */
export function latencyStrip(n, { slot = 'beat', label = '延迟心跳' } = {}) {
  const m = beatModel(n);
  return `<div class="beat" data-s="${esc(slot)}">
    <div class="beat-cap"><span class="beat-main" data-b="cap" data-tip="${esc(m.tip)}">${m.cap}</span><span class="beat-win" data-b="win">${esc(m.win)}</span></div>
    <div class="beat-cells" data-beat tabindex="0" role="img" aria-label="${esc(label)}：${esc(m.cap.replace(/<[^>]+>/g, ''))}">${m.cells.map((c) => `<i${c.t ? ` data-t="${c.t}"` : ''} data-tip-fast data-tip="${esc(c.tip)}"></i>`).join('')}</div>
  </div>`;
}
export function patchBeat(el, n) {
  if (!el) return;
  const m = beatModel(n);
  const cap = el.querySelector('[data-b="cap"]'), win = el.querySelector('[data-b="win"]');
  if (cap.innerHTML !== m.cap) cap.innerHTML = m.cap;
  if (cap.dataset.tip !== m.tip) cap.dataset.tip = m.tip;
  if (win.textContent !== m.win) win.textContent = m.win;
  const cells = el.querySelectorAll('.beat-cells > i');
  m.cells.forEach((c, i) => {
    const e = cells[i];
    if (!e) return;
    if ((e.dataset.t ?? null) !== (c.t ?? null)) { if (c.t) e.dataset.t = c.t; else delete e.dataset.t; }
    if (e.dataset.tip !== c.tip) e.dataset.tip = c.tip;
  });
  refreshTip(el);
}

// ─── 30 天条 DayTracker ─────────────────────────────────────────────────────

export function dayTracker(t30) {
  if (!t30?.state) return '<span class="t-muted">—</span>';
  const cells = t30.state.map((s, i) => `<i data-s="${esc(s)}" data-tip-fast data-tip="${esc(dayTip(t30, i))}"></i>`).join('');
  return `<div class="days" data-beat tabindex="0" role="img" aria-label="近 30 天成功率">${cells}</div>`;
}
function dayTip(t30, i) {
  const [y, m, d] = (t30.start).split('-').map(Number);
  const day = new Date(Date.UTC(y, m - 1, d + i));
  const date = `${String(day.getUTCMonth() + 1).padStart(2, '0')}-${String(day.getUTCDate()).padStart(2, '0')}`;
  const st = t30.state[i];
  if (st === 'none') return `${date} · 未监控`;
  const a = t30.availability?.[i];
  const inc = t30.incidents?.[i] ?? 0;
  return [date, a == null ? '—' : fmt.pctAvail(a).text, inc ? `${inc} 次故障` : null].filter(Boolean).join(' · ');
}
export const dayLegend = () => `<div class="days-legend"><span><i style="background:var(--tracker-ok)"></i>正常</span><span><i style="background:var(--tracker-minor)"></i>≥99%</span><span><i style="background:var(--warn)"></i>≥95%</span><span><i style="background:var(--bad)"></i>故障</span><span><i></i>未监控</span></div>`;

// ─── 心跳格 / 日格的键盘与 tooltip（整组一个 Tab 位，← → 移动） ─────────────

let beatInstalled = false;
export function installBeats(root = document) {
  if (beatInstalled) return;
  beatInstalled = true;
  on(root, 'keydown', '[data-beat]', (e, box) => {
    const cells = [...box.children];
    let i = cells.findIndex((c) => c.classList.contains('is-active'));
    if (e.key === 'ArrowRight' || e.key === 'ArrowLeft' || e.key === 'Home' || e.key === 'End') {
      e.preventDefault();
      if (i < 0) i = cells.length;
      i = e.key === 'Home' ? 0 : e.key === 'End' ? cells.length - 1 : Math.max(0, Math.min(cells.length - 1, i + (e.key === 'ArrowRight' ? 1 : -1)));
      cells.forEach((c) => c.classList.remove('is-active'));
      cells[i].classList.add('is-active');
      showTip(cells[i]);
    } else if (e.key === 'Escape') { cells.forEach((c) => c.classList.remove('is-active')); hideTip(); }
  });
  on(root, 'focusout', '[data-beat]', (e, box) => { [...box.children].forEach((c) => c.classList.remove('is-active')); hideTip(); });
  // 触屏：点按格子出 tooltip
  on(root, 'pointerdown', '[data-beat] > i', (e, c) => { if (e.pointerType === 'touch') showTip(c); });
}

// ─── 图表（uPlot，§3.15） ────────────────────────────────────────────────────

/**
 * mountChart(el, { data, series:[{ label, color, fill }], kind, height, syncKey, yMin, yMax, markers })
 * 在 shared/charts.js mountTimeSeries 之上补两件事：↓ / 接收 系列的淡填充（--chart-fill），以及图例由面板头负责（legend:false）。
 * 线 2 px、平时不画点、只画横向发丝线网格、DOM 坐标轴、北京时间。返回 mountTimeSeries 的控制器。
 */
export async function mountChart(el, spec) {
  el.classList.add('chart');
  const ctl = await charts.mountTimeSeries(el, { legend: false, ...spec });
  const u = ctl.uplot;
  const applyFill = () => {
    const pct = parseFloat(getComputedStyle(el).getPropertyValue('--chart-fill')) || 8;
    (spec.series ?? []).forEach((s, i) => {
      if (!s.fill) return;
      const c = charts.resolveColor(el, s.color ?? `var(--chart-${i + 1})`);
      u.series[i + 1].fill = () => alpha(c, pct / 100);
    });
    u.redraw();
  };
  applyFill();
  const off = charts.watchTheme(() => requestAnimationFrame(applyFill));
  const destroy = ctl.destroy;
  ctl.destroy = () => { off(); destroy(); };
  return ctl;
}
function alpha(color, a) {
  const c = color.trim();
  let r, g, b;
  if (c.startsWith('#')) {
    const h = c.length === 4 ? c.slice(1).split('').map((x) => x + x).join('') : c.slice(1, 7);
    r = parseInt(h.slice(0, 2), 16); g = parseInt(h.slice(2, 4), 16); b = parseInt(h.slice(4, 6), 16);
  } else {
    const m = c.match(/rgba?\(([^)]+)\)/);
    if (!m) return c;
    [r, g, b] = m[1].split(/[ ,/]+/).map(Number);
  }
  return `rgba(${r}, ${g}, ${b}, ${a})`;
}
/** 图例（面板头右侧）：12×2 色标 + 文字（文字不用系列色）。 */
export function chartLegend(series) {
  return `<span class="chart-legend">${series.map((s, i) => `<span><i style="background:${s.color ?? `var(--chart-${i + 1})`}"></i>${esc(s.label)}</span>`).join('')}</span>`;
}
/** 表格视图（数据来自 ctl.toTable()）。 */
export function chartTable(t) {
  return `<div class="chart-table"><table class="dt"><thead><tr>${t.columns.map((c, i) => `<th${i ? ' class="r"' : ''}>${esc(c)}</th>`).join('')}</tr></thead><tbody>${t.rows.slice().reverse().map((r) => `<tr>${r.map((c, i) => `<td${i ? ' class="r v"' : ''}>${esc(c)}</td>`).join('')}</tr>`).join('')}</tbody></table></div>`;
}
