// crosscheck.test.mjs — the adapter must compute what build.py computes from the same database copy,
// except for the explained paths listed in crosscheck.mjs (different source, API resolution, fake values).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { crosscheck, hasCrosscheck, SAMPLE } from './crosscheck.mjs';

const skip = !hasCrosscheck && 'crosscheck output missing: run staging/crosscheck_build.sh on a395';

test('five sample nodes: identical to build.py except explained paths', { skip }, async () => {
  const { rows } = await crosscheck({ ids: SAMPLE });
  const bad = rows.filter((r) => !r.why);
  assert.deepEqual(bad, [], 'unexplained differences');
  // the explained set is small and specific
  const kinds = new Set(rows.map((r) => r.why));
  assert.ok(kinds.size <= 8);
});

test('all 41 nodes and the fleet aggregates: no unexplained difference', { skip }, async () => {
  const { rows, top, snap, ref } = await crosscheck();
  assert.deepEqual(rows.filter((r) => !r.why).map((r) => `${r.id} ${r.path}: ${JSON.stringify(r.ref)} → ${JSON.stringify(r.live)}`), []);
  const unexplained = Object.values(top).flat().filter((r) => !r.why).map((r) => `${r.path}: ${JSON.stringify(r.ref)} → ${JSON.stringify(r.live)}`);
  assert.deepEqual(unexplained, []);
  // derived fields the owner cares about, compared explicitly
  const byId = new Map(ref.nodes.map((n) => [n.id, n]));
  for (const n of snap.nodes) {
    const r = byId.get(n.id);
    assert.equal(n.status.code, r.status.code, `${n.id} status`);
    assert.equal(n.status.attention, r.status.attention, `${n.id} attention`);
    assert.equal(n.billing.due_days, r.billing.due_days, `${n.id} due days`);
    assert.equal(n.billing.tz_badge, r.billing.tz_badge, `${n.id} tz badge`);
    assert.deepEqual(n.rate, r.rate, `${n.id} rate`);
    assert.deepEqual(n.traffic.today, r.traffic.today, `${n.id} today`);
    assert.deepEqual(n.traffic.month, r.traffic.month, `${n.id} month`);
    assert.deepEqual(n.traffic.last24h, r.traffic.last24h, `${n.id} last24h`);
    assert.equal(n.traffic.coverage_start, r.traffic.coverage_start, `${n.id} coverage start`);
    assert.deepEqual(n.bandwidth, r.bandwidth, `${n.id} bandwidth`);
    assert.deepEqual(n.capabilities, r.capabilities, `${n.id} capabilities`);
  }
  assert.deepEqual(snap.groups, ref.groups);
  assert.deepEqual(snap.upcoming_due, ref.upcoming_due);
  assert.deepEqual(snap.kpis.due_7d, ref.kpis.due_7d);
  assert.deepEqual(snap.status_replays, ref.status_replays);
});
