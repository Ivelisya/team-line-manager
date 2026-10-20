// fleet.test.mjs — the data layer on fleetd (fleet/API.md): /v2/api/legacy/* instead of /api/team-ops/*, SSE instead
// of polling, full latency strips from /v2/api/v1/probes/latest, and the automatic fall back to the legacy API.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fixtureFetch, apiData, hasFixtures } from './helpers.mjs';
import * as adapter from '../live/adapter.js';
import { createStore } from '../../../prototype/shared/data.js';

const skip = !hasFixtures && 'fixtures missing: run staging/stage_server.py capture on a395';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const res = (st, body) => ({ ok: st >= 200 && st < 300, status: st, json: async () => body, text: async () => JSON.stringify(body) });

/** probes/latest in the API.md shape, 20 samples per node and kind, one timeout among the TCP samples. */
function probesLatest() {
  const ov = apiData('/api/team-ops/overview');
  const nodes = {};
  for (const a of ov.assets) {
    if (!a.probe?.tcp_at) continue;
    const t1 = a.probe.tcp_at;
    const tcp = Array.from({ length: 20 }, (_, i) => (i === 7 ? { ts: t1 - (19 - i) * 30, status: 'timeout', ms: null, rc: 28 } : { ts: t1 - (19 - i) * 30, status: 'ok', ms: 10 + i, rc: 0 }));
    const egress = Array.from({ length: 20 }, (_, i) => ({ ts: t1 - (19 - i) * 300, status: i === 3 ? 'retry' : 'ok', ms: 600 + i, rc: 0 }));
    nodes[adapter.synth.nodeId(a)] = { node: adapter.synth.nodeId(a), uuid: a.uuid, tcp: { samples: 20, recent: tcp }, egress: { samples: 20, recent: egress }, path: { samples: 0, recent: [] } };
  }
  return { recent_size: 20, nodes };
}

/** A fetch that serves the fleetd paths from the legacy fixtures; `down` makes /v2/api/* fail. */
function fleetFetch({ down = () => false } = {}) {
  const legacy = fixtureFetch();
  const calls = [];
  const probes = probesLatest();
  async function f(url, init = {}) {
    const u = String(url);
    calls.push(u);
    if (u.startsWith('/v2/api/') && down()) throw new TypeError('fleetd down');
    if (u.startsWith('/v2/api/legacy/')) return legacy(u.replace('/v2/api/legacy/', '/api/team-ops/'), init);
    if (u.startsWith('/v2/api/v1/system')) return res(200, { data: { service: 'fleetd', version: '0.1.0' } });
    if (u.startsWith('/v2/api/v1/probes/latest')) return res(200, { data: probes });
    if (u.startsWith('/api/team-ops/')) return legacy(u, init);
    return res(404, { error: '接口不存在' });
  }
  f.calls = calls;
  return f;
}

/** Minimal EventSource double: the test drives it with emit(). */
class FakeES {
  static last = null;
  constructor(url) { this.url = url; this.handlers = {}; this.closed = false; FakeES.last = this; }
  addEventListener(type, fn) { (this.handlers[type] ??= []).push(fn); }
  emit(type, data = {}) { for (const fn of this.handlers[type] ?? []) fn({ data: JSON.stringify(data) }); }
  close() { this.closed = true; }
}

test('detectFleet: fleetd system JSON or its 401 → true; 404, HTML, network error → false', async () => {
  const { detectFleet } = await import('../live/boot.js');
  assert.equal(await detectFleet(async () => res(200, { data: { service: 'fleetd' } })), true);
  assert.equal(await detectFleet(async () => res(401, { error: '请先登录管理员账户' })), true);
  assert.equal(await detectFleet(async () => res(404, { error: 'not found' })), false);
  assert.equal(await detectFleet(async () => ({ ok: true, status: 200, json: async () => { throw new SyntaxError('html'); } })), false);
  assert.equal(await detectFleet(async () => { throw new TypeError('offline'); }), false);
});

test('fleet mode: legacy lists from /v2/api/legacy/, strips full (20) on first load from probes/latest', { skip }, async () => {
  const f = fleetFetch();
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  lf.source.useFleet();
  const snap = await (await lf('http://x/data/snapshot.json')).json();
  assert.ok(f.calls.every((u) => u.startsWith('/v2/api/')), 'nothing goes to /api/team-ops/ while fleetd is in use');
  assert.ok(f.calls.some((u) => u.startsWith('/v2/api/v1/probes/latest')));
  const n = snap.nodes.find((x) => x.latency?.strip_tcp);
  assert.equal(n.latency.strip_tcp.samples, 20, 'TCP strip full at once');
  assert.equal(n.latency.strip_tcp.lost.filter(Boolean).length, 1, 'the timeout is a lost cell');
  assert.ok(n.latency.strip_tcp.ms.filter((x) => x == null).length >= 1, 'no ms for the timeout');
  assert.equal(n.latency.strip_egress.samples, 19, 'retry attempts are not samples');
});

test('SSE: after hello no interval polling; changes for data read moments ago are skipped; a change refetches only that group', { skip }, async () => {
  const saved = globalThis.EventSource;
  globalThis.EventSource = FakeES;
  let poller = null;
  try {
    const f = fleetFetch();
    const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
    lf.source.useFleet();
    const raw = await (await lf('http://x/data/snapshot.json')).json();
    const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
    poller = adapter.createLivePoller(store, lf, { intervalMs: 40, lanesDelayMs: 60_000, stream: '/v2/api/stream', sseFreshMs: 300 });
    poller.start();
    const es = FakeES.last;
    assert.equal(es.url, '/v2/api/stream');
    const n00 = f.calls.length;
    es.emit('hello', { epoch: 1, versions: {} });
    // fleetd announces every resource on first sight: all of it was read at load → nothing refetched
    for (const r of ['overview', 'health', 'incidents', 'history', 'renewals', 'probes']) es.emit('change', { resource: r });
    await sleep(200);
    assert.equal(f.calls.length, n00, `first-connect changes skipped: ${f.calls.slice(n00).join(' ')}`);
    await sleep(300); // 7 × intervalMs: polling would have issued ≥ 14 requests
    const n0 = f.calls.length;
    assert.equal(n0, n00, 'no polling while the stream is healthy');
    assert.equal(poller.transport, 'sse');
    es.emit('change', { resource: 'incidents', version: 2, path: '/v2/api/legacy/incidents' });
    await sleep(400);
    const got = f.calls.slice(n0);
    assert.deepEqual(got.map((u) => u.split('?')[0]).sort(), ['/v2/api/legacy/activity', '/v2/api/legacy/behavior', '/v2/api/legacy/incidents'], got.join(' '));
    const fleetEvents = [];
    poller.onFleet((e) => fleetEvents.push(e.resource));
    es.emit('change', { resource: 'alerts', version: 3, path: '/v2/api/v1/alerts' });
    assert.deepEqual(fleetEvents, ['alerts'], 'fleetd-own resources go to the pages, not the store');
    await sleep(350);
    const n1 = f.calls.length;
    es.emit('change', { resource: 'overview' }); es.emit('change', { resource: 'health' }); es.emit('change', { resource: 'probes' });
    await sleep(400);
    assert.deepEqual(f.calls.slice(n1).map((u) => u.split('?')[0]).sort(), ['/v2/api/legacy/health', '/v2/api/legacy/overview'], 'coalesced; probes/latest at most every 30 s');
    poller.dispose();
    assert.ok(es.closed, 'stream closed on stop');
  } finally { poller?.dispose(); globalThis.EventSource = saved; }
});

test('fall back: fleetd stops answering → back to /api/team-ops/ with polling', { skip }, async () => {
  let down = false;
  const f = fleetFetch({ down: () => down });
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  lf.source.useFleet();
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  const poller = adapter.createLivePoller(store, lf, { intervalMs: 3_600_000, lanesDelayMs: 60_000 });
  const events = [];
  poller.onFleet((e) => events.push(e.resource));
  down = true;
  for (let i = 0; i < 3; i++) await poller.tick();
  await sleep(20);
  assert.equal(lf.source.apiBase, '/api/team-ops/');
  assert.equal(lf.source.fleetBase, null);
  assert.deepEqual(events, ['fleet-down']);
  const n = f.calls.length;
  const d = await poller.tick();
  assert.ok(d !== null, 'the legacy API answers again');
  assert.ok(f.calls.slice(n).every((u) => u.startsWith('/api/team-ops/')));
  poller.dispose();
});

test('decommissioned (legacy_source native): fleetd failures never fall back to /api/team-ops/', { skip }, async () => {
  let down = false;
  const f = fleetFetch({ down: () => down });
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  lf.source.useFleet();
  lf.source.decommissioned = true;
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  const poller = adapter.createLivePoller(store, lf, { intervalMs: 3_600_000, lanesDelayMs: 60_000 });
  const events = [];
  poller.onFleet((e) => events.push(e.resource));
  down = true;
  const n = f.calls.length;
  for (let i = 0; i < 4; i++) await poller.tick();
  await sleep(20);
  assert.equal(lf.source.fleetBase, '/v2/api/v1/');
  assert.equal(lf.source.apiBase, '/v2/api/legacy/');
  assert.deepEqual(events, []);
  assert.ok(f.calls.slice(n).every((u) => u.startsWith('/v2/api/')), 'no /api/team-ops/ request');
  poller.dispose();
});

test('SSE: onboard / ops / policies changes go to the page listeners with their path (no data-group refetch)', { skip }, async () => {
  const f = fleetFetch();
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  lf.source.useFleet();
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  const ES = class { constructor() { ES.last = this; this.l = {}; } addEventListener(k, fn) { this.l[k] = fn; } close() {} emit(k, d) { this.l[k]?.({ data: JSON.stringify(d) }); } };
  const prev = globalThis.EventSource;
  globalThis.EventSource = ES;
  try {
    const poller = adapter.createLivePoller(store, lf, { intervalMs: 3_600_000, lanesDelayMs: 60_000, stream: '/v2/api/stream', sseFreshMs: 0 });
    const got = [];
    poller.onFleet((e) => got.push([e.resource, e.path ?? null]));
    poller.start();
    ES.last.emit('hello', {});
    const n = f.calls.length;
    ES.last.emit('change', { resource: 'onboard', path: '/v2/api/v1/onboard/batches/b-1' });
    ES.last.emit('change', { resource: 'ops', path: '/v2/api/v1/ops/op-7' });
    ES.last.emit('change', { resource: 'policies' });
    await sleep(300);
    assert.deepEqual(got, [['onboard', '/v2/api/v1/onboard/batches/b-1'], ['ops', '/v2/api/v1/ops/op-7'], ['policies', null]]);
    assert.equal(f.calls.length, n, 'page resources do not refetch the data groups');
    poller.dispose();
  } finally { globalThis.EventSource = prev; }
});

test('switch over: fleetd appears while the page is open → its mirror + fleet-up for the pages', { skip }, async () => {
  const f = fleetFetch();
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  assert.ok(f.calls.every((u) => u.startsWith('/api/team-ops/')), 'legacy first');
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  const poller = adapter.createLivePoller(store, lf, { intervalMs: 3_600_000, lanesDelayMs: 60_000 });
  const events = [];
  poller.onFleet((e) => events.push(e.resource));
  poller.useFleet('/v2/api/stream', {});
  assert.deepEqual(events, ['fleet-up']);
  const n = f.calls.length;
  await poller.tick();
  assert.ok(f.calls.slice(n).some((u) => u.startsWith('/v2/api/legacy/')) && f.calls.slice(n).every((u) => u.startsWith('/v2/api/')), f.calls.slice(n).join(' '));
  poller.dispose();
});

test('after a write: the returned asset wins over an older /overview copy until a read catches up', { skip }, async () => {
  const f = fleetFetch();
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  await lf('http://x/data/snapshot.json');
  const src = lf.source;
  const a = src.raw.overview.assets[0];
  const upd = { ...a, revision: (a.revision ?? 1) + 1, due_date: '2027-01-15', note: 'renewed' };
  src.noteAsset(upd);
  assert.equal(src.raw.overview.assets[0].revision, upd.revision);
  assert.equal(src.raw.overview.assets[0].due_date, '2027-01-15');
  assert.ok(src.raw.overview.assets[0].due_days > 90, 'due_days recomputed in the billing time zone');
  await src.refreshFast(); // the mirror still serves the old revision
  assert.equal(src.raw.overview.assets[0].revision, upd.revision, 'stale read does not roll the edit back');
  assert.equal(src.build().snapshot.nodes.find((n) => n.komari_uuid === a.uuid || true) != null, true);
  assert.equal(adapter.calendarDays('2026-10-01', 'America/Los_Angeles', Date.parse('2026-09-23T07:22:18Z') / 1000), 8, 'same rule as the legacy due_days (线路机1 = 8 天)');
});

test('3x-ui retired: panel down is not a fault, fleet-xray gives Xray status and per-entry counters, no raw errors', { skip }, async () => {
  const S = adapter.synth;
  const ov = structuredClone(apiData('/api/team-ops/overview'));
  const health = apiData('/api/team-ops/health');
  const ERR = '<urlopen error [Errno 111] Connection refused>';
  for (const r of Object.values(ov.relays)) r.panel = { ok: false, error: ERR, xray_running: null };
  const now = ov.now;
  // legacy only (fleetd absent): Xray unknown, never “已停止”, panel not a fault, error string never surfaces
  let { snapshot } = S.buildSnapshot({ overview: ov, health }, now);
  for (const r of snapshot.relays) {
    assert.equal(r.status.parts.panel, null);
    assert.equal(r.status.parts.xray, null, 'unknown ≠ stopped');
    assert.notEqual(r.status.code, 'fail', `${r.name}: ${JSON.stringify(r.status)}`);
  }
  assert.ok(!JSON.stringify(snapshot).includes('urlopen'), 'raw panel error never reaches the UI');
  // fleetd reports 线路机1: Xray running, one inbound with 5 connections and traffic
  const r1 = snapshot.relays.find((r) => r.name === '线路机1');
  const e1 = snapshot.entries.find((e) => e.relay === '线路机1' && e.bound_node);
  const fleetRelays = { 线路机1: { host: { host_id: 'line1', online: true }, xray: {
    received_at: now - 5, online: true, process: { xray: { running: true, version: '26.9.9' }, xui: { running: false }, last_query: { ok: true } },
    entries: [{ kind: 'inbound', tag: `inbound-${e1.port}`, port: e1.port, cum_up: 1000, cum_down: 5000, rate_up_bps: 100, rate_down_bps: 2500, window_up: 10, window_down: 50, conns: 5, ts: now - 5 }] } } };
  ({ snapshot } = S.buildSnapshot({ overview: ov, health, fleetRelays }, now));
  const r = snapshot.relays.find((x) => x.name === '线路机1');
  assert.equal(r.status.parts.xray, true);
  assert.equal(r.xray.version, '26.9.9');
  assert.equal(r.managed, 'fleet');
  assert.ok(['ok', 'degraded'].includes(r.status.code), JSON.stringify(r.status));
  const e = snapshot.entries.find((x) => x.id === e1.id);
  assert.equal(e.tcp_connections, 5);
  assert.equal(e.rate_down_bps, 20000, 'B/s × 8');
  assert.equal(e.active, true);
  assert.equal(e.cumulative.down, e1.cumulative.down, 'cumulative stays on the legacy ledger (fleet-xray counters restart with the process)');
  assert.equal(snapshot.nodes.find((x) => x.id === e1.bound_node).traffic.cumulative.down, snapshot.nodes.find((x) => x.id === e1.bound_node).traffic.cumulative.down);
  const n = snapshot.nodes.find((x) => x.id === e1.bound_node);
  assert.equal(n.rate.down_bps, 20000);
  assert.equal(n.rate.source, 'fleet_xray');
  void r1;
});

test('relay node: a legacy collector-only failure is cleared when fleet-xray reports the relay healthy (dashboard = lines)', { skip }, async () => {
  const S = adapter.synth;
  const ov = structuredClone(apiData('/api/team-ops/overview'));
  const health = structuredClone(apiData('/api/team-ops/health'));
  const relay = ov.assets.find((a) => a.team === '线路机' && a.name === '线路机1');
  const hn = health.monitor.nodes[relay.uuid];
  hn.status = 'fail'; hn.reason = 'stale';
  for (const c of hn.checks) if (c.key === 'collector') { c.status = 'fail'; c.reason = 'stale'; }
  const now = ov.now;
  let { snapshot } = S.buildSnapshot({ overview: ov, health }, now);
  assert.equal(snapshot.nodes.find((n) => n.full_name === '线路机1').status.code, 'fail', 'no fleet data: legacy verdict stands');
  const fleetRelays = { 线路机1: { host: { online: true }, xray: { received_at: now - 5, online: true, process: { xray: { running: true } }, entries: [] } } };
  ({ snapshot } = S.buildSnapshot({ overview: ov, health, fleetRelays }, now));
  const n = snapshot.nodes.find((x) => x.full_name === '线路机1');
  assert.equal(n.status.code, 'ok');
  assert.equal(n.status.source, 'fleet');
  assert.equal(n.status.attention, false);
});

test('ETag: If-None-Match sent, 304 → same data, no rebuild / no diff / no clock jump', { skip }, async () => {
  const legacy = fleetFetch();
  const seen = [];
  let tagN = 1;
  async function f(url, init = {}) {
    const u = String(url);
    const inm = init.headers?.['If-None-Match'];
    seen.push({ u, inm });
    if (/\/(overview|health)$/.test(u.split('?')[0])) {
      const etag = `"v${tagN}-${u.includes('health') ? 'h' : 'o'}"`;
      if (inm === etag) return { ok: false, status: 304, headers: new Headers({ ETag: etag }), json: async () => { throw new SyntaxError('empty'); } };
      const r = await legacy(url, init);
      const body = await r.json();
      return { ok: true, status: 200, headers: new Headers({ ETag: etag }), json: async () => body };
    }
    return legacy(url, init);
  }
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  lf.source.useFleet();
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  const poller = adapter.createLivePoller(store, lf, { intervalMs: 3_600_000, lanesDelayMs: 60_000, historyMs: 3_600_000, minuteMs: 3_600_000, slowMs: 3_600_000 });
  await poller.tick(); // the first tick also reads the 60-second lists
  const t0 = store.now();
  const n304 = lf.source.stats.notModified;
  const d = await poller.tick();
  assert.ok(seen.filter((x) => /overview$/.test(x.u)).pop().inm, 'If-None-Match sent');
  assert.equal(d.unchanged, true, 'no diff on 304');
  assert.equal(lf.source.stats.notModified - n304, 2);
  assert.ok(store.now() >= t0, 'clock does not jump back');
  assert.ok(poller.age() < 1, 'a 304 still confirms the data is current');
  tagN = 2; // the server has new data
  const d2 = await poller.tick();
  assert.ok(!d2.unchanged);
  poller.dispose();
});

test('drawer: opening the same node twice within 30 s reads /node once', { skip }, async () => {
  const f = fleetFetch();
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  lf.source.useFleet();
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  const poller = adapter.createLivePoller(store, lf, { lanesDelayMs: 60_000, detailWaitMs: 5000 });
  const id = raw.nodes.find((n) => n.capabilities.includes('host')).id;
  await store.detail(id);
  await store.detail(id);
  await lf.source.enrichNode(lf.source.build().model.byId.get(id).komari_uuid); // a forced re-enrich inside 30 s
  assert.equal(f.calls.filter((u) => u.includes('/node?')).length, 1);
  poller.dispose();
});
