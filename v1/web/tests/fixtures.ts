// 单元测试用的最小机器 / 入口：只填被测函数会读的字段，其余按类型断言补上（视图很大，逐个字段写全没有意义）
import type { EntryView, MachineView, Meta, ProbeResult } from '@/lib/types';

/** 下标、解构取到的值：没有就让这条用例失败，有就收窄成 T（noUncheckedIndexedAccess） */
export function must<T>(x: T | undefined): T {
  if (x === undefined) throw new Error('预期有值，实际是 undefined');
  return x;
}

export const probe = (ok: boolean, ms: number | null = 20, detail: string | null = null): ProbeResult => ({ ok, latency_ms: ms, ts: '2026-01-01T00:00:00Z', detail, median_ms: ms });

export function entry(p: Partial<EntryView> = {}): EntryView {
  return {
    id: 'e1', relay: { id: 'r1', name: '线路机1' }, node: { id: 'n1', name: '团队1-01' }, direct: false, mode: 'normal', port: 20001,
    lifecycle: 'active', applied: true, apply_error: null, tcp: probe(true), egress: probe(true), reach: null,
    ...p,
  } as EntryView;
}

export function machine(p: Partial<MachineView> = {}): MachineView {
  return {
    id: 'm1', kind: 'vps', name: '团队1-01', team: { id: 't1', name: '团队1' }, number: 1, lifecycle: 'active',
    spare: false, idle_spare: false, spare_unbilled: false, router_exits: [], entries: [],
    agent: { version: '1.0.0', last_report_at: null, online: true, xray_running: true, cores: 2, health: null },
    health: { status: 'ok', reasons: [] }, maintenance: false, host: null, subscription: null, plan: null,
    quota: { used: 0, period_start: '2026-01-01', period_end: null, mode: 'monthly', counted_from: null, rule: null, base: null, base_at: null, limit_gb: null, limit_source: 'none', forecast_full_at: null },
    ...p,
  } as unknown as MachineView;
}

/** 被测函数会读到的那几项 /meta */
export const META = { org_timezone: 'Asia/Shanghai', thresholds: { traffic_quota_warn_pct: 80 } } as unknown as Meta;
