// API 类型，对应 v1/docs/API.md。文档没写明形状的（series / incidents / silences / audit / renewals）为前端假定，已标注。

export type Kind = 'relay' | 'vps' | 'proxy';
export type Lifecycle = 'draft' | 'provisioning' | 'active' | 'retiring' | 'retired' | 'failed';
export type HealthStatus = 'ok' | 'degraded' | 'down' | 'unknown' | 'maintenance' | 'inactive';
export type Cycle = 'monthly' | 'quarterly' | 'semiannual' | 'yearly' | 'once' | string;

export interface Subscription {
  price_cents: number; currency: string; cycle: Cycle; due_date: string; due_days: number;
  billing_tz: string; auto_renew: boolean; remind_days: number; traffic_gb: number | null; traffic_reset_day: number | null;
}
/** 指标缺测（agent 刚装、上报不全）时对应字段为 null，使用处一律按可空处理 */
export interface HostMetrics {
  ts: string; cpu_pct: number | null; load1: number | null; mem_used: number | null; mem_total: number | null;
  disk_used: number | null; disk_total: number | null; net_rx_bps: number | null; net_tx_bps: number | null; tcp_conns: number | null;
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
  /** cores：agent 报的可用核数（容器里是限额），没报过为 null */
  agent: { version: string | null; last_report_at: string | null; online: boolean; xray_running: boolean | null; cores: number | null } | null;
  host: HostMetrics | null;
  entries: EntryView[];
  removed_traffic: EntryTraffic; // 已移除入口（如换线路机后的旧入口）今天/本月的流量，不在 entries 里
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
export type JobKind = 'provision_vps' | 'provision_proxy' | 'provision_relay' | 'attach_entry' | 'move_entry' | 'restore_vps' | 'restore_proxy' | 'restore_relay' | 'retire' | 'purge' | 'upgrade_agent';
/** 步骤 state 文档未列全，前端按 pending/running/waiting/succeeded(done)/failed/skipped(cancelled) 宽松处理 */
export interface JobStep { idx: number; key: string; title: string; state: string; started_at: string | null; finished_at: string | null; output: Record<string, unknown> | null; error: string | null; error_kind?: string | null }
export interface Job {
  id: string; kind: JobKind | string; target: { id: string; name: string } | null; state: JobState | string;
  step: number | null; step_count: number; error: string | null; created_by: string; created_at: string; started_at: string | null; finished_at: string | null;
  steps: JobStep[]; cancellable: boolean;
  /** 换线路机作业（move_entry）才有：新入口 id 与「分享链接已变」的提示 */
  result?: JobResult | null;
}
export interface JobResult { new_entry_id: string | null; old_entry_id: string | null; from_relay_id: string | null; to_relay_id: string | null; share_changed: boolean; note: string }
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
  /** 恢复后 30 分钟内复发、合并进这条的次数 */
  reopen_count: number;
}
export interface Silence { id: string; subject_id: string | null; subject_name: string | null; rule: string | null; starts_at: string; ends_at: string; is_maintenance: boolean; comment: string }
export interface Renewal { id: string; old_due: string | null; new_due: string; amount_cents: number | null; note: string | null; actor: string; created_at: string }
/** 审计：界面只显示 title 与 summary（服务端生成的中文）；detail 是原始 JSON，不要直接渲染 */
export interface AuditEntry { id: string; ts: string; actor: string; action: string; title: string; summary: string; target_kind: string | null; target_id: string | null; detail: unknown }
export interface RenumberPreview { changes: { machine_id: string; old_name: string; new_name: string }[]; token: string }
/** 换线路机 / 搬空线路机 / 恢复已退役机器（API.md「生命周期操作」） */
export interface MoveResult { job_id: string; name: string; from: string; to: string; share_changed: boolean; note: string }
export interface EvacuateResult { from: string; to: string; jobs: { machine_id: string; name: string; job_id: string }[]; skipped: { machine_id: string; name: string | null; reason: string }[]; share_changed: boolean; note: string }
export interface RestorePreview {
  id: string; kind: Kind; lifecycle: Lifecycle; ip: string; old_name: string; name: string; team: { id: string; name: string } | null; number: number | null; label: string | null;
  restorable: boolean; blockers: string[]; warnings: string[]; ip_conflict: { machine_id: string; name: string; lifecycle: string } | null;
  last_relay: { id: string; name: string } | null; retired_reason: string | null;
  ssh: { reachable: boolean; key_login: 'ok' | 'denied' | 'error' | 'unreachable' | string; host_key?: string; detail?: string } | null;
  /** password：required 中控密钥登录被拒、必须给 root 密码；optional 探测不到、可选；no 不需要 */
  needs: { relay: boolean; upstream: boolean; password: 'required' | 'optional' | 'no' };
}
export interface RestoreBody {
  team_id?: string; number?: number; label?: string; relay_id?: string; reason?: string; reset_host_key?: boolean;
  login?: { password: string }; upstream?: { protocol: 'socks5' | 'http'; host: string; port: number; username?: string; password?: string };
}
/** 退役预览：退役后同团队后面的节点会依次前移改名 */
export interface RetirePreview { name: string; renames: { machine_id: string; old_name: string; new_name: string }[] }

// ---- 设置页：API 密钥、告警通知渠道（契约见 COMMON.md） ----
export type ApiScope = 'read' | 'write' | 'secrets' | 'exec';
export interface ApiKey { id: string; name: string; prefix: string; scopes: ApiScope[] | string[]; created_at: string; expires_at: string | null; last_used_at: string | null; revoked_at: string | null }
export type NotifyKind = 'telegram' | 'wecom' | 'webhook';
export type NotifySeverity = 'info' | 'warning' | 'critical';
export interface NotifyChannel { id: string; name: string; kind: NotifyKind | string; min_severity: NotifySeverity | string; enabled: boolean; created_at: string; config_hint: string }
export interface NotifyLogEntry { id: string; channel_id: string; channel_name: string; incident_id: string | null; ts: string; ok: boolean; error: string | null }

/** 服务端分页列表（/jobs、/incidents）：next_offset 只在后面还有数据时给出 */
export interface Paged<T> { items: T[]; total: number; next_offset: number | null }
