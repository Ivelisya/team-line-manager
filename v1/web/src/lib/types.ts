// API 类型，对应 v1/docs/API.md。文档没写明形状的（series / incidents / silences / audit / renewals）为前端假定，已标注。

export type Kind = 'relay' | 'vps' | 'proxy';
export type Lifecycle = 'draft' | 'provisioning' | 'active' | 'retiring' | 'retired' | 'failed';
/** 入口生命周期：pending = 已建、等线路机应用；removing = 等线路机删掉 */
export type EntryLifecycle = 'pending' | 'active' | 'disabled' | 'removing' | 'removed';
export type HealthStatus = 'ok' | 'degraded' | 'down' | 'unknown' | 'maintenance' | 'inactive';
export type Cycle = 'monthly' | 'quarterly' | 'semiannual' | 'yearly' | string;

/** 订阅的生效值：价格、币种、周期、流量上限没单独设置的跟随套餐，账单时区、提前提醒、流量重置日跟随供应商（见 SubTemplate）。
 *  traffic_gb 0 / null = 不限；traffic_reset_day 是生效的重置日。overrides：单独设置了的字段 */
export interface Subscription {
  price_cents: number; currency: string; cycle: Cycle; due_date: string; due_days: number;
  billing_tz: string; auto_renew: boolean; remind_days: number; traffic_gb: number | null; traffic_reset_day: number | null;
  overrides?: SubField[];
  /** 账单日（每月几号）：直接设到期日时记下，按周期续费不变（31 号的在 2-28 时仍是 31）；按账单日重置的流量重置日跟它。旧接口没有 */
  billing_day?: number;
  /** 服务端算好的：due_state 到期状态（同续费告警）、monthly_cents 折成每月的价格（分）、next_due 续一个周期后的到期日（续费的默认值）、
   *  period_days 当前这期的天数（到期日往前推一个周期 → 到期日） */
  due_state: DueState; monthly_cents: number; next_due: string; period_days: number;
}
/** overdue 已过期；soon 到期前 remind_days 天内且不是自动续费；其余 ok */
export type DueState = 'ok' | 'soon' | 'overdue';
/** 订阅里可以单独设置的字段 */
export type SubField = 'price_cents' | 'currency' | 'cycle' | 'billing_tz' | 'remind_days' | 'traffic_gb' | 'traffic_reset_day';
/** 机器的订阅模板：订阅没单独设置的项取这里。没有套餐时 price_cents 为 null（要单独设置价格）；traffic_reset：供应商按自然月 / 账单日重置流量 */
export interface SubTemplate { price_cents: number | null; currency: string; cycle: Cycle; billing_tz: string; remind_days: number; traffic_gb: number | null; traffic_reset: TrafficReset }
export type TrafficReset = 'month' | 'billing';
/** 指标缺测（agent 刚装、上报不全）时对应字段为 null，使用处一律按可空处理 */
export interface HostMetrics {
  ts: string; cpu_pct: number | null; load1: number | null; mem_used: number | null; mem_total: number | null;
  disk_used: number | null; disk_total: number | null; net_rx_bps: number | null; net_tx_bps: number | null; tcp_conns: number | null;
}
/** median_ms：TCP 探测近 1 小时成功样本的延迟中位数（卡片读数）；出口探测为 null */
export interface ProbeResult { ok: boolean; latency_ms: number | null; detail: string | null; ts: string; median_ms?: number | null }
export interface EntryTraffic { today_up: number; today_down: number; month_up: number; month_down: number }
export interface EntryView {
  id: string; relay: { id: string; name: string }; node: { id: string; name: string };
  /** 线路机直出：节点就是线路机自己（node.id = relay.id，node.name =「线路机名-直出」），从线路机自己的 IP 出去；没有 TCP 探测，不能换线路机 / 搬空 */
  direct: boolean;
  port: number;
  lifecycle: EntryLifecycle; applied: boolean; apply_error: string | null;
  tcp: ProbeResult | null; egress: ProbeResult | null; traffic: EntryTraffic;
}
export interface HealthReason { key: string; severity: 'info' | 'warning' | 'critical' | string; text: string }
/** agent 自报的健康数据（只列界面用到的字段，完整见 docs/API.md） */
export interface AgentHealth {
  queue_len: number; queue_dropped: number; queue_dead: number;
  access_unparsed: number; access_dropped_lines: number; access_rotation_gaps: number; access_truncations: number; last_apply_error?: string | null;
  disk_write_error?: string | null;
}
export interface MachineView {
  id: string; kind: Kind; name: string; team: { id: string; name: string } | null; number: number | null; label: string | null;
  ip: string; ssh_port: number;
  /** 国家或地区（ISO 3166-1 两位大写，如 US）：手动设了用手动的，否则中控离线 IP 库按机器自己的 IP 查；
   *  内网、家宽代理填的域名、库没装时为 null */
  country: string | null;
  /** 手动设的国家（上架时选的或「管理」页签改的）；null = 按 IP 自动 */
  country_set: string | null;
  /** 同机 IP 节点（kind 仍是 vps）：它所在的宿主机。它没有自己的 agent 和 SSH——agent、host（主机读数）、ssh_port 都是宿主机的，
   *  plan 为 null，宿主机维护时 maintenance 也为 true；其它机器为 null */
  host_machine: { id: string; name: string } | null;
  /** 宿主机：它上面未退役的同机 IP 节点（按团队、编号）；其它机器为 []。宿主机的 quota.used 按网卡算，含它们的流量 */
  ip_nodes: { id: string; name: string }[];
  provider: { id: string; name: string } | null; provider_code: string | null;
  plan: { id: string; name: string; bandwidth_mbps: number | null; traffic_gb: number | null } | null; service_ref: string | null;
  lifecycle: Lifecycle; lifecycle_reason: string | null; note: string; revision: number;
  created_at: string; activated_at: string | null; retired_at: string | null;
  subscription: Subscription | null;
  sub_template?: SubTemplate;
  /** cores：agent 报的可用核数（容器里是限额），没报过为 null */
  /** health：agent 自报的健康数据（自 agent 进程启动起累计；旧 agent 没报过为 null），不随实时推送更新 */
  agent: { version: string | null; last_report_at: string | null; online: boolean; xray_running: boolean | null; cores: number | null; health: AgentHealth | null } | null;
  host: HostMetrics | null;
  entries: EntryView[];
  removed_traffic: EntryTraffic; // 已移除入口（如换线路机后的旧入口）今天/本月的流量，不在 entries 里
  /** 流量额度周期：起点（YYYY-MM-DD，有流量重置日按它，否则本月 1 号）与周期内已用（字节）。有 agent 的机器按网卡、照供应商的
   *  rule 算；家宽代理与同机 IP 节点 rule 为 null，是业务流量。base / base_at：本周期按供应商面板校准时的读数（字节）与时间，没有为 null */
  /** limit_gb / limit_source：生效的流量上限（GB，没有为 null）与来源——subscription 单独设置、plan 跟随套餐、host 同机 IP 节点与宿主机共用、none 不限 */
  quota: { used: number; period_start: string; rule: TrafficRule | null; base: number | null; base_at: string | null; limit_gb?: number | null; limit_source?: 'subscription' | 'plan' | 'host' | 'none' };
  health: { status: HealthStatus; reasons: HealthReason[] };
  active_job: { id: string; kind: string; state: string } | null;
  /** 上架失败（lifecycle = failed）：失败的上架/恢复作业；退役中（retiring）且最近的退役作业失败：那个退役作业 */
  failed_job: { id: string; kind: string; state: string } | null;
  open_incidents: number; maintenance: boolean;
  relay: { public_host: string; generation: number; applied_generation: number; applied_at: string | null; apply_error: string | null; port_min: number; port_max: number } | null;
}

export interface Team { id: string; name: string; sort_order: number; node_count: number }
/** 供应商怎么计量流量额度：网卡本周期入、出总量取小 / 取大 / 相加 / 只算出站 */
export type TrafficRule = 'min' | 'max' | 'sum' | 'out';
/** 供应商也是名下机器订阅的模板：账单时区（null = 跟随组织时区）、流量重置（自然月 / 账单日）、提前提醒天数 */
export interface Provider { id: string; name: string; code: string; panel_url: string | null; timezone: string | null; traffic_rule: TrafficRule; traffic_reset?: TrafficReset; remind_days?: number }
export interface Plan {
  id: string; provider_id: string; name: string; cpu_cores: number | null; mem_mb: number | null; disk_gb: number | null;
  bandwidth_mbps: number | null; traffic_gb: number | null; price_cents: number; currency: string; cycle: Cycle; notes: string | null;
  archived_at: string | null; machine_count: number;
}

export interface Overview {
  nodes: Record<'total' | 'ok' | 'degraded' | 'down' | 'unknown' | 'maintenance', number>;
  relays: Record<'total' | 'ok' | 'degraded' | 'down' | 'unknown' | 'maintenance', number>;
  /** 在线/离线按 agent 连通（同 isQuiet），与健康无关 */
  online: number; offline: number;
  /** failed：上架失败（lifecycle = failed）的台数 */
  provisioning: number; retiring: number; failed: number;
  /** 在用机器里即将到期（due_state = soon）与已过期（overdue）的台数 */
  due_soon: number; overdue: number;
  /** 各币种的月费合计（每台的 monthly_cents 相加） */
  monthly_cost: { currency: string; cents: number }[];
  traffic_month: { up: number; down: number }; net_rx_bps: number; net_tx_bps: number;
}

/** GET /meta：中文名、枚举、阈值、默认值的唯一来源（服务端 meta.rs），一次会话内不变 */
export interface Meta {
  org_timezone: string; org_timezone_label: string; org_utc_offset_s: number;
  /** 时区下拉的常用项；不在里面的照样能填 */
  timezones: string[];
  currencies: string[];
  cycles: { key: string; label: string; months: number }[];
  kinds: Record<string, string>; ip_node_label: string;
  lifecycles: Record<string, string>; healths: Record<string, string>;
  job_kinds: Record<string, string>; job_states: Record<string, string>; step_states: Record<string, string>; entry_states: Record<string, string>;
  traffic_rules: Record<string, string>; traffic_resets: Record<string, string>; due_states: Record<string, string>;
  /** 访问策略的模式 → 中文名 */
  policy_modes: Record<string, string>;
  /** 策略版本在线路机上的同步状态：applied / applying / failed / gone */
  policy_sync_states: Record<string, string>;
  /** 告警规则 key → 中文名 */
  rules: Record<string, string>;
  severities: Record<string, { label: string; tone: 'bad' | 'warn' | 'info' }>;
  thresholds: { traffic_quota_warn_pct: number; traffic_quota_over_pct: number; usage: Record<UsageMetric, { warn: number; bad: number }> };
  defaults: { currency: string; cycle: string; remind_days: number; traffic_rule: TrafficRule; traffic_reset: TrafficReset; ss_method: string; ss_port: number };
  /** 网页终端的上限（服务端 terminal.rs）：每人 / 全中控同时几个、空闲与最长秒数、一条消息最多几字节、行列上限、票据与免验证的有效秒数 */
  terminal: { per_user: number; global: number; idle_s: number; max_age_s: number; max_message: number; max_dim: number; ticket_ttl_s: number; verify_ttl_s: number };
}
export type UsageMetric = 'cpu' | 'mem' | 'disk' | 'load';

export type JobState = 'queued' | 'running' | 'waiting' | 'cancelling' | 'succeeded' | 'failed' | 'cancelled';
export type JobKind = 'provision_vps' | 'provision_proxy' | 'provision_relay' | 'provision_ip' | 'attach_entry' | 'move_entry' | 'restore_vps' | 'restore_proxy' | 'restore_relay' | 'restore_ip' | 'retire' | 'purge' | 'upgrade_agent';
/** 步骤 state 文档未列全，前端按 pending/running/waiting/succeeded(done)/failed/skipped(cancelled) 宽松处理 */
export interface JobStep { idx: number; key: string; title: string; state: string; started_at: string | null; finished_at: string | null; output: Record<string, unknown> | null; error: string | null; error_kind?: string | null }
/** 作业列表（GET /jobs）的一项：不带 steps。step = 当前（或失败的）步骤下标，step_title 为它的标题，做完为 null */
export interface JobSummary {
  id: string; kind: JobKind | string; target: { id: string; name: string } | null; state: JobState | string;
  step: number; step_count: number; step_title: string | null; error: string | null; created_by: string; created_at: string; started_at: string | null; finished_at: string | null;
}
/** 作业详情（GET /jobs/{id}） */
export interface Job extends JobSummary {
  steps: JobStep[]; cancellable: boolean;
  /** 换线路机作业（move_entry）才有：新入口 id 与「分享链接已变」的提示 */
  result?: JobResult | null;
}
export interface JobResult { new_entry_id: string | null; old_entry_id: string | null; from_relay_id: string | null; to_relay_id: string | null; share_changed: boolean; note: string }
export interface JobLog { id: number; ts: string; level: 'info' | 'warn' | 'error' | string; msg: string }

export interface ProvisionItem {
  kind: Kind; ip: string; ssh_port?: number; team_id?: string; number?: number | null; label?: string;
  provider_id?: string; plan_id?: string; service_ref?: string; note?: string; relay_id?: string;
  /** 首次登录（只支持 root）：不填用中控的密钥 */
  login?: { password?: string };
  /** 国家或地区（两位代码）；不给 = 按 IP 自动 */
  country?: string;
  upstream?: { protocol: 'socks5' | 'http'; host: string; port: number; username?: string; password?: string };
  ss_port?: number;
  /** 只要到期日（选了套餐时必填），其余没给的跟随套餐 / 供应商；没有套餐（含同机 IP 节点）要给 price_cents */
  subscription?: { due_date: string; price_cents?: number; currency?: string; cycle?: string; billing_tz?: string; remind_days?: number; traffic_gb?: number; traffic_reset_day?: number };
  /** 同机 IP 节点：加在这台 VPS 节点（宿主机）上。带了它时 ssh_port / login / ss_port / provider_id / plan_id 不适用（服务端忽略）：
   *  SSH 端口与供应商取宿主机的、不带套餐，SS 端口由中控在宿主机上分配 */
  host_id?: string;
}
/** ss_port：同机 IP 节点暂定的 SS 端口（作业在宿主机上核对时被占用会换），其它为 null */
export interface ProvisionCheckItem { ok: boolean; errors: string[]; name: string; number: number | null; relay_port?: number; ss_port: number | null }

// ---- 以 API.md「返回形状补充」为准 ----
/** series：{metric, range, resolution, points}，点里时间字段叫 ts。
 *  host: cpu_pct mem_pct disk_pct load1 net_rx_bps net_tx_bps（1h 分辨率另有 cpu_max，无 load1）；traffic: up down conns；
 *  probe: tcp_ok tcp_ms egress_ok egress_ms（失败时 *_ms 为 null） */
export interface SeriesPoint { ts: string; [k: string]: number | boolean | string | null }
/** 心跳格：每台机器近 60 分钟、20 个 3 分钟桶（旧→新），每桶 [次数, 成功次数, 成功的延迟中位数 ms, SYN 重发过的次数]；[0,0,null,0] = 这段没有数据。
 *  节点是线路机 → 节点的 TCP 探测；线路机是中控 → 线路机的连通性（只判通不通，延迟为 null） */
export type ProbeBucket = [number, number, number | null, number?];
export type ProbeBeats = Record<string, ProbeBucket[]>;
/** 全站网卡速率的历史：每分钟一个点（旧→新），首页集群状态折线开场用 */
export interface OverviewSeries { points: { ts: string; net_rx_bps: number; net_tx_bps: number }[] }
export interface SeriesResp { metric: string; range: string; resolution: '1m' | '1h' | string; points: SeriesPoint[] }
/** 近 N 天可用率（GET /machines/{id}/availability）：每天一项，旧→新共 days 项，最后一项是今天（还没过完）；按这台机器的账单时区分天，
 *  没有探测的天各项为 0。tcp_pct / egress_pct 是整段的成功率（0–100），没有探测为 null */
export interface AvailabilityDay { date: string; tcp_total: number; tcp_ok: number; egress_total: number; egress_ok: number }
export interface Availability { days: number; items: AvailabilityDay[]; tcp_pct: number | null; egress_pct: number | null }
export interface Incident {
  /** subject.kind：规则以 router_ 开头的告警对象是软路由，为 "router"（机器的没有这个字段） */
  id: string; rule: string; rule_name: string; subject: { id: string; name: string; kind?: 'router' } | null; subject_id: string | null; subject_name: string | null;
  severity: 'info' | 'warning' | 'critical' | string; state: 'firing' | 'resolved'; summary: string;
  opened_at: string; last_seen_at: string | null; resolved_at: string | null; ack_at: string | null; ack_by: string | null;
  /** 恢复后 30 分钟内复发、合并进这条的次数 */
  reopen_count: number;
}
/** 静默：subject_id 为 null = 全部机器，rule 为 null = 全部规则（rule_name 是规则中文名，同告警）。列表只有没结束的：进行中的与已预约的（starts_at 还没到） */
export interface Silence { id: string; subject_id: string | null; subject_name: string | null; rule: string | null; rule_name: string | null; starts_at: string; ends_at: string; is_maintenance: boolean; comment: string }
/** currency：登记时订阅的币种（之后改订阅币种，这条的金额不跟着变） */
export interface Renewal { id: string; old_due: string | null; new_due: string; amount_cents: number | null; currency: string; note: string | null; actor: string; created_at: string }
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
  /** 同机 IP 节点不试 SSH（用中控密钥登录宿主机），为 null、needs.password 为 no；宿主机不在用时 blockers 里有原因 */
  /** host_key_changed：退役时存过主机公钥才有；true 时 blockers 里有「主机指纹与退役时不同…」（请求带 reset_host_key=true 时改在 warnings） */
  ssh: { reachable: boolean; key_login: 'ok' | 'denied' | 'error' | 'unreachable' | string; host_key?: string; host_key_changed?: boolean; detail?: string } | null;
  /** 退役时留下的出站参数（不含秘密），恢复时默认沿用：VPS 节点是 SS 端口（NAT 映射的公网端口），家宽代理是上游。迁移 0029 之前退役的为 null */
  outbound: { protocol: 'shadowsocks' | 'socks5' | 'http'; host: string; port: number; username: string | null } | null;
  /** password：required 中控密钥登录被拒、必须给 root 密码；optional 探测不到、可选；no 不需要。
   *  upstream：家宽代理没留下上游、要给全；upstream_password：留下的上游有用户名，密码退役时已清掉、要重新给 */
  needs: { relay: boolean; upstream: boolean; upstream_password: boolean; password: 'required' | 'optional' | 'no' };
}
export interface RestoreBody {
  team_id?: string; number?: number; label?: string; relay_id?: string; reason?: string; reset_host_key?: boolean; ss_port?: number;
  /** 没给的字段沿用退役时留下的；username 给空串 = 去掉 */
  login?: { password: string }; upstream?: { protocol?: 'socks5' | 'http'; host?: string; port?: number; username?: string; password?: string };
}
/** 退役预览：退役后同团队后面的节点会依次前移改名。ip_nodes：会一并退役的同机 IP 节点（宿主机才有），renames 按所有受影响的团队算 */
export interface RetirePreview { name: string; renames: { machine_id: string; old_name: string; new_name: string }[]; ip_nodes: { id: string; name: string }[] }
/** 退役：宿主机的同机 IP 节点各一个退役作业（ip_node_jobs，没有为 []） */
export interface RetireResult { job_id: string; ip_node_jobs: { machine_id: string; name: string; job_id: string }[] }

// ---- 设置页：API 密钥、告警通知渠道（契约见 COMMON.md） ----
export type ApiScope = 'read' | 'write' | 'secrets' | 'exec';
export interface ApiKey { id: string; name: string; prefix: string; scopes: ApiScope[] | string[]; created_at: string; expires_at: string | null; last_used_at: string | null; revoked_at: string | null }
export type NotifyKind = 'telegram' | 'wecom' | 'webhook';
export type NotifySeverity = 'info' | 'warning' | 'critical';
export interface NotifyChannel { id: string; name: string; kind: NotifyKind | string; min_severity: NotifySeverity | string; enabled: boolean; created_at: string; config_hint: string }
/** channel_name：渠道删了之后为 null（发送记录还留着） */
export interface NotifyLogEntry { id: string; channel_id: string; channel_name: string | null; incident_id: string | null; ts: string; ok: boolean; error: string | null;
  /** firing / repeat / resolved / test */
  kind: 'firing' | 'repeat' | 'resolved' | 'test'; subject_name: string | null; rule_name: string | null }

/** 服务端分页列表（/jobs、/incidents）：next_offset 只在后面还有数据时给出 */
export interface Paged<T> { items: T[]; total: number; next_offset: number | null }

// ---- 账户安全（/account/*，只给登录会话） ----
export interface TotpStatus { enabled: boolean; recovery_codes_left: number }
/** 开启两步验证的第一步：待确认的密钥（10 分钟内用 enable 确认）。qr_svg 是服务端画好的二维码 SVG */
export interface TotpSetup { secret: string; otpauth_url: string; qr_svg: string }
/** 登录会话：id 是会话哈希前 8 字节的十六进制（不是 Cookie），current = 本设备 */
export interface AccountSession { id: string; created_at: string; last_seen_at: string; expires_at: string; ip: string | null; user_agent: string | null; current: boolean }
