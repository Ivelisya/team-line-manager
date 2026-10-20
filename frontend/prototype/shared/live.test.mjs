import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import * as D from './data.js';
import { createLive, createPatcher, touched } from './live.js';

const SNAP = JSON.parse(readFileSync(new URL('../data/snapshot.json', import.meta.url), 'utf8'));
const setup = (opts) => { const store = D.createStore(SNAP, { clone: true }); return { store, live: createLive(store, opts) }; };

test('ticks advance the simulated clock and emit diffs through the store', () => {
  const { store, live } = setup({ seed: 1, eventRate: 0 });
  const seen = [];
  store.on('change', (d) => seen.push(d.seq));
  const d = live.tick(2);
  assert.equal(store.now(), SNAP.as_of + 2);
  assert.equal(d.now, SNAP.as_of + 2);
  assert.deepEqual(seen, [1]);
  assert.ok(Object.keys(d.nodes).length > 0, 'host metrics change every tick');
});

test('diffs list only values that changed, and match the store', () => {
  const { store, live } = setup({ seed: 2, eventRate: 0 });
  for (let i = 0; i < 20; i++) {
    const d = live.tick(2);
    for (const [id, ch] of Object.entries(d.nodes)) {
      const n = store.node(id);
      for (const [path, v] of Object.entries(ch)) assert.equal(D.get(n, path), v, `${id} ${path}`);
    }
  }
});

test('cadence: host every tick, rates per 10 s collector, TCP every 30 s', () => {
  const { store, live } = setup({ seed: 3, eventRate: 0 });
  let rateTicks = 0, tcpTicks = 0;
  for (let i = 0; i < 30; i++) { // 60 s
    const d = live.tick(2);
    if (touched(d, 't2-01', 'rate.down_bps', 'rate.sample_at')) rateTicks++;
    if (touched(d, 't2-01', 'latency.strip_tcp')) tcpTicks++;
    assert.ok(touched(d, 't2-01', 'host.reported_at'));
  }
  assert.ok(rateTicks >= 5 && rateTicks <= 7, `rate updates in 60 s: ${rateTicks}`);
  assert.ok(tcpTicks >= 1 && tcpTicks <= 3, `tcp probes in 60 s: ${tcpTicks}`);
  const strip = store.node('t2-01').latency.strip_tcp;
  assert.equal(strip.ms.length, 20);
  assert.equal(strip.t_end <= store.now(), true);
});

test('counters only grow', () => {
  const { store, live } = setup({ seed: 4, eventRate: 0 });
  const pick = () => store.nodes().map((n) => [n.traffic.today.down + n.traffic.today.up, n.traffic.month.down + n.traffic.month.up, n.host?.net_total_rx ?? 0, n.host?.uptime_s ?? 0]);
  let prev = pick();
  let prevEvents = store.raw.kpis.events_today;
  for (let i = 0; i < 120; i++) {
    live.tick(2);
    const cur = pick();
    cur.forEach((row, k) => row.forEach((v, j) => assert.ok(v >= prev[k][j], `node ${k} counter ${j} decreased`)));
    assert.ok(store.raw.kpis.events_today >= prevEvents);
    prev = cur; prevEvents = store.raw.kpis.events_today;
  }
  assert.ok(store.kpis().today_business.total > SNAP.kpis.today_business.total);
});

test('fleet 1-minute series appends and keeps its length', () => {
  const { store, live } = setup({ seed: 5, eventRate: 0 });
  const f = store.raw.traffic.fleet_24h;
  const len = f.down.length, t0 = f.t0;
  let appended = 0;
  for (let i = 0; i < 90; i++) { const d = live.tick(2); if (d.fleet['traffic.fleet_24h']) appended += d.fleet['traffic.fleet_24h']; }
  assert.equal(f.down.length, len);
  assert.equal(f.t0, t0 + appended * 60);
  assert.ok(appended >= 2 && appended <= 4);
});

test('status transitions are rare, recover, open/close incidents and drive attention', () => {
  const { store, live } = setup({ seed: 6, eventRate: 3 });
  const events = [];
  for (let i = 0; i < 900; i++) events.push(...live.tick(2).events); // 30 minutes
  const status = events.filter((e) => e.type === 'status');
  const starts = status.filter((e) => e.from === 'ok');
  assert.ok(starts.length >= 5 && starts.length <= 40, `transitions: ${starts.length}`);
  assert.ok(status.some((e) => e.to === 'ok'), 'some recover');
  const live1 = store.incidents().filter((i) => i.live);
  assert.ok(live1.some((i) => !i.open && i.resolved_at), 'resolved incidents');
  // attention agrees with the rule at every point
  for (const n of store.nodes()) {
    const want = n.status.code === 'fail' || (n.status.code === 'degraded' && store.now() - n.status.since >= 300);
    assert.equal(store.attentionOf(n), want || !!n.status.attention);
  }
});

test('eventRate 0 → no transitions; triggerEvent forces one', () => {
  const { store, live } = setup({ seed: 7, eventRate: 0 });
  for (let i = 0; i < 300; i++) assert.equal(live.tick(2).events.filter((e) => e.type === 'status').length, 0);
  const d = live.triggerEvent();
  assert.equal(d.events.filter((e) => e.type === 'status').length, 1);
  assert.equal(store.kpis().normal.ok, 40);
});

test('deterministic with the same seed', () => {
  const a = setup({ seed: 9, eventRate: 2 }), b = setup({ seed: 9, eventRate: 2 });
  for (let i = 0; i < 50; i++) { a.live.tick(2); b.live.tick(2); }
  assert.equal(a.store.node('t2-01').rate.down_bps, b.store.node('t2-01').rate.down_bps);
  assert.deepEqual(a.store.kpis().normal, b.store.kpis().normal);
});

test('patcher and touched helpers', () => {
  const p = createPatcher();
  assert.equal(p.changed('t1:rate', '12.4 Mbps'), true);
  assert.equal(p.changed('t1:rate', '12.4 Mbps'), false);
  assert.equal(p.changed('t1:rate', '12.5 Mbps'), true);
  const d = { nodes: { a: { 'rate.down_bps': 1, 'host.cpu_pct': 2 } } };
  assert.equal(touched(d, 'a', 'rate.'), true);
  assert.equal(touched(d, 'a', 'latency.'), false);
  assert.equal(touched(d, 'b'), false);
});
