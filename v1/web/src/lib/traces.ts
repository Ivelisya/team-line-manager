// 实时迹线：每台机器的网卡速率、全站总速率，各留最近 TRACE_N 个点（纯客户端环形缓冲）。
// 不自己发请求，也不挂在哪个页面上：订阅 QueryCache，机器列表 / 单台机器 / 线路机列表 / 总览每拿到一份新数据就记一个点。
// 数据是轮询来的、实时流失效后重取来的，还是以后由 metrics 推送直接 setQueryData 写进来的，这里都一样处理。
// 去重：机器按上报时间 host.ts（同一份上报只记一次）；总览按查询的 dataUpdatedAt。
import { useSyncExternalStore } from 'react';
import { useQueryClient, type QueryClient } from '@tanstack/react-query';
import type { MachineView, Overview } from './types';

export interface TracePoint { t: number; rx: number; tx: number }
export const TRACE_N = 30;
/** 全站总速率在缓冲里的键 */
export const FLEET = '*';

const buf = new Map<string, TracePoint[]>();
const stamps = new Map<string, string | number>();
const listeners = new Set<() => void>();
const EMPTY: TracePoint[] = [];

function push(key: string, stamp: string | number, p: TracePoint): boolean {
  if (stamps.get(key) === stamp) return false;
  stamps.set(key, stamp);
  const prev = buf.get(key) ?? EMPTY;
  buf.set(key, [...(prev.length >= TRACE_N ? prev.slice(prev.length - TRACE_N + 1) : prev), p]); // 新数组：订阅者按引用判断变化
  return true;
}

function ingest(key: readonly unknown[], data: unknown, at: number) {
  let changed = false;
  if (key[0] === 'overview') {
    const o = data as Overview;
    changed = push(FLEET, at, { t: at, rx: o.net_rx_bps, tx: o.net_tx_bps });
  } else if (key[0] === 'machines' || key[0] === 'relays' || key[0] === 'machine') {
    for (const m of (Array.isArray(data) ? data : [data]) as MachineView[]) {
      const h = m.host;
      if (h?.ts && h.net_rx_bps != null && h.net_tx_bps != null && push(m.id, h.ts, { t: Date.parse(h.ts), rx: h.net_rx_bps, tx: h.net_tx_bps })) changed = true;
    }
  }
  if (changed) listeners.forEach((l) => l());
}

let installed: QueryClient | null = null;
function install(qc: QueryClient) {
  if (installed === qc) return;
  installed = qc;
  const cache = qc.getQueryCache();
  for (const q of cache.getAll()) if (q.state.data != null) ingest(q.queryKey, q.state.data, q.state.dataUpdatedAt);
  cache.subscribe((e) => {
    if (e.type === 'updated' && e.action.type === 'success' && e.query.state.data != null) ingest(e.query.queryKey, e.query.state.data, e.query.state.dataUpdatedAt);
  });
}

const subscribe = (l: () => void) => { listeners.add(l); return () => { listeners.delete(l); }; };

/** 读某台机器（或 FLEET）的迹线；第一次调用时接上 QueryCache（幂等） */
export function useTrace(key: string): TracePoint[] {
  install(useQueryClient());
  return useSyncExternalStore(subscribe, () => buf.get(key) ?? EMPTY);
}
