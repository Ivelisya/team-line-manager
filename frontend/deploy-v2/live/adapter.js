// adapter.js — live data layer for the refined 团队线路管理 UI.
//
// The pages only know the prototype store (prototype/shared/data.js).  This module makes that store run
// on the real team-ops read API instead of the anonymised snapshot:
//
//   import { loadSnapshot } from '../shared/index.js';
//   import { createLiveFetch, createLivePoller } from '../live/adapter.js';
//   const liveFetch = createLiveFetch({ onAuthError: () => showLogin() });
//   const store = await loadSnapshot({ base: new URL('../data/', import.meta.url), fetch: liveFetch });
//   const live = createLivePoller(store, liveFetch).start();    // same controller shape as createLive()
//
// createLiveFetch() answers the store's data-file requests (snapshot.json, detail.json, extra.json,
// series/*.json) by calling GET /api/team-ops/<path> and synthesising the documented schema
// (synth.js; rules copied from prototype/data/tools/build.py).  Anything else (plan-catalog.json,
// static files) is passed to the real fetch.  It only ever issues GET requests.
//
// createLivePoller() re-reads overview + health every 10 s (history every 60 s, the slow lists every
// 60–300 s), rebuilds, and patches the store in place, emitting the same `store.on('change', diff)`
// diffs as shared/live.js, so pages update cells without re-rendering.

import * as S from './synth.js';

export const AUTH_MESSAGE = '请先登录管理员账户';
export const AUTH_LINK = '/admin';
export { S as synth };

const perfNow = () => (globalThis.performance?.now ? globalThis.performance.now() : Date.now());
const MONTH_LANE_KEY = 'hours';

const FILES = [
  ['snapshot', /(?:^|\/)snapshot\.json$/],
  ['detail', /(?:^|\/)detail\.json$/],
  ['extra', /(?:^|\/)extra\.json$/],
  ['traffic_5m', /(?:^|\/)series\/traffic-5m\.json$/],
  ['traffic_1m', /(?:^|\/)series\/traffic-1m\.json$/],
  ['host_24h', /(?:^|\/)series\/host-24h\.json$/],
  ['latency_24h', /(?:^|\/)series\/latency-24h\.json$/],
];

export class ApiError extends Error {
  constructor(status, message, path) {
    super(message || `读取失败 ${status}`);
    this.status = status;
    this.path = path;
    this.auth = status === 401;
  }
}

/** Minimal fetch Response for synthesised JSON (what data.js reads: ok, status, json()). */
export function jsonResponse(obj, status = 200, url = '') {
  const res = {
    ok: status >= 200 && status < 300, status, statusText: status === 200 ? 'OK' : String(status), url, redirected: false, type: 'basic',
    headers: typeof Headers === 'function' ? new Headers({ 'content-type': 'application/json; charset=utf-8' }) : new Map([['content-type', 'application/json; charset=utf-8']]),
    json: async () => obj,
    text: async () => JSON.stringify(obj),
    clone: () => jsonResponse(obj, status, url),
  };
  return res;
}

/** localStorage that never throws (private windows, blocked storage, node): get/set JSON. */
function safeStore(backing) {
  let ls = backing;
  if (ls === undefined) { try { ls = globalThis.localStorage ?? null; } catch { ls = null; } }
  return {
    get(k) { try { const v = ls?.getItem(k); return v == null ? null : JSON.parse(v); } catch { return null; } },
    set(k, v) { try { ls?.setItem(k, JSON.stringify(v)); } catch { /* quota / disabled: cache is optional */ } },
    remove(k) { try { ls?.removeItem(k); } catch { /* ignore */ } },
  };
}
const CACHE_PREFIX = 'teamops:live:';
/**
 * fleetd delta patch (fleet/API.md §12.5; reference apply() in fleet/fleetd/src/delta.rs): a JSON Merge Patch variant —
 * objects list changed keys (object value = sub-patch, anything else replaces; `null` is a value), deleted keys in `$del`,
 * arrays of {uuid} objects as {$uuid:{<uuid>: sub-patch | new element}, $removed:[…], $order:[…]}.  Never mutates
 * `target`: unchanged subtrees are shared, changed paths are new objects (so identity = unchanged).
 */
export function applyDelta(target, patch) {
  if (patch === null || typeof patch !== 'object' || Array.isArray(patch)) return patch;
  if (patch.$uuid && typeof patch.$uuid === 'object') {
    const removed = new Set(patch.$removed ?? []);
    const items = (Array.isArray(target) ? target : []).filter((x) => !removed.has(x?.uuid));
    for (const [u, sub] of Object.entries(patch.$uuid)) {
      const i = items.findIndex((x) => x?.uuid === u);
      if (i >= 0) items[i] = applyDelta(items[i], sub);
      else items.push(applyDelta({}, sub));
    }
    if (Array.isArray(patch.$order)) {
      const pos = new Map(patch.$order.map((u, i) => [u, i]));
      items.sort((a, b) => (pos.get(a?.uuid) ?? Infinity) - (pos.get(b?.uuid) ?? Infinity));
    }
    return items;
  }
  const t = target && typeof target === 'object' && !Array.isArray(target) ? { ...target } : {};
  for (const [k, v] of Object.entries(patch)) {
    if (k === '$del') continue;
    if (v !== null && typeof v === 'object' && !Array.isArray(v)) {
      const old = t[k];
      t[k] = applyDelta((old && typeof old === 'object' && !Array.isArray(old)) || v.$uuid ? old : {}, v);
    } else t[k] = v;
  }
  for (const k of patch.$del ?? []) delete t[k];
  return t;
}
/**
 * Boot cache: the last core lists, so a page opens instantly from them (stale-while-revalidate) and the fresh data
 * patches in.  One localStorage entry (~1 MB), schema-versioned, ≤ 24 h old, per API base; cleared on logout / 401.
 */
export const BOOT_KEY = CACHE_PREFIX + 'boot:v1';
const BOOT_FIELDS = ['overview', 'health', 'history', 'incidents', 'behavior', 'activity', 'clients', 'renewals', 'jobs', 'komariHosts', 'fleetQuota', 'fleetRelays', 'strips', 'audit', 'pings'];
const BOOT_MAX_AGE_MS = 24 * 3600 * 1000;
/** The part of a finished job the snapshot uses (no pending UI actions, no passwords even redacted). */
const slimJob = (d) => d && ({
  id: d.id, kind: d.kind, title: d.title, status: d.status, error: d.error ?? null, result: { error: d.result?.error ?? null },
  params: { relay: d.params?.relay, team: d.params?.team, provider: d.params?.provider, service_id: d.params?.service_id, name: d.params?.name,
    ip: d.params?.ip, ssh_port: d.params?.ssh_port, outbound: d.params?.outbound ? { type: d.params.outbound.type, method: d.params.outbound.method } : undefined },
  steps: (d.steps ?? []).map((st) => ({ key: st.key, name: st.name, status: st.status, started_at: st.started_at, finished_at: st.finished_at, error: st.error ?? null })),
});

/** Calendar days from today (in `tz`) to `ymd` — the legacy due_days rule (a date label, not 24 h units). */
export function calendarDays(ymd, tz, nowS) {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(ymd ?? '')) return null;
  let today;
  try { today = new Intl.DateTimeFormat('en-CA', { timeZone: tz || 'Asia/Shanghai', year: 'numeric', month: '2-digit', day: '2-digit' }).format(new Date(nowS * 1000)); } catch { return null; }
  return Math.round((Date.parse(`${ymd}T00:00:00Z`) - Date.parse(`${today}T00:00:00Z`)) / 86400000);
}

/** Run async fns with bounded concurrency. */
async function pool(items, limit, fn) {
  const out = new Array(items.length);
  let i = 0;
  const worker = async () => { while (i < items.length) { const k = i++; try { out[k] = await fn(items[k], k); } catch (e) { out[k] = e; } } };
  await Promise.all(Array.from({ length: Math.max(1, Math.min(limit, items.length)) }, worker));
  return out;
}

// ─── LiveSource: raw API state, dedupe/cache, accumulators, lanes ───────────────────────────────────

// Request priorities for the shared scheduler: the fast poll and first load go first, then what the user
// just opened (node drawer, charts), then background lanes.
export const PRIORITY = { core: 0, demand: 1, background: 2 };
const hasHostMetrics = (a) => !!(a?.host?.metrics_available && (a.host?.ram_total ?? 0) > 0);

export class LiveSource {
  /**
   * Lanes (what is fetched besides the 10 core lists; production team-ops runs with CPUQuota=50 %, so the
   * defaults fetch only what a visible part of the page needs, at most `maxInflight` requests at a time):
   *   trafficLane  'needed' (default: per-node 24 h traffic only for nodes without a host — the home
   *                card's “今日入口流量” popover shows their 24 h peak — plus nodes a page opened; month
   *                start hour only for opened nodes) · 'background' (all 41 + all month starts, after
   *                first paint) · 'eager' (all, before the snapshot resolves) · 'off'
   *   jobsLane     'needed' (default: finished-job details only after need('jobs'), e.g. the onboarding or
   *                lines page; cached forever in localStorage) · true | 'background' · 'eager' · false
   *   hostSpark    'visible' (default: seed a host sparkline only after need('sparks', ids); polls still
   *                accumulate) · 'seed' (all hosts, 4 h each) · 'accumulate' (never seed)
   *   maxInflight  3 — global cap on concurrent API requests, shared by every lane, poll and on-demand read
   */
  constructor({ apiBase = '/api/team-ops/', fetchImpl = globalThis.fetch?.bind(globalThis), onAuthError, concurrency = 3, maxInflight = 3,
    trafficLane = 'needed', hostSpark = 'visible', jobsLane = 'needed', meta = {}, storage, fleetBase = null,
    detailInflight = 2, timeoutMs = 30000, detailTimeoutMs = 15000, relayHosts = {}, coreGraceMs = 3000, bootCache = false } = {}) {
    this.persist = safeStore(storage);
    this.apiBase = apiBase.endsWith('/') ? apiBase : apiBase + '/';
    this.fetchImpl = fetchImpl;
    this.onAuthError = onAuthError;
    this.maxInflight = Math.max(1, maxInflight);
    this.concurrency = Math.max(1, Math.min(concurrency, this.maxInflight));
    this.opts = { trafficLane, hostSpark, jobsLane, coreGraceMs, bootCache };
    this.meta = meta;
    this.raw = { jobDetails: {}, strips: {}, sparks: {}, nodeActivity: {}, nodeHistory: null, nodeMonthStart: null };
    this.fetchedAt = {};
    this.inflight = new Map();
    this.cache = new Map();
    this.enrich = {};
    this.serverNow = null;
    this.anchor = null;
    this.authFailed = false;
    this.listeners = new Set();
    this.lastHostT = {};
    this.model = null;
    this.stats = { requests: 0, bytes: 0, peakInflight: 0, byPriority: [0, 0, 0], notModified: 0 };
    this.etags = new Map(); // url → { etag, data } (last 200 OK with an ETag)
    this.lanes = {};
    // scheduler (two lanes, see poolOf) and the client timeout per request
    this.pools = { main: { max: this.maxInflight, active: 0, waiters: [], peak: 0 }, detail: { max: Math.max(1, detailInflight), active: 0, waiters: [], peak: 0 } };
    this.qseq = 0;
    this.timeouts = { core: timeoutMs, demand: detailTimeoutMs };
    // what visible parts of the page asked for (need())
    this.wanted = { jobs: jobsLane === true || jobsLane === 'background' || jobsLane === 'eager', traffic: new Set(), sparks: new Set() };
    this.seeded = new Set();
    this.monthKey = null;
    this.monthTried = new Set();
    this.fastAt = null;
    this.fleetBase = fleetBase;
    // relay name → fleet-edge host id (fleetd [edge] relay_hosts; fleet/API.md §5c)
    this.relayHosts = { 线路机1: 'line1', 线路机2: 'central', 线路机3: 'line3', ...relayHosts };
  }

  /** Wait for one of the `maxInflight` request slots; lower priority number first, FIFO within a priority. */
  /**
   * Two lanes: `main` (first load, polls, background lanes: maxInflight) and `detail` (what the user just opened —
   * drawer, charts: detailInflight).  A slow drawer read can never starve the page / navigation data, and vice versa.
   */
  poolOf(priority) { return priority === PRIORITY.demand ? this.pools.detail : this.pools.main; }
  acquire(priority = 0) {
    const pool = this.poolOf(priority);
    if (pool.active < pool.max) {
      pool.active++;
      if (pool.active > pool.peak) pool.peak = pool.active;
      if (pool === this.pools.main && pool.active > this.stats.peakInflight) this.stats.peakInflight = pool.active;
      return null;
    }
    return new Promise((resolve) => { pool.waiters.push({ priority, seq: this.qseq++, resolve }); });
  }
  release(priority = 0) {
    const pool = this.poolOf(priority);
    if (!pool.waiters.length) { pool.active = Math.max(0, pool.active - 1); return; }
    let k = 0;
    for (let i = 1; i < pool.waiters.length; i++) {
      const w = pool.waiters[i], b = pool.waiters[k];
      if (w.priority < b.priority || (w.priority === b.priority && w.seq < b.seq)) k = i;
    }
    const [w] = pool.waiters.splice(k, 1);
    w.resolve(); // the slot passes straight to the next request
  }
  get active() { return this.pools.main.active + this.pools.detail.active; }
  get waiters() { return [...this.pools.main.waiters, ...this.pools.detail.waiters]; }
  /** Requests waiting for a slot and in flight (for status / tests). */
  queue() { return { active: this.active, waiting: this.waiters.length, main: this.pools.main.active, detail: this.pools.detail.active, detail_peak: this.pools.detail.peak }; }

  /** Server clock estimate (Unix s, fractional): last server `now` plus local elapsed time. */
  now() { return this.serverNow == null ? Date.now() / 1000 : this.serverNow + (perfNow() - this.anchor) / 1000; }
  setServerNow(t) { if (Number.isFinite(t)) { this.serverNow = t; this.anchor = perfNow(); } }
  onUpdate(fn) { this.listeners.add(fn); return () => this.listeners.delete(fn); }
  notify(reason) { if (reason !== 'core-error') this.bootSave(); for (const fn of this.listeners) { try { fn(reason); } catch (e) { void e; } } }

  url(path, query, base = this.apiBase) {
    const q = Object.entries(query ?? {}).filter(([, v]) => v != null).sort(([a], [b]) => (a < b ? -1 : 1)).map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`).join('&');
    return base + String(path).replace(/^\//, '') + (q ? '?' + q : '');
  }

  /**
   * Switch the source to fleetd (the new backend): the legacy lists come from `/v2/api/legacy/` (byte-identical to
   * `/api/team-ops/`), fleetd's own resources (probes, alerts, plans …) from `/v2/api/v1/`.  useLegacy() switches back
   * (fleetd went away): the next requests go to `/api/team-ops/` again.
   */
  useFleet({ legacyBase = '/v2/api/legacy/', fleetBase = '/v2/api/v1/' } = {}) { this.apiBase = legacyBase; this.fleetBase = fleetBase; this.cache.clear(); }
  useLegacy(apiBase = '/api/team-ops/') { this.apiBase = apiBase; this.fleetBase = null; this.cache.clear(); }
  /**
   * A write returned the updated legacy asset ({uuid, revision, …}: /assets/edit, /node/profile).  Patch it into the
   * last /overview now, and keep it over any older copy until a read catches up: fleetd's mirror of /overview can lag
   * a write by one poll (≤ 10 s), and a stale `revision` would make the next edit fail with “资产信息已变化”.
   */
  noteAsset(asset) {
    if (!asset?.uuid || !Number.isFinite(asset.revision)) return;
    (this.assetNotes ??= new Map()).set(asset.uuid, asset);
    this.applyAssetNotes();
    this.model = null;
  }
  applyAssetNotes() {
    const list = this.raw.overview?.assets;
    if (!this.assetNotes?.size || !Array.isArray(list)) return;
    for (const [u, note] of this.assetNotes) {
      const a = list.find((x) => x.uuid === u);
      if (!a) continue;
      if ((a.revision ?? 0) >= note.revision) { this.assetNotes.delete(u); continue; } // the read caught up
      for (const k of ['name', 'note', 'price', 'price_cents', 'due_date', 'expired_at', 'revision', 'profile', 'updated_at']) if (k in note) a[k] = note[k];
      if ('due_date' in note) a.due_days = calendarDays(note.due_date, a.billing_timezone, this.now()); // the server's due_days is stale too
    }
  }

  /** GET one fleetd resource (`/v2/api/v1/<path>`) through the same scheduler; null when fleetd is not in use. */
  fleetGet(path, query, opts = {}) {
    if (!this.fleetBase) return Promise.resolve(null);
    return this.get(path, query, { ...opts, base: this.fleetBase });
  }
  /**
   * Latency strips from fleetd's probe history (`/v2/api/v1/probes/latest`: the last 20 samples per node and kind),
   * so the strips are full as soon as a page opens.  Failed / timed-out / mismatched samples count as lost with no ms;
   * `retry` (a non-final egress attempt) is not a sample.  Absent endpoint → nothing changes (strips accumulate).
   */
  async refreshProbes({ force = false } = {}) {
    if (!this.fleetBase) return false;
    // ~224 KB: at most every 30 s whatever SSE says (the strips cover minutes, not seconds)
    if (!force && this.probesAt != null && perfNow() - this.probesAt < 30000) return false;
    let data;
    try { data = await this.fleetGet('/probes/latest', null, { priority: PRIORITY.core }); } catch (e) { if (e.auth) throw e; return false; }
    const byShort = new Map((this.raw.overview?.assets ?? []).map((a) => [S.nodeId(a), a.uuid]));
    // relays: fleetd may key them by node id (r1), by edge host id (line1 / central / line3) or list them under `relays`
    for (const a of this.raw.overview?.assets ?? []) { const h = this.relayHosts[S.shortName(a)]; if (h) byShort.set(h, a.uuid); }
    let n = 0;
    for (const [id, x] of [...Object.entries(data?.nodes ?? {}), ...Object.entries(data?.relays ?? {})]) {
      const u = x?.uuid ?? byShort.get(x?.node ?? id);
      if (!u) continue;
      const st = (this.raw.strips[u] ??= { tcp: { samples: [] }, egress: { samples: [] } });
      for (const kind of ['tcp', 'egress']) {
        const rec = (x[kind]?.recent ?? []).filter((r) => r && r.status !== 'retry').slice().sort((a, b) => a.ts - b.ts);
        if (!rec.length || rec.length < (st[kind]?.samples?.length ?? 0)) continue;
        const fresh = { samples: [] };
        for (const r of rec) S.stripPush(fresh, r.ts, r.ms, r.status !== 'ok' || r.ms == null);
        st[kind] = fresh;
        n++;
      }
    }
    this.probesAt = perfNow();
    this.model = null;
    return n > 0;
  }

  /** GET one API path → `data`.  Concurrent identical calls share one request; maxAge (ms) reuses a result.
   *  Every request goes through the shared scheduler (≤ maxInflight at once, by priority). */
  async get(path, query, { maxAge = 0, priority = PRIORITY.core, base } = {}) {
    const url = this.url(path, query, base);
    const hit = this.cache.get(url);
    if (hit && maxAge > 0 && perfNow() - hit.at < maxAge) return hit.data;
    if (this.inflight.has(url)) return this.inflight.get(url);
    const p = (async () => {
      if (this.authFailed) throw new ApiError(401, AUTH_MESSAGE, path);
      await this.acquire(priority);
      let res, body = null;
      // client timeout: a slow legacy read gives its slot back instead of holding it (drawer 15 s, the rest 30 s)
      const ms = priority === PRIORITY.demand ? this.timeouts.demand : this.timeouts.core;
      const ctl = typeof AbortController === 'function' && ms > 0 ? new AbortController() : null;
      const tmo = ctl ? setTimeout(() => ctl.abort(), ms) : null;
      try {
        if (this.authFailed) throw new ApiError(401, AUTH_MESSAGE, path); // the session ended while waiting
        try {
          // ETag (fleetd sends one with no-store, so the browser cache never revalidates): send If-None-Match ourselves;
          // 304 → the previous parsed data object, and callers can skip rebuild / diff when nothing changed
          const tag = this.etags.get(url);
          const headers = { Accept: 'application/json', ...(tag ? { 'If-None-Match': tag.etag } : {}) };
          res = await this.fetchImpl(url, { method: 'GET', credentials: 'same-origin', headers, ...(ctl ? { signal: ctl.signal } : {}) });
          this.stats.requests++;
          this.stats.byPriority[priority] = (this.stats.byPriority[priority] ?? 0) + 1;
          if (res.status === 304 && tag) { this.stats.notModified++; body = { data: tag.data }; }
          else { try { body = await res.json(); } catch { body = null; } }
        } catch (e) {
          if (e instanceof ApiError) throw e;
          throw new ApiError(0, ctl?.signal.aborted ? '读取超时，请稍后重试' : '运营服务暂不可用，请稍后重试', path);
        }
      } finally { if (tmo) clearTimeout(tmo); this.release(priority); }
      if (res.status === 401) {
        this.bootClear(); // never open from cached data after the session ended
        if (!this.authFailed) { this.authFailed = true; try { this.onAuthError?.({ status: 401, message: body?.error || AUTH_MESSAGE, link: AUTH_LINK }); } catch (e) { void e; } }
        throw new ApiError(401, body?.error || AUTH_MESSAGE, path);
      }
      if (!res.ok && res.status !== 304) throw new ApiError(res.status, body?.error || `读取失败 ${res.status}`, path);
      const data = body?.data;
      const etag = res.status === 304 ? null : res.headers?.get?.('ETag') ?? null;
      if (etag) this.etags.set(url, { etag, data });
      this.cache.set(url, { at: perfNow(), data });
      this.fetchedAt[path] = Math.round(this.now());
      return data;
    })();
    this.inflight.set(url, p);
    try { return await p; } finally { this.inflight.delete(url); }
  }

  // ── cadences ──
  /**
   * The relays' Xray under the new system (fleet-xray via fleet-edge relayd): /v2/api/v1/hosts + /relays/{host}/xray,
   * at most every 15 s, fleetd only.  A relay without reports (404) just has no entry — status falls back to unknown.
   */
  async refreshFleetRelays() {
    if (!this.fleetBase) return;
    if (this.fleetRelaysAt != null && perfNow() - this.fleetRelaysAt < 15000) return;
    this.fleetRelaysAt = perfNow();
    const soft = (p) => p.catch((e) => { if (e?.auth) throw e; return null; });
    const hosts = await soft(this.fleetGet('/hosts'));
    const byId = new Map((Array.isArray(hosts) ? hosts : hosts?.hosts ?? []).map((h) => [h.host_id, h]));
    const out = {};
    // only relays that report to the new system (no 404 noise for the others)
    await Promise.all(Object.entries(this.relayHosts).filter(([, id]) => byId.has(id)).map(async ([name, id]) => {
      const x = await soft(this.fleetGet(`/relays/${encodeURIComponent(id)}/xray`, { range: '5m' }));
      if (x || byId.get(id)) out[name] = { host: byId.get(id) ?? null, xray: x };
    }));
    this.raw.fleetRelays = out;
    this.model = null;
  }

  /**
   * overview / health: with fleetd's deltas (features delta.overview / delta.health) fetch only the patch since the
   * version this page holds and apply it; the first read (or an unknown version) comes back whole.  503 (no snapshot
   * yet) or any error → the full resource as before.  An empty patch returns the same object (nothing changed).
   */
  async deltaGet(name) {
    const full = () => this.get('/' + name);
    if (!this.fleetBase || !this.features?.includes(`delta.${name}`)) return full();
    const have = this.raw[name] != null ? this.deltaVer?.[name] : null;
    let d;
    try { d = await this.fleetGet(`/delta/${name}`, have != null ? { since: have } : null, { priority: PRIORITY.core }); }
    catch (e) { if (e?.auth) throw e; return full(); }
    if (!d) return full();
    (this.deltaVer ??= {})[name] = d.version;
    this.stats.deltas = (this.stats.deltas ?? 0) + (d.full ? 0 : 1);
    if (d.full) return d.data;
    if (!d.patch || !Object.keys(d.patch).length) return this.raw[name];
    return applyDelta(this.raw[name], d.patch);
  }
  async refreshFast() {
    const fr0 = this.raw.fleetRelays;
    const [overview, health] = await Promise.all([this.deltaGet('overview'), this.deltaGet('health'), this.refreshFleetRelays().catch((e) => { if (e?.auth) throw e; })]);
    // 304 for both (same objects back) and no new relay data: nothing to rebuild — keep the model and the clock anchor
    this.fastUnchanged = overview === this.raw.overview && health === this.raw.health && fr0 === this.raw.fleetRelays;
    if (this.fastUnchanged) { this.fastAt = perfNow(); return; }
    this.raw.overview = overview;
    this.raw.health = health;
    this.applyAssetNotes();
    this.setServerNow(overview?.now ?? health?.time);
    this.fastAt = perfNow();
    this.observe();
    this.model = null;
    this.bootSave();
  }
  async refreshHistory() { this.raw.history = await this.get('/history', { hours: 24 }); this.model = null; }
  async refreshMinute() {
    const [incidents, behavior, activity] = await Promise.allSettled([this.get('/incidents'), this.get('/behavior'), this.get('/activity', { limit: 500 })]);
    if (incidents.status === 'fulfilled') this.raw.incidents = incidents.value;
    if (behavior.status === 'fulfilled') this.raw.behavior = behavior.value;
    if (activity.status === 'fulfilled') this.raw.activity = activity.value;
    this.model = null;
    for (const r of [incidents, behavior, activity]) if (r.status === 'rejected' && r.reason?.auth) throw r.reason;
  }
  async refreshSlow() {
    const [clients, renewals, jobs, hosts, quota] = await Promise.allSettled([this.get('/komari/clients'), this.get('/renewals'), this.get('/jobs', { limit: 50 }), this.get('/komari/hosts'),
      // relay plan quota (catalog outbound GB) + outbound used this cycle: 流量上限 on relay cards
      this.fleetBase ? this.fleetGet('/quota', null, { maxAge: 60000 }) : Promise.resolve(null)]);
    if (quota.status === 'fulfilled' && quota.value) this.raw.fleetQuota = quota.value;
    if (clients.status === 'fulfilled') this.raw.clients = clients.value;
    if (renewals.status === 'fulfilled') this.raw.renewals = renewals.value;
    if (jobs.status === 'fulfilled') { this.raw.jobs = jobs.value; this.hydrateJobs(); }
    if (hosts.status === 'fulfilled') this.raw.komariHosts = hosts.value;
    this.model = null;
    for (const r of [clients, renewals, jobs, hosts]) if (r.status === 'rejected' && r.reason?.auth) throw r.reason;
  }

  /** First load: everything snapshot.json needs, in one round of 10 requests (≤ maxInflight at a time,
   *  overview + health first).  Lanes do NOT start here: the poller starts them after first paint. */
  async loadCore({ graceMs = this.opts.coreGraceMs ?? 3000 } = {}) {
    // Only overview + health are required for the first paint.  The other lists (history, incidents, behavior,
    // activity, clients, renewals, jobs, hosts, probe strips) are waited for at most `graceMs` after that; whatever
    // is still loading lands later through notify('core') and the poller patches the page — a slow or missing
    // optional list can never hold the first render (a production home page once sat on its skeleton).
    const soft = (p) => p.catch((e) => { if (e?.auth) throw e; });
    const fast = this.refreshFast();
    const rest = [soft(this.refreshHistory()), soft(this.refreshMinute()), soft(this.refreshSlow())];
    // probe strips map onto nodes by the overview's assets: after overview + health
    if (this.fleetBase) rest.push(soft(fast.then(() => this.refreshProbes({ force: true }))));
    const all = Promise.all(rest);
    all.catch(() => {}); // an auth failure is also reported through onAuthError by get()
    await fast;
    let timer = null;
    const late = await Promise.race([all.then(() => false, (e) => { throw e; }), new Promise((r) => { timer = setTimeout(() => r(true), graceMs); })]).finally(() => clearTimeout(timer));
    if (late) {
      this.coreLate = true;
      all.then(() => { this.coreLate = false; this.model = null; this.notify('core'); }, () => {});
    }
    this.loadedAt = perfNow();
    if (this.opts.trafficLane === 'eager') await this.trafficLane({ all: true, month: true, priority: PRIORITY.core });
    if (this.opts.jobsLane === 'eager') await this.jobsLane({ priority: PRIORITY.core });
  }
  /** Background lanes, one after another (lowest value last): per-node traffic, job details, host-spark
   *  seed — each only for what the options / need() ask for.  Called by the poller after first paint
   *  (createLivePoller lanesDelayMs); each lane patches the store through notify() when it finishes. */
  startLanes() {
    if (this.lanesStarted) return;
    this.lanesStarted = true;
    (async () => {
      const t = this.opts.trafficLane;
      if (t === 'background') await this.lane('traffic', () => this.trafficLane({ all: true, month: true }));
      else if (t === 'needed') await this.lane('traffic', () => this.trafficLane({ month: true }));
      if (this.wanted.jobs && this.opts.jobsLane !== 'eager') await this.lane('jobs', () => this.jobsLane());
      if (this.opts.hostSpark === 'seed') await this.lane('spark', () => this.sparkSeedLane());
      else if (this.wanted.sparks.size) await this.lane('spark', () => this.sparkSeedLane(this.wanted.sparks));
    })();
  }
  lane(name, fn) {
    if (this.lanes[name]) return this.lanes[name];
    const p = (async () => { try { await fn(); this.model = null; this.notify(name); } catch (e) { void e; } finally { delete this.lanes[name]; } })();
    this.lanes[name] = p;
    return p;
  }

  /**
   * A visible part of the page needs data the core lists do not carry.  Deduped, cached, low priority;
   * resolves when the data is in the store (the poller emits a diff for it).
   *   need('jobs')                job details (step timings) for the onboarding / lines pages
   *   need('traffic', ids)        per-node 24 h traffic + this month's first hour (drawer 流量 tab)
   *   need('sparks', ids)         host sparkline seed (last 4 h) for cards on screen
   * ids are node ids ('t1-01') or Komari uuids.
   */
  need(kind, ids = []) {
    const uuids = new Set();
    if (kind !== 'jobs') {
      const assets = this.raw.overview?.assets ?? [];
      const byId = this.raw.overview ? this.build().model.byId : new Map();
      for (const id of ids) {
        const u = byId.get(id)?.komari_uuid ?? (assets.some((a) => a.uuid === id) ? id : null);
        if (u) uuids.add(u);
      }
    }
    if (kind === 'jobs') {
      this.wanted.jobs = true;
      if (!this.raw.jobs) return Promise.resolve();
      const missing = (this.raw.jobs.jobs ?? []).some((j) => !this.raw.jobDetails[j.id] || j.status === 'running');
      return missing ? this.lane('jobs', () => this.jobsLane({ priority: PRIORITY.demand })) : Promise.resolve();
    }
    if (kind === 'traffic') {
      const fresh = [...uuids].filter((u) => !this.wanted.traffic.has(u) || !this.raw.nodeHistory?.[u]);
      for (const u of uuids) this.wanted.traffic.add(u);
      if (!fresh.length) return Promise.resolve();
      return this.lane('traffic:' + fresh.sort().join(','), () => this.trafficLane({ uuids: new Set(fresh), month: true, priority: PRIORITY.demand }));
    }
    if (kind === 'sparks') {
      if (this.opts.hostSpark === 'accumulate') return Promise.resolve();
      const fresh = [...uuids].filter((u) => !this.seeded.has(u));
      for (const u of uuids) this.wanted.sparks.add(u);
      if (!fresh.length) return Promise.resolve();
      return this.lane('spark:' + fresh.sort().join(','), () => this.sparkSeedLane(new Set(fresh)));
    }
    return Promise.resolve();
  }

  /**
   * Per-node 24 h traffic (5-minute buckets) and the node's first hour this month (cached per month in
   * localStorage).  Targets: `uuids` if given; else all nodes (`all`, or trafficLane 'background' |
   * 'eager'); else ('needed') the nodes without a host plus the nodes a page asked for.  Month start
   * hours: in 'needed' mode only for nodes a page asked for.
   */
  async trafficLane({ month = false, all = false, uuids = null, priority = PRIORITY.background } = {}) {
    const assets = this.raw.overview?.assets ?? [];
    const needed = this.opts.trafficLane === 'needed' && !all;
    const targets = uuids ? assets.filter((a) => uuids.has(a.uuid))
      : needed ? assets.filter((a) => !hasHostMetrics(a) || this.wanted.traffic.has(a.uuid)) : assets;
    const now = this.now();
    const hist = (this.raw.nodeHistory ??= {});
    await pool(targets, this.concurrency, async (a) => { hist[a.uuid] = await this.get('/history', { asset: a.uuid, hours: 24 }, { priority, maxAge: 60000 }); });
    this.model = null;
    if (month) {
      const m0 = S.bjMonthStart(now);
      const key = CACHE_PREFIX + 'month:' + m0;
      if (this.monthKey !== m0) { this.monthKey = m0; this.monthTried = new Set(); this.raw.nodeMonthStart = null; }
      // A node's first hour this month never changes once known.
      const first = { ...(this.persist.get(key) ?? {}), ...(this.raw.nodeMonthStart ?? {}) };
      const monthTargets = (uuids ? targets : needed ? targets.filter((a) => this.wanted.traffic.has(a.uuid)) : targets)
        .filter((a) => first[a.uuid] == null && !this.monthTried.has(a.uuid));
      const hours = Math.min(840, Math.floor((now - m0) / 3600) + 2);
      await pool(monthTargets, this.concurrency, async (a) => {
        const rows = await this.get('/history', { asset: a.uuid, [MONTH_LANE_KEY]: hours }, { priority });
        first[a.uuid] = Array.isArray(rows) && rows.length ? rows[0].t : null;
        this.monthTried.add(a.uuid); // failures are retried on the next lane
      });
      this.raw.nodeMonthStart = first;
      this.persist.set(key, Object.fromEntries(Object.entries(first).filter(([, v]) => v != null)));
    }
    this.lastTraffic = perfNow();
  }
  /** Finished job details from the local cache (no requests). */
  hydrateJobs() {
    for (const j of this.raw.jobs?.jobs ?? []) {
      if (this.raw.jobDetails[j.id] || j.status === 'running') continue;
      const cached = this.persist.get(CACHE_PREFIX + 'job:' + j.id);
      if (cached && cached.status === j.status) this.raw.jobDetails[j.id] = cached;
    }
  }
  /** Job details (step timings, SS method) — finished jobs are immutable, so fetched once and cached. */
  async jobsLane({ priority = PRIORITY.background } = {}) {
    this.hydrateJobs();
    const jobs = this.raw.jobs?.jobs ?? [];
    const need = jobs.filter((j) => !this.raw.jobDetails[j.id] || this.raw.jobDetails[j.id].status === 'running' || j.status === 'running');
    await pool(need, this.concurrency, async (j) => {
      const d = slimJob(await this.get('/jobs/' + encodeURIComponent(j.id), null, { priority }));
      this.raw.jobDetails[j.id] = d;
      if (d.status !== 'running') this.persist.set(CACHE_PREFIX + 'job:' + j.id, d);
    });
  }
  /** Slow-cadence job refresh: new / running jobs get details only when a page needs them. */
  jobsTick() { return this.wanted.jobs ? this.jobsLane() : this.hydrateJobs(); }
  /** Seed host sparklines with the last 4 h of Komari records (once per node per session, 2 at a time). */
  async sparkSeedLane(uuids = null) {
    const hosts = (this.raw.overview?.assets ?? []).filter((a) => hasHostMetrics(a) && (!uuids || uuids.has(a.uuid)) && !this.seeded.has(a.uuid));
    await pool(hosts, Math.min(2, this.concurrency), async (a) => {
      this.seeded.add(a.uuid);
      try {
        const res = await this.get(`/komari/hosts/${encodeURIComponent(a.uuid)}/metrics`, { range: '4h' }, { priority: PRIORITY.background });
        const st = (this.raw.sparks[a.uuid] ??= { buckets: new Map() });
        S.sparkSeed(st, res?.records, a.host.ram_total);
      } catch (e) { this.seeded.delete(a.uuid); throw e; }
    });
  }

  /** Accumulate probe samples (latency strips) and host samples (sparklines) from the latest poll. */
  observe() {
    const ov = this.raw.overview;
    if (!ov) return;
    for (const a of ov.assets ?? []) {
      const u = a.uuid;
      const pr = a.probe;
      if (pr) {
        const st = (this.raw.strips[u] ??= { tcp: { samples: [] }, egress: { samples: [] } });
        if (a.tcp_task_id && pr.tcp_at) S.stripPush(st.tcp, pr.tcp_at, pr.tcp_ms, !!pr.tcp_error || !(pr.tcp_ms >= 0));
        if (a.egress_task_id && pr.egress_at) S.stripPush(st.egress, pr.egress_at, pr.egress_ms, !!pr.egress_error || !(pr.egress_ms >= 0));
      }
      const h = a.host;
      const t = S.parseIso(h?.time);
      if (h && t != null && h.metrics_available && this.lastHostT[u] !== t) {
        this.lastHostT[u] = t;
        S.sparkPush((this.raw.sparks[u] ??= { buckets: new Map() }), t, S.sparkSample(h));
      }
    }
  }

  extraByUuid() {
    const out = {};
    for (const [u, e] of Object.entries(this.enrich)) if (e.node) out[u] = { hourly48: null };
    return out;
  }
  build() {
    if (this.model && this.modelAt === this.serverNow) return this.model;
    const now = this.now();
    const { snapshot, model } = S.buildSnapshot(this.raw, now, { apiBase: this.apiBase, fetchedAt: this.fetchedAt, ...this.meta });
    // availability windows 1 / 7 exist only in /node: carry them over for nodes a page has opened
    for (const n of snapshot.nodes) {
      const e = this.enrich[model.byId.get(n.id)?.komari_uuid];
      if (e?.node?.windows) for (const k of ['1', '7']) { const w = e.node.windows[k]; if (w) n.availability.windows[k] = { availability_percent: w.availability_percent ?? null, coverage_percent: w.coverage_percent ?? null }; }
    }
    this.model = { snapshot, model, now };
    this.modelAt = this.serverNow;
    return this.model;
  }

  // ── access-log increments (fleetd GET /v2/api/v1/activity/delta?after=<cursor>, SSE resource `activity`) ──
  hasActivityDelta() { return !!this.fleetBase && !!this.features?.some((f) => f === 'activity.delta' || f === 'activity.source_intel'); }
  /** New access rows since the cursor → prepended to the recent list and to each opened node's list.  → { n, assets } */
  async activityDelta() {
    if (!this.hasActivityDelta()) return { n: 0, assets: [] };
    if (this.actCursor == null) {
      const d0 = await this.fleetGet('/activity/delta', { limit: 1 }, { priority: PRIORITY.core });
      this.actCursor = d0?.cursor ?? null;
      return { n: 0, assets: [] };
    }
    const d = await this.fleetGet('/activity/delta', { after: this.actCursor, limit: 500 }, { priority: PRIORITY.core });
    if (!d) return { n: 0, assets: [] };
    this.actCursor = d.cursor ?? this.actCursor;
    const rows = (d.rows ?? []).map(({ rid, ...r }) => (void rid, r)).reverse(); // newest first, like /activity
    if (!rows.length) return { n: 0, assets: [] };
    const act = this.raw.activity;
    if (act?.rows) act.rows = [...rows, ...act.rows].slice(0, 500);
    const assets = new Set();
    for (const r of rows) {
      assets.add(r.asset);
      const na = this.raw.nodeActivity?.[r.asset];
      if (na) this.raw.nodeActivity[r.asset] = [r, ...na].slice(0, 50);
      const e = this.enrich[r.asset]?.activity;
      if (e?.rows) e.rows = [r, ...e.rows].slice(0, 50);
    }
    this.model = null;
    this.bootSave();
    return { n: rows.length, assets: [...assets] };
  }

  // ── boot cache ──
  bootPeek() {
    const c = this.persist.get(BOOT_KEY);
    return c && c.v === 1 && c.data?.overview && Date.now() - c.at < BOOT_MAX_AGE_MS ? c : null;
  }
  /** Fill raw from the boot cache (same API base only).  → the cache's age in ms, or null. */
  bootLoad() {
    const c = this.bootPeek();
    if (!c || c.apiBase !== this.apiBase) return null;
    for (const k of BOOT_FIELDS) if (c.data[k] != null) this.raw[k] = c.data[k];
    const ageMs = Date.now() - c.at;
    this.fastAt = perfNow() - ageMs; // freshness shows the cache's age until the fresh lists land
    this.fromCacheAt = c.at;
    this.model = null;
    return ageMs;
  }
  /** Debounced save of the core lists (after each successful fast refresh / late core / lane). */
  bootSave({ now = false } = {}) {
    if (!this.opts.bootCache || !this.raw.overview) return;
    const save = () => {
      const data = {};
      for (const k of BOOT_FIELDS) if (this.raw[k] != null) data[k] = this.raw[k];
      this.persist.set(BOOT_KEY, { v: 1, at: Date.now(), apiBase: this.apiBase, data });
    };
    if (now) { save(); return; }
    // throttle, not debounce: lanes notify often, the save must still happen within ~2 s of the first change
    if (this.bootTimer) return;
    this.bootTimer = setTimeout(() => { this.bootTimer = null; save(); }, 2000);
    this.bootTimer.unref?.();
  }
  bootClear() { clearTimeout(this.bootTimer); this.bootTimer = null; this.persist.remove(BOOT_KEY); }

  // ── files ──
  async snapshot() {
    if (!this.raw.overview) {
      // cache first: open from the last lists at once, load fresh in the background (the poller patches the page)
      if (this.opts.bootCache && this.bootLoad() != null) {
        this.bootedFromCache = true;
        this.loadCore().then(() => { this.bootedFromCache = false; this.fromCacheAt = null; this.model = null; this.notify('core'); },
          (e) => { if (e?.auth) this.bootClear(); this.notify('core-error'); });
      } else await this.loadCore();
    }
    return this.build().snapshot;
  }
  async detailFile() {
    if (!this.raw.overview) await this.loadCore();
    try { this.raw.audit = await this.get('/audit', null, { maxAge: 60000, priority: PRIORITY.demand }); } catch (e) { if (e.auth) throw e; }
    const { model } = this.build();
    model.enrich = this.enrich;
    return S.buildDetail(model);
  }
  async extraFile() {
    if (!this.raw.overview) await this.loadCore();
    // stale-while-revalidate: with audit + pings from the boot cache (or an earlier read) answer at once, refresh behind
    if (this.raw.audit !== undefined && this.raw.pings !== undefined && !this.extraFresh) {
      this.extraFresh = true;
      Promise.allSettled([this.get('/audit', null, { maxAge: 60000, priority: PRIORITY.demand }), this.get('/komari/pings', null, { maxAge: 300000, priority: PRIORITY.demand })])
        .then(([a, p]) => { if (a.status === 'fulfilled') this.raw.audit = a.value; if (p.status === 'fulfilled') this.raw.pings = p.value; this.model = null; this.notify('extra'); });
      return S.buildExtra(this.build().model);
    }
    const [audit, pings] = await Promise.allSettled([this.get('/audit', null, { maxAge: 60000, priority: PRIORITY.demand }), this.get('/komari/pings', null, { maxAge: 300000, priority: PRIORITY.demand })]);
    if (audit.status === 'fulfilled') this.raw.audit = audit.value;
    if (pings.status === 'fulfilled') this.raw.pings = pings.value;
    for (const r of [audit, pings]) if (r.status === 'rejected' && r.reason?.auth) throw r.reason;
    return S.buildExtra(this.build().model);
  }
  async seriesFile(kind) {
    if (!this.raw.overview) await this.loadCore();
    if (kind === 'traffic_5m' || kind === 'traffic_1m') {
      // the whole-fleet file was asked for explicitly: every node (pages use store.series per node instead)
      const missing = (this.raw.overview?.assets ?? []).some((a) => !this.raw.nodeHistory?.[a.uuid]);
      if (missing) await this.lane('traffic:all', () => this.trafficLane({ all: true, month: true, priority: PRIORITY.demand }));
      this.model = null;
      return S.trafficFile(this.build().model, { oneMinute: kind === 'traffic_1m' });
    }
    const { model } = this.build();
    const g = S.gridH5(this.now());
    const targets = kind === 'host_24h' ? model.full.filter((n) => n.capabilities.includes('host')) : model.full.filter((n) => n.probe_tasks.tcp_task_id || n.probe_tasks.egress_task_id);
    const nodes = {};
    await pool(targets, this.concurrency, async (n) => { const s = await this.nodeSeries(n.komari_uuid, kind); if (s) { const { t0, step, unit, ...cols } = s; nodes[n.id] = cols; } });
    return S.normalize({ t0: g.t0, step: 300, nodes, live: true });
  }

  /** One node's series, on demand (what store.series() is rewired to). */
  async nodeSeries(uuid, kind) {
    const { model } = this.build();
    const n = model.full.find((x) => x.komari_uuid === uuid);
    if (!n) return null;
    const now = this.now();
    if (kind === 'traffic_5m' || kind === 'traffic_1m') {
      const rows = this.raw.nodeHistory?.[uuid] ?? await this.get('/history', { asset: uuid, hours: 24 }, { maxAge: 60000, priority: PRIORITY.demand });
      const s = S.trafficSeries(rows, now);
      return S.normalize({ ...s, unit: 'bytes_per_bucket', ...(kind === 'traffic_1m' ? { resolution_s: 300 } : {}) });
    }
    if (kind === 'host_24h') {
      if (!n.capabilities.includes('host')) return null;
      const res = await this.get(`/komari/hosts/${encodeURIComponent(uuid)}/metrics`, { range: '1d' }, { maxAge: 60000, priority: PRIORITY.demand });
      const a = this.raw.overview.assets.find((x) => x.uuid === uuid);
      return S.normalize(S.hostSeries(res?.records, now, { ramTotal: a?.host?.ram_total, diskTotal: a?.host?.disk_total }));
    }
    if (kind === 'latency_24h') {
      const pings = this.raw.pings ?? (this.raw.pings = await this.get('/komari/pings', null, { maxAge: 300000, priority: PRIORITY.demand }));
      const relayUuid = model.full.find((x) => x.kind === 'relay' && x.name === n.relay)?.komari_uuid;
      const pick = (taskId, prefer) => {
        const t = (pings ?? []).find((p) => p.id === taskId);
        const clients = t?.clients ?? [];
        return prefer.find((c) => c && clients.includes(c)) ?? clients[0] ?? null;
      };
      const one = async (taskId, prefer) => {
        if (!taskId) return null;
        const entity = pick(taskId, prefer);
        if (!entity) return null;
        const res = await this.get(`/komari/pings/${taskId}/results`, { hours: 24, uuid: entity }, { maxAge: 60000, priority: PRIORITY.demand });
        return S.latencySeries(res, now, taskId);
      };
      const [tcp, egress] = await Promise.all([one(n.probe_tasks.tcp_task_id, [relayUuid, uuid]), one(n.probe_tasks.egress_task_id, [uuid, relayUuid])]);
      const g = S.gridH5(now);
      return S.normalize({ t0: g.t0, step: 300, tcp, egress });
    }
    return null;
  }

  /** Per-node detail enrichment when a page opens a node: /node (windows 1/7, 24 h monitor samples),
   *  14-day behaviour profile, last 10 access rows, 24 h TCP stability, and the node's own 24 h traffic
   *  and month start (drawer 流量 tab; the 10-minute traffic lane keeps it fresh from then on). */
  async enrichNode(uuid) {
    // the drawer's 24 h traffic first (small, and the 概览 / 流量 tabs show it): ahead of /node, /behavior,
    // /activity in the queue, patched into the store as soon as it lands
    const h24 = this.opts.trafficLane === 'off' ? null : this.get('/history', { asset: uuid, hours: 24 }, { maxAge: 60000, priority: PRIORITY.core })
      .then((rows) => { (this.raw.nodeHistory ??= {})[uuid] = rows; this.model = null; this.notify('traffic'); }, () => {});
    // 来源画像 / 来源 IP: fleetd's IP-intel sources for this node (24 h and 14 d windows), when fleetd is in use
    // (not for relays: fleetd has no source profile for a relay itself — the request would only 404)
    const isRelayAsset = (this.raw.overview?.assets ?? []).find((x) => x.uuid === uuid)?.team === '线路机';
    const src = this.fleetBase && !isRelayAsset
      ? Promise.allSettled([
        this.fleetGet(`/nodes/${encodeURIComponent(uuid)}/sources`, { window: '24h', limit: 100 }, { maxAge: 60000, priority: PRIORITY.demand }),
        this.fleetGet(`/nodes/${encodeURIComponent(uuid)}/sources`, { window: '14d', limit: 200 }, { maxAge: 300000, priority: PRIORITY.demand }),
      ])
      : Promise.resolve([]);
    const [node, behavior, activity, lat] = await Promise.allSettled([
      this.get('/node', { asset: uuid }, { maxAge: 30000, priority: PRIORITY.demand }), // 371 KB, 2–3 s server-side: reused for 30 s
      this.get('/behavior', { asset: uuid, seconds: 1209600 }, { maxAge: 60000, priority: PRIORITY.demand }),
      // 最近访问: 24 h, not 14 d — the 14-day scan takes 16–20 s on the legacy backend (0.7 s for 24 h)
      this.get('/activity', { asset: uuid, limit: 10, seconds: 86400 }, { maxAge: 15000, priority: PRIORITY.demand }),
      this.nodeSeries(uuid, 'latency_24h'),
      h24?.then(() => this.need('traffic', [uuid])), // then the month start (cached per month); history is a cache hit
    ]);
    for (const r of [node, behavior, activity]) if (r.status === 'rejected' && r.reason?.auth) throw r.reason;
    const e = (this.enrich[uuid] ??= {});
    if (node.status === 'fulfilled') e.node = node.value;
    if (behavior.status === 'fulfilled') e.behavior = behavior.value;
    if (activity.status === 'fulfilled') { e.activity = activity.value; this.raw.nodeActivity[uuid] = activity.value.rows; }
    if (lat.status === 'fulfilled' && lat.value) e.stability = S.stability(lat.value.tcp);
    const [s24, s14] = await src;
    this.model = null;
    const { model } = this.build();
    const n = model.full.find((x) => x.komari_uuid === uuid);
    if (!n) return null;
    const det = S.detailNode(n, model, e);
    const entryIps = model.entries?.find((x) => x.bound_node === n.id)?.source_ips ?? n.connection?.source_ips ?? [];
    det.ipintel = S.liveIpintel({ h24: s24?.status === 'fulfilled' ? s24.value : null, d14: s14?.status === 'fulfilled' ? s14.value : null, activity: e.activity?.rows ?? this.raw.nodeActivity?.[uuid] ?? [], entryIps, now: this.now() });
    return det;
  }
}

// ─── createLiveFetch ───────────────────────────────────────────────────────────────────────────────

/**
 * A fetch-compatible function for createStore / loadSnapshot.  Data files are synthesised from the
 * live API; everything else goes to `fetchImpl`.  `liveFetch.source` is the shared LiveSource.
 * Options: apiBase ('/api/team-ops/'), fetchImpl (fetch), onAuthError({status,message,link}),
 * concurrency (4), trafficLane ('background' | 'eager' | 'off'), hostSpark ('seed' | 'accumulate'),
 * jobsLane (true | 'eager' | false), meta ({siteName, siteDescription, komariVersion}),
 * storage (localStorage by default; null disables the small cache of finished jobs and month start hours).
 */
export function createLiveFetch({ apiBase = '/api/team-ops/', fetchImpl = globalThis.fetch?.bind(globalThis), onAuthError, ...rest } = {}) {
  const source = new LiveSource({ apiBase, fetchImpl, onAuthError, ...rest });
  async function liveFetch(input, init = {}) {
    const url = typeof input === 'string' ? input : input?.href ?? input?.url ?? String(input);
    const method = String(init?.method ?? input?.method ?? 'GET').toUpperCase();
    const path = url.split('#')[0].split('?')[0];
    const file = FILES.find(([, rx]) => rx.test(path))?.[0];
    if (!file) return fetchImpl(input, init);
    if (method !== 'GET' && method !== 'HEAD') throw new TypeError('实时数据只读：不发出写请求');
    try {
      let obj;
      if (file === 'snapshot') obj = await source.snapshot();
      else if (file === 'detail') obj = await source.detailFile();
      else if (file === 'extra') obj = await source.extraFile();
      else obj = await source.seriesFile(file);
      return jsonResponse(obj, 200, url);
    } catch (e) {
      const status = e?.status || 502;
      return jsonResponse({ error: e?.message || '读取失败' }, status === 0 ? 503 : status, url);
    }
  }
  liveFetch.source = source;
  return liveFetch;
}

// ─── createLivePoller ──────────────────────────────────────────────────────────────────────────────

// Paths replaced as a whole (live.js emits these the same way); everything else is diffed down to leaves,
// so `touched(diff, id, 'rate.', 'panel.')` style checks work exactly as with the simulator.
const ATOMIC = new Set(['status', 'latency.strip_tcp', 'latency.strip_egress', 'traffic.hourly24', 'traffic.peak_24h', 'availability.tracker30',
  'host_spark', 'connection.reality', 'reality', 'region', 'limits', 'billing.last_renewal']);
const SHARED = new Set(['host', 'bandwidth', 'billing', 'node']);
const isObj = (v) => v != null && typeof v === 'object' && !Array.isArray(v);
const same = (a, b) => a === b || (a != null && b != null && typeof a === 'object' && typeof b === 'object' && JSON.stringify(a) === JSON.stringify(b));

function diffInto(target, src, prefix, out, skip) {
  for (const key of Object.keys(src)) {
    if (!prefix && skip?.has(key)) continue;
    const path = prefix ? `${prefix}.${key}` : key;
    const sv = src[key], tv = target[key];
    if (isObj(sv) && isObj(tv) && !ATOMIC.has(path)) diffInto(tv, sv, path, out);
    else if (!same(sv, tv)) { target[key] = sv; out[path] = sv; }
  }
}

function ipNum(ip) {
  if (!ip || ip.includes(':')) return ip ? 2 ** 33 : null;
  const p = ip.split('.').map(Number);
  return ((p[0] * 256 + p[1]) * 256 + p[2]) * 256 + p[3];
}
/** Same derived fields shared/data.js normalizeNode() adds (it is not exported). */
function renormalize(n, store) {
  const caps = n.capabilities ?? [];
  n.relayId = store.relay(n.relay)?.id ?? null;
  n.hasHost = caps.includes('host') && !!n.host;
  n.isProxy = caps.includes('proxy');
  n.isRelay = caps.includes('relay') || n.kind === 'relay';
  n.caps = n.hasHost ? caps : [...caps, 'no_host'];
  n.ipNum = ipNum(n.ip);
  const pad2 = (x) => String(x).padStart(2, '0');
  n.haystack = [n.name, n.full_name, n.id, n.number != null ? pad2(n.number) : '', n.ip, n.ipv6, n.team, n.relay, n.entry_label, n.provider, n.provider_code,
    n.region?.code, n.region?.name, n.billing?.service_id, n.billing?.plan, n.connection?.client_label, ...(n.tags ?? [])].filter(Boolean).join(' ').toLowerCase();
}

const FLEET_KEYS = ['groups', 'attention', 'incidents_total', 'rankings', 'upcoming_due', 'system', 'behavior_kpis', 'behavior_top', 'access_recent', 'providers', 'teams'];

/**
 * createLivePoller(store, liveFetch, opts) → controller with the createLive() shape:
 * start, stop, running, tick() (async: one refresh, resolves to the diff), on(fn), age(), sources(),
 * clock, refresh(), status(), setEventRate / triggerEvent (no-ops: real events only).
 * opts: intervalMs (10000), historyMs (60000), minuteMs (60000), slowMs (300000), trafficMs (600000),
 *       pauseWhenHidden (true), onDemand (true: store.detail / store.series fetch per node), onError(err).
 */
export function createLivePoller(store, liveFetch, { intervalMs = 10000, historyMs = 60000, minuteMs = 60000, slowMs = 300000, trafficMs = 600000,
  pauseWhenHidden = true, onDemand = true, onError, detailWaitMs = 2500, lanesDelayMs = 2500, stream = null, sseFreshMs = 5000 } = {}) {
  const src = liveFetch?.source;
  if (!src) throw new TypeError('createLivePoller 需要 createLiveFetch() 返回的函数');
  const raw = store.raw;
  const listeners = new Set();
  const errListeners = new Set();
  const recoverListeners = new Set();
  // the store was just loaded from the core lists: that load is the first "tick" for age()
  let timer = null, seq = 0, lastTickReal = src.fastAt, visHandler = null, busy = null, lastError = null, lastSim = store.now();
  let lanesTimer = null, failures = 0;
  const due = { history: perfNow(), minute: perfNow(), slow: perfNow() + slowMs, traffic: perfNow() + trafficMs };
  const clock = () => src.now();
  store.setClock(clock);
  const enriched = new Map();

  function emit(out) {
    const empty = !Object.keys(out.nodes).length && !Object.keys(out.relays).length && !Object.keys(out.entries).length && !out.events.length && !Object.keys(out.fleet).length;
    if (empty) return out;
    store.emit('change', out);
    for (const fn of listeners) fn(out);
    return out;
  }

  /** Rebuild from the source's raw state and patch the store; returns the diff (not yet emitted). */
  function apply({ catchUp = false, reason = 'poll' } = {}) {
    const { snapshot } = src.build();
    const t = Math.floor(src.now());
    const d = { seq: ++seq, now: t, dt: Math.max(0, t - lastSim), catchUp, reason, nodes: {}, relays: {}, entries: {}, fleet: {}, events: [], live: true };
    lastSim = t;
    raw.as_of = Math.floor(src.serverNow ?? t);
    raw.as_of_text = snapshot.as_of_text;
    // nodes
    for (const nn of snapshot.nodes) {
      const n = store.node(nn.id);
      if (!n) { d.fleet.reload = 'nodes'; continue; }
      const before = { code: n.status?.code, attention: !!n.status?.attention };
      const ch = {};
      diffInto(n, nn, '', ch);
      if (Object.keys(ch).length) {
        d.nodes[n.id] = ch;
        if ('capabilities' in ch || 'host' in ch || 'ip' in ch || 'name' in ch || 'tags' in ch) renormalize(n, store);
      }
      if (n.status?.code !== before.code) d.events.push({ type: 'status', id: n.id, from: before.code, to: n.status.code, reason: n.status.reason, check: n.status.first_failed_check });
      if (!!n.status?.attention !== before.attention) d.events.push({ type: 'attention', id: n.id, attention: !!n.status.attention });
    }
    if (snapshot.nodes.length !== store.nodes().length) d.fleet.reload = 'nodes';
    // relays (host / bandwidth / billing are the node's objects: mirror the node diff like live.js)
    for (const rr of snapshot.relays) {
      const r = store.relay(rr.id);
      if (!r) { d.fleet.reload = 'relays'; continue; }
      const ch = {};
      diffInto(r, rr, '', ch, SHARED);
      if (r.node) {
        for (const k of ['host', 'bandwidth', 'billing']) if (r.node[k] && r[k] !== r.node[k]) { r[k] = r.node[k]; ch[k] = r[k]; }
        const nd = d.nodes[r.id];
        if (nd) for (const k in nd) if (k.startsWith('host.') || k.startsWith('bandwidth.') || k.startsWith('billing.') || k === 'host' || k === 'bandwidth' || k === 'billing') ch[k] = nd[k];
      }
      if (Object.keys(ch).length) d.relays[r.id] = ch;
    }
    // entries
    for (const ee of snapshot.entries) {
      const e = store.entry(ee.id);
      if (!e) { d.fleet.reload = 'entries'; continue; }
      const ch = {};
      diffInto(e, ee, '', ch);
      if (Object.keys(ch).length) d.entries[e.id] = ch;
    }
    // fleet-level objects the store reads straight from raw
    for (const k of FLEET_KEYS) if (!same(raw[k], snapshot[k])) { raw[k] = snapshot[k]; d.fleet[k] = true; }
    if (!same(raw.traffic?.fleet_24h, snapshot.traffic.fleet_24h)) { raw.traffic = { ...(raw.traffic ?? {}), fleet_24h: snapshot.traffic.fleet_24h }; d.fleet['traffic.fleet_24h'] = 'replace'; }
    const k0 = raw.kpis ?? (raw.kpis = {});
    const k1 = snapshot.kpis;
    if (!same(k0.business_rate?.spark, k1.business_rate.spark)) { k0.business_rate = { ...(k0.business_rate ?? {}), spark: k1.business_rate.spark }; d.fleet['kpis.business_rate.spark'] = 'replace'; }
    if (k0.events_today !== k1.events_today) { k0.events_today = k1.events_today; d.fleet['kpis.events_today'] = k1.events_today; }
    for (const [key, sub] of [['today_business', 'since'], ['month_business', 'since'], ['month_business', 'coverage_start']]) {
      if (k0[key]?.[sub] !== k1[key][sub]) { k0[key] = { ...(k0[key] ?? {}), [sub]: k1[key][sub] }; d.fleet[`kpis.${key}.${sub}`] = k1[key][sub]; }
    }
    if (!same(raw.onboarding?.history, snapshot.onboarding.history)) { raw.onboarding = { ...(raw.onboarding ?? {}), history: snapshot.onboarding.history }; d.fleet.onboarding = true; }
    // incidents: update known rows in place, add new ones (store keeps its own list)
    const known = new Map(store.incidents().map((i) => [i.id, i]));
    const fresh = snapshot.incidents_recent;
    for (const inc of fresh.slice().reverse()) {
      const cur = known.get(inc.id);
      if (!cur) { store.addIncident(inc); d.events.push({ type: 'incident', incident: inc }); }
      else if (!same({ ...cur }, inc)) { Object.assign(cur, inc); d.events.push({ type: 'incident', incident: cur }); }
    }
    if (Object.keys(d.nodes).length || d.events.length) d.fleet.kpis = true;
    return d;
  }

  /**
   * One refresh.  Default (polling): the fast lists always, the others when their cadence is due (or `force`d).
   * `only` (SSE): exactly these groups — fast · history · minute · slow · probes — because fleetd said they changed.
   */
  async function refresh({ catchUp = false, force = [], only = null } = {}) {
    if (busy) return busy;
    busy = (async () => {
      const now = perfNow();
      const want = (g, isDue) => (only ? only.has(g) : isDue || force.includes(g));
      const jobs = [];
      const fastJob = want('fast', true) ? src.refreshFast() : null;
      if (fastJob) jobs.push(fastJob);
      if (want('history', now >= due.history)) { jobs.push(src.refreshHistory()); due.history = now + historyMs; }
      if (want('minute', now >= due.minute)) { jobs.push(src.refreshMinute()); due.minute = now + minuteMs; }
      if (want('slow', now >= due.slow)) { jobs.push(src.refreshSlow().then(() => src.jobsTick())); due.slow = now + slowMs; }
      if (src.fleetBase && (only ? only.has('probes') : false)) jobs.push(src.refreshProbes());
      if (!only && now >= due.traffic) {
        // 10-minute refresh of per-node traffic: only the nodes the lane policy / need() cover
        if (src.opts.trafficLane !== 'off') src.lane('traffic', () => src.trafficLane({ month: true, all: src.opts.trafficLane !== 'needed' }));
        due.traffic = now + trafficMs;
      }
      const res = await Promise.allSettled(jobs);
      const bad = res.find((r) => r.status === 'rejected');
      const fastFailed = fastJob && res[0].status === 'rejected';
      // when each group was last read (SSE: a change for data read moments ago is not refetched)
      const t = perfNow();
      if (fastJob && !fastFailed) groupAt.fast = t;
      for (const g of ['history', 'minute', 'slow', 'probes']) if (only ? only.has(g) : false) groupAt[g] = t;
      if (bad) {
        lastError = bad.reason;
        if (fastFailed) failures++;
        // fleetd was in use and its legacy mirror keeps failing (not a login problem): fall back to the old API
        // fleetd unreachable 3 times in a row → back to the legacy API — unless the old system is decommissioned
        // (fleet/API.md §12): then fleetd is the only backend and the page keeps retrying it (errors shown, no fall back)
        if (fastFailed && failures >= 3 && src.fleetBase && !bad.reason?.auth && !src.decommissioned) queueMicrotask(() => api.useLegacy());
        if (bad.reason?.auth) { api.stop(); }
        try { onError?.(bad.reason); } catch (e) { void e; }
        if (fastFailed) { for (const fn of errListeners) { try { fn(bad.reason); } catch (e) { void e; } } return null; }
      } else { lastError = null; }
      if (failures && fastJob) { failures = 0; for (const fn of recoverListeners) { try { fn(); } catch (e) { void e; } } }
      // ETag 304 on overview + health and nothing else read: no rebuild, no diff — the data is confirmed current
      if (fastJob && src.fastUnchanged && jobs.length === 1) {
        lastTickReal = perfNow();
        return { seq, now: Math.floor(src.now()), dt: 0, catchUp, reason: 'not-modified', nodes: {}, relays: {}, entries: {}, fleet: {}, events: [], live: true, unchanged: true };
      }
      const d = apply({ catchUp });
      if (fastJob) lastTickReal = perfNow();
      // a status change usually comes with an incident: read incidents now instead of in a minute
      if (d.events.some((e) => e.type === 'status') && !force.includes('minute')) {
        due.minute = 0;
        if (sseOk()) queue('incidents');
      }
      return emit(d);
    })();
    try { return await busy; } finally { busy = null; }
  }

  // ─── SSE (fleetd /v2/api/stream): no polling while the stream is healthy ───
  // change {resource} → refetch just that group; reset → everything; auth → login; no heartbeat for 45 s or an
  // error → back to interval polling until the stream says hello again (EventSource reconnects by itself).
  const GROUP = { overview: 'fast', health: 'fast', history: 'history', incidents: 'minute', behavior: 'minute', activity: 'minute',
    renewals: 'slow', jobs: 'slow', audit: 'audit', probes: 'probes', alerts: 'fleet', notify: 'fleet', silences: 'fleet',
    // pages subscribe to these themselves (fleet.js onFleet): pipeline batches, relay ops, access-rule policies, IP intel
    onboard: 'fleet', ops: 'fleet', policies: 'fleet', ipintel: 'fleet' };
  let es = null, sseHealthy = false, lastBeat = 0, sseWatch = null, sseSlow = null, flushTimer = null;
  const groupAt = { fast: src.fastAt ?? 0, history: src.loadedAt ?? 0, minute: src.loadedAt ?? 0, slow: src.loadedAt ?? 0, probes: src.probesAt ?? 0 };
  const SSE_FRESH_MS = sseFreshMs;
  const pendingGroups = new Set();
  const fleetListeners = new Set();
  const sseOk = () => sseHealthy && perfNow() - lastBeat < 45000;
  let deltaBusy = null;
  function activityDelta() {
    if (deltaBusy) { deltaBusy.again = true; return; }
    deltaBusy = { again: false };
    src.activityDelta().then((r) => {
      if (r.n) {
        const out = apply({ reason: 'activity' });
        const ids = r.assets.map((u) => src.build().model.full.find((x) => x.komari_uuid === u)?.id).filter(Boolean);
        out.fleet.activity = { n: r.n, nodes: ids };
        store.emit('change', out);
      }
    }, () => {}).finally(() => { const again = deltaBusy?.again; deltaBusy = null; if (again) activityDelta(); });
  }
  function queue(resource, path) {
    // new access rows: fetch only the increment when fleetd offers it (no refetch of the minute lists)
    if (resource === 'activity' && src.hasActivityDelta()) { activityDelta(); return; }
    const g = resource === '*' ? '*' : GROUP[resource] ?? GROUP[String(resource ?? '').split('/')[0]];
    if (!g) return;
    if (g === 'fleet') { for (const fn of fleetListeners) { try { fn({ resource, path }); } catch (e) { void e; } } return; }
    if (g === 'audit') { for (const k of [...src.cache.keys()]) if (/\/audit(\?|$)/.test(k)) src.cache.delete(k); return; }
    if (g === '*') for (const x of ['fast', 'history', 'minute', 'slow', 'probes']) pendingGroups.add(x);
    // fleetd announces every resource the first time it sees it (and the first client makes it poll at once):
    // skip what this page read in the last few seconds — the load it just did already has that data
    else if (groupAt[g] != null && perfNow() - groupAt[g] < SSE_FRESH_MS) return;
    else pendingGroups.add(g);
    if (!flushTimer) flushTimer = setTimeout(flush, 250);
  }
  async function flush() {
    flushTimer = null;
    if (busy) { try { await busy; } catch (e) { void e; } }
    if (!pendingGroups.size) return;
    const only = new Set(pendingGroups);
    pendingGroups.clear();
    await refresh({ only }).catch(() => {});
  }
  function startStream() {
    if (!stream || typeof EventSource !== 'function' || es) return;
    es = new EventSource(stream);
    es.addEventListener('hello', () => {
      const wasHealthy = sseHealthy;
      sseHealthy = true; lastBeat = perfNow();
      if (timer) { clearInterval(timer); timer = null; }
      // (re)connected: catch up once — unless the lists were read moments ago (first load, or the last poll)
      const fresh = lastTickReal != null && perfNow() - lastTickReal < Math.min(intervalMs, 15000);
      if (!wasHealthy && !fresh) queue('*');
    });
    es.addEventListener('change', (e) => { lastBeat = perfNow(); let c = null; try { c = JSON.parse(e.data); } catch { return; } queue(c?.resource, c?.path ?? null); });
    es.addEventListener('heartbeat', () => { lastBeat = perfNow(); });
    es.addEventListener('reset', () => { lastBeat = perfNow(); queue('*'); });
    es.addEventListener('auth', () => { src.authFailed = true; try { src.onAuthError?.({ status: 401, message: AUTH_MESSAGE, link: AUTH_LINK }); } catch (e) { void e; } api.stop(); });
    es.onerror = () => { if (sseHealthy) { sseHealthy = false; ensurePolling(); } };
    if (!sseWatch) sseWatch = setInterval(() => {
      if (sseHealthy && !sseOk()) { sseHealthy = false; es?.close(); es = null; ensurePolling(); startStream(); }
    }, 10000);
    // what fleetd does not push (Komari client list / hosts without a Komari API key): every slowMs
    if (!sseSlow) sseSlow = setInterval(() => { if (sseOk()) { queue('renewals'); src.lane('traffic', () => src.trafficLane({ month: true })); } }, slowMs);
  }
  function stopStream() {
    es?.close(); es = null; sseHealthy = false;
    clearInterval(sseWatch); sseWatch = null; clearInterval(sseSlow); sseSlow = null; clearTimeout(flushTimer); flushTimer = null;
  }
  function ensurePolling() { if (!timer && !stopped) timer = setInterval(() => { refresh().catch(() => {}); }, intervalMs); }
  let stopped = true;

  // lanes (per-node traffic, job details, spark seed) finish in the background: patch right away
  const offUpdate = src.onUpdate((reason) => {
    if (reason === 'core') lastTickReal = src.fastAt; // the fresh lists after a cache-first open: freshness is now
    if (!busy) emit(apply({ reason: 'lane' }));
  });

  if (onDemand) {
    const origDetail = store.detail.bind(store);
    store.detail = async (id) => {
      const base = await origDetail(id);
      const n = store.node(id);
      const u = src.build().model.byId.get(n?.id)?.komari_uuid;
      if (!base || !u) return base;
      if (!enriched.has(id)) {
        const p = src.enrichNode(u).then((det) => {
          if (det) Object.assign(base, det);
          // IP intel of this node's sources → store.raw.ipintel.sources (ui/intel.js intelOf), before the drawer re-renders
          const intel = det?.ipintel?.intel ?? [];
          if (intel.length && store.raw) {
            const seen = new Set(intel.map((x) => x.ip));
            store.raw.ipintel = { ...(store.raw.ipintel ?? {}), sources: [...(store.raw.ipintel?.sources ?? []).filter((x) => !seen.has(x.ip)), ...intel] };
          }
          const d = apply({ reason: 'detail' });
          (d.nodes[id] ??= {}).detail = true;
          emit(d);
          return base;
        }).catch(() => { enriched.delete(id); return base; });
        enriched.set(id, p);
      }
      let timer;
      const wait = new Promise((r) => { timer = setTimeout(() => r(base), detailWaitMs); });
      try { return await Promise.race([enriched.get(id), wait]); } finally { clearTimeout(timer); }
    };
    store.series = async (id, kind) => {
      const n = store.node(id);
      const u = src.build().model.byId.get(n?.id)?.komari_uuid;
      if (!u) return null;
      return src.nodeSeries(u, kind);
    };
  }

  /** Background lanes start once, after first paint (the page mounts right after start()). */
  function scheduleLanes() {
    if (lanesTimer != null || src.lanesStarted) return;
    const kick = () => { lanesTimer = null; if (!src.authFailed) src.startLanes(); };
    if (lanesDelayMs <= 0) { lanesTimer = 0; kick(); return; }
    lanesTimer = setTimeout(() => {
      if (typeof globalThis.requestIdleCallback === 'function') globalThis.requestIdleCallback(kick, { timeout: 5000 });
      else kick();
    }, lanesDelayMs);
  }

  const api = {
    start() {
      if (!stopped) return api;
      stopped = false;
      src.authFailed = false;
      // SSE when there is a stream (fleetd); polling until it says hello, and whenever it is unhealthy
      ensurePolling();
      startStream();
      scheduleLanes();
      if (pauseWhenHidden && typeof document !== 'undefined' && !visHandler) {
        visHandler = () => {
          if (document.visibilityState === 'hidden') { clearInterval(timer); timer = null; stopStream(); }
          else if (!stopped) {
            if (stream && typeof EventSource === 'function') { ensurePolling(); startStream(); } // hello → catch-up, polling stops
            else { refresh({ catchUp: true }).catch(() => {}); ensurePolling(); }
          }
        };
        document.addEventListener('visibilitychange', visHandler);
      }
      return api;
    },
    stop() {
      stopped = true;
      stopStream();
      if (timer) clearInterval(timer);
      timer = null;
      if (lanesTimer) { clearTimeout(lanesTimer); lanesTimer = null; }
      if (visHandler && typeof document !== 'undefined') { document.removeEventListener('visibilitychange', visHandler); visHandler = null; }
      return api;
    },
    get running() { return !stopped; },
    /** 'sse' while fleetd's stream is healthy, else 'poll'. */
    get transport() { return sseOk() ? 'sse' : 'poll'; },
    /** fleetd's own resources changed (alerts · notify · silences): fn({resource}). */
    onFleet(fn) { fleetListeners.add(fn); return () => fleetListeners.delete(fn); },
    /** fleetd came up while the page was open: legacy lists from its mirror, SSE instead of polling. */
    useFleet(url, bases) {
      if (stream) return;
      src.useFleet(bases);
      stream = url;
      for (const fn of fleetListeners) { try { fn({ resource: 'fleet-up' }); } catch (e) { void e; } }
      if (!stopped) startStream();
    },
    /** fleetd went away: back to the legacy API and interval polling (same store, same diffs). */
    useLegacy() {
      stopStream(); stream = null; src.useLegacy();
      for (const fn of fleetListeners) { try { fn({ resource: 'fleet-down' }); } catch (e) { void e; } }
      if (!stopped) { ensurePolling(); refresh({ catchUp: true }).catch(() => {}); }
    },
    /** Poll cadence in seconds: the freshness indicator's expected interval (simulator: 2). */
    intervalS: intervalMs / 1000,
    /** One refresh now (tests / manual refresh button); resolves to the emitted diff. */
    tick() { return refresh(); },
    refresh(opts) { return refresh(opts); },
    on(fn) { listeners.add(fn); return () => listeners.delete(fn); },
    /** Failed fast refreshes (network / 5xx / 401): fn(error).  The store keeps the last data. */
    onError(fn) { errListeners.add(fn); return () => errListeners.delete(fn); },
    /** The first successful fast refresh after one or more failures: fn(). */
    onRecover(fn) { recoverListeners.add(fn); return () => recoverListeners.delete(fn); },
    /** Seconds since the last successful overview + health read (the first load counts). */
    age() { return lastTickReal == null ? null : (perfNow() - lastTickReal) / 1000; },
    /** Last update per source (server Unix s): komari · xray · monitor · panel. */
    sources() {
      const ov = src.raw.overview ?? {};
      const relays = Object.values(ov.relays ?? {});
      const max = (xs) => { const v = xs.filter((x) => Number.isFinite(x)); return v.length ? Math.max(...v) : null; };
      return { komari: ov.hosts_at ?? null, xray: max(relays.map((r) => r.received_at)), monitor: src.raw.health?.monitor?.checked_at ?? null, panel: max(relays.map((r) => r.panel?.checked_at)) };
    },
    /** ok: last refresh fully succeeded · failing: consecutive failed fast refreshes · auth: session valid. */
    status() {
      return { ok: !lastError, error: lastError?.message ?? null, failing: failures, auth: !src.authFailed, lastSuccess: lastTickReal,
        requests: src.stats.requests, peakInflight: src.stats.peakInflight, queue: src.queue() };
    },
    /** A visible part of the page needs more data (see LiveSource.need): 'jobs' | 'traffic' | 'sparks'. */
    need(kind, ids) { return src.need(kind, ids); },
    setEventRate() {},
    triggerEvent() { return null; },
    dispose() { api.stop(); offUpdate(); },
    clock,
    source: src,
  };
  return api;
}
