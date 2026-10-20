// budget.test.mjs — request budget of a first visit (production team-ops runs with CPUQuota=50 %).
//   ≤ 3 requests in flight at any time; background lanes only after the poller starts (first paint);
//   per-node traffic only where a visible part needs it; job details only when a page asks.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import * as adapter from '../live/adapter.js';
import { hasFixtures } from './helpers.mjs';
import { firstVisit, slowFetch, settle } from './burst.mjs';

const skip = !hasFixtures && 'fixtures missing: run staging/stage_server.py capture on a395';

test('first visit, home page: 10 core + 8 traffic (hostless nodes), ≤ 3 in flight, lanes after start', { skip }, async () => {
  const r = await firstVisit(adapter, { page: 'home', drawer: false });
  assert.equal(r.atPaint.total, 10, 'only the 10 core lists before first paint');
  assert.equal(r.afterLanes.traffic, 8, 'per-node 24 h traffic only for the 8 nodes without a host');
  assert.equal(r.afterLanes.month + r.afterLanes.jobs + r.afterLanes.spark, 0, 'no month-start, job-detail or sparkline requests');
  assert.ok(r.afterLanes.total <= 20, `first visit ${r.afterLanes.total} requests`);
  assert.ok(r.peak <= 3, `peak in flight ${r.peak}`);
});

test('lines / onboarding page asks for job details; a drawer adds that node\'s traffic and month start', { skip }, async () => {
  const r = await firstVisit(adapter, { page: 'lines', drawer: true });
  assert.equal(r.afterPage.jobs, 22, 'need(\'jobs\') fetches the finished jobs once');
  assert.equal(r.afterDrawer.traffic - r.afterPage.traffic, 1, 'one node history for the opened drawer');
  assert.equal(r.afterDrawer.month - r.afterPage.month, 1, 'one month start for the opened drawer');
  assert.ok(r.peak <= 3, `peak in flight ${r.peak}`);
});

test('no lane request before the poller starts; priorities: core before background', { skip }, async () => {
  const f = slowFetch();
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  await lf('http://x/data/snapshot.json');
  await settle(lf.source);
  assert.equal(f.calls.length, 10, 'loadCore alone never starts lanes');
  // a queued core request overtakes queued background ones
  const src = lf.source;
  const order = [];
  const slow = adapter.createLiveFetch({ fetchImpl: async (u, i) => { order.push(String(u)); await new Promise((r) => setTimeout(r, 5)); return f(u, i); }, storage: null, maxInflight: 1 }).source;
  const bg = ['/renewals', '/incidents', '/behavior'].map((p) => slow.get(p, null, { priority: adapter.PRIORITY.background }));
  const core = slow.get('/health', null, { priority: adapter.PRIORITY.core });
  await Promise.all([...bg, core]);
  assert.match(order[1], /\/health$/, `core request goes right after the one already in flight: ${order.join(' ')}`);
  assert.equal(src.stats.peakInflight <= 3, true);
});

test('need(\'sparks\') seeds only the asked-for hosts; hostSpark accumulate never seeds', { skip }, async () => {
  const f = slowFetch();
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const ids = raw.nodes.filter((n) => n.capabilities.includes('host')).slice(0, 2).map((n) => n.id);
  await lf.source.need('sparks', ids);
  await lf.source.need('sparks', ids); // cached
  assert.equal(f.calls.filter((c) => /metrics\?range=4h$/.test(c.key)).length, 2);
  const f2 = slowFetch();
  const lf2 = adapter.createLiveFetch({ fetchImpl: f2, storage: null, hostSpark: 'accumulate' });
  await lf2('http://x/data/snapshot.json');
  await lf2.source.need('sparks', ids);
  assert.equal(f2.calls.filter((c) => /metrics\?range=4h$/.test(c.key)).length, 0);
});

test('poller: age() counts the first load, intervalS is the poll cadence, failures are reported', { skip }, async () => {
  const f = slowFetch();
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const { createStore } = await import('../../../prototype/shared/data.js');
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  const poller = adapter.createLivePoller(store, lf, { lanesDelayMs: 60_000 });
  assert.ok(poller.age() != null && poller.age() < 5, 'fresh right after load');
  assert.equal(poller.intervalS, 10);
  const errs = [];
  poller.onError((e) => errs.push(e));
  lf.source.fetchImpl = async () => { throw new TypeError('offline'); };
  const d = await poller.tick();
  assert.equal(d, null, 'no diff when the fast refresh fails; the store keeps its data');
  assert.equal(errs.length, 1);
  assert.equal(poller.status().failing, 1);
  assert.equal(poller.status().ok, false);
  assert.equal(store.nodes().length, 41);
  poller.dispose();
});

test('opening a drawer: its 24 h history is requested before /node, /behavior and /activity', { skip }, async () => {
  const f = slowFetch();
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const { createStore } = await import('../../../prototype/shared/data.js');
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  const poller = adapter.createLivePoller(store, lf, { lanesDelayMs: 60_000, detailWaitMs: 10_000 });
  const n0 = f.calls.length;
  const id = raw.nodes.find((n) => n.capabilities.includes('host')).id;
  await store.detail(id);
  await settle(lf.source);
  const keys = f.calls.slice(n0).map((c) => c.key);
  const hist = keys.findIndex((k) => /^\/api\/team-ops\/history\?asset=[^&]+&hours=24$/.test(k));
  const node = keys.findIndex((k) => k.startsWith('/api/team-ops/node?'));
  assert.ok(hist >= 0 && hist < node, keys.join(' '));
  assert.equal(keys.filter((k) => /hours=24$/.test(k) && k.includes('asset=')).length, 1, 'no duplicate history request');
  assert.ok(store.node(id).traffic.last24h != null, 'last24h filled');
  poller.dispose();
});
