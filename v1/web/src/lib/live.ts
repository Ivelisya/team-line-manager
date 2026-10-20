// 实时指标：实时流的 metrics 消息（服务端最多每 3 s 一条，只带变了的字段）直接写进机器类查询缓存，不重取；
// 同时给每台机器攒最近 60 个网卡读数（≈10 分钟，agent 每 10 s 一报），卡片迷你曲线用 useLiveSeries。
// 消息形状见 docs/API.md「实时流」：host、tcp/egress 是定长数组（时间为 Unix 秒），其余字段与 REST 同形。
import { useSyncExternalStore } from 'react';
import type { QueryClient } from '@tanstack/react-query';
import type { EntryView, HostMetrics, MachineView, Overview, ProbeResult } from './types';

type N = number | null;
/** [ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bps, net_tx_bps, tcp_conns] */
type HostTuple = [number, N, N, N, N, N, N, N, N, N];
/** [ok, latency_ms, ts, detail] */
type ProbeTuple = [boolean, N, number, string | null];
interface MachinePatch {
  host?: HostTuple | null; seen?: N; agent?: { version: string | null; online: boolean; xray_running: boolean | null } | null;
  health?: MachineView['health']; maintenance?: boolean; due_days?: N; removed_traffic?: MachineView['removed_traffic'];
}
interface EntryPatch { tcp?: ProbeTuple | null; egress?: ProbeTuple | null; traffic?: EntryView['traffic'] }
export interface MetricsMsg { type: 'metrics'; machines: Record<string, MachinePatch>; entries: Record<string, EntryPatch>; overview?: Overview }

const iso = (s: number) => new Date(s * 1000).toISOString();
const host = (h: HostTuple): HostMetrics => ({
  ts: iso(h[0]), cpu_pct: h[1], load1: h[2], mem_used: h[3], mem_total: h[4], disk_used: h[5], disk_total: h[6], net_rx_bps: h[7], net_tx_bps: h[8], tcp_conns: h[9],
});
const probe = (p: ProbeTuple | null): ProbeResult | null => p && { ok: p[0], latency_ms: p[1], ts: iso(p[2]), detail: p[3] };

function patchEntry(e: EntryView, d: EntryPatch | undefined): EntryView {
  if (!d) return e;
  const n = { ...e };
  if (d.tcp !== undefined) n.tcp = probe(d.tcp);
  if (d.egress !== undefined) n.egress = probe(d.egress);
  if (d.traffic) n.traffic = d.traffic;
  return n;
}

/** 没变的机器原样返回（引用不变，卡片 memo 不被击穿） */
function patchMachine(m: MachineView, msg: MetricsMsg): MachineView {
  const d = msg.machines[m.id];
  const entries = m.entries.some((e) => msg.entries[e.id]) ? m.entries.map((e) => patchEntry(e, msg.entries[e.id])) : m.entries;
  if (!d && entries === m.entries) return m;
  const n = { ...m, entries };
  if (!d) return n;
  if (d.host !== undefined) n.host = d.host && host(d.host);
  if (d.agent !== undefined) n.agent = d.agent && { last_report_at: m.agent?.last_report_at ?? null, ...d.agent };
  if (d.seen !== undefined && n.agent) n.agent = { ...n.agent, last_report_at: d.seen == null ? null : iso(d.seen) };
  if (d.health) n.health = d.health;
  if (d.maintenance !== undefined) n.maintenance = d.maintenance;
  if (d.due_days != null && m.subscription) n.subscription = { ...m.subscription, due_days: d.due_days };
  if (d.removed_traffic) n.removed_traffic = d.removed_traffic;
  return n;
}

const patchList = (msg: MetricsMsg) => (old: MachineView[] | undefined) => {
  if (!old) return old;
  let hit = false;
  const out = old.map((m) => { const n = patchMachine(m, msg); hit ||= n !== m; return n; });
  return hit ? out : old;
};

export function applyMetrics(qc: QueryClient, msg: MetricsMsg) {
  const list = patchList(msg);
  qc.setQueriesData<MachineView[]>({ queryKey: ['machines'] }, list);
  qc.setQueriesData<MachineView[]>({ queryKey: ['relays'] }, list);
  qc.setQueriesData<MachineView>({ queryKey: ['machine'] }, (m) => m && patchMachine(m, msg));
  if (msg.overview) qc.setQueryData(['overview'], msg.overview);
  pushSeries(msg);
}

// ---- 每台机器最近的网卡读数 ----
export interface LivePoint { t: number; rx: N; tx: N }
const SERIES_N = 60;
const EMPTY: readonly LivePoint[] = [];
const series = new Map<string, readonly LivePoint[]>();
const subs = new Set<() => void>();
const subscribe = (f: () => void) => { subs.add(f); return () => { subs.delete(f); }; };

function pushSeries(msg: MetricsMsg) {
  let changed = false;
  for (const [id, d] of Object.entries(msg.machines)) {
    if (!d.host) continue;
    const t = d.host[0] * 1000;
    const buf = series.get(id) ?? EMPTY;
    if (buf.length && buf[buf.length - 1].t >= t) continue;
    series.set(id, [...buf.slice(-(SERIES_N - 1)), { t, rx: d.host[7], tx: d.host[8] }]);
    changed = true;
  }
  if (changed) subs.forEach((f) => f());
}

/** 这台机器连上实时流以来最近 60 个网卡读数（时间升序，t 为毫秒）；页面在后台时不攒，所以可能有断档 */
export function useLiveSeries(id: string | null): readonly LivePoint[] {
  return useSyncExternalStore(subscribe, () => (id && series.get(id)) || EMPTY, () => EMPTY);
}
