import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import * as D from './data.js';
import * as f from './format.js';

const SNAP = JSON.parse(readFileSync(new URL('../data/snapshot.json', import.meta.url), 'utf8'));
const fresh = (opts = {}) => D.createStore(SNAP, { clone: true, ...opts });

test('store: entities and indexes', () => {
  const s = fresh();
  assert.equal(s.nodes().length, 41);
  assert.equal(s.relays().length, 3);
  assert.equal(s.entries().length, 41);
  assert.equal(s.node('t1-31').name, '团队1-31');
  assert.equal(s.node('团队1-31').id, 't1-31');
  assert.equal(s.relay('线路机1').id, 'r1');
  assert.equal(s.node('r1').host, s.relay('r1').host, 'relay row and node row share the live host object');
  assert.equal(s.entryOf(s.node('t1-31')).id, '线路机3:20007');
  assert.equal(s.now(), SNAP.as_of);
  assert.equal(f.now(), SNAP.as_of, 'format clock follows the store');
});

test('normalised node fields', () => {
  const s = fresh();
  const proxy = s.node('t1-13');
  assert.equal(proxy.isProxy, true);
  assert.equal(proxy.hasHost, false);
  assert.ok(proxy.caps.includes('no_host'));
  const relay = s.node('r2');
  assert.equal(relay.isRelay, true);
  assert.ok(s.node('t1-31').haystack.includes('198.51.100.41'));
  assert.equal(s.nodes().filter((n) => n.hasHost).length, 33);
});

test('kpis are derived from nodes and match the snapshot', () => {
  const s = fresh();
  const k = s.kpis();
  assert.deepEqual(k.normal, SNAP.kpis.normal);
  assert.equal(k.attention, 0);
  assert.equal(k.business_rate.down_bps, SNAP.kpis.business_rate.down_bps);
  assert.equal(k.business_rate.up_bps, SNAP.kpis.business_rate.up_bps);
  assert.equal(k.today_business.total, SNAP.kpis.today_business.total);
  assert.equal(k.month_business.total, SNAP.kpis.month_business.total);
  assert.equal(k.monthly_cost.cents, 128135);
  assert.equal(k.monthly_cost.services, 41);
  assert.equal(k.due_7d.count, 0);
  assert.deepEqual([k.due_7d.nearest.name, k.due_7d.nearest.days], ['线路机1', 8]);
});

test('groups match snapshot team summary', () => {
  const s = fresh();
  const g = s.groups();
  for (const row of SNAP.groups) {
    const mine = g.find((x) => x.team === row.team);
    for (const key of ['count', 'active', 'today_bytes', 'monthly_cents', 'rate_down_bps', 'rate_up_bps']) assert.equal(mine[key], row[key], `${row.team}.${key}`);
    assert.deepEqual(mine.month, row.month);
  }
});

test('due tiers recompute with the clock (billing zone calendar)', () => {
  const s = fresh();
  assert.deepEqual(s.dueOf(s.node('r1')), { days: 8, tier: 'normal', due7: false });
  s.setNow(SNAP.as_of + 86400 * 2); // two days later → 6 days left in LA
  assert.deepEqual(s.dueOf(s.node('r1')), { days: 6, tier: 'soon', due7: true });
  assert.equal(s.kpis().due_7d.count, 1);
  s.setNow(SNAP.as_of + 86400 * 9);
  assert.equal(s.dueOf(s.node('r1')).tier, 'expired');
  assert.equal(s.kpis().due_7d.items[0].days, -1);
});

test('replays populate the attention list with real past moments', () => {
  const s = fresh();
  let got = null;
  s.on('change', (d) => { got = d; });
  s.applyReplay(0); // 09-23 09:11 团队1-31 入口认证失败
  assert.ok(got.nodes['t1-31']);
  const att = s.attention();
  assert.equal(att.length, 1);
  assert.equal(att[0].name, '团队1-31');
  assert.equal(att[0].word, '故障');
  assert.equal(att[0].check_title, '实际入口到出口');
  assert.equal(att[0].reason_word, '入口认证失败');
  assert.equal(att[0].duration_s, 376);
  assert.ok(att[0].incident);
  assert.equal(s.kpis().normal.fail, 1);
  s.applyReplay(4); // 16 hosts unknown
  assert.equal(s.unknown().length, 16);
  assert.equal(s.attention().length, 0);
  s.clearReplay();
  assert.equal(s.kpis().normal.ok, 41);
  assert.equal(s.incidents({ open: true }).length, 0);
});

test('attention rule: fail at once, degraded only after 5 minutes', () => {
  const s = fresh();
  const n = s.node('t1-05');
  n.status = { ...n.status, code: 'degraded', since: s.now() - 120, attention: false };
  assert.equal(s.attentionOf(n), false);
  n.status.since = s.now() - 301;
  assert.equal(s.attentionOf(n), true);
  n.status = { ...n.status, code: 'fail', since: s.now() };
  assert.equal(s.attentionOf(n), true);
});

test('onboarding batch is normalised for the matrix', () => {
  const s = fresh();
  const { history, batch } = s.onboarding();
  assert.equal(history.length, 22);
  assert.equal(batch.rows.length, 8);
  const byName = Object.fromEntries(batch.rows.map((r) => [r.name, r]));
  assert.equal(byName['团队1-43'].stateWord, '已停止（风险）');
  assert.equal(byName['团队1-43'].risk.message, '主机公钥与上次不一致，已停止，未发送密码');
  assert.equal(byName['团队1-42'].currentStep.code, 'P6');
  assert.equal(byName['团队1-42'].phaseCells.find((c) => c.key === 'reinstall').status, 'failed');
  assert.equal(byName['团队1-44'].phaseCells.find((c) => c.key === 'reinstall').status, 'skipped');
  assert.equal(byName['团队1-41'].attention, true);
  assert.deepEqual(batch.phaseSummary.find((p) => p.key === 'preflight'), { key: 'preflight', done: 7, applicable: 8 });
  assert.equal(D.aggregateSteps(['succeeded', 'running', 'queued']), 'running');
  assert.equal(D.aggregateSteps(['succeeded', 'skipped']), 'succeeded');
  assert.equal(D.aggregateSteps(['queued', 'queued']), 'queued');
});

test('stress duplicator: 200 unique, plausible rows', () => {
  const s = fresh().stressed(200);
  const ns = s.nodes();
  assert.equal(ns.length, 200);
  assert.equal(new Set(ns.map((n) => n.id)).size, 200);
  assert.equal(new Set(ns.map((n) => n.name)).size, 200);
  assert.equal(new Set(ns.map((n) => n.ip)).size, 200);
  const extra = ns.filter((n) => n.stress);
  assert.equal(extra.length, 159);
  assert.ok(extra.every((n) => /^(192\.0\.2|198\.18)\./.test(n.ip)), 'synthetic IPs stay in reserved ranges');
  assert.ok(extra.some((n) => n.status.code !== 'ok'), 'some rows exercise non-normal states');
  const again = fresh().stressed(200);
  assert.deepEqual(again.nodes().map((n) => n.name), ns.map((n) => n.name), 'deterministic with the same seed');
});

test('node table config: tokens and segments', () => {
  const s = fresh();
  const tk = D.nodeTokens(s);
  const count = (fn) => s.nodes().filter(fn).length;
  assert.equal(count((n) => tk.cap.test(n, 'proxy')), 8);
  assert.equal(count((n) => tk.relay.test(n, '2')), 16);
  assert.equal(count((n) => tk.team.test(n, '团队2')), 2);
  assert.equal(count((n) => tk.provider.test(n, 'NodeMach')), 8);
  assert.equal(count((n) => tk.ip.test(n, '198.51.100.4*')), 9);
  assert.equal(count((n) => tk.due.test(n, '<7d')), 0);
  assert.equal(count((n) => tk.due.test(n, '<=15')), 3);
  assert.equal(count((n) => tk.status.test(n, '正常')), 41);
  const segs = D.nodeSegments(s);
  assert.deepEqual(segs.map((x) => x.label), ['全部', '需处理', '未知', '7 天内到期', '尚未接入']);
});

test('palette items cover nodes, relays and entries', () => {
  const s = fresh();
  const opened = [];
  const items = D.paletteItems(s, (k, id) => opened.push([k, id]));
  assert.equal(items.filter((i) => i.group === '节点').length, 41);
  const n21 = items.find((i) => i.title === '团队1-21');
  assert.ok(n21.keywords.includes('21'));
  assert.ok(n21.keywords.every((k) => typeof k === 'string'));
  n21.run();
  assert.deepEqual(opened, [['node', 't1-21']]);
});

test('lazy files load through the injected fetch and are cached', async () => {
  let calls = 0;
  const fakeFetch = async (url) => {
    calls++;
    const path = new URL(url).pathname;
    const body = readFileSync(new URL(`../data/${path.split('/data/')[1]}`, import.meta.url), 'utf8');
    return { ok: true, json: async () => JSON.parse(body) };
  };
  const s = fresh({ fetch: fakeFetch, base: new URL('../data/', import.meta.url) });
  const s1 = await s.series('t2-01', 'traffic_5m');
  assert.equal(s1.step, 300);
  assert.ok(Array.isArray(s1.down));
  const s2 = await s.series('t1-01', 'traffic_1m');
  assert.equal(s2.step, 60, 't0/step filled from the file header');
  assert.equal(typeof s2.t0, 'number');
  await s.series('t1-01', 'traffic_5m');
  assert.equal(calls, 2);
  const det = await s.detail('t1-31');
  assert.ok(det.checks);
});
