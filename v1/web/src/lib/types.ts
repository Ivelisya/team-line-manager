// API 类型，对应 v1/docs/API.md。文档没写明形状的（series / incidents / silences / audit / renewals）为前端假定，已标注。

export type Kind = 'relay' | 'vps' | 'proxy';
export type Lifecycle = 'draft' | 'provisioning' | 'active' | 'retiring' | 'retired' | 'failed';
export type HealthStatus = 'ok' | 'degraded' | 'down' | 'unknown' | 'maintenance' | 'inactive';
export type Cycle = 'monthly' | 'quarterly' | 'semiannual' | 'yearly' | 'once' | string;

export interface Subscription {
  price_cents: number; currency: string; cycle: Cycle; due_date: string; due_days: number;
  billing_tz: string; auto_renew: boolean; remind_days: number; traffic_gb: number | null; traffic_reset_day: number | null;
}
export interface HostMetrics {
  ts: string; cpu_pct: number; load1: number; mem_used: number; mem_total: number;
  disk_used: number; disk_total: number; net_rx_bps: number; net_tx_bps: number; tcp_conns: number;
}
export interface ProbeResult { ok: boolean; latency_ms: number | null; detail: string | null; ts: string }
export interface EntryTraffic { today_up: number; today_down: number; month_up: number; month_down: number }
export interface EntryView {
  id: string; relay: { id: string; name: string }; node: { id: string; name: string }; port: number;
  lifecycle: Lifecycle | 'disabled' | string; applied: boolean; apply_error: string | null;
  tcp: ProbeResult | null; egress: ProbeResult | null; traffic: EntryTraffic;
}
export interface HealthReason { key: string; severity: 'info' | 'warning' | 'critical' | string; text: string }
export interface MachineView {
  id: string; kind: Kind; name: string; team: { id: string; name: string } | null; number: number | null; label: string | null;
  ip: string; ssh_port: number;
  provider: { id: string; name: string } | null; provider_code: string | null;
  plan: { id: string; name: string; bandwidth_mbps: number; traffic_gb: number } | null; service_ref: string | null;
  lifecycle: Lifecycle; lifecycle_reason: string | null; note: string; revision: number;
  created_at: string; activated_at: string | null; retired_at: string | null;
  subscription: Subscription | null;
  agent: { version: string; last_report_at: string; online: boolean } | null;
  host: HostMetrics | null;
  entries: EntryView[];
  health: { status: HealthStatus; reasons: HealthReason[] };
  active_job: { id: string; kind: string; state: string } | null;
  open_incidents: number; maintenance: boolean;
  relay: { public_host: string; generation: number; applied_generation: number; applied_at: string | null; apply_error: string | null; port_min: number; port_max: number } | null;
}

export interface Team { id: string; name: string; sort_order: number; node_count: number }
export interface Provider { id: string; name: string; code: string; panel_url: string | null; timezone: string | null }
export interface Plan {
  id: string; provider_id: string; name: string; cpu_cores: number; mem_mb: number; disk_gb: number;
  bandwidth_mbps: number; traffic_gb: number; price_cents: number; currency: string; cycle: Cycle; notes: string | null;
  archived_at: string | null; machine_count: number;
}

export interface Overview {
  nodes: Record<'total' | 'ok' | 'degraded' | 'down' | 'unknown' | 'maintenance', number>;
  relays: Record<'total' | 'ok' | 'degraded' | 'down' | 'unknown' | 'maintenance', number>;
  provisioning: number; retiring: number; due_7d: number; monthly_cost: { currency: string; cents: number }[];
  traffic_month: { up: number; down: number }; net_rx_bps: number; net_tx_bps: number;
}

export type JobState = 'queued' | 'running' | 'waiting' | 'cancelling' | 'succeeded' | 'failed' | 'cancelled';
export type JobKind = 'provision_vps' | 'provision_proxy' | 'provision_relay' | 'attach_entry' | 'retire' | 'purge';
/** 步骤 state 文档未列全，前端按 pending/running/waiting/succeeded(done)/failed/skipped(cancelled) 宽松处理 */
export interface JobStep { idx: number; key: string; title: string; state: string; started_at: string | null; finished_at: string | null; output: Record<string, unknown> | null; error: string | null }
export interface Job {
  id: string; kind: JobKind | string; target: { id: string; name: string } | null; state: JobState | string;
  step: number | null; step_count: number; error: string | null; created_by: string; created_at: string; started_at: string | null; finished_at: string | null;
  steps: JobStep[]; cancellable: boolean;
}
export interface JobLog { id: number; ts: string; level: 'info' | 'warn' | 'error' | string; msg: string }

export interface ProvisionItem {
  kind: Kind; ip: string; ssh_port?: number; team_id?: string; number?: number | null; label?: string;
  provider_id?: string; plan_id?: string; service_ref?: string; note?: string; relay_id?: string;
  login?: { user?: string; password?: string };
  upstream?: { protocol: 'socks5' | 'http'; host: string; port: number; username?: string; password?: string };
  ss_port?: number;
  subscription?: { price_cents?: number; due_date: string; cycle?: string; billing_tz?: string };
}
export interface ProvisionCheckItem { ok: boolean; errors: string[]; name: string; number: number | null; relay_port?: number }

// ---- 以 API.md「返回形状补充」为准 ----
/** series：{metric, range, resolution, points}，点里时间字段叫 ts。
 *  host: cpu_pct mem_pct disk_pct load1 net_rx_bps net_tx_bps（1h 分辨率另有 cpu_max，无 load1）；traffic: up down conns；
 *  probe: tcp_ok tcp_ms egress_ok egress_ms（失败时 *_ms 为 null） */
export interface SeriesPoint { ts: string; [k: string]: number | boolean | string | null }
export interface SeriesResp { metric: string; range: string; resolution: '1m' | '1h' | string; points: SeriesPoint[] }
export interface Incident {
  id: string; rule: string; subject: { id: string; name: string } | null; subject_id: string | null; subject_name: string | null;
  severity: 'info' | 'warning' | 'critical' | string; state: 'firing' | 'resolved'; summary: string;
  opened_at: string; last_seen_at: string | null; resolved_at: string | null; ack_at: string | null; ack_by: string | null;
}
export interface Silence { id: string; subject_id: string | null; subject_name: string | null; rule: string | null; starts_at: string; ends_at: string; is_maintenance: boolean; comment: string }
export interface Renewal { id: string; old_due: string | null; new_due: string; amount_cents: number | null; note: string | null; actor: string; created_at: string }
/** 审计：界面只显示 title 与 summary（服务端生成的中文）；detail 是原始 JSON，不要直接渲染 */
export interface AuditEntry { id: string; ts: string; actor: string; action: string; title: string; summary: string; target_kind: string | null; target_id: string | null; detail: unknown }
export interface RenumberPreview { changes: { machine_id: string; old_name: string; new_name: string }[]; token: string }
/** 退役预览：退役后同团队后面的节点会依次前移改名 */
export interface RetirePreview { name: string; renames: { machine_id: string; old_name: string; new_name: string }[] }
