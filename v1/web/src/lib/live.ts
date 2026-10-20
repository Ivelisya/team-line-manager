// 实时指标：实时流的 metrics 消息（服务端最多每 3 s 一条，只带变了的字段）直接写进机器类查询缓存，不重取；
// 同时攒实时迹线（每台机器、全站各最近 30 个速率点），卡片点阵与集群状态折线用 useLiveSeries。
// 消息形状见 docs/API.md「实时流」：host、tcp/egress 是定长数组（时间为 Unix 秒），其余字段与 REST 同形。
import { useSyncExternalStore } from 'react';
import type { QueryClient } from '@tanstack/react-query';
import type { EntryView, HostMetrics, MachineView, Overview, ProbeResult } from './types';
import { isQuiet } from './derive';

type N = number | null;
/** [ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bps, net_tx_bps, tcp_conns] */
type HostTuple = [number, N, N, N, N, N, N, N, N, N];
/** [ok, latency_ms, ts, detail] */
type ProbeTuple = [boolean, N, number, string | null];
interface MachinePatch {
  host?: HostTuple | null; seen?: N; agent?: { version: string | null; online: boolean; xray_running: boolean | null; cores: number | null } | null;
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
  // 合并进原对象：增量不带 last_report_at（由 seen 给）
  if (d.agent !== undefined) n.agent = d.agent && { last_report_at: null, ...m.agent, ...d.agent };
  if (d.seen !== undefined && n.agent) n.agent = { ...n.agent, last_report_at: d.seen == null ? null : iso(d.seen) };
  if (d.health) n.health = d.health;
  if (d.maintenance !== undefined) n.maintenance = d.maintenance;
  if (d.due_days != null && m.subscription) n.subscription = { ...m.subscription, due_days: d.due_days };
  if (d.removed_traffic) n.removed_traffic = d.removed_traffic;
  const h = d.host;
  if (h && h[7] != null && h[8] != null && !isQuiet(n)) record(m.id, { t: h[0] * 1000, rx: h[7], tx: h[8] });
  return n;
}

const patchList = (msg: MetricsMsg) => (old: MachineView[] | undefined) => {
  if (!old) return old;
  let hit = false;
  const out = old.map((m) => { const n = patchMachine(m, msg); hit ||= n !== m; return n; });
  return hit ? out : old;
};

/** metrics 写的这几类查询 */
export const LIVE_KEYS = [['machines'], ['machine'], ['relays'], ['overview']];

export function applyMetrics(qc: QueryClient, msg: MetricsMsg) {
  // 正在重取的：稍后回来的响应是增量之前的快照，会把这次写进去的盖掉（健康、在线等不常变的字段要等下次变化才纠正）。
  // 打断重取：服务端广播前已把新视图存进缓存，新请求带上这次变化
  for (const k of LIVE_KEYS) {
    for (const q of qc.getQueryCache().findAll({ queryKey: k, fetchStatus: 'fetching' })) {
      void qc.invalidateQueries({ queryKey: q.queryKey, exact: true, refetchType: 'all' }, { cancelRefetch: true });
    }
  }
  const list = patchList(msg);
  qc.setQueriesData<MachineView[]>({ queryKey: ['machines'] }, list);
  qc.setQueriesData<MachineView[]>({ queryKey: ['relays'] }, list);
  qc.setQueriesData<MachineView>({ queryKey: ['machine'] }, (m) => m && patchMachine(m, msg));
  if (msg.overview) {
    qc.setQueryData(['overview'], msg.overview);
    record(FLEET, { t: Date.now(), rx: msg.overview.net_rx_bps, tx: msg.overview.net_tx_bps });
  }
  if (dirty) { dirty = false; subs.forEach((f) => f()); }
}

// ---- 实时迹线：每台机器的网卡速率、全站总速率（FLEET），各留最近 SERIES_N 个点 ----
// 只由 metrics 消息喂：机器按上报时间（同一份上报在几份缓存里各 patch 一次，只记一次），总览按收到的时刻。
// 数据过期（isQuiet）的机器不记；页面在后台时不写缓存也就不记，所以可能有断档。
export interface LivePoint { t: number; rx: number; tx: number }
export const SERIES_N = 30;
/** 全站总速率在迹线里的键 */
export const FLEET = '*';
const EMPTY: readonly LivePoint[] = [];
const series = new Map<string, readonly LivePoint[]>();
const subs = new Set<() => void>();
const subscribe = (f: () => void) => { subs.add(f); return () => { subs.delete(f); }; };
let dirty = false;

function record(key: string, p: LivePoint) {
  const buf = series.get(key) ?? EMPTY;
  if (buf.length && buf[buf.length - 1].t >= p.t) return;
  series.set(key, [...buf.slice(-(SERIES_N - 1)), p]); // 新数组：订阅者按引用判断变化
  dirty = true;
}

/** 某台机器（或 FLEET）最近的速率点（时间升序，t 为毫秒） */
export function useLiveSeries(key: string): readonly LivePoint[] {
  return useSyncExternalStore(subscribe, () => series.get(key) ?? EMPTY, () => EMPTY);
}
