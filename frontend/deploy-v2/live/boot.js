// boot.js — live-mode glue for the read-only preview build (deploy/v2/build.sh).
//
// The build renames prototype/shared/index.js to shared/index.proto.js and generates shared/index.js:
//
//   export * from './index.proto.js';
//   import { loadSnapshot as protoLoadSnapshot } from './index.proto.js';
//   import { liveLoadSnapshot, liveCreateLive } from '../live/boot.js';
//   export const loadSnapshot = (opts) => liveLoadSnapshot(protoLoadSnapshot, opts);
//   export const createLive = (store, opts) => liveCreateLive(store, opts);
//
// so the page code stays the prototype's: loadSnapshot() reads the live API through the adapter and
// createLive() returns the poller (same controller shape as shared/live.js).  Nothing here renders UI:
// app/js/live-mode.js (globalThis.TEAMOPS_LIVE) subscribes to `TEAMOPS.on(...)` for 401 / errors.
//
// Read-only: the adapter only issues GET; this module adds no request of its own.

import { createLiveFetch, createLivePoller, AUTH_MESSAGE, AUTH_LINK } from './adapter.js';

/** Poller cadence and lane policy for production (CPUQuota=50 %: fetch only what is on screen). */
export const LIVE_OPTIONS = {
  fetch: { maxInflight: 3, trafficLane: 'needed', jobsLane: 'needed', hostSpark: 'visible', bootCache: true },
  poll: { intervalMs: 10000, lanesDelayMs: 2500 },
  /** fleetd (the new backend) behind the same origin: detected once at load, else the legacy API. */
  fleet: { system: '/v2/api/v1/system', legacy: '/v2/api/legacy/', api: '/v2/api/v1/', stream: '/v2/api/stream', detectMs: 5000, redetectMs: 300000 },
};

/**
 * Is fleetd serving this origin?  GET /v2/api/v1/system → {data:{service:'fleetd'}}.  A 401 with the fleetd login
 * message also counts (fleetd is there, the session is not).  Anything else — 404, the Komari SPA's HTML for an unknown
 * path, network error, timeout — means “not yet”: the build keeps using /api/team-ops/ with polling.
 */
export async function detectFleet(fetchImpl = globalThis.fetch?.bind(globalThis), { url = LIVE_OPTIONS.fleet.system, timeoutMs = LIVE_OPTIONS.fleet.detectMs } = {}) {
  if (!fetchImpl) return false;
  const ctl = typeof AbortController === 'function' ? new AbortController() : null;
  const t = ctl ? setTimeout(() => ctl.abort(), timeoutMs) : null;
  try {
    const r = await fetchImpl(url, { method: 'GET', credentials: 'same-origin', headers: { Accept: 'application/json' }, signal: ctl?.signal });
    let j = null;
    try { j = await r.json(); } catch { return false; }
    detectFleet.unauthorized = r.status === 401;
    if (r.status === 401) return j?.error === AUTH_MESSAGE;
    const ok = r.ok && j?.data?.service === 'fleetd';
    if (ok) detectFleet.system = j.data;
    return ok;
  } catch { return false; } finally { if (t) clearTimeout(t); }
}


const listeners = { auth: new Set(), error: new Set(), recover: new Set() };
const emit = (kind, detail) => {
  for (const fn of listeners[kind]) { try { fn(detail); } catch (e) { console.error(e); } }
  try { globalThis.dispatchEvent?.(new CustomEvent(`teamops:${kind}`, { detail })); } catch { /* node */ }
};

export const liveFetch = createLiveFetch({
  ...LIVE_OPTIONS.fetch,
  onAuthError: ({ status, message, link }) => {
    TEAMOPS.authError = { status, message: message || AUTH_MESSAGE, link: link || AUTH_LINK };
    emit('auth', TEAMOPS.authError);
  },
});

/** State the page's live-mode module reads; also handy from the console (`TEAMOPS.poller.status()`). */
export const TEAMOPS = {
  live: true,
  readOnly: true,
  authError: null,
  /** fleet/API.md §12: the old system (team-ops plugin + Komari) is gone; fleetd serves everything. */
  decommissioned: false,
  liveFetch,
  source: liveFetch.source,
  poller: null,
  options: LIVE_OPTIONS,
  /** fleetd: available (detected at load, false again after a fall back), get / post / del for its own API. */
  fleet: {
    available: false,
    /** fleetd answered /system (or its 401): the v2 login page is the fleetd one. */
    detected: false,
    /** GET /v2/api/v1/system data from the last detection (version, features, auth, legacy_source …); null before login. */
    system: null,
    /**
     * Is a fleetd capability routed?  data.features lists exactly what is (fleet/API.md §6: auth, onboard, ops, entries,
     * restart_xray, policies, assets.write …).  A fleetd without the list (< 0.4.1) has none of the §9 writes; its auth
     * (0.4.0) and onboard (edge.onboard) are recognised from the version.  Never probed with a request (no 404 noise).
     */
    has(feature) {
      const sys = TEAMOPS.fleet.system;
      if (!TEAMOPS.fleet.available || !sys) return false;
      if (Array.isArray(sys.features)) return sys.features.includes(feature);
      const v = String(sys.version ?? '0').split('.').map((x) => parseInt(x, 10) || 0);
      if (feature === 'auth') return v[0] > 0 || v[1] >= 4;
      if (feature === 'onboard') return sys.edge?.onboard === true;
      return false;
    },
    get(path, query) { return liveFetch.source.fleetGet(path, query, { priority: 0 }); },
    /** fleetd writes (alert ack, silences, maintenance): X-Fleet-Request: 1, same-origin; {data} / {error}. */
    async write(method, path, body) {
      const res = await fetch(LIVE_OPTIONS.fleet.api + String(path).replace(/^\//, ''), {
        method, credentials: 'same-origin', cache: 'no-store',
        headers: { 'Content-Type': 'application/json', Accept: 'application/json', 'X-Fleet-Request': '1' },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
      let j = null;
      try { j = await res.json(); } catch { j = null; }
      if (res.status === 401) { TEAMOPS.authError = { status: 401, message: j?.error || AUTH_MESSAGE, link: AUTH_LINK }; emit('auth', TEAMOPS.authError); }
      if (!res.ok) throw Object.assign(new Error(j?.error || `操作失败（${res.status}）`), { status: res.status, auth: res.status === 401, data: j?.data ?? null, body: j });
      return j?.data ?? j;
    },
    /** fn({resource}) when fleetd pushes alerts / notify / silences changes (or 'fleet-down' after a fall back). */
    on(fn) { return TEAMOPS.poller?.onFleet?.(fn) ?? (() => {}); },
  },
  /** on('auth' | 'error' | 'recover', fn) → unsubscribe.  'error' fires on every failed fast refresh. */
  on(kind, fn) { listeners[kind]?.add(fn); return () => listeners[kind]?.delete(fn); },
  /** The original console's asset id (Komari uuid) for a node id ('t1-01') — deep links such as
   *  /admin/nodes?asset=<uuid>&tab=billing.  null before the first load or for unknown ids. */
  assetOf(id) {
    try { return liveFetch.source.raw.overview ? liveFetch.source.build().model.byId.get(id)?.komari_uuid ?? null : null; } catch { return null; }
  },
  /** The legacy asset record (from the last /overview) for a node id: name, price, due_date, note, revision … —
   *  what the wired write actions send back (expected_revision guards against concurrent edits). */
  asset(id) {
    const u = TEAMOPS.assetOf(id);
    return u ? liveFetch.source.raw.overview?.assets?.find((a) => a.uuid === u) ?? null : null;
  },
  /** GET /node?asset= now (fresh revision and the monitoring profile), read-only like every adapter request. */
  async nodeRecord(id) {
    const u = TEAMOPS.assetOf(id);
    return u ? liveFetch.source.get('/node', { asset: u }, { priority: 0 }) : null;
  },
};
globalThis.TEAMOPS = TEAMOPS;
if (globalThis.TEAMOPS_LIVE == null) globalThis.TEAMOPS_LIVE = true;

/**
 * The /system data from detection.  legacy_source 'native' = the old system is decommissioned (fleet/API.md §12):
 * the adapter never falls back to /api/team-ops/, and the UI stops linking to the original console (Komari).
 */
function noteSystem(sys) {
  TEAMOPS.fleet.system = sys;
  TEAMOPS.decommissioned = sys?.legacy_source === 'native';
  liveFetch.source.decommissioned = TEAMOPS.decommissioned;
  liveFetch.source.features = Array.isArray(sys?.features) ? sys.features : [];
}

/** loadSnapshot through the live adapter; a 401 becomes an Error with `.auth` and the login link. */
const SYSTEM_KEY = 'teamops:live:system:v1';
const readSystem = () => { try { return JSON.parse(globalThis.localStorage?.getItem(SYSTEM_KEY) ?? 'null'); } catch { return null; } };
const writeSystem = (sys) => { try { if (sys) globalThis.localStorage?.setItem(SYSTEM_KEY, JSON.stringify(sys)); else globalThis.localStorage?.removeItem(SYSTEM_KEY); } catch { /* optional */ } };

export async function liveLoadSnapshot(protoLoadSnapshot, opts = {}) {
  // Cache-first: the last fleetd system + core lists are on this browser → open from them at once (no wait for the
  // detection round trip); detection and the fresh lists run in the background and patch the page.
  const cachedSys = readSystem();
  if (cachedSys?.service === 'fleetd' && liveFetch.source.bootPeek()?.apiBase === LIVE_OPTIONS.fleet.legacy) {
    liveFetch.source.useFleet({ legacyBase: LIVE_OPTIONS.fleet.legacy, fleetBase: LIVE_OPTIONS.fleet.api });
    TEAMOPS.fleet.available = true;
    TEAMOPS.fleet.detected = true;
    noteSystem(cachedSys);
    TEAMOPS.fromCache = true;
    detectFleet().then((ok) => {
      if (detectFleet.unauthorized) {
        liveFetch.source.bootClear(); writeSystem(null);
        liveFetch.source.authFailed = true;
        TEAMOPS.authError = { status: 401, message: AUTH_MESSAGE, link: AUTH_LINK };
        emit('auth', TEAMOPS.authError);
      } else if (ok) { noteSystem(detectFleet.system ?? cachedSys); writeSystem(detectFleet.system ?? null); }
    });
    return protoLoadSnapshot({ ...opts, fetch: liveFetch });
  }
  if (await detectFleet()) {
    liveFetch.source.useFleet({ legacyBase: LIVE_OPTIONS.fleet.legacy, fleetBase: LIVE_OPTIONS.fleet.api });
    TEAMOPS.fleet.available = true;
    TEAMOPS.fleet.detected = true;
    noteSystem(detectFleet.unauthorized ? null : detectFleet.system ?? null);
    writeSystem(detectFleet.unauthorized ? null : detectFleet.system ?? null);
    // fleetd said 401 already: straight to the login page (no second 401 from /overview in the console)
    if (detectFleet.unauthorized) {
      liveFetch.source.authFailed = true;
      TEAMOPS.authError = { status: 401, message: AUTH_MESSAGE, link: AUTH_LINK };
      throw Object.assign(new Error(AUTH_MESSAGE), { auth: true, link: AUTH_LINK });
    }
  }
  try {
    return await protoLoadSnapshot({ ...opts, fetch: liveFetch });
  } catch (e) {
    if (liveFetch.source.authFailed) {
      const err = new Error(AUTH_MESSAGE);
      err.auth = true;
      err.link = AUTH_LINK;
      throw err;
    }
    throw e;
  }
}

/** createLive(store) → the live poller.  Replays cannot rewrite live status (the `?demo=1` bar is a
 *  prototype tool).  Pages that show job steps (lines, onboarding) call app.live.need('jobs') themselves. */
export function liveCreateLive(store, opts = {}) {
  if (TEAMOPS.poller) return TEAMOPS.poller;
  // status_replays are real past moments, but applying one would overwrite the live status: disabled.
  store.replays = () => [];
  store.applyReplay = () => false;
  store.clearReplay = () => false;
  const poller = createLivePoller(store, liveFetch, { ...LIVE_OPTIONS.poll, ...(TEAMOPS.fleet.available ? { stream: LIVE_OPTIONS.fleet.stream } : {}), ...opts });
  TEAMOPS.poller = poller;
  // access-log increments: establish the cursor now, then every SSE `activity` fetches only what is new
  if (liveFetch.source.hasActivityDelta()) liveFetch.source.activityDelta().catch(() => {});
  poller.onFleet(({ resource }) => { if (resource === 'fleet-down') TEAMOPS.fleet.available = false; if (resource === 'fleet-up') TEAMOPS.fleet.available = true; });
  // fleetd not there (yet, or it went away): look again every 5 minutes while the page is visible, switch over when it is
  const redetect = setInterval(async () => {
    if (TEAMOPS.fleet.available || globalThis.document?.visibilityState === 'hidden') return;
    if (await detectFleet()) { TEAMOPS.fleet.detected = true; noteSystem(detectFleet.system ?? null); poller.useFleet(LIVE_OPTIONS.fleet.stream, { legacyBase: LIVE_OPTIONS.fleet.legacy, fleetBase: LIVE_OPTIONS.fleet.api }); }
  }, LIVE_OPTIONS.fleet.redetectMs);
  redetect.unref?.(); // node (tests): never keeps the process alive
  const dispose = poller.dispose;
  poller.dispose = () => { clearInterval(redetect); dispose(); };
  poller.onError((err) => emit('error', { message: err?.message ?? String(err), status: err?.status ?? null, auth: !!err?.auth, failing: poller.status().failing }));
  poller.onRecover(() => emit('recover', {}));
  return poller;
}
