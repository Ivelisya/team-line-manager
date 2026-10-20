// format.js — the only place numbers, dates and status words become text.
// Implements redesign brief §4 (formatting spec, status words §4.10, glossary §4.12).
//
// Every formatter returns a `Fmt`: it stringifies to the display text, and also
// carries the parts views need to typeset it (number vs unit in a smaller,
// secondary style), a tone (for colour tokens), the kind of value (real value,
// real zero, missing, unset …) and an exact value for tooltips.
//
// Pure: no DOM, no fetch. The clock is injectable (setClock) so relative times are
// computed against the snapshot's `as_of`, never the browser clock.

// ─── primitives ──────────────────────────────────────────────────────────────

export const DASH = '—';
export const MINUS = '−';
export const TZ = 'Asia/Shanghai';
export const TZ_LABEL = '北京时间';
const BJ_OFFSET = 8 * 3600; // Asia/Shanghai has had no DST since 1991

let clock = () => Math.floor(Date.now() / 1000);
/** Replace the clock used by relative formatters. `fn` returns Unix seconds. */
export function setClock(fn) { clock = typeof fn === 'function' ? fn : clock; }
/** Current time in Unix seconds, per the injected clock. */
export function now() { return clock(); }

/**
 * A formatted value.
 *  text   full display string            "1.23 GiB"
 *  prefix text before the number          "<", "$", "↓ ", "覆盖 "
 *  num    the number part                 "1.23"
 *  unit   the unit part (may be "")       "GiB"
 *  kind   value | zero | missing | unset | unlimited | pending | na | stale | unverified | error | word
 *  tone   null | ok | warn | bad | info | muted | secondary
 *  exact  tooltip text with the precise value, or null
 *  raw    the input value
 *  sep    separator between num and unit ("" for %, " " otherwise)
 */
export class Fmt {
  constructor(text, o = {}) {
    this.text = text;
    this.prefix = o.prefix ?? '';
    this.num = o.num ?? (o.unit || o.prefix ? '' : text);
    this.unit = o.unit ?? '';
    this.sep = o.sep ?? (this.unit && this.unit !== '%' ? ' ' : '');
    this.suffix = o.suffix ?? '';
    this.kind = o.kind ?? 'value';
    this.tone = o.tone ?? null;
    this.exact = o.exact ?? null;
    this.raw = o.raw;
    if (o.extra) Object.assign(this, o.extra);
  }
  toString() { return this.text; }
  /** true for real values, including a real zero */
  get ok() { return this.kind === 'value' || this.kind === 'zero'; }
}

function V(num, unit, o = {}) {
  const prefix = o.prefix ?? '';
  const sep = o.sep ?? (unit && unit !== '%' ? ' ' : '');
  const suffix = o.suffix ?? '';
  return new Fmt(prefix + num + (unit ? sep + unit : '') + suffix, { ...o, num, unit, sep, prefix, suffix });
}

const isNum = (v) => typeof v === 'number' && Number.isFinite(v);
const absent = (v) => v === null || v === undefined || (typeof v === 'number' && !Number.isFinite(v));

/** Round half away from zero to `d` decimals, robust to binary noise (1.005 → 1.01). */
export function round(v, d = 0) {
  const p = 10 ** d;
  const a = Math.round(Math.abs(v) * p * (1 + 2e-15)) / p;
  return v < 0 ? -a : a;
}

function groupInt(s) { return s.replace(/\B(?=(\d{3})+(?!\d))/g, ','); }

/** Grouped fixed-decimal number string: num(1234.5, 2) → "1,234.50"; negative uses U+2212. */
export function num(v, d = 0) {
  if (absent(v)) return DASH;
  const r = round(v, d);
  const [i, f] = Math.abs(r).toFixed(d).split('.');
  return (r < 0 ? MINUS : '') + groupInt(i) + (f ? '.' + f : '');
}

/** Grouped integer as a Fmt. int(232721) → "232,721"; optional unit ("个", "条"). */
export function int(v, unit = '') {
  if (absent(v)) return missing();
  const n = num(Math.round(v), 0);
  return V(n, unit, { kind: v === 0 ? 'zero' : 'value', raw: v, sep: unit ? ' ' : '' });
}

// 3 significant digits: <10 → 2 decimals, 10–99.9 → 1, ≥100 → integer (brief §4.2).
function sig3(v) {
  let d = v < 10 ? 2 : v < 100 ? 1 : 0;
  let r = round(v, d);
  if (d === 2 && r >= 10) { d = 1; r = round(v, 1); }
  if (d === 1 && r >= 100) { d = 0; r = round(v, 0); }
  return [r, d];
}

// Step up a unit whenever the value in the current unit is ≥ 1000 (so ≤ 3 integer digits).
function scale(v, base, units, integerBase) {
  let i = 0;
  while (v >= 1000 && i < units.length - 1) { v /= base; i++; }
  let r, d;
  if (i === 0 && integerBase) { r = Math.round(v); d = 0; } else [r, d] = sig3(v);
  if (r >= 1000 && i < units.length - 1) { v /= base; i++; [r, d] = sig3(v); }
  return { n: r.toFixed(d), unit: units[i], i };
}

// ─── placeholders (§4.9) ─────────────────────────────────────────────────────

/** 缺测、未知 → "—" (--text-3) */
export function missing(exact = null) { return new Fmt(DASH, { kind: 'missing', tone: 'muted', exact }); }
/** 配置缺失 → "未设置" (--text-3) */
export function unset() { return new Fmt('未设置', { kind: 'unset', tone: 'muted' }); }
/** 不限 → "不限" (--text-2); replaces "∞" */
export function unlimited() { return new Fmt('不限', { kind: 'unlimited', tone: 'secondary' }); }
/** 录入时未知 → "待定" (--warn-text) */
export function pending() { return new Fmt('待定', { kind: 'pending', tone: 'warn' }); }
/** 能力不适用 → capability tag, e.g. "无主机" (--text-3); show once per row */
export function na(word = '不适用') { return new Fmt(word, { kind: 'na', tone: 'muted' }); }
/** 读取失败 (--bad-text); view adds a retry affordance */
export function loadFailed() { return new Fmt('读取失败', { kind: 'error', tone: 'bad' }); }
/** 待核验: shows "—" plus a 待核验 chip; the raw formatted value goes to the tooltip */
export function unverified(value) {
  const inner = value instanceof Fmt ? value.text : String(value ?? DASH);
  return new Fmt(DASH, { kind: 'unverified', tone: 'warn', exact: `待核验 · 原值 ${inner}`, extra: { chip: '待核验' } });
}
/** 数据过期: keep the last value, mark hollow + --text-3; tooltip "3 分钟前的数据" */
export function stale(value, ageSeconds) {
  const f = value instanceof Fmt ? value : new Fmt(String(value ?? DASH));
  const when = isNum(ageSeconds) ? agoPhrase(ageSeconds) : '较早';
  return new Fmt(f.text, {
    prefix: f.prefix, num: f.num, unit: f.unit, sep: f.sep, suffix: f.suffix, raw: f.raw,
    kind: 'stale', tone: 'muted', exact: `${when}的数据`, extra: { hollow: true },
  });
}

// ─── bytes (§4.2) ────────────────────────────────────────────────────────────

export const IEC = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];

/** bytes(1318123456) → "1.23 GiB"; tooltip "1,318,123,456 B". 1024 base, ≥1000 steps up. */
export function bytes(v) {
  if (absent(v)) return missing();
  if (v === 0) return V('0', 'B', { kind: 'zero', raw: 0, exact: '0 B' });
  const s = scale(Math.abs(v), 1024, IEC, true);
  return V((v < 0 ? MINUS : '') + s.n, s.unit, { raw: v, exact: exactBytes(v) });
}
/** "1,318,123,456 B" */
export function exactBytes(v) { return absent(v) ? DASH : `${num(Math.round(v))} B`; }

/** Pair ↓/↑ bytes (down first). */
export function bytesPair(down, up) { return pair(bytes(down), bytes(up)); }

// ─── rates and bandwidth (§4.3) ──────────────────────────────────────────────

export const SI_BITS = ['bps', 'Kbps', 'Mbps', 'Gbps', 'Tbps'];
export const IEC_RATE = ['B/s', 'KiB/s', 'MiB/s', 'GiB/s', 'TiB/s'];
let rateUnit = 'bit';
/** Global rate unit preference: 'bit' (Mbps, default per brief) or 'byte' (MiB/s). */
export function setRateUnit(u) { if (u === 'bit' || u === 'byte') rateUnit = u; return rateUnit; }
export function getRateUnit() { return rateUnit; }

const ARROW = { down: '↓', up: '↑' };

/**
 * rate(12_400_000) → "12.4 Mbps" (bit mode) / "1.48 MiB/s" (byte mode).
 * Input is always bits per second. Non-zero below 1 Kbps → "<1 Kbps"; true zero → "0 bps".
 * opts.unit overrides the global preference; opts.dir 'down'|'up' prefixes ↓/↑.
 */
export function rate(bps, { unit = rateUnit, dir } = {}) {
  if (absent(bps)) return missing();
  const prefix = dir ? ARROW[dir] + ' ' : '';
  const exact = exactRate(bps);
  const a = Math.abs(bps);
  if (unit === 'byte') {
    const B = a / 8;
    if (bps === 0) return V('0', 'B/s', { prefix, kind: 'zero', raw: 0, exact });
    if (B < 1024) return V('1', 'KiB/s', { prefix: prefix + '<', raw: bps, exact });
    const s = scale(B, 1024, IEC_RATE, true);
    return V(s.n, s.unit, { prefix, raw: bps, exact });
  }
  if (bps === 0) return V('0', 'bps', { prefix, kind: 'zero', raw: 0, exact });
  if (a < 1000) return V('1', 'Kbps', { prefix: prefix + '<', raw: bps, exact });
  const s = scale(a, 1000, SI_BITS, false);
  return V(s.n, s.unit, { prefix, raw: bps, exact });
}
/** "12,400,000 bit/s（1.48 MiB/s）" */
export function exactRate(bps) {
  if (absent(bps)) return DASH;
  const B = Math.abs(bps) / 8;
  const s = B === 0 ? { n: '0', unit: 'B/s' } : scale(B, 1024, IEC_RATE, true);
  return `${num(Math.round(bps))} bit/s（${s.n} ${s.unit}）`;
}
/** Pair of rates, down first: { down, up, text: "↓ 12.4 Mbps ↑ 1.10 Mbps" } */
export function ratePair(downBps, upBps, opts = {}) {
  return pair(rate(downBps, { ...opts, dir: 'down' }), rate(upBps, { ...opts, dir: 'up' }));
}
function pair(down, up) {
  return { down, up, text: `${down.text} ${up.text}`, toString() { return this.text; } };
}

/** Nominal bandwidth, whole numbers: 50e6 → "50 Mbps", 1e9 → "1 Gbps" (2.5e9 → "2.5 Gbps"). */
export function bandwidth(bps) {
  if (absent(bps) || bps <= 0) return absent(bps) ? missing() : unset();
  let v = bps, i = 0;
  while (v >= 1000 && i < SI_BITS.length - 1) { v /= 1000; i++; }
  const r = round(v, 2);
  const n = Number.isInteger(r) ? String(r) : String(r).replace(/0+$/, '');
  return V(n, SI_BITS[i], { raw: bps, exact: `${num(bps)} bit/s` });
}

// ─── percentages (§4.4) ──────────────────────────────────────────────────────

function pctV(n, raw, o = {}) { return V(n, '%', { raw, ...o }); }

/** CPU / memory / disk: integer in lists, 1 decimal with {precise:true} (detail, tooltip). */
export function pctResource(v, { precise = false } = {}) {
  if (absent(v)) return missing();
  return pctV(num(v, precise ? 1 : 0), v, { exact: `${num(v, 1)}%`, kind: v === 0 ? 'zero' : 'value' });
}
/** Per-core load: integer, may exceed 100. */
export function pctLoad(v) {
  if (absent(v)) return missing();
  return pctV(num(v, 0), v, { kind: v === 0 ? 'zero' : 'value' });
}
/** "负载 132%（5 分钟 · 4 核）" */
export function loadLabel(pct, { cores, windowMin = 5 } = {}) {
  const p = pctLoad(pct);
  const tail = cores ? `（${windowMin} 分钟 · ${cores} 核）` : `（${windowMin} 分钟）`;
  return new Fmt(`负载 ${p.text}${tail}`, { prefix: '负载 ', num: p.num, unit: '%', sep: '', suffix: tail, kind: p.kind, tone: p.tone, raw: pct });
}
/** Raw load averages "1.21 / 1.32 / 0.98". */
export function loadTriple(l1, l5, l15) {
  if ([l1, l5, l15].every(absent)) return missing();
  return new Fmt([l1, l5, l15].map((x) => (absent(x) ? DASH : num(x, 2))).join(' / '), { raw: [l1, l5, l15] });
}

// Completeness ratios never round up to a perfect 100.
function capBelow100(v, d) {
  const r = round(v, d);
  if (v < 100 && r >= 100) return (100 - 10 ** -d).toFixed(d);
  return r.toFixed(d);
}
/** Success / availability rate: 2 decimals, exactly 100 → "100%", 99.999 → "99.99%". */
export function pctAvail(v) {
  if (absent(v)) return missing();
  if (v >= 100) return pctV('100', v, { exact: `${num(v, 3)}%` });
  return pctV(capBelow100(v, 2), v, { exact: `${num(v, 3)}%` });
}
/** Packet loss: 1 decimal, exactly 0 → "0%", non-zero below 0.05 → "<0.1%". */
export function pctLoss(v) {
  if (absent(v)) return missing();
  if (v === 0) return pctV('0', 0, { kind: 'zero' });
  if (v > 0 && v < 0.05) return pctV('0.1', v, { prefix: '<', exact: `${num(v, 3)}%` });
  return pctV(num(v, 1), v);
}
/** Bandwidth utilisation: integer, non-zero below 1 → "<1%". */
export function pctUtil(v) {
  if (absent(v)) return missing();
  if (v === 0) return pctV('0', 0, { kind: 'zero' });
  if (v > 0 && v < 1) return pctV('1', v, { prefix: '<', exact: `${num(v, 2)}%` });
  return pctV(num(v, 0), v, { exact: `${num(v, 2)}%` });
}
/** "↓42% ↑3%" */
export function utilPair(down, up) {
  const d = pctUtil(down), u = pctUtil(up);
  const t = (a, f) => (f.kind === 'missing' ? `${a}${DASH}` : `${a}${f.text}`);
  return { down: d, up: u, text: `${t('↓', d)} ${t('↑', u)}`, toString() { return this.text; } };
}
/** Sampling coverage, integer, never rounds up to 100: "覆盖 96%". */
export function coverage(v) {
  if (absent(v)) return missing();
  const n = v >= 100 ? '100' : capBelow100(v, 0);
  return pctV(n, v, { prefix: '覆盖 ', exact: `采样覆盖 ${num(v, 2)}%` });
}
/** Signed change, 1 decimal: "+3.2%", "−1.0%". */
export function pctDelta(v) {
  if (absent(v)) return missing();
  const r = round(v, 1);
  const sign = r > 0 ? '+' : r < 0 ? MINUS : '';
  return pctV(Math.abs(r).toFixed(1), v, { prefix: sign, tone: null });
}

/** Meter tone for micro-bars (§5.4 #10): ≥ limit → bad, ≥ 0.9 × limit → warn, else null. */
export function meterTone(value, limit) {
  if (absent(value) || absent(limit) || limit <= 0) return null;
  if (value >= limit) return 'bad';
  if (value >= limit * 0.9) return 'warn';
  return null;
}

// ─── latency (§4.5) ──────────────────────────────────────────────────────────

export const LATENCY_DEFAULTS = { tcpOk: 100, tcpWarn: 300, egress: 3000 };

/**
 * latency(42) → "42 ms"; 1240 → "1.24 s"; 0.4 → "<1 ms".
 * opts.failed → "失败", opts.timeout → "超时", null → "—".
 */
export function latency(ms, { failed = false, timeout = false } = {}) {
  if (failed || ms === 'fail') return new Fmt('失败', { kind: 'error', tone: 'bad', raw: ms });
  if (timeout || ms === 'timeout') return new Fmt('超时', { kind: 'error', tone: 'bad', raw: ms });
  if (absent(ms)) return missing();
  const exact = `${num(ms, ms % 1 ? 1 : 0)} ms`;
  if (ms < 1) return V('1', 'ms', { prefix: '<', raw: ms, exact });
  const r = Math.round(ms);
  if (r < 1000) return V(String(r), 'ms', { raw: ms, exact });
  return V(round(ms / 1000, 2).toFixed(2), 's', { raw: ms, exact });
}
/**
 * Tone from thresholds sent by the server (`node.limits`), defaults from §4.5.
 * kind 'tcp': ≤100 ok, ≤300 warn, >300 bad.  kind 'egress': ≤ limits.latency_ms ok, above → warn.
 * Returns 'ok' | 'warn' | 'bad' | null (no sample).
 */
export function latencyTone(ms, kind = 'tcp', limits = {}) {
  if (ms === 'fail' || ms === 'timeout') return 'bad';
  if (absent(ms)) return null;
  if (kind === 'egress') return ms <= (limits.latency_ms ?? limits.egress ?? LATENCY_DEFAULTS.egress) ? 'ok' : 'warn';
  if (ms <= (limits.tcp_ok_ms ?? LATENCY_DEFAULTS.tcpOk)) return 'ok';
  if (ms <= (limits.tcp_warn_ms ?? LATENCY_DEFAULTS.tcpWarn)) return 'warn';
  return 'bad';
}

// ─── money (§4.6) ────────────────────────────────────────────────────────────

export const CYCLES = {
  monthly: { word: '月付', months: 1 },
  quarterly: { word: '季付', months: 3 },
  semiannual: { word: '半年付', months: 6 },
  yearly: { word: '年付', months: 12 },
  biennial: { word: '两年付', months: 24 },
  triennial: { word: '三年付', months: 36 },
  lifetime: { word: '长期', months: null },
};
const CYCLE_BY_WORD = Object.fromEntries(Object.entries(CYCLES).map(([k, v]) => [v.word, k]));
/** Normalise a cycle ('月付' or 'monthly') to the enum key. Unknown → null. */
export function cycleKey(c) { return c == null ? null : CYCLES[c] ? c : CYCLE_BY_WORD[c] ?? null; }
export function cycleWord(c) { const k = cycleKey(c); return k ? CYCLES[k].word : DASH; }
/** Convert a price for a cycle to monthly cents (rounded). Lifetime → null. */
export function monthlyCents(cents, cycle = 'monthly') {
  const k = cycleKey(cycle) ?? 'monthly';
  const m = CYCLES[k].months;
  return absent(cents) || !m ? null : Math.round(cents / m);
}
/** "28.88" → 2888 without float error. */
export function priceToCents(price) {
  if (absent(price) || price === '') return null;
  if (typeof price === 'number') return Math.round(price * 100);
  const m = String(price).trim().match(/^(-)?(\d+)(?:\.(\d{1,2})\d*)?$/);
  if (!m) return null;
  const c = Number(m[2]) * 100 + Number((m[3] ?? '0').padEnd(2, '0'));
  return m[1] ? -c : c;
}

/** money(128135) → "$1,281.35"; EUR → "EUR 4.00"; opts.perMonth appends "/月". */
export function money(cents, { currency = 'USD', perMonth = false } = {}) {
  if (absent(cents)) return missing();
  const n = num(Math.abs(cents) / 100, 2);
  const sign = cents < 0 ? MINUS : '';
  const cur = currency === 'USD' ? '$' : `${currency} `;
  return V(n, '', { prefix: sign + cur, suffix: perMonth ? '/月' : '', raw: cents, kind: cents === 0 ? 'zero' : 'value' });
}

/**
 * Monthly fee cell for an asset's `billing` object.
 * price 0 → "免费"; billing.pending → "待定" (warn); missing → "—";
 * non-monthly → converted monthly with tooltip "季付 $15.00"; lifetime → "长期".
 */
export function monthlyFee(billing, { perMonth = false } = {}) {
  if (!billing) return missing();
  if (billing.pending) return pending();
  const cents = billing.price_cents ?? priceToCents(billing.price);
  if (absent(cents)) return missing();
  const currency = billing.currency || 'USD';
  if (cents === 0) return new Fmt('免费', { kind: 'word', raw: 0 });
  const cyc = cycleKey(billing.cycle ?? billing.cycle_word) ?? 'monthly';
  const orig = `${CYCLES[cyc].word} ${money(cents, { currency }).text}`;
  if (cyc === 'lifetime') return new Fmt('长期', { kind: 'word', tone: 'secondary', exact: orig, raw: cents });
  const m = billing.monthly_cents ?? monthlyCents(cents, cyc);
  const f = money(m, { currency, perMonth });
  if (cyc !== 'monthly') f.exact = orig;
  return f;
}

/** Sum amounts per currency, never converting. Returns Fmt[] (USD first). */
export function moneyTotals(items) {
  const by = new Map();
  for (const it of items) {
    if (!it || absent(it.cents)) continue;
    const c = it.currency || 'USD';
    by.set(c, (by.get(c) ?? 0) + it.cents);
  }
  return [...by.entries()].sort((a, b) => (a[0] === 'USD' ? -1 : b[0] === 'USD' ? 1 : a[0] < b[0] ? -1 : 1))
    .map(([c, v]) => money(v, { currency: c }));
}

// ─── dates, due countdowns (§4.7) ────────────────────────────────────────────

const p2 = (n) => (n < 10 ? '0' : '') + n;
/** Two-digit zero pad: pad2(7) → "07". */
export const pad2 = (n) => (absent(n) ? DASH : String(n).padStart(2, '0'));

/** Beijing-time calendar parts of a Unix-seconds timestamp. */
export function bjParts(t) {
  const d = new Date((t + BJ_OFFSET) * 1000);
  return { y: d.getUTCFullYear(), mo: d.getUTCMonth() + 1, d: d.getUTCDate(), h: d.getUTCHours(), mi: d.getUTCMinutes(), s: d.getUTCSeconds(), dow: d.getUTCDay() };
}
/** "2026-10-13" (Beijing calendar date). A 'YYYY-MM-DD' string passes through. */
export function date(t) {
  if (typeof t === 'string') return new Fmt(t, { raw: t });
  if (absent(t)) return missing();
  const p = bjParts(t);
  return new Fmt(`${p.y}-${p2(p.mo)}-${p2(p.d)}`, { raw: t, exact: absoluteText(t) });
}
/** Table timestamp: same year "09-23 14:05", other year "2025-12-31 23:59"; {seconds:true} adds ":09". */
export function stamp(t, { seconds = false, now: n = clock() } = {}) {
  if (absent(t)) return missing();
  const p = bjParts(t);
  const hm = `${p2(p.h)}:${p2(p.mi)}${seconds ? ':' + p2(p.s) : ''}`;
  const sameYear = bjParts(n).y === p.y;
  return new Fmt(`${sameYear ? '' : p.y + '-'}${p2(p.mo)}-${p2(p.d)} ${hm}`, { raw: t, exact: absoluteText(t) });
}
/** "14:05" or "14:05:09" Beijing clock time (axes, compact logs). */
export function clockTime(t, { seconds = false } = {}) {
  if (absent(t)) return DASH;
  const p = bjParts(t);
  return `${p2(p.h)}:${p2(p.mi)}${seconds ? ':' + p2(p.s) : ''}`;
}
function absoluteText(t) {
  const p = bjParts(t);
  return `${p.y}-${p2(p.mo)}-${p2(p.d)} ${p2(p.h)}:${p2(p.mi)}:${p2(p.s)} ${TZ_LABEL}`;
}
/** Tooltip form: "2026-09-23 14:05:09 北京时间". */
export function absolute(t) { return absent(t) ? DASH : absoluteText(t); }

const dtfCache = new Map();
/** Calendar date 'YYYY-MM-DD' of instant `t` in IANA zone `tz` (fast path for Shanghai). */
export function ymdIn(t, tz = TZ) {
  if (!tz || tz === TZ || tz === 'Asia/Chongqing' || tz === 'PRC') {
    const p = bjParts(t);
    return `${p.y}-${p2(p.mo)}-${p2(p.d)}`;
  }
  let f = dtfCache.get(tz);
  if (!f) {
    f = new Intl.DateTimeFormat('en-US', { timeZone: tz, year: 'numeric', month: '2-digit', day: '2-digit' });
    dtfCache.set(tz, f);
  }
  const parts = Object.fromEntries(f.formatToParts(new Date(t * 1000)).map((x) => [x.type, x.value]));
  return `${parts.year}-${parts.month}-${parts.day}`;
}
function dayNumber(ymd) {
  const [y, m, d] = ymd.split('-').map(Number);
  return Math.round(Date.UTC(y, m - 1, d) / 86400000);
}
/** Whole calendar days from `a` to `b` ('YYYY-MM-DD'). */
export function dayDiff(a, b) { return dayNumber(b) - dayNumber(a); }

const TZ_BADGES = {
  'America/Los_Angeles': 'LA', 'America/New_York': 'NY', 'America/Chicago': 'CHI', 'America/Denver': 'DEN',
  'America/Phoenix': 'PHX', 'America/Mexico_City': 'MEX', 'Europe/London': 'LON', 'Europe/Berlin': 'BER',
  'Europe/Amsterdam': 'AMS', 'Europe/Paris': 'PAR', 'Asia/Tokyo': 'TYO', 'Asia/Singapore': 'SG',
  'Asia/Hong_Kong': 'HK', 'Asia/Taipei': 'TPE', UTC: 'UTC', 'Etc/UTC': 'UTC',
};
/** Small badge shown after a billing date when its zone isn't Shanghai: 'LA'. Shanghai → null. */
export function tzBadge(tz) {
  if (!tz || tz === TZ || tz === 'Asia/Chongqing' || tz === 'PRC') return null;
  return TZ_BADGES[tz] ?? tz.split('/').pop().replace(/_/g, ' ').slice(0, 3).toUpperCase();
}

/** Days until `dueDate` counted in the billing time zone's calendar at instant `t`. */
export function dueDays(dueDate, tz = TZ, t = clock()) {
  if (!dueDate) return null;
  return dayDiff(ymdIn(t, tz), String(dueDate).slice(0, 10));
}

/** Tier for a remaining-days count: unset | expired | today | soon | normal | long */
export function dueTier(days, cycle) {
  if (cycleKey(cycle) === 'lifetime') return 'long';
  if (absent(days)) return 'unset';
  if (days >= 36500) return 'long';
  if (days < 0) return 'expired';
  if (days === 0) return 'today';
  if (days <= 7) return 'soon';
  return 'normal';
}
/** "7 天内到期" = remaining ≤ 7, including expired (§4.7). */
export function isDue7(days, cycle) { const t = dueTier(days, cycle); return t === 'expired' || t === 'today' || t === 'soon'; }

const TIER_TONE = { expired: 'bad', today: 'bad', soon: 'warn', normal: null, long: 'secondary', unset: 'muted' };
/** Countdown cell: "已过期 3 天" / "今天到期" / "3 天" / "26 天" / "长期" / "—". fmt.tier carries the tier. */
export function countdown(days, { cycle } = {}) {
  const tier = dueTier(days, cycle);
  const extra = { tier };
  if (tier === 'unset') return new Fmt(DASH, { kind: 'missing', tone: 'muted', extra });
  if (tier === 'long') return new Fmt('长期', { kind: 'word', tone: TIER_TONE.long, raw: days, extra });
  if (tier === 'expired') return V(String(-days), '天', { prefix: '已过期 ', tone: 'bad', raw: days, extra });
  if (tier === 'today') return new Fmt('今天到期', { kind: 'word', tone: 'bad', raw: 0, extra });
  return V(String(days), '天', { tone: TIER_TONE[tier], raw: days, extra });
}
/** Countdown for a `billing` object, recomputed at instant `t`; exact = due date + zone. */
export function due(billing, t = clock()) {
  if (!billing || !billing.due_date) return countdown(null);
  const tz = billing.billing_timezone || TZ;
  const days = dueDays(billing.due_date, tz, t);
  const f = countdown(days, { cycle: billing.cycle });
  const badge = tzBadge(tz);
  f.exact = `到期 ${billing.due_date}${badge ? `（账期时区 ${badge} · ${tz}）` : `（${TZ_LABEL}）`}`;
  f.badge = badge;
  f.days = days;
  return f;
}
/** Due date with its zone badge: text "2026-10-01", fmt.badge "LA". */
export function dueDate(billing) {
  if (!billing || !billing.due_date) return missing();
  const tz = billing.billing_timezone || TZ;
  const badge = tzBadge(tz);
  return new Fmt(billing.due_date, { raw: billing.due_date, exact: badge ? `账期时区 ${tz}` : null, extra: { badge } });
}

// ─── relative time, durations, freshness (§4.8) ──────────────────────────────

function agoPhrase(age) {
  if (age < 10) return '刚刚';
  if (age < 60) return `${Math.floor(age)} 秒前`;
  if (age < 3600) return `${Math.floor(age / 60)} 分钟前`;
  if (age < 86400) return `${Math.floor(age / 3600)} 小时前`;
  return `${Math.floor(age / 86400)} 天前`;
}
function aheadPhrase(s) {
  if (s < 60) return `${Math.max(1, Math.floor(s))} 秒后`;
  if (s < 3600) return `${Math.floor(s / 60)} 分钟后`;
  if (s < 86400) return `${Math.floor(s / 3600)} 小时后`;
  return `${Math.floor(s / 86400)} 天后`;
}
/**
 * Past: "刚刚" (<10 s) · "N 秒前" · "N 分钟前" · "N 小时前" · "N 天前" (<7 天) · else table stamp.
 * Future (maintenance deadlines etc.): "N 分钟后" / "N 小时后". Tooltip is the absolute Beijing time.
 */
export function ago(t, { now: n = clock() } = {}) {
  if (absent(t)) return missing();
  const age = n - t;
  const exact = absoluteText(t);
  if (age < -9) return new Fmt(aheadPhrase(-age), { raw: t, exact, extra: { future: true } });
  if (age >= 7 * 86400) return stamp(t, { now: n });
  return new Fmt(agoPhrase(Math.max(0, age)), { raw: t, exact });
}
export const relative = ago;
/** Shared timer cadence (§4.8): values younger than 60 s refresh every second, others every 30 s. */
export function refreshEvery(t, n = clock()) { return absent(t) || n - t >= 60 ? 30000 : 1000; }

const DUR_UNITS = [[86400, '天', '天'], [3600, '小时', '小时'], [60, '分', '分钟'], [1, '秒', '秒']];
/**
 * Largest two units: "48 秒", "3 分 12 秒", "2 小时 5 分", "3 天 4 小时".
 * {units:1} → the largest unit only ("12 分钟"), used in compact status lines.
 */
export function duration(s, { units = 2 } = {}) {
  if (absent(s) || s < 0) return missing();
  const total = s;
  let rest = Math.floor(s);
  if (rest === 0) return new Fmt('0 秒', { kind: 'zero', raw: total });
  const out = [];
  for (let i = 0; i < DUR_UNITS.length && out.length < units; i++) {
    const [size] = DUR_UNITS[i];
    const q = Math.floor(rest / size);
    if (q > 0 || out.length) { out.push([q, i]); rest -= q * size; }
  }
  const parts = out.filter(([q], k) => k === 0 || q > 0);
  const text = parts.length === 1
    ? `${parts[0][0]} ${DUR_UNITS[parts[0][1]][2]}`
    : parts.map(([q, i]) => `${q} ${DUR_UNITS[i][1]}`).join(' ');
  return new Fmt(text, { raw: total });
}
/** "持续运行 12 天" (under a day: "持续运行 6 小时 7 分"). */
export function uptime(s) {
  if (absent(s)) return missing();
  const body = s >= 86400 ? `${Math.floor(s / 86400)} 天` : duration(s).text;
  return new Fmt(`持续运行 ${body}`, { raw: s, exact: duration(s, { units: 4 }).text });
}

/** Expected update intervals in seconds (§4.8). */
export const INTERVALS = { komari: 2, xray: 10, monitor: 10, panel: 15, collector: 10 };
/**
 * Freshness of a value `age` seconds old, given its expected interval (seconds or an INTERVALS key).
 * ≤2× fresh · 2–5× delayed "延迟" (warn) · >5× stale "数据过期" (muted, hollow dot).
 */
export function freshness(age, interval = 'komari') {
  const i = typeof interval === 'number' ? interval : INTERVALS[interval] ?? 10;
  if (absent(age)) return { state: 'stale', word: '数据过期', tone: 'muted', ratio: Infinity, hollow: true };
  const ratio = age / i;
  if (ratio <= 2) return { state: 'fresh', word: '实时', tone: null, ratio, hollow: false };
  if (ratio <= 5) return { state: 'delayed', word: '延迟', tone: 'warn', ratio, hollow: false };
  return { state: 'stale', word: '数据过期', tone: 'muted', ratio, hollow: true };
}
/** Top-bar pill: "实时 · 2 秒前" / "延迟 · 25 秒前" (warn) / "已断开" (bad). */
export function freshnessPill(age, interval = 'komari') {
  const f = freshness(age, interval);
  const secs = absent(age) ? null : age < 60 ? `${Math.max(0, Math.round(age))} 秒前` : agoPhrase(age);
  if (f.state === 'stale') return new Fmt('已断开', { kind: 'word', tone: 'bad', extra: { state: 'disconnected' }, exact: secs ? `最后更新 ${secs}` : null });
  return new Fmt(`${f.word} · ${secs}`, { kind: 'word', tone: f.tone, extra: { state: f.state } });
}

// ─── status words (§4.10) ────────────────────────────────────────────────────

export const HEALTH = {
  ok: { word: '正常', tone: 'ok', dot: 'solid', rank: 4 },
  degraded: { word: '降级', tone: 'warn', dot: 'solid', rank: 1 },
  fail: { word: '故障', tone: 'bad', dot: 'solid', rank: 0 },
  maintenance: { word: '维护中', tone: 'info', dot: 'solid', rank: 3 },
  unknown: { word: '未知', tone: 'unknown', dot: 'ring', rank: 2 },
  na: { word: '不适用', tone: 'muted', dot: 'none', rank: 5 },
};
const HEALTH_ALIASES = { normal: 'ok', warn: 'degraded', warning: 'degraded', failed: 'fail', critical: 'fail', maint: 'maintenance' };
const HEALTH_BY_WORD = Object.fromEntries(Object.entries(HEALTH).map(([k, v]) => [v.word, k]));
/** Normalise a health code or word to ok|degraded|fail|maintenance|unknown|na. */
export function healthCode(c) { return c == null ? 'unknown' : HEALTH[c] ? c : HEALTH_ALIASES[c] ?? HEALTH_BY_WORD[c] ?? 'unknown'; }
/** health('degraded') → { code, word:'降级', tone:'warn', dot:'solid', rank }. {stale:true} → hollow dot + modifier 数据过期. */
export function health(code, { stale: isStale = false } = {}) {
  const c = healthCode(code);
  const h = HEALTH[c];
  return { code: c, word: h.word, tone: h.tone, dot: isStale && h.dot !== 'none' ? 'ring' : h.dot, rank: h.rank, modifier: isStale ? '数据过期' : null, toString() { return this.word; } };
}

export const LIFECYCLE = { onboarding: '上架中', unbound: '尚未接入', active: '在用', retiring: '下线中', retired: '已下线' };
export function lifecycle(c) { return { code: c, word: LIFECYCLE[c] ?? DASH, tone: c === 'active' ? null : c === 'retired' ? 'muted' : 'info' }; }

export const STEP = {
  queued: { word: '排队', tone: 'muted', glyph: '○' },
  running: { word: '进行中', tone: 'info', glyph: '◐' },
  succeeded: { word: '成功', tone: 'ok', glyph: '✓' },
  failed: { word: '失败', tone: 'bad', glyph: '✕' },
  rolled_back: { word: '已回滚', tone: 'warn', glyph: '↺' },
  skipped: { word: '跳过', tone: 'muted', glyph: '–' },
  cancelled: { word: '已取消', tone: 'muted', glyph: '⊘' },
  blocked: { word: '已停止（风险）', tone: 'bad', glyph: '■' },
  waiting_input: { word: '等待输入', tone: 'warn', glyph: '…' },
  waiting_gate: { word: '等待闸门', tone: 'info', glyph: '⋯' },
};
/** Pipeline step status → { code, word, tone, glyph } (glyphs are fallbacks; views use lucide icons). */
export function step(c) { const s = STEP[c] ?? { word: c ?? DASH, tone: null, glyph: '?' }; return { code: c, ...s }; }

export const RELAY_PARTS = {
  agent: ['在线', '离线', 'agent'],
  xray: ['运行中', '已停止', 'Xray'],
  panel: ['可达', '不可达', '面板'],
  collector: ['正常', '中断', '采集'],
  config: ['已核对', '待核对', '配置'],
};
/** relayPart('xray', false) → { word:'已停止', label:'Xray', tone:'bad' } */
export function relayPart(part, ok) {
  const p = RELAY_PARTS[part];
  if (!p) return { word: DASH, label: part, tone: 'muted' };
  if (ok == null) return { word: DASH, label: p[2], tone: 'muted' };
  return { word: ok ? p[0] : p[1], label: p[2], tone: ok ? 'ok' : part === 'config' ? 'warn' : 'bad' };
}

export const ENTRY = {
  enable: ['启用', '停用'],
  activity: ['有活动', '空闲'],
  binding: ['已绑定', '未绑定'],
};
/** entryWord('enable', false) → '停用' */
export function entryWord(kind, yes) { const e = ENTRY[kind]; return e ? e[yes ? 0 : 1] : DASH; }

export const ACCESS = { routed: '已转发', blocked: '已阻止' };
export function accessAction(c) { return { code: c, word: ACCESS[c] ?? DASH, tone: c === 'blocked' ? 'bad' : null }; }

export const INCIDENT = { critical: '故障', warning: '提醒', resolved: '已恢复', ack: '已知悉' };
/** Incident → { word, tone, acked }: open critical 故障 (bad), open warning 提醒 (warn), closed 已恢复 (ok). */
export function incident(inc) {
  if (!inc) return { word: DASH, tone: 'muted', acked: false };
  const acked = !!inc.ack_at;
  if (inc.open === false || inc.resolved_at) return { word: INCIDENT.resolved, tone: 'ok', acked };
  const crit = inc.severity === 'critical' || inc.status === 'fail';
  return { word: crit ? INCIDENT.critical : INCIDENT.warning, tone: crit ? 'bad' : 'warn', acked };
}

export const API_KEY = {
  state: { active: ['可用', 'ok'], expired: ['已过期', 'muted'], revoked: ['已撤销', 'muted'] },
  scope: { read: ['只读', 'info'], write: ['读写', 'warn'], full: ['完整权限', 'bad'] },
};
export function apiKey(kind, c) { const v = API_KEY[kind]?.[c]; return v ? { word: v[0], tone: v[1] } : { word: DASH, tone: 'muted' }; }

/** Reason codes (dict.reason in the snapshot). */
export const REASONS = {
  auth: '入口认证失败', egress_mismatch: '出口变更', policy_blocked: '规则阻止', probe_offline: '探针离线',
  retrying: '出口重试中', stale: '数据尚未更新', timeout: '探测超时', unstable: '链路不稳定', upstream_down: '上游不通',
};
/** Check keys (dict.check). */
export const CHECKS = {
  collector: '线路管理通道', cpu: 'CPU 容量', disk: '磁盘容量', egress: '出口', entry: '业务入口',
  host: '主机探针', ingress: '实际入口到出口', latency: '出口请求耗时', mapping: '固定出口绑定', memory: '内存容量', tcp: '线路到上游 TCP',
};
export const CAPABILITY = { host: '主机', proxy: '家宽代理', relay: '线路机', probe: '连通性探针', no_host: '无主机' };

/** Load dictionaries shipped in the snapshot (`dict`) over the built-in defaults. */
export function useDict(dict = {}) {
  if (dict.reason) Object.assign(REASONS, dict.reason);
  if (dict.check) Object.assign(CHECKS, dict.check);
  if (dict.capability) Object.assign(CAPABILITY, dict.capability);
  if (dict.lifecycle) Object.assign(LIFECYCLE, dict.lifecycle);
  if (dict.access_action) Object.assign(ACCESS, dict.access_action);
}
export function reasonWord(c) { return c ? REASONS[c] ?? c : null; }
export function checkTitle(k) { return k ? CHECKS[k] ?? k : null; }

/**
 * Second line of the status cell (§3.4): abnormal → "出口不符 · 12 分钟" (reason or first failed check +
 * how long); reason 'retrying' → "重试中"; normal → last check "38 秒前".
 */
export function statusLine(status, t = clock()) {
  if (!status) return missing();
  const c = healthCode(status.code);
  if (status.reason === 'retrying') return new Fmt('重试中', { kind: 'word', tone: 'info' });
  if (c === 'ok' || c === 'na') {
    const f = ago(status.checked_at, { now: t });
    return f.kind === 'missing' ? f : new Fmt(f.text, { raw: status.checked_at, exact: `最近检查 ${f.exact}` });
  }
  const what = status.reason_word || reasonWord(status.reason) || checkTitle(status.first_failed_check) || HEALTH[c].word;
  const d = isNum(status.since) ? duration(t - status.since, { units: 1 }).text : null;
  return new Fmt(d ? `${what} · ${d}` : what, { kind: 'word', tone: HEALTH[c].tone, exact: isNum(status.since) ? `自 ${absoluteText(status.since)}` : null });
}

// ─── names and identifiers (§4.11) ───────────────────────────────────────────

/** "团队1-07" */
export function shortName(team, number) { return `${team}-${pad2(number)}`; }
/** "线路机2 :20015"; master entry → "线路机2 主节点". */
export function entryLabel(relay, port, { master = false } = {}) {
  if (!relay) return DASH;
  return master ? `${relay} 主节点` : `${relay}:${port}`; // 原版写法：线路机2:20002（名称与端口之间不空格）
}
/** IP with IPv6 middle-truncated past `max` chars; exact = full address. */
export function ip(addr, { max = 22 } = {}) {
  if (!addr) return missing();
  if (addr.length <= max) return new Fmt(addr, { raw: addr, extra: { v: addr.includes(':') ? 6 : 4 } });
  // Cut on group boundaries: keep the network head and the interface tail.
  const g = addr.split(':');
  let h = 1, t = 1;
  const len = () => g.slice(0, h).join(':').length + 1 + g.slice(g.length - t).join(':').length;
  while (h + t < g.length && len() < max) { if (h <= t) h++; else t++; }
  if (len() > max && h > 1) h--;
  const short = g.slice(0, h).join(':') + '…' + g.slice(g.length - t).join(':');
  return new Fmt(short, { raw: addr, exact: addr, extra: { v: 6, truncated: true } });
}
/** UUID / token prefix: first 8 + "…" (exact = full value, for copy). */
export function shortId(id, n = 8) {
  if (!id) return missing();
  return new Fmt(id.length > n ? id.slice(0, n) + '…' : id, { raw: id, exact: id });
}

// ─── glossary (§4.12) ────────────────────────────────────────────────────────

export const GLOSSARY = [
  { term: '下行 ↓', meaning: '业务流量中流向用户客户端的方向（客户端下载）', avoid: ['下载', '入站', '上游'] },
  { term: '上行 ↑', meaning: '业务流量中来自用户客户端的方向（客户端上传）', avoid: ['上传', '出站'] },
  { term: '接收 / 发送', meaning: '主机网卡计数 RX / TX，和业务方向没有固定对应关系', avoid: ['上行', '下行', '入站', '出站'], scope: 'nic' },
  { term: '业务流量', meaning: 'Xray 入口计数，按入口去重；含监控探测流量', avoid: ['流量'] },
  { term: '网卡流量', meaning: '主机 agent 上报的网卡计数；线路机和落地机会重复计入同一份业务', avoid: ['主机流量', '主机传输速度'] },
  { term: '节点', meaning: '一项资产：落地机、代理产品或线路机', avoid: ['服务器', '主机', '实例'] },
  { term: '线路机', meaning: 'relay', avoid: ['线路', '主节点'] },
  { term: '入口', meaning: '线路机上的一个 VLESS 入站；端口是它的属性', avoid: ['连接入口'] },
  { term: '连接凭据', meaning: 'VLESS client UUID', avoid: ['UUID'] },
  { term: '正常 / 降级 / 故障', meaning: '监控综合判定', avoid: ['可用', '不稳定', '异常', '不可用'] },
  { term: '需处理', meaning: '未恢复故障，加上持续降级 ≥ 5 分钟（服务端 monitor.attention）', avoid: ['需要关注', 'attention'] },
  { term: '在线', meaning: 'agent 与服务端保持连接，不用于业务可用性', avoid: [] },
  { term: '已阻止', meaning: '访问规则或 Xray 内置规则丢弃的连接', avoid: ['已拦截'] },
];
const BANNED = ['已拦截', '拦截', '需要关注', '连接入口', '主机流量', '主机传输速度', '未填写', '未填', 'IP 未填写', '见节点详情', '状态待更新', '等待主机采样', '∞', '不稳定', '不可用'];
/** Dev lint: returns banned UI words found in `text` (brief §4.9 deleted phrasings, §4.12 "不再使用"). */
export function lintTerms(text) { return BANNED.filter((w) => String(text).includes(w)); }

// ─── tiny render helpers (strings only) ──────────────────────────────────────

const ESC = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };
export function esc(s) { return String(s ?? '').replace(/[&<>"']/g, (c) => ESC[c]); }
/**
 * Fmt → HTML string with the number and unit in separate spans so the unit can be set
 * smaller and secondary (§4.1). Class names are configurable; no inline styles.
 */
export function html(f, { numClass = 'num', unitClass = 'unit' } = {}) {
  if (!(f instanceof Fmt)) return esc(f);
  if (!f.unit && !f.prefix && !f.suffix) return esc(f.text);
  return `${esc(f.prefix)}<span class="${numClass}">${esc(f.num)}</span>${f.unit ? `${f.sep}<span class="${unitClass}">${esc(f.unit)}</span>` : ''}${f.suffix ? `<span class="${unitClass}">${esc(f.suffix)}</span>` : ''}`;
}
/** Tone → CSS custom property reference, e.g. toneVar('warn') → 'var(--warn-text)'. */
export function toneVar(tone, part = 'text') {
  if (!tone) return 'var(--text-1)';
  if (tone === 'muted') return 'var(--text-3)';
  if (tone === 'secondary') return 'var(--text-2)';
  if (tone === 'unknown') return part === 'dot' ? 'var(--unknown-ring)' : 'var(--text-3)';
  return `var(--${tone}-${part})`;
}
