// 由 fleet-server 生成（crates/fleet-server/tests/types_gen.rs，ts-rs），不要手改。
// 重新生成：cd v1 && FLEET_TYPES_GEN=write cargo test -p fleet-server --test types_gen
// 时间字段是 RFC 3339 字符串（日期是 YYYY-MM-DD），整数都是 number。

export type Agent = { version: string | null, last_report_at: string | null, online: boolean, 
/**
 * agent 最近一次报告的 Xray 是否在运行（仅线路机；没报过为 None）
 */
xray_running: boolean | null, 
/**
 * agent 报的可用核数（容器里是限额）；没报过为 None
 */
cores: number | null, 
/**
 * agent 自报的健康数据（离线队列、访问日志尾读、探测与对齐，自 agent 进程启动起累计）；旧 agent 没报过为 None。
 * 每次上报都变，不进实时指标推送（live.rs 的 agent 字段不含它），随 REST 重取更新
 */
health: AgentHealth | null, 
/**
 * agent 时钟偏差（毫秒）= 中控收到时刻 − agent 报的发送时刻，正数 = agent 时钟慢；只按积压之外的报告更新，没算过为 None。
 * 同 `health` 不进实时指标推送；偏差过大由健康原因 `clock_skew` 体现
 */
clock_skew_ms: number | null, 
/**
 * agent 计量流量用的网卡（SystemInfo.nic_ifaces）；旧 agent 没报过为空
 */
nic_ifaces: Array<string>, 
/**
 * 线路机的 AnyTLS 外壳（sing-box）：装没装、在不在跑、版本、最近一次应用错误；旧 agent / 节点没报过为 None
 */
anytls: SingboxStatus | null, };

/**
 * Agent 自身的健康数据：离线队列、访问日志尾读、最近一次应用失败、写盘失败。服务端存进 agent_state.info。
 */
export type AgentHealth = { 
/**
 * 离线队列里待发送的报告数
 */
queue_len: number, 
/**
 * 因超过字节/时间上限被丢弃的报告数
 */
queue_dropped: number, 
/**
 * 被移入 dead/ 的报告数：只有服务端永久拒绝（400/413/422 等 4xx）的才进 dead/，5xx 一直重试
 */
queue_dead: number, 
/**
 * 访问日志里解析不了的行
 */
access_unparsed: number, 
/**
 * 访问日志里被丢弃的行（超长、主机名含非法字符、轮转后旧文件末尾的残行）
 */
access_dropped_lines: number, 
/**
 * 轮转后找不到旧文件、可能漏掉一段
 */
access_rotation_gaps: number, access_truncations: number, 
/**
 * 最近一次应用失败的错误（成功后清空）
 */
last_apply_error?: string | null, 
/**
 * 这份报告写不进磁盘队列（磁盘满等）、是直接从内存发来的：写盘的错误。写得进时不带
 */
disk_write_error?: string | null, };

/**
 * 总览里节点、线路机、闲置备用各一份的健康计数
 */
export type Bucket = { total: number, ok: number, degraded: number, down: number, unknown: number, maintenance: number, };

export type EntryView = { id: string, relay: Ref, 
/**
 * 直出入口的节点就是线路机自己（id 同 relay，名字是「线路机名-直出」）
 */
node: Ref, 
/**
 * 线路机直出：从线路机自己的 IP 出去，没有落地节点，也就没有 TCP 探测（tcp 恒为 null）
 */
direct: boolean, 
/**
 * normal / reverse（穿透：节点主动连这个入口，线路机不连节点，见 crate::reverse）。
 * tcp 的方向跟着模式：普通 = 线路机 → 节点（线路机 agent 量），穿透 = 节点 → 线路机（节点 agent 量，只画图，不进告警，见 health.rs）
 */
mode: string, 
/**
 * 穿透入口的 bridge 端口（节点连线路机的这个端口；None = 连入口端口）。普通入口恒为 None
 */
bridge_port: number | null, port: number, lifecycle: string, 
/**
 * 线路机已应用到这条入口的最近一次改动
 */
applied: boolean, apply_error: string | null, tcp: Probe | null, egress: Probe | null, 
/**
 * 中控 → 这个入口端口（线路机上）最近一次 TCP 检查；只有在用入口有，没检查过为 None。与 tcp / egress（线路机 → 节点）是两段
 */
reach: Reach | null, traffic: Traffic, };

export type Health = { status: Status, 
/**
 * 机器可读的原因键（告警规则名同名）
 */
reasons: Array<Reason>, };

export type HostNow = { ts: string, cpu_pct: number | null, load1: number | null, mem_used: number | null, mem_total: number | null, disk_used: number | null, disk_total: number | null, net_rx_bps: number | null, net_tx_bps: number | null, tcp_conns: number | null, };

export type IncidentOut = { id: string, 
/**
 * 写这条告警的引擎（incident::Engine：machine / ipintel / central / router / probe）：前端按它决定点对象时开什么
 */
engine: string, rule: string, subject_id: string, subject_name: string | null, severity: string, state: string, summary: string, opened_at: string, last_seen_at: string, resolved_at: string | null, ack_at: string | null, ack_by: string | null, 
/**
 * 恢复后又在重开窗口内复发的次数
 */
reopen_count: number, };

export type JobOut = { id: string, kind: string, state: string, step: number, error: string | null, created_by: string, created_at: string, started_at: string | null, finished_at: string | null, };

export type JobRef = { id: string, kind: string, state: string, };

export type JsonValue = number | string | boolean | Array<JsonValue> | { [key in string]: JsonValue } | null;

export type LogOut = { id: number, ts: string, level: string, msg: string, };

export type MachineView = { id: string, kind: string, name: string, team: Ref | null, 
/**
 * 所在团队是备用团队（已退役、等到期回收）：在用时订阅过期满 1 天由中控清理并移进回收站（见 spare.rs）。没有团队为 false
 */
spare: boolean, 
/**
 * 进备用团队前的显示名（「团队2-07」）与进备用的时刻（即退役时间）；不在备用团队为 null
 */
spare_from: string | null, spare_since: string | null, 
/**
 * 备用团队里在用、有订阅的机器自动回收的日子（账单时区，到期日 + 2 天，见 spare::recycle_on）；其余为 null。
 * 还被软路由出口指着的（`router_exits` 非空）、上面还有在用的同机 IP 节点的（`live_ip_nodes` 非空）、上面有开着自动续费的备用同机 IP 节点的（`renewing_ip_nodes` 非空）、开着自动续费的不自动回收，这里为 null、`recycle_held` 为 true
 */
recycle_on: string | null, 
/**
 * 备用团队里在用的机器，因为软路由出口还指着它（`router_exits`）、上面还有在用的同机 IP 节点（`live_ip_nodes`）、上面有开着自动续费的备用同机 IP 节点
 * （`renewing_ip_nodes`，宿主机退役会连带它）或自己开着自动续费（订阅的 `auto_renew`：一直续下去，永远到不了回收日）所以不自动回收：
 * 显示「软路由出口还指着，不自动回收」/「同机 IP 节点 X 还在用，不自动回收」/「开着自动续费，不自动回收」（有几个都写），不写回收日。
 * 这是费用侧的口径：被扣住的照常续费（月费、到期、待付清单、续费提醒），不被扣住的是 `spare_unbilled`
 */
recycle_held: boolean, 
/**
 * 闲置备用（客户侧口径）：备用团队里在用、没有软路由出口指着（`router_exits` 空，含经同机 IP 节点）、没有在用的同机 IP 节点（`live_ip_nodes` 空）的机器：
 * 不关系到客户，所以不开告警、不进总览的健康计数与在线 / 离线、不算故障、可用率不含。不看自动续费（只开着自动续费的备用机照样是闲置的，只是照常续费，见 `spare_unbilled`）。
 * 网页、fleetctl 据此判断，不自己拼条件
 */
idle_spare: boolean, 
/**
 * 不续费的备用机（费用侧口径）：备用团队里在用、没被扣住不回收（`recycle_held` 为 false）的机器，是等到期放生的：不提醒续费、不算到期，月费不计入合计，待付清单默认不列。
 * 一定是闲置备用（`idle_spare`），反过来只开着自动续费的闲置备用机不是它
 */
spare_unbilled: boolean, 
/**
 * 备用团队里在用的宿主机：它上面不在备用团队、没退役的同机 IP 节点（显示名，如「团队1-02」）——宿主机退役会连带它们，所以不自动回收（spare::run_once keeps_live），
 * 也不是闲置备用（宿主机断了它们跟着断，要续费、要告警）；其余为空
 */
live_ip_nodes: Array<string>, 
/**
 * 备用团队里在用的宿主机：它上面在备用团队里、开着自动续费的同机 IP 节点（显示名）——宿主机回收会连带它们，所以也不自动回收（spare::run_once keeps_live），
 * 宿主机照常续费（`recycle_held`）；它们不关系到客户，宿主机仍可以是闲置备用。其余为空
 */
renewing_ip_nodes: Array<string>, 
/**
 * 备用团队里在用的机器：还指向它（备用宿主机还包括它上面没退役的同机 IP 节点，后缀「（经同机 IP 节点 X）」）的软路由出口（「路由名 · 出口名」），续费提醒里列出来；
 * 它、`live_ip_nodes` 都为空的就是闲置备用（`idle_spare`，网页据此不把它算进异常）
 */
router_exits: Array<string>, 
/**
 * 团队里在用的节点（不在备用）、所在团队的软路由都看得清出口（routers::pointed_nodes），却没有任何出口指着它：
 * 多半是出口被改指向别处（改出口指向、手改）之后留下的，卡片看着像正常在用、其实没有客户。网页抽屉与 fleetctl show 标一句，不影响健康与告警。
 * 团队没有软路由、有路由还没采集到 / 配置读不了的都为 false（看不出）
 */
no_router_exits: boolean, number: number | null, label: string | null, ip: string, 
/**
 * 国家（ISO 3166-1 两位代码，如 "US"），名字旁显示国旗：手动指定的（`country_set`），没指定按地址查离线 IP 库
 * （走画像缓存，同访问监控的来源画像）；内网地址、家宽代理填的域名、库里查不到的为 null
 */
country: string | null, 
/**
 * 上架或修改时手动指定的国家；null = 按 IP 自动
 */
country_set: string | null, 
/**
 * 标了不计费（PUT /machines/{id}/billing-exempt）：计费对账把它当不在团队里（ledger.rs）
 */
billing_exempt: boolean, ssh_port: number, 
/**
 * 同机 IP 节点：它所在的宿主机（SSH、agent、主机读数都是宿主机的，`agent` / `host` 就是宿主机的）；其它机器为 null
 */
host_machine: Ref | null, 
/**
 * 同机 IP 节点的公网 IP 是 1:1 NAT 映射的：它在宿主机网卡上的本地地址（ssserver 监听并从它出站）；公网 IP 就在网卡上的、其它机器为 null
 */
bind_ip: string | null, 
/**
 * 端口转发型 NAT 节点（入口 IP 与出站 IP 不同）：它的出口 IP；`ip` 是线路机连的入口。null = 出口就是 `ip`
 */
egress_ip: string | null, 
/**
 * 出口 IP 动态（供应商的出口会变，只验收能出网；此时 egress_ip 为 null）
 */
egress_dynamic: boolean, 
/**
 * 动态出口节点：最近一次成功出口探测看到的 IP（入口上有成功的出口探测样本才有）；其它机器为 null
 */
egress_seen: string | null, 
/**
 * 宿主机：它上面未退役的同机 IP 节点
 */
ip_nodes: Array<Ref>, provider: Ref | null, provider_code: string | null, plan: PlanRef | null, service_ref: string | null, lifecycle: string, lifecycle_reason: string | null, note: string, revision: number, created_at: string, activated_at: string | null, retired_at: string | null, subscription: Subscription | null, sub_template: SubTemplate, agent: Agent | null, host: HostNow | null, 
/**
 * 节点：它的入口；线路机：挂在它上面的入口
 */
entries: Array<EntryView>, 
/**
 * 已移除入口（如换线路机后的旧入口）本月 / 今天的流量：入口列表里没有它们，但这些流量仍属于这台机器
 */
removed_traffic: Traffic, 
/**
 * 按网卡计的今天 / 本月流量：只有有自己 agent 的机器（线路机、VPS、宿主机）有；家宽代理、同机 IP 节点为 null（看业务流量）
 */
nic: NicTraffic | null, 
/**
 * 本月（组织时区）的数据从这一刻才有：首次上架完成（activated_at）晚于本月 1 号 0 点时为它，否则为 null。
 * 网卡与业务的本月流量都适用
 */
month_counted_from: string | null, 
/**
 * 流量额度：周期里已用（网卡按供应商规则，或业务流量）
 */
quota: Quota, health: Health, active_job: JobRef | null, 
/**
 * 上架失败（lifecycle = failed）：最近一次失败的上架作业；退役中（retiring）：失败的退役作业。网页据此给出「查看作业」
 */
failed_job: JobRef | null, open_incidents: number, maintenance: boolean, 
/**
 * 仅线路机
 */
relay: RelayInfo | null, 
/**
 * 系统调优记录（jobs/tune.rs）；家宽代理和同机 IP 节点（系统设置是宿主机的）为 null
 */
tune: TuneInfo | null, };

export type MetaCycle = { key: string, label: string, months: number, };

export type MetaDefaults = { currency: string, cycle: string, remind_days: number, traffic_rule: string, traffic_reset: string, ss_method: string, ss_port: number, };

/**
 * `GET /meta`：meta.rs 的常量与中文名，另带中控公钥
 */
export type MetaOut = { ssh_public_key: string | null, org_timezone: string, org_timezone_label: string, org_utc_offset_s: number, timezones: Array<string>, currencies: Array<string>, cycles: Array<MetaCycle>, kinds: { [key in string]: string }, ip_node_label: string, lifecycles: { [key in string]: string }, healths: { [key in string]: string }, severities: { [key in string]: MetaSeverity }, job_kinds: { [key in string]: string }, job_states: { [key in string]: string }, step_states: { [key in string]: string }, entry_states: { [key in string]: string }, traffic_rules: { [key in string]: string }, traffic_rule_short: { [key in string]: string }, traffic_resets: { [key in string]: string }, due_states: { [key in string]: string }, policy_modes: { [key in string]: string }, policy_sync_states: { [key in string]: string }, rules: { [key in string]: string }, thresholds: MetaThresholds, defaults: MetaDefaults, terminal: TerminalMeta, speedtest: SpeedtestMeta, };

export type MetaSeverity = { label: string, tone: string, };

export type MetaThresholds = { traffic_quota_warn_pct: number, traffic_quota_over_pct: number, usage: MetaUsage, };

export type MetaUsage = { cpu: MetaUsageLevel, mem: MetaUsageLevel, disk: MetaUsageLevel, load: MetaUsageLevel, };

/**
 * 主机读数的两档：warn 起黄、bad 起红（同健康判定）
 */
export type MetaUsageLevel = { warn: number, bad: number, };

/**
 * 有自己 agent 的机器（非家宽代理、非同机 IP 节点）按网卡计的今天 / 本月流量（字节，组织时区，同入口的今天 / 本月）：
 * rx = 下行（本机收），tx = 上行（本机发）。来自 host_metric_hourly
 */
export type NicTraffic = { today_rx: number, today_tx: number, month_rx: number, month_tx: number, };

/**
 * 总览 KPI（GET /overview，实时推送的 `overview` 同一份，字段见 docs/API.md）
 */
export type Overview = { nodes: Bucket, relays: Bucket, provisioning: number, retiring: number, failed: number, due_soon: number, overdue: number, online: number, offline: number, no_agent: number, ip_nodes: number, spare: number, spare_health: Bucket, monthly_usd: Total, paid_machines: number, monthly_usd_spare: SpareTotal, traffic_month: TrafficMonth, net_rx_bps: number, net_tx_bps: number, 
/**
 * 中控刚启动或刚恢复收上报（`ReportGaps::settling`）：只有 GET /overview 与实时推送带（`overview_typed`、live.rs），`overview_of` 不带
 */
settling?: boolean, };

/**
 * `GET /overview/series`
 */
export type OverviewSeries = { points: Array<RatePoint>, };

/**
 * 合计里的一个币种
 */
export type Part = { currency: string, 
/**
 * 原币种的合计（分）
 */
cents: number, 
/**
 * 折成的 USD（分，逐台折算后相加）
 */
usd_cents: number, 
/**
 * 所用汇率：1 单位合多少 USD
 */
usd_per_unit: number, };

/**
 * `GET /ping-beats` 的一个槽位（见 `beats`）
 */
export type PingBeat = { slot: number, name: string, target: string, 
/**
 * 最近一次读数的方式（icmp / tcp），没有读数时是此刻合成的方式
 */
mode: string, cur_ms: number | null, 
/**
 * 丢包 %（一位小数）
 */
cur_loss: number | null, 
/**
 * RFC 3339
 */
cur_ts: string | null, 
/**
 * 20 格，旧→新
 */
cells: Array<[number | null, number | null]>, };

/**
 * `GET /ping-beats`：`{<线路机 id>: [PingBeat]}`
 */
export type PingBeats = { [key in string]: Array<PingBeat> };

export type PlanRef = { id: string, name: string, bandwidth_mbps: number | null, traffic_gb: number | null, };

export type Probe = { ok: boolean, latency_ms: number | null, detail: string | null, ts: string, 
/**
 * TCP 探测：近 1 小时成功样本延迟的中位数（卡片读数；单次的 SYN 重发尖刺不左右它）。出口探测为 None
 */
median_ms: number | null, };

/**
 * `GET /probe-beats`：`{<机器 id>: [Bucket × 20]}`（见 `probe_beats`）
 */
export type ProbeBeats = { [key in string]: Array<[number, number, number | null, number]> };

/**
 * 流量额度周期：有订阅的按账单时区里最近一次（生效的）重置日起算，没有订阅的是模板时区（供应商的，没填为组织时区）的本月；
 * 供应商按滚动 30 天（rolling30）且订阅没单独设置重置日的：此刻往前整 30 天（720 小时，quota::rolling_start；同供应商面板的 last 30 days，和时区无关）
 */
export type Quota = { 
/**
 * 生效的流量上限（GB，十进制）；没有上限为 None。告警、网页、fleetctl 都读它，不各自再算
 */
limit_gb: number | null, 
/**
 * 上限从哪来：subscription = 订阅单独设置，plan = 跟随套餐，host = 同机 IP 节点与宿主机共用（没有自己的上限），none = 不限
 */
limit_source: string, 
/**
 * 周期内已用（字节）：有 agent 的机器按网卡流量、照生效的计量规则（订阅单独设置的，或供应商的）算（宿主机含同机 IP 节点）；
 * 家宽代理与同机 IP 节点没有自己的网卡读数，是业务流量（下载 + 上传，含已移除的入口）
 */
used: number, 
/**
 * 周期起点的日期（账单时区）：每月重置的是重置日；滚动 30 天是窗口起点时刻（`period_start_at`）落在账单时区的哪一天，只用来显示「X 起」
 */
period_start: string, 
/**
 * 下次重置日（账单时区，这一天 0 点起是新周期）；滚动 30 天没有重置日，为 None
 */
period_end: string | null, 
/**
 * monthly = 每月重置日起算；rolling30 = 近 30 天（滚动，整 720 小时）
 */
mode: string, 
/**
 * 本周期的数据从这一刻才有：机器首次上架完成（activated_at）晚于周期起点、且没有按面板校准的读数时为它，之前的用量不在 used 里；
 * 否则为 None
 */
counted_from: string | null, 
/**
 * 网卡计量规则（min / max / sum / out / in，没有供应商按 max）；按业务流量算的为 None
 */
rule: string | null, 
/**
 * 按供应商面板校准（`PUT …/subscription` 的 `traffic_used_gb`）：录入时面板上本周期已用的原始读数（字节）与录入时间。
 * 月度周期 used = base + 录入之后的实测，进入下一个周期失效；滚动 30 天的读数按均匀分布逐日线性衰减（quota::calibrated），
 * used = base × (1 − 录入后天数 / 30) + 录入之后的实测，满 30 天失效。这里始终是原始读数，不是衰减后的；失效了两个都为 None
 */
base: number | null, base_at: string | null, 
/**
 * 近 7 天日均用量（字节 / 天；周期不足 7 天按已有天数，不足 1 天为 None），只算实测（不含按面板校准的读数）
 */
daily_avg: number | null, 
/**
 * 按 `daily_avg` 预计用满的时刻（早于下次重置日才有；已用满、没有上限、没有重置日（滚动 30 天）为 None）。精确到小时
 */
forecast_full_at: string | null, };

export type RatePoint = { ts: string, net_rx_bps: number, net_tx_bps: number, };

/**
 * 主探测来源（中控或探测点，见 probe_points/）→ 线路机上这个入口端口的最近一次 TCP 建连（reach.rs，轮流连各端口，所以每个口隔 N 轮才更新一次）
 */
export type Reach = { ok: boolean, ts: string, detail: string | null, 
/**
 * 这个端口到最近一次为止连续连不上的次数（通的为 0）。告警按同一个端口连续 3 次算（reach::unreachable_sql），界面同一口径；
 * 整台线路机连不上（最近 3 轮都失败）时这台上的入口都按连不上：ok 为 false、次数至少 3，时间与原因取最近一轮的
 */
fails: number, };

/**
 * `Reason` 序列化出来的样子（手写的 Serialize 带上 `rule_name`），只给 TypeScript 导出用（tests/types_gen.rs）
 */
export type Reason = { key: string, rule_name: string, severity: string, text: string, };

export type Ref = { id: string, name: string, };

export type RelayInfo = { public_host: string, generation: number, applied_generation: number, applied_at: string | null, 
/**
 * 期望代数最近一次变化的时间（配置未应用告警从这里起算）
 */
generation_at: string | null, apply_error: string | null, port_min: number, port_max: number, 
/**
 * 入口协议：both（Reality 与 AnyTLS）/ reality / anytls（只留 AnyTLS，Reality 入站听回环），见 `PATCH /relays/{id}`
 */
protocols: string, };

export type Silence = { id: string | null, 
/**
 * 对象类型：machine / router / probe_point（[`KINDS`]），全部为 null；新建时给了 subject_id、没给类型的按机器
 */
subject_kind: string | null, subject_id: string | null, rule: string | null, starts_at: string, ends_at: string, is_maintenance: boolean, comment: string, 
/**
 * 只在读取时给出
 */
subject_name: string | null, 
/**
 * 规则中文名（同告警），不限规则时为 null；只在读取时给出
 */
rule_name: string | null, };

/**
 * 线路机上 sing-box（只做 AnyTLS 外壳，见 fleet-agent 的 singbox.rs）的状态。中控存进 agent_state.info.singbox；
 * 入口的 AnyTLS 链接、订阅里的 AnyTLS 条目只在 `running` 时才给（没在跑时链接是坏的）
 */
export type SingboxStatus = { 
/**
 * 线路机上有 sing-box 程序（没有 = 未安装，agent 跳过 AnyTLS，Xray 那一侧照常）
 */
installed: boolean, 
/**
 * fleet-singbox 单元在跑
 */
running: boolean, version?: string | null, 
/**
 * 最近一次应用 sing-box 配置的错误（成功后清空）
 */
last_error?: string | null, };

/**
 * 不续费的备用机的月费小计：合计同 `monthly_usd`，另带台数
 */
export type SpareTotal = { count: number, 
/**
 * 能折算的部分的 USD 合计（分）；不含 `unconverted`
 */
cents: number, 
/**
 * 各原币种（含 USD）的金额与所用汇率，按币种排
 */
by_currency: Array<Part>, 
/**
 * 没有汇率的币种：原币种金额单列，不算进 `cents`
 */
unconverted: Array<Unconverted>, };

/**
 * `GET /meta` 的 `speedtest`
 */
export type SpeedtestMeta = { sizes_mb: [number, number, number], default_mb: number, targets: Array<SpeedtestTarget>, };

/**
 * 测速目标（不带主机名）
 */
export type SpeedtestTarget = { key: string, name: string, };

export type Status = "ok" | "degraded" | "down" | "unknown" | "maintenance" | "inactive";

export type StepOut = { idx: number, key: string, state: string, started_at: string | null, finished_at: string | null, output: JsonValue | null, error: string | null, 
/**
 * 失败类别：network / auth / host_key / remote / timeout / internal
 */
error_kind: string | null, };

/**
 * 机器的订阅模板（库里的 sub_template）：订阅没单独设置的项取这里。没有套餐时 price_cents 为 null（订阅要单独设置价格）
 */
export type SubTemplate = { price_cents: number | null, currency: string, cycle: string, billing_tz: string, traffic_gb: number | null, 
/**
 * 供应商的流量重置方式：month = 自然月（1 号），billing = 账单日（到期日几号）
 */
traffic_reset: string, remind_days: number, 
/**
 * 供应商的流量计费方式（没有供应商为 max）
 */
traffic_rule: string, };

/**
 * 订阅的生效值（库里的 subscription_eff）：价格、币种、周期、流量上限没单独设置的跟随套餐，账单时区、流量重置日跟随供应商
 */
export type Subscription = { price_cents: number, currency: string, cycle: string, due_date: string, 
/**
 * 账单日（每月几号，1–31）：直接设到期日时记下，按周期续费不变；续费的新到期日、按账单日重置的流量重置日都落在这一天（小月按月末）
 */
billing_day: number, due_days: number, 
/**
 * ok / soon / overdue（meta::due_state，按生效的提前提醒天数）：续费告警、总览「即将到期」、网页与 fleetctl 都按它
 */
due_state: string, billing_tz: string, auto_renew: boolean, remind_days: number, 
/**
 * 0 / null = 不限
 */
traffic_gb: number | null, 
/**
 * 生效的重置日（有订阅时总有值）：单独设置的，或供应商按账单日 = 到期日几号、按自然月 = 1
 */
traffic_reset_day: number | null, 
/**
 * 生效的流量计费方式（sum 双向 / max、min、out、in 单向）：单独设置的，或供应商的；只对按网卡计量的机器有意义
 */
traffic_rule: string, 
/**
 * 单独设置了的字段（price_cents / currency / cycle / billing_tz / remind_days / traffic_gb / traffic_reset_day / traffic_rule），其余跟随 `sub_template`
 */
overrides: Array<string>, 
/**
 * 折成每月的费用（分，四舍五入，meta::monthly_cents）：总览的月费合计、网页的月费列
 */
monthly_cents: number, 
/**
 * 折成 USD（分，按 fx.rs 的汇率四舍五入）：月费、每期价格，`usd_per_unit` 是所用汇率（1 单位合多少 USD）；没有汇率时三个都是 null（未折算）
 */
monthly_usd_cents: number | null, price_usd_cents: number | null, usd_per_unit: number | null, 
/**
 * 到期日再推一个周期（落在账单日，小月按月末）：登记续费的默认新到期日
 */
next_due: string, 
/**
 * 当前这一期的天数（到期日往前推一个周期到到期日）：网页算剩余价值
 */
period_days: number, };

/**
 * `GET /meta` 的 `terminal`
 */
export type TerminalMeta = { per_user: number, global: number, idle_s: number, max_age_s: number, max_message: number, max_dim: number, ticket_ttl_s: number, verify_ttl_s: number, };

/**
 * 月费合计（USD）：总览的 `monthly_usd`、团队的 `monthly_usd`
 */
export type Total = { 
/**
 * 能折算的部分的 USD 合计（分）；不含 `unconverted`
 */
cents: number, 
/**
 * 各原币种（含 USD）的金额与所用汇率，按币种排
 */
by_currency: Array<Part>, 
/**
 * 没有汇率的币种：原币种金额单列，不算进 `cents`
 */
unconverted: Array<Unconverted>, };

export type Traffic = { today_up: number, today_down: number, month_up: number, month_down: number, };

/**
 * 本月业务流量合计（字节）；`counted_from`：本月数据从这一刻才有（RFC 3339），完整的为 null
 */
export type TrafficMonth = { up: number, down: number, counted_from: string | null, };

export type TuneInfo = { 
/**
 * 最近一次成功应用的调优档；null = 没调优过（迁移前上线的机器）
 */
profile: string | null, at: string | null, 
/**
 * 机器上的档就是中控当前的档（`jobs::tune::PROFILE`）；false = 旧档或没调优过
 */
current: boolean, 
/**
 * 最近一次调优的推导参数（tune-v2 起）：`{bw, bw_plan, bw_source, rtt, rtt_source, rtt_samples, mem_mb, buf_max}`；
 * null = 没调优过、旧档 tune-v1（固定值）或已撤销
 */
params: JsonValue | null, };

/**
 * 没有汇率、没折算的币种
 */
export type Unconverted = { currency: string, cents: number, };
