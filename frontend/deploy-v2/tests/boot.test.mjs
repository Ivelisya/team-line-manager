// boot.test.mjs — the dist wiring (live/boot.js): the generated shared/index.js hands the prototype's
// loadSnapshot / createLive to these two functions.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fixtureFetch, hasFixtures } from './helpers.mjs';
import { loadSnapshot } from '../../../prototype/shared/data.js';

const skip = !hasFixtures && 'fixtures missing: run staging/stage_server.py capture on a395';

test('boot: loadSnapshot → live store, createLive → poller; replays cannot overwrite live status', { skip }, async () => {
  const boot = await import('../live/boot.js');
  const f = fixtureFetch();
  boot.TEAMOPS.source.fetchImpl = f;
  const store = await boot.liveLoadSnapshot(loadSnapshot, { base: new URL('http://x/admin/v2/data/') });
  assert.equal(store.nodes().length, 41);
  assert.equal(store.raw.provenance.source, 'live');
  assert.equal(globalThis.TEAMOPS_LIVE, true);
  const poller = boot.liveCreateLive(store, { lanesDelayMs: 60_000 });
  assert.equal(boot.liveCreateLive(store), poller, 'one poller per page');
  assert.equal(typeof poller.need, 'function');
  assert.deepEqual(store.replays(), []);
  assert.equal(store.applyReplay(0), false);
  const id = store.nodes()[0].id;
  assert.match(boot.TEAMOPS.assetOf(id), /^[0-9a-f-]{36}$/);
  assert.equal(boot.TEAMOPS.assetOf('nope'), null);
  assert.ok(f.calls.every((c) => c.method === 'GET'));
  poller.dispose();
});

test('TEAMOPS.fleet.has: data.features decides (no probing); without the list only auth (≥ 0.4) and onboard (edge.onboard)', async () => {
  const { TEAMOPS } = await import('../live/boot.js');
  const f = TEAMOPS.fleet;
  const saved = { available: f.available, system: f.system };
  try {
    f.available = true;
    f.system = { version: '0.4.1', features: ['auth', 'onboard', 'alerts'] };
    assert.equal(f.has('onboard'), true);
    assert.equal(f.has('entries'), false);
    assert.equal(f.has('policies'), false);
    f.system = { version: '0.5.2', features: ['auth', 'entries', 'ops'] };
    assert.equal(f.has('entries'), true);
    assert.equal(f.has('onboard'), false, 'only what is listed');
    f.system = { version: '0.4.0', edge: { onboard: true } };
    assert.equal(f.has('auth'), true);
    assert.equal(f.has('onboard'), true);
    assert.equal(f.has('entries'), false, 'no list → no §9 writes, whatever the version');
    f.system = { version: '0.3.4' };
    assert.equal(f.has('auth'), false);
    f.available = false;
    f.system = { version: '0.5.0', features: ['entries'] };
    assert.equal(f.has('entries'), false, 'fleetd not in use');
  } finally { Object.assign(f, saved); }
});
