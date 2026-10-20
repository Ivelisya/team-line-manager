// burst.mjs — how many API requests a first visit makes, and how many are in flight at once.
//
//   ~/.local/bin/node tests/burst.mjs                      current adapter, default options
//   ~/.local/bin/node tests/burst.mjs <adapter.js>         another adapter build (e.g. the pre-scheduler one)
//
// Replays the fixtures with a small per-request delay, then walks a first visit: core load → first paint →
// background lanes → (optionally) the lines / onboarding page asking for job details → one node drawer.
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { fixtureFetch, apiData } from './helpers.mjs';
import { createStore } from '../../../prototype/shared/data.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Fixture fetch that takes `delayMs` per request and records the peak number in flight. */
export function slowFetch({ delayMs = 4 } = {}) {
  const f = fixtureFetch();
  let active = 0, peak = 0;
  async function g(url, init) {
    active++; peak = Math.max(peak, active);
    try { await sleep(delayMs); return await f(url, init); } finally { active--; }
  }
  g.calls = f.calls;
  g.peak = () => peak;
  return g;
}

/** Wait until no request is in flight or queued and no lane runs, for `quietMs`. */
export async function settle(src, quietMs = 60) {
  let quiet = 0;
  while (quiet < quietMs) {
    await sleep(10);
    const busy = Object.keys(src.lanes ?? {}).length || src.inflight.size || (src.active ?? 0) || (src.waiters?.length ?? 0);
    quiet = busy ? 0 : quiet + 10;
  }
}

const api = (c) => c.key.startsWith('/api/team-ops/');
export function classify(calls) {
  const out = { total: 0, core: 0, traffic: 0, month: 0, jobs: 0, spark: 0, other: 0 };
  for (const c of calls.filter(api)) {
    out.total++;
    const k = c.key.slice('/api/team-ops'.length);
    if (/^\/history\?asset=.*hours=24$/.test(k) || /^\/history\?asset=[^&]+&hours=24$/.test(k)) out.traffic++;
    else if (/^\/history\?asset=/.test(k)) out.month++;
    else if (/^\/jobs\/[^?]+$/.test(k)) out.jobs++;
    else if (/^\/komari\/hosts\/[^/]+\/metrics\?range=4h$/.test(k)) out.spark++;
    else if (/^\/(overview|health|history\?hours=24|incidents|behavior|activity\?limit=500|komari\/clients|renewals|jobs\?limit=50|komari\/hosts)$/.test(k)) out.core++;
    else out.other++;
  }
  return out;
}

/**
 * One first visit.  Returns request counts per phase and the peak in flight.
 * opts: adapter options; page: 'home' | 'lines' (asks for job details) ; drawer: open one node drawer.
 */
export async function firstVisit(mod, { opts = {}, page = 'home', drawer = true, storage = null } = {}) {
  const f = slowFetch();
  const lf = mod.createLiveFetch({ fetchImpl: f, storage, ...opts });
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const atPaint = classify(f.calls);
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  const poller = mod.createLivePoller(store, lf, { lanesDelayMs: 0, intervalMs: 3_600_000, detailWaitMs: 10_000 });
  poller.start();
  await sleep(20);
  await settle(lf.source);
  const afterLanes = classify(f.calls);
  if (page === 'lines' && poller.need) { await poller.need('jobs'); await settle(lf.source); }
  const afterPage = classify(f.calls);
  if (drawer) {
    const a = apiData('/api/team-ops/overview').assets.find((x) => x.host_measured);
    await store.detail(mod.synth.nodeId(a));
    await settle(lf.source);
  }
  const afterDrawer = classify(f.calls);
  poller.dispose();
  return { atPaint, afterLanes, afterPage, afterDrawer, peak: f.peak() };
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const file = process.argv[2] ? path.resolve(process.argv[2]) : new URL('../live/adapter.js', import.meta.url).pathname;
  const mod = await import(pathToFileURL(file).href);
  for (const page of ['home', 'lines']) {
    const r = await firstVisit(mod, { page });
    console.log(`${path.basename(file)} · first visit · ${page} page + one drawer`);
    for (const [k, v] of Object.entries(r)) console.log(`  ${k.padEnd(12)} ${typeof v === 'object' ? JSON.stringify(v) : v}`);
  }
}
