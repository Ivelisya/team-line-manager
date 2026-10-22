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
    Retired,
    All,
}

impl Scope {
    /// 对 `machine m` 的条件
    fn cond(self) -> &'static str {
        match self {
            Scope::Live => "m.lifecycle <> 'retired'",
            Scope::Retired => "m.lifecycle = 'retired'",
            Scope::All => "true",
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
    .fetch_one(&st.db)
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
    /// 单独设置了的字段（price_cents / currency / cycle / billing_tz / remind_days / traffic_gb / traffic_reset_day），其余跟随 `sub_template`
    pub overrides: Vec<String>,
    /// 折成每月的费用（分，四舍五入，meta::monthly_cents）：总览的月费合计、网页的月费列
    pub monthly_cents: i64,
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
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Traffic {
    pub today_up: i64,
    pub today_down: i64,
    pub month_up: i64,
    pub month_down: i64,
}

/// 流量额度周期：有订阅的按账单时区里最近一次（生效的）重置日起算，没有订阅的是模板时区（供应商的，没填为组织时区）的本月
#[derive(Debug, Clone, Serialize)]
pub struct Quota {
    /// 生效的流量上限（GB，十进制）；没有上限为 None。告警、网页、fleetctl 都读它，不各自再算
    pub limit_gb: Option<i32>,
    /// 上限从哪来：subscription = 订阅单独设置，plan = 跟随套餐，host = 同机 IP 节点与宿主机共用（没有自己的上限），none = 不限
    pub limit_source: &'static str,
    /// 周期内已用（字节）：有 agent 的机器按网卡流量、照供应商的计量规则算（宿主机含同机 IP 节点）；
    /// 家宽代理与同机 IP 节点没有自己的网卡读数，是业务流量（下载 + 上传，含已移除的入口）
    pub used: i64,
    pub period_start: Date,
    /// 网卡计量规则（min / max / sum / out，没有供应商按 max）；按业务流量算的为 None
    pub rule: Option<&'static str>,
    /// 本周期按供应商面板校准过：校准时面板上的已用（字节）与校准时间。校准不在当前周期内的不算，为 None
    pub base: Option<i64>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub base_at: Option<OffsetDateTime>,
    /// 有订阅时距下次重置还有几天（账单时区的日历天，≥ 1）；没有订阅为 None。流量额度告警的摘要用，不对外输出
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
    pub port: i32,
    pub lifecycle: String,
    /// 线路机已应用到这条入口的最近一次改动
    pub applied: bool,
    pub apply_error: Option<String>,
    /// 这条入口所在线路机的 Xray 已确认没在运行（节点侧据此抑制出口/TCP 告警）
    #[serde(skip)]
    pub relay_xray_down: bool,
    pub tcp: Option<Probe>,
    pub egress: Option<Probe>,
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
    /// 所在团队是备用团队：在用时订阅过期满 1 天由中控自动退役并彻底删除（见 spare.rs）。没有团队为 false
    pub spare: bool,
    pub number: Option<i32>,
    pub label: Option<String>,
    pub ip: String,
    /// 国家（ISO 3166-1 两位代码，如 "US"），名字旁显示国旗：手动指定的（`country_set`），没指定按地址查离线 IP 库
    /// （走画像缓存，同访问监控的来源画像）；内网地址、家宽代理填的域名、库里查不到的为 null
    pub country: Option<String>,
    /// 上架或修改时手动指定的国家；null = 按 IP 自动
    pub country_set: Option<String>,
    pub ssh_port: i32,
    /// 同机 IP 节点：它所在的宿主机（SSH、agent、主机读数都是宿主机的，`agent` / `host` 就是宿主机的）；其它机器为 null
    pub host_machine: Option<Ref>,
    /// 同机 IP 节点的公网 IP 是 1:1 NAT 映射的：它在宿主机网卡上的本地地址（ssserver 监听并从它出站）；公网 IP 就在网卡上的、其它机器为 null
    pub bind_ip: Option<String>,
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
    /// 流量额度：周期里已用（网卡按供应商规则，或业务流量）
    pub quota: Quota,
    pub health: Health,
    pub active_job: Option<JobRef>,
    /// 上架失败（lifecycle = failed）：最近一次失败的上架/恢复作业；退役中（retiring）：失败的退役作业。网页据此给出「查看作业」
    pub failed_job: Option<JobRef>,
    pub open_incidents: i64,
    pub maintenance: bool,
    /// 仅线路机
    pub relay: Option<RelayInfo>,
    #[serde(skip)]
    pub sustained: Sustained,
    /// 节点的有效访问策略是「暂停新连接」（健康判定里提示，不算故障）
    #[serde(skip)]
    pub policy_blocked: bool,
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
}

#[derive(sqlx::FromRow)]
struct MRow {
    id: Uuid,
    kind: String,
    team_id: Option<Uuid>,
    team_name: Option<String>,
    team_spare: Option<bool>,
    number: Option<i32>,
    label: Option<String>,
    ip: String,
    ssh_port: i32,
    host_id: Option<Uuid>,
    host_name: Option<String>,
    bind_ip: Option<String>,
    provider_id: Option<Uuid>,
    provider_name: Option<String>,
    provider_code: Option<String>,
    plan_id: Option<Uuid>,
    plan_name: Option<String>,
    plan_bw: Option<i32>,
    plan_traffic: Option<i32>,
    service_ref: Option<String>,
    country: Option<String>,
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
    t_price: Option<i32>,
    t_currency: String,
    t_cycle: String,
    t_tz: String,
    t_traffic: Option<i32>,
    t_reset: String,
    t_remind: i32,
    /// 模板时区里的「今天」（没有订阅时的流量周期）
    t_today: Date,
    p_rule: Option<String>,
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
}

/// agent_state 里视图要的列（`A_COLS`，表别名 a）：完整加载与补指标共用
#[derive(sqlx::FromRow, Clone)]
struct ACols {
    a_version: Option<String>,
    a_last: Option<OffsetDateTime>,
    a_xray: Option<bool>,
    a_cores: Option<i32>,
    a_health: Option<sqlx::types::Json<fleet_proto::AgentHealth>>,
    a_skew: Option<i64>,
}

const A_COLS: &str = "a.agent_version a_version, a.last_report_at a_last, (a.info->'xray'->>'running')::bool a_xray, (a.info->>'cpu_cores')::int a_cores,
                a.info->'agent_health' a_health, (a.info->>'clock_skew_ms')::bigint a_skew";

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
        }
    }
}

/// agent 多久没上报算离线（中控收不到上报的时段不算，见 `ReportGaps`）
pub const AGENT_OFFLINE_S: i64 = 120;

/// 中控收不到上报的时段：中控没在跑（进程启动时补记：库里最新的上报时刻到启动）；库连不上——agent 的请求因库不可用
/// （`db::unavailable`）处理不了，而且整整 `GAP_MIN_S` 秒一份上报都没入库（中途有入库的，只是个别请求失败，不算）。
/// 判 agent 离线时这些时段不算它的沉默：恢复后各 agent 退避中的重发（最长约 90 秒）要一会儿才到，按墙钟算会把一直在线的
/// 机器成批判成离线（开一批「agent 离线」严重告警再成批恢复），等摘除的退役、换线路机作业也会判线路机不在线而失败。
/// 停机之前就已经离线的机器照样离线：只扣掉中断的那一段，不是从恢复时重新计时。
/// 最近一小时的时段同时存进库里（app_setting 的 `report_gaps`）：中断恢复后不久又重启一次（连着两次部署、崩溃重启），
/// 新进程只看得到「库里最新的上报时刻」——那已经是恢复之后的了，前一段中断要从库里接着算，否则重发还没到的 agent 照样成批判离线
#[derive(Default)]
pub struct ReportGaps(std::sync::Mutex<Gaps>);

#[derive(Default)]
struct Gaps {
    /// 最近一次上报入库（进程启动也算）
    last_ok: Option<OffsetDateTime>,
    /// 这之后最近一次因库不可用失败
    last_fail: Option<OffsetDateTime>,
    /// 已经结束的中断时段 [起, 止]
    done: Vec<(OffsetDateTime, OffsetDateTime)>,
}

/// 连续这么久一份上报都没入库、其间又有因库不可用失败的，才算中控收不到上报
const GAP_MIN_S: i64 = 30;
/// 存中断时段的 app_setting 键：`[["起", "止"], …]`（RFC 3339）
const GAPS_KEY: &str = "report_gaps";

/// 加一段中断，和已有的重叠、相接的并成一段：同一段停机不会因为连着重启记两次而被重复扣掉
/// （一直没收到上报时，每次启动补记的都是「库里最新的上报时刻 → 启动」，起点相同）
fn add_gap(done: &mut Vec<(OffsetDateTime, OffsetDateTime)>, gap: (OffsetDateTime, OffsetDateTime)) {
    done.push(gap);
    done.sort();
    let mut merged: Vec<(OffsetDateTime, OffsetDateTime)> = Vec::with_capacity(done.len());
    for (from, to) in done.drain(..) {
        match merged.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => merged.push((from, to)),
        }
    }
    *done = merged;
}

impl ReportGaps {
    /// 进程启动时调：之前的进程记在库里的时段接着算，再补一段「上一个进程最后收到的上报（库里最新的上报时刻）→ 现在」，存回库里
    pub async fn started(&self, db: &sqlx::PgPool) -> sqlx::Result<()> {
        let last: Option<OffsetDateTime> = sqlx::query_scalar("select max(last_report_at) from agent_state").fetch_one(db).await?;
        let saved: Option<Value> = sqlx::query_scalar("select value from app_setting where key = $1").bind(GAPS_KEY).fetch_optional(db).await?;
        let now = OffsetDateTime::now_utc();
        {
            let mut g = self.0.lock().unwrap();
            let rfc = |v: &Value| OffsetDateTime::parse(v.as_str()?, &time::format_description::well_known::Rfc3339).ok();
            for p in saved.as_ref().and_then(Value::as_array).into_iter().flatten() {
                if let (Some(from), Some(to)) = (rfc(&p[0]), rfc(&p[1])) {
                    add_gap(&mut g.done, (from, to));
                }
            }
            if let Some(l) = last.filter(|l| *l < now) {
                add_gap(&mut g.done, (l, now));
            }
            g.done.retain(|(_, to)| (now - *to).whole_hours() < 1);
            g.last_ok = Some(now);
        }
        self.save(db).await
    }

    /// 把最近一小时的时段存进库里（启动时、库中断恢复后）
    pub async fn save(&self, db: &sqlx::PgPool) -> sqlx::Result<()> {
        let fmt = |t: OffsetDateTime| t.format(&time::format_description::well_known::Rfc3339).unwrap_or_default();
        let v: Vec<[String; 2]> = self.0.lock().unwrap().done.iter().map(|(f, t)| [fmt(*f), fmt(*t)]).collect();
        sqlx::query(
            "insert into app_setting (key, value, updated_by) values ($1, $2, 'fleet-server')
             on conflict (key) do update set value = excluded.value, updated_at = now(), updated_by = excluded.updated_by",
        )
        .bind(GAPS_KEY)
        .bind(json!(v))
        .execute(db)
        .await
        .map(|_| ())
    }

    /// 一份上报入库了：之前在失败的，从上次入库到现在算一段（够 `GAP_MIN_S` 的话）。记了新的一段返回 true（调用方存进库里）
    pub fn ok(&self, now: OffsetDateTime) -> bool {
        let mut g = self.0.lock().unwrap();
        let mut added = false;
        if let (Some(from), Some(_)) = (g.last_ok, g.last_fail.take()) {
            if (now - from).whole_seconds() >= GAP_MIN_S {
                add_gap(&mut g.done, (from, now));
                added = true;
            }
        }
        g.last_ok = Some(now);
        // 结束一小时以上的不再影响判定：之后一直没报的早就按自己的沉默算离线了
        g.done.retain(|(_, to)| (now - *to).whole_hours() < 1);
        added
    }

    /// agent 的请求因库不可用没能处理
    pub fn failed(&self, now: OffsetDateTime) { self.0.lock().unwrap().last_fail = Some(now); }

    /// 上次上报在 `last` 的 agent 到 `now` 算不算在线：沉默的时长扣掉其间中控收不到上报的时段（还没恢复的这一段算到最近一次失败）
    pub fn online(&self, last: OffsetDateTime, now: OffsetDateTime) -> bool {
        let g = self.0.lock().unwrap();
        let open = g.last_ok.zip(g.last_fail).filter(|(from, to)| (*to - *from).whole_seconds() >= GAP_MIN_S);
        let deaf: i64 = g.done.iter().copied().chain(open).filter(|(_, to)| *to > last).map(|(from, to)| (to - from.max(last)).whole_seconds()).sum();
        (now - last).whole_seconds() - deaf < AGENT_OFFLINE_S
    }
}

/// 在用机器全量视图的短缓存：/machines、/relays、/overview、告警评估、实时指标共用，并发请求合并成一次查询（单飞）。
/// 资源变更事件（`AppState::emit`）让它作废、下次完整加载；agent 上报的指标不发事件，TTL 过了就在上一份上补指标（`refresh`），
/// 每 `FULL_EVERY` 或小时汇总之后才再完整加载一次。
struct Snap {
    /// 最近一次加载或补指标的时刻（TTL 从这里算）
    at: Instant,
    /// 最近一次完整加载的时刻
    full_at: Instant,
    gen: u64,
    rolled: u64,
    touched: u64,
    v: Arc<Vec<MachineView>>,
}
/// 指标（心跳、探测）最多旧这么久
const LIVE_TTL: std::time::Duration = std::time::Duration::from_secs(3);
/// 只补指标最多这么久，之后完整加载一次：跨天的到期天数与流量周期、国旗画像、作业步骤里改了机器却没发事件的，都在这时跟上
const FULL_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
/// 入口 id → 今天 / 本月流量（组织时区，来自小时汇总表，含已移除的入口）
type TrafficMap = Arc<HashMap<Uuid, Traffic>>;

#[derive(Default)]
pub struct LiveCache {
    /// 每次作废 +1；加载开始时记下，加载期间发生过变更的结果不当作新鲜的
    gen: AtomicU64,
    /// 作业推进、告警或静默变化 +1（`touched`）：下次取视图时补指标并重查作业与告警数，不等 TTL、不整份重算
    touched: AtomicU64,
    slot: tokio::sync::Mutex<Option<Snap>>,
    /// `GET /machines` 的响应体，与它序列化自的那一份视图（同一份视图只序列化一次）
    json: std::sync::Mutex<Option<(Arc<Vec<MachineView>>, axum::body::Bytes)>>,
    /// 小时汇总每跑完一轮 +1
    rolled: AtomicU64,
    /// 流量只在小时汇总后（或跨天、跨月）才变：按汇总轮次缓存，到库里时钟的下一个组织时区零点过期
    traffic: tokio::sync::Mutex<Option<((u64, Instant), TrafficMap)>>,
    /// 机器的周期已用（不含校准值）与校准是否在周期内，同样只在汇总后作废
    quota: tokio::sync::Mutex<(u64, HashMap<QuotaKey, (i64, bool)>)>,
}

/// (机器, 周期起点, 周期时区, 网卡计量规则（None = 业务流量）, 校准时间)
type QuotaKey = (Uuid, Date, String, Option<&'static str>, Option<OffsetDateTime>);

/// 供应商的网卡计量规则；没有供应商按 max
fn nic_rule(r: Option<&str>) -> &'static str {
    match r {
        Some("min") => "min",
        Some("sum") => "sum",
        Some("out") => "out",
        _ => "max",
    }
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
}

/// 在用机器的完整视图，走 `LiveCache`
pub async fn live(st: &AppState) -> sqlx::Result<Arc<Vec<MachineView>>> {
    cached(st, LIVE_TTL).await
}

/// 同 `live`，但不复用短缓存：至少补一次指标（实时指标比对用，比对间隔与 TTL 同为 3 s，复用的话会隔一轮才拿到新数据）
pub async fn live_fresh(st: &AppState) -> sqlx::Result<Arc<Vec<MachineView>>> {
    cached(st, std::time::Duration::ZERO).await
}

async fn cached(st: &AppState, ttl: std::time::Duration) -> sqlx::Result<Arc<Vec<MachineView>>> {
    let c = &st.live;
    let mut slot = c.slot.lock().await;
    // 先记下再查：查询期间又有变更的，下次照样重查
    let (gen, rolled, touched) = (c.gen.load(Ordering::SeqCst), c.rolled.load(Ordering::SeqCst), c.touched.load(Ordering::SeqCst));
    if let Some(s) = slot.as_mut().filter(|s| s.gen == gen && s.rolled == rolled) {
        if s.touched == touched && s.at.elapsed() < ttl {
            return Ok(s.v.clone());
        }
        if s.full_at.elapsed() < FULL_EVERY {
            s.v = Arc::new(refresh(st, &s.v, s.touched != touched).await?);
            (s.at, s.touched) = (Instant::now(), touched);
            return Ok(s.v.clone());
        }
    }
    let v = Arc::new(load(st, Scope::Live, None).await?);
    let at = Instant::now();
    *slot = Some(Snap { at, full_at: at, gen, rolled, touched, v: v.clone() });
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
    let db = &st.db;
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
    let eids: Vec<Uuid> =
        prev.iter().flat_map(|m| &m.entries).filter(|e| changed.contains_key(&e.relay.id)).map(|e| e.id).collect::<HashSet<_>>().into_iter().collect();
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
    let mut out = prev.to_vec();
    for m in &mut out {
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
    // 入口的 relay_xray_down 同完整加载：线路机报了 Xray 没在运行、且这份报告还新（不看中控收不到上报的时段）
    let xray_down: HashMap<Uuid, bool> = out
        .iter()
        .filter(|m| m.kind == "relay")
        .map(|m| {
            let down = m.agent.as_ref().is_some_and(|a| a.xray_running == Some(false) && a.last_report_at.is_some_and(|t| (now - t).whole_seconds() < AGENT_OFFLINE_S));
            (m.id, down)
        })
        .collect();
    for m in &mut out {
        for e in &mut m.entries {
            if changed.contains_key(&e.relay.id) {
                e.tcp = probes.get(&(e.id, "tcp".into())).cloned();
                e.egress = probes.get(&(e.id, "egress".into())).cloned();
            }
            if let Some(d) = xray_down.get(&e.relay.id) {
                e.relay_xray_down = *d;
            }
        }
        m.health = health::evaluate(m, now);
    }
    Ok(out)
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
         from host_metric where machine_id = any($1) and ts > now() - interval '5 minutes' group by machine_id",
    )
    .bind(hids)
    .fetch_all(db)
    .await?;
    Ok(su.into_iter().map(|r| (r.machine_id, Sustained { span_s: r.span_s.unwrap_or(0), cpu_min: r.cpu_min, mem_min: r.mem_min, disk_min: r.disk_min })).collect())
}

/// 每个入口 × (tcp, egress) 沿 (entry_id, kind, ts desc) 索引各取最新一条（不把 15 分钟内的探测全读出来排序）；
/// 上界同主机样本，剪掉预建的未来分区
async fn probes_of(db: &sqlx::PgPool, eids: &[Uuid]) -> sqlx::Result<HashMap<(Uuid, String), Probe>> {
    #[derive(sqlx::FromRow)]
    struct P {
        entry_id: Uuid,
        kind: String,
        ok: bool,
        latency_ms: Option<f32>,
        detail: Option<String>,
        ts: OffsetDateTime,
    }
    let ps = sqlx::query_as::<_, P>(
        "select x.* from unnest($1::uuid[]) e(id) cross join (values ('tcp'), ('egress')) k(kind) cross join lateral (
            select entry_id, kind, ok, latency_ms, detail, ts from probe_result
            where entry_id = e.id and kind = k.kind and ts > now() - interval '15 minutes' and ts <= now() + interval '1 day'
            order by ts desc limit 1) x",
    )
    .bind(eids)
    .fetch_all(db)
    .await?;
    Ok(ps.into_iter().map(|p| ((p.entry_id, p.kind), Probe { ok: p.ok, latency_ms: p.latency_ms, detail: p.detail, ts: p.ts })).collect())
}

/// 进行中的作业与失败的上架 / 恢复 / 退役作业，各按机器取最后建的：(进行中, 失败)
async fn jobs_of(db: &sqlx::PgPool, ids: &[Uuid]) -> sqlx::Result<(HashMap<Uuid, JobRef>, HashMap<Uuid, JobRef>)> {
    let jobs = sqlx::query_as::<_, (Uuid, Uuid, String, String)>(
        "select target_id, id, kind, state::text from job
         where target_id = any($1) and (state in ('queued','running','waiting','cancelling')
           or (state = 'failed' and kind ~ '^((provision|restore)_|retire$)'))
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

/// 上架失败：最近失败的上架/恢复作业；退役中：失败的退役作业（等重试）
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
async fn traffic(st: &AppState) -> sqlx::Result<TrafficMap> {
    let c = &st.live;
    let mut slot = c.traffic.lock().await;
    // 先记下轮次再查：查询期间汇总又跑完一轮的，下次照样重查
    let rolled = c.rolled.load(Ordering::SeqCst);
    let t0 = Instant::now();
    if let Some(((r, exp), v)) = &*slot {
        if *r == rolled && t0 < *exp {
            return Ok(v.clone());
        }
    }
    // 跨天看库里的时钟（“今天”也是按库里的 now() 算的）：离下一个组织时区零点还有多少秒
    let left: f64 = sqlx::query_scalar(
        "select extract(epoch from date_trunc('day', now() at time zone org_tz()) + interval '1 day' - now() at time zone org_tz())::float8",
    )
    .fetch_one(&st.db)
    .await?;
    let rows: Vec<(Uuid, i64, i64, i64, i64)> = sqlx::query_as(
        "select entry_id,
           least(coalesce(sum(up_bytes) filter (where hour >= date_trunc('day', now() at time zone org_tz()) at time zone org_tz()), 0), 9e18)::bigint,
           least(coalesce(sum(down_bytes) filter (where hour >= date_trunc('day', now() at time zone org_tz()) at time zone org_tz()), 0), 9e18)::bigint,
           least(coalesce(sum(up_bytes), 0), 9e18)::bigint, least(coalesce(sum(down_bytes), 0), 9e18)::bigint
         from entry_traffic_hourly where hour >= date_trunc('month', now() at time zone org_tz()) at time zone org_tz()
         group by entry_id",
    )
    .fetch_all(&st.db)
    .await?;
    let v: TrafficMap =
        Arc::new(rows.into_iter().map(|(id, tu, td, mu, md)| (id, Traffic { today_up: tu, today_down: td, month_up: mu, month_down: md })).collect());
    *slot = Some(((rolled, t0 + std::time::Duration::from_secs_f64(left)), v.clone()));
    Ok(v)
}

/// 这些机器各自周期里（校准过的从校准时刻起）的用量与校准是否在周期内，走 `LiveCache.quota`，缺的才查
async fn quota_used(st: &AppState, keys: Vec<QuotaKey>) -> sqlx::Result<HashMap<Uuid, (i64, bool)>> {
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
        // 校准时刻在周期内的从校准时刻所在整点的下一桶算起：那一小时里校准之后的流量不计（至多差 1 小时）。
        // 网卡：逐台按 host_metric_hourly 主键区间求本周期入、出总量，再按规则取小 / 取大 / 相加 / 只取出站。
        // 业务流量（家宽代理、同机 IP 节点）：先展开「机器 → 入口」，再逐个入口按小时表主键求和
        // （子查询放在 OR 连接条件里时规划器会先连出全部小时行，实验室 64 台要 11 s）。
        // 按 numeric 求和、夹到 9e18 再转 bigint：一台机器的离谱读数不让 bigint 溢出、拖垮所有人的机器列表
        let rows: Vec<(Uuid, i64, bool)> = sqlx::query_as(
            "with q as (select q.mid, q.rule, q.base_at >= s.s0 cal,
                               case when q.base_at >= s.s0 then date_trunc('hour', q.base_at) else s.s0 - interval '1 hour' end since
                        from unnest($1::uuid[], $2::date[], $3::text[], $4::text[], $5::timestamptz[]) q(mid, start, tz, rule, base_at)
                        cross join lateral (select q.start::timestamp at time zone q.tz s0) s),
             qe as (select q.mid, q.since, e.id eid from q join entry e on e.node_id = q.mid or e.relay_id = q.mid where q.rule is null)
             select q.mid, least(case q.rule when 'min' then least(t.rx, t.tx) when 'max' then greatest(t.rx, t.tx) when 'sum' then t.rx + t.tx else t.tx end, 9e18)::bigint,
                    coalesce(q.cal, false)
             from q cross join lateral (
               select coalesce(sum(h.net_rx), 0) rx, coalesce(sum(h.net_tx), 0) tx from host_metric_hourly h
               where h.machine_id = q.mid and h.hour > q.since) t
             where q.rule is not null
             union all
             select q.mid, least(coalesce(sum(t.b), 0), 9e18)::bigint, coalesce(q.cal, false)
             from q left join qe on qe.mid = q.mid left join lateral (
               select sum(h.up_bytes::numeric + h.down_bytes) b from entry_traffic_hourly h where h.entry_id = qe.eid and h.hour > qe.since) t on true
             where q.rule is null
             group by q.mid, q.cal",
        )
        .bind(miss.iter().map(|k| k.0).collect::<Vec<_>>())
        .bind(miss.iter().map(|k| k.1).collect::<Vec<_>>())
        .bind(miss.iter().map(|k| k.2.clone()).collect::<Vec<_>>())
        .bind(miss.iter().map(|k| k.3).collect::<Vec<_>>())
        .bind(miss.iter().map(|k| k.4).collect::<Vec<_>>())
        .fetch_all(&st.db)
        .await?;
        let got: HashMap<Uuid, (i64, bool)> = rows.into_iter().map(|(m, b, cal)| (m, (b, cal))).collect();
        for k in miss {
            slot.1.insert(k.clone(), got.get(&k.0).copied().unwrap_or((0, false)));
        }
    }
    Ok(keys.into_iter().map(|k| (k.0, slot.1[&k])).collect())
}

pub async fn load(st: &AppState, scope: Scope, only: Option<Uuid>) -> sqlx::Result<Vec<MachineView>> {
    let db = &st.db;
    let lc = scope.cond();
    // 同机 IP 节点没有自己的 agent：agent 状态取宿主机的
    let sql = format!(
        "select m.id, m.kind::text kind, m.team_id, t.name team_name, t.spare team_spare, m.number, m.label, host(m.primary_ip) ip, coalesce(hm.ssh_port, m.ssh_port) ssh_port,
                m.host_id, display_name(hm.kind::text, hm.label, ht.name, hm.number) host_name, host(m.bind_ip) bind_ip,
                p.id provider_id, p.name provider_name, p.code provider_code, m.plan_id, pl.name plan_name,
                pl.bandwidth_mbps plan_bw, pl.traffic_gb plan_traffic, m.service_ref, m.country, m.lifecycle::text lifecycle,
                m.lifecycle_reason, m.note, m.revision, m.created_at, m.activated_at, m.retired_at,
                s.price_cents s_price, s.currency s_currency, s.cycle::text s_cycle, s.due_date s_due, s.billing_tz s_tz,
                local_date(s.billing_tz) s_today, s.auto_renew s_auto, s.remind_days s_remind, s.traffic_gb s_traffic, s.traffic_reset_day s_reset,
                s.traffic_used_base s_base, s.traffic_used_base_at s_base_at, s.overrides s_overrides, sb.billing_day::int s_bday, p.traffic_rule p_rule,
                tp.price_cents t_price, tp.currency t_currency, tp.cycle::text t_cycle, tp.billing_tz t_tz, tp.traffic_gb t_traffic, tp.traffic_reset t_reset, tp.remind_days t_remind, local_date(tp.billing_tz) t_today,
                {A_COLS}, r.public_host r_host, r.generation r_gen, r.applied_generation r_applied, r.applied_at r_applied_at, r.generation_at r_gen_at,
                r.apply_error r_err, r.port_min r_pmin, r.port_max r_pmax
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
         order by m.kind = 'relay' desc, t.sort_order nulls first, t.name, m.number, m.label, m.id"
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
    // 没有订阅的是模板时区（供应商的，供应商没填为组织时区）的本月。
    // 有自己 agent 的机器按网卡计量（规则取供应商的），家宽代理、同机 IP 节点按业务流量
    let periods: Vec<QuotaKey> = rows
        .iter()
        .map(|r| {
            let (start, tz) = match (r.s_today, r.s_reset, &r.s_tz) {
                (Some(t), Some(d), Some(tz)) => (period_start(t, d as u8), tz.clone()),
                _ => (period_start(r.t_today, 1), r.t_tz.clone()),
            };
            let rule = (r.kind != "proxy" && r.host_id.is_none()).then(|| nic_rule(r.p_rule.as_deref()));
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
    }
    // 下面这些查询互不依赖（探测要等入口列表），并发跑
    let entries_q = async {
        let es: Vec<E> = sqlx::query_as(
            "select e.id, e.relay_id, rm.label relay_label, e.node_id, nm.label node_label, t.name node_team, nm.number node_number, e.port,
                    e.lifecycle::text lifecycle, r.applied_generation >= e.changed_gen applied, e.apply_error,
                    coalesce((ra.info->'xray'->>'running')::bool = false and ra.last_report_at > now() - make_interval(secs => $2), false) relay_xray_down
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
        let ps = probes_of(db, &eids).await?;
        Ok::<_, sqlx::Error>((es, ps))
    };
    // 已移除入口本月的流量（换线路机后旧入口 removed，但本月它跑过的流量仍属于这个节点 / 线路机）：
    // 只查本月有流量的那些入口是否已移除、属于谁
    let traffic_q = async {
        let tr = traffic(st).await?;
        let with_traffic: Vec<Uuid> = tr.keys().copied().collect();
        let removed: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
            "select id, node_id, relay_id from entry where lifecycle = 'removed' and id = any($1) and (node_id = any($2) or relay_id = any($2))",
        )
        .bind(&with_traffic)
        .bind(&ids)
        .fetch_all(db)
        .await?;
        Ok::<_, sqlx::Error>((tr, removed))
    };
    // 维护中：同告警的静默共用一份查询与匹配
    let sil_q = crate::silence::Silences::load(db);
    // 宿主机上未退役的同机 IP 节点
    let ipn_q = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        "select m.host_id, m.id, display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id
         where m.host_id = any($1) and m.lifecycle <> 'retired' order by t.sort_order nulls first, t.name, m.number, m.id",
    )
    .bind(&ids)
    .fetch_all(db);
    let period_of: HashMap<Uuid, (Date, Option<&'static str>)> = periods.iter().map(|k| (k.0, (k.1, k.3))).collect();
    let (intel, host, (es, probes), (traffic, removed), (live_jobs, failed_jobs), inc, silences, sustained, policies, period_used, ipn) = tokio::try_join!(
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
        ipn_q
    )?;

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
    for (h, id, name) in ipn {
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
            tcp: probes.get(&(e.id, "tcp".into())).cloned(),
            egress: probes.get(&(e.id, "egress".into())).cloned(),
            traffic: traffic.get(&e.id).cloned().unwrap_or_default(),
        };
        // 直出入口的节点和线路机是同一台：只列一次
        if !direct {
            entries_by.entry(e.node_id).or_default().push(v.clone());
        }
        entries_by.entry(e.relay_id).or_default().push(v);
    }

    let today = now.to_offset(meta::org_offset()).date();
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
                Subscription {
                    price_cents: price,
                    currency: r.s_currency.clone().unwrap_or_default(),
                    monthly_cents: meta::monthly_cents(price, &cycle),
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
            });
            let entries = entries_by.remove(&r.id).unwrap_or_default();
            let removed_traffic = removed_by.remove(&r.id).unwrap_or_default();
            let (start, rule) = period_of[&r.id];
            let (after, cal) = period_used[&r.id];
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
            let quota = Quota {
                limit_gb,
                limit_source,
                used: base.unwrap_or(0).saturating_add(after),
                period_start: start,
                rule,
                base,
                base_at: r.s_base_at.filter(|_| cal),
                reset_in_days: r.s_today.zip(r.s_reset).map(|(t, d)| (next_reset(start, d as u8) - t).whole_days()),
            };
            // 宿主机维护时它上面的同机 IP 节点一起维护（是同一台机器）
            let maintenance = silences.maintenance(r.id) || r.host_id.is_some_and(|h| silences.maintenance(h));
            let policy_blocked = r.kind != "relay" && policies.effective(r.id, r.team_id).0.mode == crate::policy::Mode::Blocked;
            let failed_job = failed_job(&failed_jobs, r.id, &r.lifecycle);
            let mut v = MachineView {
                id: r.id,
                kind: r.kind,
                name,
                team: r.team_id.map(|id| Ref { id, name: r.team_name.clone().unwrap_or_default() }),
                spare: r.team_spare.unwrap_or(false),
                number: r.number,
                label: r.label,
                country: r.country.clone().or_else(|| r.ip.parse::<IpAddr>().ok().and_then(|ip| intel.get(&ip)).and_then(|b| b.country.clone())),
                country_set: r.country,
                ip: r.ip,
                ssh_port: r.ssh_port,
                host_machine: r.host_id.map(|id| Ref { id, name: r.host_name.clone().unwrap_or_default() }),
                bind_ip: r.bind_ip,
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
                quota,
                health: Health::default(),
                active_job: live_jobs.get(&r.id).cloned(),
                failed_job,
                open_incidents: inc.get(&r.id).copied().unwrap_or(0),
                maintenance,
                relay,
                sustained: sustained.get(&hid).copied().unwrap_or_default(),
                policy_blocked,
            };
            v.health = health::evaluate(&v, now);
            v
        })
        .collect())
}

/// 总览 KPI，只统计 active 的机器（实时指标推送也用它）
pub fn overview_of(ms: &[MachineView]) -> Value {
    let mut nodes = json!({"total": 0, "ok": 0, "degraded": 0, "down": 0, "unknown": 0, "maintenance": 0});
    let mut relays = nodes.clone();
    let (mut provisioning, mut retiring, mut failed, mut up, mut down, mut rx, mut tx) = (0, 0, 0, 0i64, 0i64, 0f64, 0f64);
    // 到期：按订阅的 due_state（同续费告警）分别计即将到期、已过期
    let (mut due_soon, mut overdue) = (0, 0);
    // 在线/离线按 agent 连通（同前端 isQuiet）：agent 没有近期上报为离线；家宽代理没有 agent，算在线。和健康无关
    let (mut online, mut offline) = (0, 0);
    // 月费按币种分别汇总（不做汇率折算）
    let mut cost: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
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
        let bucket = if m.kind == "relay" { &mut relays } else { &mut nodes };
        let key = match m.health.status {
            Status::Ok => "ok",
            Status::Degraded => "degraded",
            Status::Down => "down",
            Status::Maintenance => "maintenance",
            _ => "unknown",
        };
        bucket["total"] = json!(bucket["total"].as_i64().unwrap_or(0) + 1);
        bucket[key] = json!(bucket[key].as_i64().unwrap_or(0) + 1);
        if let Some(s) = &m.subscription {
            match s.due_state {
                "soon" => due_soon += 1,
                "overdue" => overdue += 1,
                _ => {}
            }
            // 月费：各台折好的 monthly_cents 按币种（大写）相加
            *cost.entry(s.currency.trim().to_ascii_uppercase()).or_default() += s.monthly_cents;
        }
        // 业务流量按节点端算（每条入口恰好属于一个节点）；线路机只算它的直出入口（节点就是它自己）
        if m.kind != "relay" {
            for e in &m.entries {
                up = up.saturating_add(e.traffic.month_up);
                down = down.saturating_add(e.traffic.month_down);
            }
            up = up.saturating_add(m.removed_traffic.month_up);
            down = down.saturating_add(m.removed_traffic.month_down);
        } else {
            for e in m.entries.iter().filter(|e| e.direct) {
                up = up.saturating_add(e.traffic.month_up);
                down = down.saturating_add(e.traffic.month_down);
            }
        }
        // 实时速率只算数据没过期的：agent 离线的机器，最后一份上报的速率不算（同前端 liveRate）。
        // 告警（哪怕严重）不影响：机器照常每 10 秒上报。同机 IP 节点的读数就是宿主机的：在线离线照常按节点计，速率不再算一遍
        let stale = m.agent.as_ref().is_some_and(|a| !a.online);
        if stale {
            offline += 1;
        } else {
            online += 1;
        }
        if let Some(h) = m.host.as_ref().filter(|_| !stale && m.host_machine.is_none()) {
            rx += h.net_rx_bps.unwrap_or(0.0);
            tx += h.net_tx_bps.unwrap_or(0.0);
        }
    }
    json!({
        "nodes": nodes, "relays": relays, "provisioning": provisioning, "retiring": retiring, "failed": failed, "due_soon": due_soon, "overdue": overdue, "online": online, "offline": offline,
        "monthly_cost": cost.into_iter().map(|(c, v)| json!({"currency": c, "cents": v})).collect::<Vec<_>>(), "traffic_month": {"up": up, "down": down}, "net_rx_bps": rx, "net_tx_bps": tx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::tests::{base, sub};
    use time::macros::date;

    /// 中控收不到上报的时段不算 agent 的沉默；停机之前就离线的照样离线；中途有入库的不算中控收不到
    #[test]
    fn report_gaps_do_not_count_as_silence() {
        let t0 = OffsetDateTime::now_utc();
        let s = |n: i64| t0 + time::Duration::seconds(n);
        // 中控停了 0–300 秒（进程启动时补记的那段）：停之前 20 秒报过的，恢复后 99 秒还算在线、101 秒算离线
        let g = ReportGaps::default();
        {
            let mut x = g.0.lock().unwrap();
            x.done.push((s(0), s(300)));
            x.last_ok = Some(s(300));
        }
        assert!(g.online(s(-20), s(399)) && !g.online(s(-20), s(401)));
        assert!(!g.online(s(-3600), s(301)), "停机之前一小时就不报了：照样离线");
        assert!(!ReportGaps::default().online(s(-20), s(399)), "没有中断：按墙钟算");
        // 库连不上：400 秒入库之后一直失败，还没恢复的这一段算到最近一次失败
        g.ok(s(400));
        g.failed(s(410));
        g.failed(s(560));
        assert!(g.online(s(395), s(565)));
        // 恢复：400–570 记成一段
        g.ok(s(570));
        assert!(g.online(s(395), s(640)) && !g.online(s(395), s(700)));
        // 中途有入库的（个别请求失败），不算中控收不到
        let h = ReportGaps::default();
        for (i, ok) in [(0, true), (5, false), (10, true), (15, false), (20, true), (40, false)] {
            if ok {
                h.ok(s(i));
            } else {
                h.failed(s(i));
            }
        }
        assert!(h.0.lock().unwrap().done.is_empty());
        assert!(!h.online(s(-120), s(41)));
    }

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
        // 已过期（严重告警，健康为故障）但照常上报：速率照算
        let mut down = base("vps");
        down.subscription = Some(sub(-3));
        down.host = Some(host(1000.0));
        down.health = health::evaluate(&down, now);
        assert_eq!(down.health.status, Status::Down);
        // agent 离线：最后一份速率不算
        let mut off = base("vps");
        off.agent.as_mut().unwrap().online = false;
        off.host = Some(host(5000.0));
        off.health = health::evaluate(&off, now);
        let ov = overview_of(&[down, off]);
        assert_eq!(ov["net_rx_bps"], 1000.0);
        assert_eq!(ov["net_tx_bps"], 500.0);
        assert_eq!(ov["nodes"]["down"], 2);

        // 同机 IP 节点的主机读数就是宿主机的：速率只算一次，节点数、在线数照常各算一台
        let mut h = base("vps");
        h.host = Some(host(800.0));
        let mut x = base("vps");
        x.host = h.host.clone();
        x.host_machine = Some(Ref { id: h.id, name: "团队1-03".into() });
        let ov = overview_of(&[h, x]);
        assert_eq!((ov["net_rx_bps"].as_f64(), ov["nodes"]["total"].as_i64(), ov["online"].as_i64()), (Some(800.0), Some(2), Some(2)));
    }

    #[test]
    fn overview_online_by_agent_not_health() {
        let now = OffsetDateTime::now_utc();
        // 严重告警（已过期）但 agent 在线：在线
        let mut crit = base("vps");
        crit.subscription = Some(sub(-3));
        crit.health = health::evaluate(&crit, now);
        assert_eq!(crit.health.status, Status::Down);
        let mut off = base("relay");
        off.agent.as_mut().unwrap().online = false;
        // 家宽代理没有 agent：在线
        let mut proxy = base("proxy");
        proxy.agent = None;
        let mut retired = base("vps");
        retired.lifecycle = "retired".into();
        retired.agent.as_mut().unwrap().online = false;
        let ov = overview_of(&[crit, off, proxy, retired]);
        assert_eq!((ov["online"].as_i64(), ov["offline"].as_i64()), (Some(2), Some(1)));
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
        far.subscription.as_mut().unwrap().monthly_cents = 33;
        let ov = overview_of(&[with(3, false), with(3, true), with(-1, true), with(7, false), far]);
        assert_eq!((ov["due_soon"].as_i64(), ov["overdue"].as_i64()), (Some(2), Some(1)));
        // 月费是各台 monthly_cents 之和
        assert_eq!(ov["monthly_cost"], json!([{"currency": "USD", "cents": 433}]));
    }
}
