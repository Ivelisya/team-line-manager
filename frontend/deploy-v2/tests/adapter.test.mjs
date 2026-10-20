// adapter.test.mjs — live adapter against real API captures (fixtures from stage_server.py capture).
//   cd deploy/v2 && ~/.local/bin/node --test
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import path from 'node:path';
import { fixtureFetch, compareShape, apiData, hasFixtures, PROTO, jtype } from './helpers.mjs';
import { createLiveFetch, createLivePoller, AUTH_MESSAGE, synth as S } from '../live/adapter.js';
import { createStore, nodeTableConfig, entryTableConfig, paletteItems } from '../../../prototype/shared/data.js';
import { createTable } from '../../../prototype/shared/table.js';
import * as fmt from '../../../prototype/shared/format.js';

const REF = JSON.parse(readFileSync(path.join(PROTO, 'data', 'snapshot.json'), 'utf8'));
const REF_DETAIL = JSON.parse(readFileSync(path.join(PROTO, 'data', 'detail.json'), 'utf8'));
const REF_EXTRA = JSON.parse(readFileSync(path.join(PROTO, 'data', 'extra.json'), 'utf8'));
const opts = { trafficLane: 'eager', jobsLane: 'eager', hostSpark: 'accumulate' };
const skip = !hasFixtures && 'fixtures missing: run staging/stage_server.py capture on a395';

async function liveSnapshot(extra = {}) {
  const f = fixtureFetch(extra.fixture);
  const lf = createLiveFetch({ fetchImpl: f, ...opts, ...extra });
  const res = await lf(new URL('snapshot.json', 'http://stage.local/admin/v2/data/'));
  return { f, lf, res, snap: await res.json() };
}

// ─── schema ────────────────────────────────────────────────────────────────────────────────────────

test('snapshot: every top-level key of the prototype snapshot, same JSON type (or null)', { skip }, async () => {
  const { res, snap } = await liveSnapshot();
  assert.equal(res.status, 200);
  for (const k of Object.keys(REF)) {
    assert.ok(k in snap, `missing top-level key ${k}`);
    if (snap[k] !== null && REF[k] !== null) assert.equal(jtype(snap[k]), jtype(REF[k]), `type of ${k}`);
  }
  assert.equal(snap.nodes.length, 41);
  assert.equal(snap.relays.length, 3);
  assert.equal(snap.entries.length, REF.entries.length);
  assert.equal(snap.as_of, Math.round(apiData('/api/team-ops/overview').now));
});

const IPINTEL = new Set(['ipintel']);
export const coverage = {};

test('nodes / relays / entries: every documented field exists with the same type (or null)', { skip }, async () => {
  const { snap } = await liveSnapshot();
  const byId = new Map(snap.nodes.map((n) => [n.id, n]));
  const agg = { missing: new Set(), typeErr: new Set(), live: new Set(), degraded: new Set() };
  for (const ref of REF.nodes) {
    const got = byId.get(ref.id);
    assert.ok(got, `node ${ref.id} missing from live snapshot`);
    const c = compareShape(ref, got, '', undefined, { skip: IPINTEL });
    for (const k of Object.keys(agg)) c[k].forEach((p) => agg[k].add(p));
  }
  assert.deepEqual([...agg.missing], [], 'node fields missing');
  assert.deepEqual([...agg.typeErr], [], 'node field types');
  const degradedOnly = [...agg.degraded].filter((p) => !agg.live.has(p));
  coverage.nodes = { live: [...agg.live].filter((p) => !agg.degraded.has(p)).length, degraded: degradedOnly, partly: [...agg.degraded].filter((p) => agg.live.has(p)) };
  for (const [kind, refs, gots] of [['relays', REF.relays, snap.relays], ['entries', REF.entries, snap.entries]]) {
    const m = new Map(gots.map((x) => [x.id, x]));
    const a = { missing: new Set(), typeErr: new Set(), live: new Set(), degraded: new Set() };
    for (const ref of refs) {
      const got = m.get(ref.id);
      assert.ok(got, `${kind} ${ref.id} missing`);
      const c = compareShape(ref, got, '', undefined, { skip: IPINTEL });
      for (const k of Object.keys(a)) c[k].forEach((p) => a[k].add(p));
    }
    assert.deepEqual([...a.missing], [], `${kind} fields missing`);
    assert.deepEqual([...a.typeErr], [], `${kind} field types`);
    coverage[kind] = { live: [...a.live].filter((p) => !a.degraded.has(p)).length, degraded: [...a.degraded].filter((p) => !a.live.has(p)) };
  }
  // top-level sections other than nodes / relays / entries
  const t = { missing: [], typeErr: [], live: [], degraded: [] };
  for (const k of Object.keys(REF)) if (!['nodes', 'relays', 'entries', 'ipintel', 'provenance', 'anonymization', 'generated_at'].includes(k)) compareShape(REF[k], snap[k], k, t, { skip: new Set(['onboarding.demo_batch', 'behavior_top.sources[]']) });
  assert.deepEqual(t.missing.filter((p) => !/\.(carrier|asn|as_org|country|province|city|conn_type|conn_conf|first_seen|sessions|sessions_h24|last_seen)$/.test(p) || !p.startsWith('access_recent')), [], 'top-level fields missing');
  assert.deepEqual(t.typeErr, [], 'top-level field types');
  coverage.top = { live: t.live.length, degraded: t.degraded };
  console.log('coverage', JSON.stringify(coverage));
});

test('detail.json / extra.json keep the prototype shapes', { skip }, async () => {
  const f = fixtureFetch();
  const lf = createLiveFetch({ fetchImpl: f, ...opts });
  const det = await (await lf('http://x/data/detail.json')).json();
  const ex = await (await lf('http://x/data/extra.json')).json();
  const refNode = REF_DETAIL.nodes['t1-01'];
  const c = compareShape(refNode, det.nodes['t1-01'], '', undefined, { skip: new Set(['ipintel', 'access.recent10[]', 'access.top_sources[]']) });
  assert.deepEqual(c.missing, []);
  assert.deepEqual(c.typeErr, []);
  const e = compareShape(REF_EXTRA, ex, '', undefined, { skip: new Set(['entries[].ipintel', 'behavior.top_sources[]', 'behavior.timeline_recent[]']) });
  assert.deepEqual(e.missing, []);
  assert.deepEqual(e.typeErr, []);
  assert.equal(Object.keys(det.nodes).length, 41);
});

test('series files: traffic from /history, host and latency from Komari, per node on demand', { skip }, async () => {
  const f = fixtureFetch();
  const lf = createLiveFetch({ fetchImpl: f, ...opts });
  await lf('http://x/data/snapshot.json');
  const t5 = await (await lf('http://x/data/series/traffic-5m.json')).json();
  assert.equal(t5.step, 300);
  assert.equal(t5.unit, 'bytes_per_bucket');
  assert.equal(Object.keys(t5.nodes).length, 41);
  const t1 = await (await lf('http://x/data/series/traffic-1m.json')).json();
  assert.equal(t1.step, 300, '1-minute file is served at the API resolution (5 min)');
  const ov = apiData('/api/team-ops/overview');
  const host = ov.assets.find((a) => a.host_measured);
  const hs = await lf.source.nodeSeries(host.uuid, 'host_24h');
  assert.equal(hs.step, 300);
  assert.equal(hs.cpu_pct.length, 288);
  assert.ok(hs.cpu_pct.some((v) => v != null));
  const lat = await lf.source.nodeSeries(host.uuid, 'latency_24h');
  assert.equal(lat.tcp.avg_ms.length, 288);
  assert.ok(lat.tcp.samples > 0);
});

// ─── units and directions ──────────────────────────────────────────────────────────────────────────

test('units: bit/s from B/s ×8, down = to client, bytes and Unix seconds are integers', { skip }, async () => {
  const { snap } = await liveSnapshot();
  const ov = apiData('/api/team-ops/overview');
  for (const a of ov.assets) {
    const n = snap.nodes.find((x) => x.full_name === a.name);
    const ch = a.channel;
    if (ch && ch.rate_down != null) {
      assert.equal(n.rate.down_bps, Math.round(ch.rate_down * 8), `${n.id} down`);
      assert.equal(n.rate.up_bps, Math.round(ch.rate_up * 8), `${n.id} up`);
    }
    if (n.host) {
      assert.equal(n.host.net_rx_bps, Math.trunc(a.host.net_in * 8));
      assert.equal(n.host.net_tx_bps, Math.trunc(a.host.net_out * 8));
    }
    assert.equal(n.traffic.today.down, a.daily.down);
    assert.equal(n.traffic.month.up, a.monthly.up);
    for (const k of ['sample_at', 'last_active']) if (n.rate?.[k] != null) assert.ok(Number.isInteger(n.rate[k]));
    assert.ok(Number.isInteger(n.status.checked_at));
    if (n.traffic.hourly24) assert.equal(n.traffic.hourly24.down.length, 24);
  }
  const e = snap.entries.find((x) => x.kind === 'relay');
  const inb = ov.relays[e.relay].inbounds.find((i) => i.port === e.port);
  assert.equal(e.cumulative.down, inb.down);
  assert.equal(e.counter.down, inb.counter_down);
  // Beijing-time text and day start
  assert.match(snap.as_of_text, /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} 北京时间$/);
  assert.equal((snap.kpis.today_business.since + 8 * 3600) % 86400, 0);
  assert.equal(fmt.date(snap.kpis.today_business.since).text, snap.as_of_text.slice(0, 10));
});

// ─── store integration ─────────────────────────────────────────────────────────────────────────────

function pageSelectors() {
  const dir = path.join(PROTO, 'original-refined', 'js');
  const names = new Set();
  const walk = (d) => { for (const e of readdirSync(d, { withFileTypes: true })) { const p = path.join(d, e.name); if (e.isDirectory()) walk(p); else if (p.endsWith('.js')) for (const m of readFileSync(p, 'utf8').matchAll(/\bstore\??\.([A-Za-z_]\w*)\s*\(/g)) names.add(m[1]); } };
  walk(dir);
  return names;
}

test('createStore on the live snapshot: every selector the refined pages call works', { skip }, async () => {
  const f = fixtureFetch();
  const lf = createLiveFetch({ fetchImpl: f, ...opts });
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  const n = store.node('t1-01');
  const relay = store.relays()[0];
  const calls = {
    nodes: () => store.nodes(), node: () => store.node('t1-01') ?? store.node('团队1-01'), relays: () => store.relays(), relay: () => store.relay('r1'),
    entries: () => store.entries(), entry: () => store.entry(store.entries()[0].id), entriesFor: () => store.entriesFor('r2'), entryOf: () => store.entryOf(n),
    teams: () => store.teams(), providers: () => store.providers(), site: () => store.site(), system: () => store.system(),
    rankings: () => [store.rankings(), store.rankings('rate')], access: () => store.access('t1-01'), behavior: () => store.behavior(),
    fleetTraffic: () => store.fleetTraffic(), upcomingDue: () => store.upcomingDue(5),
    attentionOf: () => store.attentionOf(n), dueOf: () => store.dueOf(n), staleOf: () => store.staleOf(n), derived: () => store.derived(n),
    kpis: () => store.kpis(), groups: () => store.groups(), attention: () => store.attention(), unknown: () => store.unknown(),
    incidents: () => [store.incidents(), store.incidents({ open: true }), store.incidents({ node: 't1-31', limit: 3 })],
    replays: () => store.replays(), applyReplay: () => store.applyReplay(0), clearReplay: () => store.clearReplay(),
    onboarding: () => store.onboarding(), load: () => store.load('extra'), detail: () => store.detail('t1-01'), extra: () => store.extra(),
    series: async () => Promise.all(['traffic_5m', 'traffic_1m', 'host_24h', 'latency_24h'].map((k) => store.series('t1-01', k))),
    now: () => store.now(), on: () => store.on('change', () => {})(), emit: () => store.emit('noop', {}), get: () => null,
    prefetch: () => store.prefetch('extra'), setNow: () => store.setNow(store.now()), setClock: () => store.setClock(null), bump: () => store.bump(),
    addIncident: () => null, stressed: () => store.stressed(60),
  };
  const used = pageSelectors();
  const uncovered = [...used].filter((k) => !(k in calls));
  assert.deepEqual(uncovered, [], 'pages call selectors this test does not exercise — add them');
  for (const name of new Set([...used, ...Object.keys(calls)])) {
    await calls[name](); // must not throw
  }
  const k = store.kpis();
  assert.equal(k.normal.total, 41);
  assert.equal(k.monthly_cost.cents, raw.kpis.monthly_cost.cents);
  assert.equal(k.today_business.total, raw.kpis.today_business.total);
  assert.equal(k.business_rate.down_bps, raw.kpis.business_rate.down_bps);
  const g = store.groups();
  for (const row of g) assert.equal(row.active, raw.groups.find((x) => x.team === row.team).active, `groups.active ${row.team}`);
  const table = createTable(nodeTableConfig(store));
  table.setSort('rate');
  assert.equal(table.rows().length, 41);
  table.setQuery('relay:线路机2 status:ok');
  assert.ok(table.rows().length > 0);
  const et = createTable(entryTableConfig(store));
  assert.equal(et.rows().length, store.entries().length);
  assert.ok(paletteItems(store).length > 41);
  // formatting on live values
  for (const x of store.nodes()) {
    fmt.rate(x.rate?.down_bps); fmt.bytes(x.traffic?.today?.down); fmt.health(x.status?.code); fmt.due(x.billing, store.now());
    if (x.latency?.strip_tcp) assert.equal(x.latency.strip_tcp.ms.length, 20);
  }
  void relay;
});

// ─── poller ────────────────────────────────────────────────────────────────────────────────────────

test('poller: two ticks emit diffs in the live.js format; status change → events; clock = server now', { skip }, async () => {
  let perturb = false;
  const f = fixtureFetch({
    override: (key, body, set) => {
      if (!perturb || set !== 'tick2') return body;
      const b = structuredClone(body);
      if (key === '/api/team-ops/overview') {
        const a = b.data.assets.find((x) => x.name.startsWith('团队1-05 '));
        a.channel.rate_down += 5000; a.channel.rate_up += 1000;
        a.daily.down += 123456;
      }
      if (key === '/api/team-ops/health') {
        const u = Object.keys(b.data.monitor.nodes).find((k) => b.data.monitor.nodes[k].name.startsWith('团队1-05 '));
        const nd = b.data.monitor.nodes[u];
        nd.status = 'fail'; nd.reason = 'auth'; nd.since = b.data.monitor.checked_at;
        const c = nd.checks.find((x) => x.key === 'ingress'); c.status = 'fail'; c.reason = 'auth';
        b.data.monitor.attention = [u];
      }
      return b;
    },
  });
  const lf = createLiveFetch({ fetchImpl: f, ...opts });
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  const poller = createLivePoller(store, lf, { intervalMs: 60000 });
  const seen = [];
  store.on('change', (d) => seen.push(d));
  f.use('tick2');
  const d1 = await poller.tick();
  assert.ok(d1 && Object.keys(d1.nodes).length > 0, 'tick 1 emits node diffs');
  assert.equal(typeof d1.seq, 'number');
  assert.ok('fleet' in d1 && 'events' in d1 && 'relays' in d1 && 'entries' in d1);
  const someNode = Object.values(d1.nodes)[0];
  assert.ok(Object.keys(someNode).every((k) => typeof k === 'string'));
  assert.ok(Object.keys(d1.nodes).some((id) => 'status' in d1.nodes[id] || 'status.checked_at' in d1.nodes[id] || 'latency.tcp_at' in d1.nodes[id]));
  const serverNow = apiData('/api/team-ops/overview', 'tick2').now;
  assert.ok(Math.abs(store.now() - serverNow) < 5, 'store clock follows the server');
  assert.equal(store.asOf, Math.floor(serverNow));
  perturb = true;
  const before = store.node('t1-05').rate.down_bps;
  const d2 = await poller.tick();
  assert.ok(d2.nodes['t1-05'], 'perturbed node diffed');
  assert.ok('rate.down_bps' in d2.nodes['t1-05']);
  assert.notEqual(store.node('t1-05').rate.down_bps, before);
  assert.equal(store.node('t1-05').status.code, 'fail');
  assert.ok(d2.events.some((e) => e.type === 'status' && e.id === 't1-05' && e.to === 'fail'));
  assert.ok(d2.events.some((e) => e.type === 'attention' && e.id === 't1-05'));
  assert.equal(store.attentionOf(store.node('t1-05')), true);
  assert.equal(store.kpis().attention, 1);
  assert.ok(d2.fleet.kpis);
  assert.equal(seen.length, 2);
  // the poller is a drop-in for createLive(): same controller surface
  for (const m of ['start', 'stop', 'tick', 'on', 'age', 'sources', 'setEventRate', 'triggerEvent']) assert.equal(typeof poller[m], 'function', m);
  const src = poller.sources();
  assert.ok(src.komari && src.xray && src.monitor && src.panel);
  assert.ok(poller.age() >= 0);
  // only GETs ever left the adapter
  assert.ok(f.calls.every((c) => c.method === 'GET'));
  poller.dispose();
});

test('poller: on-demand detail enriches from /node (windows 1/7, 24 h samples, 14 d profile)', { skip }, async () => {
  const f = fixtureFetch();
  const lf = createLiveFetch({ fetchImpl: f, ...opts });
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  const poller = createLivePoller(store, lf, { detailWaitMs: 5000 });
  const ov = apiData('/api/team-ops/overview');
  const a = ov.assets.find((x) => x.host_measured);
  const id = S.nodeId(a);
  const det = await store.detail(id);
  assert.ok(det.availability.windows['7'].availability_percent != null, 'window 7 from /node');
  assert.ok(det.availability.hourly48.ok.some((v) => v != null));
  assert.ok(det.access.enriched);
  assert.ok(store.node(id).availability.windows['1'].availability_percent != null, 'store node gets windows 1');
  assert.ok(f.calls.some((c) => c.key.startsWith('/api/team-ops/node?asset=')));
  poller.dispose();
});

test('dedupe: one request per API path during a load, and GET only', { skip }, async () => {
  const f = fixtureFetch();
  const lf = createLiveFetch({ fetchImpl: f, trafficLane: 'off', jobsLane: false, hostSpark: 'accumulate' });
  await Promise.all([lf('http://x/data/snapshot.json'), lf('http://x/data/snapshot.json'), lf('http://x/data/detail.json')]);
  const count = (p) => f.calls.filter((c) => c.key === p).length;
  assert.equal(count('/api/team-ops/overview'), 1);
  assert.equal(count('/api/team-ops/health'), 1);
  assert.ok(f.calls.length <= 11, `first load uses a handful of requests (${f.calls.length})`);
  await assert.rejects(() => lf('http://x/data/snapshot.json', { method: 'POST' }), /只读/);
  assert.ok(f.calls.every((c) => c.method === 'GET'));
});

test('401: onAuthError once, the store load fails with the login message, no further requests', { skip }, async () => {
  const f = fixtureFetch({ status: 401 });
  const errs = [];
  const lf = createLiveFetch({ fetchImpl: f, ...opts, onAuthError: (e) => errs.push(e) });
  const res = await lf('http://x/data/snapshot.json');
  assert.equal(res.status, 401);
  assert.equal((await res.json()).error, AUTH_MESSAGE);
  const n = f.calls.length;
  const res2 = await lf('http://x/data/extra.json');
  assert.equal(res2.status, 401);
  assert.equal(f.calls.length, n, 'no requests after the session is known to be gone');
  assert.equal(errs.length, 1);
  assert.equal(errs[0].link, '/admin');
});

test('passthrough: plan-catalog.json and other files go to the real fetch', { skip }, async () => {
  const f = fixtureFetch();
  const lf = createLiveFetch({ fetchImpl: f, ...opts });
  const res = await lf('http://x/data/plan-catalog.json');
  const cat = await res.json();
  assert.ok(Array.isArray(cat.plans));
});

test('strips and sparks accumulate across polls (monitor probe samples, host samples)', () => {
  const st = { samples: [] };
  for (let i = 0; i < 25; i++) S.stripPush(st, 1000 + i * 30, 10 + i, i === 2);
  S.stripPush(st, 1000 + 24 * 30, 99, false); // duplicate t ignored
  const s = S.stripOut(st, { taskId: 1, interval: 30 });
  assert.equal(s.ms.length, 20);
  assert.equal(s.dt[19], 0);
  assert.equal(s.lost.reduce((a, b) => a + b, 0), 0, 'the lost sample scrolled out');
  const short = S.stripOut({ samples: [{ t: 5, ms: 3, lost: 0 }] }, { taskId: 1, interval: 30 });
  assert.equal(short.ms.length, 20);
  assert.equal(short.ms[0], null);
  const sp = { buckets: new Map() };
  const now = 1_790_148_138;
  for (let t = now - 3600; t < now; t += 10) S.sparkPush(sp, t, S.sparkSample({ cpu: 5, ram: 50, ram_total: 100, net_in: 1000, net_out: 10, load: 0.5, connections: 7 }));
  const out = S.sparkOut(sp, now);
  assert.equal(out.cpu_pct.length, 48);
  assert.equal(out.cpu_pct[47], 5);
  assert.equal(out.mem_pct[47], 50);
  assert.equal(out.net_rx_bps[47], 8000);
  assert.equal(out.cpu_pct[0], null);
});

test('cache: finished job details and month start hours are reused from storage on the next load', { skip }, async () => {
  const mem = new Map();
  const storage = { getItem: (k) => (mem.has(k) ? mem.get(k) : null), setItem: (k, v) => mem.set(k, String(v)) };
  const f1 = fixtureFetch();
  const lf1 = createLiveFetch({ fetchImpl: f1, ...opts, storage });
  await lf1('http://x/data/snapshot.json');
  const f2 = fixtureFetch();
  const lf2 = createLiveFetch({ fetchImpl: f2, ...opts, storage });
  const snap2 = await (await lf2('http://x/data/snapshot.json')).json();
  const jobCalls = (f) => f.calls.filter((c) => /^\/api\/team-ops\/jobs\/[^?]+$/.test(c.key)).length;
  assert.ok(jobCalls(f1) > 10);
  assert.equal(jobCalls(f2), 0, 'no job detail requests on the second load');
  assert.ok(f2.calls.length < f1.calls.length - 50, `${f2.calls.length} vs ${f1.calls.length}`);
  assert.ok(snap2.onboarding.history.every((h) => h.steps_loaded));
  assert.ok([...mem.values()].every((v) => !v.includes('pending_ui_actions') && !v.includes('***')), 'cache holds no UI actions or redacted secrets');
});

test('egress (connectivity only, fleetd 0.13.3): an exit IP back = ok; none (timeout, curl-28) = unknown; never a mismatch', { skip }, async () => {
  const ov = apiData('/api/team-ops/overview');
  const [a, b] = ov.assets.filter((x) => x.probe?.expected_ip);
  const f = fixtureFetch({ override: (key, body) => {
    if (key !== '/api/team-ops/overview') return body;
    const copy = structuredClone(body);
    for (const x of copy.data.assets) {
      if (x.uuid === a.uuid) { x.probe.actual_ip = null; x.probe.egress_error = 'curl: (28) Operation timed out'; x.probe.egress_rc = 28; }
      if (x.uuid === b.uuid) x.probe.actual_ip = '203.0.113.7';
    }
    return copy;
  } });
  const lf = createLiveFetch({ fetchImpl: f, ...opts });
  const snap = await (await lf('http://x/data/snapshot.json')).json();
  const byId = new Map(snap.nodes.map((n) => [n.id, n]));
  assert.equal(byId.get(S.nodeId(a)).latency.egress_verified, null, 'timeout → unknown');
  assert.equal(byId.get(S.nodeId(b)).latency.egress_verified, true, 'another IP is still a working exit (the IP is information only)');
  assert.ok(snap.nodes.filter((n) => n.id !== S.nodeId(a) && n.latency.egress_expected_ip && n.latency.egress_actual_ip === n.latency.egress_expected_ip).every((n) => n.latency.egress_verified === true));
});
