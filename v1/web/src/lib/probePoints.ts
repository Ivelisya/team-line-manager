// 探测点与主探测来源（对应 docs/API.md「探测点」）。线路机卡片的心跳格、入口表的「连不上」、relay_unreachable 告警看主探测来源那一路：
// 中控，或放在靠近客户的地方（如香港）的探测点。改动由实时流的 settings 事件失效（eventKeys）。
import { keepPreviousData, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { del, get, patch, post, put } from './api';

/** 此刻实际用哪一路：central 中控；probe 探测点；fallback 主来源的探测点离线、暂由中控代替（连不上照报、连得上不算恢复）；none 标灰 */
export interface ReachEffective { state: 'central' | 'probe' | 'fallback' | 'none'; probe_point_id: string | null; label: string; primary_label: string | null }
export interface ReachSource { primary: 'central' | string; primary_name: string; central_enabled: boolean; effective: ReachEffective; offline_after_s: number }
export interface ProbeRelayStat { relay_id: string; name: string; avg_ms: number | null; median_ms: number | null; total: number; ok: number }
/** 到各线路机的统计窗口：1h 读原始记录，其余读小时汇总 */
export type ProbeWindow = '1h' | '24h' | '7d' | '30d';
/** blind：在线、但最近 3 分钟的检查都作废了（对照目标连不上，discard_reason），同离线处理 */
export interface ProbePoint {
  id: string; name: string; label: string; enabled: boolean; note: string; state: 'online' | 'blind' | 'offline' | 'pending' | 'disabled'; installed: boolean; primary: boolean;
  last_seen_at: string | null; last_kept_at: string | null; discard_reason: string | null; window: ProbeWindow;
  last_ip: string | null; version: string | null; created_at: string; bootstrap_expires_at: string | null;
  install: { state: 'running' | 'done' | 'failed'; step: string | null; error: string | null; host: string | null; port: number | null; started_at: string | null; finished_at: string | null } | null;
  upgrade: { sha256: string; version: string | null } | null;
  /** 上次升级失败（下载后 5 分钟没报上新版本，探测点已换回旧版本）：中控不再下发，要手动再升级 */
  upgrade_failed: { sha256: string; to: string | null; from: string | null; at: string } | null;
  relays: ProbeRelayStat[];
}
export interface Bootstrap { cmd: string; expires_at: string }

export const useReachSource = () => useQuery({ queryKey: ['reach-source'], queryFn: ({ signal }) => get<ReachSource>('/settings/reach-source', signal), staleTime: 60_000 });
/** 换窗口时先留着上一份，不闪骨架 */
export const useProbePoints = (window: ProbeWindow = '1h') =>
  useQuery({ queryKey: ['probe-points', window], queryFn: ({ signal }) => get<ProbePoint[]>(`/probe-points?window=${window}`, signal), staleTime: 30_000, refetchInterval: 30_000, placeholderData: keepPreviousData });

/** 线路机抽屉「连接质量」：各路来源（中控、各探测点）近 24 小时 / 7 天 / 30 天连客户入口的统计（小时汇总，每 5 分钟重算），抽屉开着时 10 分钟重取一次 */
export interface ReachWindowStat { total: number; ok: number; avg_ms: number | null; median_ms: number | null }
export interface ReachQualitySource { source: 'central' | string; label: string; primary: boolean; windows: Record<'24h' | '7d' | '30d', ReachWindowStat | null> }
export const useReachQuality = (id: string) =>
  useQuery({ queryKey: ['reach-quality', id], queryFn: ({ signal }) => get<{ sources: ReachQualitySource[] }>(`/machines/${id}/reach-quality`, signal), staleTime: 600_000, refetchInterval: 600_000, refetchOnWindowFocus: false });

/** 卡片、入口表里「谁连不上」的主语：中控 / 香港探测点 / 中控（代替…）。还没取到时按中控 */
export const useReachLabel = () => useQuery({ queryKey: ['reach-source'], queryFn: ({ signal }) => get<ReachSource>('/settings/reach-source', signal), staleTime: 60_000, select: (s) => s.effective }).data;

function useInvalidate() {
  const qc = useQueryClient();
  return () => { for (const k of [['probe-points'], ['reach-source'], ['probe-beats']]) void qc.invalidateQueries({ queryKey: k }); };
}

export function useSaveReachSource() {
  const qc = useQueryClient(); const inv = useInvalidate();
  return useMutation({ mutationFn: (b: { primary?: string; central_enabled?: boolean }) => put<ReachSource>('/settings/reach-source', b), onSuccess: (d) => { qc.setQueryData(['reach-source'], d); inv(); } });
}
export function useAddProbePoint() {
  const inv = useInvalidate();
  return useMutation({ mutationFn: (b: { name: string; note?: string; ssh?: { host: string; port?: number } }) => post<{ id: string; bootstrap?: Bootstrap }>('/probe-points', b), onSuccess: inv });
}
export function useReinstallProbePoint() {
  const inv = useInvalidate();
  return useMutation({ mutationFn: (id: string) => post<{ id: string; bootstrap?: Bootstrap }>(`/probe-points/${id}/install`, {}), onSuccess: inv });
}
export function usePatchProbePoint() {
  const inv = useInvalidate();
  return useMutation({ mutationFn: ({ id, ...b }: { id: string; enabled?: boolean; name?: string; note?: string }) => patch<ProbePoint>(`/probe-points/${id}`, b), onSuccess: inv });
}
export function useDelProbePoint() {
  const inv = useInvalidate();
  return useMutation({ mutationFn: (id: string) => del<{ ok: boolean; cleanup: string }>(`/probe-points/${id}`), onSuccess: inv });
}
export function useUpgradeProbePoint() {
  const inv = useInvalidate();
  return useMutation({ mutationFn: (id: string) => post<{ version: string; from: string | null }>(`/probe-points/${id}/upgrade`, {}), onSuccess: inv });
}
