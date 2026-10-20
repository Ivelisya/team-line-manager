// keys.js — shortcut registry (⌘K, /, j/k, Esc, g n …) and a headless command palette.
// The registry takes plain KeyboardEvent-like objects, so it runs (and is tested) without a DOM.

import { safeStorage } from './table.js';

const isMacLike = () => {
  try { return /Mac|iPhone|iPad/.test(globalThis.navigator?.platform ?? globalThis.navigator?.userAgent ?? ''); } catch { return false; }
};

const KEY_ALIASES = { esc: 'escape', return: 'enter', space: ' ', spacebar: ' ', up: 'arrowup', down: 'arrowdown', left: 'arrowleft', right: 'arrowright', del: 'delete' };

/** Parse 'mod+k', 'shift+?', 'g n', 'Escape' into steps [{ key, mod, ctrl, meta, alt, shift }]. */
export function parseCombo(combo, { mac = isMacLike() } = {}) {
  return String(combo).trim().split(/\s+/).map((chord) => {
    const parts = chord.split('+');
    // allow '+' itself as a key: 'shift++'
    const keyPart = chord.endsWith('++') ? '+' : parts.pop();
    const mods = new Set(parts.map((m) => m.toLowerCase()));
    let key = keyPart.toLowerCase();
    key = KEY_ALIASES[key] ?? key;
    const s = { key, ctrl: mods.has('ctrl'), meta: mods.has('meta') || mods.has('cmd'), alt: mods.has('alt') || mods.has('option'), shift: mods.has('shift') };
    if (mods.has('mod')) { if (mac) s.meta = true; else s.ctrl = true; }
    return s;
  });
}

// Printable non-letter keys already encode shift ('?' is shift+/), so shift is ignored for them.
function eventStep(e) {
  let key = String(e.key ?? '').toLowerCase();
  key = KEY_ALIASES[key] ?? key;
  const printableSymbol = key.length === 1 && !/[a-z0-9]/.test(key);
  return { key, ctrl: !!e.ctrlKey, meta: !!e.metaKey, alt: !!e.altKey, shift: printableSymbol ? false : !!e.shiftKey };
}
const sameStep = (a, b) => a.key === b.key && a.ctrl === b.ctrl && a.meta === b.meta && a.alt === b.alt && a.shift === b.shift;

/** Display form: 'mod+k' → ['⌘','K'] on mac, ['Ctrl','K'] elsewhere; 'g n' → ['G','N'] with then:true. */
export function comboKeys(combo, { mac = isMacLike() } = {}) {
  const NAMES = { escape: 'Esc', enter: '↵', arrowup: '↑', arrowdown: '↓', arrowleft: '←', arrowright: '→', ' ': 'Space', home: 'Home', end: 'End', tab: 'Tab', backspace: '⌫', delete: 'Del' };
  return parseCombo(combo, { mac }).map((s) => {
    const k = [];
    if (s.ctrl) k.push(mac ? '⌃' : 'Ctrl');
    if (s.alt) k.push(mac ? '⌥' : 'Alt');
    if (s.shift) k.push(mac ? '⇧' : 'Shift');
    if (s.meta) k.push(mac ? '⌘' : 'Win');
    k.push(NAMES[s.key] ?? (s.key.length === 1 ? s.key.toUpperCase() : s.key));
    return k;
  });
}
/** Human label: 'mod+k' → '⌘K' / 'Ctrl K'; 'g n' → 'G 然后 N'. */
export function comboLabel(combo, opts = {}) {
  const mac = opts.mac ?? isMacLike();
  return comboKeys(combo, { mac }).map((ks) => ks.join(mac ? '' : ' ')).join(' 然后 ');
}

function isEditable(el) {
  if (!el || typeof el !== 'object') return false;
  const tag = String(el.tagName ?? '').toLowerCase();
  if (tag === 'textarea' || tag === 'select') return true;
  if (tag === 'input') {
    const t = String(el.type ?? 'text').toLowerCase();
    return !['checkbox', 'radio', 'button', 'submit', 'reset', 'range', 'color', 'file'].includes(t);
  }
  return !!el.isContentEditable;
}

/**
 * Shortcut registry.
 *   const keys = createKeys();
 *   keys.bind('mod+k', openPalette, { desc: '命令面板', group: '全局' });
 *   keys.bind('g n', () => go('/nodes'), { desc: '去节点', group: '跳转' });
 *   const pop = keys.pushScope('palette', { exclusive: true }); … pop();
 *   keys.start(document);
 *
 * Rules: ignored during IME composition (Chinese input); in text fields only bindings with
 * allowInInput:true fire, plus Escape (unless a binding sets allowInInput:false); scopes are a
 * stack — the top scope is tried first, an exclusive scope blocks everything below it except
 * 'global' bindings that use a modifier (⌘K) or set allowInInput.
 */
export function createKeys({ sequenceTimeout = 1000, mac = isMacLike(), now = () => Date.now() } = {}) {
  const bindings = [];
  const scopes = [{ name: 'global', exclusive: false }];
  let pending = [], pendingAt = 0, target = null, listener = null;
  const changeListeners = new Set();

  function activeScopes() {
    const out = [];
    for (let i = scopes.length - 1; i >= 0; i--) {
      out.push(scopes[i].name);
      if (scopes[i].exclusive) break;
    }
    if (!out.includes('global')) out.push('global!'); // mod-combos only
    return out;
  }
  // Returns true/false when a binding ran (or a sequence started), null when nothing matched.
  function tryMatch(e, seq, step, inInput) {
    const hasMod = step.ctrl || step.meta || step.alt;
    let prefixMatch = false;
    for (const scopeName of activeScopes()) {
      const modOnly = scopeName === 'global!';
      const sName = modOnly ? 'global' : scopeName;
      for (let bi = bindings.length - 1; bi >= 0; bi--) {
        const b = bindings[bi];
        if (b.scope !== sName) continue;
        if (b.when && !b.when()) continue;
        if (modOnly && !b.allowInInput && !hasMod) continue;
        // text fields: only bindings that opt in, plus Escape unless a binding opts out
        if (inInput && !(b.allowInInput === true || (step.key === 'escape' && b.allowInInput !== false))) continue;
        for (const steps of b.steps) {
          if (steps.length < seq.length) continue;
          let ok = true;
          for (let i = 0; i < seq.length; i++) if (!sameStep(steps[i], seq[i])) { ok = false; break; }
          if (!ok) continue;
          if (modOnly && steps.length !== 1) continue;
          if (steps.length === seq.length) {
            pending = [];
            emitPending();
            if (b.preventDefault) e.preventDefault?.();
            return b.run(e) !== false;
          }
          prefixMatch = true;
        }
      }
    }
    if (prefixMatch && !inInput) {
      pending = seq; pendingAt = now(); emitPending();
      e.preventDefault?.();
      return true;
    }
    return null;
  }
  const emitPending = () => { for (const fn of changeListeners) fn({ pending: pendingLabel() }); };
  function pendingLabel() { return pending.length ? pending.map((s) => s.key.toUpperCase()).join(' ') : null; }

  const api = {
    mac,
    /**
     * Bind one or more combos. opts: { desc, group, scope='global', when()→bool, allowInInput,
     * preventDefault=true, hidden }. Returns an unbind function.
     */
    bind(combo, run, opts = {}) {
      const combos = Array.isArray(combo) ? combo : [combo];
      const b = { combos, steps: combos.map((c) => parseCombo(c, { mac })), run, scope: 'global', preventDefault: true, ...opts };
      bindings.push(b);
      return () => { const i = bindings.indexOf(b); if (i >= 0) bindings.splice(i, 1); };
    },
    /** Bind a list of { combo, run, ...opts } (e.g. table.keyBindings()). Returns one unbind. */
    bindAll(list) { const offs = list.map((b) => api.bind(b.combo, b.run, b)); return () => offs.forEach((f) => f()); },
    /** Push a scope (e.g. 'palette', 'drawer', 'dialog'); returns a pop function. */
    pushScope(name, { exclusive = false } = {}) {
      const s = { name, exclusive };
      scopes.push(s);
      return () => { const i = scopes.lastIndexOf(s); if (i > 0) scopes.splice(i, 1); };
    },
    scopes: () => scopes.map((s) => s.name),
    /** Handle a keydown; returns true when a binding ran. Safe to call with a plain object. */
    handle(e) {
      if (e.isComposing || e.keyCode === 229) return false;
      if (['shift', 'control', 'alt', 'meta'].includes(String(e.key).toLowerCase())) return false;
      const step = eventStep(e);
      const inInput = isEditable(e.target);
      if (pending.length && now() - pendingAt > sequenceTimeout) pending = [];
      const r = tryMatch(e, [...pending, step], step, inInput);
      if (r !== null) return r;
      if (pending.length) {
        // an abandoned sequence ('g' then 'j'): drop it and try the key on its own
        pending = []; emitPending();
        const r2 = tryMatch(e, [step], step, inInput);
        if (r2 !== null) return r2;
      }
      return false;
    },
    /** Pending sequence label for a hint ("G …"), or null. */
    pending: pendingLabel,
    onPending(fn) { changeListeners.add(fn); return () => changeListeners.delete(fn); },
    /** Attach to a target (default document). */
    start(t = globalThis.document) {
      if (!t || listener) return api;
      target = t;
      listener = (e) => api.handle(e);
      target.addEventListener('keydown', listener);
      return api;
    },
    stop() { if (target && listener) target.removeEventListener('keydown', listener); target = null; listener = null; },
    /** For the '?' help overlay: [{ group, items:[{ combos, labels, keys, desc }] }]. */
    list() {
      const groups = new Map();
      for (const b of bindings) {
        if (b.hidden || !b.desc) continue;
        const g = b.group ?? '其他';
        if (!groups.has(g)) groups.set(g, []);
        groups.get(g).push({ combos: b.combos, labels: b.combos.map((c) => comboLabel(c, { mac })), keys: b.combos.map((c) => comboKeys(c, { mac })), desc: b.desc, scope: b.scope });
      }
      return [...groups].map(([group, items]) => ({ group, items }));
    },
  };
  return api;
}

/** Standard app shortcuts (brief §2.5). `actions` supplies the handlers; missing ones are skipped. */
export function defaultBindings(actions = {}) {
  const def = [
    ['mod+k', 'palette', '命令面板', '全局', { allowInInput: true }],
    ['/', 'focusSearch', '聚焦搜索', '全局'],
    ['?', 'help', '快捷键帮助', '全局'],
    ['g o', 'goOverview', '去总览', '跳转'],
    ['g n', 'goNodes', '去节点', '跳转'],
    ['g r', 'goRelays', '去线路机', '跳转'],
    ['g t', 'goTraffic', '去流量', '跳转'],
    ['g b', 'goBehavior', '去节点行为', '跳转'],
    ['g p', 'goOnboarding', '去上架流水线', '跳转'],
    ['g a', 'goAudit', '去审计日志', '跳转'],
    ['g s', 'goSettings', '去设置', '跳转'],
    ['[', 'prev', '上一条（抽屉）', '抽屉'],
    [']', 'next', '下一条（抽屉）', '抽屉'],
    ['mod+shift+l', 'toggleTheme', '切换浅色 / 深色', '外观'],
    ['mod+shift+d', 'cycleDensity', '切换密度', '外观'],
  ];
  return def.filter(([, k]) => typeof actions[k] === 'function').map(([combo, k, desc, group, extra]) => ({ combo, run: actions[k], desc, group, ...(extra ?? {}) }));
}

// ─── fuzzy matching ──────────────────────────────────────────────────────────

const BOUNDARY = /[\s\-_.:/·|()（）]/;
/**
 * Score `query` against `text`. Returns { score, ranges:[[start,end)…] } or null.
 * exact 1000 · prefix 800 · word-boundary substring 600 · substring 400 · subsequence ≤ 300.
 */
export function fuzzy(query, text) {
  const q = String(query ?? '').trim().toLowerCase();
  const t = String(text ?? '').toLowerCase();
  if (!q) return { score: 1, ranges: [] };
  if (!t) return null;
  if (t === q) return { score: 1000, ranges: [[0, q.length]] };
  if (t.startsWith(q)) return { score: 800 - Math.min(99, t.length - q.length), ranges: [[0, q.length]] };
  let i = t.indexOf(q);
  while (i > 0 && !BOUNDARY.test(t[i - 1])) i = t.indexOf(q, i + 1);
  if (i > 0) return { score: 600 - Math.min(99, i), ranges: [[i, i + q.length]] };
  const j = t.indexOf(q);
  if (j >= 0) return { score: 400 - Math.min(99, j), ranges: [[j, j + q.length]] };
  // subsequence with bonuses for consecutive runs and boundaries — only for queries with some
  // letters/CJK and ≥ 3 chars; numbers and IP fragments must match as substrings ("21" ≠ ":2001")
  if (q.replace(/\s/g, '').length < 3 || /^[\d.:\s-]+$/.test(q)) return null;
  let score = 0, ti = 0, run = 0;
  const ranges = [];
  for (let qi = 0; qi < q.length; qi++) {
    const c = q[qi];
    if (c === ' ') continue;
    const k = t.indexOf(c, ti);
    if (k < 0) return null;
    if (k === ti && qi > 0) { run++; score += 8 * run; ranges[ranges.length - 1][1] = k + 1; } else { run = 0; ranges.push([k, k + 1]); score -= Math.min(10, k - ti); }
    if (k === 0 || BOUNDARY.test(t[k - 1])) score += 12;
    ti = k + 1;
  }
  return { score: Math.max(1, Math.min(300, 150 + score)), ranges };
}

/**
 * Headless command palette (⌘K). Items: { id, group, title, subtitle?, keywords?:string[],
 * status?, hint?, run(), danger?:bool, confirm?:string }. Groups render in `groups` order.
 *   const p = createPalette({ items: () => [...paletteItems(store, open), ...actions] });
 *   p.open(); p.setQuery('21'); p.results(); p.move(1); p.run();
 * A `danger` item needs a second run() (returns { needsConfirm:true } first) — §5.4 #20.
 */
export function createPalette({ items, groups = ['跳转', '节点', '线路机', '操作'], limitPerGroup = 8, recentKey = 'teamops:palette:recent', storage } = {}) {
  const store = safeStorage(storage);
  const source = typeof items === 'function' ? items : () => items ?? [];
  const listeners = new Set();
  let q = '', active = 0, isOpen = false, armed = null, cache = null;
  const emit = (reason) => { for (const fn of listeners) fn({ reason }); };

  function search() {
    if (cache && cache.q === q) return cache.flat;
    const all = source();
    const recent = store.get(recentKey, []);
    const scored = [];
    for (const it of all) {
      if (!q.trim()) {
        const r = recent.indexOf(it.id);
        if (r >= 0 || it.group === groups[0]) scored.push({ item: it, score: r >= 0 ? 2000 - r : 1, ranges: [], recent: r >= 0 });
        continue;
      }
      let best = null;
      const t = fuzzy(q, it.title);
      if (t) best = { score: t.score + 50, ranges: t.ranges, field: 'title' };
      for (const kw of it.keywords ?? []) {
        const m = fuzzy(q, kw);
        if (m && (!best || m.score > best.score)) best = { score: m.score, ranges: [], field: 'keyword', matched: kw };
      }
      if (it.subtitle) {
        const m = fuzzy(q, it.subtitle);
        if (m && m.score >= 400 && (!best || m.score - 100 > best.score)) best = { score: m.score - 100, ranges: [], field: 'subtitle', subtitleRanges: m.ranges };
      }
      if (best) scored.push({ item: it, ...best });
    }
    const byGroup = new Map();
    for (const s of scored.sort((a, b) => b.score - a.score)) {
      const g = !q.trim() && s.recent ? '最近' : s.item.group ?? '其他';
      if (!byGroup.has(g)) byGroup.set(g, []);
      if (byGroup.get(g).length < limitPerGroup) byGroup.get(g).push(s);
    }
    const order = ['最近', ...groups, ...[...byGroup.keys()].filter((g) => g !== '最近' && !groups.includes(g))];
    const out = [];
    for (const g of order) {
      const list = byGroup.get(g);
      if (!list?.length) continue;
      out.push({ group: g, items: list.map((s) => ({ ...s, index: 0 })) });
    }
    let i = 0;
    const flat = [];
    for (const g of out) for (const it of g.items) { it.index = i++; flat.push(it); }
    cache = { q, grouped: out, flat };
    return flat;
  }

  const api = {
    open() { isOpen = true; q = ''; active = 0; armed = null; cache = null; emit('open'); },
    close() { isOpen = false; armed = null; emit('close'); },
    get isOpen() { return isOpen; },
    setQuery(v) { q = String(v ?? ''); active = 0; armed = null; emit('query'); },
    query: () => q,
    /** Flat ranked results: [{ item, score, ranges, index, field }]. */
    results: () => search(),
    /** Grouped: [{ group, items:[…] }] in display order. */
    grouped: () => { search(); return cache.grouped; },
    active: () => active,
    setActive(i) { const n = search().length; active = n ? Math.max(0, Math.min(n - 1, i)) : 0; armed = null; emit('active'); },
    move(delta) { const n = search().length; if (!n) return; active = (active + delta + n) % n; armed = null; emit('active'); },
    /** Run the active (or given) item. Danger items need a second call; returns { ran, needsConfirm, item }. */
    run(i = active) {
      const r = search()[i];
      if (!r) return { ran: false };
      const it = r.item;
      if (it.danger && armed !== it.id) { armed = it.id; emit('confirm'); return { ran: false, needsConfirm: true, item: it, message: it.confirm ?? `再按一次 ↵ 执行「${it.title}」` }; }
      armed = null;
      const recent = [it.id, ...store.get(recentKey, []).filter((x) => x !== it.id)].slice(0, 8);
      store.set(recentKey, recent);
      isOpen = false;
      emit('run');
      it.run?.();
      return { ran: true, item: it };
    },
    armed: () => armed,
    /** Palette key bindings for keys.js (scope 'palette', exclusive). */
    keyBindings({ scope = 'palette' } = {}) {
      return [
        { combo: ['ArrowDown', 'ctrl+n'], run: () => api.move(1), scope, allowInInput: true, hidden: true },
        { combo: ['ArrowUp', 'ctrl+p'], run: () => api.move(-1), scope, allowInInput: true, hidden: true },
        { combo: 'Enter', run: () => { api.run(); }, scope, allowInInput: true, hidden: true },
        { combo: 'Escape', run: () => api.close(), scope, allowInInput: true, hidden: true },
      ];
    },
    on(fn) { listeners.add(fn); return () => listeners.delete(fn); },
    invalidate() { cache = null; },
  };
  return api;
}
