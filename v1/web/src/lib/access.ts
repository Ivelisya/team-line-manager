// 访问监控：类型（对应 docs/API.md「访问监控」）+ 查询 hook。数据 30 秒轮询，页面不在前台时 TanStack Query 自动暂停。
import { keepPreviousData, useQuery } from '@tanstack/react-query';
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
export interface AccessNode { node_id: string; name: string; team: string | null; conns: number; targets: number; domains: number; sources: number | null; blocked: number; last_ts: string | null }
export interface AccessSummary { kpi: AccessKpi; nodes: AccessNode[]; window_start: string }
export interface AccessTarget {
  node_id: string; name: string; dest_host: string; kind: 'domain' | 'ip'; conns: number; sources: number; blocked: number;
  first_seen: string | null; last_seen: string | null;
}
export interface AccessTargets { items: AccessTarget[]; next_offset: number | null; window_start: string }
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

/** 一页目标（offset 分页）。接口不回总数：总数就是同一筛选下 /access/summary 的 kpi.targets（同一个聚合，各节点去重后相加） */
export const useAccessTargets = (f: AccessFilter, sort: TargetSort, offset: number, limit: number, enabled = true) =>
  useQuery({
    queryKey: ['access', 'targets', f, sort, offset, limit], enabled, refetchInterval: POLL, placeholderData: keepPreviousData,
    queryFn: ({ signal }) => get<AccessTargets>(`/access/targets?${accessQS(f, { sort, limit, offset })}`, signal),
  });

export const useNewTargets = (f: AccessFilter, offset: number, limit: number, enabled = true) =>
  useQuery({
    queryKey: ['access', 'new', f, offset, limit], enabled, refetchInterval: POLL, placeholderData: keepPreviousData,
    queryFn: ({ signal }) => get<NewTargets>(`/access/new-targets?${accessQS(f, { limit, offset })}`, signal),
  });

export const useAccessSources = (f: AccessFilter, enabled = true) =>
  useQuery({
    queryKey: ['access', 'sources', f], enabled, refetchInterval: POLL, placeholderData: keepPreviousData,
    queryFn: ({ signal }) => get<{ items: AccessSource[] }>(`/access/sources?${accessQS(f, { limit: 200 })}`, signal),
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
