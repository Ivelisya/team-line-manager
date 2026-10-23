// 三网 PING（对应 docs/API.md「三网 PING」）：线路机每分钟往外测一轮线路质量（延迟、丢包），只展示、不告警。
// 与探测点（lib/probePoints）分工：探测点管客户连不连得上入口（可用性、告警），这里看线路好不好。
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { get, put } from './api';
import type { BeatCell } from './derive';

export interface PingSlot { name: string; target: string }
export interface PingGlobal { mode: 'tcp' | 'icmp'; slots: PingSlot[]; modes: string[]; interval_s: number; count: number; tcp_default_port: number; defaults: { mode: string; slots: PingSlot[] } }
/** 卡片一行：近 1 小时 20 格（3 分钟一格，同心跳格），每格 [延迟中位数 ms, 丢包 %]，没有数据为 null */
export interface PingRow { slot: number; name: string; target: string; mode: string; cur_ms: number | null; cur_loss: number | null; cur_ts: string | null; cells: [number | null, number | null][] }
export type PingBeats = Record<string, PingRow[]>;
export interface PingOverride { mode?: 'tcp' | 'icmp' | null; slots?: (PingSlot | null)[] }
export interface MachinePing { mode: string; slots: (PingSlot & { slot: number; source: 'global' | 'override' })[]; override: PingOverride | null; global: { mode: string; slots: PingSlot[] }; current: PingRow[] }
export interface PingSeries { range: string; step_s: number; series: { slot: number; name: string; target: string; points: { ts: string; rtt_ms: number | null; loss_pct: number | null }[] }[] }

/** BGP 槽位默认留空，输入框里的占位提示 */
export const PING_PLACEHOLDER: Record<number, string> = { 3: 'ip.zstaticcdn.com' };

// 延迟分档：跨境线路 150–200 ms 是常态，不按国内的标准标红。≤ 220 绿、≤ 320 黄、更高红；丢包 0 绿、≤ 10% 黄、更高红
export const PING_MS_OK = 220;
export const PING_MS_WARN = 320;
export const PING_LOSS_WARN = 10;
export const pingMsTone = (ms: number | null): BeatCell => (ms == null ? null : ms <= PING_MS_OK ? 'ok' : ms <= PING_MS_WARN ? 'warn' : 'bad');
export const pingLossTone = (loss: number | null): BeatCell => (loss == null ? null : loss <= 0 ? 'ok' : loss <= PING_LOSS_WARN ? 'warn' : 'bad');
export const PING_LEGEND = `线路机每分钟往外连一次各目标（每次 5 下），左边是延迟、右边是丢包，3 分钟一格、近 1 小时\n延迟：≤ ${PING_MS_OK} ms 绿 · ≤ ${PING_MS_WARN} ms 黄 · 更高红（跨境 150–200 ms 是常态）\n丢包：0 绿 · ≤ ${PING_LOSS_WARN}% 黄 · 更高红 · 灰：没有数据`;

const pingBeatsQuery = { queryKey: ['ping-beats'], queryFn: ({ signal }: { signal: AbortSignal }) => get<PingBeats>('/ping-beats', signal), staleTime: 60_000, refetchInterval: 60_000 };
/** 卡片共用一个查询，各取自己那份 */
export const usePingRows = (id: string) => useQuery({ ...pingBeatsQuery, select: (d: PingBeats) => d[id] });

export const usePingTargets = () => useQuery({ queryKey: ['ping-targets'], queryFn: ({ signal }) => get<PingGlobal>('/settings/ping-targets', signal), staleTime: Infinity });
export function useSavePingTargets() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: (b: { mode: string; slots: PingSlot[] }) => put<PingGlobal>('/settings/ping-targets', b), onSuccess: (d) => { qc.setQueryData(['ping-targets'], d); void qc.invalidateQueries({ queryKey: ['ping-beats'] }); void qc.invalidateQueries({ queryKey: ['machine-ping'] }); } });
}
export const useMachinePing = (id: string) => useQuery({ queryKey: ['machine-ping', id], queryFn: ({ signal }) => get<MachinePing>(`/machines/${id}/ping`, signal), staleTime: 60_000 });
export function useSaveMachinePing(id: string) {
  const qc = useQueryClient();
  return useMutation({ mutationFn: (o: PingOverride | null) => put<MachinePing>(`/machines/${id}/ping`, { override: o }), onSuccess: (d) => { qc.setQueryData(['machine-ping', id], d); void qc.invalidateQueries({ queryKey: ['ping-beats'] }); } });
}
export const usePingSeries = (id: string, range: string, poll: number) =>
  useQuery({ queryKey: ['ping-series', id, range], queryFn: ({ signal }) => get<PingSeries>(`/machines/${id}/ping-series?range=${range}`, signal), staleTime: poll, refetchInterval: poll, refetchOnWindowFocus: false });
