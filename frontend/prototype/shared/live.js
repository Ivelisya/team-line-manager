// live.js — simulated live updates on top of the snapshot store.
//
// Cadence mirrors the real system (brief §4.8): Komari host metrics every 2 s, relay collector
// (Xray entry counters → business rates) every 10 s per relay, monitor evaluation every 10 s,
// TCP probes every 30 s, egress probes every 300 s, panel every 15 s. A tick every `intervalMs`
// runs whatever is due, mutates the store in place and emits one minimal diff:
//
//   { seq, now, dt, catchUp,
//     nodes:   { 't1-31': { 'rate.down_bps': 3310, 'host.cpu_pct': 0.2, 'latency.strip_tcp': {…} } },
//     relays:  { 'r1': { 'host.cpu_pct': 45.1, 'traffic_quota.used_bytes': … } },
//     entries: { '线路机1:20001': { 'rate_down_bps': 118, … } },
//     fleet:   { 'kpis.events_today': 140830, 'traffic.fleet_24h': 'append', … },
//     events:  [{ type:'status', id, from, to, reason, check }, { type:'incident', incident }] }
//
// Only values that actually changed are listed. Counters (today, month, cumulative, NIC totals,
// quota used, events) only grow; status changes are rare and recover on their own.

import { rng, set } from './data.js';
import * as fmt from './format.js';

const perfNow = () => (globalThis.performance?.now ? globalThis.performance.now() : Date.now());

/** Remembers the last text per key so views touch the DOM only when the formatted text changes. */
export function createPatcher() {
  const m = new Map();
  return {
    changed(key, text) { if (m.get(key) === text) return false; m.set(key, text); return true; },
    forget(key) { key == null ? m.clear() : m.delete(key); },
  };
}
/** Did the diff change any of `prefixes` (e.g. 'rate.', 'host.cpu_pct') for this node? */
export function touched(diff, id, ...prefixes) {
  const ch = diff?.nodes?.[id] ?? diff?.relays?.[id] ?? diff?.entries?.[id];
  if (!ch) return false;
  if (!prefixes.length) return true;
  for (const k in ch) for (const p of prefixes) if (k === p || k.startsWith(p)) return true;
  return false;
}

const SCENARIOS = [
  // [weight, code, reason, check, minS, maxS]
  [26, 'degraded', 'unstable', 'tcp', 40, 480],
  [18, 'degraded', 'timeout', 'egress', 60, 420],
  [14, 'degraded', null, 'ingress', 30, 360],
  [10, 'degraded', 'retrying', 'egress', 20, 120],
  [12, 'fail', 'auth', 'ingress', 120, 540],
  [8, 'fail', 'upstream_down', 'tcp', 60, 400],
  [6, 'fail', 'egress_mismatch', 'egress', 90, 600],
  [6, 'unknown', null, 'host', 60, 240],
];

/**
 * createLive(store, opts) → controller
 *  opts.intervalMs   tick period (default 2000)
 *  opts.seed         PRNG seed (deterministic runs)
 *  opts.eventRate    expected status transitions per 5 minutes, fleet-wide (default 1; 0 = none)
 *  opts.pauseWhenHidden  stop ticking while the tab is hidden, catch up once on return (default true)
 *  opts.maxCatchUpS  cap for one catch-up step (default 3600)
 */
export function createLive(store, { intervalMs = 2000, seed = 42, eventRate = 1, pauseWhenHidden = true, maxCatchUpS = 3600 } = {}) {
  const r = rng(seed);
  const raw = store.raw;
  const asOf = store.asOf;
  const listeners = new Set();
  let offset = store.now() - asOf, realAnchor = null, timer = null, seq = 0, lastSim = store.now(), lastTickReal = null, visHandler = null;
  const clock = () => asOf + offset + (realAnchor != null ? (perfNow() - realAnchor) / 1000 : 0);
  store.setClock(clock);

  // ── per-entity model state ──
  const model = new Map();
  for (const n of store.nodes()) {
    const h = n.traffic?.hourly24;
    const lastHourBps = (d) => (h && h[d]?.length > 1 && h[d][h[d].length - 2] != null ? (h[d][h[d].length - 2] * 8) / 3600 : 0);
    const snapD = n.rate?.down_bps ?? 0, snapU = n.rate?.up_bps ?? 0;
    const cpuSeries = n.host_spark?.cpu_pct?.filter((v) => v != null) ?? [];
    const lt = n.latency ?? {};
    model.set(n.id, {
      baseDown: 0.5 * snapD + 0.5 * lastHourBps('down'),
      baseUp: 0.5 * snapU + 0.5 * lastHourBps('up'),
      hist: [[snapD, snapU]],
      cpuBase: cpuSeries.length ? cpuSeries.slice(-12).reduce((a, b) => a + b, 0) / Math.min(12, cpuSeries.length) : n.host?.cpu_pct ?? 0,
      memBase: n.host?.mem_pct ?? 0,
      loadBase: n.host?.load1 ?? 0,
      tcpConnBase: n.host?.tcp_conns ?? 0,
      nicRx: n.host?.net_rx_bps ?? 0, nicTx: n.host?.net_tx_bps ?? 0, nicRate: snapD + snapU,
      tcpBase: lt.strip_tcp?.median_ms ?? lt.tcp_ms ?? null,
      egBase: lt.strip_egress?.median_ms ?? lt.egress_ms ?? null,
      pathBase: lt.path_ms ?? null,
      nextTcp: lt.strip_tcp ? lt.strip_tcp.t_end + lt.strip_tcp.interval_s : (lt.tcp_at ?? asOf) + 30,
      nextEgress: lt.strip_egress ? lt.strip_egress.t_end + lt.strip_egress.interval_s : (lt.egress_at ?? asOf) + 300,
      nextPath: (lt.path_at ?? asOf) + 30,
      event: null, // { code, reason, check, recoverAt, incident }
    });
  }
  // A relay's NIC carries every node behind it: baseline its NIC against the summed business rate.
  const relayRate = (name) => store.nodes().reduce((s, x) => s + (x.relay === name ? (x.rate?.down_bps ?? 0) + (x.rate?.up_bps ?? 0) : 0), 0);
  for (const n of store.nodes()) if (n.isRelay) model.get(n.id).nicRate = relayRate(n.name);
  const nextCollect = new Map(store.relays().map((rl) => [rl.name, (rl.collector?.sample_ts ?? asOf) + 10]));
  const nextPanel = new Map(store.relays().map((rl) => [rl.id, (rl.panel?.checked_at ?? asOf) + 15]));
  let nextMonitor = (raw.system?.monitor?.checked_at ?? asOf) + 10;
  const fleet = raw.traffic?.fleet_24h;
  const spark = raw.kpis?.business_rate?.spark;
  const minuteAcc = { down: 0, up: 0 };
  {
    const k = store.kpis();
    const minuteStart = asOf - (asOf % 60);
    minuteAcc.down = (k.business_rate.down_bps * (asOf - minuteStart)) / 8;
    minuteAcc.up = (k.business_rate.up_bps * (asOf - minuteStart)) / 8;
  }
  const last = { komari: asOf, xray: asOf, monitor: raw.system?.monitor?.checked_at ?? asOf, panel: asOf };

  // ── diff helpers ──
  let d = null;
  function put(bucket, id, root, path, value) {
    if (set(root, path, value)) (d[bucket][id] ??= {})[path] = value;
  }
  const putN = (n, path, value) => put('nodes', n.id, n, path, value);

  // ── models ──
  function ou(x, target, theta, sigma) { return x + theta * (target - x) + sigma * r.normal(); }
  function nextRate(x, target, shared) {
    if (target < 1) return r() < 0.01 ? Math.round(2000 + r() * 40000) : 0; // idle entry: rare blips
    let lx = Math.log(Math.max(x, target * 0.3, 1));
    lx += 0.25 * (Math.log(target) - lx) + 0.35 * shared + 0.25 * r.normal();
    if (r() < 0.03) lx += Math.log(2 + r() * 6);
    return Math.max(0, Math.round(Math.exp(lx)));
  }

  function collect(relayName, t) {
    const ns = store.nodes().filter((n) => n.relay === relayName && !n.isRelay || (n.isRelay && n.name === relayName));
    for (const n of ns) {
      if (!n.rate) continue;
      const m = model.get(n.id);
      const dtN = Math.max(0, Math.min(maxCatchUpS, t - (n.rate.sample_at ?? t - 10)));
      const down0 = n.rate.down_bps ?? 0, up0 = n.rate.up_bps ?? 0;
      // bytes moved since the previous sample, at the previous rate
      const bd = (down0 * dtN) / 8, bu = (up0 * dtN) / 8;
      addBytes(n, bd, bu);
      let down, up;
      const ev = m.event;
      if (ev && ev.code === 'fail' && (ev.reason === 'auth' || ev.reason === 'upstream_down')) { down = 0; up = 0; }
      else {
        const shared = r.normal();
        down = nextRate(down0, m.baseDown, shared);
        up = nextRate(up0, m.baseUp, shared);
        const cap = n.bandwidth?.capacity_bps;
        if (cap) { down = Math.min(down, cap * 0.95); up = Math.min(up, cap * 0.95); }
      }
      putN(n, 'rate.down_bps', down);
      putN(n, 'rate.up_bps', up);
      putN(n, 'rate.sample_at', t);
      m.hist.push([down, up]);
      if (m.hist.length > 3) m.hist.shift();
      putN(n, 'rate.avg3_down_bps', Math.round(m.hist.reduce((s, h) => s + h[0], 0) / m.hist.length));
      putN(n, 'rate.avg3_up_bps', Math.round(m.hist.reduce((s, h) => s + h[1], 0) / m.hist.length));
      if (down + up > 0) putN(n, 'rate.last_active', t);
      const conns = n.rate.tcp_connections ?? 0;
      putN(n, 'rate.tcp_connections', Math.max(0, Math.round(down + up > 0 ? ou(conns, Math.max(1, conns), 0.3, 1 + conns * 0.05) : conns * 0.8)));
      if (n.bandwidth?.capacity_bps && n.bandwidth.measurement !== 'host_interface') {
        putN(n, 'bandwidth.util_down_pct', round3((down * 100) / n.bandwidth.capacity_bps));
        putN(n, 'bandwidth.util_up_pct', round3((up * 100) / n.bandwidth.capacity_bps));
      }
      if (n.behavior && down + up > 0 && r() < 0.3) {
        putN(n, 'behavior.connections', n.behavior.connections + 1 + Math.floor(r() * 4));
        putN(n, 'behavior.last_seen', t);
      }
      // entry row mirrors the node's business traffic
      const e = store.entryOf(n);
      if (e) {
        put('entries', e.id, e, 'rate_down_bps', down);
        put('entries', e.id, e, 'rate_up_bps', up);
        put('entries', e.id, e, 'tcp_connections', n.rate.tcp_connections);
        if (down + up > 0) put('entries', e.id, e, 'last_active', t);
        if (e.cumulative) { put('entries', e.id, e, 'cumulative.down', e.cumulative.down + Math.round(bd)); put('entries', e.id, e, 'cumulative.up', e.cumulative.up + Math.round(bu)); }
        if (e.counter) { put('entries', e.id, e, 'counter.down', e.counter.down + Math.round(bd)); put('entries', e.id, e, 'counter.up', e.counter.up + Math.round(bu)); }
      }
      minuteAcc.down += bd; minuteAcc.up += bu;
    }
    const rl = store.relay(relayName);
    if (rl?.collector) {
      put('relays', rl.id, rl, 'collector.received_at', t);
      put('relays', rl.id, rl, 'collector.sample_ts', t);
      put('relays', rl.id, rl, 'collector.duration_s', Math.round((rl.collector.duration_s * 0.8 + (3.8 + r() * 1.2) * 0.2) * 100) / 100);
    }
    last.xray = t;
  }

  function addBytes(n, bd, bu) {
    const tr = n.traffic;
    if (!tr || !(bd + bu)) return;
    const D = Math.round(bd), U = Math.round(bu);
    for (const w of ['today', 'month', 'last24h']) if (tr[w]) { putN(n, `traffic.${w}.down`, tr[w].down + D); putN(n, `traffic.${w}.up`, tr[w].up + U); }
    if (tr.cumulative) { putN(n, 'traffic.cumulative.down', tr.cumulative.down + D); putN(n, 'traffic.cumulative.up', tr.cumulative.up + U); }
    const h = tr.hourly24;
    if (h) {
      const i = h.down.length - 1;
      h.down[i] = (h.down[i] ?? 0) + D; h.up[i] = (h.up[i] ?? 0) + U;
      (d.nodes[n.id] ??= {})['traffic.hourly24'] = h;
    }
  }

  function hostTick(n, t, dt) {
    const h = n.host, m = model.get(n.id);
    if (!h) return;
    if (m.event?.code === 'unknown') { putN(n, 'host.fresh', false); putN(n, 'agent.online', false); return; }
    if (h.fresh === false) putN(n, 'host.fresh', true);
    if (n.agent && n.agent.online === false) putN(n, 'agent.online', true);
    const cores = h.cores || 1;
    putN(n, 'host.cpu_pct', clamp(round1(ou(h.cpu_pct ?? 0, m.cpuBase, 0.35, Math.max(0.15, m.cpuBase * 0.12))), 0, 100));
    const mem = clamp(round2(ou(h.mem_pct ?? 0, m.memBase, 0.2, 0.04)), 0, 100);
    putN(n, 'host.mem_pct', mem);
    if (h.mem_total) putN(n, 'host.mem_used', Math.round((mem / 100) * h.mem_total));
    const l1 = Math.max(0, round2(ou(h.load1 ?? 0, m.loadBase, 0.2, 0.05 * cores)));
    putN(n, 'host.load1', l1);
    const l5 = round2(h.load5 + (l1 - h.load5) * Math.min(1, dt / 300));
    putN(n, 'host.load5', l5);
    putN(n, 'host.load15', round2(h.load15 + (l1 - h.load15) * Math.min(1, dt / 900)));
    putN(n, 'host.load5_per_core_pct', Math.round((l5 / cores) * 100));
    // NIC follows business traffic (relays carry every node behind them)
    const rate = n.isRelay ? relayRate(n.name) : (n.rate?.down_bps ?? 0) + (n.rate?.up_bps ?? 0);
    const scale = (rate + 4000) / (m.nicRate + 4000);
    const rx = Math.max(0, Math.round(m.nicRx * scale * Math.exp(0.08 * r.normal())));
    const tx = Math.max(0, Math.round(m.nicTx * scale * Math.exp(0.08 * r.normal())));
    putN(n, 'host.net_rx_bps', rx);
    putN(n, 'host.net_tx_bps', tx);
    putN(n, 'host.net_total_rx', h.net_total_rx + Math.round((rx * dt) / 8));
    putN(n, 'host.net_total_tx', h.net_total_tx + Math.round((tx * dt) / 8));
    putN(n, 'host.tcp_conns', Math.max(0, Math.round(ou(h.tcp_conns ?? 0, m.tcpConnBase, 0.3, 1 + m.tcpConnBase * 0.03))));
    putN(n, 'host.uptime_s', h.uptime_s + Math.round(dt));
    putN(n, 'host.reported_at', t);
    if (n.agent) putN(n, 'agent.reported_at', t);
    if (n.isRelay && n.bandwidth?.capacity_bps) {
      putN(n, 'bandwidth.util_down_pct', round3((rx * 100) / n.bandwidth.capacity_bps));
      putN(n, 'bandwidth.util_up_pct', round3((tx * 100) / n.bandwidth.capacity_bps));
    }
    if (n.isRelay && n.traffic?.quota && n.traffic.quota.type === 'up') {
      putN(n, 'traffic.quota.used_bytes', n.traffic.quota.used_bytes + Math.round((tx * dt) / 8));
    }
    last.komari = t;
  }

  function probe(n, t, kind) {
    const m = model.get(n.id);
    const lt = n.latency;
    const key = kind === 'tcp' ? 'strip_tcp' : 'strip_egress';
    const base = kind === 'tcp' ? m.tcpBase : m.egBase;
    const strip = lt?.[key];
    if (!lt || base == null) return;
    const ev = m.event;
    const failing = ev && ev.code === 'fail' && (kind === 'tcp' ? ev.check === 'tcp' : ev.check !== 'tcp');
    const slow = ev && ev.code === 'degraded' && (kind === 'tcp' ? ev.check === 'tcp' : ev.check !== 'tcp');
    let v = Math.round(base * Math.exp(0.22 * r.normal()) * (r() < 0.03 ? 2 + r() * 2 : 1) * (slow ? 3 + r() * 3 : 1));
    const lost = failing || (!ev && r() < 0.004) ? 1 : 0;
    if (lost) v = null;
    if (strip) {
      const ms = strip.ms.slice(1).concat(v);
      const lostArr = strip.lost.slice(1).concat(lost);
      const ok = ms.filter((x, i) => x != null && !lostArr[i]).sort((a, b) => a - b);
      const median = ok.length ? ok[Math.floor((ok.length - 1) / 2)] : null;
      putN(n, `latency.${key}`, { ...strip, ms, lost: lostArr, t_end: t, median_ms: median, loss_pct: round1((lostArr.reduce((s, x) => s + x, 0) * 100) / lostArr.length) });
    }
    if (kind === 'tcp') {
      putN(n, 'latency.tcp_ms', lost ? null : Math.max(1, Math.round(v * (1 + 0.08 * r.normal()))));
      putN(n, 'latency.tcp_at', t);
    } else {
      putN(n, 'latency.egress_ms', lost ? null : v);
      putN(n, 'latency.egress_at', t);
      const mismatch = ev?.reason === 'egress_mismatch';
      putN(n, 'latency.egress_verified', !mismatch && !lost);
      putN(n, 'latency.egress_actual_ip', mismatch ? '203.0.113.99' : lost ? null : lt.egress_expected_ip);
    }
  }

  function startEvent(t) {
    const cands = store.nodes().filter((n) => !n.isRelay && fmt.healthCode(n.status?.code) === 'ok' && !model.get(n.id).event);
    if (!cands.length) return;
    const n = r.pick(cands);
    let total = SCENARIOS.reduce((s, x) => s + x[0], 0), roll = r() * total, sc = SCENARIOS[0];
    for (const x of SCENARIOS) { if ((roll -= x[0]) <= 0) { sc = x; break; } }
    let [, code, reason, check, lo, hi] = sc;
    if (code === 'unknown' && !n.hasHost) { code = 'degraded'; reason = 'unstable'; check = 'tcp'; }
    const dur = Math.round(Math.exp(Math.log(lo) + r() * (Math.log(hi) - Math.log(lo))));
    const from = n.status.code;
    const failed = [{ key: check, title: fmt.checkTitle(check), critical: code === 'fail', status: code, reason, since: t, measured_at: t, value: null, pending: false }];
    putN(n, 'status', { ...n.status, code, word: fmt.health(code).word, reason, reason_word: fmt.reasonWord(reason), since: t, checked_at: t, first_failed_check: check, attention: code === 'fail', failed_checks: failed, summary_counts: { ...(n.status.summary_counts ?? {}), [code]: 1 } });
    let incident = null;
    if (code !== 'unknown') {
      incident = store.addIncident({
        id: `live-${seq}-${n.id}`, node: n.id, name: n.name, check, title: fmt.checkTitle(check),
        severity: code === 'fail' ? 'critical' : 'warning', severity_word: code === 'fail' ? '故障' : '提醒',
        status: code, status_word: fmt.health(code).word, reason, reason_word: fmt.reasonWord(reason),
        opened_at: t, last_seen: t, resolved_at: null, duration_s: null, open: true, ack_at: null, ack_actor: null, note: '', evidence: {}, live: true,
      });
      d.events.push({ type: 'incident', incident });
      const tr = n.availability?.tracker30;
      if (tr?.incidents) { tr.incidents[tr.incidents.length - 1] = (tr.incidents[tr.incidents.length - 1] ?? 0) + 1; (d.nodes[n.id] ??= {})['availability.tracker30'] = tr; }
    }
    model.get(n.id).event = { code, reason, check, recoverAt: t + dur, incident };
    d.events.push({ type: 'status', id: n.id, from, to: code, reason, check });
  }

  function endEvent(n, t) {
    const m = model.get(n.id);
    const ev = m.event;
    m.event = null;
    const from = n.status.code;
    putN(n, 'status', { ...n.status, code: 'ok', word: '正常', reason: null, reason_word: null, since: t, checked_at: t, first_failed_check: null, attention: false, failed_checks: [], summary_counts: { ok: Object.values(n.status.summary_counts ?? { ok: 1 }).reduce((a, b) => a + b, 0) } });
    if (ev.incident) {
      Object.assign(ev.incident, { open: false, resolved_at: t, last_seen: t, duration_s: t - ev.incident.opened_at });
      d.events.push({ type: 'incident', incident: ev.incident });
    }
    d.events.push({ type: 'status', id: n.id, from, to: 'ok' });
  }

  function minuteRollover(t) {
    if (!fleet) return;
    let appended = 0;
    // a bucket for minute [s, s+60) is appended once t ≥ s + 60
    while (t >= fleet.t0 + fleet.down.length * 60 + 60 && appended < 60) {
      const down = Math.round(minuteAcc.down), up = Math.round(minuteAcc.up);
      const stamp = fleet.t0 + fleet.down.length * 60;
      fleet.down.push(down); fleet.up.push(up); fleet.down.shift(); fleet.up.shift(); fleet.t0 += 60;
      if (fleet.total) { fleet.total.down += down; fleet.total.up += up; }
      if (fleet.peak_down && (down * 8) / 60 > fleet.peak_down.bps) fleet.peak_down = { bps: Math.round((down * 8) / 60), bytes: down, t: stamp };
      if (fleet.peak_up && (up * 8) / 60 > fleet.peak_up.bps) fleet.peak_up = { bps: Math.round((up * 8) / 60), bytes: up, t: stamp };
      if (spark) {
        spark.down_bps.push(Math.round((down * 8) / 60)); spark.up_bps.push(Math.round((up * 8) / 60));
        spark.down_bps.shift(); spark.up_bps.shift(); spark.t0 += 60;
        d.fleet['kpis.business_rate.spark'] = 'append';
      }
      minuteAcc.down = 0; minuteAcc.up = 0;
      appended++;
    }
    if (appended) d.fleet['traffic.fleet_24h'] = appended;
  }

  function calendarRollover(n, t) {
    const tr = n.traffic;
    if (!tr) return;
    const h = tr.hourly24;
    if (h) {
      let shifted = false;
      while (t >= h.t0 + h.down.length * h.step) {
        h.down.shift(); h.up.shift(); h.down.push(0); h.up.push(0); h.t0 += h.step; shifted = true;
      }
      if (shifted) (d.nodes[n.id] ??= {})['traffic.hourly24'] = h;
    }
    if (tr.day_start != null && t >= tr.day_start + 86400) {
      putN(n, 'traffic.day_start', tr.day_start + 86400 * Math.floor((t - tr.day_start) / 86400));
      putN(n, 'traffic.today', { down: 0, up: 0 });
    }
  }

  // ── one step ──
  function step({ catchUp = false } = {}) {
    const t = store.now();
    const dt = Math.max(0, Math.min(maxCatchUpS, t - lastSim));
    lastSim = t;
    d = { seq: ++seq, now: t, dt, catchUp, nodes: {}, relays: {}, entries: {}, fleet: {}, events: [] };

    // status: recoveries, then maybe a new event (at most one per step)
    for (const n of store.nodes()) {
      const ev = model.get(n.id)?.event;
      if (ev && t >= ev.recoverAt) endEvent(n, t);
    }
    if (eventRate > 0 && r() < Math.min(1, (eventRate * dt) / 300)) startEvent(t);

    for (const n of store.nodes()) calendarRollover(n, t);

    // collectors (10 s per relay)
    for (const [name, due] of nextCollect) {
      if (t < due) continue;
      collect(name, t);
      nextCollect.set(name, due + 10 * Math.max(1, Math.ceil((t - due + 0.001) / 10)));
    }
    // host metrics (Komari, every tick)
    if (dt > 0) for (const n of store.nodes()) if (n.hasHost) hostTick(n, t, dt);
    // probes
    for (const n of store.nodes()) {
      const m = model.get(n.id);
      if (m.tcpBase != null && t >= m.nextTcp) { probe(n, t, 'tcp'); m.nextTcp = t + 30; }
      if (m.egBase != null && t >= m.nextEgress) { probe(n, t, 'egress'); m.nextEgress = t + 300; }
      if (m.pathBase != null && t >= m.nextPath) {
        const slow = m.event && m.event.code !== 'unknown' ? 2 + r() * 2 : 1;
        putN(n, 'latency.path_ms', m.event?.code === 'fail' ? null : round1(m.pathBase * Math.exp(0.15 * r.normal()) * slow));
        putN(n, 'latency.path_at', t);
        m.nextPath = t + 30;
      }
    }
    // monitor evaluation (10 s): checked_at, attention flips for degraded ≥ 5 min
    if (t >= nextMonitor) {
      for (const n of store.nodes()) {
        if (!n.status) continue;
        putN(n, 'status.checked_at', t);
        const shouldAttend = n.status.code === 'fail' || (n.status.code === 'degraded' && t - n.status.since >= 300);
        if (!!n.status.attention !== shouldAttend) { putN(n, 'status.attention', shouldAttend); d.events.push({ type: 'attention', id: n.id, attention: shouldAttend }); }
      }
      if (raw.system?.monitor) raw.system.monitor.checked_at = t;
      last.monitor = t;
      nextMonitor = t + 10;
    }
    // relay panels (15 s)
    for (const rl of store.relays()) {
      if (t >= nextPanel.get(rl.id) && rl.panel) {
        put('relays', rl.id, rl, 'panel.checked_at', t);
        put('relays', rl.id, rl, 'panel.cpu_pct', round2(clamp((rl.host?.cpu_pct ?? rl.panel.cpu_pct) * (0.9 + r() * 0.2), 0, 100)));
        put('relays', rl.id, rl, 'panel.online_clients', store.entriesFor(rl.id).filter((e) => (e.rate_down_bps ?? 0) + (e.rate_up_bps ?? 0) > 0).length);
        nextPanel.set(rl.id, t + 15);
        last.panel = t;
      }
      // relay rows share host/bandwidth with their node rows; mirror the node diff
      const nd = d.nodes[rl.id];
      if (nd) for (const k in nd) if (k.startsWith('host.') || k.startsWith('bandwidth.')) (d.relays[rl.id] ??= {})[k] = nd[k];
      if (rl.rate && rl.host) { put('relays', rl.id, rl, 'rate.rx_bps', rl.host.net_rx_bps); put('relays', rl.id, rl, 'rate.tx_bps', rl.host.net_tx_bps); }
      if (rl.host) put('relays', rl.id, rl, 'tcp_connections', rl.host.tcp_conns);
      if (rl.traffic_quota && rl.node?.traffic?.quota) put('relays', rl.id, rl, 'traffic_quota.used_bytes', rl.node.traffic.quota.used_bytes);
    }
    // fleet counters
    if (raw.kpis && dt > 0) {
      const dayStart = raw.kpis.today_business?.since;
      if (dayStart != null && t >= dayStart + 86400) {
        raw.kpis.today_business.since = dayStart + 86400 * Math.floor((t - dayStart) / 86400);
        raw.kpis.events_today = 0;
      }
      const add = Math.round(2.55 * dt * (0.7 + r() * 0.6));
      raw.kpis.events_today = (raw.kpis.events_today ?? 0) + add;
      d.fleet['kpis.events_today'] = raw.kpis.events_today;
    }
    minuteRollover(t);
    if (Object.keys(d.nodes).length || d.events.length) d.fleet.kpis = true;

    const out = d;
    d = null;
    lastTickReal = perfNow();
    const empty = !Object.keys(out.nodes).length && !Object.keys(out.relays).length && !Object.keys(out.entries).length && !out.events.length && !Object.keys(out.fleet).length;
    if (!empty) {
      store.emit('change', out);
      for (const fn of listeners) fn(out);
    }
    return out;
  }

  const api = {
    /** Start ticking in real time (the simulated clock runs continuously from here). */
    start() {
      if (timer) return api;
      if (realAnchor == null) realAnchor = perfNow();
      lastTickReal = perfNow();
      timer = setInterval(() => step(), intervalMs);
      if (pauseWhenHidden && typeof document !== 'undefined' && !visHandler) {
        visHandler = () => {
          if (document.visibilityState === 'hidden') { clearInterval(timer); timer = null; }
          else if (!timer) { step({ catchUp: true }); timer = setInterval(() => step(), intervalMs); }
        };
        document.addEventListener('visibilitychange', visHandler);
      }
      return api;
    },
    /** Stop ticking and freeze the simulated clock. */
    stop() {
      if (timer) clearInterval(timer);
      timer = null;
      if (realAnchor != null) { offset += (perfNow() - realAnchor) / 1000; realAnchor = null; }
      if (visHandler) { document.removeEventListener('visibilitychange', visHandler); visHandler = null; }
      return api;
    },
    get running() { return !!timer; },
    /** Advance the simulated clock by `dtSeconds` (manual / test mode) and run one step. */
    tick(dtSeconds = intervalMs / 1000) {
      offset += dtSeconds;
      return step();
    },
    /** Subscribe to diffs; returns unsubscribe. (The store also emits 'change'.) */
    on(fn) { listeners.add(fn); return () => listeners.delete(fn); },
    setEventRate(v) { eventRate = v; },
    /** Force a status transition now on a random healthy node (demo button). */
    triggerEvent() { d = { seq: ++seq, now: store.now(), dt: 0, nodes: {}, relays: {}, entries: {}, fleet: { kpis: true }, events: [] }; startEvent(store.now()); const out = d; d = null; store.emit('change', out); for (const fn of listeners) fn(out); return out; },
    /** Seconds since the last tick (real time) — drives the FreshnessPill between ticks. */
    age() { return lastTickReal == null ? null : (perfNow() - lastTickReal) / 1000; },
    /** Last update instant per data source (simulated Unix s): komari · xray · monitor · panel. */
    sources: () => ({ ...last }),
    clock,
  };
  return api;
}

const round1 = (v) => Math.round(v * 10) / 10;
const round2 = (v) => Math.round(v * 100) / 100;
const round3 = (v) => Math.round(v * 1000) / 1000;
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
