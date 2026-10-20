// charts.js — thin uPlot helpers for time series (brief §5.6) plus tiny SVG renderers
// (sparkline, mirrored sparkline, availability tracker, latency strip, stacked bar).
//
// Colours always come from CSS custom properties (--chart-1 …, --ok-dot …); canvas colours are
// resolved with getComputedStyle and re-resolved when the theme changes. Axis labels are DOM
// (not canvas) so they get real `font-variant-numeric: tabular-nums` and the page's numeric font.
// uPlot is loaded lazily with dynamic import(), so pages that only draw SVG never fetch it.

import * as fmt from './format.js';

const UPLOT_URL = new URL('./vendor/uplot/uPlot.min.mjs', import.meta.url);
export const UPLOT_CSS_URL = new URL('./vendor/uplot/uPlot.min.css', import.meta.url);
let uplotPromise = null;
/** Lazy-load the vendored uPlot (22 KB gzip). */
export function loadUplot() { return (uplotPromise ??= import(UPLOT_URL.href).then((m) => m.default)); }
let cssPromise = null;
/**
 * Inject uPlot's structural stylesheet once (DOM helper). Resolves when it has loaded, so charts
 * never measure an unstyled layout. Pages that already bundle the CSS can skip this (spec.css:false).
 */
export function ensureUplotCss(doc = globalThis.document) {
  if (!doc) return Promise.resolve();
  if (cssPromise) return cssPromise;
  const existing = doc.querySelector('link[data-uplot-css]');
  if (existing?.sheet) return (cssPromise = Promise.resolve());
  const l = existing ?? doc.createElement('link');
  cssPromise = new Promise((res) => { l.addEventListener('load', res, { once: true }); l.addEventListener('error', res, { once: true }); });
  if (!existing) { l.rel = 'stylesheet'; l.href = UPLOT_CSS_URL.href; l.dataset.uplotCss = ''; doc.head.appendChild(l); }
  return cssPromise;
}

// ─── colours from CSS variables ──────────────────────────────────────────────

/** 'var(--chart-1)' | '--chart-1' | '#hex' → 'var(--chart-1)' form for SVG style attributes. */
export function cssRef(c) { if (!c) return 'currentColor'; return c.startsWith('--') ? `var(${c})` : c; }
/** Resolve a colour spec to a concrete value on `el` (for canvas). Handles nested var() fallbacks. */
export function resolveColor(el, spec, fallback = '#888') {
  if (!spec) return fallback;
  const m = /^var\((--[\w-]+)(?:\s*,\s*(.+))?\)$/.exec(spec.trim()) ?? (spec.startsWith('--') ? [null, spec, null] : null);
  if (!m) return spec;
  const cs = el && globalThis.getComputedStyle ? globalThis.getComputedStyle(el) : null;
  const v = cs?.getPropertyValue(m[1]).trim();
  if (v) return v;
  return m[2] ? resolveColor(el, m[2], fallback) : fallback;
}
/**
 * Call `cb` when the theme changes: <html data-theme> mutations or prefers-color-scheme flips.
 * Returns an unsubscribe function.
 */
export function watchTheme(cb, doc = globalThis.document) {
  if (!doc) return () => {};
  const offs = [];
  if (globalThis.MutationObserver) {
    const mo = new MutationObserver(() => cb());
    mo.observe(doc.documentElement, { attributes: true, attributeFilter: ['data-theme', 'data-palette', 'class', 'style'] });
    offs.push(() => mo.disconnect());
  }
  const mq = globalThis.matchMedia?.('(prefers-color-scheme: dark)');
  if (mq?.addEventListener) { const h = () => cb(); mq.addEventListener('change', h); offs.push(() => mq.removeEventListener('change', h)); }
  return () => offs.forEach((f) => f());
}

// ─── ticks ───────────────────────────────────────────────────────────────────

function niceStep(span, count) {
  const raw = span / Math.max(1, count);
  const p = 10 ** Math.floor(Math.log10(raw));
  const m = raw / p;
  return (m <= 1 ? 1 : m <= 2 ? 2 : m <= 2.5 ? 2.5 : m <= 5 ? 5 : 10) * p;
}
/** ~count round ticks covering [min, max] (1-2-2.5-5 steps). Returns { ticks, step, min, max }. */
export function niceTicks(min, max, count = 5) {
  if (!(max > min)) { max = min + 1; }
  const step = niceStep(max - min, count);
  const lo = Math.floor(min / step + 1e-9) * step, hi = Math.ceil(max / step - 1e-9) * step;
  const d = stepDecimals(step);
  const clean = (v) => +v.toFixed(d + 2);
  const ticks = [];
  for (let k = Math.round(lo / step); k <= Math.round(hi / step); k++) ticks.push(clean(k * step));
  return { ticks, step, min: clean(lo), max: clean(hi) };
}

// Decimals needed to print every multiple of `step` exactly (2.5 → 1, 0.25 → 2, 50 → 0).
function stepDecimals(step) {
  const str = String(+step.toPrecision(6));
  return str.includes('e-') ? 3 : str.includes('.') ? Math.min(3, str.split('.')[1].length) : 0;
}

const UNIT_SETS = {
  rate: { base: 1000, units: fmt.SI_BITS },
  rateByte: { base: 1024, units: fmt.IEC_RATE, div: 8 },
  bytes: { base: 1024, units: fmt.IEC },
  pct: { base: null, units: ['%'] },
  ms: { base: null, units: ['ms'] },
  count: { base: null, units: [''] },
};
/**
 * Y ticks in display units. kind: rate | bytes | pct | ms | count. Mirror mode takes
 * min < 0. Returns { ticks (raw values), labels (numbers only, fixed decimals, tabular),
 * unit (shown once as the axis caption), min, max }.
 */
export function yTicks(min, max, kind = 'count', count = 5) {
  const set = kind === 'rate' && fmt.getRateUnit() === 'byte' ? UNIT_SETS.rateByte : UNIT_SETS[kind] ?? UNIT_SETS.count;
  const div0 = set.div ?? 1;
  const absMax = Math.max(Math.abs(min), Math.abs(max)) / div0 || 1;
  let i = 0, f = 1;
  if (set.base) while (absMax / f >= 1000 && i < set.units.length - 1) { f *= set.base; i++; }
  const scaleF = f * div0;
  const nt = niceTicks(min / scaleF, max / scaleF, count);
  const unit = set.units[i];
  if (kind === 'ms' && absMax >= 1000) {
    const s = niceTicks(min / 1000, max / 1000, count);
    return { ticks: s.ticks.map((v) => v * 1000), labels: s.ticks.map((v) => fmt.num(Math.abs(v), stepDecimals(s.step))), unit: 's', min: s.min * 1000, max: s.max * 1000 };
  }
  const d = stepDecimals(nt.step);
  return {
    ticks: nt.ticks.map((v) => v * scaleF),
    labels: nt.ticks.map((v) => fmt.num(Math.abs(v), d)),
    unit, min: nt.min * scaleF, max: nt.max * scaleF,
  };
}

const TIME_STEPS = [60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400, 172800, 604800];
/** Beijing-aligned time ticks: { ticks, labels, step } with ≤ maxTicks ticks. */
export function timeTicks(min, max, maxTicks = 6) {
  const span = max - min;
  const step = TIME_STEPS.find((s) => span / s <= maxTicks) ?? TIME_STEPS[TIME_STEPS.length - 1];
  const off = 8 * 3600;
  const first = Math.ceil((min + off) / step) * step - off;
  const ticks = [];
  for (let t = first; t <= max; t += step) ticks.push(t);
  const labels = ticks.map((t) => {
    const p = fmt.bjParts(t);
    if (step >= 86400 || (p.h === 0 && p.mi === 0)) return `${String(p.mo).padStart(2, '0')}-${String(p.d).padStart(2, '0')}`;
    return fmt.clockTime(t);
  });
  return { ticks, labels, step };
}

// ─── data shaping ────────────────────────────────────────────────────────────

/** x values for a columnar series { t0, step } of length n. */
export function xsOf(t0, step, n) { const xs = new Array(n); for (let i = 0; i < n; i++) xs[i] = t0 + i * step; return xs; }
/** bytes-per-bucket → bits per second (null stays null). */
export function bytesToBps(arr, step) { return arr.map((v) => (v == null ? null : (v * 8) / step)); }
/**
 * uPlot data from a columnar series: fields picked in order, optional conversion.
 *   columnar(s, ['down','up'], { toRate: true }) → [xs, downBps, upBps]
 */
export function columnar(s, fields, { toRate = false } = {}) {
  const n = s[fields[0]]?.length ?? 0;
  const xs = xsOf(s.t0, s.step, n);
  return [xs, ...fields.map((f) => (toRate ? bytesToBps(s[f], s.step) : s[f].slice()))];
}
/** Insert nulls where the x gap exceeds `factor` × the median interval (brief: 1.6). */
export function insertGaps(data, factor = 1.6) {
  const [xs, ...ys] = data;
  if (xs.length < 3) return data;
  const gaps = [];
  for (let i = 1; i < xs.length; i++) gaps.push(xs[i] - xs[i - 1]);
  const med = gaps.slice().sort((a, b) => a - b)[Math.floor(gaps.length / 2)];
  const outX = [xs[0]], outY = ys.map((y) => [y[0]]);
  for (let i = 1; i < xs.length; i++) {
    if (xs[i] - xs[i - 1] > med * factor) { outX.push(xs[i - 1] + med); outY.forEach((o) => o.push(null)); }
    outX.push(xs[i]); outY.forEach((o, k) => o.push(ys[k][i]));
  }
  return [outX, ...outY];
}
/** Largest value and its x (for a "峰值" marker). */
export function peakOf(xs, ys) {
  let bi = -1;
  for (let i = 0; i < ys.length; i++) if (ys[i] != null && (bi < 0 || ys[i] > ys[bi])) bi = i;
  return bi < 0 ? null : { x: xs[bi], y: ys[bi], i: bi };
}

const KIND_FMT = {
  rate: (v) => fmt.rate(v).text, bytes: (v) => fmt.bytes(v).text, pct: (v) => fmt.pctResource(v, { precise: true }).text,
  ms: (v) => fmt.latency(v).text, count: (v) => fmt.int(v).text, avail: (v) => fmt.pctAvail(v).text, loss: (v) => fmt.pctLoss(v).text,
};

// ─── time-series chart (uPlot) ───────────────────────────────────────────────

/**
 * Mount a time-series chart into `el`. Async (loads uPlot on first use).
 * spec:
 *   data        [xs, y1, y2, …]  (x = Unix seconds; null = gap)
 *   series      [{ label, color:'var(--chart-1)', fmt?(v)→string }]
 *   kind        'rate' | 'bytes' | 'pct' | 'ms' | 'count'   (axis units + default value format)
 *   mirror      true → series 1 above zero, series 2 drawn below zero (MirrorChart, §5.4 #15)
 *   height      total px including the x-axis band (160 panel / 240 detail)
 *   yMin/yMax   fixed y range (e.g. pct 0–100); default 0 … nice max
 *   thresholds  [{ value, label }] dashed line + label
 *   markers     [{ x, y, label }] e.g. peak
 *   syncKey     cursor sync group
 *   gapFactor   insert gaps above factor × median interval (default 1.6; 0 = off)
 *   tooltip     true (default DOM tooltip) | false; onCursor({ idx, x, values }) callback
 *   legend      auto (≥ 2 series) | true | false
 * Returns { uplot, setData(data), setLoading(bool), resize(), refreshColors(), toTable(), destroy() }.
 * DOM classes for styling: ts-root ts-legend ts-legend-item ts-key ts-unit ts-tick ts-tick-x ts-tick-y
 * ts-tip ts-tip-time ts-tip-row ts-threshold ts-marker.
 */
export async function mountTimeSeries(el, spec) {
  const [uPlot] = await Promise.all([loadUplot(), spec.css === false ? null : ensureUplotCss(el.ownerDocument)]);
  const doc = el.ownerDocument;
  const kind = spec.kind ?? 'count';
  const mirror = !!spec.mirror;
  const height = spec.height ?? 160;
  const seriesSpec = spec.series ?? [];
  const valueFmt = (i) => seriesSpec[i]?.fmt ?? KIND_FMT[kind] ?? String;
  const colors = {};
  const readColors = () => {
    colors.grid = resolveColor(el, 'var(--chart-grid)', 'rgba(128,128,128,.2)');
    colors.zero = resolveColor(el, 'var(--chart-zero)', 'rgba(128,128,128,.5)');
    colors.axis = resolveColor(el, 'var(--chart-axis)', '#888');
    colors.surface = resolveColor(el, 'var(--surface)', '#fff');
    colors.threshold = resolveColor(el, 'var(--text-3)', '#888');
    colors.series = seriesSpec.map((s, i) => resolveColor(el, s.color ?? `var(--chart-${i + 1})`, '#888'));
  };
  readColors();

  el.classList.add('ts-root');
  el.style.position = el.style.position || 'relative';
  const legendOn = spec.legend ?? seriesSpec.length >= 2;
  let legendEl = null;
  if (legendOn) {
    legendEl = doc.createElement('div');
    legendEl.className = 'ts-legend';
    legendEl.innerHTML = seriesSpec.map((s, i) => `<span class="ts-legend-item"><span class="ts-key" style="background:${cssRef(s.color ?? `var(--chart-${i + 1})`)};display:inline-block;width:10px;height:2px;vertical-align:middle;margin-right:4px"></span>${fmt.esc(s.label)}</span>`).join(' ');
    el.appendChild(legendEl);
  }
  const plotEl = doc.createElement('div');
  el.appendChild(plotEl);

  let data = prep(spec.data);
  let yt = null;
  const yWidth = () => Math.max(28, Math.ceil(Math.max(...(yt?.labels ?? ['0']).map((s) => s.length)) * 6.6) + 10);

  function prep(d) {
    let out = spec.gapFactor === 0 ? d : insertGaps(d, spec.gapFactor ?? 1.6);
    if (mirror && out.length > 2) out = [out[0], out[1], out[2].map((v) => (v == null ? null : -v)), ...out.slice(3)];
    return out;
  }
  function yRange(u, dmin, dmax) {
    let lo = spec.yMin ?? (mirror ? Math.min(0, dmin ?? 0) : Math.min(0, dmin ?? 0));
    let hi = spec.yMax ?? Math.max(dmax ?? 0, 0);
    for (const th of spec.thresholds ?? []) hi = Math.max(hi, th.value);
    if (hi === lo) hi = lo + 1;
    yt = yTicks(lo, hi, kind, spec.yTicks ?? (mirror ? 5 : 4));
    if (spec.yMin != null) yt.min = spec.yMin;
    if (spec.yMax != null) yt.max = spec.yMax;
    return [yt.min, yt.max];
  }

  // DOM layers: axis labels, unit caption, tooltip, threshold / marker labels
  const axesEl = doc.createElement('div');
  axesEl.className = 'ts-axes';
  axesEl.setAttribute('aria-hidden', 'true');
  axesEl.style.cssText = 'position:absolute;inset:0;pointer-events:none;font-variant-numeric:tabular-nums;color:var(--chart-axis,currentColor)';
  const tipEl = doc.createElement('div');
  tipEl.className = 'ts-tip';
  tipEl.setAttribute('role', 'status');
  tipEl.style.cssText = 'position:absolute;pointer-events:none;display:none;z-index:5;white-space:nowrap';

  function renderAxes(u) {
    const L = u.over.offsetLeft, T = u.over.offsetTop, W = u.over.clientWidth, H = u.over.clientHeight;
    const parts = [];
    if (yt) {
      yt.ticks.forEach((v, i) => {
        const y = T + u.valToPos(v, 'y');
        if (y < T - 1 || y > T + H + 1) return;
        parts.push(`<span class="ts-tick ts-tick-y" style="position:absolute;left:0;width:${L - 6}px;top:${y}px;transform:translateY(-50%);text-align:right">${yt.labels[i]}</span>`);
      });
      if (yt.unit) parts.push(`<span class="ts-unit" style="position:absolute;left:0;width:${L - 6}px;top:${Math.max(0, T - 22)}px;text-align:right">${fmt.esc(yt.unit)}</span>`);
    }
    const xs = u.data[0];
    if (xs.length) {
      const tt = timeTicks(u.scales.x.min, u.scales.x.max, Math.max(2, Math.floor(W / (spec.xTickSpace ?? 100))));
      tt.ticks.forEach((t, i) => {
        const x = L + u.valToPos(t, 'x');
        parts.push(`<span class="ts-tick ts-tick-x" style="position:absolute;left:${x}px;top:${T + H + 4}px;transform:translateX(-50%)">${tt.labels[i]}</span>`);
      });
    }
    for (const th of spec.thresholds ?? []) {
      const y = T + u.valToPos(th.value, 'y');
      parts.push(`<span class="ts-threshold" style="position:absolute;right:${u.root.clientWidth - (L + W)}px;top:${y - 14}px">${fmt.esc(th.label ?? '')}</span>`);
    }
    for (const mk of spec.markers ?? []) {
      if (mk.x < u.scales.x.min || mk.x > u.scales.x.max) continue;
      const x = L + u.valToPos(mk.x, 'x'), y = T + u.valToPos(mirror && mk.below ? -mk.y : mk.y, 'y');
      const right = x > L + W * 0.7;
      parts.push(`<span class="ts-marker" style="position:absolute;top:${y - (mk.below ? -4 : 18)}px;${right ? `right:${u.root.clientWidth - x + 6}px` : `left:${x + 6}px`}">${fmt.esc(mk.label ?? '')}</span>`);
    }
    axesEl.innerHTML = parts.join('');
  }

  function drawOverlays(u) {
    const ctx = u.ctx, { left, width } = u.bbox, pr = uPlot.pxRatio;
    ctx.save();
    if (mirror || (u.scales.y.min < 0 && u.scales.y.max > 0)) {
      const y = Math.round(u.valToPos(0, 'y', true)) + 0.5;
      ctx.strokeStyle = colors.zero; ctx.lineWidth = pr; ctx.beginPath(); ctx.moveTo(left, y); ctx.lineTo(left + width, y); ctx.stroke();
    }
    for (const th of spec.thresholds ?? []) {
      const y = Math.round(u.valToPos(th.value, 'y', true)) + 0.5;
      ctx.strokeStyle = colors.threshold; ctx.lineWidth = pr; ctx.setLineDash([4 * pr, 3 * pr]);
      ctx.beginPath(); ctx.moveTo(left, y); ctx.lineTo(left + width, y); ctx.stroke(); ctx.setLineDash([]);
    }
    for (const mk of spec.markers ?? []) {
      if (mk.x < u.scales.x.min || mk.x > u.scales.x.max) continue;
      const x = u.valToPos(mk.x, 'x', true), y = u.valToPos(mirror && mk.below ? -mk.y : mk.y, 'y', true);
      ctx.fillStyle = colors.series[mk.series ?? 0] ?? colors.axis;
      ctx.beginPath(); ctx.arc(x, y, 3 * pr, 0, Math.PI * 2); ctx.fill();
    }
    ctx.restore();
  }

  function onCursor(u) {
    const idx = u.cursor.idx;
    if (idx == null || u.cursor.left < 0) { tipEl.style.display = 'none'; spec.onCursor?.(null); return; }
    const x = u.data[0][idx];
    const values = seriesSpec.map((s, i) => { const v = u.data[i + 1]?.[idx]; return v == null ? null : mirror && i === 1 ? -v : v; });
    spec.onCursor?.({ idx, x, values });
    if (spec.tooltip === false) return;
    const rows = seriesSpec.map((s, i) => {
      const v = values[i];
      const txt = v == null ? fmt.DASH : valueFmt(i)(v);
      return `<div class="ts-tip-row"><span class="ts-key" style="display:inline-block;width:10px;height:2px;vertical-align:middle;margin-right:6px;background:${cssRef(s.color ?? `var(--chart-${i + 1})`)}"></span><b>${fmt.esc(txt)}</b> ${fmt.esc(s.label ?? '')}</div>`;
    }).join('');
    tipEl.innerHTML = `<div class="ts-tip-time">${fmt.esc(fmt.stamp(x, { seconds: false }).text)}</div>${rows}`;
    tipEl.style.display = 'block';
    const L = u.over.offsetLeft, T = u.over.offsetTop;
    const tw = tipEl.offsetWidth, W = u.root.clientWidth;
    let left = L + u.cursor.left + 12;
    if (left + tw > W) left = L + u.cursor.left - tw - 12;
    tipEl.style.left = `${Math.max(0, left)}px`;
    tipEl.style.top = `${T + 4}px`;
  }

  const opts = {
    width: Math.max(120, el.clientWidth || 600),
    height: Math.max(80, height - (legendEl ? legendEl.offsetHeight || 18 : 0)),
    padding: [22, 8, 0, 0], // top band holds the unit caption
    legend: { show: false },
    cursor: {
      y: false,
      drag: { x: false, y: false, setScale: false },
      points: { size: 8, width: 2, stroke: () => colors.surface, fill: (u, i) => colors.series[i - 1] },
      ...(spec.syncKey ? { sync: { key: spec.syncKey, setSeries: false } } : {}),
    },
    scales: { x: { time: true }, y: { range: yRange } },
    axes: [
      { scale: 'x', stroke: () => 'transparent', grid: { show: false }, ticks: { show: false }, size: 22, splits: () => [], values: () => [] },
      { scale: 'y', side: 3, stroke: () => 'transparent', size: () => yWidth(), ticks: { show: false }, grid: { stroke: () => colors.grid, width: 1 }, splits: () => yt?.ticks ?? [], values: (u, s) => s.map(() => '') },
    ],
    series: [{}, ...seriesSpec.map((s, i) => ({ label: s.label, stroke: () => colors.series[i], width: s.width ?? 2, points: { show: false }, spanGaps: false, dash: s.dash }))],
    hooks: { draw: [drawOverlays, renderAxes], setCursor: [onCursor] },
  };
  const u = new uPlot(opts, data, plotEl);
  u.root.querySelector('.u-wrap')?.append(axesEl, tipEl);
  // first draw happened before the overlay was attached / fonts settled: lay the labels out again
  renderAxes(u);
  globalThis.requestAnimationFrame?.(() => renderAxes(u));
  doc.fonts?.ready?.then(() => renderAxes(u));
  u.over.addEventListener('mouseleave', () => { tipEl.style.display = 'none'; });

  let ro = null;
  if (globalThis.ResizeObserver) {
    ro = new ResizeObserver(() => { const w = el.clientWidth; if (w && Math.abs(w - u.width) > 1) u.setSize({ width: w, height: u.height }); });
    ro.observe(el);
  }
  const offTheme = watchTheme(() => { readColors(); u.redraw(false); });

  return {
    uplot: u,
    /** Replace data (keeps the last frame until drawn; no skeleton flash). */
    setData(d) { data = prep(d); u.setData(data); el.removeAttribute('data-loading'); },
    /** Toggle data-loading on the root; CSS dims the previous frame. */
    setLoading(on) { on ? el.setAttribute('data-loading', '') : el.removeAttribute('data-loading'); },
    resize() { u.setSize({ width: el.clientWidth, height: u.height }); },
    refreshColors() { readColors(); u.redraw(false); },
    /** Table view of the same data: { columns, rows } with formatted text. */
    toTable() {
      const [xs, ...ys] = u.data;
      return {
        columns: ['时间（北京时间）', ...seriesSpec.map((s) => s.label)],
        rows: xs.map((x, i) => [fmt.stamp(x).text, ...ys.map((y, k) => (y[i] == null ? fmt.DASH : valueFmt(k)(mirror && k === 1 ? -y[i] : y[i])))]),
      };
    },
    destroy() { ro?.disconnect(); offTheme(); u.destroy(); el.innerHTML = ''; el.classList.remove('ts-root'); },
  };
}

/** Mirrored ↓/↑ traffic chart: down above zero (--chart-1), up below (--chart-2). §5.4 #15. */
export function mountMirror(el, { xs, down, up, height, syncKey, markers, kind = 'rate', labels = ['下行 ↓', '上行 ↑'], ...rest }) {
  return mountTimeSeries(el, {
    data: [xs, down, up], mirror: true, kind, height, syncKey, markers,
    series: [{ label: labels[0], color: 'var(--chart-1)' }, { label: labels[1], color: 'var(--chart-2)' }],
    ...rest,
  });
}

// ─── SVG renderers (strings; colours via CSS variables) ──────────────────────

/**
 * Path for a sparkline. Nulls break the line. Returns { d, last:{x,y}|null, min, max }.
 * opts: width, height, pad (stroke inset), min, max (fixed domain), baseline (e.g. 0 in domain).
 */
export function sparkPath(values, { width = 72, height = 20, pad = 1.5, min, max } = {}) {
  const vals = values ?? [];
  const nums = vals.filter((v) => v != null && Number.isFinite(v));
  if (!nums.length) return { d: '', last: null, min: null, max: null };
  let lo = min ?? Math.min(...nums), hi = max ?? Math.max(...nums);
  if (hi === lo) {
    // flat series: zero sits on the baseline, anything else runs through the middle
    if (lo === 0 || min != null) hi = lo + 1; else { lo -= Math.abs(lo) || 1; hi += Math.abs(hi) || 1; }
  }
  const n = vals.length;
  const X = (i) => pad + (n <= 1 ? 0 : (i * (width - 2 * pad)) / (n - 1));
  const Y = (v) => height - pad - ((v - lo) * (height - 2 * pad)) / (hi - lo);
  let d = '', pen = false, last = null;
  for (let i = 0; i < n; i++) {
    const v = vals[i];
    if (v == null || !Number.isFinite(v)) { pen = false; continue; }
    const x = +X(i).toFixed(2), y = +Y(v).toFixed(2);
    d += `${pen ? 'L' : 'M'}${x} ${y}`;
    pen = true;
    last = { x, y };
  }
  return { d, last, min: lo, max: hi };
}

/**
 * Sparkline SVG string (72×20, 1.5 px, no fill, --text-3, 3 px end dot — §5.4 #11).
 * opts: width, height, color, strokeWidth, dot, min, max, label (aria-label), className.
 */
export function sparkline(values, { width = 72, height = 20, color = 'var(--text-3)', strokeWidth = 1.5, dot = 3, min, max, label = '', className = 'spark' } = {}) {
  const p = sparkPath(values, { width, height, pad: Math.max(strokeWidth, dot / 2), min, max });
  const c = cssRef(color);
  return `<svg class="${className}" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="img"${label ? ` aria-label="${fmt.esc(label)}"` : ' aria-hidden="true"'} style="overflow:visible">`
    + `<path d="${p.d}" fill="none" style="stroke:${c}" stroke-width="${strokeWidth}" stroke-linejoin="round" stroke-linecap="round" vector-effect="non-scaling-stroke"/>`
    + (p.last && dot ? `<circle cx="${p.last.x}" cy="${p.last.y}" r="${dot / 2}" style="fill:${c}"/>` : '')
    + '</svg>';
}
/** Patch an existing sparkline <svg> in place (live updates, no re-mount). */
export function updateSparkline(svg, values, opts = {}) {
  const width = +svg.getAttribute('width'), height = +svg.getAttribute('height');
  const strokeWidth = opts.strokeWidth ?? 1.5, dot = opts.dot ?? 3;
  const p = sparkPath(values, { width, height, pad: Math.max(strokeWidth, dot / 2), min: opts.min, max: opts.max });
  svg.querySelector('path')?.setAttribute('d', p.d);
  const c = svg.querySelector('circle');
  if (c && p.last) { c.setAttribute('cx', p.last.x); c.setAttribute('cy', p.last.y); }
}

/**
 * Mirrored sparkline for ↓/↑ pairs (KPI 业务速率): down above the midline (--chart-1), up below
 * (--chart-2), both scaled to the same max so their sizes compare honestly.
 */
export function mirrorSparkline(down, up, { width = 64, height = 24, strokeWidth = 1.5, label = '', className = 'spark spark-mirror' } = {}) {
  const nums = [...(down ?? []), ...(up ?? [])].filter((v) => v != null);
  const max = nums.length ? Math.max(...nums, 1) : 1;
  const half = height / 2;
  const pd = sparkPath(down, { width, height: half, pad: strokeWidth / 2, min: 0, max });
  const pu = sparkPath((up ?? []).map((v) => (v == null ? null : max - v)), { width, height: half, pad: strokeWidth / 2, min: 0, max });
  return `<svg class="${className}" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="img"${label ? ` aria-label="${fmt.esc(label)}"` : ' aria-hidden="true"'}>`
    + `<line x1="0" x2="${width}" y1="${half}" y2="${half}" style="stroke:var(--chart-zero)" stroke-width="1" vector-effect="non-scaling-stroke"/>`
    + `<path d="${pd.d}" fill="none" style="stroke:var(--chart-1)" stroke-width="${strokeWidth}" stroke-linejoin="round" vector-effect="non-scaling-stroke"/>`
    + `<path d="${pu.d}" transform="translate(0 ${half})" fill="none" style="stroke:var(--chart-2)" stroke-width="${strokeWidth}" stroke-linejoin="round" vector-effect="non-scaling-stroke"/>`
    + '</svg>';
}

/** CSS colour reference for a tracker day state. */
export const TRACKER_COLORS = {
  ok: 'var(--tracker-ok, var(--ok-dot))', minor: 'var(--tracker-minor, var(--ok-dot))', degraded: 'var(--warn-dot)', fail: 'var(--bad-dot)',
  unknown: 'var(--unknown-ring)', none: 'var(--border-subtle)',
};

/** Tooltip text for tracker cell i: "9/12 · 99.2% · 1 次故障" (or "9/12 · 未监控"). */
export function trackerTip(t30, i) {
  const start = t30.start ?? t30.start_date;
  const [y, m, d] = start.split('-').map(Number);
  const day = new Date(Date.UTC(y, m - 1, d + i));
  const date = `${day.getUTCMonth() + 1}/${day.getUTCDate()}`;
  const st = t30.state[i];
  if (st === 'none') return `${date} · 未监控`;
  const a = t30.availability?.[i];
  const inc = t30.incidents?.[i] ?? 0;
  const cov = t30.coverage?.[i];
  const parts = [date, a == null ? fmt.DASH : fmt.pctAvail(a).text];
  if (inc) parts.push(`${inc} 次故障`);
  if (cov != null && cov < 95) parts.push(fmt.coverage(cov).text);
  return parts.join(' · ');
}

/**
 * 30-day availability tracker (§5.4 #12): 30 cells × 3 px, gap 1, height 14, radius 1 → 119 px.
 * Cells carry data-i (and tabindex + aria-label when focusable) for the view's tooltip.
 */
export function tracker(t30, { cell = 3, gap = 1, height = 14, radius = 1, focusable = false, className = 'tracker', label } = {}) {
  const n = t30?.state?.length ?? 0;
  const width = n * cell + Math.max(0, n - 1) * gap;
  let rects = '';
  for (let i = 0; i < n; i++) {
    const st = t30.state[i];
    const fill = TRACKER_COLORS[st] ?? TRACKER_COLORS.none;
    const a11y = focusable ? ` tabindex="0" role="img" aria-label="${fmt.esc(trackerTip(t30, i))}"` : '';
    rects += `<rect x="${i * (cell + gap)}" y="0" width="${cell}" height="${height}" rx="${radius}" data-i="${i}" data-state="${st}" style="fill:${fill}"${a11y}/>`;
  }
  const aria = label ?? `近 ${n} 天可用性`;
  return `<svg class="${className}" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="group" aria-label="${fmt.esc(aria)}">${rects}</svg>`;
}

/** Tooltip for latency strip sample i: "15:21:30 · 42 ms" / "15:21:30 · 丢包". */
export function latencyTip(strip, i) {
  const t = strip.t_end - (strip.dt?.[i] ?? (strip.ms.length - 1 - i) * strip.interval_s);
  return `${fmt.clockTime(t, { seconds: true })} · ${strip.lost?.[i] ? '丢包' : fmt.latency(strip.ms[i]).text}`;
}
/** Caption for a strip: "近 10 分钟 · 中位 42 ms · 丢包 0%". */
export function latencyCaption(strip) {
  if (!strip) return fmt.DASH;
  const win = fmt.duration(strip.window_s ?? strip.ms.length * strip.interval_s, { units: 1 }).text.replace(/^(\d+) 分$/, '$1 分钟');
  return `近 ${win} · 中位 ${fmt.latency(strip.median_ms).text} · 丢包 ${fmt.pctLoss(strip.loss_pct).text}`;
}

/**
 * 20-sample latency strip (§5.4 #13). mode 'cells' (tracker-sized coloured cells) or 'bars'
 * (height ∝ latency within the window, colour = tone). kind 'tcp' | 'egress', limits = node.limits.
 */
export function latencyStrip(strip, { kind = 'tcp', limits = {}, mode = 'cells', cell = 3, gap = 1, height = 14, radius = 1, focusable = false, className = 'lstrip' } = {}) {
  if (!strip?.ms) return `<svg class="${className}" width="0" height="${height}" aria-hidden="true"></svg>`;
  const n = strip.ms.length;
  const width = n * cell + Math.max(0, n - 1) * gap;
  const vals = strip.ms.filter((v, i) => v != null && !strip.lost?.[i]);
  const hi = Math.max(...vals, 1);
  let rects = '';
  for (let i = 0; i < n; i++) {
    const v = strip.ms[i], lost = strip.lost?.[i];
    const tone = lost ? 'bad' : v == null ? null : fmt.latencyTone(v, kind, limits);
    const fill = lost ? 'var(--bad-dot)' : v == null ? 'var(--border-subtle)' : tone === 'ok' ? 'var(--latency-ok, var(--ok-dot))' : `var(--${tone}-dot)`;
    const h = mode === 'bars' && !lost && v != null ? Math.max(2, Math.round((v / hi) * height)) : height;
    const a11y = focusable ? ` tabindex="0" role="img" aria-label="${fmt.esc(latencyTip(strip, i))}"` : '';
    rects += `<rect x="${i * (cell + gap)}" y="${height - h}" width="${cell}" height="${h}" rx="${radius}" data-i="${i}" data-tone="${tone ?? 'none'}" style="fill:${fill}"${a11y}/>`;
  }
  return `<svg class="${className}" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="group" aria-label="${fmt.esc(latencyCaption(strip))}">${rects}</svg>`;
}

/**
 * Horizontal stacked bar (team status distribution). segments: [{ value, color, label }].
 * 1 px gaps between non-empty segments; zero segments are skipped.
 */
export function stackBar(segments, { width = 96, height = 6, radius = 1, gap = 1, className = 'stackbar', label = '' } = {}) {
  const segs = segments.filter((s) => s.value > 0);
  const total = segs.reduce((a, s) => a + s.value, 0);
  const avail = width - gap * Math.max(0, segs.length - 1);
  let x = 0, out = '';
  segs.forEach((s, i) => {
    const w = i === segs.length - 1 ? width - x : Math.max(1, Math.round((s.value / total) * avail));
    out += `<rect x="${x}" y="0" width="${w}" height="${height}" rx="${radius}" data-label="${fmt.esc(s.label ?? '')}" style="fill:${cssRef(s.color)}"/>`;
    x += w + gap;
  });
  if (!segs.length) out = `<rect x="0" y="0" width="${width}" height="${height}" rx="${radius}" style="fill:var(--border-subtle)"/>`;
  return `<svg class="${className}" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="img" aria-label="${fmt.esc(label || segs.map((s) => `${s.label} ${s.value}`).join('，'))}">${out}</svg>`;
}
/** Status counts → stackBar segments in the fixed order 正常 · 降级 · 故障 · 未知 · 维护中. */
export function statusSegments(counts) {
  return ['ok', 'degraded', 'fail', 'unknown', 'maintenance'].map((c) => ({ value: counts?.[c] ?? 0, color: statusColor(c), label: fmt.health(c).word }));
}

/** Dot colour for a health code: 'var(--ok-dot)' … 'var(--unknown-ring)'. */
export function statusColor(code) { return fmt.toneVar(fmt.health(code).tone, 'dot'); }

/**
 * Sequential colour bucket (status wall "按数值着色"): 'var(--chart-seq-1)' … 'var(--chart-seq-5)'.
 * invert=true for "higher is better" metrics such as availability.
 */
export function seqColor(v, [lo, hi], { steps = 5, invert = false } = {}) {
  if (v == null || !Number.isFinite(v)) return 'var(--border-subtle)';
  let t = hi > lo ? (v - lo) / (hi - lo) : 0;
  t = Math.max(0, Math.min(1, invert ? 1 - t : t));
  return `var(--chart-seq-${Math.min(steps, 1 + Math.floor(t * steps))})`;
}

/** Micro-bar numbers (§5.4 #10): { pct 0–100 for the fill width, tone } — the view draws a div. */
export function microBar(value, { max = 100, limit } = {}) {
  if (value == null || !Number.isFinite(value)) return { pct: 0, tone: null, missing: true };
  return { pct: Math.max(0, Math.min(100, (value / max) * 100)), tone: fmt.meterTone(value, limit ?? max), missing: false };
}
