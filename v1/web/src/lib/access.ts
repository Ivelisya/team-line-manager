// 访问监控：类型（对应 docs/API.md「访问监控」）+ 查询 hook。数据 30 秒轮询，页面不在前台时 TanStack Query 自动暂停。
import { keepPreviousData, useInfiniteQuery, useQuery, type QueryClient } from '@tanstack/react-query';
import { get } from './api';
import type { IpIntel } from './ipintel';

export type AccessWindow = '1h' | '24h' | '7d' | '30d';
export const ACCESS_WINDOWS: { value: AccessWindow; label: string; long: string }[] = [
  { value: '1h', label: '1 小时', long: '1 小时' }, { value: '24h', label: '24 小时', long: '24 小时' },
  { value: '7d', label: '7 天', long: '7 天' }, { value: '30d', label: '30 天', long: '30 天' },
];
export const isAccessWindow = (v: string | null): v is AccessWindow => ACCESS_WINDOWS.some((w) => w.value === v);

export interface AccessFilter {
  window: AccessWindow; team: string; node: string; search: string;
  kind: '' | 'domain' | 'ip'; action: '' | 'routed' | 'blocked';
}
export type TargetSort = 'conns' | 'last' | 'first';

export interface AccessKpi { active_nodes: number; conns: number; targets: number; blocked: number }
export interface AccessNode { node_id: string; name: string; team: string | null; conns: number; targets: number; domains: number; sources: number; blocked: number; last_ts: string | null }
export interface AccessSummary { kpi: AccessKpi; nodes: AccessNode[]; window_start: string }
export interface AccessTarget {
  node_id: string; name: string; dest_host: string; kind: 'domain' | 'ip'; conns: number; sources: number; blocked: number;
  first_seen: string | null; last_seen: string | null;
}
export interface AccessTargets { items: AccessTarget[]; next_offset: number | null; window_start: string }
/** intel：来源画像（运营商 / 城市 / 接入方式…），IP 库没装或查不到时为 null */
export interface AccessSource { node_id: string; name: string; src_ip: string; conns: number; last_ts: string | null; intel?: IpIntel | null }
export interface AccessEvent { ts: string; node_id: string; name: string; src_ip: string | null; dest_host: string; dest_port: number | null; network: string | null; blocked: boolean }
export interface AccessEvents { items: AccessEvent[]; next_before: string | null }
export interface NewTarget { node_id: string; name: string; dest_host: string; kind: 'domain' | 'ip'; first_seen: string; last_seen: string; total: number }
export interface MachineAccess {
  window_start: string;
  top_domains: { dest_host: string; kind: 'domain' | 'ip'; conns: number; blocked: number; sources: number }[];
  top_sources: AccessSource[]; recent: AccessEvent[]; hourly: { hour: string; conns: number; blocked: number }[];
}

const POLL = 30_000;

/** 无限查询加载超过 1 页后停止自动轮询：轮询会按顺序重拉所有已加载的页，且 offset 分页在排序键变化时会重复/漏行。
 *  这时由页面提示“已暂停自动刷新”，用 refreshFirstPage 手动回到第一页。 */
const pollFirstPageOnly = (q: { state: { data?: { pages: unknown[] } } }) => ((q.state.data?.pages.length ?? 0) > 1 ? false : POLL);
export const pagesLoaded = (d: { pages: unknown[] } | undefined) => d?.pages.length ?? 0;
/** 只保留第一页并重新拉取（手动刷新） */
export function refreshFirstPage(qc: QueryClient, queryKey: readonly unknown[]) {
  qc.setQueryData(queryKey, (d: { pages: unknown[]; pageParams: unknown[] } | undefined) => (d ? { pages: d.pages.slice(0, 1), pageParams: d.pageParams.slice(0, 1) } : d));
  void qc.invalidateQueries({ queryKey });
}

/** 筛选条件 → 查询串；空值不带 */
export function accessQS(f: AccessFilter, extra: Record<string, string | number | undefined> = {}): string {
  const p = new URLSearchParams({ window: f.window });
  if (f.team) p.set('team', f.team);
  if (f.node) p.set('node', f.node);
  if (f.search.trim()) p.set('search', f.search.trim());
  if (f.kind) p.set('kind', f.kind);
  if (f.action) p.set('action', f.action);
  for (const [k, v] of Object.entries(extra)) if (v !== undefined && v !== '') p.set(k, String(v));
  return p.toString();
}

export const useAccessSummary = (f: AccessFilter) =>
  useQuery({ queryKey: ['access', 'summary', f], queryFn: ({ signal }) => get<AccessSummary>(`/access/summary?${accessQS(f)}`, signal), refetchInterval: POLL, placeholderData: keepPreviousData });

export const useAccessTargets = (f: AccessFilter, sort: TargetSort, enabled = true) =>
  useInfiniteQuery({
    queryKey: ['access', 'targets', f, sort], enabled, initialPageParam: 0, refetchInterval: pollFirstPageOnly, placeholderData: keepPreviousData,
    queryFn: ({ pageParam, signal }) => get<AccessTargets>(`/access/targets?${accessQS(f, { sort, limit: 100, offset: pageParam })}`, signal),
    getNextPageParam: (last) => last.next_offset ?? undefined,
  });

export const useNewTargets = (f: AccessFilter, enabled = true) =>
  useQuery({
    queryKey: ['access', 'new', f], enabled, refetchInterval: POLL, placeholderData: keepPreviousData,
    queryFn: ({ signal }) => get<{ items: NewTarget[]; next_offset: number | null }>(`/access/new-targets?${accessQS(f, { limit: 200 })}`, signal),
  });

export const useAccessSources = (f: AccessFilter, enabled = true) =>
  useQuery({
    queryKey: ['access', 'sources', f], enabled, refetchInterval: POLL, placeholderData: keepPreviousData,
    queryFn: ({ signal }) => get<{ items: AccessSource[] }>(`/access/sources?${accessQS(f, { limit: 200 })}`, signal),
  });

export const useAccessEvents = (f: AccessFilter, enabled = true) =>
  useInfiniteQuery({
    queryKey: ['access', 'events', f], enabled, initialPageParam: '', refetchInterval: pollFirstPageOnly, placeholderData: keepPreviousData,
    queryFn: ({ pageParam, signal }) => get<AccessEvents>(`/access/events?${accessQS(f, { limit: 100, before: pageParam })}`, signal),
    getNextPageParam: (last) => last.next_before ?? undefined,
  });

export const useMachineAccess = (id: string | null, window: AccessWindow) =>
  useQuery({ queryKey: ['access', 'machine', id, window], enabled: !!id, refetchInterval: POLL, placeholderData: keepPreviousData, queryFn: ({ signal }) => get<MachineAccess>(`/machines/${id}/access?window=${window}`, signal) });
