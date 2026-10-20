// data.js — loads the snapshot and exposes normalised selectors + derived fields.
// No DOM. Mutable store: live.js patches node objects in place and emits diffs through it.

import * as fmt from './format.js';

export const DATA_URL = new URL('../data/', import.meta.url);
export const LAZY = {
  detail: 'detail.json',
  extra: 'extra.json',
  traffic_5m: 'series/traffic-5m.json',
  traffic_1m: 'series/traffic-1m.json',
  host_24h: 'series/host-24h.json',
  latency_24h: 'series/latency-24h.json',
};
export const ATTENTION_DEGRADED_S = 300; // 持续降级 ≥ 5 分钟 counts as 需处理
const ACTIVE_BPS = 8192; // groups.active: ≥ 1 KiB/s of business traffic (same rule as tools/build.py)

/** Fetch data/snapshot.json and build a store. */
export async function loadSnapshot(opts = {}) {
  const base = opts.base ?? DATA_URL;
  const fetchImpl = opts.fetch ?? globalThis.fetch?.bind(globalThis);
  const raw = await fetchJSON(fetchImpl, new URL('snapshot.json', base));
  return createStore(raw, { ...opts, base, fetch: fetchImpl });
}

async function fetchJSON(fetchImpl, url) {
  if (!fetchImpl) throw new Error('no fetch available');
  const res = await fetchImpl(url);
  if (!res.ok) throw new Error(`读取失败 ${res.status} ${url}`);
  return res.json();
}

// ─── tiny utilities ──────────────────────────────────────────────────────────

/** Deterministic PRNG (mulberry32). */
export function rng(seed = 1) {
  let a = seed >>> 0;
  const r = () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  r.normal = () => { let u = 0; while (u === 0) u = r(); return Math.sqrt(-2 * Math.log(u)) * Math.cos(2 * Math.PI * r()); };
  r.pick = (arr) => arr[Math.floor(r() * arr.length)];
  return r;
}

/** Read a dotted path: get(node, 'rate.down_bps'). */
export function get(obj, path) {
  let o = obj;
  for (const k of path.split('.')) { if (o == null) return undefined; o = o[k]; }
  return o;
}
/** Write a dotted path, creating objects as needed; returns true if the value changed. */
export function set(obj, path, value) {
  const ks = path.split('.');
  let o = obj;
  for (let i = 0; i < ks.length - 1; i++) o = o[ks[i]] ??= {};
  const k = ks[ks.length - 1];
  if (o[k] === value) return false;
  o[k] = value;
  return true;
}

function ipNum(ip) {
  if (!ip || ip.includes(':')) return ip ? 2 ** 33 : null; // v6 after v4
  const p = ip.split('.').map(Number);
  return ((p[0] * 256 + p[1]) * 256 + p[2]) * 256 + p[3];
}
const sum = (arr, f) => arr.reduce((s, x) => s + (f(x) ?? 0), 0);
const clone = (o) => (typeof structuredClone === 'function' ? structuredClone(o) : JSON.parse(JSON.stringify(o)));

// ─── store ───────────────────────────────────────────────────────────────────

/**
 * Build a store from a parsed snapshot.
 * opts.base       base URL for lazy files (default ../data/)
 * opts.fetch      fetch implementation (default globalThis.fetch)
 * opts.bindClock  make format.js's clock follow the store's simulated clock (default true)
 * opts.stress     total node rows to synthesise (e.g. 200); extra rows are marked `stress: true`
 * opts.clone      deep-clone `raw` first (default false; the store mutates node objects)
 */
export function createStore(raw, opts = {}) {
  if (opts.clone) raw = clone(raw);
  const base = opts.base ?? DATA_URL;
  const fetchImpl = opts.fetch ?? globalThis.fetch?.bind(globalThis);
  const listeners = new Map();
  const lazy = new Map();
  let simNow = raw.as_of;
  let clockFn = null; // live.js installs a continuous simulated clock
  let version = 0;

  fmt.useDict(raw.dict);
  const teamOrder = new Map((raw.teams ?? []).map((t) => [t.name, t.order]));
  const relays = raw.relays ?? [];
  const relayByName = new Map(relays.map((r) => [r.name, r]));
  const relayById = new Map(relays.map((r) => [r.id, r]));

  let nodes = raw.nodes ?? [];
  if (opts.stress && opts.stress > nodes.length) nodes = nodes.concat(stressNodes(nodes, opts.stress, { seed: opts.seed ?? 7, asOf: raw.as_of }));
  nodes.forEach((n) => normalizeNode(n, teamOrder, relayByName));
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const byName = new Map(nodes.map((n) => [n.name, n]));
  // Relay rows and their node rows describe the same machine: share the live objects.
  for (const r of relays) {
    const n = byId.get(r.id);
    if (!n) continue;
    r.node = n;
    if (n.host && r.host) n.host = r.host;
    if (n.bandwidth && r.bandwidth) n.bandwidth = r.bandwidth;
    if (n.billing && r.billing) n.billing = r.billing;
  }
  const entries = raw.entries ?? [];
  const entryById = new Map(entries.map((e) => [e.id, e]));
  let incidents = (raw.incidents_recent ?? []).slice().sort((a, b) => b.opened_at - a.opened_at);
  let replayState = null;

  const store = {
    raw,
    get asOf() { return raw.as_of; },
    get version() { return version; },
    /** Simulated "now" (integer Unix s). Starts at the snapshot's as_of; live.js advances it. */
    now: () => (clockFn ? Math.floor(clockFn()) : simNow),
    /** Pin the clock to a fixed instant. */
    setNow(t) { clockFn = null; simNow = t; },
    /** Install a clock function (seconds, may be fractional); null restores the pinned clock. */
    setClock(fn) { if (!fn) simNow = store.now(); clockFn = fn; },
    bump() { return ++version; },

    // events
    on(type, fn) {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type).add(fn);
      return () => listeners.get(type)?.delete(fn);
    },
    emit(type, payload) {
      version++;
      for (const fn of listeners.get(type) ?? []) fn(payload, store);
    },

    // entities
    nodes: () => nodes,
    node: (id) => byId.get(id) ?? byName.get(id),
    relays: () => relays,
    relay: (idOrName) => relayById.get(idOrName) ?? relayByName.get(idOrName),
    entries: () => entries,
    entry: (id) => entryById.get(id),
    entriesFor: (relay) => { const r = store.relay(relay); return r ? entries.filter((e) => e.relay_id === r.id) : []; },
    /** The relay entry a node is reachable through ('线路机2:20015'). */
    entryOf: (n) => (n && n.relay ? entryById.get(`${n.relay}:${n.connection?.port ?? n.relay_port}`) ?? null : null),
    teams: () => raw.teams ?? [],
    providers: () => raw.providers ?? [],
    site: () => raw.site,
    system: () => raw.system,
    rankings: (kind) => (kind ? raw.rankings?.[kind] ?? [] : raw.rankings),
    access: (id) => raw.access_recent?.[id] ?? [],
    behavior: () => ({ kpis: raw.behavior_kpis, top: raw.behavior_top }),
    fleetTraffic: () => raw.traffic?.fleet_24h,
    upcomingDue: (limit = 10) => nodes.slice().filter((n) => n.billing?.due_date)
      .map((n) => ({ node: n, ...store.dueOf(n) }))
      .sort((a, b) => (a.days ?? 1e9) - (b.days ?? 1e9)).slice(0, limit),

    // derived per node
    /** true when the node counts toward 需处理: open failure, or degraded ≥ 5 min (or the server says so). */
    attentionOf(n, t = store.now()) {
      const s = n.status;
      if (!s) return false;
      if (s.attention) return true;
      if (s.code === 'fail') return true;
      return s.code === 'degraded' && s.since != null && t - s.since >= ATTENTION_DEGRADED_S;
    },
    /** { days, tier, due7, fmt } — days in the billing zone, recomputed at the simulated now. */
    dueOf(n, t = store.now()) {
      const b = n.billing;
      const days = b?.due_date ? fmt.dueDays(b.due_date, b.billing_timezone, t) : null;
      const tier = fmt.dueTier(days, b?.cycle);
      return { days, tier, due7: tier === 'expired' || tier === 'today' || tier === 'soon' };
    },
    /** true when the monitor evaluation is older than 5× its 10 s interval. */
    staleOf(n, t = store.now()) {
      const c = n.status?.checked_at;
      return c != null && fmt.freshness(t - c, 'monitor').state === 'stale';
    },
    derived(n, t = store.now()) {
      const due = store.dueOf(n, t);
      const stale = store.staleOf(n, t);
      return { attention: store.attentionOf(n, t), stale, health: fmt.health(n.status?.code, { stale }), dueDays: due.days, dueTier: due.tier, due7: due.due7 };
    },

    // fleet level
    /** Overview KPI strip, derived from current node state so it stays consistent with live updates. */
    kpis(t = store.now()) {
      const counts = { ok: 0, degraded: 0, fail: 0, maintenance: 0, unknown: 0, total: nodes.length };
      let attention = 0, sampled = 0, due7 = [], nearest = null;
      for (const n of nodes) {
        const c = fmt.healthCode(n.status?.code);
        if (c in counts) counts[c]++;
        if (store.attentionOf(n, t)) attention++;
        if (n.rate?.fresh !== false && n.rate?.down_bps != null) sampled++;
        const d = store.dueOf(n, t);
        if (d.due7) due7.push({ id: n.id, name: n.name, days: d.days, due_date: n.billing.due_date, tz_badge: fmt.tzBadge(n.billing.billing_timezone) });
        if (d.days != null && d.tier !== 'long' && (!nearest || d.days < nearest.days)) nearest = { id: n.id, name: n.name, days: d.days, due_date: n.billing.due_date };
      }
      due7.sort((a, b) => a.days - b.days);
      const today = { down: sum(nodes, (n) => n.traffic?.today?.down), up: sum(nodes, (n) => n.traffic?.today?.up) };
      const month = { down: sum(nodes, (n) => n.traffic?.month?.down), up: sum(nodes, (n) => n.traffic?.month?.up) };
      const k = raw.kpis ?? {};
      return {
        normal: counts,
        attention,
        business_rate: {
          down_bps: sum(nodes, (n) => n.rate?.down_bps), up_bps: sum(nodes, (n) => n.rate?.up_bps),
          sampled_entries: sampled, expected_entries: nodes.length, spark: k.business_rate?.spark ?? null,
        },
        today_business: { ...today, total: today.down + today.up, since: k.today_business?.since, since_text: k.today_business?.since_text ?? '北京时间 0 点起' },
        month_business: { ...month, total: month.down + month.up, since: k.month_business?.since, coverage_start: k.month_business?.coverage_start },
        due_7d: { count: due7.length, items: due7, nearest },
        monthly_cost: { cents: sum(nodes, (n) => n.billing?.monthly_cents), currency: 'USD', services: nodes.filter((n) => n.billing).length },
        events_today: k.events_today ?? null,
      };
    },
    /** Team summary rows (overview table), derived live. */
    groups(t = store.now()) {
      return store.teams().map(({ name }) => {
        const ns = nodes.filter((n) => n.team === name);
        const status = { ok: 0, degraded: 0, fail: 0, unknown: 0, maintenance: 0 };
        for (const n of ns) { const c = fmt.healthCode(n.status?.code); if (c in status) status[c]++; }
        const sampledNs = ns.filter((n) => n.rate?.fresh !== false && n.rate?.down_bps != null);
        const today = { down: sum(ns, (n) => n.traffic?.today?.down), up: sum(ns, (n) => n.traffic?.today?.up) };
        return {
          team: name, count: ns.length, status,
          attention: ns.filter((n) => store.attentionOf(n, t)).length,
          rate_down_bps: sum(ns, (n) => n.rate?.down_bps), rate_up_bps: sum(ns, (n) => n.rate?.up_bps),
          rate_sampled: sampledNs.length, rate_expected: ns.length, rate_complete: sampledNs.length === ns.length,
          active: sampledNs.filter((n) => n.rate.down_bps + n.rate.up_bps >= ACTIVE_BPS).length,
          today, today_bytes: today.down + today.up,
          month: { down: sum(ns, (n) => n.traffic?.month?.down), up: sum(ns, (n) => n.traffic?.month?.up) },
          monthly_cents: sum(ns, (n) => n.billing?.monthly_cents),
        };
      });
    },
    /** 需处理 list: [{ node, status, since, duration_s, check, check_title, reason_word }] fail first, then longest. */
    attention(t = store.now()) {
      return nodes.filter((n) => store.attentionOf(n, t)).map((n) => attentionItem(n, t))
        .sort((a, b) => a.rank - b.rank || a.since - b.since);
    },
    /** Nodes whose health is 未知 (the "未知" tab of the attention panel). */
    unknown(t = store.now()) { return nodes.filter((n) => fmt.healthCode(n.status?.code) === 'unknown').map((n) => attentionItem(n, t)); },

    // incidents
    incidents({ open, node, limit } = {}) {
      let list = incidents;
      if (open != null) list = list.filter((i) => !!i.open === open);
      if (node) list = list.filter((i) => i.node === node);
      return limit ? list.slice(0, limit) : list;
    },
    addIncident(inc) { incidents = [inc, ...incidents]; return inc; },
    get incidentsTotal() { return (raw.incidents_total ?? 0) + incidents.filter((i) => i.live).length; },

    // replays of real past moments (to demo a populated attention list)
    replays: () => raw.status_replays ?? [],
    get replay() { return replayState?.index ?? null; },
    /**
     * Re-enact status_replays[i] "as if now": affected nodes take the recorded status, reason and
     * failed check; `since` is shifted so the recorded duration is preserved. Emits 'change'.
     */
    applyReplay(i) {
      store.clearReplay({ silent: true });
      const rp = raw.status_replays?.[i];
      if (!rp) return null;
      const at = store.now();
      const shift = at - rp.at;
      const saved = [];
      const diff = { now: at, nodes: {}, events: [], replay: i };
      for (const r of rp.nodes) {
        const n = byId.get(r.node);
        if (!n) continue;
        saved.push([n, n.status]);
        const since = r.since + shift;
        n.status = {
          ...n.status, code: r.status, word: r.word, reason: r.reason, reason_word: r.reason_word ?? fmt.reasonWord(r.reason),
          since, first_failed_check: r.first_failed_check, attention: r.status === 'fail' || (r.status === 'degraded' && rp.at - r.since >= ATTENTION_DEGRADED_S),
          failed_checks: (r.checks ?? [r.first_failed_check]).map((k) => ({ key: k, title: fmt.checkTitle(k), status: r.status, reason: r.reason, since, value: k === r.first_failed_check ? r.value : null, evidence: k === r.first_failed_check ? r.evidence : null })),
          replayed: true,
        };
        diff.nodes[n.id] = { status: n.status };
        diff.events.push({ type: 'status', id: n.id, from: 'ok', to: r.status, replay: true });
        if (r.status !== 'unknown') {
          store.addIncident({
            id: `replay-${r.incident_ids?.[0] ?? n.id}`, node: n.id, name: n.name, check: r.first_failed_check, title: r.check_title,
            severity: r.status === 'fail' ? 'critical' : 'warning', status: r.status, status_word: r.word, reason: r.reason, reason_word: r.reason_word,
            opened_at: since, last_seen: at, resolved_at: null, open: true, evidence: r.evidence, replay: true,
          });
        }
      }
      replayState = { index: i, saved };
      store.emit('change', diff);
      return rp;
    },
    clearReplay({ silent = false } = {}) {
      if (!replayState) return;
      const diff = { now: store.now(), nodes: {}, events: [], replay: null };
      for (const [n, s] of replayState.saved) { n.status = s; diff.nodes[n.id] = { status: s }; }
      incidents = incidents.filter((x) => !x.replay);
      replayState = null;
      if (!silent) store.emit('change', diff);
    },

    // onboarding
    onboarding() {
      const ob = raw.onboarding ?? {};
      return { history: (ob.history ?? []).slice().sort((a, b) => b.started_at - a.started_at), batch: ob.demo_batch ? normalizeBatch(ob.demo_batch) : null, note: ob.note };
    },

    // lazy files
    /** Load a lazy file by key (see LAZY). Cached; failed loads are retried on the next call. */
    load(name) {
      if (!LAZY[name]) return Promise.reject(new Error(`unknown lazy file ${name}`));
      if (!lazy.has(name)) {
        const p = fetchJSON(fetchImpl, new URL(LAZY[name], base));
        p.catch(() => lazy.delete(name));
        lazy.set(name, p);
      }
      return lazy.get(name);
    },
    /** Fire-and-forget prefetch (e.g. after 150 ms hover on a link). */
    prefetch(name) { store.load(name).catch(() => {}); },
    /** Per-node detail (checks, 48 h availability, audit …) from detail.json. */
    async detail(id) { return (await store.load('detail')).nodes?.[id] ?? null; },
    /** extra.json: all entries, all 78 incidents, behaviour, probes, renewals, audit. */
    extra() { return store.load('extra'); },
    /**
     * Columnar series for one node: kind = traffic_5m | traffic_1m | host_24h | latency_24h.
     * Returns { t0, step, ...columns } (t0/step filled from the file header when absent), or null.
     */
    async series(id, kind) {
      const file = await store.load(kind);
      const s = file.nodes?.[id];
      if (!s) return null;
      return { t0: file.t0, step: file.step, unit: file.unit, ...s };
    },

    /** A new store with `n` node rows (originals + synthetic copies) for table / render stress tests. */
    stressed(n = 200, seed = 7) {
      const r2 = clone({
        ...raw,
        nodes: raw.nodes.filter((x) => !x.stress).map(stripNode),
        relays: raw.relays.map(({ node, ...rest }) => rest),
      });
      return createStore(r2, { ...opts, clone: false, stress: n, seed });
    },
  };

  function attentionItem(n, t) {
    const s = n.status ?? {};
    const h = fmt.health(s.code);
    return {
      node: n, id: n.id, name: n.name, status: h.code, word: h.word, tone: h.tone, rank: h.rank,
      since: s.since, duration_s: s.since != null ? Math.max(0, t - s.since) : null,
      check: s.first_failed_check, check_title: fmt.checkTitle(s.first_failed_check),
      reason: s.reason, reason_word: s.reason_word ?? fmt.reasonWord(s.reason),
      incident: incidents.find((i) => i.node === n.id && i.open) ?? null,
    };
  }

  if (opts.bindClock !== false) fmt.setClock(() => store.now());
  return store;
}

function stripNode(n) {
  const { teamOrder, relayId, hasHost, isProxy, isRelay, haystack, ipNum: _ip, caps, ...rest } = n;
  return rest;
}

function normalizeNode(n, teamOrder, relayByName) {
  const caps = n.capabilities ?? [];
  n.teamOrder = teamOrder.get(n.team) ?? 99;
  n.relayId = relayByName.get(n.relay)?.id ?? null;
  n.hasHost = caps.includes('host') && !!n.host;
  n.isProxy = caps.includes('proxy');
  n.isRelay = caps.includes('relay') || n.kind === 'relay';
  n.caps = n.hasHost ? caps : [...caps, 'no_host'];
  n.ipNum = ipNum(n.ip);
  n.haystack = [
    n.name, n.full_name, n.id, n.number != null ? fmt.pad2(n.number) : '', n.ip, n.ipv6, n.team, n.relay, n.entry_label,
    n.provider, n.provider_code, n.region?.code, n.region?.name, n.billing?.service_id, n.billing?.plan,
    n.connection?.client_label, ...(n.tags ?? []),
  ].filter(Boolean).join(' ').toLowerCase();
  return n;
}

// ─── onboarding batch ────────────────────────────────────────────────────────

const PHASE_PRIORITY = ['blocked', 'failed', 'waiting_input', 'running', 'waiting_gate', 'rolled_back', 'cancelled'];
/** Aggregate a set of step statuses into one cell status (worst/most active wins). */
export function aggregateSteps(statuses) {
  const list = statuses.filter(Boolean);
  if (!list.length) return null;
  for (const p of PHASE_PRIORITY) if (list.includes(p)) return p;
  if (list.every((s) => s === 'skipped')) return 'skipped';
  if (list.every((s) => s === 'succeeded' || s === 'skipped')) return 'succeeded';
  if (list.some((s) => s === 'succeeded')) return 'running';
  return 'queued';
}

export const ROW_STATE = {
  active: { word: '已交付', tone: 'ok' },
  in_relay_batch: { word: '线路机合批中', tone: 'info' },
  running: { word: '进行中', tone: 'info' },
  waiting_input: { word: '等待输入', tone: 'warn' },
  failed: { word: '失败', tone: 'bad' },
  blocked: { word: '已停止（风险）', tone: 'bad' },
  queued: { word: '排队', tone: 'muted' },
  cancelled: { word: '已取消', tone: 'muted' },
};

/** Normalise onboarding.demo_batch for the pipeline matrix. */
export function normalizeBatch(b) {
  const stepIndex = new Map(b.steps.map((s, i) => [s.code, i]));
  const groupCols = (cols) => cols.map((c) => ({ ...c, key: c.key ?? c.name }));
  const phases = groupCols(b.phases);
  const briefColumns = groupCols(b.brief_columns ?? []);
  const summary = new Map((b.column_summary ?? []).map((c) => [c.step, c]));
  const rows = b.rows.map((r) => {
    const cellMap = new Map(r.cells.map((c) => [c.step, c]));
    const statusOf = (codes) => aggregateSteps(codes.map((c) => cellMap.get(c)?.status));
    const durOf = (codes) => codes.reduce((s, c) => s + (cellMap.get(c)?.duration_s ?? 0), 0);
    const current = r.cells.find((c) => c.status !== 'succeeded' && c.status !== 'skipped' && c.status !== 'queued') ?? null;
    const applicable = r.cells.filter((c) => c.status !== 'skipped');
    const done = applicable.filter((c) => c.status === 'succeeded').length;
    const state = ROW_STATE[r.row_state] ?? { word: r.row_state, tone: null };
    return {
      ...r, id: r.name, cellMap, current,
      currentStep: current ? { ...b.steps[stepIndex.get(current.step)], ...current } : null,
      state: r.row_state, stateWord: state.word, tone: state.tone,
      attention: ['failed', 'blocked', 'waiting_input'].includes(r.row_state),
      progress: { done, applicable: applicable.length },
      phaseCells: phases.map((p) => ({ key: p.key, status: statusOf(p.steps), duration_s: durOf(p.steps) })),
      columnCells: briefColumns.map((p) => ({ key: p.key, status: statusOf(p.steps), duration_s: durOf(p.steps) })),
      elapsed_s: r.cells.reduce((s, c) => s + (c.duration_s ?? 0), 0),
    };
  });
  const colSummary = (cols) => cols.map((p) => {
    const st = rows.map((r) => r.columnCells.concat(r.phaseCells).find((c) => c.key === p.key)?.status);
    return { key: p.key, done: st.filter((s) => s === 'succeeded').length, applicable: st.filter((s) => s && s !== 'skipped').length };
  });
  return {
    ...b, rows, phases, briefColumns, stepIndex,
    stepSummary: b.steps.map((s) => ({ ...s, ...(summary.get(s.code) ?? {}) })),
    phaseSummary: colSummary(phases),
    columnSummary: colSummary(briefColumns),
  };
}

// ─── stress duplicator ───────────────────────────────────────────────────────

/**
 * Synthesise node rows until the fleet has `n` rows. Copies landing nodes with seeded jitter on
 * rates, traffic, host metrics and latency; fresh ids/names continuing each team's numbering;
 * IPs from 192.0.2.0/24 then 198.18.0.0/15 (documentation / benchmark ranges); due dates spread
 * from −3 to +35 days; a few rows degraded / failing / unknown so every state renders.
 */
export function stressNodes(base, n = 200, { seed = 7, asOf = base[0]?.status?.checked_at ?? 0, vary = true } = {}) {
  const r = rng(seed);
  const landing = base.filter((x) => x.kind !== 'relay' && !x.stress);
  const out = [];
  const nextNum = {};
  for (const x of base) nextNum[x.team] = Math.max(nextNum[x.team] ?? 0, (x.number ?? 0) + 1);
  const relayPorts = {};
  for (const x of base) if (x.relay && x.relay_port) relayPorts[x.relay] = Math.max(relayPorts[x.relay] ?? 20000, x.relay_port);
  const relaysNames = Object.keys(relayPorts).sort();
  const teams = [...new Set(landing.map((x) => x.team))];
  const REASONS = [['degraded', 'unstable', 'tcp'], ['degraded', 'timeout', 'egress'], ['degraded', 'retrying', 'egress'], ['fail', 'auth', 'ingress'], ['fail', 'upstream_down', 'tcp'], ['fail', 'egress_mismatch', 'egress']];
  let i = 0;
  while (base.length + out.length < n) {
    const src = landing[i % landing.length];
    const c = clone(stripNode(src));
    const team = teams.length > 1 && r() < 0.25 ? r.pick(teams) : src.team;
    const number = nextNum[team]++;
    const teamIdx = team.match(/\d+/)?.[0] ?? 'x';
    c.id = `t${teamIdx}-${fmt.pad2(number)}`;
    c.team = team;
    c.number = number;
    c.name = fmt.shortName(team, number);
    const k = out.length + 1;
    c.ip = k <= 254 ? `192.0.2.${k}` : `198.18.${Math.floor((k - 255) / 254)}.${((k - 255) % 254) + 1}`;
    c.ipv6 = null;
    c.full_name = `${c.name} · ${c.provider_code} · ${c.ip}`;
    const relay = r.pick(relaysNames);
    const port = ++relayPorts[relay];
    c.relay = relay; c.relay_port = port; c.entry_label = fmt.entryLabel(relay, port);
    if (c.connection) { c.connection.port = port; c.connection.inbound_tag = `inbound-${port}`; c.connection.client_label = `node-${fmt.pad2(number)}`; }
    if (c.outbound) c.outbound.target = c.ip;
    const f = Math.exp(r.normal() * 0.6);
    const mul = (o, keys) => { if (o) for (const key of keys) if (typeof o[key] === 'number') o[key] = Math.round(o[key] * f); };
    mul(c.rate, ['down_bps', 'up_bps']);
    for (const w of ['today', 'month', 'last24h', 'cumulative']) mul(c.traffic?.[w], ['down', 'up']);
    if (c.traffic?.hourly24) for (const d of ['down', 'up']) c.traffic.hourly24[d] = c.traffic.hourly24[d].map((v) => (v == null ? v : Math.round(v * f)));
    if (c.host) {
      c.host.cpu_pct = Math.min(100, Math.round(c.host.cpu_pct * f * 10 + r() * 30) / 10);
      c.host.mem_pct = Math.min(99, Math.round((c.host.mem_pct * (0.7 + r() * 0.8)) * 100) / 100);
      mul(c.host, ['net_rx_bps', 'net_tx_bps']);
    }
    if (c.latency) {
      const lf = 0.8 + r() * 0.5;
      for (const key of ['tcp_ms', 'egress_ms', 'path_ms']) if (typeof c.latency[key] === 'number') c.latency[key] = Math.round(c.latency[key] * lf);
    }
    if (c.billing) {
      const days = r() < 0.1 ? Math.floor(r() * 11) - 3 : 8 + Math.floor(r() * 38); // ~10% within 7 days
      c.billing.due_date = fmt.ymdIn(asOf + days * 86400, c.billing.billing_timezone);
      c.billing.due_days = days;
      c.billing.service_id = String(70000 + k);
    }
    if (vary && c.status) {
      const roll = r();
      if (roll < 0.1) {
        const [code, reason, check] = REASONS[Math.floor(r() * REASONS.length)];
        const since = asOf - Math.floor(30 + r() * 1800);
        Object.assign(c.status, { code, word: fmt.health(code).word, reason, reason_word: fmt.reasonWord(reason), since, first_failed_check: check,
          attention: code === 'fail' || asOf - since >= ATTENTION_DEGRADED_S,
          failed_checks: [{ key: check, title: fmt.checkTitle(check), status: code, reason, since }] });
      } else if (roll < 0.13) {
        Object.assign(c.status, { code: 'unknown', word: '未知', reason: 'stale', reason_word: fmt.reasonWord('stale'), since: asOf - 120, first_failed_check: 'host', attention: false });
      } else if (roll < 0.145) {
        Object.assign(c.status, { code: 'maintenance', word: '维护中', reason: null, reason_word: null, since: asOf - 900, maintenance: true, attention: false });
      }
    }
    c.stress = true;
    out.push(c);
    i++;
  }
  return out;
}

// ─── table configuration for the node list ───────────────────────────────────

const HEALTH_RANK = (n) => fmt.health(n.status?.code).rank;
const rateSum = (n) => (n.rate ? (n.rate.avg3_down_bps ?? n.rate.down_bps ?? 0) + (n.rate.avg3_up_bps ?? n.rate.up_bps ?? 0) : null);
const pairSum = (o) => (o ? (o.down ?? 0) + (o.up ?? 0) : null);

/**
 * Column accessors for the node table / card sort menu. `get` returns the sort value
 * (numbers or strings; null always sorts last). Status ascending = worst first.
 */
export function nodeColumns(store) {
  return {
    name: { label: '节点', type: 'number', get: (n) => n.teamOrder * 10000 + (n.number ?? 0) },
    number: { label: '编号', type: 'number', get: (n) => n.number },
    team: { label: '团队', type: 'string', get: (n) => n.team },
    relay: { label: '线路', type: 'string', get: (n) => (n.relay ? `${n.relay}:${String(n.relay_port ?? 0).padStart(5, '0')}` : null) },
    status: { label: '状态', type: 'number', get: (n) => HEALTH_RANK(n) * 1e10 + (store.attentionOf(n) ? 0 : 1e9) + (n.status?.since ?? 0) % 1e9 },
    latency: { label: '延迟 TCP', type: 'number', get: (n) => n.latency?.tcp_ms ?? null },
    egress: { label: '延迟 出口', type: 'number', get: (n) => n.latency?.egress_ms ?? null },
    avail30: { label: '近 30 天', type: 'number', get: (n) => n.availability?.windows?.['30']?.availability_percent ?? null },
    rate: { label: '业务速率', type: 'number', get: rateSum, defaultDir: 'desc' },
    rate_down: { label: '下行速率', type: 'number', get: (n) => n.rate?.down_bps ?? null, defaultDir: 'desc' },
    rate_up: { label: '上行速率', type: 'number', get: (n) => n.rate?.up_bps ?? null, defaultDir: 'desc' },
    today: { label: '今日', type: 'number', get: (n) => pairSum(n.traffic?.today), defaultDir: 'desc' },
    month: { label: '本月', type: 'number', get: (n) => pairSum(n.traffic?.month), defaultDir: 'desc' },
    cpu: { label: 'CPU', type: 'number', get: (n) => n.host?.cpu_pct ?? null, defaultDir: 'desc' },
    mem: { label: '内存', type: 'number', get: (n) => n.host?.mem_pct ?? null, defaultDir: 'desc' },
    disk: { label: '磁盘', type: 'number', get: (n) => n.host?.disk_pct ?? null, defaultDir: 'desc' },
    load: { label: '负载', type: 'number', get: (n) => n.host?.load5_per_core_pct ?? null, defaultDir: 'desc' },
    uptime: { label: '运行时长', type: 'number', get: (n) => n.host?.uptime_s ?? null },
    cost: { label: '月费', type: 'number', get: (n) => n.billing?.monthly_cents ?? null, defaultDir: 'desc' },
    due: { label: '到期', type: 'number', get: (n) => store.dueOf(n).days },
    provider: { label: '供应商', type: 'string', get: (n) => n.provider },
    region: { label: '地区', type: 'string', get: (n) => n.region?.code },
    plan: { label: '套餐', type: 'string', get: (n) => n.billing?.plan },
    service_id: { label: '服务号', type: 'string', get: (n) => n.billing?.service_id },
    ip: { label: 'IP', type: 'number', get: (n) => n.ipNum },
    bandwidth: { label: '带宽', type: 'number', get: (n) => n.bandwidth?.capacity_bps ?? null, defaultDir: 'desc' },
    connections: { label: '连接数', type: 'number', get: (n) => n.host?.tcp_conns ?? n.rate?.tcp_connections ?? null, defaultDir: 'desc' },
  };
}

/** Facets for "+ 筛选" (values may be arrays for multi-valued facets like capabilities). */
export function nodeFacets(store) {
  return {
    team: { label: '团队', get: (n) => n.team },
    relay: { label: '线路机', get: (n) => n.relay ?? '直连' },
    status: { label: '状态', get: (n) => fmt.healthCode(n.status?.code), word: (v) => fmt.health(v).word },
    region: { label: '地区', get: (n) => n.region?.code, word: (v) => store.nodes().find((n) => n.region?.code === v)?.region?.name ?? v },
    provider: { label: '供应商', get: (n) => n.provider },
    cap: { label: '能力', get: (n) => n.caps, word: (v) => fmt.CAPABILITY[v] ?? v },
    plan: { label: '套餐', get: (n) => n.billing?.plan },
    lifecycle: { label: '生命周期', get: (n) => n.lifecycle, word: (v) => fmt.LIFECYCLE[v] ?? v },
  };
}

const lc = (s) => String(s ?? '').toLowerCase();
/**
 * Search tokens (`team: relay: status: provider: region: cap:host|proxy ip: plan: due:<7d`).
 * Each test(row, value) is OR-ed across comma-separated values, AND-ed across tokens.
 */
export function nodeTokens(store) {
  const inc = (a, b) => lc(a).includes(lc(b));
  return {
    team: { facet: 'team', test: (n, v) => inc(n.team, v) || lc(n.team) === lc(`团队${v}`) },
    relay: { facet: 'relay', test: (n, v) => inc(n.relay, v) || lc(n.relay) === lc(`线路机${v}`) || (v === '直连' && !n.relay) },
    status: {
      facet: 'status',
      test: (n, v) => {
        const w = lc(v);
        if (w === 'attention' || w === '需处理') return store.attentionOf(n);
        if (w === 'stale' || w === '数据过期') return store.staleOf(n);
        return fmt.healthCode(n.status?.code) === fmt.healthCode(v);
      },
      values: ['ok', 'degraded', 'fail', 'maintenance', 'unknown', 'attention'],
    },
    provider: { facet: 'provider', test: (n, v) => inc(n.provider, v) || lc(n.provider_code) === lc(v) },
    region: { facet: 'region', test: (n, v) => lc(n.region?.code) === lc(v) || inc(n.region?.name, v) },
    cap: { facet: 'cap', test: (n, v) => n.caps.includes(lc(v)) || n.caps.some((c) => fmt.CAPABILITY[c] === v), values: ['host', 'proxy', 'relay', 'no_host'] },
    ip: {
      test: (n, v) => {
        const pat = lc(v);
        if (pat.includes('*')) { const re = new RegExp('^' + pat.replace(/[.+?^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*') + '$'); return re.test(lc(n.ip)) || re.test(lc(n.ipv6)); }
        return lc(n.ip).startsWith(pat) || lc(n.ipv6).startsWith(pat);
      },
    },
    plan: { facet: 'plan', test: (n, v) => inc(n.billing?.plan, v) },
    due: {
      // due:<7d / due:7 → 剩余 ≤ 7 天（含已过期，与“7 天内到期”同口径）; due:>30; due:expired
      test: (n, v) => {
        const d = store.dueOf(n);
        if (v === 'expired' || v === '已过期') return d.tier === 'expired';
        const m = String(v).match(/^(<=|<|>=|>|=)?\s*(-?\d+)\s*[d天]?$/);
        if (!m || d.days == null || d.tier === 'long') return false;
        const x = Number(m[2]);
        switch (m[1]) {
          case '>': return d.days > x;
          case '>=': return d.days >= x;
          case '=': return d.days === x;
          default: return d.days <= x;
        }
      },
    },
    lifecycle: { facet: 'lifecycle', test: (n, v) => n.lifecycle === v || fmt.LIFECYCLE[n.lifecycle] === v },
  };
}

/** Status segments shown before the search box: 全部 · 需处理 · 未知 · 7 天内到期 · 尚未接入. */
export function nodeSegments(store) {
  return [
    { key: 'all', label: '全部', test: () => true },
    { key: 'attention', label: '需处理', test: (n) => store.attentionOf(n) },
    { key: 'unknown', label: '未知', test: (n) => fmt.healthCode(n.status?.code) === 'unknown' },
    { key: 'due7', label: '7 天内到期', test: (n) => store.dueOf(n).due7 },
    { key: 'unbound', label: '尚未接入', test: (n) => n.lifecycle === 'unbound' },
  ];
}

/** Built-in saved views (sidebar under 节点). */
export const NODE_PRESETS = [
  { name: '需处理', state: { segment: 'attention' } },
  { name: '7 天内到期', state: { segment: 'due7', sort: [{ key: 'due', dir: 'asc' }] } },
  { name: '尚未接入', state: { segment: 'unbound' } },
  { name: '家宽代理', state: { filters: { cap: ['proxy'] } } },
];

/** Everything createTable() needs for the node list. */
export function nodeTableConfig(store, extra = {}) {
  return {
    rows: () => store.nodes(),
    getId: (n) => n.id,
    columns: nodeColumns(store),
    facets: nodeFacets(store),
    tokens: nodeTokens(store),
    haystack: (n) => n.haystack,
    segments: nodeSegments(store),
    presets: NODE_PRESETS,
    defaults: { sort: [{ key: 'name', dir: 'asc' }], density: 'standard', view: 'table', groupBy: null },
    storageKey: 'nodes',
    ...extra,
  };
}

/** Table config for the cross-relay entry list (/relays). */
export function entryTableConfig(store, extra = {}) {
  return {
    rows: () => store.entries(),
    getId: (x) => x.id,
    columns: {
      port: { label: '端口', type: 'number', get: (x) => x.port },
      relay: { label: '线路机', type: 'string', get: (x) => `${x.relay}:${String(x.port).padStart(5, '0')}` },
      node: { label: '绑定节点', type: 'number', get: (x) => { const n = store.node(x.bound_node); return n ? n.teamOrder * 10000 + (n.number ?? 0) : null; } },
      connections: { label: '连接', type: 'number', get: (x) => x.tcp_connections ?? null, defaultDir: 'desc' },
      rate: { label: '速率', type: 'number', get: (x) => (x.rate_down_bps ?? 0) + (x.rate_up_bps ?? 0), defaultDir: 'desc' },
      cumulative: { label: '累计', type: 'number', get: (x) => pairSum(x.cumulative), defaultDir: 'desc' },
      tcp: { label: '链路 TCP', type: 'number', get: (x) => x.link?.tcp_ms ?? null },
      egress: { label: '链路 出口', type: 'number', get: (x) => x.link?.egress_ms ?? null },
      last_active: { label: '上次活动', type: 'number', get: (x) => x.last_active ?? null, defaultDir: 'desc' },
    },
    facets: {
      relay: { label: '线路机', get: (x) => x.relay },
      team: { label: '团队', get: (x) => x.team ?? '—' },
      activity: { label: '活动', get: (x) => (x.active ? 'active' : 'idle'), word: (v) => fmt.entryWord('activity', v === 'active') },
      state: { label: '状态', get: (x) => (x.enable ? 'enabled' : 'disabled'), word: (v) => fmt.entryWord('enable', v === 'enabled') },
      binding: { label: '绑定', get: (x) => x.binding, word: (v) => fmt.entryWord('binding', v === 'bound') },
    },
    tokens: {},
    haystack: (x) => [x.id, x.port, x.bound_name, x.team, x.remark, x.outbound?.target, x.egress?.expected_ip, x.client?.label].filter(Boolean).join(' ').toLowerCase(),
    segments: [],
    defaults: { sort: [{ key: 'relay', dir: 'asc' }], groupBy: 'relay' },
    storageKey: 'entries',
    ...extra,
  };
}

/**
 * Command-palette items for nodes, relays and entries (keys.js createPalette source format).
 * `open(kind, id)` is called when an item runs.
 */
export function paletteItems(store, open = () => {}) {
  const nodeItems = store.nodes().map((n) => ({
    id: `node:${n.id}`, group: '节点', title: n.name,
    subtitle: [n.ip, n.entry_label].filter(Boolean).join(' · '),
    keywords: [n.number != null ? fmt.pad2(n.number) : null, n.number, n.ip, n.ipv6, n.billing?.service_id, n.provider, n.team, n.relay, n.connection?.client_label].filter((x) => x != null && x !== '').map(String),
    status: n.status?.code, run: () => open('node', n.id),
  }));
  const relayItems = store.relays().map((r) => ({
    id: `relay:${r.id}`, group: '线路机', title: r.name, subtitle: r.ip, keywords: [r.ip, r.provider, r.id].filter(Boolean),
    status: r.status?.code, run: () => open('relay', r.id),
  }));
  const entryItems = store.entries().map((e) => ({
    id: `entry:${e.id}`, group: '线路机', title: fmt.entryLabel(e.relay, e.port, { master: e.kind === 'relay' && !e.bound_node }),
    subtitle: e.bound_name ? `→ ${e.bound_name}` : '未绑定', keywords: [e.port, e.bound_name, e.remark].filter(Boolean).map(String),
    run: () => open('entry', e.id),
  }));
  return [...nodeItems, ...relayItems, ...entryItems];
}
