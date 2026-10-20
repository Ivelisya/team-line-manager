// crosscheck.mjs — adapter output (from live API captures) vs prototype/data/tools/build.py output computed
// from the SAME database copy (staging/crosscheck_build.sh, no anonymisation).
//   node tests/crosscheck.mjs [--nodes t1-01,t1-13,t1-31,t2-01,r2] [--all]
// Prints every differing leaf path with both values.  Used by crosscheck.test.mjs.
import { readFileSync, existsSync } from 'node:fs';
import path from 'node:path';
import { fixtureFetch, FIX } from './helpers.mjs';
import { createLiveFetch } from '../live/adapter.js';

export const CC = path.join(FIX, 'crosscheck');
export const hasCrosscheck = existsSync(path.join(CC, 'snapshot.json'));
export const SAMPLE = ['t1-01', 't1-13', 't1-31', 't2-01', 'r2'];

// Paths whose values legitimately differ, with the reason (README "与快照构建器的差异").
export const EXPLAINED = [
  [/^traffic\.cumulative\.source$/, 'label: the legacy ledger continues after 3x-ui (fleet-xui-traffic)'],
  [/^latency\.strip_(tcp|egress)(\.|$)/, 'strip: monitor probe samples accumulated by polling (build: Komari 1-minute rollups)'],
  [/^host_spark(\.|$)/, 'spark: Komari records seeded / accumulated (build: 5-minute rollups from metrics.db)'],
  [/^availability\.tracker30\./, 'tracker30: the monitor’s own 30 UTC-day strip (build: Beijing calendar days from health_hours)'],
  [/^availability\.windows\.(1|7)\./, 'windows 1/7 only exist in /node (loaded when a node is opened)'],
  [/^connection\.credential_prefix$/, 'build writes a demo value; live shows the real first 8 characters'],
  [/^connection\.credential_prefix_note$/, 'wording'],
  [/^traffic\.hourly24\.(down|up)\.23$/, 'last (partial) hour: /history includes the current, still-growing minute'],
  [/^komari_uuid$/, 'build writes a random fake UUID'],
];
// fleet-level paths that differ for a known reason
export const EXPLAINED_TOP = [
  [/^kpis\.business_rate\.spark\./, 'spark: /history is 5-minute (build: 1-minute minutes table)'],
  [/^kpis\.events_today$/, 'server counts activity after as_of too (rows up to the relay receive time)'],
  [/^system\.storage\./, 'no API for database file sizes'],
  [/^system\.retention\.komari_/, 'Komari retention settings are not exposed'],
  [/^system\.health_ok$/, 'live-only field'],
  [/^relays\.\d+\.config\.core_id$/, 'build writes a fake hash; live shows the real core id prefix'],
  [/^entries\.\d+\.client\.credential_prefix$/, 'build writes a demo value'],
  [/^entries\.\d+\.ipintel$/, 'ipintel is null live'],
  // 2026-09-24: 3x-ui / x-ui retired, relays run fleet-xray under the new system (LIVE-MODE.md §16)
  [/^relays\.\d+\.(status\.parts\.(panel|config)|versions\.panel|panel\.(ok|error|retired))$/, 'panel / config checks retired with 3x-ui (null = not applicable)'],
  [/^relays\.\d+\.(managed|xray\.\w+)$/, 'live-only: Xray status under the new system'],
  [/^entries\.\d+\.source$/, 'live-only: which system the counters came from'],
];
export const explainTop = (p) => EXPLAINED_TOP.find(([rx]) => rx.test(p))?.[1] ?? null;
export const explain = (p) => EXPLAINED.find(([rx]) => rx.test(p))?.[1] ?? null;

function leaves(o, prefix = '', out = new Map()) {
  if (o !== null && typeof o === 'object') {
    const keys = Array.isArray(o) ? o.map((_, i) => i) : Object.keys(o);
    if (!keys.length) out.set(prefix, Array.isArray(o) ? '[]' : '{}');
    for (const k of keys) leaves(o[k], prefix ? `${prefix}.${k}` : String(k), out);
  } else out.set(prefix, o);
  return out;
}
const eq = (a, b) => a === b || (typeof a === 'number' && typeof b === 'number' && Math.abs(a - b) <= 1e-9 * Math.max(1, Math.abs(a)));

export async function crosscheck({ ids = null } = {}) {
  const ref = JSON.parse(readFileSync(path.join(CC, 'snapshot.json'), 'utf8'));
  // The captured build.py output predates two rule changes made in build.py and synth.js together (2026-09-24):
  // entry labels are written 线路机2:20002 (no space before the colon), and egress_verified is null when the probe
  // has no actual exit IP.  Apply them to the old capture instead of re-running build.py on a395.
  for (const n of ref.nodes) {
    if (typeof n.entry_label === 'string') n.entry_label = n.entry_label.replace(/ :(\d+)$/, ':$1');
    if (n.latency && n.latency.egress_actual_ip == null && n.latency.egress_verified === false) n.latency.egress_verified = null;
    // fleetd 0.13.3: egress is connectivity only — an actual exit IP means verified, whatever the ledger says
    if (n.latency && n.latency.egress_actual_ip) n.latency.egress_verified = true;
  }
  for (const e of ref.entries ?? []) if (e.egress?.actual_ip) e.egress.verified = true;
  const f = fixtureFetch();
  const lf = createLiveFetch({ fetchImpl: f, trafficLane: 'eager', jobsLane: 'eager', hostSpark: 'accumulate' });
  const snap = await (await lf('http://x/data/snapshot.json')).json();
  const refById = new Map(ref.nodes.map((n) => [n.id, n]));
  const rows = [];
  for (const n of snap.nodes) {
    if (ids && !ids.includes(n.id)) continue;
    const r = refById.get(n.id);
    if (!r) { rows.push({ id: n.id, path: '(node)', ref: 'missing', live: 'present' }); continue; }
    const A = leaves(r), B = leaves(n);
    for (const p of new Set([...A.keys(), ...B.keys()])) {
      if (p.startsWith('ipintel')) continue;
      const a = A.has(p) ? A.get(p) : '(absent)', b = B.has(p) ? B.get(p) : '(absent)';
      if (!eq(a, b)) rows.push({ id: n.id, path: p, ref: a, live: b, why: explain(p) });
    }
  }
  const top = {};
  for (const k of ['kpis', 'groups', 'upcoming_due', 'system', 'behavior_kpis', 'relays', 'entries', 'status_replays', 'incidents_total', 'providers', 'teams']) {
    const A = leaves(ref[k], k), B = leaves(snap[k], k);
    for (const p of new Set([...A.keys(), ...B.keys()])) {
      const a = A.has(p) ? A.get(p) : '(absent)', b = B.has(p) ? B.get(p) : '(absent)';
      if (!eq(a, b)) (top[k] ??= []).push({ path: p, ref: a, live: b, why: explainTop(p) });
    }
  }
  return { rows, top, ref, snap };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const arg = process.argv.slice(2);
  const all = arg.includes('--all');
  const ids = all ? null : (arg[arg.indexOf('--nodes') + 1] && arg.includes('--nodes') ? arg[arg.indexOf('--nodes') + 1].split(',') : SAMPLE);
  const { rows, top } = await crosscheck({ ids });
  const byPath = new Map();
  for (const r of rows) {
    const k = r.path.replace(/\.\d+(?=\.|$)/g, '.#') + (r.why ? '' : ' (' + r.path + ')');
    if (!byPath.has(k)) byPath.set(k, { n: 0, ids: new Set(), ex: r });
    const e = byPath.get(k); e.n++; e.ids.add(r.id);
  }
  console.log(`node leaf differences (${ids ? ids.join(',') : 'all nodes'}):`);
  for (const [p, e] of [...byPath].sort((a, b) => (a[1].ex.why ? 1 : 0) - (b[1].ex.why ? 1 : 0) || a[0].localeCompare(b[0]))) {
    console.log(`  ${e.ex.why ? 'explained ' : 'UNEXPLAINED'} ${p}  ×${e.n} [${[...e.ids].slice(0, 6).join(' ')}]  ref=${JSON.stringify(e.ex.ref)} live=${JSON.stringify(e.ex.live)}${e.ex.why ? '  — ' + e.ex.why : ''}`);
  }
  console.log('fleet-level differences:');
  for (const [k, list] of Object.entries(top)) {
    const collapsed = new Map();
    for (const r of list) { const kk = r.path.replace(/\.\d+(?=\.|$)/g, '.#'); if (!collapsed.has(kk)) collapsed.set(kk, { n: 0, ex: r }); collapsed.get(kk).n++; }
    for (const [p, e] of collapsed) console.log(`  ${e.ex.why ? 'explained ' : 'UNEXPLAINED'} ${p} ×${e.n} ref=${JSON.stringify(e.ex.ref)} live=${JSON.stringify(e.ex.live)}${e.ex.why ? '  — ' + e.ex.why : ''}`);
  }
}
