// API 类型，对应 v1/docs/API.md。文档没写明形状的（series / incidents / silences / audit / renewals）为前端假定，已标注。

export type Kind = 'relay' | 'vps' | 'proxy';
export type Lifecycle = 'draft' | 'provisioning' | 'active' | 'retiring' | 'retired' | 'failed';
/** 入口生命周期：pending = 已建、等线路机应用；removing = 等线路机删掉 */
export type EntryLifecycle = 'pending' | 'active' | 'disabled' | 'removing' | 'removed';
export type HealthStatus = 'ok' | 'degraded' | 'down' | 'unknown' | 'maintenance' | 'inactive';
export type Cycle = 'monthly' | 'quarterly' | 'semiannual' | 'yearly' | string;

/** 订阅的生效值：价格、币种、周期、流量上限没单独设置的跟随套餐，账单时区、提前提醒、流量重置日、流量计费跟随供应商（见 SubTemplate）。
 *  traffic_gb 0 / null = 不限；traffic_reset_day 是生效的重置日；traffic_rule 是生效的流量计费方式（双向 / 单向，只对按网卡计量的机器有意义）。overrides：单独设置了的字段 */
export interface Subscription {
  price_cents: number; currency: string; cycle: Cycle; due_date: string; due_days: number;
  billing_tz: string; auto_renew: boolean; remind_days: number; traffic_gb: number | null; traffic_reset_day: number | null; traffic_rule: TrafficRule;
  overrides: SubField[];
  /** 账单日（每月几号）：直接设到期日时记下，按周期续费不变（31 号的在 2-28 时仍是 31）；按账单日重置的流量重置日跟它 */
  billing_day: number;
  /** 服务端算好的：due_state 到期状态（同续费告警）、monthly_cents 折成每月的价格（分）、next_due 续一个周期后的到期日（续费的默认值）、
   *  period_days 当前这期的天数（到期日往前推一个周期 → 到期日） */
  due_state: DueState; monthly_cents: number; next_due: string; period_days: number;
  /** 计费统一按 USD 展示：服务端按汇率折好的月费、每期价格（分），usd_per_unit 是所用汇率（1 单位合多少 USD）。
   *  没有该币种的汇率时三个都是 null（未折算，不当 1:1） */
  monthly_usd_cents: number | null; price_usd_cents: number | null; usd_per_unit: number | null;
}
/** overdue 已过期；soon 到期前 remind_days 天内且不是自动续费；其余 ok */
export type DueState = 'ok' | 'soon' | 'overdue';
/** 订阅里可以单独设置的字段 */
export type SubField = 'price_cents' | 'currency' | 'cycle' | 'billing_tz' | 'remind_days' | 'traffic_gb' | 'traffic_reset_day' | 'traffic_rule';
/** 机器的订阅模板：订阅没单独设置的项取这里。没有套餐时 price_cents 为 null（要单独设置价格）；traffic_reset：供应商按自然月 / 账单日重置流量 */
export interface SubTemplate { price_cents: number | null; currency: string; cycle: Cycle; billing_tz: string; remind_days: number; traffic_gb: number | null; traffic_reset: TrafficReset; traffic_rule: TrafficRule }
export type TrafficReset = 'month' | 'billing' | 'rolling30';
/** 指标缺测（agent 刚装、上报不全）时对应字段为 null，使用处一律按可空处理 */
export interface HostMetrics {
  ts: string; cpu_pct: number | null; load1: number | null; mem_used: number | null; mem_total: number | null;
  disk_used: number | null; disk_total: number | null; net_rx_bps: number | null; net_tx_bps: number | null; tcp_conns: number | null;
}
/** median_ms：TCP 探测近 1 小时成功样本的延迟中位数（卡片读数）；出口探测为 null */
export interface ProbeResult { ok: boolean; latency_ms: number | null; detail: string | null; ts: string; median_ms?: number | null }
export interface EntryTraffic { today_up: number; today_down: number; month_up: number; month_down: number }
/** 网卡口径的今日 / 本月流量（字节）：rx = 本机收（下行）、tx = 本机发（上行）。只有自己有 agent 的机器有 */
export interface NicTraffic { today_rx: number; today_tx: number; month_rx: number; month_tx: number }
export type EntryMode = 'normal' | 'reverse';
export interface EntryView {
  id: string; relay: { id: string; name: string }; node: { id: string; name: string };
  /** 线路机直出：节点就是线路机自己（node.id = relay.id，node.name =「线路机名-直出」），从线路机自己的 IP 出去；没有 TCP 探测，不能换线路机 / 搬空 */
  direct: boolean;
  /** 穿透（reverse）：节点上的 bridge 主动连这个入口，线路机不连节点——没有 TCP 探测（tcp 恒为 null），只看出口。旧中控没有这个字段，按普通 */
  mode?: EntryMode;
  port: number;
  lifecycle: EntryLifecycle; applied: boolean; apply_error: string | null;
  tcp: ProbeResult | null; egress: ProbeResult | null;
  /** 中控 → 这个入口端口（线路机上）最近一次 TCP 检查；从没检查过为 null。与 tcp / egress（线路机 → 节点）是两段。
   *  fails：这个端口到最近一次为止连续连不上几次（通的为 0）；连续 3 次才算连不上（同 relay_unreachable 告警） */
  reach: { ok: boolean; ts: string; detail: string | null; fails: number } | null;
  traffic: EntryTraffic;
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
  /** 所在团队是备用团队：在用时订阅过期满 1 天（账单时区的「到期日 + 1 天」过完）由中控自动清理并移入回收站 */
  spare: boolean;
  /** 备用里的机器：进备用前的显示名（如 团队2-07）与进备用（= 退役）的时刻、自动回收的起始日（YYYY-MM-DD，没有订阅为 null）；不在备用里为 null */
  spare_from: string | null; spare_since: string | null; recycle_on: string | null;
  /** 费用侧：备用里在用、还被软路由出口指着（router_exits 非空）、上面还有在用的同机 IP 节点（live_ip_nodes 非空）、上面有开着自动续费的备用同机 IP 节点（renewing_ip_nodes 非空）或开着自动续费（subscription.auto_renew）：
   *  不自动回收，recycle_on 为 null，照常续费（月费、到期、待付清单、续费提醒） */
  recycle_held: boolean;
  /** 闲置备用（服务端算好，客户侧口径）：备用里在用、没有软路由出口指着、没有在用的同机 IP 节点——不开告警、不算进健康计数与在线 / 离线、可用率不含；不看自动续费。旧中控没有这个字段，按 router_exits / live_ip_nodes 推 */
  idle_spare?: boolean;
  /** 不续费的备用机（服务端算好，费用侧口径）：备用里在用、没被扣住不回收（recycle_held 为 false）——不提醒续费、不算到期、月费不进合计。一定是闲置备用，反过来只开着自动续费的闲置备用机不是它。旧中控没有这个字段，按 recycle_held 推 */
  spare_unbilled?: boolean;
  /** 备用团队里在用的宿主机：它上面在备用团队里、开着自动续费的同机 IP 节点（显示名）——宿主机回收会连带它们，所以也不自动回收、照常续费；其余为 []。旧中控没有这个字段 */
  renewing_ip_nodes?: string[];
  /** 备用团队里在用的宿主机：上面不在备用团队、没退役的同机 IP 节点（显示名）——宿主机退役会连带它们，所以不自动回收，也不是闲置备用；其余为 []。旧中控没有这个字段 */
  live_ip_nodes?: string[];
  /** 备用团队里在用的机器：还指向它（备用宿主机还含它上面没退役的同机 IP 节点，后缀「（经同机 IP 节点 X）」）的软路由出口（「路由名 · 出口名」）；其余为 []。在用、在备用里、这里和 live_ip_nodes 都为空 = 闲置备用（不开告警、不计健康，见 isIdleSpare；再没开自动续费才是不续费的，见 isUnbilledSpare） */
  router_exits: string[];
  /** 团队里在用的节点（不在备用）、团队的软路由都看得清出口，却没有任何出口指着它：多半是出口改指向别处后留下的，看着像在用其实没客户（抽屉标一句）。
   *  团队没有软路由、有路由没采集到 / 配置读不了为 false。旧中控没有这个字段 */
  no_router_exits?: boolean;
  ip: string; ssh_port: number;
  /** 国家或地区（ISO 3166-1 两位大写，如 US）：手动设了用手动的，否则中控离线 IP 库按机器自己的 IP 查；
   *  内网、家宽代理填的域名、库没装时为 null */
  country: string | null;
  /** 手动设的国家（上架时选的或「管理」页签改的）；null = 按 IP 自动 */
  country_set: string | null;
  /** 同机 IP 节点（kind 仍是 vps）：它所在的宿主机。它没有自己的 agent 和 SSH——agent、host（主机读数）、ssh_port 都是宿主机的，
   *  plan 为 null，宿主机维护时 maintenance 也为 true；其它机器为 null */
  host_machine: { id: string; name: string } | null;
  /** 同机 IP 节点的公网 IP 是 1:1 NAT 映射的：它在宿主机网卡上的本地地址（ssserver 监听并从它出站，线路机连的仍是 ip）；
   *  公网 IP 直接配在网卡上的、其它机器为 null */
  bind_ip: string | null;
  /** 宿主机：它上面未退役的同机 IP 节点（按团队、编号）；其它机器为 []。宿主机的 quota.used 按网卡算，含它们的流量 */
  ip_nodes: { id: string; name: string }[];
  provider: { id: string; name: string } | null; provider_code: string | null;
  plan: { id: string; name: string; bandwidth_mbps: number | null; traffic_gb: number | null } | null; service_ref: string | null;
  lifecycle: Lifecycle; lifecycle_reason: string | null; note: string; revision: number;
  created_at: string; activated_at: string | null; retired_at: string | null;
  subscription: Subscription | null;
  sub_template: SubTemplate;
  /** cores：agent 报的可用核数（容器里是限额），没报过为 null；
   *  health：agent 自报的健康数据（自 agent 进程启动起累计；旧 agent 没报过为 null），不随实时推送更新 */
  agent: { version: string | null; last_report_at: string | null; online: boolean; xray_running: boolean | null; cores: number | null; health: AgentHealth | null; nic_ifaces?: string[] } | null;
  host: HostMetrics | null;
  entries: EntryView[];
  removed_traffic: EntryTraffic; // 已移除入口（如换线路机后的旧入口）今天/本月的流量，不在 entries 里
  /** 网卡口径的今日 / 本月流量：自己有 agent 的机器（线路机、VPS、宿主机）才有；家宽代理、同机 IP 节点没有，只看业务流量（entries） */
  nic: NicTraffic | null;
  /** 本月数据从哪天起才有：机器上架晚于本月 1 号时有值，之前的流量不在内；完整为 null */
  month_counted_from: string | null;
  /** 流量额度周期：起点（YYYY-MM-DD，有流量重置日按它，否则本月 1 号）与周期内已用（字节）。有 agent 的机器按网卡、照供应商的
   *  rule 算；家宽代理与同机 IP 节点 rule 为 null，是业务流量。
   *  period_end：下次重置日（滚动 30 天为 null）；mode：monthly 按重置日、rolling30 滚动 30 天；counted_from：数据不完整时从哪天起才有；
   *  base：接入前的面板读数（字节，割接遗留、只读，下个周期起不再使用），没有为 null；
   *  limit_gb / limit_source：生效的流量上限（GB，没有为 null）与来源——subscription 单独设置、plan 跟随套餐、host 同机 IP 节点与宿主机共用、none 不限 */
  quota: { used: number; period_start: string; period_end: string | null; mode: 'monthly' | 'rolling30'; counted_from: string | null; rule: TrafficRule | null; base: number | null; limit_gb: number | null; limit_source: 'subscription' | 'plan' | 'host' | 'none';
    /** 近 7 天日均用量（字节 / 天；周期不足 1 天为 null）与按它预计用满的时刻（早于下次重置日才有，否则 null） */
    daily_avg: number | null; forecast_full_at: string | null };
  health: { status: HealthStatus; reasons: HealthReason[] };
  active_job: { id: string; kind: string; state: string } | null;
  /** 上架失败（lifecycle = failed）：失败的上架/恢复作业；退役中（retiring）且最近的退役作业失败：那个退役作业 */
  failed_job: { id: string; kind: string; state: string } | null;
  open_incidents: number; maintenance: boolean;
  relay: { public_host: string; generation: number; applied_generation: number; applied_at: string | null; apply_error: string | null; port_min: number; port_max: number } | null;
  /** 系统调优记录：profile 为 null = 未调优（迁移前上线的机器、已撤销）；current = 就是中控当前的档，false 为旧档或未调优；
   *  params = 最近一次调优的推导参数（tune-v2 起，旧档与未调优为 null）。家宽代理、同机 IP 节点为 null */
  tune: { profile: string | null; at: string | null; current: boolean; params: TuneParams | null } | null;
}

/** 调优档 tune-v2 的推导参数：缓冲区上限 buf_max（字节）= clamp(2×bw×rtt/8 + 2 MiB, 16 MiB, min(内存/32, 64 MiB))。
 *  bw（Mbps）取套餐（封顶 1000；没写用 1000，bw_source=default）；rtt（ms）线路机采样入口连接的 P90（rtt_source=sampled，样本 rtt_samples 条），否则默认值 */
export interface TuneParams {
  bw: number; bw_plan: number | null; bw_source: 'plan' | 'default';
  rtt: number; rtt_source: 'sampled' | 'default'; rtt_samples: number | null;
  mem_mb: number | null; buf_max: number;
}

/** spare：唯一的备用团队（系统管理，不能删、不能取消；里面在用的机器订阅过期满 1 天后自动回收） */
export interface Team { id: string; name: string; sort_order: number; spare: boolean; node_count: number;
  /** 在用机器的月费合计（USD，同总览） */
  monthly_usd: UsdTotal;
  /** 备用团队里被扣住不回收（出口还指着 / 同机 IP 节点还在用 / 开着自动续费）的机器的月费与台数：照常续费、计入合计；别的团队为空、0。旧中控没有 */
  monthly_usd_held?: UsdTotal; held_count?: number;
  /** 备用团队里被扣住不回收的总台数（不论有没有订阅；held_count 只数有订阅的）。旧中控没有 */
  held_total?: number }
/** 月费合计（USD）：cents 是能折算部分的合计；by_currency 各原币种的金额、折成的 USD 与所用汇率；unconverted 没有汇率、没算进合计的 */
export interface UsdTotal {
  cents: number;
  by_currency: { currency: string; cents: number; usd_cents: number; usd_per_unit: number }[];
  unconverted: { currency: string; cents: number }[];
}
/** GET /billing/due：未来待付清单。groups 按 供应商 × 到期日 分组（不含不续费的备用机、自动续费的；备用团队里被出口指着 / 有在用 IP 节点 / 开着自动续费的备用机在内，见 spare_hold）；auto 开了自动续费的（到期后自动顺延，不算待付） */
export interface DueMachine {
  id: string; name: string; kind: Kind; due_date: string; due_days: number; billing_tz?: string; cycle: Cycle; price_cents: number; currency: string; price_usd_cents: number | null;
  /** 续不续费的依据（近 30 天）：月费（USD 分）、业务流量（字节）、月费 ÷ 流量、TCP 可用率 %（没有探测为 null）、闲置与近 7 天日均业务流量（字节） */
  monthly_usd_cents: number | null; bytes_30d: number; usd_per_gb: number | null; avail_pct: number | null; idle: boolean; avg7: number;
  /** 备用团队里的机器；spare_hold：还被软路由出口指着 / 上面有在用同机 IP 节点 / 开着自动续费所以不自动回收、照常续费的原因（普通机器与不续费的备用机为 null）。旧中控没有 */
  spare?: boolean; spare_hold?: string | null;
}
export interface DueProvider { id: string | null; name: string | null; panel_url: string | null }
/** 按 供应商 × 到期日 × 账单时区 分组：剩余天数按账单时区的今天算（同一天到期、时区不同的会差一天） */
export interface DueGroup { provider: DueProvider; due_date: string; billing_tz?: string; machines: DueMachine[]; total: UsdTotal }
export interface DueList { days: number | null; until: string | null; groups: DueGroup[]; auto: { provider: DueProvider; machine: DueMachine }[]; total: UsdTotal }
/** POST /renewals/batch：dry_run 为 true 时只是预览（什么都没写） */
export interface RenewBatchResult {
  dry_run: boolean; total: UsdTotal;
  items: { machine_id: string; name: string; old_due: string; new_due: string; amount_cents: number | null; currency: string; usd_cents: number | null }[];
}
/** GET /settings/fx-rates：汇率设置（折算凭据）。rates 在用的币种排前面；source：base（USD）/ manual 手动 / auto 自动 / null 没有汇率 */
export interface FxRates {
  base: 'USD'; auto: boolean; url: string; fetched_at: string | null; rates_date: string | null; source: string | null;
  last_attempt_at: string | null; last_error: string | null; missing: string[];
  rates: { currency: string; usd_per_unit: number | null; per_usd: number | null; source: 'base' | 'manual' | 'auto' | null; auto_per_usd: number | null;
    manual: { usd_per_unit: number; by: string; at: string } | null; in_use: boolean }[];
}
/** 网卡计量规则：sum 双向（下行加上行）；单向只记其中一个——取大 / 取小 / 只算上行 / 只算下行 */
export type TrafficRule = 'min' | 'max' | 'sum' | 'out' | 'in';
/** 供应商也是名下机器订阅的模板：账单时区（null = 跟随组织时区）、流量重置（自然月 / 账单日）、提前提醒天数 */
export interface Provider { id: string; name: string; code: string; panel_url: string | null; timezone: string | null; traffic_rule: TrafficRule; traffic_reset: TrafficReset; remind_days: number }
export interface Plan {
  id: string; provider_id: string; name: string; cpu_cores: number | null; mem_mb: number | null; disk_gb: number | null;
  bandwidth_mbps: number | null; traffic_gb: number | null; price_cents: number; currency: string; cycle: Cycle; notes: string | null;
  archived_at: string | null; machine_count: number;
  /** 服务端折好的 USD（分）：每期价格、按周期折成的月费；没有汇率为 null */
  price_usd_cents?: number | null; monthly_usd_cents?: number | null; usd_per_unit?: number | null;
}

export interface Overview {
  nodes: Record<'total' | 'ok' | 'degraded' | 'down' | 'unknown' | 'maintenance', number>;
  relays: Record<'total' | 'ok' | 'degraded' | 'down' | 'unknown' | 'maintenance', number>;
  /** 在线/离线按台机器、按 agent 连通（同 isQuiet），与健康无关；同机 IP 节点随宿主机计入（ip_nodes 是其中的台数），
   *  家宽代理不需要 agent，不计入，另数 no_agent */
  online: number; offline: number; no_agent: number; ip_nodes: number;
  /** 闲置备用（isIdleSpare，客户侧口径：没有出口指着、没有在用 IP 节点）的台数：它们不在 nodes / relays 的 total 与各健康计数里，也不计入 online / offline；速率照算。旧中控没有这个字段 */
  spare?: number;
  /** 闲置备用自己的健康计数（同 nodes 的口径）：备用池里有几台此刻能顶替（总数 − 故障 − 未知）。旧中控没有这个字段 */
  spare_health?: Record<'total' | 'ok' | 'degraded' | 'down' | 'unknown' | 'maintenance', number>;
  /** 中控刚启动或刚恢复收上报（2 分钟内）：agent 的重发、线路机连通性还在陆续到，健康计数还会变——网页先显示加载态。旧中控没有这个字段 */
  settling?: boolean;
  /** failed：上架失败（lifecycle = failed）的台数 */
  provisioning: number; retiring: number; failed: number;
  /** 在用机器里即将到期（due_state = soon）与已过期（overdue）的台数 */
  due_soon: number; overdue: number;
  /** 月费合计折成 USD（每台的 monthly_usd_cents 相加；没有汇率的单列） */
  monthly_usd: UsdTotal;
  /** 月费合计里的台数：在用、不在备用团队、有订阅的（正好是 monthly_usd 加起来的那些台）。旧中控没有 */
  paid_machines?: number;
  /** 不续费的备用机（isUnbilledSpare）的月费小计（不在 monthly_usd 里）；count = 其中有订阅的台数（备用团队里被出口指着 / 有在用 IP 节点 / 开着自动续费的照常续费，算在 monthly_usd 里）。旧中控没有 */
  monthly_usd_spare?: UsdTotal & { count: number };
  /** counted_from：本月数据不完整时的起点（RFC 3339，agent 接入晚于月初），完整则 null */
  traffic_month: { up: number; down: number; counted_from: string | null }; net_rx_bps: number; net_tx_bps: number;
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
  traffic_rules: Record<string, string>;
  /** 计费方式短名（小标签用，key 同 traffic_rules）：「双向 · 入 + 出」「单向 · 取较大」… */
  traffic_rule_short: Record<string, string>; traffic_resets: Record<string, string>; due_states: Record<string, string>;
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
  /** 中控公钥全文（上架页「复制中控公钥」：供应商面板录入它之后可以不填 root 密码）；中控读不到时为 null */
  ssh_public_key: string | null;
  /** 按需测速（服务端 jobs/speedtest.rs）：可选的下载量（MB）、默认值、目标（第一个是默认） */
  speedtest: SpeedtestMeta;
}
export interface SpeedtestMeta { sizes_mb: number[]; default_mb: number; targets: { key: string; name: string }[] }
export type UsageMetric = 'cpu' | 'mem' | 'disk' | 'load';

export type JobState = 'queued' | 'running' | 'waiting' | 'cancelling' | 'succeeded' | 'failed' | 'cancelled';
export type JobKind = 'provision_vps' | 'provision_proxy' | 'provision_relay' | 'provision_ip' | 'attach_entry' | 'move_entry' | 'retire' | 'purge' | 'upgrade_agent' | 'replace' | 'tune' | 'ss_refresh' | 'speedtest' | 'router_sync';
/** 步骤 state 文档未列全，前端按 pending/running/waiting/succeeded(done)/failed/skipped(cancelled) 宽松处理 */
export interface JobStep { idx: number; key: string; title: string; state: string; started_at: string | null; finished_at: string | null; output: Record<string, unknown> | null; error: string | null; error_kind?: string | null }
/** 作业列表（GET /jobs）的一项：不带 steps。step = 当前（或失败的）步骤下标，step_title 为它的标题，做完为 null */
export interface JobSummary {
  id: string; kind: JobKind | string; target: { id: string; name: string; /** 退役（进备用、回收）前的名字，没退役过为 null */ origin?: string | null } | null; state: JobState | string;
  step: number; step_count: number; step_title: string | null; error: string | null; created_by: string; created_at: string; started_at: string | null; finished_at: string | null;
  /** 失败且要人去软路由上手工处理（改一半、换回失败）时才有：说明与要处理的软路由、出口；普通失败为 null */
  manual?: JobManual | null;
}
export interface JobManual { message: string; routers: { id: string; name: string; exits: string[] }[] }
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
  /** 入口模式；不给 = 面板设置的新入口默认模式（GET /settings/entry-mode） */
  entry_mode?: EntryMode;
  /** 只要到期日（选了套餐时必填），其余没给的跟随套餐 / 供应商；没有套餐（含同机 IP 节点）要给 price_cents */
  subscription?: { due_date: string; price_cents?: number; currency?: string; cycle?: string; billing_tz?: string; remind_days?: number; traffic_gb?: number; traffic_reset_day?: number; traffic_rule?: TrafficRule };
  /** 同机 IP 节点：加在这台 VPS 节点（宿主机）上。带了它时 ssh_port / login / ss_port / provider_id / plan_id 不适用（服务端忽略）：
   *  SSH 端口与供应商取宿主机的、不带套餐，SS 端口由中控在宿主机上分配 */
  host_id?: string;
}
/** ss_port：同机 IP 节点暂定的 SS 端口（作业在宿主机上核对时被占用会换），其它为 null */
/** warnings：不影响通过的提醒（发行版已过标准支持期、用中控密钥试登录没有结果）；旧中控没有这个字段 */
/** entry_mode：这台新入口的模式（线路机为 null；家宽代理、同机 IP 节点要穿透时已退回普通，warnings 里写明）。旧中控没有这个字段 */
export interface ProvisionCheckItem { ok: boolean; errors: string[]; warnings: string[]; name: string; number: number | null; ss_port: number | null; entry_mode?: EntryMode | null }

// ---- 以 API.md「返回形状补充」为准 ----
/** series：{metric, range, resolution, points}，点里时间字段叫 ts。
 *  host: cpu_pct mem_pct disk_pct load1 net_rx_bps net_tx_bps（1h 分辨率另有 cpu_max，无 load1）；traffic: up down conns；
 *  probe: tcp_ok tcp_ms egress_ok egress_ms（失败时 *_ms 为 null） */
export interface SeriesPoint { ts: string; [k: string]: number | boolean | string | null }
/** 心跳格：每台机器近 60 分钟、20 个 3 分钟桶（旧→新），每桶 [次数, 成功次数, 成功的延迟中位数 ms, SYN 重发过的次数]；[0,0,null,0] = 这段没有数据。
 *  节点是线路机 → 节点的 TCP 探测；线路机是主探测来源（中控或探测点）→ 线路机的连通性（中控只判通不通、延迟为 null；探测点带建连延迟） */
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
  /** engine：写这条告警的引擎（machine 机器、ipintel 来源画像、central 中控自身、router 软路由、probe 探测点）；
   *  subject.kind：软路由的告警对象是路由，为 "router"（机器的没有这个字段） */
  id: string; engine: 'machine' | 'ipintel' | 'central' | 'router' | 'probe'; rule: string; rule_name: string; subject: { id: string; name: string; kind?: 'router' } | null; subject_id: string | null; subject_name: string | null;
  severity: 'info' | 'warning' | 'critical' | string; state: 'firing' | 'resolved'; summary: string;
  opened_at: string; last_seen_at: string | null; resolved_at: string | null; ack_at: string | null; ack_by: string | null;
  /** 恢复后 30 分钟内复发、合并进这条的次数 */
  reopen_count: number;
  /** 近 7 天同对象同规则开过几次（含这次） */
  recent_count?: number;
  /** 近 7 天反复（recent_count 达到中控定的门槛）：网页标「近 7 天第 N 次」、status 列入「近 7 天反复」 */
  chronic?: boolean;
  /** 建议操作：可以直接执行的 fleetctl 命令（只有进行中的、只有几条规则有）；quick_switch：其中临时切换的结构化一份（网页「临时切到…」用）。旧中控没有这两个字段 */
  suggested?: string[];
  quick_switch?: { router_id: string; router: string; group: string; from: string; to: string }[];
}
/** 静默：subject_id 为 null = 全部机器，rule 为 null = 全部规则（rule_name 是规则中文名，同告警）。列表只有没结束的：进行中的与已预约的（starts_at 还没到） */
/** 静默 / 维护窗口的对象类型；全部为 null */
export type SilenceKind = 'machine' | 'router' | 'probe_point';
export interface Silence { id: string; subject_kind: SilenceKind | null; subject_id: string | null; subject_name: string | null; rule: string | null; rule_name: string | null; starts_at: string; ends_at: string; is_maintenance: boolean; comment: string }
/** currency：登记时订阅的币种（之后改订阅币种，这条的金额不跟着变） */
export interface Renewal { id: string; old_due: string | null; new_due: string; amount_cents: number | null; currency: string; note: string | null; actor: string; created_at: string }
/** 审计：界面只显示 title 与 summary（服务端生成的中文）；detail 是原始 JSON，不要直接渲染 */
export interface AuditEntry { id: string; ts: string; actor: string; action: string; title: string; summary: string; target_kind: string | null; target_id: string | null; detail: unknown }
/** GET /machines/{id}/rename-preview：改团队 / 编号后的名字，原团队因此连带改号的节点（换团队时原团队剩下的压缩成 01..N），
 *  和随宿主机一起换团队的同机 IP 节点（ip_nodes：新团队里接在宿主机之后取最小空号） */
export interface RenamePreview { name: string; old_name: string; renames: { machine_id: string; old_name: string; new_name: string }[]; ip_nodes: { machine_id: string; old_name: string; new_name: string }[];
  /** 换团队时还指向这台和一起走的同机 IP 节点的软路由出口（进备用的客户会继续用；换到别的团队的改名后出口名不跟着改）；没换团队为 [] */
  router_exits: { node: string; router: string; exit: string }[];
  /** 改到备用团队但进不了备用的原因（订阅已过期满 1 天、没有订阅、有进行中的作业…；保存会被拒，要进备用请用退役）；否则 null */
  spare_blocked: string | null
  /** 改到备用团队且进得了备用时，进去后不自动回收的原因（出口还指着这台或它上面的同机 IP 节点、已在备用的同机 IP 节点开着自动续费；同退役预览 spare.held）；否则 null */
  held: string | null
  /** 还被软路由出口指着的节点（这台和随它走的同机 IP 节点）换到的团队生效的访问策略与现在不同时，每台一句；
   *  原团队压缩改名的节点还被出口指着时一句「团队2-09（路由 · 节点09）→ 团队2-08」（面板名和出口名会对不上）；其余为 [] */
  warnings: string[] }
/** notes：指向要改名的节点的软路由出口（出口名不跟着改） */
export interface RenumberPreview { changes: { machine_id: string; old_name: string; new_name: string }[]; notes: string[]; token: string }
/** 软路由上要改的出口（替换节点、换线路机、搬空同一个格式）；换线路机的 new_entry_id 为 null、to 写「server:新端口」 */
export interface RouterExitPlan { id: string; name: string; state: 'online' | 'offline' | 'pending' | string; exits: { exit: string; old_entry_id: string; new_entry_id: string | null; from: string; to: string | null; error?: string | null }[] }
/** 换线路机 / 搬空线路机（API.md「生命周期操作」）：GET 是预览（没有 job_id），POST 的响应同样的内容加 job_id */
/** 换走的入口（线路机和端口都会变；新入口的端口作业建好时才分配） */
export interface OldEntry { id: string; relay: string; port: number }
export interface MovePreview { name?: string; from?: string; to?: string; old_entry?: OldEntry; routers_enabled: boolean; routers: RouterExitPlan[]; ok: boolean; blockers: string[]; warnings: string[]; share_changed: boolean; note: string }
/** 搬迁建议（GET /relays/{id}/rebalance，只读）：重置前会用满时，搬走哪些入口、搬去哪台；daily_bytes 是搬走后这台每天少的流量 */
export interface RebalancePlan {
  relay: { id: string; name: string }; limit_gb: number; used: number; period_end: string; daily_avg: number | null; forecast_full_at: string | null; projected_pct: number | null;
  /** over_limit：本周期已超出额度，搬迁目标是搬走后日均降到原来的 20% 以下（到重置日前不再大量增长），不是撑到重置日 */
  needed: boolean; ok: boolean; reason: string | null; over_limit?: boolean;
  target: { id: string; name: string; limit_gb: number | null; used: number; daily_avg: number | null; projected_pct: number | null } | null;
  moves: { node: { id: string; name: string }; entry_id: string; port: number; period_bytes: number; daily_bytes: number; needs_from: boolean }[];
  after?: { daily_avg: number; projected_pct: number }; target_after?: { fits: boolean; projected_pct: number | null };
}
/** 放置矩阵（GET /placement）：各在用 VPS 节点到每台线路机近 window_h 小时的往返。cells 与 relays 同序；
 *  current_relay_id：节点恰有一条入口（待应用 / 在用 / 停用都算）且在用时它所在的线路机，否则 null、不给批量建议；entry_relay_ids：在用入口所在的线路机（多入口节点有多个）；best_relay_id：成功率 ≥ 99%、非故障、额度放得下的里面 p50 最小的；
 *  suggest：当前比最优慢 ≥ min_gain_ms 或当前成功率 < 99%；no_room：更快但额度放不下而跳过的线路机 */
export interface PlacementCell { relay_id: string; samples: number; ok: number; success_pct: number | null; p50_ms: number | null }
export interface PlacementNode {
  id: string; name: string; team: { id: string; name: string } | null; ip: string; current_relay_id: string | null; entry_relay_ids: string[]; entries: number;
  cells: PlacementCell[]; tcp_fail_7d: number; daily_bytes: number; best_relay_id: string | null; suggest: boolean; reason: string | null; no_room: string[];
}
export interface PlacementView {
  relays: { id: string; name: string; status: HealthStatus }[];
  nodes: PlacementNode[];
  suggestions: { to: { id: string; name: string }; nodes: { id: string; name: string; from: { id: string; name: string }; port: number; reason: string | null }[]; command: string }[];
  /** 多入口节点：换掉某台线路机上的入口（带 --from 的单条命令，不进批量） */
  single_suggestions: { node: { id: string; name: string }; from: { id: string; name: string }; to: { id: string; name: string }; port: number; reason: string; command: string }[];
  window_h: number; interval_s: number; min_success_pct: number; min_gain_ms: number;
}
export interface MoveResult extends MovePreview { job_id: string; name: string; from: string; to: string }
export interface EvacuateResult { from: string; to: string; /** job_id 只在真正搬空（POST）的结果里有，预览没有 */ jobs: { machine_id: string; name: string; from: string; old_entry: OldEntry; job_id?: string; routers: RouterExitPlan[] }[]; skipped: { machine_id: string; name: string | null; reason: string }[]; warnings: string[];
  /** 要改的软路由：同批一起改，每台只重启 1 次 OpenClash（times 恒为 1）；nodes 是有出口要改的节点数 */
  router_restarts?: { id: string; name: string; times: number; nodes: number }[];
  /** 因软路由不在线（offline）/ 还没接入（pending）而被跳过的节点的出口，按软路由合并（每台一条；这些节点不计入 router_restarts） */
  offline_routers?: OfflineRouter[]; share_changed: boolean; note: string }
export interface OfflineRouter { id: string; name: string; state: string; error: string | null; exits: { node: string; exit: string }[] }
/** 批量换线路机（GET / POST /moves）：同搬空；有一个节点不能换（都在 skipped 里）就 ok 为 false、整批不能提交。
 *  blockers 是别的原因（带节点名）；因软路由不在线不能换的不在 blockers 里，列在 offline_routers */
export interface MovesResult extends EvacuateResult { ok: boolean; blockers: string[] }
/** 替换节点预览（GET /machines/{old}/replace）：新节点接旧编号（new_name），旧节点去 old_to（默认同团队的备用团队）；
 *  routers：出口指向旧节点、会改成新节点入口的软路由（state 不是 online 时 blockers 里有原因）；renames：新节点原来的团队压缩编号 */
export interface ReplaceSide { id: string; name: string; kind: Kind; team: { id: string; name: string } | null }
export interface ReplacePreview {
  old: ReplaceSide; new: ReplaceSide; new_name: string;
  old_to: { team: { id: string; name: string; spare: boolean } | null; number: number | null; name: string };
  renames: { machine_id: string; old_name: string; new_name: string }[];
  routers_enabled: boolean;
  routers: RouterExitPlan[];
  ok: boolean; blockers: string[]; warnings: string[];
}
export interface ReplaceBody { new_machine_id: string; old_to?: { team_id?: string; number?: number }; routers?: boolean }
/** 退役预览：spare = 能不能移入备用（ok；不能时 why 写原因）、去哪（to_name）、从哪天起自动回收（recycle_on）；
 *  ip_nodes：会一并走的同机 IP 节点（宿主机才有）；routers：还指向它的软路由出口（只警告）；
 *  renames：原团队后面的节点前移补号，按所有受影响的团队算 */
export interface RetirePreview {
  name: string;
  /** notes：进备用的提示（开着的自动续费会关掉、撤销退役回不到原编号）；warnings：还被出口指着的节点进备用后访问策略会变（客户受影响） */
  spare: { ok: boolean; why: string | null; team: { id: string; name: string }; to_name: string; recycle_on: string | null;
    /** 进备用后不自动回收的原因（软路由出口还指着、同机 IP 节点还在用 / 开着自动续费，多个用「、」连）；null = 没被扣住；旧中控没有这个字段 */
    held?: string | null;
    ip_nodes: { id: string; name: string; to_name: string }[]; notes: string[]; warnings: string[] };
  routers: { id: string; name: string; exits: { exit: string; node: string }[] }[];
  renames: { machine_id: string; old_name: string; new_name: string }[];
  ip_nodes: { id: string; name: string }[];
  /** 现在做不了的原因（如线路机还挂着节点入口，要先搬空）；没有或为空 = 可以退役 */
  blockers?: string[];
  /** 两种去向都有的提醒：原团队前移改名的节点还被软路由出口指着（面板名和出口名会对不上）。旧中控没有这个字段 */
  warnings?: string[];
}
/** 退役：mode=spare 已移入备用（name 是新名、from 原名、recycle_on 起自动回收）；mode=now 提交了清理作业（宿主机的同机 IP 节点各一个，没有为 []） */
export type RetireResult =
  | { mode: 'spare'; name: string; from: string; recycle_on: string | null; held?: string | null; ip_nodes: { id: string; name: string; from: string }[]; auto_renew_off: boolean; notes: string[] }
  | { mode: 'now'; job_id: string; ip_node_jobs: { machine_id: string; name: string; job_id: string }[] };

/** 回收站一行（GET /recycle-bin，只读）：source = fleet 本面板 / legacy 旧面板导入；name 是退役前的名字，last_name 回收时的名字 */
export interface RecycleItem {
  id: number; source: 'fleet' | 'legacy'; source_id: string; machine_id: string | null; kind: Kind;
  name: string; last_name: string | null; team: string | null; ip: string | null; host_name: string | null;
  provider: string | null; plan: string | null; service_ref: string | null;
  price_cents: number | null; currency: string | null; cycle: string | null;
  activated_at: string | null; retired_at: string | null; due_date: string | null; recycled_at: string; reason: string | null;
  traffic_up: number | null; traffic_down: number | null; nic_rx: number | null; nic_tx: number | null; actor: string;
}
/** 回收时的快照（fleet 来源）：机器行（不含秘密）、订阅、续费、入口历史、出口；legacy 来源是导入时的原始 JSON，结构不定，所以字段都可缺 */
export interface RecycleSnapshot {
  machine?: { note?: string | null } & Record<string, unknown>; name?: string; team?: string | null; host?: string | null; provider?: string | null; plan?: string | null;
  subscription?: object | null;
  renewals?: { id?: string; old_due: string; new_due: string; amount_cents: number; currency: string; note: string; actor: string; created_at: string }[];
  entries?: { relay: string; node: string; port: number; lifecycle: string; created_at: string; removed_at: string | null }[];
  outbound?: { protocol?: string; host?: string; port?: number; method?: string | null; username?: string | null; updated_at?: string } | null;
}
/** GET /recycle-bin/{id}：一项加上快照 */
export interface RecycleDetail extends RecycleItem { snapshot: RecycleSnapshot }

// ---- 设置页：API 密钥、告警通知渠道（契约见 COMMON.md） ----
export type ApiScope = 'read' | 'write' | 'secrets' | 'exec';
export interface ApiKey { id: string; name: string; prefix: string; scopes: ApiScope[] | string[]; created_at: string; expires_at: string | null; last_used_at: string | null; revoked_at: string | null }
export type NotifyKind = 'telegram' | 'feishu';
export type NotifySeverity = 'info' | 'warning' | 'critical';
export interface NotifyChannel { id: string; name: string; kind: NotifyKind | string; min_severity: NotifySeverity | string; enabled: boolean; /** 收每月 1 号发的上月摘要 */ monthly_report: boolean; created_at: string; config_hint: string }
/** channel_name：渠道删了之后为 null（发送记录还留着） */
export interface NotifyLogEntry { id: string; channel_id: string; channel_name: string | null; incident_id: string | null; ts: string; ok: boolean; error: string | null;
  /** firing / repeat / resolved / test；silence 静默 / 维护变更；auto 备用团队的机器到期自动回收；jobfail 作业失败；switch 出口自动切回推迟；report 月报（subject_name 是「月报」） */
  kind: 'firing' | 'repeat' | 'resolved' | 'test' | 'silence' | 'auto' | 'jobfail' | 'switch' | 'report'; subject_name: string | null; rule_name: string | null }

/** 服务端分页列表（/jobs、/incidents）：next_offset 只在后面还有数据时给出 */
export interface Paged<T> { items: T[]; total: number; next_offset: number | null }

// ---- 账户安全（/account/*，只给登录会话） ----
export interface TotpStatus { enabled: boolean; recovery_codes_left: number }
/** 开启两步验证的第一步：待确认的密钥（10 分钟内用 enable 确认）。qr_svg 是服务端画好的二维码 SVG */
export interface TotpSetup { secret: string; otpauth_url: string; qr_svg: string }
/** 登录会话：id 是会话哈希前 8 字节的十六进制（不是 Cookie），current = 本设备 */
export interface AccountSession { id: string; created_at: string; last_seen_at: string; expires_at: string; ip: string | null; user_agent: string | null; current: boolean }
/** 每日流量（GET /traffic/daily）：按组织时区的自然日，今天算到目前为止。业务流量（下载 / 上传）来自入口；网卡（下行 / 上行）只有线路机，
 *  daily[].billed 是当天按供应商规则（rule）算的计费量；线路机的 billed 是区间合计，先求区间内下行之和、上行之和再套规则（和额度已用同口径），不是每天相加 */
export interface DailyBiz { date: string; down: number; up: number }
export interface DailyNic { date: string; rx: number; tx: number; billed: number }
/** 节点：以它为节点的全部入口（含换线前的旧入口）；direct = 线路机的直出入口（id 是线路机的）；窗口内有流量的已退役机器照列 */
export interface TrafficDailyNode {
  id: string; name: string; team: { id: string; name: string } | null; kind: Kind; lifecycle: Lifecycle; country: string | null; direct: boolean; down: number; up: number; daily: DailyBiz[];
  /** 节点自己的网卡（VPS 节点，同流量额度的口径）：生效的计费方式、区间合计的下行 / 上行 / 计费量、每天一格；直出行、家宽代理、同机 IP 节点、没有读数的为 null */
  rule: TrafficRule | null; rx: number | null; tx: number | null; billed: number | null; nic_daily: DailyNic[] | null;
  /** 月费（USD 分）：在用且有价格的；直出、没价格为 null */
  monthly_usd_cents: number | null;
  /** 月费 ÷ 按取回的天数日均折成的 30 天业务流量；页面按所选区间自己重算 */
  usd_per_gb: number | null;
  /** 近 idle.days 天日均业务流量（字节）；直出为 null */
  avg7: number | null;
  /** 闲置：在用、上架满 idle.days 天、avg7 低于 idle.gb_per_day（阈值只在服务端定义） */
  idle: boolean;
  /** 被扣住不回收的备用机（软路由出口还指着 / 同机 IP 节点还在用 / 开着自动续费）：照常续费，月费计入合计、不灰显 */
  held: boolean;
}
/** 团队成本（近 30 天，不受团队筛选影响）：unpriced = 在用节点里没价格、没算进月费的台数 */
export interface TrafficDailyTeam { id: string; name: string; spare: boolean; nodes: number; idle: number; monthly_usd_cents: number | null; unpriced: number; bytes_30d: number; usd_per_gb: number | null;
  /** 备用团队里被扣住不回收的在用台数及它们近 30 天的流量（别的团队为 0）；备用团队的月费、没价格台数、USD/GB 只按这几台算（不续费的备用机不算） */
  held: number; held_bytes_30d: number }
/** 线路机：每天的网卡与计费量（daily），经过它的业务流量（business） */
export interface TrafficDailyRelay { id: string; name: string; lifecycle: Lifecycle; country: string | null; rule: TrafficRule; rx: number; tx: number; billed: number; down: number; up: number; daily: DailyNic[]; business: DailyBiz[] }
export interface TrafficDaily { tz: string; dates: string[]; total: DailyBiz[]; idle: { days: number; gb_per_day: number }; teams: TrafficDailyTeam[]; nodes: TrafficDailyNode[]; relays: TrafficDailyRelay[] }

// ---- 报表：月度可用率（GET /reports/uptime）、月报（GET /reports/monthly、POST /reports/monthly/send）与按需测速 ----
/** 一个在用节点（不含线路机、备用团队）在窗口内的可用率：成功率 = 小时汇总里成功次数 ÷ 探测次数（同抽屉「连接质量」），窗口内没有探测为 null；
 *  出口成功率是主指标（客户流量走的路径），TCP 是次要的参考；断线次数：出口探测失败 3 次以上的整点小时，连着的合成一次；
 *  incidents 是窗口内可用性相关规则（出口 / TCP 不通、agent 离线、没有入口）开出的告警次数，含 30 分钟内复发合并的 */
export interface UptimeItem {
  id: string; name: string; team: { id: string; name: string } | null; provider: { id: string; name: string } | null;
  egress_total: number; egress_ok: number; egress_pct: number | null; tcp_total: number; tcp_ok: number; tcp_pct: number | null;
  outages: number; longest_outage_h: number; incidents: number;
}
/** 按供应商汇总（id 为 null = 未填供应商）：with_data = 窗口内有出口探测的节点数，平均 / 最差的出口只看它们；worst = 出口最差的那台 */
export interface UptimeProvider {
  id: string | null; name: string; nodes: number; with_data: number; avg_egress_pct: number | null; worst_egress_pct: number | null; worst: { id: string; name: string } | null;
  avg_tcp_pct: number | null; worst_tcp_pct: number | null; outages: number; incidents: number;
}
/** month 按月查时是 YYYY-MM（本月算到今天），按 days 查时为 null；from / to 是组织时区的自然日（含） */
export interface UptimeReport { month: string | null; days: number; from: string; to: string; tz: string; items: UptimeItem[]; providers: UptimeProvider[] }
export interface MonthlyReport {
  month: string; from: string; to: string; tz: string;
  traffic: { down: number; up: number; total: number };
  /** 月费合计（USD 分，没汇率的币种在 unconverted 里） */
  monthly_usd: { cents: number; unconverted?: { currency: string; cents: number }[] };
  machines: number;
  costly: { id: string; name: string; monthly_usd_cents: number; gb: number; usd_per_gb: number }[];
  worst_uptime: { id: string; name: string; egress_pct: number; tcp_pct: number | null; outages: number; longest_outage_h: number }[];
  top_rules: { rule: string; name: string; count: number }[];
  due_soon: number; overdue: number;
  /** 通知里发出的整段文字 */
  text: string;
}
export interface MonthlySendResult { month: string; channels: number; text: string }
/** POST /machines/{id}/speedtest：traffic_mb = 这次约会用掉的流量 */
export interface SpeedtestStarted { machine_id: string; name: string; job_id: string; size_mb: number; target: string; target_name: string; traffic_mb: number }
/** 测速作业步骤的输出（jobs/speedtest.rs summarize）：avg_mbps 只算收到第一个字节之后的；complete = false 时 300 秒内没下完，速率按已下载的算 */
export interface SpeedtestOutput { target: string; target_name: string; url: string; size_mb: number; bytes: number; complete: boolean; avg_mbps: number; avg_mb_s: number; total_s: number; ttfb_s: number; remote_ip: string }
