import test from 'node:test';
import assert from 'node:assert/strict';
import { createKeys, createPalette, parseCombo, comboLabel, fuzzy, defaultBindings } from './keys.js';

const ev = (key, o = {}) => ({ key, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, target: o.target ?? { tagName: 'BODY' }, preventDefault() { this.prevented = true; }, ...o });
const input = { tagName: 'INPUT', type: 'text' };

test('parseCombo and labels', () => {
  assert.deepEqual(parseCombo('mod+k', { mac: true }), [{ key: 'k', ctrl: false, meta: true, alt: false, shift: false }]);
  assert.deepEqual(parseCombo('mod+k', { mac: false }), [{ key: 'k', ctrl: true, meta: false, alt: false, shift: false }]);
  assert.equal(parseCombo('g n').length, 2);
  assert.equal(comboLabel('mod+k', { mac: true }), '⌘K');
  assert.equal(comboLabel('mod+k', { mac: false }), 'Ctrl K');
  assert.equal(comboLabel('g n', { mac: true }), 'G 然后 N');
  assert.equal(comboLabel('Escape', { mac: true }), 'Esc');
});

test('single keys, modifiers, symbols', () => {
  const k = createKeys({ mac: true });
  const hits = [];
  k.bind('mod+k', () => hits.push('palette'));
  k.bind('/', () => hits.push('search'));
  k.bind('?', () => hits.push('help'));
  k.bind('shift+x', () => hits.push('range'));
  k.bind('x', () => hits.push('x'));
  const e = ev('k', { metaKey: true });
  assert.equal(k.handle(e), true);
  assert.equal(e.prevented, true);
  k.handle(ev('/'));
  k.handle(ev('?', { shiftKey: true }));
  k.handle(ev('X', { shiftKey: true }));
  k.handle(ev('x'));
  assert.deepEqual(hits, ['palette', 'search', 'help', 'range', 'x']);
});

test('sequences: g then n; abandoned sequence falls back to the single key', () => {
  let t = 0;
  const k = createKeys({ mac: true, now: () => t });
  const hits = [];
  k.bind('g n', () => hits.push('nodes'));
  k.bind('g r', () => hits.push('relays'));
  k.bind('j', () => hits.push('down'));
  assert.equal(k.handle(ev('g')), true);
  assert.equal(k.pending(), 'G');
  k.handle(ev('n'));
  k.handle(ev('g'));
  k.handle(ev('j'));
  k.handle(ev('g'));
  t = 5000; // timed out
  k.handle(ev('r'));
  assert.deepEqual(hits, ['nodes', 'down']);
  assert.equal(k.pending(), null);
});

test('text fields: only opt-in bindings and Escape; IME composition ignored', () => {
  const k = createKeys({ mac: true });
  const hits = [];
  k.bind('mod+k', () => hits.push('palette'), { allowInInput: true });
  k.bind('j', () => hits.push('j'));
  k.bind('mod+a', () => hits.push('all'));
  k.bind('Escape', () => hits.push('esc'));
  k.handle(ev('j', { target: input }));
  k.handle(ev('a', { metaKey: true, target: input }));
  k.handle(ev('k', { metaKey: true, target: input }));
  k.handle(ev('Escape', { target: input }));
  k.handle(ev('j', { isComposing: true }));
  k.handle(ev('Process', { keyCode: 229 }));
  assert.deepEqual(hits, ['palette', 'esc']);
});

test('scopes: top scope first, exclusive scope blocks lower ones except global mod-combos', () => {
  const k = createKeys({ mac: true });
  const hits = [];
  k.bind('Escape', () => hits.push('global-esc'));
  k.bind('j', () => hits.push('table-j'), { scope: 'global' });
  k.bind('mod+k', () => hits.push('palette-toggle'));
  k.bind('Escape', () => hits.push('drawer-esc'), { scope: 'drawer' });
  k.bind('ArrowDown', () => hits.push('palette-down'), { scope: 'palette', allowInInput: true });
  const popDrawer = k.pushScope('drawer');
  k.handle(ev('Escape'));
  k.handle(ev('j'));
  const popPalette = k.pushScope('palette', { exclusive: true });
  k.handle(ev('j'));
  k.handle(ev('ArrowDown', { target: input }));
  k.handle(ev('k', { metaKey: true }));
  popPalette();
  popDrawer();
  k.handle(ev('Escape'));
  assert.deepEqual(hits, ['drawer-esc', 'table-j', 'palette-down', 'palette-toggle', 'global-esc']);
});

test('help list groups bindings', () => {
  const k = createKeys({ mac: true });
  k.bindAll(defaultBindings({ palette() {}, focusSearch() {}, goNodes() {}, goRelays() {} }));
  const list = k.list();
  assert.deepEqual(list.map((g) => g.group), ['全局', '跳转']);
  assert.equal(list[1].items[0].labels[0], 'G 然后 N');
});

test('fuzzy scoring order', () => {
  assert.equal(fuzzy('团队1-21', '团队1-21').score, 1000);
  assert.ok(fuzzy('21', '团队1-21').score > fuzzy('21', '团队1-02 198.51.100.21x').score - 1000);
  assert.ok(fuzzy('团队1', '团队1-21').score > fuzzy('1-21', '团队1-21').score);
  assert.ok(fuzzy('100.31', '198.51.100.31').score >= 400);
  assert.ok(fuzzy('xl2', '线路机2 xl2')); // subsequence
  assert.equal(fuzzy('zzz', '团队1-21'), null);
});

test('palette: ranking, groups, recents, danger confirm', () => {
  const ran = [];
  const mem = new Map();
  const storage = { getItem: (k) => mem.get(k) ?? null, setItem: (k, v) => mem.set(k, v), removeItem: (k) => mem.delete(k) };
  const items = [
    { id: 'go:nodes', group: '跳转', title: '节点', run: () => ran.push('go:nodes') },
    { id: 'node:t1-21', group: '节点', title: '团队1-21', keywords: ['21', '198.51.100.31', '60131'], run: () => ran.push('t1-21') },
    { id: 'node:t1-02', group: '节点', title: '团队1-02', keywords: ['02', '2', '198.51.100.12'], run: () => ran.push('t1-02') },
    { id: 'act:restart', group: '操作', title: '重启 Xray 线路机2', danger: true, run: () => ran.push('restart') },
  ];
  const p = createPalette({ items, storage });
  p.open();
  assert.deepEqual(p.grouped().map((g) => g.group), ['跳转']);
  p.setQuery('21');
  assert.equal(p.results()[0].item.id, 'node:t1-21');
  p.setQuery('60131');
  assert.equal(p.results()[0].item.id, 'node:t1-21', 'service id matches');
  p.run();
  assert.deepEqual(ran, ['t1-21']);
  p.open();
  assert.equal(p.grouped()[0].group, '最近');
  p.setQuery('重启');
  const first = p.run();
  assert.equal(first.needsConfirm, true);
  assert.equal(p.run().ran, true);
  assert.deepEqual(ran, ['t1-21', 'restart']);
  p.setQuery('团队');
  p.move(1);
  assert.equal(p.active(), 1);
  p.move(-2);
  assert.equal(p.active(), p.results().length - 1, 'wraps');
});
