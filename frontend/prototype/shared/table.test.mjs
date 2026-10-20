import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import * as D from './data.js';
import { createTable, parseQuery, safeStorage, virtualWindow } from './table.js';

const SNAP = JSON.parse(readFileSync(new URL('../data/snapshot.json', import.meta.url), 'utf8'));
function memStorage() {
  const m = new Map();
  return { getItem: (k) => (m.has(k) ? m.get(k) : null), setItem: (k, v) => m.set(k, String(v)), removeItem: (k) => m.delete(k), _m: m };
}
function setup(extra = {}) {
  const store = D.createStore(SNAP, { clone: true });
  const storage = memStorage();
  const t = createTable(D.nodeTableConfig(store, { storage, ...extra }));
  return { store, t, storage };
}

test('parseQuery: tokens, negation, quoting, IPv6 stays text', () => {
  const q = parseQuery('team:团队1 relay:线路机2,线路机3 -status:ok "ATT 基础" 2001:db8::1 21', ['team', 'relay', 'status']);
  assert.deepEqual(q.tokens.map((x) => [x.key, x.values, x.neg]), [['team', ['团队1'], false], ['relay', ['线路机2', '线路机3'], false], ['status', ['ok'], true]]);
  assert.deepEqual(q.text, ['att 基础', '2001:db8::1', '21']);
});

test('default order is 编号 (team order then number); sort toggles and multi-sort', () => {
  const { t } = setup();
  const rows = t.rows();
  assert.equal(rows.length, 41);
  assert.equal(rows[0].name, '团队1-01');
  assert.equal(rows.at(-1).name, '线路机3');
  t.setSort('rate');
  assert.equal(t.sortOf('rate').dir, 'desc', 'rate defaults to descending');
  assert.equal(t.rows()[0].name, '团队2-01');
  t.setSort('rate');
  assert.equal(t.sortOf('rate').dir, 'asc');
  t.setSort('team');
  t.setSort('cost', { multi: true });
  assert.deepEqual(t.state().sort, [{ key: 'team', dir: 'asc' }, { key: 'cost', dir: 'desc' }]);
  const t1 = t.rows().filter((r) => r.team === '团队1');
  for (let i = 1; i < t1.length; i++) assert.ok(t1[i - 1].billing.monthly_cents >= t1[i].billing.monthly_cents);
});

test('nulls sort last in both directions', () => {
  const { t } = setup();
  t.setSort('cpu');
  assert.equal(t.rows().at(-1).hasHost, false);
  t.setSort('cpu');
  assert.equal(t.rows().at(-1).hasHost, false);
});

test('facets, tokens, free text and segment counts', () => {
  const { t } = setup();
  t.toggleFilter('relay', '线路机2');
  assert.equal(t.count(), 16);
  const teamFacet = t.facetCounts('team');
  const expected = {};
  for (const n of SNAP.nodes) if (n.relay === '线路机2') expected[n.team] = (expected[n.team] ?? 0) + 1;
  assert.deepEqual(Object.fromEntries(teamFacet.map((x) => [x.value, x.count])), expected);
  assert.ok(teamFacet[0].count >= teamFacet.at(-1).count, 'sorted by count');
  const relayFacet = t.facetCounts('relay');
  assert.equal(relayFacet.find((x) => x.value === '线路机1').count, 13, 'a facet ignores its own filter');
  t.setQuery('cap:proxy');
  assert.equal(t.count(), t.rows().filter((r) => r.isProxy).length);
  t.setQuery('198.51.100.41');
  assert.equal(t.count(), 0, 'AND with relay filter: 团队1-31 is on 线路机3');
  t.clearFilters();
  assert.equal(t.rows()[0].name, '团队1-31');
  t.setQuery('');
  const segs = t.segmentCounts();
  assert.deepEqual(segs.map((x) => [x.label, x.count]), [['全部', 41], ['需处理', 0], ['未知', 0], ['7 天内到期', 0], ['尚未接入', 0]]);
  assert.equal(t.chips().length, 0);
  t.toggleFilter('team', '团队2');
  t.setQuery('relay:线路机1');
  assert.deepEqual(t.chips().map((c) => c.kind), ['facet', 'token']);
  t.chips()[1].remove();
  assert.equal(t.query(), '');
});

test('segment follows live data (replay)', () => {
  const { store, t } = setup();
  t.setSegment('attention');
  assert.equal(t.count(), 0);
  store.applyReplay(0);
  t.invalidate();
  assert.equal(t.count(), 1);
  assert.equal(t.rows()[0].name, '团队1-31');
  t.setSegment(null);
  t.setSort('status');
  assert.equal(t.rows()[0].name, '团队1-31', 'status ascending = worst first');
});

test('URL state round-trip', () => {
  const { t } = setup();
  t.setSegment('due7');
  t.toggleFilter('status', 'degraded');
  t.toggleFilter('team', '团队1');
  t.setQuery('relay:线路机2');
  t.setSort('rate');
  const p = t.toParams();
  assert.equal(p.get('status'), 'due7,degraded');
  assert.equal(p.get('team'), '团队1');
  assert.equal(p.get('q'), 'relay:线路机2');
  assert.equal(p.get('sort'), '-rate');
  const { t: t2 } = setup();
  t2.fromParams(p.toString());
  assert.deepEqual(t2.state().filters, { status: ['degraded'], team: ['团队1'] });
  assert.equal(t2.segment(), 'due7');
  assert.deepEqual(t2.state().sort, [{ key: 'rate', dir: 'desc' }]);
  const { t: t3 } = setup();
  t3.fromParams('status=attention');
  assert.equal(t3.segment(), 'attention');
});

test('selection: toggle, range, all, header state', () => {
  const { t } = setup();
  const ids = t.rows().map((r) => r.id);
  t.toggleSelect(ids[2]);
  t.toggleSelect(ids[6], { range: true });
  assert.equal(t.selectedCount(), 5);
  assert.equal(t.selectionState(), 'some');
  t.selectAll();
  assert.equal(t.selectionState(), 'all');
  t.clearSelection();
  assert.equal(t.selectionState(), 'none');
});

test('keyboard cursor: j/k, edges, recovery when the row disappears', () => {
  const { t } = setup();
  assert.equal(t.move(1), 't1-01');
  assert.equal(t.move(1), 't1-02');
  assert.equal(t.move(-5), 't1-01');
  assert.equal(t.moveTo('last'), 'r3');
  t.setCursor('t1-05');
  t.setQuery('-team:团队9 -relay:nothing'); // still everything
  t.toggleFilter('relay', '线路机1'); // t1-05 is on another relay
  const next = t.move(1);
  assert.ok(next);
  t.toggleCursor();
  assert.equal(t.isSelected(next), true);
  const keys = t.keyBindings({ open: () => {} });
  assert.ok(keys.find((k) => k.combo === 'x'));
});

test('grouping in facet order and collapsed state', () => {
  const { t } = setup();
  t.setGroupBy('relay');
  const g = t.groups();
  assert.deepEqual(g.map((x) => [x.key, x.count]), [['线路机1', 13], ['线路机2', 16], ['线路机3', 12]]);
  t.toggleGroup('线路机2');
  assert.equal(t.groups()[1].collapsed, true);
});

test('preferences persist in storage; broken storage never throws', () => {
  const { t, storage } = setup();
  t.setDensity('compact');
  t.setView('cards');
  t.toggleColumn('provider', true);
  t.saveView('我的视图');
  const t2 = createTable(D.nodeTableConfig(D.createStore(SNAP, { clone: true }), { storage }));
  assert.equal(t2.density(), 'compact');
  assert.equal(t2.view(), 'cards');
  assert.ok(t2.views().some((v) => v.name === '我的视图'));
  assert.ok(t2.views().some((v) => v.name === '需处理' && v.builtin));
  const broken = { getItem() { throw new Error('denied'); }, setItem() { throw new Error('quota'); }, removeItem() { throw new Error('x'); } };
  const s = safeStorage(broken);
  assert.equal(s.get('a', 'fallback'), 'fallback');
  assert.equal(s.set('a', 1), false);
  const t3 = createTable(D.nodeTableConfig(D.createStore(SNAP, { clone: true }), { storage: broken }));
  t3.setDensity('comfortable');
  assert.equal(t3.density(), 'comfortable');
});

test('applyView restores a preset', () => {
  const { t } = setup();
  t.applyView('家宽代理');
  assert.equal(t.count(), 8);
  t.reset();
  assert.equal(t.count(), 41);
});

test('resort throttle keeps order stable under live data', () => {
  let now = 0;
  const { store, t } = setup({ resortThrottleMs: 5000, clock: () => now });
  t.setSort('rate');
  const first = t.rows().map((r) => r.id);
  const last = store.node(first.at(-1));
  last.rate.down_bps = 1e9; // would jump to the top
  t.invalidate();
  now = 1000;
  assert.deepEqual(t.rows().map((r) => r.id), first, 'within 5 s: same order');
  now = 6000;
  t.invalidate();
  assert.equal(t.rows()[0].id, last.id, 'after 5 s: re-sorted');
});

test('virtualWindow', () => {
  assert.deepEqual(virtualWindow({ count: 200, rowHeight: 40, scrollTop: 400, viewport: 400, overscan: 2 }), { start: 8, end: 22, padTop: 320, padBottom: 7120 });
});
