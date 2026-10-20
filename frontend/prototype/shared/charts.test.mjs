import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import * as C from './charts.js';

const SNAP = JSON.parse(readFileSync(new URL('../data/snapshot.json', import.meta.url), 'utf8'));

test('niceTicks: round steps, no float noise', () => {
  assert.deepEqual(C.niceTicks(0, 0.7, 4).ticks, [0, 0.2, 0.4, 0.6, 0.8]);
  assert.deepEqual(C.niceTicks(-3, 17, 5).ticks, [-5, 0, 5, 10, 15, 20]);
});

test('yTicks: one unit per axis, numbers only, fixed decimals', () => {
  const r = C.yTicks(0, 170343778, 'rate'); // fleet peak down
  assert.equal(r.unit, 'Mbps');
  assert.deepEqual(r.labels, ['0', '50', '100', '150', '200']);
  const b = C.yTicks(0, 3.3 * 1024 ** 3, 'bytes');
  assert.equal(b.unit, 'GiB');
  assert.deepEqual(b.labels, ['0', '1', '2', '3', '4']);
  const m = C.yTicks(-19938179, 170343778, 'rate');
  assert.equal(m.labels[0], '50', 'mirror: below-zero ticks print as absolute values');
  const s = C.yTicks(0, 2400, 'ms');
  assert.equal(s.unit, 's');
  assert.ok(s.labels.every((l) => /^\d\.\d$/.test(l)), 'same decimals on every tick');
});

test('timeTicks: Beijing-aligned, midnight shows the date', () => {
  const f = SNAP.traffic.fleet_24h;
  const tt = C.timeTicks(f.t0, f.t0 + (f.down.length - 1) * 60, 6);
  assert.equal(tt.step, 21600);
  assert.deepEqual(tt.labels, ['18:00', '09-23', '06:00', '12:00']);
});

test('columnar + bytesToBps + insertGaps + peak', () => {
  const f = SNAP.traffic.fleet_24h;
  const [xs, down, up] = C.columnar(f, ['down', 'up'], { toRate: true });
  assert.equal(xs.length, 1439);
  assert.equal(down[0], (f.down[0] * 8) / 60);
  const pk = C.peakOf(xs, down);
  assert.equal(pk.x, f.peak_down.t);
  assert.equal(Math.round(pk.y), f.peak_down.bps);
  assert.deepEqual(C.insertGaps([[0, 60, 120, 480, 540], [1, 2, 3, 4, 5]]), [[0, 60, 120, 180, 480, 540], [1, 2, 3, null, 4, 5]]);
  assert.ok(up.length === 1439);
});

test('sparkPath breaks on nulls; flat zero sits on the baseline', () => {
  const p = C.sparkPath([1, 2, null, 4, 3], { width: 72, height: 20 });
  assert.equal((p.d.match(/M/g) ?? []).length, 2);
  const z = C.sparkPath([0, 0, 0], { width: 72, height: 20, pad: 1.5 });
  assert.match(z.d, /18\.5/);
  assert.equal(C.sparkPath([null, null]).d, '');
});

test('SVG renderers use CSS variables, not hex', () => {
  const n = SNAP.nodes.find((x) => x.id === 't1-01');
  const svgs = [
    C.sparkline(n.host_spark.cpu_pct),
    C.mirrorSparkline(SNAP.kpis.business_rate.spark.down_bps, SNAP.kpis.business_rate.spark.up_bps),
    C.tracker(n.availability.tracker30),
    C.latencyStrip(n.latency.strip_tcp, { limits: n.limits }),
    C.latencyStrip(n.latency.strip_egress, { kind: 'egress', limits: n.limits, mode: 'bars' }),
    C.stackBar(C.statusSegments({ ok: 36, degraded: 1, fail: 1 })),
  ];
  for (const s of svgs) {
    assert.match(s, /^<svg /);
    assert.doesNotMatch(s, /#[0-9a-f]{3,6}\b/i);
    assert.match(s, /var\(--/);
  }
  assert.match(svgs[2], /width="119"/, 'tracker is 30×3 + 29×1 = 119 px');
  assert.equal((svgs[2].match(/<rect/g) ?? []).length, 30);
  assert.equal((svgs[3].match(/<rect/g) ?? []).length, 20);
});

test('tracker and latency tooltips', () => {
  const n = SNAP.nodes.find((x) => x.id === 't1-01');
  const t = n.availability.tracker30;
  assert.equal(C.trackerTip(t, 0), '8/25 · 未监控');
  const i = t.state.findIndex((s) => s === 'minor');
  assert.match(C.trackerTip(t, i), /^\d+\/\d+ · 99\.\d\d% · \d 次故障$/);
  assert.match(C.latencyTip(n.latency.strip_tcp, 19), /^\d\d:\d\d:\d\d · \d+ ms$/);
  assert.match(C.latencyCaption(n.latency.strip_tcp), /^近 9 分钟 · 中位 \d+ ms · 丢包 0%$/);
});

test('colour helpers', () => {
  assert.equal(C.statusColor('fail'), 'var(--bad-dot)');
  assert.equal(C.statusColor('unknown'), 'var(--unknown-ring)');
  assert.equal(C.seqColor(45, [0, 100]), 'var(--chart-seq-3)');
  assert.equal(C.seqColor(100, [95, 100], { invert: true }), 'var(--chart-seq-1)');
  assert.equal(C.seqColor(null, [0, 1]), 'var(--border-subtle)');
  assert.deepEqual(C.microBar(85, { limit: 90 }), { pct: 85, tone: 'warn', missing: false });
  assert.equal(C.cssRef('--chart-1'), 'var(--chart-1)');
  assert.equal(C.resolveColor(null, 'var(--x, #123)', '#000'), '#123');
});
