// 来源 IP 画像：类型（对应 docs/API.md「来源 IP 画像」）+ 查询 hook。
import { keepPreviousData, useQuery } from '@tanstack/react-query';
import { get } from './api';

export type ConnType = 'residential' | 'mobile' | 'datacenter' | 'vpn' | 'unknown';
export const CONN_TYPES: { value: ConnType; label: string }[] = [
  { value: 'residential', label: '住宅' }, { value: 'mobile', label: '移动' }, { value: 'datacenter', label: '机房' }, { value: 'vpn', label: 'VPN' }, { value: 'unknown', label: '未知' },
];

/** 一个来源 IP 的画像摘要；查不到（库未装 / 库里没有）时整个对象为 null */
export interface IpIntel {
  isp: string; asn: number | null; as_org: string | null;
  country: string | null; province: string | null; city: string | null; place: string;
  conn_type: ConnType; conn_label: string; confidence: 'high' | 'medium' | 'low'; reason: string;
  /** 我方线路机 / 节点 / 运维出口的名称 */
  internal: string | null; private: boolean;
}

export interface DistItem { value: string; label: string; conns: number; sources: number; pct: number }
export interface KnownItem { value: string; label: string; first_seen: string; last_seen: string; conns: number; anomaly: boolean }
export interface SourcesProfile {
  window: string; window_start: string; empty: boolean; degraded: boolean;
  total_conns: number; total_sources: number; internal_conns: number; private_conns: number; unresolved_sources: number;
  distribution: { isp: DistItem[]; city: DistItem[]; province: DistItem[]; country: DistItem[]; conn_type: DistItem[] };
  top_sources: { src_ip: string; conns: number; last_ts: string; intel: IpIntel | null }[];
  known: { isp: KnownItem[]; city: KnownItem[]; conn_type: KnownItem[] };
  learning: { started_at: string | null; ends_at: string | null; learn_days: number; done: boolean; remaining_hours: number | null };
  anomalies: { id: string; rule: string; rule_name: string; severity: string; summary: string; opened_at: string }[];
}

export interface IpIntelFile { id: string; loaded: boolean; bytes?: number; mtime?: number; build?: string | null; entries?: number | null }
export interface IpIntelStatus {
  enabled: boolean; installed: boolean; loaded: number; expected: number; db_dir: string; version: string;
  files: IpIntelFile[]; errors: string[]; cache_rows: number; hint: string | null;
}

/** 画像分布支持 24h / 7d / 30d；1h 按 24h 查 */
export type ProfileWindow = '24h' | '7d' | '30d';

export const useSourcesProfile = (id: string | null, window: ProfileWindow) =>
  useQuery({ queryKey: ['ipintel', 'sources', id, window], enabled: !!id, refetchInterval: 30_000, placeholderData: keepPreviousData, queryFn: () => get<SourcesProfile>(`/machines/${id}/sources?window=${window}&limit=10`) });

export const useIpIntelStatus = () =>
  useQuery({ queryKey: ['ipintel', 'status'], refetchInterval: 60_000, queryFn: () => get<IpIntelStatus>('/ipintel/status') });
