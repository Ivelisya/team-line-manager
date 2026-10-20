// table.js — headless table state: sort, faceted filters, token search, segments, grouping,
// selection, keyboard cursor, URL state and localStorage preferences. No DOM.
//
// Brief §2.2: filters / sort / segment / query live in the URL (toParams / fromParams);
// personal preferences (density, columns, default view, grouping, collapsed groups) and saved
// views live in localStorage, every access wrapped in try/catch.

const collator = new Intl.Collator('zh-Hans-CN', { numeric: true, sensitivity: 'base' });

/** localStorage wrapper that never throws (private mode, blocked storage, node). */
export function safeStorage(backing) {
  const ls = () => {
    if (backing) return backing;
    try { return globalThis.localStorage ?? null; } catch { return null; }
  };
  return {
    get(key, fallback = null) {
      try { const v = ls()?.getItem(key); return v == null ? fallback : JSON.parse(v); } catch { return fallback; }
    },
    set(key, value) {
      try { ls()?.setItem(key, JSON.stringify(value)); return true; } catch { return false; }
    },
    remove(key) { try { ls()?.removeItem(key); } catch { /* ignore */ } },
  };
}

/**
 * Parse a search string into tokens and free text.
 *   'team:团队1 relay:线路机2,线路机3 -status:ok "ATT 基础" 21'
 * → { tokens: [{key:'team', values:['团队1'], neg:false}, …], text: ['att 基础', '21'] }
 * Only keys listed in `tokenKeys` become tokens, so an IPv6 like 2001:db8::1 stays free text.
 */
export function parseQuery(q, tokenKeys = []) {
  const keys = new Set(tokenKeys);
  const out = { tokens: [], text: [] };
  const re = /(-?)([^\s:"]+):("([^"]*)"|\S*)|"([^"]*)"|(\S+)/g;
  let m;
  while ((m = re.exec(String(q ?? '')))) {
    if (m[2] !== undefined && keys.has(m[2].toLowerCase())) {
      const rawVal = m[4] ?? m[3];
      const values = rawVal.split(',').map((v) => v.trim()).filter(Boolean);
      if (values.length) out.tokens.push({ key: m[2].toLowerCase(), values, neg: m[1] === '-', raw: m[0] });
      continue;
    }
    const word = (m[5] ?? m[6] ?? m[0]).trim().toLowerCase();
    if (word) out.text.push(word);
  }
  return out;
}

/** Row window for a virtual list: { start, end, padTop, padBottom }. */
export function virtualWindow({ count, rowHeight, scrollTop, viewport, overscan = 6 }) {
  const start = Math.max(0, Math.floor(scrollTop / rowHeight) - overscan);
  const end = Math.min(count, Math.ceil((scrollTop + viewport) / rowHeight) + overscan);
  return { start, end, padTop: start * rowHeight, padBottom: (count - end) * rowHeight };
}

/**
 * Create a table model.
 * cfg.rows            array or () => array (live source)
 * cfg.getId           row → id
 * cfg.columns         { key: { get(row), type:'number'|'string', defaultDir?, label? } }
 * cfg.facets          { key: { get(row) → value | value[], word?(v), label?, order?(v) } }
 * cfg.tokens          { key: { test(row, value) → bool, facet? } } (search `key:value`)
 * cfg.haystack        row → lowercase text for free-text search
 * cfg.segments        [{ key, label, test(row) }] (first one = default, usually 全部)
 * cfg.presets         [{ name, state }] built-in saved views
 * cfg.defaults        { sort:[{key,dir}], density, view, groupBy, hidden:[column keys] }
 * cfg.storageKey      localStorage namespace (null = no persistence)
 * cfg.segmentParam    URL param that carries the segment and the `status` facet (default 'status')
 * cfg.resortThrottleMs keep row order stable under live data for this long (brief: 5000 for rate sort)
 */
export function createTable(cfg) {
  const {
    getId = (r) => r.id, columns = {}, facets = {}, tokens = {}, haystack = () => '',
    segments = [], presets = [], defaults = {}, storageKey = null, segmentParam = 'status',
    resortThrottleMs = 0, clock = () => Date.now(), pageSize = 20,
  } = cfg;
  const source = typeof cfg.rows === 'function' ? cfg.rows : () => cfg.rows ?? [];
  const store = safeStorage(cfg.storage);
  const prefKey = storageKey ? `teamops:table:${storageKey}` : null;
  const viewsKey = storageKey ? `teamops:views:${storageKey}` : null;
  const tokenKeys = Object.keys(tokens);
  const defaultSegment = segments[0]?.key ?? null;
  const listeners = new Set();

  const st = {
    sort: clone(defaults.sort ?? []),
    filters: new Map(),
    q: '',
    parsed: { tokens: [], text: [] },
    segment: defaultSegment,
    groupBy: defaults.groupBy ?? null,
    collapsed: new Set(),
    density: defaults.density ?? 'standard',
    view: defaults.view ?? 'table',
    hidden: new Set(defaults.hidden ?? []),
    order: defaults.order ? [...defaults.order] : null,
  };
  const sel = new Set();
  let anchor = null, cursor = null, cursorIdx = -1;
  let stateVer = 0, dataVer = 0, batching = 0, pendingReasons = new Set();
  let cache = { stateVer: -1, dataVer: -1, rows: [], order: null, sortedAt: 0 };

  // preferences
  const prefs = prefKey ? store.get(prefKey, null) : null;
  if (prefs) {
    if (prefs.density) st.density = prefs.density;
    if (prefs.view) st.view = prefs.view;
    if ('groupBy' in prefs) st.groupBy = prefs.groupBy;
    if (Array.isArray(prefs.hidden)) st.hidden = new Set(prefs.hidden);
    if (Array.isArray(prefs.order)) st.order = prefs.order;
    if (Array.isArray(prefs.collapsed)) st.collapsed = new Set(prefs.collapsed);
  }
  function savePrefs() {
    if (!prefKey) return;
    store.set(prefKey, { density: st.density, view: st.view, groupBy: st.groupBy, hidden: [...st.hidden], order: st.order, collapsed: [...st.collapsed] });
  }

  function changed(reason, { state = true } = {}) {
    if (state) stateVer++;
    pendingReasons.add(reason);
    if (batching) return;
    const reasons = [...pendingReasons];
    pendingReasons = new Set();
    for (const fn of listeners) fn({ reason: reasons[0], reasons });
  }

  // ── filtering ──
  function matches(row, skipFacet = null, skipSegment = false) {
    if (!skipSegment && st.segment && st.segment !== defaultSegment) {
      const seg = segments.find((s) => s.key === st.segment);
      if (seg && !seg.test(row)) return false;
    }
    for (const [k, set] of st.filters) {
      if (k === skipFacet || !set.size || !facets[k]) continue;
      const v = facets[k].get(row);
      if (Array.isArray(v) ? !v.some((x) => set.has(String(x))) : !set.has(String(v))) return false;
    }
    for (const tok of st.parsed.tokens) {
      const def = tokens[tok.key];
      if (!def) continue;
      const hit = tok.values.some((v) => def.test(row, v));
      if (hit === tok.neg) return false;
    }
    if (st.parsed.text.length) {
      const h = haystack(row) ?? '';
      for (const w of st.parsed.text) if (!h.includes(w)) return false;
    }
    return true;
  }

  function compareRows(a, b) {
    for (let i = 0; i < a.k.length; i++) {
      const va = a.k[i], vb = b.k[i];
      if (va === vb) continue;
      if (va == null) return 1; // nulls last in either direction
      if (vb == null) return -1;
      const c = typeof va === 'string' || typeof vb === 'string' ? collator.compare(String(va), String(vb)) : va < vb ? -1 : 1;
      if (c) return st.sort[i].dir === 'desc' ? -c : c;
    }
    return a.i - b.i;
  }
  function sortRows(list, all) {
    if (!st.sort.length) return list;
    const idx = new Map(all.map((r, i) => [r, i]));
    const deco = list.map((r) => ({ r, i: idx.get(r), k: st.sort.map((s) => columns[s.key]?.get(r) ?? null) }));
    deco.sort(compareRows);
    return deco.map((d) => d.r);
  }

  function compute() {
    if (cache.stateVer === stateVer && cache.dataVer === dataVer) return cache.rows;
    const all = source();
    const filtered = all.filter((r) => matches(r));
    let rows;
    const t = clock();
    const onlyData = cache.stateVer === stateVer && cache.order;
    if (onlyData && resortThrottleMs > 0 && t - cache.sortedAt < resortThrottleMs) {
      // keep the previous order; rows that newly match go to the end in sort order
      const pos = cache.order;
      const known = filtered.filter((r) => pos.has(getId(r))).sort((a, b) => pos.get(getId(a)) - pos.get(getId(b)));
      const fresh = sortRows(filtered.filter((r) => !pos.has(getId(r))), all);
      rows = known.concat(fresh);
    } else {
      rows = sortRows(filtered, all);
      cache.sortedAt = t;
    }
    cache = { stateVer, dataVer, rows, order: new Map(rows.map((r, i) => [getId(r), i])), sortedAt: cache.sortedAt };
    if (cursor != null) {
      const i = cache.order.get(cursor);
      if (i != null) cursorIdx = i;
    }
    return rows;
  }

  const api = {
    // ── reading ──
    /** Filtered + sorted rows (memoised). */
    rows: compute,
    count: () => compute().length,
    total: () => source().length,
    indexOf: (id) => { compute(); return cache.order.get(id) ?? -1; },
    row: (id) => source().find((r) => getId(r) === id),
    /** Plain snapshot of the state (for debugging / saved views). */
    state: () => ({
      sort: clone(st.sort), filters: Object.fromEntries([...st.filters].map(([k, v]) => [k, [...v]])), q: st.q,
      segment: st.segment, groupBy: st.groupBy, density: st.density, view: st.view, hidden: [...st.hidden], order: st.order,
    }),

    // ── sorting ──
    /** Click on a header: same key toggles direction; `multi` (shift-click) adds/toggles a secondary key. */
    setSort(key, { multi = false, dir } = {}) {
      if (!columns[key]) return;
      const col = columns[key];
      const i = st.sort.findIndex((s) => s.key === key);
      if (multi) {
        if (i >= 0) st.sort[i].dir = dir ?? (st.sort[i].dir === 'asc' ? 'desc' : 'asc');
        else st.sort.push({ key, dir: dir ?? col.defaultDir ?? 'asc' });
      } else if (i === 0 && !dir) {
        st.sort = [{ key, dir: st.sort[0].dir === 'asc' ? 'desc' : 'asc' }];
      } else {
        st.sort = [{ key, dir: dir ?? col.defaultDir ?? 'asc' }];
      }
      changed('sort');
    },
    clearSort() { st.sort = clone(defaults.sort ?? []); changed('sort'); },
    /** { dir:'asc'|'desc', index, ariaSort } or null. */
    sortOf(key) {
      const i = st.sort.findIndex((s) => s.key === key);
      if (i < 0) return null;
      return { dir: st.sort[i].dir, index: i, ariaSort: i === 0 ? (st.sort[i].dir === 'asc' ? 'ascending' : 'descending') : 'other' };
    },

    // ── filters ──
    toggleFilter(key, value) {
      const v = String(value);
      const set = st.filters.get(key) ?? new Set();
      set.has(v) ? set.delete(v) : set.add(v);
      set.size ? st.filters.set(key, set) : st.filters.delete(key);
      changed('filter');
    },
    setFilter(key, values = []) {
      const set = new Set(values.map(String));
      set.size ? st.filters.set(key, set) : st.filters.delete(key);
      changed('filter');
    },
    clearFilters(key) {
      if (key) st.filters.delete(key); else st.filters.clear();
      changed('filter');
    },
    filterValues: (key) => [...(st.filters.get(key) ?? [])],
    isFiltered: () => st.filters.size > 0 || !!st.q.trim() || (st.segment && st.segment !== defaultSegment),
    /**
     * Facet popover content: [{ value, word, count, selected }] computed with every other filter
     * applied (standard faceted search), sorted by facet order, else count desc.
     */
    facetCounts(key) {
      const f = facets[key];
      if (!f) return [];
      const counts = new Map();
      for (const r of source()) {
        if (!matches(r, key)) continue;
        const v = f.get(r);
        for (const x of Array.isArray(v) ? v : [v]) {
          if (x == null) continue;
          counts.set(String(x), (counts.get(String(x)) ?? 0) + 1);
        }
      }
      for (const v of st.filters.get(key) ?? []) if (!counts.has(v)) counts.set(v, 0);
      const sel2 = st.filters.get(key) ?? new Set();
      return [...counts].map(([value, count]) => ({ value, word: f.word ? f.word(value) : value, count, selected: sel2.has(value) }))
        .sort((a, b) => (f.order ? f.order(a.value) - f.order(b.value) : b.count - a.count || collator.compare(a.value, b.value)));
    },

    // ── search ──
    setQuery(q) {
      st.q = String(q ?? '');
      st.parsed = parseQuery(st.q, tokenKeys);
      changed('query');
    },
    query: () => st.q,
    parsedQuery: () => st.parsed,
    /** Autocomplete for the token under the caret: `rel` → ['relay:'], `relay:线` → ['relay:线路机1', …]. */
    suggest(fragment) {
      const s = String(fragment ?? '').toLowerCase();
      const i = s.indexOf(':');
      if (i < 0) return tokenKeys.filter((k) => k.startsWith(s)).map((k) => `${k}:`);
      const key = s.slice(0, i), part = s.slice(i + 1);
      const def = tokens[key];
      if (!def) return [];
      const vals = def.values ?? (def.facet && facets[def.facet] ? [...new Set(source().flatMap((r) => [].concat(facets[def.facet].get(r) ?? [])))] : []);
      return vals.map(String).filter((v) => v.toLowerCase().startsWith(part)).sort(collator.compare).map((v) => `${key}:${v}`);
    },

    // ── segments ──
    setSegment(key) { st.segment = key ?? defaultSegment; changed('segment'); },
    segment: () => st.segment,
    /** [{ key, label, count, active }] — counts honour filters and search but not the segment itself. */
    segmentCounts() {
      const base = source().filter((r) => matches(r, null, true));
      return segments.map((s) => ({ key: s.key, label: s.label, count: base.filter(s.test).length, active: s.key === st.segment }));
    },

    /** Active filters as removable chips: segment, facet values and query tokens. */
    chips() {
      const out = [];
      if (st.segment && st.segment !== defaultSegment) {
        const s = segments.find((x) => x.key === st.segment);
        out.push({ kind: 'segment', key: st.segment, label: s?.label ?? st.segment, remove: () => api.setSegment(null) });
      }
      for (const [k, set] of st.filters) for (const v of set) {
        out.push({ kind: 'facet', key: k, value: v, label: `${facets[k]?.label ?? k}：${facets[k]?.word ? facets[k].word(v) : v}`, remove: () => api.toggleFilter(k, v) });
      }
      for (const tok of st.parsed.tokens) {
        out.push({ kind: 'token', key: tok.key, value: tok.values.join(','), label: tok.raw, remove: () => api.setQuery(st.q.replace(tok.raw, '').replace(/\s+/g, ' ').trim()) });
      }
      return out;
    },
    /** Reset segment, filters and search (keeps sort and preferences). */
    reset() { api.batch(() => { st.filters.clear(); api.setQuery(''); api.setSegment(null); }); },

    // ── grouping ──
    setGroupBy(key) { st.groupBy = key && facets[key] ? key : null; savePrefs(); changed('group'); },
    groupBy: () => st.groupBy,
    /** [{ key, word, rows, count, collapsed }] in facet order (or first appearance). */
    groups() {
      const rows = compute();
      if (!st.groupBy) return [{ key: null, word: null, rows, count: rows.length, collapsed: false }];
      const f = facets[st.groupBy];
      const map = new Map();
      for (const r of rows) {
        const v = f.get(r);
        const k = String(Array.isArray(v) ? v[0] : v ?? '—');
        if (!map.has(k)) map.set(k, []);
        map.get(k).push(r);
      }
      const keys = [...map.keys()];
      if (f.order) keys.sort((a, b) => f.order(a) - f.order(b)); else keys.sort(collator.compare);
      return keys.map((k) => ({ key: k, word: f.word ? f.word(k) : k, rows: map.get(k), count: map.get(k).length, collapsed: st.collapsed.has(k) }));
    },
    toggleGroup(key) { st.collapsed.has(key) ? st.collapsed.delete(key) : st.collapsed.add(key); savePrefs(); changed('group', { state: false }); },

    // ── display preferences ──
    setDensity(d) { st.density = d; savePrefs(); changed('prefs', { state: false }); },
    density: () => st.density,
    setView(v) { st.view = v; savePrefs(); changed('prefs', { state: false }); },
    view: () => st.view,
    /** Visible column keys in display order, from a list of all column keys. */
    visibleColumns(all) {
      const order = st.order ? [...st.order.filter((k) => all.includes(k)), ...all.filter((k) => !st.order.includes(k))] : all;
      return order.filter((k) => !st.hidden.has(k));
    },
    isColumnVisible: (key) => !st.hidden.has(key),
    toggleColumn(key, visible) {
      const show = visible ?? st.hidden.has(key);
      show ? st.hidden.delete(key) : st.hidden.add(key);
      savePrefs(); changed('prefs', { state: false });
    },
    moveColumn(key, toIndex, all) {
      const order = (st.order ?? all ?? []).filter((k) => k !== key);
      order.splice(Math.max(0, Math.min(toIndex, order.length)), 0, key);
      st.order = order;
      savePrefs(); changed('prefs', { state: false });
    },
    resetColumns() { st.hidden = new Set(defaults.hidden ?? []); st.order = defaults.order ? [...defaults.order] : null; savePrefs(); changed('prefs', { state: false }); },

    // ── selection ──
    isSelected: (id) => sel.has(id),
    selected: () => [...sel],
    selectedCount: () => sel.size,
    /** Toggle one row; {range:true} (shift) applies the anchor row's new state to the whole span. */
    toggleSelect(id, { range = false } = {}) {
      const rows = compute();
      if (range && anchor != null && cache.order.has(anchor) && cache.order.has(id)) {
        const a = cache.order.get(anchor), b = cache.order.get(id);
        const on = sel.has(anchor);
        for (let i = Math.min(a, b); i <= Math.max(a, b); i++) { const rid = getId(rows[i]); on ? sel.add(rid) : sel.delete(rid); }
      } else {
        sel.has(id) ? sel.delete(id) : sel.add(id);
        anchor = id;
      }
      changed('selection', { state: false });
    },
    selectAll() { for (const r of compute()) sel.add(getId(r)); changed('selection', { state: false }); },
    clearSelection() { if (!sel.size) return; sel.clear(); anchor = null; changed('selection', { state: false }); },
    /** Header checkbox state over the visible rows: 'none' | 'some' | 'all'. */
    selectionState() {
      const rows = compute();
      if (!rows.length) return 'none';
      let n = 0;
      for (const r of rows) if (sel.has(getId(r))) n++;
      return n === 0 ? 'none' : n === rows.length ? 'all' : 'some';
    },

    // ── keyboard cursor ──
    cursor: () => cursor,
    setCursor(id) { cursor = id; compute(); cursorIdx = cache.order.get(id) ?? -1; changed('cursor', { state: false }); return id; },
    /** Move the cursor by `delta` rows (j = +1, k = −1), clamped; returns the new id. */
    move(delta) {
      const rows = compute();
      if (!rows.length) return null;
      let i = cursor != null && cache.order.has(cursor) ? cache.order.get(cursor) : cursorIdx >= 0 ? Math.min(cursorIdx, rows.length - 1) - (delta > 0 ? 1 : 0) : delta > 0 ? -1 : rows.length;
      i = Math.max(0, Math.min(rows.length - 1, i + delta));
      return api.setCursor(getId(rows[i]));
    },
    moveTo(edge) { const rows = compute(); if (!rows.length) return null; return api.setCursor(getId(rows[edge === 'last' ? rows.length - 1 : 0])); },
    page(dir = 1) { return api.move(dir * pageSize); },
    /** 'x' on the cursor row (shift+x extends from the anchor). */
    toggleCursor({ range = false } = {}) { if (cursor != null) api.toggleSelect(cursor, { range }); },
    /**
     * Bindings for keys.js: j/k/↑/↓ move, x select, shift+x range, Enter open, Esc close/clear,
     * mod+a select all, Home/End. `open(id)` / `close()` come from the view.
     */
    keyBindings({ open = () => {}, close = () => false, scope = 'table' } = {}) {
      return [
        { combo: ['j', 'ArrowDown'], run: () => api.move(1), desc: '下一行', group: '表格', scope },
        { combo: ['k', 'ArrowUp'], run: () => api.move(-1), desc: '上一行', group: '表格', scope },
        { combo: 'x', run: () => api.toggleCursor(), desc: '选择 / 取消选择', group: '表格', scope },
        { combo: 'shift+x', run: () => api.toggleCursor({ range: true }), desc: '连续选择', group: '表格', scope },
        { combo: ['Enter', 'o'], run: () => cursor != null && open(cursor), desc: '打开', group: '表格', scope },
        { combo: 'Escape', run: () => close() || api.clearSelection(), desc: '关闭 / 取消选择', group: '表格', scope },
        { combo: 'mod+a', run: () => api.selectAll(), desc: '全选当前结果', group: '表格', scope },
        { combo: 'Home', run: () => api.moveTo('first'), desc: '第一行', group: '表格', scope },
        { combo: 'End', run: () => api.moveTo('last'), desc: '最后一行', group: '表格', scope },
      ];
    },

    // ── live data ──
    /** Tell the table its rows changed (live diff). Membership updates now; order respects the throttle. */
    invalidate() {
      dataVer++;
      if (sel.size) {
        const ids = new Set(source().map(getId));
        for (const id of [...sel]) if (!ids.has(id)) sel.delete(id);
      }
      changed('data', { state: false });
    },
    /** Force a full re-sort on the next read even inside the throttle window. */
    resort() { cache.sortedAt = 0; dataVer++; changed('data', { state: false }); },

    // ── URL state ──
    /** Serialise segment/filters/query/sort/view to URL params (defaults omitted). */
    toParams(params = new URLSearchParams()) {
      const statusVals = [];
      if (st.segment && st.segment !== defaultSegment) statusVals.push(st.segment);
      for (const [k, set] of st.filters) {
        if (k === segmentParam) statusVals.push(...set);
        else params.set(k, [...set].join(','));
      }
      if (statusVals.length) params.set(segmentParam, statusVals.join(','));
      if (st.q.trim()) params.set('q', st.q.trim());
      const s = sortParam(st.sort), d = sortParam(defaults.sort ?? []);
      if (s && s !== d) params.set('sort', s);
      if (defaults.view && st.view !== defaults.view) params.set('view', st.view);
      return params;
    },
    /** Restore from URL params (a URLSearchParams or a query string). Unknown keys are ignored. */
    fromParams(p) {
      const params = typeof p === 'string' ? new URLSearchParams(p) : p;
      api.batch(() => {
        st.filters.clear();
        st.segment = defaultSegment;
        for (const key of Object.keys(facets)) {
          const v = params.get(key);
          if (v == null || v === '') continue;
          const vals = v.split(',').filter(Boolean);
          if (key === segmentParam) {
            const segs = vals.filter((x) => segments.some((s) => s.key === x));
            if (segs.length) st.segment = segs[0];
            const rest = vals.filter((x) => !segments.some((s) => s.key === x));
            if (rest.length) st.filters.set(key, new Set(rest));
          } else st.filters.set(key, new Set(vals));
        }
        if (!facets[segmentParam] && params.get(segmentParam)) {
          const seg = params.get(segmentParam).split(',').find((x) => segments.some((s) => s.key === x));
          if (seg) st.segment = seg;
        }
        api.setQuery(params.get('q') ?? '');
        const sort = params.get('sort');
        st.sort = sort ? parseSort(sort, columns) : clone(defaults.sort ?? []);
        const view = params.get('view');
        if (view) st.view = view;
        changed('filter');
      });
    },

    // ── saved views ──
    /** Built-in presets followed by the user's saved views: [{ name, state, builtin }]. */
    views() {
      const user = viewsKey ? store.get(viewsKey, []) : [];
      return [...presets.map((p) => ({ ...p, builtin: true })), ...(Array.isArray(user) ? user : [])];
    },
    saveView(name) {
      if (!viewsKey) return false;
      const s = api.state();
      const view = { name, state: { segment: s.segment, filters: s.filters, q: s.q, sort: s.sort } };
      const user = store.get(viewsKey, []).filter((v) => v.name !== name);
      return store.set(viewsKey, [...user, view]);
    },
    deleteView(name) { if (viewsKey) store.set(viewsKey, store.get(viewsKey, []).filter((v) => v.name !== name)); },
    applyView(nameOrView) {
      const v = typeof nameOrView === 'string' ? api.views().find((x) => x.name === nameOrView) : nameOrView;
      if (!v) return false;
      const s = v.state ?? {};
      api.batch(() => {
        st.filters = new Map(Object.entries(s.filters ?? {}).map(([k, vals]) => [k, new Set(vals.map(String))]));
        st.segment = s.segment ?? defaultSegment;
        api.setQuery(s.q ?? '');
        if (s.sort) st.sort = clone(s.sort);
        changed('filter');
      });
      return true;
    },

    // ── events ──
    on(fn) { listeners.add(fn); return () => listeners.delete(fn); },
    /** Run several mutations and emit one change event. */
    batch(fn) {
      batching++;
      try { fn(); } finally { batching--; }
      if (!batching && pendingReasons.size) { const r = [...pendingReasons]; pendingReasons = new Set(); for (const l of listeners) l({ reason: r[0], reasons: r }); }
    },
  };
  return api;
}

function clone(x) { return JSON.parse(JSON.stringify(x)); }
function sortParam(sort) { return sort.map((s) => (s.dir === 'desc' ? '-' : '') + s.key).join(','); }
function parseSort(str, columns) {
  return str.split(',').filter(Boolean).map((t) => ({ key: t.replace(/^[-+]/, ''), dir: t.startsWith('-') ? 'desc' : 'asc' })).filter((s) => columns[s.key]);
}
