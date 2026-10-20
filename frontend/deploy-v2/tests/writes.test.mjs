// writes.test.mjs — the write layer of the v2 pages (prototype/original-refined/js): where each write goes (fleetd native
// §9 / legacy / nowhere after decommission), which actions are wired, the relay-op progress, the live onboarding model
// (fleetd /onboard/* batch view → the pipeline page model) and the CSV check of the new-batch dialog.  No DOM needed.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const APP = '../../../prototype/original-refined/js/';
globalThis.TEAMOPS_LIVE = true;
globalThis.TEAMOPS_WRITES = true;

/** A TEAMOPS stand-in: fleetd with the given features; writes / gets recorded; SSE listeners callable. */
function fakeFleet({ features = [], decommissioned = false, get = async () => null } = {}) {
  const calls = [];
  const listeners = new Set();
  globalThis.TEAMOPS = {
    live: true, decommissioned,
    fleet: {
      available: true, detected: true, system: { version: '0.5.0', features },
      has: (f) => features.includes(f),
      get: async (path, query) => { calls.push(['GET', path, query ?? null]); return get(path, query); },
      write: async (method, path, body) => { calls.push([method, path, body]); return { ok: true, op_id: 'op-1', relay: '线路机3', generation: 42, status: 'pending' }; },
      on: (fn) => { listeners.add(fn); return () => listeners.delete(fn); },
    },
    source: { noteAsset() {} },
  };
  return { calls, emit: (e) => { for (const fn of listeners) fn(e); } };
}

test('class a writes: fleetd native (§9.2) when it lists the feature, else legacy; after decommission never legacy', async () => {
  const W = await import(`${APP}live-writes.js`);
  const edit = { uuid: 'u-1', expected_revision: 7, name: '团队1-01', price: '12.50', due_date: '2026-10-23', note: 'n', renewal: false, renewal_note: '' };
  assert.deepEqual(W.nativeRoute('/assets/edit', edit), ['assets.write', 'PATCH', 'assets/u-1', { expected_revision: 7, name: '团队1-01', price: '12.50', due_date: '2026-10-23', note: 'n' }]);
  assert.deepEqual(W.nativeRoute('/assets/edit', { ...edit, renewal: true, renewal_note: '账单 88' }), ['renewals', 'POST', 'assets/u-1/renewals', { new_due: '2026-10-23', amount_cents: 1250, note: '账单 88' }]);
  assert.deepEqual(W.nativeRoute('/node/profile', { uuid: 'u-1', expected_revision: 3, profile: { thresholds: { cpu_percent: 80 }, maintenance_until: 0 } }), ['profile', 'PUT', 'nodes/u-1/profile', { expected_revision: 3, maintenance_until: 0, thresholds: { cpu_percent: 80 } }]);
  assert.deepEqual(W.nativeRoute('/incidents/ack', { id: 55, note: '' }), ['incidents.ack', 'POST', 'incidents/55/ack', { note: '' }]);
  assert.deepEqual(W.nativeRoute('/api-keys/revoke', { id: 9 }), ['api-keys', 'POST', 'api-keys/9/revoke', {}]);
  assert.equal(W.nativeRoute('/fleet/bind', {}), null);

  fakeFleet({ features: ['assets.write'] });
  assert.equal(W.writeTarget('/assets/edit', edit), 'native');
  assert.equal(W.writeTarget('/assets/edit', { ...edit, renewal: true }), 'legacy', 'renewals not listed yet');
  const f = fakeFleet({ features: ['assets.write'] });
  await W.postOps('/assets/edit', edit);
  assert.deepEqual(f.calls, [['PATCH', 'assets/u-1', { expected_revision: 7, name: '团队1-01', price: '12.50', due_date: '2026-10-23', note: 'n' }]]);

  // old system gone (fleet/API.md §12.2): the old POST paths, same bodies, under /v2/api/v1/legacy-write/
  const g = fakeFleet({ features: ['legacy.write', 'assets.edit'], decommissioned: true });
  assert.equal(W.writeTarget('/assets/edit', { ...edit, renewal: true }), 'legacy-write');
  await W.postOps('/assets/edit', edit);
  await W.postOps('/api-keys/revoke', { id: 9 });
  assert.deepEqual(g.calls, [['POST', 'legacy-write/assets/edit', edit], ['POST', 'legacy-write/api-keys/revoke', { id: 9 }]]);

  fakeFleet({ features: [], decommissioned: true });
  assert.equal(W.writeTarget('/assets/edit', edit), null);
  await assert.rejects(W.postOps('/assets/edit', edit), /新后台尚未开放/);
});

test('class b wiring: relay actions run only on a write build with the fleetd capability; the rest never', async () => {
  const L = await import(`${APP}live-mode.js`);
  fakeFleet({ features: ['entries', 'ops'] });
  assert.equal(L.isWired('lines.entry.disable'), true);
  assert.equal(L.isWired('drawer.rotate'), true);
  assert.equal(L.isWired('lines.relay.restartXray'), false, 'restart_xray not listed');
  assert.equal(L.isWired('lines.entry.verify'), false, 'no API planned');
  assert.equal(L.isWired('behavior.rules.apply'), false);
  fakeFleet({ features: ['policies', 'onboard', 'restart_xray'] });
  assert.equal(L.isWired('behavior.rules.saveDraft'), true);
  assert.equal(L.isWired('onboarding.row'), true);
  assert.equal(L.isWired('palette.restartXray'), true);
  globalThis.TEAMOPS_WRITES = false;
  try { assert.equal(L.isWired('onboarding.row'), false, 'read-only build'); assert.equal(L.isWired('drawer.editAsset'), false); }
  finally { globalThis.TEAMOPS_WRITES = true; }
});

test('relay op: 202 pending → SSE ops poke → GET /ops/{id} until applied; errors come from the relayd receipt', async () => {
  const O = await import(`${APP}live-ops.js`);
  let n = 0;
  const f = fakeFleet({ features: ['entries', 'ops'], get: async (path) => (path === 'ops/op-1' ? (++n < 2 ? { op_id: 'op-1', status: 'pending' } : { op_id: 'op-1', status: 'applied', relay: '线路机3', generation: 43 }) : null) });
  const seen = [];
  const p = O.waitOp({ op_id: 'op-1', status: 'pending' }, { onUpdate: (o) => seen.push(o.status), timeoutMs: 10000 });
  for (let i = 0; i < 20 && n < 2; i++) { await new Promise((r) => setTimeout(r, 20)); f.emit({ resource: 'ops', path: '/v2/api/v1/ops/op-1' }); }
  const fin = await p;
  assert.equal(fin.status, 'applied');
  assert.deepEqual(seen, ['pending', 'applied']);
  assert.equal(O.opError({ status: 'rejected', result: { results: [{ ok: true }, { ok: false, error: 'base_changed' }] } }), 'base_changed');
  assert.equal(O.opError({ status: 'expired', error: 'relayd 60 秒内没有取走' }), 'relayd 60 秒内没有取走');
  assert.deepEqual(O.ENTRY_ACTION, { enable: 'enable', disable: 'disable', reset: 'reset_traffic', rename: 'rename', rotate: 'rotate_uuid' });
});

test('entry op request: rotate sends confirm "<relay>:<port>"; rename carries the remark; reset → reset_traffic', async () => {
  const O = await import(`${APP}live-ops.js`);
  const e = { relay: '线路机3', port: 20007 };
  const path = `relays/${encodeURIComponent('线路机3')}/entries/20007/actions`;
  assert.deepEqual(O.entryRequest(e, 'rotate'), [path, { action: 'rotate_uuid', confirm: '线路机3:20007' }]);
  assert.deepEqual(O.entryRequest(e, 'rename', { remark: '团队1-07' }), [path, { action: 'rename', remark: '团队1-07' }]);
  assert.deepEqual(O.entryRequest(e, 'reset'), [path, { action: 'reset_traffic' }]);
  assert.deepEqual(O.entryRequest(e, 'disable'), [path, { action: 'disable' }]);
});

/** A fleetd onboard batch view (WIRING.md §3 shape), trimmed. */
function batchView(over = {}) {
  const steps = [['P1', 'preflight', '预检', '30–45 s'], ['P2', 'bootstrap_key', '推密钥', '5–10 s'], ['P13', 'relay_attach', '线路机接入', '每批 30–60 s'], ['P16', 'activate', '上线交付', '< 5 s']];
  const cells = (st) => steps.map(([code], i) => ({ step: code, status: st[i], duration_s: st[i] === 'succeeded' ? 10 : st[i] === 'running' ? 4 : null, ...(st[i] === 'waiting_input' ? { detail: '上架密钥登录被拒，这一行也没有给密码文件' } : {}) }));
  return {
    id: 'b-0123456789', demo: false, title: '新机 ×3（ATTVPS ×3）', type: 'residential', type_word: '家宽节点', status: 'running', status_word: '进行中',
    recipe: 'shadowsocks-rust ssserver 1.25.0', canary: { enabled: true, passed: true, node: '团队1-41' },
    options: { relay_window_s: 30, concurrency: 5 }, operator: 'admin', confirmed_count: 3, plan_hash: 'abcdef0123456789',
    started_at: 1000, finished_at: null, elapsed_s: 60, eta_s: null,
    phases: [{ key: 'preflight', name: '预检', steps: ['P1'] }, { key: 'keys', name: '密钥', steps: ['P2'] }, { key: 'relay', name: '线路机', steps: ['P13'] }, { key: 'deliver', name: '验收交付', steps: ['P16'] }],
    steps: steps.map(([code, key, name, estimate]) => ({ code, key, name, estimate })),
    rows: [
      { id: 'b-0123456789-1', name: '团队1-41', provider: 'ATTVPS', ip: '198.51.100.21', ssh_port: 22, relay: '线路机3', relay_port: 20031, row_state: 'active', current_step: 'P16', canary: true, cells: cells(['succeeded', 'succeeded', 'succeeded', 'succeeded']) },
      { id: 'b-0123456789-2', name: '团队1-42', provider: 'ATTVPS', ip: '198.51.100.22', ssh_port: 22, relay: '线路机3', row_state: 'waiting_input', current_step: 'P2', cells: cells(['succeeded', 'waiting_input', 'queued', 'queued']) },
      { id: 'b-0123456789-3', name: '团队1-43', provider: 'ATTVPS', ip: '198.51.100.23', ssh_port: 2201, relay: '线路机3', row_state: 'in_relay_batch', current_step: 'P13', cells: cells(['succeeded', 'succeeded', 'running', 'queued']) },
    ],
    relay_groups: [{ relay: '线路机3', batch_no: 2, state: 'collecting', window_left_s: 12, members: ['团队1-43'], xray_reloads: 0, probe_restarts: 0, impact: '窗口结束后合并为一次 Xray API 变更（不重启）' }],
    kpis: { total: 3, done: 1, running: 1, attention: 1, failed: 0, eta_s: null },
    ...over,
  };
}

test('live onboarding model: fleetd batch view → page model; row / batch state from the API, never simulated', async () => {
  const OB = await import(`${APP}onboarding-model.js`);
  let now = 1060;
  const store = { onboarding: () => ({ history: [], batch: null }), now: () => now };
  const st = OB.obState(store);
  assert.equal(st.batches.length, 0);
  const b = OB.liveBatch(st, batchView());
  assert.equal(b.live, true);
  assert.equal(b.demo, false);
  assert.deepEqual(b.rows.map((r) => OB.rowWord(b, r).state), ['active', 'waiting_input', 'in_relay_batch']);
  assert.equal(OB.batchStatus(b), 'running');
  const k = OB.kpisOf(b, now);
  assert.equal(k.done.length, 1);
  assert.equal(k.attention.length, 1);
  assert.equal(k.elapsed, 60);
  // the engine never advances a live batch
  const before = JSON.stringify(b.rows.map((r) => r.cells.map((c) => c.status)));
  now += 3600;
  OB.tick(st);
  assert.equal(JSON.stringify(b.rows.map((r) => r.cells.map((c) => c.status))), before);
  // statuses: paused (lease gone) → 已中断, validated → 待确认; a finished batch's elapsed stops at finished_at
  const b2 = OB.liveBatch(st, batchView({ status: 'paused' }));
  assert.equal(OB.batchStatus(b2), 'interrupted');
  assert.deepEqual(OB.BATCH_WORD.interrupted, ['已中断', 'warn']);
  const b3 = OB.liveBatch(st, batchView({ status: 'succeeded', finished_at: 1500 }));
  assert.equal(OB.kpisOf(b3, 99999).elapsed, 500);
  assert.equal(st.batches.length, 1, 'same id → replaced, not duplicated');
  const b4 = OB.liveBatch(st, batchView({ id: 'b-x', status: 'draft', started_at: null }));
  assert.equal(OB.batchStatus(b4), 'draft');
  assert.equal(OB.kpisOf(b4, now).elapsed, 0);
});

test('live onboarding log: fleetd events (step.started / finished / node.state / relay_batch.updated) → step drawer lines', async () => {
  const OB = await import(`${APP}onboarding-model.js`);
  const store = { onboarding: () => ({ history: [], batch: null }), now: () => 2000 };
  const st = OB.obState(store);
  const b = OB.liveBatch(st, batchView());
  OB.addEvents(b, [
    { id: 1, ts: 1_000_000, node_id: 'b-0123456789-2', type: 'step.started', data: { step: 'P2', attempt: 1 } },
    { id: 2, ts: 1_004_000, node_id: 'b-0123456789-2', type: 'step.finished', data: { step: 'P2', status: 'waiting_input', detail: '上架密钥登录被拒' } },
    { id: 3, ts: 1_004_100, node_id: 'b-0123456789-2', type: 'node.state', data: { name: '团队1-42', state: 'waiting_input', current_step: 'P2', reason: '上架密钥登录被拒' } },
    { id: 4, ts: 1_010_000, node_id: null, type: 'relay_batch.updated', data: { relay: '线路机3', batch_no: 2, status: 'merging', members: ['团队1-43'] } },
    { id: 2, ts: 0, node_id: 'x', type: 'dup', data: {} },
  ]);
  assert.equal(b.eventsAfter, 4);
  assert.equal(b.events.length, 4, 'ids at or below the cursor are ignored');
  const r2 = b.rows.find((r) => r.name === '团队1-42');
  const log = OB.logFor(b, r2, 'P2', 2000);
  assert.deepEqual(log.map((l) => l.t), ['00:00', '00:04', '00:04']);
  assert.match(log[1].text, /上架密钥登录被拒/);
  assert.equal(log[2].tone, 'warn');
  const r3 = b.rows.find((r) => r.name === '团队1-43');
  assert.match(OB.logFor(b, r3, 'P13', 2000).map((l) => l.text).join('\n'), /线路机3 批次 #2：合并中/);
  // no events yet: the cell detail stands in
  const b2 = OB.liveBatch(st, batchView({ id: 'b-y' }));
  assert.match(OB.logFor(b2, b2.rows[1], 'P2', 2000)[0].text, /密码文件/);
  const io = OB.ioFor(b2, b2.rows[2], 'P13');
  assert.ok(io.input.some(([k, v]) => k === 'IP' && v === '198.51.100.23:2201'));
  assert.ok(io.input.every(([, v]) => !/pass|密码：/.test(String(v))), 'no secret values');
});

test('new batch dialog: local CSV check (header, no password column, size)', async () => {
  const D = await import(`${APP}onboarding-live-dialog.js`);
  const head = 'provider,ip,ssh_port,auth,plan,team,number';
  assert.equal(D.csvProblem(''), '还没有内容');
  assert.equal(D.csvProblem(`${head}\nATTVPS,198.51.100.1,22,key,ATTVPS 基础版,团队1,41`), '');
  assert.match(D.csvProblem(`${head},password\nATTVPS,198.51.100.1,22,key,x,团队1,41,secret`), /不要放密码列/);
  assert.match(D.csvProblem('ip,provider\n1.2.3.4,ATTVPS'), /表头缺少 ssh_port、auth、plan、team、number/);
  assert.match(D.csvProblem(head), /只有表头/);
  assert.match(D.csvProblem(`${head}\n${'x'.repeat(61 * 1024)}`), /太大/);
});

test('account page: user agent → short device name', async () => {
  const A = await import(`${APP}pages/account.js`);
  assert.equal(A.uaShort('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36'), 'Chrome 129 · macOS');
  assert.equal(A.uaShort('curl/8.4.0'), 'curl');
  assert.equal(A.uaShort(''), '—');
});
