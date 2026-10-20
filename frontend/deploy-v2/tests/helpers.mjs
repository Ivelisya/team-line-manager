// helpers.mjs — fixture-backed fetch for the adapter tests.
//
// fixtures/index.json maps "/api/team-ops/<path>?<sorted query>" (optionally "tick2|…") to a captured
// {status, body} file.  The captures come from stage_server.py capture: the real teamops backend code,
// in-process on a395, over a fresh copy of the production databases, clock pinned at as_of.
import { readFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

export const HERE = path.dirname(fileURLToPath(import.meta.url));
export const V2 = path.resolve(HERE, '..');
export const ROOT = path.resolve(V2, '../..');
export const FIX = path.join(V2, 'live', 'fixtures');
export const PROTO = path.join(ROOT, 'prototype');
export const hasFixtures = existsSync(path.join(FIX, 'index.json'));

let INDEX = null;
const cache = new Map();
export function index() { return (INDEX ??= JSON.parse(readFileSync(path.join(FIX, 'index.json'), 'utf8'))); }
function load(file) {
  if (!cache.has(file)) cache.set(file, readFileSync(path.join(FIX, file), 'utf8'));
  return JSON.parse(cache.get(file));
}
export function keyOf(url) {
  const u = new URL(String(url), 'http://stage.local');
  const q = [...u.searchParams.entries()].sort(([a], [b]) => (a < b ? -1 : 1)).map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`).join('&');
  return decodeURIComponent(u.pathname) + (q ? '?' + q : '');
}
export function apiData(key, set = '') {
  const r = index().routes[(set ? set + '|' : '') + key] ?? index().routes[key];
  return r ? load(r).body.data : undefined;
}

/**
 * fixtureFetch({ set, log, override, status }) → fetch-compatible function.
 * set: fixture set prefix ('' | 'tick2'); may be changed later via f.use('tick2').
 * override(key, body) → body: lets a test perturb a response.  status: force every response to this code.
 */
export function fixtureFetch({ set = '', override = null, status = null } = {}) {
  const calls = [];
  let current = set;
  async function f(url, init = {}) {
    const method = String(init.method ?? 'GET').toUpperCase();
    const key = keyOf(url);
    calls.push({ key, method });
    const res = (st, body) => ({ ok: st >= 200 && st < 300, status: st, json: async () => body, text: async () => JSON.stringify(body) });
    if (status) return res(status, { error: status === 401 ? '请先登录管理员账户' : 'error' });
    if (!key.startsWith('/api/team-ops/')) {
      // static passthrough (plan-catalog.json etc.) from the prototype data dir
      const file = path.join(PROTO, 'data', path.basename(key));
      return existsSync(file) ? res(200, JSON.parse(readFileSync(file, 'utf8'))) : res(404, { error: 'not found' });
    }
    const r = index().routes[(current ? current + '|' : '') + key] ?? index().routes[key];
    if (!r) return res(404, { error: '接口不存在' });
    const cap = load(r);
    let body = cap.body;
    if (override && cap.status === 200) body = override(key, body, current) ?? body;
    return res(cap.status, body);
  }
  f.calls = calls;
  f.use = (s) => { current = s; };
  return f;
}

/** JSON type of a value: 'object' | 'array' | 'number' | 'string' | 'boolean' | 'null'. */
export const jtype = (v) => (v === null || v === undefined ? 'null' : Array.isArray(v) ? 'array' : typeof v);

/**
 * Compare a reference object (prototype snapshot) with a synthesised one.
 * Every key of `ref` must exist in `got` with the same JSON type, or be null in `got` (degraded).
 * Returns { missing[], typeErr[], live[], degraded[] } of dotted paths.
 */
export function compareShape(ref, got, pathName = '', out = { missing: [], typeErr: [], live: [], degraded: [] }, { skip = new Set(), maps = MAPS } = {}) {
  if (skip.has(pathName)) return out;
  const tr = jtype(ref), tg = jtype(got);
  if (tg === 'null') { if (tr !== 'null') out.degraded.push(pathName); else out.live.push(pathName); return out; }
  if (tr === 'null') { out.live.push(pathName); return out; }
  if (tr !== tg) { out.typeErr.push(`${pathName}: ${tr} → ${tg}`); return out; }
  if (tr === 'object') {
    const bare = pathName.replace(/\[\]/g, '');
    if (maps.has(bare)) {
      // dynamic keys (node ids, relay names, audit actions …): compare value shapes, not key sets
      const rk = Object.keys(ref).find((k) => ref[k] != null);
      const gk = rk in got ? rk : Object.keys(got).find((k) => got[k] != null);
      if (rk !== undefined && gk !== undefined) compareShape(ref[rk], got[gk], pathName + '{}', out, { skip, maps });
      else out.live.push(pathName);
      return out;
    }
    for (const k of Object.keys(ref)) {
      const p = pathName ? `${pathName}.${k}` : k;
      if (skip.has(p)) continue;
      if (!(k in got)) { out.missing.push(p); continue; }
      compareShape(ref[k], got[k], p, out, { skip, maps });
    }
    return out;
  }
  if (tr === 'array') {
    const r0 = ref.find((x) => x != null);
    if (r0 !== undefined && jtype(r0) === 'object') {
      // heterogeneous rows (incidents, audit …): pair by id when rows carry one
      const pairs = 'id' in r0 ? ref.filter((x) => x && typeof x === 'object').map((x) => [x, got.find((y) => y && y.id === x.id)]).filter(([, y]) => y) : [];
      if (!pairs.length) { const g0 = got.find((x) => x != null); if (g0 !== undefined) pairs.push([r0, g0]); }
      for (const [x, y] of pairs.slice(0, 50)) compareShape(x, y, pathName + '[]', out, { skip, maps });
      if (!pairs.length) out.live.push(pathName);
    } else out.live.push(pathName);
    return out;
  }
  out.live.push(pathName);
  return out;
}
export const MAPS = new Set(['access_recent', 'audit_counts', 'behavior.nodes', 'nodes', 'system.collector.duration_seconds', 'provenance.relay_snapshot_at',
  'status.summary_counts', 'counts', 'status_replays.counts', 'status_replays.nodes.evidence']);
