// 访问监控：类型（对应 docs/API.md「访问监控」）+ 查询 hook。数据 30 秒轮询，页面不在前台时 TanStack Query 自动暂停。
import { keepPreviousData, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { get, put } from './api';
import type { IpIntel } from './ipintel';

export type AccessWindow = '1h' | '24h' | '7d' | '30d';
export const ACCESS_WINDOWS: { value: AccessWindow; label: string; long: string }[] = [
  { value: '1h', label: '1 小时', long: '1 小时' }, { value: '24h', label: '24 小时', long: '24 小时' },
  { value: '7d', label: '7 天', long: '7 天' }, { value: '30d', label: '30 天', long: '30 天' },
];
export const isAccessWindow = (v: string | null): v is AccessWindow => ACCESS_WINDOWS.some((w) => w.value === v);
/** 各时间范围要多少天的小时汇总：超过保留期的范围不给选（查出来是不全的） */
export const WINDOW_DAYS: Record<AccessWindow, number> = { '1h': 1, '24h': 1, '7d': 7, '30d': 30 };

/** 访问记录保留期（设置「数据保留」）：原始记录与小时汇总各保留多少天。改动由实时流的 settings 事件失效 */
/** profile_days：来源画像实际留的天数（汇总天数与 IP 画像学习期取长的），只读 */
export interface AccessRetention { raw_days: number; hourly_days: number; min: number; max: number; profile_days: number }
export const useAccessRetention = () => useQuery({ queryKey: ['access-retention'], queryFn: ({ signal }) => get<AccessRetention>('/settings/access-retention', signal), staleTime: Infinity });
export function useSaveRetention() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: (b: { raw_days: number; hourly_days: number }) => put<AccessRetention>('/settings/access-retention', b), onSuccess: (d) => qc.setQueryData(['access-retention'], d) });
}

export interface AccessFilter {
  window: AccessWindow; team: string; node: string; search: string;
  kind: '' | 'domain' | 'ip'; action: '' | 'routed' | 'blocked';
}
export type TargetSort = 'conns' | 'last' | 'first';

/** `targets` 是各节点分别去重后相加；`unique_targets` 按目标去重（同「域名与 IP」的总数） */
export interface AccessKpi { active_nodes: number; conns: number; targets: number; unique_targets: number; blocked: number }
export interface AccessNode { node_id: string; name: string; team: string | null; conns: number; targets: number; domains: number; sources: number | null; blocked: number; last_ts: string | null }
export interface AccessSummary { kpi: AccessKpi; nodes: AccessNode[]; window_start: string }
export interface AccessTarget {
  node_id: string; name: string; dest_host: string; kind: 'domain' | 'ip'; conns: number; sources: number; blocked: number;
  first_seen: string | null; last_seen: string | null;
}
export interface AccessTargets { items: AccessTarget[]; next_offset: number | null; window_start: string }
/** 按目标合并各节点的一行（/access/targets?group=target）：nodes 是访问过它的节点数，首次 / 最近取各节点里最早 / 最近的；不带来源数 */
export interface AccessTargetGroup { dest_host: string; kind: 'domain' | 'ip'; nodes: number; conns: number; blocked: number; first_seen: string | null; last_seen: string | null }
export interface AccessTargetGroups { items: AccessTargetGroup[]; total: number; next_offset: number | null; window_start: string }
/** intel：来源画像（运营商 / 城市 / 接入方式…），IP 库没装或查不到时为 null */
export interface AccessSource { node_id: string; name: string; src_ip: string; conns: number; last_ts: string | null; intel?: IpIntel | null }
export interface AccessEvent { ts: string; node_id: string; name: string; src_ip: string | null; dest_host: string; dest_port: number | null; network: string | null; blocked: boolean }
/** next_before：不透明游标（下一页传回 before），null = 没有更早的了 */
export interface AccessEvents { items: AccessEvent[]; next_before: string | null }
export interface NewTargets { items: NewTarget[]; total: number; next_offset: number | null; window_start: string }
export interface NewTarget { node_id: string; name: string; dest_host: string; kind: 'domain' | 'ip'; first_seen: string; last_seen: string; total: number }
export interface MachineAccess {
  window_start: string;
  top_domains: { dest_host: string; kind: 'domain' | 'ip'; conns: number; blocked: number; sources: number }[];
  top_sources: AccessSource[]; recent: AccessEvent[]; hourly: { hour: string; conns: number; blocked: number }[];
}

const POLL = 30_000;

/** 筛选条件 → 查询串；空值不带 */
function accessQS(f: AccessFilter, extra: Record<string, string | number | undefined> = {}): string {
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

/** 「域名与 IP」的一页（按目标合并各节点，offset 分页，带总数） */
export const useAccessTargetGroups = (f: AccessFilter, sort: TargetSort, offset: number, limit: number) =>
  useQuery({
    queryKey: ['access', 'targets', 'group', f, sort, offset, limit], refetchInterval: POLL, placeholderData: keepPreviousData,
    queryFn: ({ signal }) => get<AccessTargetGroups>(`/access/targets?${accessQS(f, { group: 'target', sort, limit, offset })}`, signal),
  });

/** 展开一个合并的目标：同样的筛选下只看它，各节点一行（按连接数）。一个目标的节点数不会超过一页上限 */
export const useAccessTargetNodes = (f: AccessFilter, host: string) =>
  useQuery({
    queryKey: ['access', 'targets', 'nodes', f, host], refetchInterval: POLL,
    queryFn: ({ signal }) => get<AccessTargets>(`/access/targets?${accessQS(f, { target: host, limit: 500 })}`, signal),
  });

export const useNewTargets = (f: AccessFilter, offset: number, limit: number, enabled = true) =>
  useQuery({
    queryKey: ['access', 'new', f, offset, limit], enabled, refetchInterval: POLL, placeholderData: keepPreviousData,
    queryFn: ({ signal }) => get<NewTargets>(`/access/new-targets?${accessQS(f, { limit, offset })}`, signal),
  });

/** 来源地址一次最多取这么多（按连接数从多到少）；接口不回总数，取满了就是「前 200 个」 */
export const SOURCES_LIMIT = 200;
export const useAccessSources = (f: AccessFilter, enabled = true) =>
  useQuery({
    queryKey: ['access', 'sources', f], enabled, refetchInterval: POLL, placeholderData: keepPreviousData,
    queryFn: ({ signal }) => get<{ items: AccessSource[] }>(`/access/sources?${accessQS(f, { limit: SOURCES_LIMIT })}`, signal),
  });

/** 原始记录的一页：before 是上一页响应里的 next_before 游标，原样传回、不要自己拼（'' = 最新一页）。只有最新一页轮询，往前翻的页是固定的历史 */
export const EVENTS_PAGE = 100;
export const useAccessEvents = (f: AccessFilter, before: string, enabled = true) =>
  useQuery({
    queryKey: ['access', 'events', f, before], enabled, refetchInterval: before ? false : POLL, placeholderData: keepPreviousData,
    queryFn: ({ signal }) => get<AccessEvents>(`/access/events?${accessQS(f, { limit: EVENTS_PAGE, before })}`, signal),
  });

/** 换时间范围时先留着上一份；换了机器（抽屉里 [ ] 切换）不沿用别台的 */
export const useMachineAccess = (id: string | null, window: AccessWindow) =>
  useQuery<MachineAccess>({ queryKey: ['access', 'machine', id, window], enabled: !!id, refetchInterval: POLL, placeholderData: (prev, prevQuery) => (prevQuery?.queryKey[2] === id ? keepPreviousData(prev) : undefined), queryFn: ({ signal }) => get<MachineAccess>(`/machines/${id}/access?window=${window}`, signal) });
