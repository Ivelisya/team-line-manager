//! 机器视图：一次把列表页、卡片、抽屉需要的东西组装好（约百台规模，分几条查询再在内存里拼）。

use crate::health::{self, Health, Status};
use crate::meta;
use crate::names::{direct_name, display_name};
use crate::state::AppState;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 未退役（含上架中、退役中）
    Live,
    /// 还没进回收站的全部机器（已退役、还没收进回收站的也在内）。回收站里的看 GET /recycle-bin
    All,
}

impl Scope {
    /// 对 `machine m` 的条件
    fn cond(self) -> &'static str {
        match self {
            Scope::Live => "m.lifecycle <> 'retired'",
            Scope::All => "m.recycled_at is null",
        }
    }
}

/// 精简列表（`GET /machines?view=brief`）：id、类型、显示名、线路机名、IP、生命周期、团队、宿主机 id，排序同完整视图。
/// fleetctl 按名字 / IP 找机器只要这些：一条查询，不为解析一个名字组装全部机器（含已退役）的完整视图
pub async fn brief(st: &AppState, scope: Scope) -> sqlx::Result<Value> {
    sqlx::query_scalar(&format!(
        "select coalesce(json_agg(json_build_object(
                    'id', m.id, 'kind', m.kind, 'name', display_name(m.kind::text, m.label, t.name, m.number), 'label', m.label,
                    'ip', host(m.primary_ip), 'lifecycle', m.lifecycle, 'host_id', m.host_id,
                    'team', case when t.id is not null then json_build_object('id', t.id, 'name', t.name) end)
                  order by m.kind = 'relay' desc, t.sort_order nulls first, t.name, m.number, m.label, m.id), '[]')
         from machine m left join team t on t.id = m.team_id
         where {}",
        scope.cond()
    ))
    .fetch_one(&st.view_db)
    .await
}

#[derive(Debug, Clone, Serialize)]
pub struct Ref {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanRef {
    pub id: Uuid,
    pub name: String,
    pub bandwidth_mbps: Option<i32>,
    pub traffic_gb: Option<i32>,
}

/// 订阅的生效值（库里的 subscription_eff）：价格、币种、周期、流量上限没单独设置的跟随套餐，账单时区、流量重置日跟随供应商
#[derive(Debug, Clone, Serialize)]
pub struct Subscription {
    pub price_cents: i32,
    pub currency: String,
    pub cycle: String,
    pub due_date: Date,
    /// 账单日（每月几号，1–31）：直接设到期日时记下，按周期续费不变；续费的新到期日、按账单日重置的流量重置日都落在这一天（小月按月末）
    pub billing_day: i32,
    pub due_days: i64,
    /// ok / soon / overdue（meta::due_state，按生效的提前提醒天数）：续费告警、总览「即将到期」、网页与 fleetctl 都按它
    pub due_state: &'static str,
    pub billing_tz: String,
    pub auto_renew: bool,
    pub remind_days: i32,
    /// 0 / null = 不限
    pub traffic_gb: Option<i32>,
    /// 生效的重置日（有订阅时总有值）：单独设置的，或供应商按账单日 = 到期日几号、按自然月 = 1
    pub traffic_reset_day: Option<i32>,
    /// 生效的流量计费方式（sum 双向 / max、min、out、in 单向）：单独设置的，或供应商的；只对按网卡计量的机器有意义
    pub traffic_rule: String,
    /// 单独设置了的字段（price_cents / currency / cycle / billing_tz / remind_days / traffic_gb / traffic_reset_day / traffic_rule），其余跟随 `sub_template`
    pub overrides: Vec<String>,
    /// 折成每月的费用（分，四舍五入，meta::monthly_cents）：总览的月费合计、网页的月费列
    pub monthly_cents: i64,
    /// 折成 USD（分，按 fx.rs 的汇率四舍五入）：月费、每期价格，`usd_per_unit` 是所用汇率（1 单位合多少 USD）；没有汇率时三个都是 null（未折算）
    pub monthly_usd_cents: Option<i64>,
    pub price_usd_cents: Option<i64>,
    pub usd_per_unit: Option<f64>,
    /// 到期日再推一个周期（落在账单日，小月按月末）：登记续费的默认新到期日
    pub next_due: Date,
    /// 当前这一期的天数（到期日往前推一个周期到到期日）：网页算剩余价值
    pub period_days: i64,
}

/// 机器的订阅模板（库里的 sub_template）：订阅没单独设置的项取这里。没有套餐时 price_cents 为 null（订阅要单独设置价格）
#[derive(Debug, Clone, Serialize)]
pub struct SubTemplate {
    pub price_cents: Option<i32>,
    pub currency: String,
    pub cycle: String,
    pub billing_tz: String,
    pub traffic_gb: Option<i32>,
    /// 供应商的流量重置方式：month = 自然月（1 号），billing = 账单日（到期日几号）
    pub traffic_reset: String,
    pub remind_days: i32,
    /// 供应商的流量计费方式（没有供应商为 max）
    pub traffic_rule: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub version: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_report_at: Option<OffsetDateTime>,
    pub online: bool,
    /// agent 最近一次报告的 Xray 是否在运行（仅线路机；没报过为 None）
    pub xray_running: Option<bool>,
    /// agent 报的可用核数（容器里是限额）；没报过为 None
    pub cores: Option<i32>,
    /// agent 自报的健康数据（离线队列、访问日志尾读、探测与对齐，自 agent 进程启动起累计）；旧 agent 没报过为 None。
    /// 每次上报都变，不进实时指标推送（live.rs 的 agent 字段不含它），随 REST 重取更新
    pub health: Option<fleet_proto::AgentHealth>,
    /// agent 时钟偏差（毫秒）= 中控收到时刻 − agent 报的发送时刻，正数 = agent 时钟慢；只按积压之外的报告更新，没算过为 None。
    /// 同 `health` 不进实时指标推送；偏差过大由健康原因 `clock_skew` 体现
    pub clock_skew_ms: Option<i64>,
    /// agent 计量流量用的网卡（SystemInfo.nic_ifaces）；旧 agent 没报过为空
    pub nic_ifaces: Vec<String>,
    /// 线路机的 AnyTLS 外壳（sing-box）：装没装、在不在跑、版本、最近一次应用错误；旧 agent / 节点没报过为 None
    pub anytls: Option<fleet_proto::SingboxStatus>,
}

/// 最近 5 分钟主机负载的「最低值」：最低值都超阈值才算持续高负载（健康判定用，不对外输出）
#[derive(Debug, Clone, Copy, Default)]
pub struct Sustained {
    /// 这 5 分钟里样本覆盖了多少秒；不足则不判定
    pub span_s: i64,
    pub cpu_min: Option<f32>,
    pub mem_min: Option<f32>,
    pub disk_min: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostNow {
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub cpu_pct: Option<f32>,
    pub load1: Option<f32>,
    pub mem_used: Option<i64>,
    pub mem_total: Option<i64>,
    pub disk_used: Option<i64>,
    pub disk_total: Option<i64>,
    pub net_rx_bps: Option<f64>,
    pub net_tx_bps: Option<f64>,
    pub tcp_conns: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Probe {
    pub ok: bool,
    pub latency_ms: Option<f32>,
    pub detail: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    /// TCP 探测：近 1 小时成功样本延迟的中位数（卡片读数；单次的 SYN 重发尖刺不左右它）。出口探测为 None
    pub median_ms: Option<f32>,
    /// 最近几次（最多 3 次，15 分钟内）里结果与这一次相同的连续次数：出口 / TCP 告警按它判恢复（连续 2 次成功，alerts.rs）。只给告警用，不对外输出
    #[serde(skip)]
    pub streak: u8,
}

/// 主探测来源（中控或探测点，见 probe_points/）→ 线路机上这个入口端口的最近一次 TCP 建连（reach.rs，轮流连各端口，所以每个口隔 N 轮才更新一次）
#[derive(Debug, Clone, Serialize)]
pub struct Reach {
    pub ok: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub detail: Option<String>,
    /// 这个端口到最近一次为止连续连不上的次数（通的为 0）。告警按同一个端口连续 3 次算（reach::unreachable_sql），界面同一口径；
    /// 整台线路机连不上（最近 3 轮都失败）时这台上的入口都按连不上：ok 为 false、次数至少 3，时间与原因取最近一轮的
    pub fails: i32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Traffic {
    pub today_up: i64,
    pub today_down: i64,
    pub month_up: i64,
    pub month_down: i64,
}

/// 有自己 agent 的机器（非家宽代理、非同机 IP 节点）按网卡计的今天 / 本月流量（字节，组织时区，同入口的今天 / 本月）：
/// rx = 下行（本机收），tx = 上行（本机发）。来自 host_metric_hourly
#[derive(Debug, Clone, Serialize, Default)]
pub struct NicTraffic {
    pub today_rx: i64,
    pub today_tx: i64,
    pub month_rx: i64,
    pub month_tx: i64,
}

/// 流量额度周期：有订阅的按账单时区里最近一次（生效的）重置日起算，没有订阅的是模板时区（供应商的，没填为组织时区）的本月；
/// 供应商按滚动 30 天（rolling30）且订阅没单独设置重置日的：账单时区的今天往前 29 天起（含今天共 30 个日历日）
#[derive(Debug, Clone, Serialize)]
pub struct Quota {
    /// 生效的流量上限（GB，十进制）；没有上限为 None。告警、网页、fleetctl 都读它，不各自再算
    pub limit_gb: Option<i32>,
    /// 上限从哪来：subscription = 订阅单独设置，plan = 跟随套餐，host = 同机 IP 节点与宿主机共用（没有自己的上限），none = 不限
    pub limit_source: &'static str,
    /// 周期内已用（字节）：有 agent 的机器按网卡流量、照生效的计量规则（订阅单独设置的，或供应商的）算（宿主机含同机 IP 节点）；
    /// 家宽代理与同机 IP 节点没有自己的网卡读数，是业务流量（下载 + 上传，含已移除的入口）
    pub used: i64,
    pub period_start: Date,
    /// 下次重置日（账单时区，这一天 0 点起是新周期）；滚动 30 天没有重置日，为 None
    pub period_end: Option<Date>,
    /// monthly = 每月重置日起算；rolling30 = 近 30 天（滚动）
    pub mode: &'static str,
    /// 本周期的数据从这一刻才有：机器首次上架完成（activated_at）晚于周期起点、且没有接入前读数时为它，之前的用量不在 used 里；
    /// 否则为 None
    #[serde(with = "time::serde::rfc3339::option")]
    pub counted_from: Option<OffsetDateTime>,
    /// 网卡计量规则（min / max / sum / out / in，没有供应商按 max）；按业务流量算的为 None
    pub rule: Option<&'static str>,
    /// 接入前读数（割接时按供应商面板录的，只读保留：接口已不收新值）：录入时面板上的已用（字节）与录入时间，used 已含它。
    /// 不在当前周期内的不算，为 None——进入下一个周期自动失效
    pub base: Option<i64>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub base_at: Option<OffsetDateTime>,
    /// 近 7 天日均用量（字节 / 天；周期不足 7 天按已有天数，不足 1 天为 None），口径同 `used`（不含接入前读数）
    pub daily_avg: Option<i64>,
    /// 按 `daily_avg` 预计用满的时刻（早于下次重置日才有；已用满、没有上限、没有重置日（滚动 30 天）为 None）。精确到小时
    #[serde(with = "time::serde::rfc3339::option")]
    pub forecast_full_at: Option<OffsetDateTime>,
    /// 周期起点时刻与日均所取窗口的起点（搬迁建议按它们汇总各入口的流量，不对外输出）
    #[serde(skip)]
    pub period_start_at: OffsetDateTime,
    #[serde(skip)]
    pub window_start: OffsetDateTime,
    /// 有订阅时距下次重置还有几天（账单时区的日历天，≥ 1）；没有订阅、滚动 30 天为 None。流量额度告警的摘要用，不对外输出
    #[serde(skip)]
    pub reset_in_days: Option<i64>,
}

/// 今天或之前最近一次重置日；月份没有这一天的按月末算（31 号在 30 天的月份是 30 号）
pub fn period_start(today: Date, reset_day: u8) -> Date {
    let at = |y: i32, m: time::Month| Date::from_calendar_date(y, m, reset_day.min(m.length(y))).expect("合法日期");
    let d = at(today.year(), today.month());
    if d <= today {
        return d;
    }
    let pm = today.month().previous();
    at(if pm == time::Month::December { today.year() - 1 } else { today.year() }, pm)
}

/// 周期起点（`period_start` 的结果）之后的下一次重置日：下个月的这一天，同样按月末截断
pub fn next_reset(start: Date, reset_day: u8) -> Date {
    let m = start.month().next();
    let y = if m == time::Month::January { start.year() + 1 } else { start.year() };
    Date::from_calendar_date(y, m, reset_day.min(m.length(y))).expect("合法日期")
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryView {
    pub id: Uuid,
    pub relay: Ref,
    /// 直出入口的节点就是线路机自己（id 同 relay，名字是「线路机名-直出」）
    pub node: Ref,
    /// 线路机直出：从线路机自己的 IP 出去，没有落地节点，也就没有 TCP 探测（tcp 恒为 null）
    pub direct: bool,
    /// normal / reverse（穿透：节点主动连这个入口，线路机不连节点，见 crate::reverse）。
    /// tcp 的方向跟着模式：普通 = 线路机 → 节点（线路机 agent 量），穿透 = 节点 → 线路机（节点 agent 量，只画图，不进告警，见 health.rs）
    pub mode: String,
    pub port: i32,
    pub lifecycle: String,
    /// 线路机已应用到这条入口的最近一次改动
    pub applied: bool,
    pub apply_error: Option<String>,
    /// 这条入口所在线路机的 Xray 已确认没在运行（节点侧据此抑制出口/TCP 告警）
    #[serde(skip)]
    pub relay_xray_down: bool,
    /// 这条入口所在线路机的 agent 离线：探测是它做的，最后一次结果之后不再更新（全网失联时尤其如此），节点侧按「判不出」处理
    #[serde(skip)]
    pub relay_offline: bool,
    /// 这条入口所在线路机在维护中（改防火墙、换内核…）：经它的探测失败不算节点的问题，节点侧同样按「判不出」处理
    #[serde(skip)]
    pub relay_maint: bool,
    pub tcp: Option<Probe>,
    pub egress: Option<Probe>,
    /// 中控 → 这个入口端口（线路机上）最近一次 TCP 检查；只有在用入口有，没检查过为 None。与 tcp / egress（线路机 → 节点）是两段
    pub reach: Option<Reach>,
    pub traffic: Traffic,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobRef {
    pub id: Uuid,
    pub kind: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineView {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub team: Option<Ref>,
    /// 所在团队是备用团队（已退役、等到期回收）：在用时订阅过期满 1 天由中控清理并移进回收站（见 spare.rs）。没有团队为 false
    pub spare: bool,
    /// 进备用团队前的显示名（「团队2-07」）与进备用的时刻（即退役时间）；不在备用团队为 null
    pub spare_from: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub spare_since: Option<OffsetDateTime>,
    /// 备用团队里在用、有订阅的机器自动回收的日子（账单时区，到期日 + 2 天，见 spare::recycle_on）；其余为 null。
    /// 还被软路由出口指着的（`router_exits` 非空）、上面还有在用的同机 IP 节点的（`live_ip_nodes` 非空）、上面有开着自动续费的备用同机 IP 节点的（`renewing_ip_nodes` 非空）、开着自动续费的不自动回收，这里为 null、`recycle_held` 为 true
    pub recycle_on: Option<Date>,
    /// 备用团队里在用的机器，因为软路由出口还指着它（`router_exits`）、上面还有在用的同机 IP 节点（`live_ip_nodes`）、上面有开着自动续费的备用同机 IP 节点
    /// （`renewing_ip_nodes`，宿主机退役会连带它）或自己开着自动续费（订阅的 `auto_renew`：一直续下去，永远到不了回收日）所以不自动回收：
    /// 显示「软路由出口还指着，不自动回收」/「同机 IP 节点 X 还在用，不自动回收」/「开着自动续费，不自动回收」（有几个都写），不写回收日。
    /// 这是费用侧的口径：被扣住的照常续费（月费、到期、待付清单、续费提醒），不被扣住的是 `spare_unbilled`
    pub recycle_held: bool,
    /// 闲置备用（客户侧口径）：备用团队里在用、没有软路由出口指着（`router_exits` 空，含经同机 IP 节点）、没有在用的同机 IP 节点（`live_ip_nodes` 空）的机器：
    /// 不关系到客户，所以不开告警、不进总览的健康计数与在线 / 离线、不算故障、可用率不含。不看自动续费（只开着自动续费的备用机照样是闲置的，只是照常续费，见 `spare_unbilled`）。
    /// 网页、fleetctl 据此判断，不自己拼条件
    pub idle_spare: bool,
    /// 不续费的备用机（费用侧口径）：备用团队里在用、没被扣住不回收（`recycle_held` 为 false）的机器，是等到期放生的：不提醒续费、不算到期，月费不计入合计，待付清单默认不列。
    /// 一定是闲置备用（`idle_spare`），反过来只开着自动续费的闲置备用机不是它
    pub spare_unbilled: bool,
    /// 备用团队里在用的宿主机：它上面不在备用团队、没退役的同机 IP 节点（显示名，如「团队1-02」）——宿主机退役会连带它们，所以不自动回收（spare::run_once keeps_live），
    /// 也不是闲置备用（宿主机断了它们跟着断，要续费、要告警）；其余为空
    pub live_ip_nodes: Vec<String>,
    /// 备用团队里在用的宿主机：它上面在备用团队里、开着自动续费的同机 IP 节点（显示名）——宿主机回收会连带它们，所以也不自动回收（spare::run_once keeps_live），
    /// 宿主机照常续费（`recycle_held`）；它们不关系到客户，宿主机仍可以是闲置备用。其余为空
    pub renewing_ip_nodes: Vec<String>,
    /// 备用团队里在用的机器：还指向它（备用宿主机还包括它上面没退役的同机 IP 节点，后缀「（经同机 IP 节点 X）」）的软路由出口（「路由名 · 出口名」），续费提醒里列出来；
    /// 它、`live_ip_nodes` 都为空的就是闲置备用（`idle_spare`，网页据此不把它算进异常）
    pub router_exits: Vec<String>,
    /// 团队里在用的节点（不在备用）、所在团队的软路由都看得清出口（routers::pointed_nodes），却没有任何出口指着它：
    /// 多半是出口被改指向别处（改出口指向、手改）之后留下的，卡片看着像正常在用、其实没有客户。网页抽屉与 fleetctl show 标一句，不影响健康与告警。
    /// 团队没有软路由、有路由还没采集到 / 配置读不了的都为 false（看不出）
    pub no_router_exits: bool,
    pub number: Option<i32>,
    pub label: Option<String>,
    pub ip: String,
    /// 国家（ISO 3166-1 两位代码，如 "US"），名字旁显示国旗：手动指定的（`country_set`），没指定按地址查离线 IP 库
    /// （走画像缓存，同访问监控的来源画像）；内网地址、家宽代理填的域名、库里查不到的为 null
    pub country: Option<String>,
    /// 上架或修改时手动指定的国家；null = 按 IP 自动
    pub country_set: Option<String>,
    /// 标了不计费（PUT /machines/{id}/billing-exempt）：计费对账把它当不在团队里（ledger.rs）
    pub billing_exempt: bool,
    pub ssh_port: i32,
    /// 同机 IP 节点：它所在的宿主机（SSH、agent、主机读数都是宿主机的，`agent` / `host` 就是宿主机的）；其它机器为 null
    pub host_machine: Option<Ref>,
    /// 同机 IP 节点的公网 IP 是 1:1 NAT 映射的：它在宿主机网卡上的本地地址（ssserver 监听并从它出站）；公网 IP 就在网卡上的、其它机器为 null
    pub bind_ip: Option<String>,
    /// 端口转发型 NAT 节点（入口 IP 与出站 IP 不同）：它的出口 IP；`ip` 是线路机连的入口。null = 出口就是 `ip`
    pub egress_ip: Option<String>,
    /// 出口 IP 动态（供应商的出口会变，只验收能出网；此时 egress_ip 为 null）
    pub egress_dynamic: bool,
    /// 动态出口节点：最近一次成功出口探测看到的 IP（入口上有成功的出口探测样本才有）；其它机器为 null
    pub egress_seen: Option<String>,
    /// 宿主机：它上面未退役的同机 IP 节点
    pub ip_nodes: Vec<Ref>,
    pub provider: Option<Ref>,
    pub provider_code: Option<String>,
    pub plan: Option<PlanRef>,
    pub service_ref: Option<String>,
    pub lifecycle: String,
    pub lifecycle_reason: Option<String>,
    pub note: String,
    pub revision: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub activated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub retired_at: Option<OffsetDateTime>,
    pub subscription: Option<Subscription>,
    pub sub_template: SubTemplate,
    pub agent: Option<Agent>,
    pub host: Option<HostNow>,
    /// 节点：它的入口；线路机：挂在它上面的入口
    pub entries: Vec<EntryView>,
    /// 已移除入口（如换线路机后的旧入口）本月 / 今天的流量：入口列表里没有它们，但这些流量仍属于这台机器
    pub removed_traffic: Traffic,
    /// 按网卡计的今天 / 本月流量：只有有自己 agent 的机器（线路机、VPS、宿主机）有；家宽代理、同机 IP 节点为 null（看业务流量）
    pub nic: Option<NicTraffic>,
    /// 本月（组织时区）的数据从这一刻才有：首次上架完成（activated_at）晚于本月 1 号 0 点时为它，否则为 null。
    /// 网卡与业务的本月流量都适用
    #[serde(with = "time::serde::rfc3339::option")]
    pub month_counted_from: Option<OffsetDateTime>,
    /// 流量额度：周期里已用（网卡按供应商规则，或业务流量）
    pub quota: Quota,
    pub health: Health,
    pub active_job: Option<JobRef>,
    /// 上架失败（lifecycle = failed）：最近一次失败的上架作业；退役中（retiring）：失败的退役作业。网页据此给出「查看作业」
    pub failed_job: Option<JobRef>,
    pub open_incidents: i64,
    pub maintenance: bool,
    /// 仅线路机
    pub relay: Option<RelayInfo>,
    /// 系统调优记录（jobs/tune.rs）；家宽代理和同机 IP 节点（系统设置是宿主机的）为 null
    pub tune: Option<TuneInfo>,
    #[serde(skip)]
    pub sustained: Sustained,
    /// 节点的有效访问策略是「暂停新连接」（健康判定里提示，不算故障）
    #[serde(skip)]
    pub policy_blocked: bool,
    /// 流量异常的判断（anomaly.rs，按小时汇总算）；中控启动后还没算过、或这台不在用为 None
    #[serde(skip)]
    pub anomaly: Option<crate::anomaly::Anomaly>,
    /// 持续握手丢包的判断（tcp_loss.rs，每 60 秒算一次）：完整加载与补指标时都取最新那份；中控启动后还没算过为 Unknown
    #[serde(skip)]
    pub tcp_loss: crate::tcp_loss::Loss,
}

/// 被扣住不回收的备用机（`recycle_held`：软路由出口还指着、上面还有在用的同机 IP 节点、开着自动续费）：它们照常续费，团队页、流量页的月费合计要带上。
/// 调用方传在用机器视图（views::live，最多旧 3 秒；算不出来时退回最近一份），都没有为空
pub fn held_ids(live: Option<Arc<Vec<MachineView>>>) -> HashSet<Uuid> {
    live.map(|v| v.iter().filter(|m| m.recycle_held).map(|m| m.id).collect()).unwrap_or_default()
}

#[cfg(test)]
impl MachineView {
    /// 测试用：按 spare / lifecycle / router_exits / live_ip_nodes / renewing_ip_nodes / 订阅的 auto_renew 补算 `recycle_held`、`idle_spare`、`spare_unbilled`（同视图构造里的算法）
    pub(crate) fn settle_spare(mut self) -> Self {
        let in_spare = self.spare && self.lifecycle == "active";
        self.recycle_held = in_spare
            && !(self.router_exits.is_empty() && self.live_ip_nodes.is_empty() && self.renewing_ip_nodes.is_empty() && !self.subscription.as_ref().is_some_and(|s| s.auto_renew));
        self.idle_spare = in_spare && self.router_exits.is_empty() && self.live_ip_nodes.is_empty();
        self.spare_unbilled = in_spare && !self.recycle_held;
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TuneInfo {
    /// 最近一次成功应用的调优档；null = 没调优过（迁移前上线的机器）
    pub profile: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub at: Option<OffsetDateTime>,
    /// 机器上的档就是中控当前的档（`jobs::tune::PROFILE`）；false = 旧档或没调优过
    pub current: bool,
    /// 最近一次调优的推导参数（tune-v2 起）：`{bw, bw_plan, bw_source, rtt, rtt_source, rtt_samples, mem_mb, buf_max}`；
    /// null = 没调优过、旧档 tune-v1（固定值）或已撤销
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelayInfo {
    pub public_host: String,
    pub generation: i64,
    pub applied_generation: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub applied_at: Option<OffsetDateTime>,
    /// 期望代数最近一次变化的时间（配置未应用告警从这里起算）
    #[serde(with = "time::serde::rfc3339::option")]
    pub generation_at: Option<OffsetDateTime>,
    pub apply_error: Option<String>,
    pub port_min: i32,
    pub port_max: i32,
    /// 入口协议：both（Reality 与 AnyTLS）/ reality / anytls（只留 AnyTLS，Reality 入站听回环），见 `PATCH /relays/{id}`
    pub protocols: String,
    /// 主探测来源（中控或探测点，reach.rs）最近 3 轮（5 分钟内）都连不上，或某个在用入口端口最近 3 次（30 分钟、至少 4 遍轮换内）都连不上：
    /// 告警摘要（写明哪一路测的，括号里是失败原因：去重，带端口；单个端口的取它最近一次）；否则为 None（`reach::unreachable_sql`）。
    /// 健康判定据此报 relay_unreachable；它变了 reach.rs 发 `Event::Machine`（视图完整重算），不对外输出
    #[serde(skip)]
    pub unreachable: Option<String>,
    /// 有在用入口、5 分钟内主探测来源的检查不到 3 轮（刚重启、整轮作废），或主来源离线（暂由中控代替 / 标灰）：整台通不通判不出，
    /// relay_unreachable 的原因消失不算恢复
    #[serde(skip)]
    pub reach_unknown: bool,
}

#[derive(sqlx::FromRow)]
struct MRow {
    id: Uuid,
    kind: String,
    team_id: Option<Uuid>,
    team_name: Option<String>,
    team_spare: Option<bool>,
    spare_from: Option<String>,
    spare_since: Option<OffsetDateTime>,
    number: Option<i32>,
    label: Option<String>,
    ip: String,
    ssh_port: i32,
    host_id: Option<Uuid>,
    host_name: Option<String>,
    bind_ip: Option<String>,
    egress_ip: Option<String>,
    egress_dynamic: bool,
    provider_id: Option<Uuid>,
    provider_name: Option<String>,
    provider_code: Option<String>,
    plan_id: Option<Uuid>,
    plan_name: Option<String>,
    plan_bw: Option<i32>,
    plan_traffic: Option<i32>,
    service_ref: Option<String>,
    country: Option<String>,
    billing_exempt: bool,
    lifecycle: String,
    lifecycle_reason: Option<String>,
    note: String,
    revision: i32,
    created_at: OffsetDateTime,
    activated_at: Option<OffsetDateTime>,
    retired_at: Option<OffsetDateTime>,
    s_price: Option<i32>,
    s_currency: Option<String>,
    s_cycle: Option<String>,
    s_due: Option<Date>,
    s_tz: Option<String>,
    /// 账单时区里的「今天」
    s_today: Option<Date>,
    s_auto: Option<bool>,
    s_remind: Option<i32>,
    s_traffic: Option<i32>,
    s_reset: Option<i32>,
    s_base: Option<i64>,
    s_base_at: Option<OffsetDateTime>,
    s_overrides: Option<Vec<String>>,
    s_bday: Option<i32>,
    s_rule: Option<String>,
    t_price: Option<i32>,
    t_currency: String,
    t_cycle: String,
    t_tz: String,
    t_traffic: Option<i32>,
    t_reset: String,
    t_remind: i32,
    /// 模板时区里的「今天」（没有订阅时的流量周期）
    t_today: Date,
    /// 供应商的流量计费方式（模板里的）
    t_rule: String,
    #[sqlx(flatten)]
    a: ACols,
    r_host: Option<String>,
    r_gen: Option<i64>,
    r_applied: Option<i64>,
    r_applied_at: Option<OffsetDateTime>,
    r_gen_at: Option<OffsetDateTime>,
    r_err: Option<String>,
    r_pmin: Option<i32>,
    r_pmax: Option<i32>,
    r_proto: Option<String>,
    tune_profile: Option<String>,
    tuned_at: Option<OffsetDateTime>,
    tune_params: Option<serde_json::Value>,
    r_unreach: Option<String>,
    r_reach_unknown: bool,
}

/// agent_state 里视图要的列（`A_COLS`，表别名 a）：完整加载与补指标共用
#[derive(sqlx::FromRow, Clone)]
struct ACols {
    a_version: Option<String>,
    a_last: Option<OffsetDateTime>,
    a_xray: Option<bool>,
    a_cores: Option<i32>,
    a_health: Option<sqlx::types::Json<fleet_proto::AgentHealth>>,
    a_singbox: Option<sqlx::types::Json<fleet_proto::SingboxStatus>>,
    a_skew: Option<i64>,
    a_nic: Option<sqlx::types::Json<Vec<String>>>,
}

const A_COLS: &str = "a.agent_version a_version, a.last_report_at a_last, (a.info->'xray'->>'running')::bool a_xray, (a.info->>'cpu_cores')::int a_cores,
                a.info->'agent_health' a_health, (a.info->>'clock_skew_ms')::bigint a_skew, a.info->'nic_ifaces' a_nic, a.info->'singbox' a_singbox";

impl ACols {
    fn agent(self, st: &AppState, now: OffsetDateTime) -> Agent {
        Agent {
            version: self.a_version,
            last_report_at: self.a_last,
            online: self.a_last.is_some_and(|t| st.report_gaps.online(t, now)),
            xray_running: self.a_xray,
            cores: self.a_cores,
            health: self.a_health.map(|h| h.0),
            clock_skew_ms: self.a_skew,
            nic_ifaces: self.a_nic.map(|n| n.0).unwrap_or_default(),
            anytls: self.a_singbox.map(|s| s.0),
        }
    }
}

/// agent 多久没上报算离线（中控收不到上报的时段不算，见 report_gaps.rs）
pub const AGENT_OFFLINE_S: i64 = 120;

/// 在用机器全量视图的短缓存：/machines、/relays、/overview、告警评估、实时指标共用，并发请求合并成一次查询（单飞）。
/// 资源变更事件（`AppState::emit`）让它作废、下次完整加载；agent 上报的指标不发事件，TTL 过了就在上一份上补指标（`refresh`），
/// 每 `FULL_EVERY`、跨天或小时汇总之后才再完整加载一次。
/// 重算正在进行（锁被占）或失败、超时时先回上一份（`LiveCache::stale`），见 `cached`。
struct Snap {
    /// 最近一次加载或补指标的时刻（TTL 从这里算）
    at: Instant,
    /// 到这个时刻再完整加载：上次完整加载 + `FULL_EVERY`，最晚到组织时区的下一个零点（流量缓存同时过期）
    full_until: Instant,
    gen: u64,
    rolled: u64,
    touched: u64,
    v: Arc<Vec<MachineView>>,
}
/// 指标（心跳、探测）最多旧这么久
const LIVE_TTL: std::time::Duration = std::time::Duration::from_secs(3);
/// 重算一次视图最多这么久，到点算失败（回上一份或报错）；排队等别人算完（等锁）另算一份同样长的时限。视图池每条语句 15 秒超时，
/// 但一次完整加载有几轮先后依赖的查询，叠起来会更久；告警评估（30 秒一轮）不能被它拖住
const VIEW_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
/// 上一份视图最多旧这么久还拿来顶：再旧就报错（告警评估这一轮失败、中控自检报 `central_view_stuck`），不拿很久以前的在线状态判告警
const STALE_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// 只补指标最多这么久，之后完整加载一次（与小时汇总同一节奏）：账单时区跨天的到期天数与流量周期、国旗画像在这时跟上。
/// 改机器的地方都发事件（含作业步骤、线路机连通性的结论），不靠它兜底
const FULL_EVERY: std::time::Duration = std::time::Duration::from_secs(300);
/// 入口 id → 今天 / 本月流量（组织时区，来自小时汇总表，含已移除的入口）
pub type TrafficMap = Arc<HashMap<Uuid, Traffic>>;
/// 机器 id → 按网卡计的今天 / 本月流量（组织时区，来自小时汇总表）
type NicMap = Arc<HashMap<Uuid, NicTraffic>>;
type Flows = (TrafficMap, NicMap);
/// 上一份算成的视图：(算成的时刻, gen, rolled, touched, 视图)
type LastView = (Instant, u64, u64, u64, Arc<Vec<MachineView>>);

#[derive(Default)]
pub struct LiveCache {
    /// 每次作废 +1；加载开始时记下，加载期间发生过变更的结果不当作新鲜的
    gen: AtomicU64,
    /// 作业推进、告警或静默变化 +1（`touched`）：下次取视图时补指标并重查作业与告警数，不等 TTL、不整份重算
    touched: AtomicU64,
    slot: tokio::sync::Mutex<Option<Snap>>,
    /// 最近一次算成的视图（同 `slot` 里的那份，另存一份不用拿锁就能读）：(算成的时刻, gen, rolled, touched, 视图)
    last: std::sync::Mutex<Option<LastView>>,
    /// 视图从什么时候起一直算不出来（失败或超时）、最近一次的错误；算成一次就清掉。中控自检据此报 `central_view_stuck`
    failing: std::sync::Mutex<Option<(OffsetDateTime, String)>>,
    /// `GET /machines` 的响应体，与它序列化自的那一份视图（同一份视图只序列化一次）
    json: std::sync::Mutex<Option<(Arc<Vec<MachineView>>, axum::body::Bytes)>>,
    /// 小时汇总每跑完一轮 +1
    rolled: AtomicU64,
    /// 流量（入口的、网卡的）只在小时汇总后（或跨天、跨月）才变：按汇总轮次缓存，到库里时钟的下一个组织时区零点过期
    traffic: tokio::sync::Mutex<Option<((u64, Instant), Flows)>>,
    /// 机器的周期已用（不含接入前读数）、接入前读数是否在周期内、周期起点时刻，同样只在汇总后作废
    quota: tokio::sync::Mutex<(u64, HashMap<QuotaKey, QuotaUsed>)>,
    /// 每日流量（traffic.rs）的按天聚合，键 (组织时区的今天, 天数)，同样只在汇总后作废
    pub(crate) daily: tokio::sync::Mutex<crate::traffic::Cache>,
    /// 流量异常（anomaly.rs）：小时汇总每轮之后算好放这里，完整加载时带进视图；中控启动后还没算过为 None
    anomaly: std::sync::Mutex<Option<crate::anomaly::Map>>,
    /// 持续握手丢包（tcp_loss.rs）：每 60 秒算好放这里，取视图时（含只补指标）带进去；中控启动后还没算过为 None
    tcp_loss: std::sync::Mutex<Option<crate::tcp_loss::Map>>,
}

/// (周期内下行之和, 上行之和, 接入前读数在周期内, 周期起点时刻, 近 7 天窗口内下行之和, 上行之和, 窗口起点时刻)；
/// 业务流量（没有网卡的机器）下载 + 上传合在下行里、上行为 0。计费量在 `quota_used` 出口按规则套（meta::billed），缓存里只存和
type QuotaUsed = (i64, i64, bool, OffsetDateTime, i64, i64, OffsetDateTime);
/// `quota_used` 查询的一行：机器 id + `QuotaUsed`
type QuotaRow = (Uuid, i64, i64, bool, OffsetDateTime, i64, i64, OffsetDateTime);

/// (机器, 周期起点, 周期时区, 网卡计量规则（None = 业务流量）, 接入前读数的录入时间)
type QuotaKey = (Uuid, Date, String, Option<&'static str>, Option<OffsetDateTime>);

/// 生效的网卡计量规则（订阅单独设置的，或供应商的）；不认识的按 max
pub(crate) fn nic_rule(r: Option<&str>) -> &'static str {
    r.and_then(|r| crate::meta::TRAFFIC_RULES.iter().find(|(k, _)| *k == r)).map_or(crate::meta::DEFAULT_TRAFFIC_RULE, |(k, _)| *k)
}

impl LiveCache {
    pub fn invalidate(&self) {
        self.gen.fetch_add(1, Ordering::SeqCst);
    }

    /// 作业推进、告警或静默变了：只影响进行中 / 失败的作业、未恢复的告警数、维护中，下次取视图时补上（见 `refresh`）
    pub fn touched(&self) {
        self.touched.fetch_add(1, Ordering::SeqCst);
    }

    /// 小时流量变了（汇总跑完、彻底删除机器）：流量缓存作废
    pub fn rolled_up(&self) {
        self.rolled.fetch_add(1, Ordering::SeqCst);
    }

    /// 小时汇总之后算好的流量异常（随后的 `rolled_up` 让视图完整加载、带上它）
    pub fn set_anomaly(&self, a: std::collections::HashMap<Uuid, crate::anomaly::Anomaly>) {
        *crate::lock(&self.anomaly) = Some(Arc::new(a));
    }

    /// 刚算好的持续握手丢包：下次补指标（最多 3 秒后）就带上，不用作废整份视图
    pub fn set_tcp_loss(&self, m: std::collections::HashMap<Uuid, crate::tcp_loss::Loss>) {
        *crate::lock(&self.tcp_loss) = Some(Arc::new(m));
    }

    /// 小时汇总跑完的轮数
    pub(crate) fn rolled(&self) -> u64 {
        self.rolled.load(Ordering::SeqCst)
    }

    /// 视图从什么时候起一直算不出来、最近一次的错误（`cached` 失败或超时时记下，算成一次就清掉）
    pub fn failing(&self) -> Option<(OffsetDateTime, String)> {
        self.failing.lock().unwrap().clone()
    }

    /// 最近一次算成的视图，不管多旧、之后变过没有（上报入库时比对探测结果、在线与 Xray 状态变没变，决定要不要唤醒告警评估）
    pub fn latest(&self) -> Option<Arc<Vec<MachineView>>> {
        self.last.lock().unwrap().as_ref().map(|l| l.4.clone())
    }

    fn settled(&self, at: Instant, gen: u64, rolled: u64, touched: u64, v: &Arc<Vec<MachineView>>) {
        *self.last.lock().unwrap() = Some((at, gen, rolled, touched, v.clone()));
        *self.failing.lock().unwrap() = None;
    }

    /// 上一份视图，旧不过 `STALE_MAX`、之后没有资源变更才给（变更之后还给变更前的，改完马上重取的会一直看到旧的）；
    /// `rolled`：还要求之后没有新的小时汇总（只差随上报变的指标）；
    /// `touched`：还要求之后没有作业推进、告警或静默的变化（开维护、静默之后马上重取的要看到它们；`cached` 回上一份顶的不要求，那里本就不等补指标）
    pub(crate) fn stale(&self, rolled: bool, touched: bool) -> Option<Arc<Vec<MachineView>>> {
        let last = self.last.lock().unwrap();
        let (at, gen, r, t, v) = last.as_ref()?;
        let unchanged = *gen == self.gen.load(Ordering::SeqCst)
            && (!rolled || *r == self.rolled.load(Ordering::SeqCst))
            && (!touched || *t == self.touched.load(Ordering::SeqCst));
        (at.elapsed() < STALE_MAX && unchanged).then(|| v.clone())
    }
}

/// 在用机器的完整视图，走 `LiveCache`
pub async fn live(st: &AppState) -> sqlx::Result<Arc<Vec<MachineView>>> {
    cached(st, LIVE_TTL, false).await
}

/// 同 `live`，但不复用短缓存：至少补一次指标（实时指标比对用，比对间隔与 TTL 同为 3 s，复用的话会隔一轮才拿到新数据）
pub async fn live_fresh(st: &AppState) -> sqlx::Result<Arc<Vec<MachineView>>> {
    cached(st, std::time::Duration::ZERO, false).await
}

/// 同 `live_fresh`，但一定是等到锁、补过一次指标之后的视图：不回锁被占时或失败后的上一份（上一份可能早于刚入库的那次上报）。
/// 被唤醒的告警评估用；等锁最多 `VIEW_WAIT`，超时报错（评估失败，下个 30 秒轮次接着评）
pub async fn live_synced(st: &AppState) -> sqlx::Result<Arc<Vec<MachineView>>> {
    cached(st, std::time::Duration::ZERO, true).await
}

/// 取视图：锁空着就自己算（或用缓存）；锁被占（别的请求正在重算）时，之后没有资源变更的话直接回上一份、不排队等它，
/// 有资源变更才等（改完马上重取的要看到改动）。等锁、重算各最多 `VIEW_WAIT`：前一个算得久，排在后面的拿到锁后照样有完整的时限
/// （多半直接用前一个刚算成的那份）。失败或超时回上一份（旧不过 `STALE_MAX`、之后没有资源变更），
/// 否则报错——超时报 `PoolTimedOut`、视图池的语句超时报 57014（接口都回 503，同等不到连接；前端查询会重试）。
/// `synced`：不回上一份——锁被占就等，失败或超时直接报错（见 `live_synced`）
async fn cached(st: &AppState, ttl: std::time::Duration, synced: bool) -> sqlx::Result<Arc<Vec<MachineView>>> {
    let c = &st.live;
    let got = match c.slot.try_lock() {
        Ok(slot) => tokio::time::timeout(VIEW_WAIT, fill(st, slot, ttl)).await,
        Err(_) => {
            if let Some(v) = c.stale(true, false).filter(|_| !synced) {
                return Ok(v);
            }
            match tokio::time::timeout(VIEW_WAIT, c.slot.lock()).await {
                Ok(slot) => tokio::time::timeout(VIEW_WAIT, fill(st, slot, ttl)).await,
                Err(e) => Err(e),
            }
        }
    };
    let e = match got {
        Ok(Ok(v)) => return Ok(v),
        Ok(Err(e)) => e,
        Err(_) => sqlx::Error::PoolTimedOut,
    };
    let msg = if matches!(e, sqlx::Error::PoolTimedOut) { format!("{} 秒内没算出来", VIEW_WAIT.as_secs()) } else { e.to_string() };
    c.failing.lock().unwrap().get_or_insert((OffsetDateTime::now_utc(), String::new())).1 = msg;
    match c.stale(false, false).filter(|_| !synced) {
        Some(v) => {
            tracing::warn!("机器视图没算出来，先用上一份：{e}");
            Ok(v)
        }
        // 上一份太旧或之后有过资源变更：报错让调用方重试，不拿变更前的顶
        None => Err(e),
    }
}

/// 拿着锁：缓存还新就直接用，否则补指标或完整加载；算成的记进 `last`、清掉失败标记
async fn fill(st: &AppState, mut slot: tokio::sync::MutexGuard<'_, Option<Snap>>, ttl: std::time::Duration) -> sqlx::Result<Arc<Vec<MachineView>>> {
    let c = &st.live;
    // 先记下再查：查询期间又有变更的，下次照样重查
    let (gen, rolled, touched) = (c.gen.load(Ordering::SeqCst), c.rolled.load(Ordering::SeqCst), c.touched.load(Ordering::SeqCst));
    if let Some(s) = slot.as_mut().filter(|s| s.gen == gen && s.rolled == rolled) {
        if s.touched == touched && s.at.elapsed() < ttl {
            return Ok(s.v.clone());
        }
        if Instant::now() < s.full_until {
            s.v = Arc::new(refresh(st, &s.v, s.touched != touched).await?);
            (s.at, s.touched) = (Instant::now(), touched);
            c.settled(s.at, gen, rolled, touched, &s.v);
            return Ok(s.v.clone());
        }
    }
    let v = Arc::new(load(st, Scope::Live, None).await?);
    let at = Instant::now();
    // 流量缓存（load 刚取过）到组织时区的下一个零点过期：跨天时今天的流量清零，完整加载一次
    let day_end = c.traffic.lock().await.as_ref().map(|((_, exp), _)| *exp);
    let full_until = day_end.map_or(at + FULL_EVERY, |e| e.min(at + FULL_EVERY));
    c.settled(at, gen, rolled, touched, &v);
    *slot = Some(Snap { at, full_until, gen, rolled, touched, v: v.clone() });
    Ok(v)
}

/// `GET /machines` 的响应体 `{"data": [...]}`：同一份缓存视图只序列化一次，接连、并发的请求直接回这份字节
pub async fn live_json(st: &AppState) -> sqlx::Result<axum::body::Bytes> {
    let v = live(st).await?;
    if let Some((of, b)) = &*st.live.json.lock().unwrap() {
        if Arc::ptr_eq(of, &v) {
            return Ok(b.clone());
        }
    }
    #[derive(Serialize)]
    struct Data<'a> {
        data: &'a [MachineView],
    }
    let b = axum::body::Bytes::from(serde_json::to_vec(&Data { data: &v }).expect("机器视图可序列化"));
    *st.live.json.lock().unwrap() = Some((v, b.clone()));
    Ok(b)
}

/// 在上一份视图上补随上报和时间变化的字段：agent 状态、主机读数与持续负载、探测——只查上次之后又上报过的那几台
/// （探测由线路机上报，补它上面的入口）；维护中每次都查；`jobs`（作业、告警或静默变过）时再补进行中 / 失败的作业与未恢复的告警数。
/// 最后按现在的时刻重判在线与健康。其余字段（名称、订阅、入口、流量、额度、策略、国旗）随完整加载更新
async fn refresh(st: &AppState, prev: &[MachineView], jobs: bool) -> sqlx::Result<Vec<MachineView>> {
    let db = &st.view_db;
    let hid = |m: &MachineView| m.host_machine.as_ref().map_or(m.id, |h| h.id);
    let mut last: HashMap<Uuid, Option<OffsetDateTime>> = HashMap::new();
    for m in prev {
        if let Some(a) = &m.agent {
            last.insert(hid(m), a.last_report_at);
        }
    }
    let (aids, lasts): (Vec<Uuid>, Vec<Option<OffsetDateTime>>) = last.into_iter().unzip();
    // 比的是库里的值而不是时刻：查询之后才提交、上报时刻却更早的那份照样认得出来
    #[derive(sqlx::FromRow)]
    struct ARow {
        machine_id: Uuid,
        #[sqlx(flatten)]
        a: ACols,
    }
    let changed: HashMap<Uuid, ACols> = sqlx::query_as::<_, ARow>(&format!(
        "select a.machine_id, {A_COLS} from unnest($1::uuid[], $2::timestamptz[]) p(id, last) join agent_state a on a.machine_id = p.id
         where a.last_report_at is distinct from p.last"
    ))
    .bind(&aids)
    .bind(&lasts)
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| (r.machine_id, r.a))
    .collect();
    let cids: Vec<Uuid> = changed.keys().copied().collect();
    // 探测由线路机上报；穿透入口的 TCP 是节点报的（节点 → 线路机），两边谁上报过都要补
    let fresh = |e: &EntryView| changed.contains_key(&e.relay.id) || (e.mode == crate::reverse::REVERSE && changed.contains_key(&e.node.id));
    let eids: Vec<Uuid> = prev.iter().flat_map(|m| &m.entries).filter(|e| fresh(e)).map(|e| e.id).collect::<HashSet<_>>().into_iter().collect();
    let ids: Vec<Uuid> = prev.iter().map(|m| m.id).collect();
    let jobs_q = async {
        if !jobs {
            return Ok(None);
        }
        let (j, i) = tokio::try_join!(jobs_of(db, &ids), incidents_of(db, &ids))?;
        Ok::<_, sqlx::Error>(Some((j, i)))
    };
    let (host, sustained, probes, silences, jobs) =
        tokio::try_join!(host_now(db, &cids), sustained_of(db, &cids), probes_of(db, &eids), crate::silence::Silences::load(db), jobs_q)?;

    let now = OffsetDateTime::now_utc();
    let loss = crate::lock(&st.live.tcp_loss).clone();
    let mut out = prev.to_vec();
    for m in &mut out {
        m.tcp_loss = crate::tcp_loss::of(loss.as_ref(), m.id);
        let h = hid(m);
        if let Some(a) = changed.get(&h) {
            if m.agent.is_some() {
                m.agent = Some(a.clone().agent(st, now));
            }
            m.host = host.get(&h).cloned();
            m.sustained = sustained.get(&h).copied().unwrap_or_default();
        } else if let Some(a) = &mut m.agent {
            a.online = a.last_report_at.is_some_and(|t| st.report_gaps.online(t, now));
        }
        m.maintenance = silences.maintenance(m.id) || m.host_machine.as_ref().is_some_and(|x| silences.maintenance(x.id));
        if let Some(((live_jobs, failed_jobs), inc)) = &jobs {
            m.active_job = live_jobs.get(&m.id).cloned();
            m.failed_job = failed_job(failed_jobs, m.id, &m.lifecycle);
            m.open_incidents = inc.get(&m.id).copied().unwrap_or(0);
        }
    }
    // 入口的 relay_xray_down 同完整加载：线路机报了 Xray 没在运行、且这份报告还新（不看中控收不到上报的时段）；relay_offline 跟线路机的在线状态
    let xray_down: HashMap<Uuid, (bool, bool)> = out
        .iter()
        .filter(|m| m.kind == "relay")
        .map(|m| {
            let down = m.agent.as_ref().is_some_and(|a| a.xray_running == Some(false) && a.last_report_at.is_some_and(|t| (now - t).whole_seconds() < AGENT_OFFLINE_S));
            (m.id, (down, !m.agent.as_ref().is_some_and(|a| a.online)))
        })
        .collect();
    for m in &mut out {
        for e in &mut m.entries {
            if fresh(e) {
                e.tcp = probes.get(&(e.id, "tcp".into())).cloned();
                e.egress = probes.get(&(e.id, "egress".into())).cloned();
            }
            if let Some(&(d, off)) = xray_down.get(&e.relay.id) {
                (e.relay_xray_down, e.relay_offline) = (d, off);
            }
            e.relay_maint = silences.maintenance(e.relay.id);
        }
        if m.egress_dynamic {
            m.egress_seen = seen_egress(&m.entries);
        }
        m.health = health::evaluate(m, now);
    }
    Ok(out)
}

/// 动态出口节点最近一次成功出口探测看到的 IP（各入口的最新出口探测里取最近成功的那条，detail 就是回显的 IP）
fn seen_egress(entries: &[EntryView]) -> Option<String> {
    entries.iter().filter_map(|e| e.egress.as_ref()).filter(|p| p.ok).max_by_key(|p| p.ts).and_then(|p| p.detail.clone())
}

/// 每台机器 15 分钟内最近两条样本 → 当前值与网卡速率：逐台走 (machine_id, ts desc) 索引取两条（带上界，预建的未来空分区被剪掉；
/// 上界与入库窗口一致放到 1 天，时钟偏快的 agent 的样本不丢）
async fn host_now(db: &sqlx::PgPool, hids: &[Uuid]) -> sqlx::Result<HashMap<Uuid, HostNow>> {
    #[derive(sqlx::FromRow)]
    struct H {
        machine_id: Uuid,
        rn: i64,
        ts: OffsetDateTime,
        cpu_pct: Option<f32>,
        load1: Option<f32>,
        mem_used: Option<i64>,
        mem_total: Option<i64>,
        disk_used: Option<i64>,
        disk_total: Option<i64>,
        net_rx_bytes: Option<i64>,
        net_tx_bytes: Option<i64>,
        tcp_conns: Option<i32>,
    }
    let hs = sqlx::query_as::<_, H>(
        "select x.* from unnest($1::uuid[]) mid cross join lateral (
            select machine_id, row_number() over (order by ts desc) rn, ts, cpu_pct, load1,
                   mem_used, mem_total, disk_used, disk_total, net_rx_bytes, net_tx_bytes, tcp_conns
            from host_metric where machine_id = mid and ts > now() - interval '15 minutes' and ts <= now() + interval '1 day'
            order by ts desc limit 2) x",
    )
    .bind(hids)
    .fetch_all(db)
    .await?;
    let mut host: HashMap<Uuid, HostNow> = HashMap::new();
    let mut prev: HashMap<Uuid, (OffsetDateTime, Option<i64>, Option<i64>)> = HashMap::new();
    for h in &hs {
        if h.rn == 2 {
            prev.insert(h.machine_id, (h.ts, h.net_rx_bytes, h.net_tx_bytes));
        }
    }
    for h in hs.into_iter().filter(|h| h.rn == 1) {
        let rate = |cur: Option<i64>, old: Option<i64>, dt: f64| match (cur, old) {
            (Some(c), Some(o)) if c >= o && dt > 0.0 => Some((c - o) as f64 * 8.0 / dt),
            _ => None,
        };
        let (rx, tx) = prev
            .get(&h.machine_id)
            .map(|(t, r, x)| {
                let dt = (h.ts - *t).as_seconds_f64();
                (rate(h.net_rx_bytes, *r, dt), rate(h.net_tx_bytes, *x, dt))
            })
            .unwrap_or((None, None));
        host.insert(
            h.machine_id,
            HostNow {
                ts: h.ts,
                cpu_pct: h.cpu_pct,
                load1: h.load1,
                mem_used: h.mem_used,
                mem_total: h.mem_total,
                disk_used: h.disk_used,
                disk_total: h.disk_total,
                net_rx_bps: rx,
                net_tx_bps: tx,
                tcp_conns: h.tcp_conns,
            },
        );
    }
    Ok(host)
}

/// 最近 5 分钟主机负载的最低值（持续高负载判定）
async fn sustained_of(db: &sqlx::PgPool, hids: &[Uuid]) -> sqlx::Result<HashMap<Uuid, Sustained>> {
    #[derive(sqlx::FromRow)]
    struct S {
        machine_id: Uuid,
        span_s: Option<i64>,
        cpu_min: Option<f32>,
        mem_min: Option<f32>,
        disk_min: Option<f32>,
    }
    let su = sqlx::query_as::<_, S>(
        "select machine_id, extract(epoch from max(ts) - min(ts))::bigint span_s, min(cpu_pct) cpu_min,
                (min(mem_used::float8 * 100 / nullif(mem_total, 0)))::real mem_min, (min(disk_used::float8 * 100 / nullif(disk_total, 0)))::real disk_min
         from host_metric where machine_id = any($1) and ts > now() - interval '5 minutes' and ts <= now() + interval '1 day' group by machine_id",
    )
    .bind(hids)
    .fetch_all(db)
    .await?;
    Ok(su.into_iter().map(|r| (r.machine_id, Sustained { span_s: r.span_s.unwrap_or(0), cpu_min: r.cpu_min, mem_min: r.mem_min, disk_min: r.disk_min })).collect())
}

/// 每个入口 × (tcp, egress) 沿 (entry_id, kind, ts desc) 索引各取最新 3 条（不把 15 分钟内的探测全读出来排序）：最新一条是读数，
/// 另两条只数连续次数（`Probe::streak`）；上界同主机样本，剪掉预建的未来分区。TCP 另带近 1 小时成功样本的延迟中位数（每入口约 120 行，同一索引，只为最新一条算）
async fn probes_of(db: &sqlx::PgPool, eids: &[Uuid]) -> sqlx::Result<HashMap<(Uuid, String), Probe>> {
    #[derive(sqlx::FromRow)]
    struct P {
        entry_id: Uuid,
        kind: String,
        ok: bool,
        latency_ms: Option<f32>,
        detail: Option<String>,
        ts: OffsetDateTime,
        rn: i64,
        median_ms: Option<f64>,
    }
    let mut ps = sqlx::query_as::<_, P>(
        "select x.*, m.median_ms from unnest($1::uuid[]) e(id) cross join (values ('tcp'), ('egress')) k(kind) cross join lateral (
            select entry_id, kind, ok, latency_ms, detail, ts, row_number() over (order by ts desc) rn from probe_result
            where entry_id = e.id and kind = k.kind and ts > now() - interval '15 minutes' and ts <= now() + interval '1 day'
            order by ts desc limit 3) x
         left join lateral (
            select percentile_cont(0.5) within group (order by latency_ms) median_ms from probe_result
            where x.rn = 1 and k.kind = 'tcp' and entry_id = e.id and kind = 'tcp' and ok and latency_ms is not null
              and ts > now() - interval '1 hour' and ts <= now() + interval '1 day') m on true",
    )
    .bind(eids)
    .fetch_all(db)
    .await?;
    ps.sort_by_key(|p| p.rn);
    let mut out: HashMap<(Uuid, String), Probe> = HashMap::new();
    for p in ps {
        match out.get_mut(&(p.entry_id, p.kind.clone())) {
            // 更早的一条：和最新一条一样、且中间没断过才接着数
            Some(cur) => {
                if cur.ok == p.ok && i64::from(cur.streak) + 1 == p.rn {
                    cur.streak += 1;
                }
            }
            None => {
                let probe = Probe { ok: p.ok, latency_ms: p.latency_ms, detail: p.detail, ts: p.ts, median_ms: p.median_ms.map(|m| m as f32), streak: 1 };
                out.insert((p.entry_id, p.kind), probe);
            }
        }
    }
    Ok(out)
}

/// 入口的连不上次数最多数这么多条（`Reach::fails` 封顶；界面写「20+」）
pub const REACH_SEEN: i32 = 20;

/// 在用入口各自端口上主探测来源（`src`，中控或探测点，见 probe_points::Effective）最近一次 TCP 检查，
/// 沿 (relay_id, probe_point_id, port, ts desc) 索引每个入口取最新 `REACH_SEEN` 条，数最前面连续失败了几次（封顶 `REACH_SEEN`，不往回扫到上一次成功）；检查早于入口创建的不算：
/// 现在端口不再复用（entries::create 从 relay.port_next 分），但 0050 之前已经复用过的端口、0050 按标签对不上回收站快照而推算偏低的端口，
/// 保留期（reach_days）里还留着旧入口的检查
async fn reach_of(db: &sqlx::PgPool, es: &[(Uuid, Uuid, i32)], src: &crate::probe_points::Effective) -> sqlx::Result<HashMap<Uuid, Reach>> {
    let cr = src.cond("rr");
    let rs = sqlx::query_as::<_, (Uuid, bool, OffsetDateTime, Option<String>)>(&format!(
        "select e.id, x.ok, x.ts, x.detail from unnest($1::uuid[], $2::uuid[], $3::int[]) e(id, relay, port)
         join entry en on en.id = e.id cross join lateral (
            select ok, ts, detail from relay_reach rr where rr.relay_id = e.relay and {cr} and rr.port = e.port and rr.ts >= en.created_at
            order by rr.ts desc limit {REACH_SEEN}) x
         order by x.ts desc",
    ))
    .bind(es.iter().map(|e| e.0).collect::<Vec<_>>())
    .bind(es.iter().map(|e| e.1).collect::<Vec<_>>())
    .bind(es.iter().map(|e| e.2).collect::<Vec<_>>())
    .fetch_all(db)
    .await?;
    // 每个入口按时间倒序：第一条是最新一次，其后接着失败的数进 fails，遇到成功就停
    let mut out: HashMap<Uuid, Reach> = HashMap::new();
    let mut counting: HashSet<Uuid> = HashSet::new();
    for (id, ok, ts, detail) in rs {
        match out.get_mut(&id) {
            None => {
                out.insert(id, Reach { ok, ts, detail, fails: i32::from(!ok) });
                if !ok {
                    counting.insert(id);
                }
            }
            Some(r) if counting.contains(&id) => {
                if ok {
                    counting.remove(&id);
                } else {
                    r.fails += 1;
                }
            }
            Some(_) => {}
        }
    }
    // 整台连不上（同告警：最近 3 轮、5 分钟内都失败，不论哪个端口）：这台上的入口都按连不上，次数至少 3，取最近一轮的时间与原因
    let mut relays: Vec<Uuid> = es.iter().map(|e| e.1).collect();
    relays.sort();
    relays.dedup();
    let down: Vec<(Uuid, OffsetDateTime, Option<String>)> = sqlx::query_as(&format!(
        "select u.id, x.ts, x.detail from unnest($1::uuid[]) u(id) cross join lateral (
            select max(ts) ts, (array_agg(detail order by ts desc))[1] detail, count(*) n, bool_or(ok) any_ok from (
                select ts, ok, detail from relay_reach rr where rr.relay_id = u.id and {cr} and rr.ts > now() - interval '5 minutes' order by ts desc limit 3) l) x
         where x.n = 3 and not x.any_ok",
    ))
    .bind(&relays)
    .fetch_all(db)
    .await?;
    for (relay, ts, detail) in down {
        for e in es.iter().filter(|e| e.1 == relay) {
            let fails = out.get(&e.0).map_or(0, |r| r.fails).max(crate::reach::FAILS);
            out.insert(e.0, Reach { ok: false, ts, detail: detail.clone(), fails });
        }
    }
    Ok(out)
}

/// 进行中的作业与失败的上架 / 退役作业，各按机器取最后建的：(进行中, 失败)
async fn jobs_of(db: &sqlx::PgPool, ids: &[Uuid]) -> sqlx::Result<(HashMap<Uuid, JobRef>, HashMap<Uuid, JobRef>)> {
    let jobs = sqlx::query_as::<_, (Uuid, Uuid, String, String)>(
        "select target_id, id, kind, state::text from job
         where target_id = any($1) and (state in ('queued','running','waiting','cancelling')
           or (state = 'failed' and kind ~ '^(provision_|retire$)'))
         order by created_at",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    // 按创建先后：同一台机器后建的覆盖先建的
    let (mut live_jobs, mut failed_jobs) = (HashMap::new(), HashMap::new());
    for (t, id, kind, state) in jobs {
        let m = if state == "failed" { &mut failed_jobs } else { &mut live_jobs };
        m.insert(t, JobRef { id, kind, state });
    }
    Ok((live_jobs, failed_jobs))
}

/// 上架失败：最近失败的上架作业；退役中：失败的退役作业（等重试）
fn failed_job(failed: &HashMap<Uuid, JobRef>, id: Uuid, lifecycle: &str) -> Option<JobRef> {
    failed
        .get(&id)
        .filter(|j| match lifecycle {
            "failed" => j.kind != "retire",
            "retiring" => j.kind == "retire",
            _ => false,
        })
        .cloned()
}

/// 各机器未恢复的告警数
async fn incidents_of(db: &sqlx::PgPool, ids: &[Uuid]) -> sqlx::Result<HashMap<Uuid, i64>> {
    let inc = sqlx::query_as::<_, (Uuid, i64)>("select subject_id, count(*) from incident where state = 'firing' and subject_id = any($1) group by 1")
        .bind(ids)
        .fetch_all(db)
        .await?;
    Ok(inc.into_iter().collect())
}

/// 所有入口今天 / 本月的流量，走 `LiveCache.traffic`
pub async fn traffic(st: &AppState) -> sqlx::Result<TrafficMap> {
    Ok(flows(st).await?.0)
}

/// 入口与机器网卡的今天 / 本月流量（组织时区），走 `LiveCache.traffic`：按汇总轮次缓存、到组织时区零点过期
async fn flows(st: &AppState) -> sqlx::Result<Flows> {
    let c = &st.live;
    let mut slot = c.traffic.lock().await;
    // 先记下轮次再查：查询期间汇总又跑完一轮的，下次照样重查
    let rolled = c.rolled.load(Ordering::SeqCst);
    let t0 = Instant::now();
    if let Some(((r, exp), f)) = &*slot {
        if *r == rolled && t0 < *exp {
            return Ok(f.clone());
        }
    }
    // 今天、本月的起点（组织时区零点）与离下一个零点还有多少秒（跨天看库里的时钟，“今天”也是按库里的 now() 算的）先单独算好再传进去：
    // org_tz() 是读 app_setting 的 SQL 函数，写进 FILTER / WHERE 会逐行求值（月底几万行要好几秒，期间整个视图缓存都在等）
    let (d0, m0, left): (OffsetDateTime, OffsetDateTime, f64) = sqlx::query_as(
        "select date_trunc('day', l) at time zone z, date_trunc('month', l) at time zone z, extract(epoch from date_trunc('day', l) + interval '1 day' - l)::float8
         from (select z, now() at time zone z l from (select org_tz() z) o) b",
    )
    .fetch_one(&st.view_db)
    .await?;
    let rows: Vec<(Uuid, i64, i64, i64, i64)> = sqlx::query_as(
        "select entry_id,
           least(coalesce(sum(up_bytes) filter (where hour >= $1), 0), 9e18)::bigint, least(coalesce(sum(down_bytes) filter (where hour >= $1), 0), 9e18)::bigint,
           least(coalesce(sum(up_bytes), 0), 9e18)::bigint, least(coalesce(sum(down_bytes), 0), 9e18)::bigint
         from entry_traffic_hourly where hour >= $2 group by entry_id",
    )
    .bind(d0)
    .bind(m0)
    .fetch_all(&st.view_db)
    .await?;
    let v: TrafficMap =
        Arc::new(rows.into_iter().map(|(id, tu, td, mu, md)| (id, Traffic { today_up: tu, today_down: td, month_up: mu, month_down: md })).collect());
    // 网卡：同样的今天 / 本月起点，按 (machine_id, hour) 小时表求和
    let rows: Vec<(Uuid, i64, i64, i64, i64)> = sqlx::query_as(
        "select machine_id,
           least(coalesce(sum(net_rx) filter (where hour >= $1), 0), 9e18)::bigint, least(coalesce(sum(net_tx) filter (where hour >= $1), 0), 9e18)::bigint,
           least(coalesce(sum(net_rx), 0), 9e18)::bigint, least(coalesce(sum(net_tx), 0), 9e18)::bigint
         from host_metric_hourly where hour >= $2 group by machine_id",
    )
    .bind(d0)
    .bind(m0)
    .fetch_all(&st.view_db)
    .await?;
    let n: NicMap =
        Arc::new(rows.into_iter().map(|(id, tr, tt, mr, mt)| (id, NicTraffic { today_rx: tr, today_tx: tt, month_rx: mr, month_tx: mt })).collect());
    *slot = Some(((rolled, t0 + std::time::Duration::from_secs_f64(left)), (v.clone(), n.clone())));
    Ok((v, n))
}

/// 这些机器各自周期里（有接入前读数的从录入时刻起）的计费量、接入前读数是否在周期内、周期起点时刻、近 7 天窗口内的计费量与窗口起点时刻，
/// 走 `LiveCache.quota`（缓存里是下行、上行之和），缺的才查
async fn quota_used(st: &AppState, keys: Vec<QuotaKey>) -> sqlx::Result<HashMap<Uuid, (i64, bool, OffsetDateTime, i64, OffsetDateTime)>> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let c = &st.live;
    let mut slot = c.quota.lock().await;
    let rolled = c.rolled.load(Ordering::SeqCst);
    if slot.0 != rolled {
        *slot = (rolled, HashMap::new());
    }
    let miss: Vec<&QuotaKey> = keys.iter().filter(|k| !slot.1.contains_key(*k)).collect();
    if !miss.is_empty() {
        // 小时表按 UTC 整点分桶：周期起点所在的那一桶整桶算进来（半小时时区如 Asia/Kolkata 多算起点前至多 30 分钟，
        // 不漏算周期开头）；整点时区（北京、洛杉矶）起点正好是桶边界，和今天 / 本月的算法一致。
        // 接入前读数的录入时刻在周期内的从录入时刻所在整点的下一桶算起：那一小时里录入之后的流量不计（至多差 1 小时）。
        // 网卡：逐台按 host_metric_hourly 主键区间求本周期入、出总量，再按规则取小 / 取大 / 相加 / 只取出站。
        // 业务流量（家宽代理、同机 IP 节点）：先展开「机器 → 入口」，再逐个入口按小时表主键求和
        // （子查询放在 OR 连接条件里时规划器会先连出全部小时行，实验室 64 台要 11 s；入口连接本身也不写 OR：
        // 分成 node_id / relay_id 两支各走自己的索引再 union，否则是 机器 × 入口 的嵌套循环）。
        // 按 numeric 求和、夹到 9e18 再转 bigint：一台机器的离谱读数不让 bigint 溢出、拖垮所有人的机器列表。
        // 周期起点时刻 s0 一并带回：首次上架晚于它的，视图标「X 起统计」（counted_from）
        // 近 7 天窗口：起点 w = 周期查询起点与「当前整点往前 7 天」中较晚的；窗口内用量与 w + 1 小时（第一个计入的整点）一并带回，
        // 日均 / 预计用满由 quota::forecast 算
        // SQL 只返回下行（rx）、上行（tx）的和（业务流量合在下行里），计费量在下面按规则套（meta::billed，和每日流量同一处）
        let sql =
            "with q as (select q.mid, q.rule, q.base_at >= s.s0 cal, s.s0, n.since, greatest(n.since, date_trunc('hour', now()) - interval '7 days') w
                        from unnest($1::uuid[], $2::date[], $3::text[], $4::text[], $5::timestamptz[]) q(mid, start, tz, rule, base_at)
                        cross join lateral (select q.start::timestamp at time zone q.tz s0) s
                        cross join lateral (select case when q.base_at >= s.s0 then date_trunc('hour', q.base_at) else s.s0 - interval '1 hour' end since) n),
             qe as (select q.mid, q.since, q.w, e.id eid from q join entry e on e.node_id = q.mid where q.rule is null
                    union
                    select q.mid, q.since, q.w, e.id from q join entry e on e.relay_id = q.mid where q.rule is null)
             select q.mid, least(t.rx, 9e18)::bigint, least(t.tx, 9e18)::bigint, coalesce(q.cal, false), q.s0,
                    least(t.rx7, 9e18)::bigint, least(t.tx7, 9e18)::bigint, q.w + interval '1 hour'
             from q cross join lateral (
               select coalesce(sum(h.net_rx), 0) rx, coalesce(sum(h.net_tx), 0) tx,
                      coalesce(sum(h.net_rx) filter (where h.hour > q.w), 0) rx7, coalesce(sum(h.net_tx) filter (where h.hour > q.w), 0) tx7
               from host_metric_hourly h
               where h.machine_id = q.mid and h.hour > q.since) t
             where q.rule is not null
             union all
             select q.mid, least(coalesce(sum(t.b), 0), 9e18)::bigint, 0::bigint, coalesce(q.cal, false), q.s0,
                    least(coalesce(sum(t.b7), 0), 9e18)::bigint, 0::bigint, q.w + interval '1 hour'
             from q left join qe on qe.mid = q.mid left join lateral (
               select sum(h.up_bytes::numeric + h.down_bytes) b, sum(h.up_bytes::numeric + h.down_bytes) filter (where h.hour > qe.w) b7
               from entry_traffic_hourly h where h.entry_id = qe.eid and h.hour > qe.since) t on true
             where q.rule is null
             group by q.mid, q.cal, q.s0, q.w";
        let rows: Vec<QuotaRow> = sqlx::query_as(sql)
        .bind(miss.iter().map(|k| k.0).collect::<Vec<_>>())
        .bind(miss.iter().map(|k| k.1).collect::<Vec<_>>())
        .bind(miss.iter().map(|k| k.2.clone()).collect::<Vec<_>>())
        .bind(miss.iter().map(|k| k.3).collect::<Vec<_>>())
        .bind(miss.iter().map(|k| k.4).collect::<Vec<_>>())
        .fetch_all(&st.view_db)
        .await?;
        let got: HashMap<Uuid, QuotaUsed> = rows.into_iter().map(|(m, rx, tx, cal, s0, rx7, tx7, w)| (m, (rx, tx, cal, s0, rx7, tx7, w))).collect();
        for k in miss {
            // 每个键都有一行（q 是 unnest 出来的，两支都按 q 聚合）；缺了是 SQL 的 bug：记下机器 id 并按 0 算，不让一台打断整个视图与实时推送
            let used = got.get(&k.0).copied().unwrap_or_else(|| {
                tracing::warn!(machine = %k.0, "流量额度查询没有返回这台机器的一行，按已用 0 算");
                (0, 0, false, k.1.midnight().assume_utc(), 0, 0, k.1.midnight().assume_utc())
            });
            slot.1.insert(k.clone(), used);
        }
    }
    // 套规则算计费量：有规则的按网卡的下行、上行，没有的（业务流量）就是下行里合在一起的那个数
    let bill = |rule: Option<&str>, rx: i64, tx: i64| rule.map_or(rx, |r| crate::meta::billed(r, rx, tx));
    Ok(keys
        .into_iter()
        .map(|k| {
            let (rx, tx, cal, s0, rx7, tx7, w) = slot.1[&k];
            (k.0, (bill(k.3, rx, tx), cal, s0, bill(k.3, rx7, tx7), w))
        })
        .collect())
}

pub async fn load(st: &AppState, scope: Scope, only: Option<Uuid>) -> sqlx::Result<Vec<MachineView>> {
    let db = &st.view_db;
    let lc = scope.cond();
    // 线路机客户入口的连通性看哪一路检查（主探测来源；离线时退回中控或标灰，见 probe_points/）
    let reach_src = crate::probe_points::effective(st, db).await?;
    // 同机 IP 节点没有自己的 agent：agent 状态取宿主机的
    let sql = format!(
        "select m.id, m.kind::text kind, m.team_id, t.name team_name, t.spare team_spare, m.spare_from, m.spare_since, m.number, m.label, host(m.primary_ip) ip, coalesce(hm.ssh_port, m.ssh_port) ssh_port,
                m.host_id, display_name(hm.kind::text, hm.label, ht.name, hm.number) host_name, host(m.bind_ip) bind_ip, host(m.egress_ip) egress_ip, m.egress_dynamic,
                p.id provider_id, p.name provider_name, p.code provider_code, m.plan_id, pl.name plan_name,
                pl.bandwidth_mbps plan_bw, pl.traffic_gb plan_traffic, m.service_ref, m.country, exists (select 1 from billing_exempt be where be.machine_id = m.id) billing_exempt, m.lifecycle::text lifecycle,
                m.lifecycle_reason, m.note, m.revision, m.created_at, m.activated_at, m.retired_at,
                s.price_cents s_price, s.currency s_currency, s.cycle::text s_cycle, s.due_date s_due, s.billing_tz s_tz,
                local_date(s.billing_tz) s_today, s.auto_renew s_auto, s.remind_days s_remind, s.traffic_gb s_traffic, s.traffic_reset_day s_reset,
                s.traffic_used_base s_base, s.traffic_used_base_at s_base_at, s.overrides s_overrides, sb.billing_day::int s_bday, s.traffic_rule s_rule, tp.traffic_rule t_rule,
                tp.price_cents t_price, tp.currency t_currency, tp.cycle::text t_cycle, tp.billing_tz t_tz, tp.traffic_gb t_traffic, tp.traffic_reset t_reset, tp.remind_days t_remind, local_date(tp.billing_tz) t_today,
                {A_COLS}, r.public_host r_host, r.generation r_gen, r.applied_generation r_applied, r.applied_at r_applied_at, r.generation_at r_gen_at,
                r.apply_error r_err, r.port_min r_pmin, r.port_max r_pmax, r.protocols r_proto, m.tune_profile, m.tuned_at, m.tune_params, case when m.kind = 'relay' then {unreach} end r_unreach,
                m.kind = 'relay' and {reach_unknown} r_reach_unknown
         from machine m
         left join team t on t.id = m.team_id
         left join machine hm on hm.id = m.host_id
         left join team ht on ht.id = hm.team_id
         left join plan pl on pl.id = m.plan_id
         left join provider p on p.id = coalesce(pl.provider_id, m.provider_id)
         left join subscription_eff s on s.machine_id = m.id
         left join subscription sb on sb.machine_id = m.id
         join sub_template tp on tp.machine_id = m.id
         left join agent_state a on a.machine_id = coalesce(m.host_id, m.id)
         left join relay r on r.machine_id = m.id
         where {lc} and ($1::uuid is null or m.id = $1)
         order by m.kind = 'relay' desc, t.sort_order nulls first, t.name, m.number, m.label, m.id",
        unreach = crate::reach::unreachable_sql("m.id", &reach_src),
        reach_unknown = crate::reach::unknown_sql("m.id", &reach_src)
    );
    let rows: Vec<MRow> = sqlx::query_as(&sql).bind(only).fetch_all(db).await?;
    if rows.is_empty() {
        return Ok(vec![]);
    }
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    // 主机读数按机器所在的那台算：同机 IP 节点取宿主机的
    let hids: Vec<Uuid> = rows.iter().map(|r| r.host_id.unwrap_or(r.id)).collect::<HashSet<_>>().into_iter().collect();
    let now = OffsetDateTime::now_utc();
    // 国旗：没手动指定国家的按机器地址取画像（先读画像缓存，没命中的现查并写回；尽力而为，查不到就是 null）
    let auto_ips: Vec<IpAddr> = rows.iter().filter(|r| r.country.is_none()).filter_map(|r| r.ip.parse().ok()).collect();
    let intel_q = async { Ok::<_, sqlx::Error>(if auto_ips.is_empty() { HashMap::new() } else { st.ipintel.enrich(db, &auto_ips, true).await }) };
    // 流量额度周期：有订阅的按账单时区里的今天和生效的重置日（单独设置的，或供应商按账单日 / 自然月）算周期起点，
    // 没有订阅的是模板时区（供应商的，供应商没填为组织时区）的本月；供应商按滚动 30 天、订阅没单独设置重置日的（生效重置日为 null）
    // 是账单时区的今天往前 29 天。键里含起点日期，滚动 30 天每天自动换一份缓存。
    // 有自己 agent 的机器按网卡计量（规则取订阅单独设置的，否则供应商的），家宽代理、同机 IP 节点按业务流量
    let mut modes: HashMap<Uuid, &'static str> = HashMap::new();
    let periods: Vec<QuotaKey> = rows
        .iter()
        .map(|r| {
            let (start, tz, mode) = match (r.s_today, r.s_reset, &r.s_tz) {
                (Some(t), Some(d), Some(tz)) => (period_start(t, d as u8), tz.clone(), "monthly"),
                _ if r.t_reset == "rolling30" => {
                    (r.s_today.unwrap_or(r.t_today) - time::Duration::days(29), r.s_tz.clone().unwrap_or_else(|| r.t_tz.clone()), "rolling30")
                }
                _ => (period_start(r.t_today, 1), r.t_tz.clone(), "monthly"),
            };
            modes.insert(r.id, mode);
            let rule = (r.kind != "proxy" && r.host_id.is_none()).then(|| nic_rule(Some(r.s_rule.as_deref().unwrap_or(&r.t_rule))));
            (r.id, start, tz, rule, r.s_base_at)
        })
        .collect();

    // 入口（作为节点或线路机）
    #[derive(sqlx::FromRow)]
    struct E {
        id: Uuid,
        relay_id: Uuid,
        relay_label: String,
        node_id: Uuid,
        node_label: Option<String>,
        node_team: Option<String>,
        node_number: Option<i32>,
        port: i32,
        lifecycle: String,
        applied: bool,
        apply_error: Option<String>,
        relay_xray_down: bool,
        relay_last: Option<OffsetDateTime>,
        mode: String,
    }
    // 下面这些查询互不依赖（探测要等入口列表），并发跑
    let entries_q = async {
        let es: Vec<E> = sqlx::query_as(
            "select e.id, e.relay_id, rm.label relay_label, e.node_id, nm.label node_label, t.name node_team, nm.number node_number, e.port,
                    e.lifecycle::text lifecycle, r.applied_generation >= e.changed_gen applied, e.apply_error,
                    coalesce((ra.info->'xray'->>'running')::bool = false and ra.last_report_at > now() - make_interval(secs => $2), false) relay_xray_down,
                    ra.last_report_at relay_last, e.mode
             from entry e join machine rm on rm.id = e.relay_id join relay r on r.machine_id = e.relay_id
             left join agent_state ra on ra.machine_id = e.relay_id
             join machine nm on nm.id = e.node_id left join team t on t.id = nm.team_id
             where e.lifecycle <> 'removed' and (e.node_id = any($1) or e.relay_id = any($1))
             order by rm.label, e.port",
        )
        .bind(&ids)
        .bind(AGENT_OFFLINE_S as f64)
        .fetch_all(db)
        .await?;
        let eids: Vec<Uuid> = es.iter().map(|e| e.id).collect();
        let live: Vec<(Uuid, Uuid, i32)> = es.iter().filter(|e| e.lifecycle == "active").map(|e| (e.id, e.relay_id, e.port)).collect();
        let (ps, rc) = tokio::try_join!(probes_of(db, &eids), reach_of(db, &live, &reach_src))?;
        Ok::<_, sqlx::Error>((es, ps, rc))
    };
    // 已移除入口本月的流量（换线路机后旧入口 removed，但本月它跑过的流量仍属于这个节点 / 线路机）：
    // 只查本月有流量的那些入口是否已移除、属于谁
    let traffic_q = async {
        let (tr, nic) = flows(st).await?;
        let with_traffic: Vec<Uuid> = tr.keys().copied().collect();
        let removed: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
            "select id, node_id, relay_id from entry where lifecycle = 'removed' and id = any($1) and (node_id = any($2) or relay_id = any($2))",
        )
        .bind(&with_traffic)
        .bind(&ids)
        .fetch_all(db)
        .await?;
        Ok::<_, sqlx::Error>((tr, nic, removed))
    };
    // 维护中：同告警的静默共用一份查询与匹配
    let sil_q = crate::silence::Silences::load(db);
    // 宿主机上未退役的同机 IP 节点
    let ipn_q = sqlx::query_as::<_, (Uuid, Uuid, String, bool, bool)>(
        "select m.host_id, m.id, display_name(m.kind::text, m.label, t.name, m.number), not coalesce(t.spare, false),
                coalesce(t.spare, false) and exists (select 1 from subscription_eff s where s.machine_id = m.id and s.auto_renew)
         from machine m left join team t on t.id = m.team_id
         where m.host_id = any($1) and m.lifecycle <> 'retired' order by t.sort_order nulls first, t.name, m.number, m.id",
    )
    .bind(&ids)
    .fetch_all(db);
    // 备用团队里在用的机器：还指向它（或它上面同机 IP 节点）的软路由出口（到期提醒里列出，让人先「替换」）
    let spare_ids: Vec<Uuid> = rows.iter().filter(|r| r.team_spare == Some(true) && r.lifecycle == "active").map(|r| r.id).collect();
    let exits_q = crate::routers::exits_using_with_ip_nodes(db, &spare_ids);
    // 团队里的节点有没有出口指着（no_router_exits）
    let pointed_q = crate::routers::pointed_nodes(db);
    let period_of: HashMap<Uuid, (Date, Option<&'static str>)> = periods.iter().map(|k| (k.0, (k.1, k.3))).collect();
    let (intel, host, (es, probes, reach), (traffic, nic, removed), (live_jobs, failed_jobs), inc, silences, sustained, policies, period_used, ipn, mut exits, (seen_teams, pointed)) = tokio::try_join!(
        intel_q,
        host_now(db, &hids),
        entries_q,
        traffic_q,
        jobs_of(db, &ids),
        incidents_of(db, &ids),
        sil_q,
        sustained_of(db, &hids),
        crate::policy::load_modes(db),
        quota_used(st, periods),
        ipn_q,
        exits_q,
        pointed_q
    )?;
    // 汇率：订阅的价格、月费折成 USD（fx.rs）
    let fx = crate::fx::current(db).await?;

    // 每条已移除入口的流量同时记到它的节点和线路机上；直出入口两端是同一台，只记一次
    let mut removed_by: HashMap<Uuid, Traffic> = HashMap::new();
    for (id, node, relay) in removed {
        let x = &traffic[&id];
        let ends: &[Uuid] = if node == relay { &[node] } else { &[node, relay] };
        for &m in ends {
            let t = removed_by.entry(m).or_default();
            t.today_up = t.today_up.saturating_add(x.today_up);
            t.today_down = t.today_down.saturating_add(x.today_down);
            t.month_up = t.month_up.saturating_add(x.month_up);
            t.month_down = t.month_down.saturating_add(x.month_down);
        }
    }
    let mut ip_nodes: HashMap<Uuid, Vec<Ref>> = HashMap::new();
    // 不在备用团队里的（含没有团队的）：宿主机进了备用也不闲置、不自动回收（同 spare::run_once 的 keeps_live）
    let mut live_ip_nodes: HashMap<Uuid, Vec<String>> = HashMap::new();
    // 在备用团队里、开着自动续费的：宿主机回收会连带它们，同样不自动回收
    let mut renewing_ip_nodes: HashMap<Uuid, Vec<String>> = HashMap::new();
    for (h, id, name, live, renewing) in ipn {
        if live {
            live_ip_nodes.entry(h).or_default().push(name.clone());
        }
        if renewing {
            renewing_ip_nodes.entry(h).or_default().push(name.clone());
        }
        ip_nodes.entry(h).or_default().push(Ref { id, name });
    }

    let mut entries_by: HashMap<Uuid, Vec<EntryView>> = HashMap::new();
    for e in es {
        let direct = e.node_id == e.relay_id;
        let v = EntryView {
            id: e.id,
            relay: Ref { id: e.relay_id, name: e.relay_label.clone() },
            node: Ref {
                id: e.node_id,
                name: if direct {
                    direct_name(&e.relay_label)
                } else {
                    display_name("vps", e.node_label.as_deref(), e.node_team.as_deref(), e.node_number)
                },
            },
            direct,
            port: e.port,
            lifecycle: e.lifecycle,
            applied: e.applied,
            apply_error: e.apply_error,
            relay_xray_down: e.relay_xray_down,
            relay_offline: !e.relay_last.is_some_and(|t| st.report_gaps.online(t, now)),
            relay_maint: silences.maintenance(e.relay_id),
            tcp: probes.get(&(e.id, "tcp".into())).cloned(),
            mode: e.mode,
            egress: probes.get(&(e.id, "egress".into())).cloned(),
            reach: reach.get(&e.id).cloned(),
            traffic: traffic.get(&e.id).cloned().unwrap_or_default(),
        };
        // 直出入口的节点和线路机是同一台：只列一次
        if !direct {
            entries_by.entry(e.node_id).or_default().push(v.clone());
        }
        entries_by.entry(e.relay_id).or_default().push(v);
    }

    let today = now.to_offset(meta::org_offset()).date();
    // 组织时区本月 1 号 0 点（同 `flows` 的本月起点）：首次上架晚于它的，本月流量标「X 起」
    let month0 = today.replace_day(1).expect("1 号").midnight().assume_offset(meta::org_offset());
    let anomaly = crate::lock(&st.live.anomaly).clone();
    let loss = crate::lock(&st.live.tcp_loss).clone();
    Ok(rows
        .into_iter()
        .map(|r| {
            let name = display_name(&r.kind, r.label.as_deref(), r.team_name.as_deref(), r.number);
            let hid = r.host_id.unwrap_or(r.id);
            let agent = (r.kind != "proxy").then(|| r.a.agent(st, now));
            let subscription = r.s_price.map(|price| {
                let due = r.s_due.unwrap_or(today);
                // 按订阅自己的账单时区算「今天」（取不到时退回组织时区）
                let due_days = (due - r.s_today.unwrap_or(today)).whole_days();
                let cycle = r.s_cycle.clone().unwrap_or_default();
                let months = i64::from(meta::cycle_months(&cycle).unwrap_or(1));
                let (auto_renew, remind_days) = (r.s_auto.unwrap_or(false), r.s_remind.unwrap_or(r.t_remind));
                let bday = r.s_bday.map_or(due.day(), |d| d.clamp(1, 31) as u8);
                let currency = r.s_currency.clone().unwrap_or_default();
                let monthly_cents = meta::monthly_cents(price, &cycle);
                let usd_per_unit = fx.usd_per_unit(&currency);
                Subscription {
                    price_cents: price,
                    monthly_usd_cents: usd_per_unit.map(|u| crate::fx::to_usd_cents(monthly_cents, u)),
                    price_usd_cents: usd_per_unit.map(|u| crate::fx::to_usd_cents(i64::from(price), u)),
                    usd_per_unit,
                    currency,
                    monthly_cents,
                    next_due: meta::add_months_on(due, months, bday).unwrap_or(due),
                    period_days: meta::add_months_on(due, -months, bday).map_or(1, |p| (due - p).whole_days().max(1)),
                    cycle,
                    due_date: due,
                    billing_day: i32::from(bday),
                    due_days,
                    due_state: meta::due_state(due_days, remind_days, auto_renew),
                    billing_tz: r.s_tz.clone().unwrap_or_default(),
                    auto_renew,
                    remind_days,
                    traffic_gb: r.s_traffic,
                    traffic_reset_day: r.s_reset,
                    traffic_rule: r.s_rule.clone().unwrap_or_else(|| r.t_rule.clone()),
                    overrides: r.s_overrides.clone().unwrap_or_default(),
                }
            });
            let sub_template = SubTemplate {
                price_cents: r.t_price,
                currency: r.t_currency.clone(),
                cycle: r.t_cycle.clone(),
                billing_tz: r.t_tz.clone(),
                traffic_gb: r.t_traffic,
                traffic_reset: r.t_reset.clone(),
                remind_days: r.t_remind,
                traffic_rule: r.t_rule.clone(),
            };
            let relay = r.r_host.clone().map(|h| RelayInfo {
                public_host: h,
                generation: r.r_gen.unwrap_or(0),
                applied_generation: r.r_applied.unwrap_or(0),
                applied_at: r.r_applied_at,
                generation_at: r.r_gen_at,
                apply_error: r.r_err.clone(),
                port_min: r.r_pmin.unwrap_or(0),
                port_max: r.r_pmax.unwrap_or(0),
                protocols: r.r_proto.clone().unwrap_or_else(|| crate::entries::BOTH.into()),
                unreachable: r.r_unreach.clone(),
                reach_unknown: r.r_reach_unknown,
            });
            let tune = (r.kind != "proxy" && r.host_id.is_none()).then(|| TuneInfo {
                current: r.tune_profile.as_deref() == Some(crate::jobs::tune::PROFILE),
                profile: r.tune_profile.clone(),
                at: r.tuned_at,
                params: r.tune_params.clone(),
            });
            let entries = entries_by.remove(&r.id).unwrap_or_default();
            let removed_traffic = removed_by.remove(&r.id).unwrap_or_default();
            let (start, rule) = period_of[&r.id];
            let mode = modes[&r.id];
            let (after, cal, s0, recent, w0) = period_used[&r.id];
            let base = r.s_base.filter(|_| cal);
            // 生效的流量上限与来源：订阅的（单独设置的或跟随套餐的），没有订阅按套餐的；都没有时同机 IP 节点是与宿主机共用
            let (limit_gb, limit_source) = match &subscription {
                Some(s) => (s.traffic_gb, if s.overrides.iter().any(|k| k == "traffic_gb") { "subscription" } else { "plan" }),
                None => (r.plan_traffic, "plan"),
            };
            let limit_gb = limit_gb.filter(|g| *g > 0);
            let limit_source = match limit_gb {
                Some(_) => limit_source,
                None if r.host_id.is_some() => "host",
                None => "none",
            };
            let period_end = (mode == "monthly").then(|| next_reset(start, r.s_reset.map_or(1, |d| d as u8)));
            // 日均与预计用满按整点算（取值随小时汇总才变，不在两次汇总之间每次刷新都抖）；窗口不早于首次上架
            let now_h = OffsetDateTime::from_unix_timestamp(now.unix_timestamp() / 3600 * 3600).unwrap_or(now);
            let window_start = r.activated_at.map_or(w0, |a| a.max(w0));
            let (daily_avg, forecast_full_at) = crate::quota::forecast(
                base.unwrap_or(0).saturating_add(after),
                limit_gb.map(|g| g as i64 * 1_000_000_000),
                recent,
                window_start,
                now_h,
                period_end.map(|e| s0 + time::Duration::days((e - start).whole_days())),
            );
            let quota = Quota {
                limit_gb,
                limit_source,
                used: base.unwrap_or(0).saturating_add(after),
                period_start: start,
                // 每月：订阅的生效重置日，没有订阅按 1 号
                period_end,
                mode,
                // 有接入前读数的，周期开头那段已由读数补上
                counted_from: r.activated_at.filter(|a| *a > s0 && base.is_none()),
                rule,
                base,
                base_at: r.s_base_at.filter(|_| cal),
                daily_avg,
                forecast_full_at,
                period_start_at: s0,
                window_start,
                reset_in_days: r.s_today.zip(r.s_reset).map(|(t, d)| (next_reset(start, d as u8) - t).whole_days()),
            };
            // 宿主机维护时它上面的同机 IP 节点一起维护（是同一台机器）
            let maintenance = silences.maintenance(r.id) || r.host_id.is_some_and(|h| silences.maintenance(h));
            let policy_blocked = r.kind != "relay" && policies.effective(r.id, r.team_id).0.mode == crate::policy::Mode::Blocked;
            let failed_job = failed_job(&failed_jobs, r.id, &r.lifecycle);
            let spare = r.team_spare.unwrap_or(false);
            let router_exits: Vec<String> = exits.remove(&r.id).unwrap_or_default().iter().map(|e| e.label()).collect();
            let live_ip_nodes = live_ip_nodes.remove(&r.id).filter(|_| spare && r.lifecycle == "active").unwrap_or_default();
            let renewing_ip_nodes = renewing_ip_nodes.remove(&r.id).filter(|_| spare && r.lifecycle == "active").unwrap_or_default();
            // 两层口径：还被出口指着、上面还有在用 IP 节点、上面有开着自动续费的备用 IP 节点、自己开着自动续费的备用不自动回收（spare::run_once），不给回收日、照常续费（recycle_held）；
            // 其中只有出口指着、有在用 IP 节点的才关系到客户，其余（只因自动续费被扣住的）仍是闲置备用（idle_spare：不告警、不计故障），只是照常续费
            let auto_renew = subscription.as_ref().is_some_and(|s| s.auto_renew);
            let in_spare = spare && r.lifecycle == "active";
            let recycle_held = in_spare && !(router_exits.is_empty() && live_ip_nodes.is_empty() && renewing_ip_nodes.is_empty() && !auto_renew);
            let idle_spare = in_spare && router_exits.is_empty() && live_ip_nodes.is_empty();
            let spare_unbilled = in_spare && !recycle_held;
            let no_router_exits = !spare && r.lifecycle == "active" && r.kind != "relay" && r.team_id.is_some_and(|t| seen_teams.contains(&t)) && !pointed.contains(&r.id);
            let recycle_on = subscription.as_ref().filter(|_| spare && r.lifecycle == "active" && !recycle_held).map(|s| crate::spare::recycle_on(s.due_date));
            let mut v = MachineView {
                id: r.id,
                kind: r.kind,
                name,
                team: r.team_id.map(|id| Ref { id, name: r.team_name.clone().unwrap_or_default() }),
                spare,
                spare_from: r.spare_from,
                spare_since: r.spare_since,
                recycle_on,
                recycle_held,
                idle_spare,
                spare_unbilled,
                live_ip_nodes,
                renewing_ip_nodes,
                router_exits,
                no_router_exits,
                number: r.number,
                label: r.label,
                country: r.country.clone().or_else(|| r.ip.parse::<IpAddr>().ok().and_then(|ip| intel.get(&ip)).and_then(|b| b.country.clone())),
                country_set: r.country,
                billing_exempt: r.billing_exempt,
                ip: r.ip,
                ssh_port: r.ssh_port,
                host_machine: r.host_id.map(|id| Ref { id, name: r.host_name.clone().unwrap_or_default() }),
                bind_ip: r.bind_ip,
                egress_ip: r.egress_ip,
                egress_dynamic: r.egress_dynamic,
                egress_seen: r.egress_dynamic.then(|| seen_egress(&entries)).flatten(),
                ip_nodes: ip_nodes.remove(&r.id).unwrap_or_default(),
                provider: r.provider_id.map(|id| Ref { id, name: r.provider_name.clone().unwrap_or_default() }),
                provider_code: r.provider_code,
                plan: r.plan_id.map(|id| PlanRef {
                    id,
                    name: r.plan_name.clone().unwrap_or_default(),
                    bandwidth_mbps: r.plan_bw,
                    traffic_gb: r.plan_traffic,
                }),
                service_ref: r.service_ref,
                lifecycle: r.lifecycle,
                lifecycle_reason: r.lifecycle_reason,
                note: r.note,
                revision: r.revision,
                created_at: r.created_at,
                activated_at: r.activated_at,
                retired_at: r.retired_at,
                subscription,
                sub_template,
                agent,
                host: host.get(&hid).cloned(),
                entries,
                removed_traffic,
                nic: rule.is_some().then(|| nic.get(&r.id).cloned().unwrap_or_default()),
                month_counted_from: r.activated_at.filter(|a| *a > month0),
                quota,
                health: Health::default(),
                active_job: live_jobs.get(&r.id).cloned(),
                failed_job,
                open_incidents: inc.get(&r.id).copied().unwrap_or(0),
                maintenance,
                relay,
                tune,
                sustained: sustained.get(&hid).copied().unwrap_or_default(),
                policy_blocked,
                anomaly: anomaly.as_ref().and_then(|a| a.get(&r.id).cloned()),
                tcp_loss: crate::tcp_loss::of(loss.as_ref(), r.id),
            };
            v.health = health::evaluate(&v, now);
            v
        })
        .collect())
}

/// 总览 KPI（`GET /overview`、实时指标推送）
pub async fn overview(st: &AppState) -> sqlx::Result<Value> {
    let ms = live(st).await?;
    let mut v = overview_of(&ms, &*traffic(st).await?);
    settling(st, &mut v);
    Ok(v)
}

/// 总览的 `settling`：中控刚启动或刚恢复收上报，健康计数还在变（见 `ReportGaps::settling`）
pub fn settling(st: &AppState, v: &mut Value) {
    v["settling"] = json!(st.report_gaps.settling(OffsetDateTime::now_utc()));
}

/// 总览里节点、线路机、闲置备用各一份的健康计数
#[derive(Default, Serialize)]
struct Bucket {
    total: i64,
    ok: i64,
    degraded: i64,
    down: i64,
    unknown: i64,
    maintenance: i64,
}

/// 总览 KPI，只统计 active 的机器；本月业务流量是全部入口（`traffic`）的合计，数据不完整时带起点（`traffic_month.counted_from`）
pub fn overview_of(ms: &[MachineView], traffic: &HashMap<Uuid, Traffic>) -> Value {
    let (mut nodes, mut relays) = (Bucket::default(), Bucket::default());
    let (mut provisioning, mut retiring, mut failed, mut spare, mut rx, mut tx) = (0, 0, 0, 0, 0f64, 0f64);
    // 闲置备用按同一套健康口径另数（spare_health）：备用池里有几台此刻能顶上（正常 / 降级），几台故障、不通——挑替换用的新节点时一眼看出，不用去查库
    let mut spare_health = Bucket::default();
    // 本月业务流量：每条入口的字节只在它的线路机上统计一次，全部入口直接相加不重复，含已移除的入口和月中退役、替换掉的机器。
    // 网卡流量不做合计：同一个字节在线路机和节点的网卡上各算一次
    let (up, down) = traffic.values().fold((0i64, 0i64), |(u, d), t| (u.saturating_add(t.month_up), d.saturating_add(t.month_down)));
    // 本月数据从哪一刻才有：在用线路机里有一台月初前就在用（month_counted_from 为 null）就是完整的，否则取其中最早的首次上架时刻
    let counted_from = ms
        .iter()
        .filter(|m| m.kind == "relay" && m.lifecycle == "active")
        .map(|m| m.month_counted_from)
        .collect::<Option<Vec<_>>>()
        .and_then(|v| v.into_iter().min());
    // 到期：按订阅的 due_state（同续费告警）分别计即将到期、已过期
    let (mut due_soon, mut overdue) = (0, 0);
    // 在线/离线按台机器、按 agent 连通（同前端 isQuiet）：agent 没有近期上报为离线，和健康无关。
    // 同机 IP 节点跟随宿主机的 agent（视图里的 agent 就是宿主机的）照常计入；家宽代理不需要 agent，不计，另数 no_agent
    let (mut online, mut offline, mut no_agent, mut ip_nodes) = (0, 0, 0, 0);
    // 月费按汇率折成 USD 合计（monthly_usd，见 fx.rs）
    // 不续费的备用机（spare_unbilled：等到期放生）月费合计不含，另数备用小计（monthly_usd_spare）；
    // 备用团队里还被软路由出口指着、上面有在用同机 IP 节点、开着自动续费的（recycle_held）照常续费，算进合计
    let mut usd = crate::fx::Total::default();
    let (mut usd_spare, mut spare_paid) = (crate::fx::Total::default(), 0);
    // 月费合计里的台数（paid_machines）：在用、不是不续费的备用机、有订阅的——正好是 monthly_usd 加起来的那些台。
    // 没有订阅的（家宽代理）和不续费的备用机不在里面，网页首页、月报、fleetctl status 的「在用 N 台」都读它，不各自去拼
    let mut paid_machines = 0;
    for m in ms {
        match m.lifecycle.as_str() {
            "provisioning" | "draft" => provisioning += 1,
            "retiring" => retiring += 1,
            // 上架失败：网页「有问题」里也算它
            "failed" => failed += 1,
            _ => {}
        }
        if m.lifecycle != "active" {
            continue;
        }
        // 闲置备用（没有出口指着、没有在用 IP 节点的，等到期放生的）：不进健康计数，也不进在线 / 离线（放生的机器常年离线，会让「agent 离线」永远不为 0），只数个 spare；
        // 实时速率照算（离线的本来不算）；费用另看 unbilled（不续费的备用机：闲置且没被扣住，月费合计不含，另列 monthly_usd_spare，见下）
        let idle = m.idle_spare;
        let unbilled = m.spare_unbilled;
        if idle {
            spare += 1;
        }
        let bucket = if idle {
            &mut spare_health
        } else if m.kind == "relay" {
            &mut relays
        } else {
            &mut nodes
        };
        {
            bucket.total += 1;
            *match m.health.status {
                Status::Ok => &mut bucket.ok,
                Status::Degraded => &mut bucket.degraded,
                Status::Down => &mut bucket.down,
                Status::Maintenance => &mut bucket.maintenance,
                _ => &mut bucket.unknown,
            } += 1;
        }
        if let Some(s) = &m.subscription {
            // 不续费的备用机（替换下来等到期丢弃的）不算要续的（同前端 nextDue 与机器页「即将到期」筛选）；还被出口指着 / 有在用 IP 节点 / 开着自动续费的备用机照常续费
            match s.due_state {
                "soon" if !unbilled => due_soon += 1,
                "overdue" if !unbilled => overdue += 1,
                _ => {}
            }
            let t = if unbilled {
                spare_paid += 1;
                &mut usd_spare
            } else {
                paid_machines += 1;
                &mut usd
            };
            t.add(&s.currency, s.monthly_cents, s.monthly_usd_cents.zip(s.usd_per_unit));
        }
        // 实时速率只算数据没过期的：agent 离线的机器，最后一份上报的速率不算（同前端 liveRate）。
        // 告警（哪怕严重）不影响：机器照常每 10 秒上报。同机 IP 节点的读数就是宿主机的：在线离线照常按节点计，速率不再算一遍
        let stale = m.agent.as_ref().is_some_and(|a| !a.online);
        // 闲置备用不计入 no_agent、ip_nodes（与 total / online / offline 同口径）
        match &m.agent {
            _ if idle => {}
            None => no_agent += 1,
            Some(_) if stale => offline += 1,
            Some(_) => online += 1,
        }
        if m.host_machine.is_some() && !idle {
            ip_nodes += 1;
        }
        if let Some(h) = m.host.as_ref().filter(|_| !stale && m.host_machine.is_none()) {
            rx += h.net_rx_bps.unwrap_or(0.0);
            tx += h.net_tx_bps.unwrap_or(0.0);
        }
    }
    let mut usd_spare = serde_json::to_value(usd_spare).expect("月费合计可序列化");
    usd_spare["count"] = json!(spare_paid);
    json!({
        "nodes": nodes, "relays": relays, "provisioning": provisioning, "retiring": retiring, "failed": failed, "due_soon": due_soon, "overdue": overdue, "online": online, "offline": offline,
        "no_agent": no_agent, "ip_nodes": ip_nodes, "spare": spare, "spare_health": spare_health, "monthly_usd": usd, "paid_machines": paid_machines, "monthly_usd_spare": usd_spare, "traffic_month": {"up": up, "down": down, "counted_from": counted_from.map(crate::meta::fmt_ts)}, "net_rx_bps": rx, "net_tx_bps": tx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::tests::{base, sub};
    use time::macros::date;

    #[test]
    fn reset_day_period_boundaries() {
        // 今天在重置日之后 / 当天 / 之前
        assert_eq!(period_start(date!(2026 - 10 - 20), 15), date!(2026 - 10 - 15));
        assert_eq!(period_start(date!(2026 - 10 - 15), 15), date!(2026 - 10 - 15));
        assert_eq!(period_start(date!(2026 - 10 - 14), 15), date!(2026 - 09 - 15));
        // 31 号：30 天的月份按 30 号，2 月按月末
        assert_eq!(period_start(date!(2026 - 09 - 30), 31), date!(2026 - 09 - 30));
        assert_eq!(period_start(date!(2026 - 09 - 29), 31), date!(2026 - 08 - 31));
        assert_eq!(period_start(date!(2026 - 10 - 30), 31), date!(2026 - 09 - 30));
        assert_eq!(period_start(date!(2028 - 03 - 01), 31), date!(2028 - 02 - 29));
        // 跨年
        assert_eq!(period_start(date!(2027 - 01 - 05), 10), date!(2026 - 12 - 10));
        assert_eq!(period_start(date!(2027 - 01 - 01), 1), date!(2027 - 01 - 01));
        // 下一次重置：下个月这一天（按月末截断、跨年）
        assert_eq!(next_reset(date!(2026 - 09 - 30), 31), date!(2026 - 10 - 31));
        assert_eq!(next_reset(date!(2026 - 08 - 31), 31), date!(2026 - 09 - 30));
        assert_eq!(next_reset(date!(2026 - 12 - 10), 10), date!(2027 - 01 - 10));
        assert_eq!(next_reset(date!(2028 - 01 - 31), 31), date!(2028 - 02 - 29));
    }

    #[test]
    fn overview_counts_alerting_machines_as_live() {
        let now = OffsetDateTime::now_utc();
        let host = |rx: f64| HostNow {
            ts: now,
            cpu_pct: None,
            load1: None,
            mem_used: None,
            mem_total: None,
            disk_used: None,
            disk_total: None,
            net_rx_bps: Some(rx),
            net_tx_bps: Some(rx / 2.0),
            tcp_conns: None,
        };
        // 已过期（严重告警，但续费不算故障；没有入口是降级）、照常上报：速率照算
        let mut down = base("vps");
        down.subscription = Some(sub(-3));
        down.host = Some(host(1000.0));
        down.health = health::evaluate(&down, now);
        assert_eq!(down.health.status, Status::Degraded);
        // agent 离线：最后一份速率不算
        let mut off = base("vps");
        off.agent.as_mut().unwrap().online = false;
        off.host = Some(host(5000.0));
        off.health = health::evaluate(&off, now);
        let ov = overview_of(&[down, off], &HashMap::new());
        assert_eq!(ov["net_rx_bps"], 1000.0);
        assert_eq!(ov["net_tx_bps"], 500.0);
        assert_eq!(ov["nodes"]["down"], 1, "只有 agent 离线的那台算故障");

        // 同机 IP 节点的主机读数就是宿主机的：速率只算一次；节点数、agent 在线数都照常各算一台（IP 节点随宿主机）
        let mut h = base("vps");
        h.host = Some(host(800.0));
        let mut x = base("vps");
        x.host = h.host.clone();
        x.host_machine = Some(Ref { id: h.id, name: "团队1-03".into() });
        let ov = overview_of(&[h, x], &HashMap::new());
        assert_eq!((ov["net_rx_bps"].as_f64(), ov["nodes"]["total"].as_i64(), ov["online"].as_i64(), ov["ip_nodes"].as_i64()), (Some(800.0), Some(2), Some(2), Some(1)));
    }

    #[test]
    fn overview_online_by_agent_not_health() {
        let now = OffsetDateTime::now_utc();
        // 严重告警（出口不通）但 agent 在线：在线
        let mut crit = base("vps");
        crit.entries = vec![crate::health::tests::entry(false, false)];
        crit.health = health::evaluate(&crit, now);
        assert_eq!(crit.health.status, Status::Down);
        let mut off = base("relay");
        off.agent.as_mut().unwrap().online = false;
        // 家宽代理没有 agent：不计
        let mut proxy = base("proxy");
        proxy.agent = None;
        let mut retired = base("vps");
        retired.lifecycle = "retired".into();
        retired.agent.as_mut().unwrap().online = false;
        let ov = overview_of(&[crit, off, proxy, retired], &HashMap::new());
        assert_eq!((ov["online"].as_i64(), ov["offline"].as_i64(), ov["no_agent"].as_i64()), (Some(1), Some(1), Some(1)));
    }

    #[test]
    fn overview_leaves_idle_spare_out_of_health_counts() {
        let now = OffsetDateTime::now_utc();
        let down = |spare: bool, exits: &[&str], online: bool| {
            let mut m = base("vps");
            m.entries = vec![crate::health::tests::entry(false, false)];
            m.spare = spare;
            m.router_exits = exits.iter().map(|s| s.to_string()).collect();
            m.agent.as_mut().unwrap().online = online;
            m.health = health::evaluate(&m, now);
            m.settle_spare()
        };
        // 闲置备用（故障、agent 离线）不计入；被出口指向的备用照常计入；普通节点照常
        let (idle, idle_off, used) = (down(true, &[], true), down(true, &[], false), down(true, &["办公室2 · 美国"], true));
        assert_eq!(idle.health.status, Status::Down, "健康状态本身照常算");
        let ov = overview_of(&[idle, idle_off, used, down(false, &[], true)], &HashMap::new());
        let n = &ov["nodes"];
        assert_eq!((n["total"].as_i64(), n["down"].as_i64(), ov["spare"].as_i64()), (Some(2), Some(2), Some(2)), "{ov}");
        assert_eq!((ov["online"].as_i64(), ov["offline"].as_i64()), (Some(2), Some(0)), "闲置备用不计在线 / 离线");
        // 闲置备用另有一份健康计数：备用池里有几台能顶上
        let sh = &ov["spare_health"];
        assert_eq!((sh["total"].as_i64(), sh["down"].as_i64(), sh["ok"].as_i64()), (Some(2), Some(2), Some(0)), "{ov}");
        let mut fine = base("vps");
        fine.spare = true;
        fine.health.status = Status::Ok;
        let fine = fine.settle_spare();
        assert_eq!(overview_of(&[fine], &HashMap::new())["spare_health"]["ok"], 1);

        // 闲置备用里的家宽代理（没 agent）、同机 IP 节点也不计入 no_agent / ip_nodes；被出口指向的、普通的照常计
        let mut idle_proxy = down(true, &[], true);
        idle_proxy.agent = None;
        let mut idle_ip = down(true, &[], true);
        idle_ip.host_machine = Some(Ref { id: Uuid::new_v4(), name: "团队1-03".into() });
        let mut used_proxy = down(true, &["办公室2 · 美国"], true);
        used_proxy.agent = None;
        let mut used_ip = down(true, &["办公室2 · 美国"], true);
        used_ip.host_machine = Some(Ref { id: Uuid::new_v4(), name: "团队1-03".into() });
        let ov = overview_of(&[idle_proxy, idle_ip, used_proxy, used_ip], &HashMap::new());
        assert_eq!((ov["no_agent"].as_i64(), ov["ip_nodes"].as_i64(), ov["spare"].as_i64()), (Some(1), Some(1), Some(2)), "{ov}");
    }

    #[test]
    fn overview_counts_due_state_and_monthly_cents() {
        let with = |due: i64, auto: bool| {
            let mut m = base("vps");
            m.subscription = Some(crate::health::tests::sub_with(due, auto));
            m
        };
        // 即将到期按提前提醒天数（7）算、自动续费的不算；已过期另计
        let mut far = with(10, false);
        let fs = far.subscription.as_mut().unwrap();
        (fs.monthly_cents, fs.monthly_usd_cents) = (33, Some(33));
        let ov = overview_of(&[with(3, false), with(3, true), with(-2, true), with(7, false), far], &HashMap::new());
        assert_eq!((ov["due_soon"].as_i64(), ov["overdue"].as_i64()), (Some(2), Some(1)));
        let ov1 = overview_of(&[with(-1, true)], &HashMap::new());
        assert_eq!(ov1["overdue"].as_i64(), Some(0), "自动续费的到期日次日还不算已过期");
        let (mut spare_soon, mut spare_over) = (with(3, false), with(-2, false));
        (spare_soon.spare, spare_over.spare) = (true, true);
        let ov2 = overview_of(&[spare_soon.settle_spare(), spare_over.settle_spare()], &HashMap::new());
        assert_eq!((ov2["due_soon"].as_i64(), ov2["overdue"].as_i64()), (Some(0), Some(0)), "备用团队的不算");
        assert_eq!((ov2["monthly_usd"]["cents"].as_i64(), ov2["monthly_usd_spare"]["cents"].as_i64(), ov2["monthly_usd_spare"]["count"].as_i64()), (Some(0), Some(200), Some(2)), "备用的月费不进合计，另列小计");
        assert_eq!(ov["monthly_usd"]["cents"], 433, "测试用的订阅是 USD，汇率 1");
        assert_eq!((ov["paid_machines"].as_i64(), ov2["paid_machines"].as_i64()), (Some(5), Some(0)), "付费台数 = 月费合计里的台数，备用团队的不算");
        // 没有订阅的（家宽代理）、备用团队里没订阅的同机 IP 节点都不在付费台数里
        let (mut proxy, mut spare_ip) = (base("proxy"), base("vps"));
        proxy.subscription = None;
        (spare_ip.subscription, spare_ip.spare) = (None, true);
        spare_ip.host_machine = Some(Ref { id: Uuid::new_v4(), name: "团队1-03".into() });
        let ov4 = overview_of(&[with(30, false), proxy, spare_ip.settle_spare()], &HashMap::new());
        assert_eq!(ov4["paid_machines"].as_i64(), Some(1));
        // 备用团队里还被出口指着、或上面有在用 IP 节点的（不闲置）照常续费：进即将到期 / 已过期、月费合计与付费台数，不进备用小计
        let (mut by_exit, mut by_ip, mut idle_soon) = (with(3, false), with(-2, false), with(3, false));
        (by_exit.spare, by_ip.spare, idle_soon.spare) = (true, true, true);
        by_exit.router_exits = vec!["办公室2 · 美国".into()];
        by_ip.live_ip_nodes = vec!["团队1-02".into()];
        // 开着自动续费的备用机也不是闲置：照常续费（自动续费到期日次日才算已过期，这里用 3 天后到期的，不进即将到期）
        let mut by_auto = with(3, true);
        by_auto.spare = true;
        let ov5 = overview_of(&[by_exit.settle_spare(), by_ip.settle_spare(), by_auto.settle_spare(), idle_soon.settle_spare()], &HashMap::new());
        assert_eq!((ov5["due_soon"].as_i64(), ov5["overdue"].as_i64(), ov5["paid_machines"].as_i64()), (Some(1), Some(1), Some(3)), "{ov5}");
        assert_eq!((ov5["monthly_usd"]["cents"].as_i64(), ov5["monthly_usd_spare"]["cents"].as_i64(), ov5["monthly_usd_spare"]["count"].as_i64()), (Some(300), Some(100), Some(1)), "{ov5}");
        // 只因开着自动续费被扣住的是闲置备用（客户侧）：不进健康计数、算进 spare；费用侧照常（上面的月费合计、付费台数已含它）
        assert_eq!((ov5["nodes"]["total"].as_i64(), ov5["spare"].as_i64()), (Some(2), Some(2)), "{ov5}");
        // 折算：逐台的 monthly_usd_cents 相加；没有汇率的单列，不算进合计
        let cny = |cents: i64, usd: Option<i64>| {
            let mut m = with(30, false);
            let s = m.subscription.as_mut().unwrap();
            (s.currency, s.monthly_cents, s.monthly_usd_cents, s.usd_per_unit) = ("CNY".into(), cents, usd, usd.map(|_| 0.14));
            m
        };
        let mut xyz = cny(500, None);
        xyz.subscription.as_mut().unwrap().currency = "XYZ".into();
        let ov3 = overview_of(&[with(30, false), cny(29999, Some(4200)), cny(100, Some(14)), xyz], &HashMap::new());
        assert_eq!(ov3["monthly_usd"], json!({"cents": 100 + 4200 + 14,
            "by_currency": [{"currency": "CNY", "cents": 30099, "usd_cents": 4214, "usd_per_unit": 0.14}, {"currency": "USD", "cents": 100, "usd_cents": 100, "usd_per_unit": 1.0}],
            "unconverted": [{"currency": "XYZ", "cents": 500}]}));
    }
}
