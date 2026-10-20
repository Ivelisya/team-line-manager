// robust.test.mjs — the production fixes after the old system went away (LIVE-MODE.md §19): first paint never waits
// for optional lists, node-drawer source cards from fleetd's IP-intel sources (with a fallback), relay plan quota.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fixtureFetch, hasFixtures } from './helpers.mjs';
import * as adapter from '../live/adapter.js';
import { createStore } from '../../../prototype/shared/data.js';

const skip = !hasFixtures && 'fixtures missing: run staging/stage_server.py capture on a395';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

test('first load: only overview + health gate the snapshot; slow optional lists land later through notify', { skip }, async () => {
  const base = fixtureFetch();
  let release;
  const gate = new Promise((r) => { release = r; });
  const slow = new Set(['/api/team-ops/history', '/api/team-ops/incidents', '/api/team-ops/komari/clients', '/api/team-ops/jobs']);
  const f = async (url, init) => { if (slow.has(String(url).split('?')[0])) await gate; return base(url, init); };
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null, coreGraceMs: 50 });
  const t0 = Date.now();
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  assert.ok(Date.now() - t0 < 2000, 'did not wait for the slow lists');
  assert.equal(raw.nodes.length, 41);
  assert.equal(lf.source.coreLate, true);
  const store = createStore(raw, { base: new URL('http://x/data/'), fetch: lf });
  assert.ok(store.kpis() && store.groups() && store.incidents(), 'pages work without the optional lists');
  const notes = [];
  lf.source.onUpdate((r) => notes.push(r));
  release();
  for (let i = 0; i < 50 && !notes.includes('core'); i++) await sleep(20);
  assert.ok(notes.includes('core'), 'the late lists announce themselves');
  assert.equal(lf.source.coreLate, false);
  assert.ok(lf.source.raw.history && lf.source.raw.incidents);
});

test('source cards: fleetd /nodes/{id}/sources (24 h + 14 d) → summary, breakdown, per-IP rows and intel records', () => {
  const now = 1790218700;
  const win = (total, n, extra = {}) => ({
    total, distinct_sources: n,
    distribution: { isp: [{ key: '中国电信', label: '中国电信', count: total - 5, sources: n - 1, pct: 95 }, { key: '中国联通', label: '中国联通', count: 5, sources: 1, pct: 5 }],
      city: [{ key: 'CN|湖北省|武汉市', label: '湖北省 武汉市', count: total - 5, sources: n - 1, pct: 95 }], province: [], type: [{ key: 'mobile', label: '移动网络', count: total - 5, sources: n - 1, pct: 95 }] },
    top_sources: [
      { ip: '203.0.113.9', count: total - 5, first_seen: now - 3600, last_seen: now - 10, intel: { ip: '203.0.113.9', isp: '中国电信', asn: 137266, country: 'CN', region: '湖北省', city: '武汉市', type: 'mobile', type_confidence: 'high', flags: [] } },
      { ip: '198.51.100.7', count: 5, first_seen: now - 9 * 86400, last_seen: now - 600, intel: { ip: '198.51.100.7', isp: '中国联通', country: 'CN', region: '河南省', city: '郑州市', type: 'residential' } },
    ],
    ...extra,
  });
  const I = adapter.synth.liveIpintel({ h24: win(100, 2), d14: win(3000, 7), now });
  assert.equal(I.basis, 'fleetd');
  assert.deepEqual(I.summary.sources, { h24: 2, d14: 7 });
  assert.equal(I.summary.new_sources_h24, 1);
  assert.deepEqual(I.summary.top_carrier.h24, { k: '中国电信', pct: 95 });
  assert.equal(I.summary.mobile_pct.h24, 95);
  assert.equal(I.breakdown.h24.total.connections, 100);
  assert.deepEqual(I.breakdown.h24.carrier[0], { k: '中国电信', key: '中国电信', pct: 95, c: 95, s: 1 });
  const a = I.sources.find((s) => s.ip === '203.0.113.9');
  assert.deepEqual(a.connections, { h24: 95, d14: 2995 });
  const x = I.intel.find((r) => r.ip === '203.0.113.9');
  assert.equal(x.carrier, '中国电信'); assert.equal(x.province, '湖北省'); assert.equal(x.conn_type, 'mobile'); assert.equal(x.conn_conf, 'high');
  // fleetd empty → the node's own access rows + the entry's current source IPs, never “no sources” while they exist
  const F = adapter.synth.liveIpintel({ h24: { total: 0, distinct_sources: 0 }, d14: null, activity: [{ ts: now - 60, source_ip: '203.0.113.9', client: 'r-01' }, { ts: now - 30, source_ip: '203.0.113.9' }, { ts: now - 90000, source_ip: '192.0.2.1' }], entryIps: ['198.51.100.7'], now });
  assert.equal(F.basis, 'activity');
  assert.equal(F.breakdown.h24.total.connections, 2);
  assert.deepEqual(F.sources.map((s) => s.ip).sort(), ['198.51.100.7', '203.0.113.9']);
  assert.deepEqual(F.sources.find((s) => s.ip === '203.0.113.9').clients, ['r-01']);
  const E = adapter.synth.liveIpintel({ now });
  assert.equal(E.basis, null);
  assert.equal(E.breakdown.h24.total.connections, 0);
});

test('relay cards: 流量上限 = plan quota (decimal GB, outbound) from fleetd /quota; latency strip from relay probes', { skip }, async () => {
  const base = fixtureFetch();
  const quota = { relays: [{ relay: '线路机1', id: 'r1', canonical_name: 'GoMami Pulse Mini', quota_bytes: 2e12, used_bytes: 2464829951618, used_pct: 123.2, direction: 'outbound', period: 'billing_cycle', reset_day: 1, timezone: 'America/Los_Angeles' }] };
  const f = async (url, init) => {
    const u = String(url);
    if (u.startsWith('/v2/api/v1/quota')) return { ok: true, status: 200, json: async () => ({ data: quota }) };
    if (u.startsWith('/v2/api/v1/probes/latest')) {
      const t = 1790218000;
      const recent = Array.from({ length: 20 }, (_, i) => ({ ts: t + i * 30, status: i === 4 ? 'timeout' : 'ok', ms: i === 4 ? null : 40 + i }));
      const eg = Array.from({ length: 20 }, (_, i) => ({ ts: t + i * 300, status: 'ok', ms: 300 + i }));
      return { ok: true, status: 200, json: async () => ({ data: { nodes: {}, relays: { line1: { tcp: { recent }, egress: { recent: eg } } } } }) };
    }
    if (u.startsWith('/v2/api/v1/')) return { ok: false, status: 404, json: async () => ({ error: '接口不存在' }) };
    return base(u.replace('/v2/api/legacy/', '/api/team-ops/'), init);
  };
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  lf.source.useFleet();
  const raw = await (await lf('http://x/data/snapshot.json')).json();
  const r1 = raw.nodes.find((n) => n.id === 'r1');
  assert.equal(r1.traffic.quota.limit_bytes, 2e12);
  assert.equal(r1.traffic.quota.used_bytes, 2464829951618);
  assert.equal(r1.traffic.quota.type, 'up');
  assert.equal(r1.traffic.quota.plan, 'GoMami Pulse Mini');
  // relay latency from fleetd's control-path probes (keyed by edge host id)
  assert.equal(r1.latency.strip_tcp?.samples, 20);
  assert.equal(r1.latency.strip_tcp.lost.filter(Boolean).length, 1);
  assert.equal(r1.latency.strip_egress?.samples, 20);
  assert.equal(r1.latency.tcp_ms, 59, 'newest TCP sample → the card / table value');
  assert.equal(r1.latency.egress_ms, 319);
  const M = await import('../../../prototype/original-refined/js/node-model.js');
  const q = M.quotaModel(r1);
  assert.equal(q.limit.text, '2,000 GB');
  assert.equal(q.used.text, '2,465 GB');
  assert.equal(q.tone, 'bad');
  assert.ok(q.pct > 123 && q.pct < 124);
});

test('cache-first boot: a second open renders from the last lists at once, fresh lists patch in afterwards', { skip }, async () => {
  const mem = new Map();
  const storage = { getItem: (k) => (mem.has(k) ? mem.get(k) : null), setItem: (k, v) => mem.set(k, String(v)), removeItem: (k) => mem.delete(k) };
  const lf1 = adapter.createLiveFetch({ fetchImpl: fixtureFetch(), storage, bootCache: true });
  await (await lf1('http://x/data/snapshot.json')).json();
  lf1.source.bootSave({ now: true });
  assert.ok(mem.has(adapter.BOOT_KEY), 'saved');
  // second open: the network is slow (1 s per request) — the snapshot must not wait for it
  const base = fixtureFetch();
  const slow = async (u, i) => { await sleep(1000); return base(u, i); };
  const lf2 = adapter.createLiveFetch({ fetchImpl: slow, storage, bootCache: true });
  const t0 = Date.now();
  const raw = await (await lf2('http://x/data/snapshot.json')).json();
  assert.ok(Date.now() - t0 < 300, `first data from cache in ${Date.now() - t0} ms`);
  assert.equal(raw.nodes.length, 41);
  assert.equal(lf2.source.bootedFromCache, true);
  const notes = [];
  lf2.source.onUpdate((r) => notes.push(r));
  for (let i = 0; i < 200 && !notes.includes('core'); i++) await sleep(25);
  assert.ok(notes.includes('core'), 'fresh lists arrive and announce themselves');
  assert.equal(lf2.source.bootedFromCache, false);
  // a 401 clears the cache
  const lf3 = adapter.createLiveFetch({ fetchImpl: fixtureFetch({ status: 401 }), storage, bootCache: false, onAuthError: () => {} });
  await lf3('http://x/data/snapshot.json').catch(() => {});
  assert.equal(mem.has(adapter.BOOT_KEY), false, 'cleared after 401');
});

test('activity increments: SSE `activity` → /v1/activity/delta?after=cursor → rows prepended (recent list + opened node)', { skip }, async () => {
  const base = fixtureFetch();
  let cursor = 100;
  const calls = [];
  const f = async (url, init) => {
    const u = String(url);
    if (u.startsWith('/v2/api/v1/activity/delta')) {
      calls.push(u);
      const q = new URL(u, 'http://x').searchParams;
      if (!q.has('after')) return { ok: true, status: 200, json: async () => ({ data: { cursor, more: false, rows: [] } }) };
      const rows = [{ rid: 101, id: 'a', ts: 1790220000, asset: 'dfe4c76d-adbb-4566-93a2-ed180d7a7d6b', source_ip: '203.0.113.9', host: 'example.com', port: 443, protocol: 'tcp', action: 'routed', kind: 'domain', source_intel: { isp: '中国电信' } },
        { rid: 102, id: 'b', ts: 1790220005, asset: 'dfe4c76d-adbb-4566-93a2-ed180d7a7d6b', source_ip: '203.0.113.9', host: 'example.org', port: 443, protocol: 'tcp', action: 'routed', kind: 'domain' }];
      cursor = 102;
      return { ok: true, status: 200, json: async () => ({ data: { cursor, more: false, rows } }) };
    }
    if (u.startsWith('/v2/api/v1/')) return { ok: false, status: 404, json: async () => ({ error: '接口不存在' }) };
    return base(u.replace('/v2/api/legacy/', '/api/team-ops/'), init);
  };
  const lf = adapter.createLiveFetch({ fetchImpl: f, storage: null });
  lf.source.useFleet();
  lf.source.features = ['activity.source_intel'];
  await (await lf('http://x/data/snapshot.json')).json();
  const before = lf.source.raw.activity.rows.length;
  assert.deepEqual(await lf.source.activityDelta(), { n: 0, assets: [] }, 'first call only sets the cursor');
  const r = await lf.source.activityDelta();
  assert.equal(r.n, 2);
  assert.deepEqual(r.assets, ['dfe4c76d-adbb-4566-93a2-ed180d7a7d6b']);
  assert.equal(lf.source.raw.activity.rows[0].id, 'b', 'newest first');
  assert.equal(lf.source.raw.activity.rows.length, Math.min(500, before + 2));
  assert.ok(calls[1].includes('after=100'));
  assert.equal(lf.source.actCursor, 102);
});

test('fleetd delta patches (API.md §12.5): same result as the reference apply(), target never mutated', () => {
  const a = { now: 1.0, assets: [{ uuid: 'a', x: 1, h: { cpu: 1, mem: 2 } }, { uuid: 'b', x: 2 }], relays: { r1: { ok: true } }, gone: 1, n: null };
  const b = { now: 2.0, assets: [{ uuid: 'b', x: 3 }, { uuid: 'c', x: 9 }, { uuid: 'a', x: 1, h: { cpu: 5 } }], relays: { r1: { ok: false }, r2: {} }, n: 5, z: null };
  // the patch fleetd's diff() produces for a → b (delta.rs roundtrip test)
  const p = { now: 2.0, assets: { $uuid: { a: { h: { cpu: 5, $del: ['mem'] } }, b: { x: 3 }, c: { uuid: 'c', x: 9 } }, $order: ['b', 'c', 'a'] }, relays: { r1: { ok: false }, r2: {} }, n: 5, z: null, $del: ['gone'] };
  const frozen = JSON.stringify(a);
  const out = adapter.applyDelta(a, p);
  assert.deepEqual(out, b);
  assert.equal(JSON.stringify(a), frozen, 'target unchanged');
  assert.deepEqual(adapter.applyDelta(a.assets, { $uuid: {}, $removed: ['a'] }), [{ uuid: 'b', x: 2 }], '$removed drops elements');
  assert.equal(adapter.applyDelta(a, { relays: { r1: { ok: true } } }).assets, a.assets, 'unchanged subtrees are shared');
});
