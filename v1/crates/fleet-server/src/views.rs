//! 机器视图：一次把列表页、卡片、抽屉需要的东西组装好（约百台规模，分几条查询再在内存里拼）。

use crate::health::{self, Health, Status};
use crate::names::display_name;
use crate::state::AppState;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
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

#[derive(Debug, Clone, Serialize)]
pub struct Subscription {
    pub price_cents: i32,
    pub currency: String,
    pub cycle: String,
    pub due_date: Date,
    pub due_days: i64,
    pub billing_tz: String,
    pub auto_renew: bool,
    pub remind_days: i32,
    pub traffic_gb: Option<i32>,
    pub traffic_reset_day: Option<i32>,
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

/// 流量额度周期：有流量重置日的按账单时区里最近一次重置日起算，否则是北京时间的本月
#[derive(Debug, Clone, Serialize)]
pub struct Quota {
    /// 周期内业务流量（下载 + 上传，含已移除的入口）
    pub used: i64,
    pub period_start: Date,
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

#[derive(Debug, Clone, Serialize)]
pub struct EntryView {
    pub id: Uuid,
    pub relay: Ref,
    pub node: Ref,
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
    pub number: Option<i32>,
    pub label: Option<String>,
    pub ip: String,
    pub ssh_port: i32,
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
    pub agent: Option<Agent>,
    pub host: Option<HostNow>,
    /// 节点：它的入口；线路机：挂在它上面的入口
    pub entries: Vec<EntryView>,
    /// 已移除入口（如换线路机后的旧入口）本月 / 今天的流量：入口列表里没有它们，但这些流量仍属于这台机器
    pub removed_traffic: Traffic,
    /// 流量额度周期里已用的业务流量
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
    number: Option<i32>,
    label: Option<String>,
    ip: String,
    ssh_port: i32,
    provider_id: Option<Uuid>,
    provider_name: Option<String>,
    provider_code: Option<String>,
    plan_id: Option<Uuid>,
    plan_name: Option<String>,
    plan_bw: Option<i32>,
    plan_traffic: Option<i32>,
    service_ref: Option<String>,
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
    a_version: Option<String>,
    a_last: Option<OffsetDateTime>,
    a_xray: Option<bool>,
    a_cores: Option<i32>,
    r_host: Option<String>,
    r_gen: Option<i64>,
    r_applied: Option<i64>,
    r_applied_at: Option<OffsetDateTime>,
    r_gen_at: Option<OffsetDateTime>,
    r_err: Option<String>,
    r_pmin: Option<i32>,
    r_pmax: Option<i32>,
}

/// agent 多久没上报算离线
pub const AGENT_OFFLINE_S: i64 = 120;

/// 在用机器全量视图的短缓存：/machines、/relays、/overview、告警评估、实时指标共用，并发请求合并成一次查询（单飞）。
/// 资源变更事件（`AppState::emit`）让它立刻作废；agent 上报的指标不发事件，靠 TTL 过期。
type Cached = (Instant, u64, Arc<Vec<MachineView>>);
/// 指标（心跳、探测）最多旧这么久
const LIVE_TTL: std::time::Duration = std::time::Duration::from_secs(3);
/// 入口 id → 今天 / 本月流量（北京时间，来自小时汇总表，含已移除的入口）
type TrafficMap = Arc<HashMap<Uuid, Traffic>>;

#[derive(Default)]
pub struct LiveCache {
    /// 每次作废 +1；加载开始时记下，加载期间发生过变更的结果不当作新鲜的
    gen: AtomicU64,
    slot: tokio::sync::Mutex<Option<Cached>>,
    /// 小时汇总每跑完一轮 +1
    rolled: AtomicU64,
    /// 流量只在小时汇总后（或跨天、跨月）才变：按汇总轮次缓存，到库里时钟的下一个北京时间零点过期
    traffic: tokio::sync::Mutex<Option<((u64, Instant), TrafficMap)>>,
    /// 有重置日的机器的周期已用：(机器, 周期起点, 账单时区) → 字节，同样只在汇总后作废
    quota: tokio::sync::Mutex<(u64, HashMap<QuotaKey, i64>)>,
}

type QuotaKey = (Uuid, Date, String);

impl LiveCache {
    pub fn invalidate(&self) {
        self.gen.fetch_add(1, Ordering::SeqCst);
    }

    /// 小时流量变了（汇总跑完、彻底删除机器）：流量缓存作废
    pub fn rolled_up(&self) {
        self.rolled.fetch_add(1, Ordering::SeqCst);
    }
}

/// 在用机器的完整视图，走 `LiveCache`
pub async fn live(st: &AppState) -> sqlx::Result<Arc<Vec<MachineView>>> {
    let c = &st.live;
    let mut slot = c.slot.lock().await;
    let gen = c.gen.load(Ordering::SeqCst);
    if let Some((at, g, v)) = &*slot {
        if *g == gen && at.elapsed() < LIVE_TTL {
            return Ok(v.clone());
        }
    }
    let v = Arc::new(load(st, Scope::Live, None).await?);
    *slot = Some((Instant::now(), gen, v.clone()));
    Ok(v)
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
    // 跨天看库里的时钟（“今天”也是按库里的 now() 算的）：离下一个北京时间零点还有多少秒
    let left: f64 = sqlx::query_scalar(
        "select extract(epoch from date_trunc('day', now() at time zone 'Asia/Shanghai') + interval '1 day' - now() at time zone 'Asia/Shanghai')::float8",
    )
    .fetch_one(&st.db)
    .await?;
    let rows: Vec<(Uuid, i64, i64, i64, i64)> = sqlx::query_as(
        "select entry_id,
           coalesce(sum(up_bytes) filter (where hour >= date_trunc('day', now() at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai'), 0)::bigint,
           coalesce(sum(down_bytes) filter (where hour >= date_trunc('day', now() at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai'), 0)::bigint,
           coalesce(sum(up_bytes), 0)::bigint, coalesce(sum(down_bytes), 0)::bigint
         from entry_traffic_hourly where hour >= date_trunc('month', now() at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai'
         group by entry_id",
    )
    .fetch_all(&st.db)
    .await?;
    let v: TrafficMap =
        Arc::new(rows.into_iter().map(|(id, tu, td, mu, md)| (id, Traffic { today_up: tu, today_down: td, month_up: mu, month_down: md })).collect());
    *slot = Some(((rolled, t0 + std::time::Duration::from_secs_f64(left)), v.clone()));
    Ok(v)
}

/// 这些机器各自周期起（账单时区零点）的业务流量，走 `LiveCache.quota`，缺的才查
async fn quota_used(st: &AppState, keys: Vec<QuotaKey>) -> sqlx::Result<HashMap<Uuid, i64>> {
    let c = &st.live;
    let mut slot = c.quota.lock().await;
    let rolled = c.rolled.load(Ordering::SeqCst);
    if slot.0 != rolled {
        *slot = (rolled, HashMap::new());
    }
    let miss: Vec<&QuotaKey> = keys.iter().filter(|k| !slot.1.contains_key(*k)).collect();
    if !miss.is_empty() {
        // 小时表按 UTC 整点分桶：周期起点所在的那一桶整桶算进来（半小时时区如 Asia/Kolkata 多算起点前至多 30 分钟，
        // 不漏算周期开头）；整点时区（北京、洛杉矶）起点正好是桶边界，和今天 / 本月的算法一致
        let rows: Vec<(Uuid, i64)> = sqlx::query_as(
            "select q.mid, coalesce(sum(h.up_bytes + h.down_bytes), 0)::bigint
             from unnest($1::uuid[], $2::date[], $3::text[]) q(mid, start, tz)
             join entry e on e.node_id = q.mid or e.relay_id = q.mid
             join entry_traffic_hourly h on h.entry_id = e.id and h.hour > (q.start::timestamp at time zone q.tz) - interval '1 hour'
             group by q.mid",
        )
        .bind(miss.iter().map(|k| k.0).collect::<Vec<_>>())
        .bind(miss.iter().map(|k| k.1).collect::<Vec<_>>())
        .bind(miss.iter().map(|k| k.2.clone()).collect::<Vec<_>>())
        .fetch_all(&st.db)
        .await?;
        let got: HashMap<Uuid, i64> = rows.into_iter().collect();
        for k in miss {
            slot.1.insert(k.clone(), got.get(&k.0).copied().unwrap_or(0));
        }
    }
    Ok(keys.into_iter().map(|k| (k.0, slot.1[&k])).collect())
}

pub async fn load(st: &AppState, scope: Scope, only: Option<Uuid>) -> sqlx::Result<Vec<MachineView>> {
    let db = &st.db;
    let lc = match scope {
        Scope::Live => "m.lifecycle <> 'retired'",
        Scope::Retired => "m.lifecycle = 'retired'",
        Scope::All => "true",
    };
    let sql = format!(
        "select m.id, m.kind::text kind, m.team_id, t.name team_name, m.number, m.label, host(m.primary_ip) ip, m.ssh_port,
                m.provider_id, p.name provider_name, p.code provider_code, m.plan_id, pl.name plan_name,
                pl.bandwidth_mbps plan_bw, pl.traffic_gb plan_traffic, m.service_ref, m.lifecycle::text lifecycle,
                m.lifecycle_reason, m.note, m.revision, m.created_at, m.activated_at, m.retired_at,
                s.price_cents s_price, s.currency s_currency, s.cycle::text s_cycle, s.due_date s_due, s.billing_tz s_tz,
                local_date(s.billing_tz) s_today, s.auto_renew s_auto, s.remind_days s_remind, s.traffic_gb s_traffic, s.traffic_reset_day s_reset,
                a.agent_version a_version, a.last_report_at a_last, (a.info->'xray'->>'running')::bool a_xray, (a.info->>'cpu_cores')::int a_cores,
                r.public_host r_host, r.generation r_gen, r.applied_generation r_applied, r.applied_at r_applied_at, r.generation_at r_gen_at,
                r.apply_error r_err, r.port_min r_pmin, r.port_max r_pmax
         from machine m
         left join team t on t.id = m.team_id
         left join provider p on p.id = m.provider_id
         left join plan pl on pl.id = m.plan_id
         left join subscription s on s.machine_id = m.id
         left join agent_state a on a.machine_id = m.id
         left join relay r on r.machine_id = m.id
         where {lc} and ($1::uuid is null or m.id = $1)
         order by m.kind = 'relay' desc, t.sort_order nulls first, t.name, m.number, m.label"
    );
    let rows: Vec<MRow> = sqlx::query_as(&sql).bind(only).fetch_all(db).await?;
    if rows.is_empty() {
        return Ok(vec![]);
    }
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let now = OffsetDateTime::now_utc();
    // 有流量重置日的订阅：账单时区里的今天 → 周期起点
    let periods: Vec<QuotaKey> = rows
        .iter()
        .filter_map(|r| Some((r.id, period_start(r.s_today?, r.s_reset? as u8), r.s_tz.clone()?)))
        .collect();

    // 最近两条主机样本 → 当前值与网卡速率
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
    #[derive(sqlx::FromRow)]
    struct P {
        entry_id: Uuid,
        kind: String,
        ok: bool,
        latency_ms: Option<f32>,
        detail: Option<String>,
        ts: OffsetDateTime,
    }
    // 最近 5 分钟主机负载的最低值（持续高负载判定）
    #[derive(sqlx::FromRow)]
    struct S {
        machine_id: Uuid,
        span_s: Option<i64>,
        cpu_min: Option<f32>,
        mem_min: Option<f32>,
        disk_min: Option<f32>,
    }
    // 下面这些查询互不依赖（探测要等入口列表），并发跑
    // 每台机器 15 分钟内最近两条样本：逐台走 (machine_id, ts desc) 索引取两条
    let hs_q = sqlx::query_as::<_, H>(
        "select x.* from unnest($1::uuid[]) mid cross join lateral (
            select machine_id, row_number() over (order by ts desc) rn, ts, cpu_pct, load1,
                   mem_used, mem_total, disk_used, disk_total, net_rx_bytes, net_tx_bytes, tcp_conns
            from host_metric where machine_id = mid and ts > now() - interval '15 minutes' order by ts desc limit 2) x",
    )
    .bind(&ids)
    .fetch_all(db);
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
        let ps = sqlx::query_as::<_, P>(
            "select distinct on (entry_id, kind) entry_id, kind, ok, latency_ms, detail, ts from probe_result
             where entry_id = any($1) and kind in ('tcp', 'egress') and ts > now() - interval '15 minutes' order by entry_id, kind, ts desc",
        )
        .bind(&eids)
        .fetch_all(db)
        .await?;
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
    let jobs_q = sqlx::query_as::<_, (Uuid, Uuid, String, String)>(
        "select target_id, id, kind, state::text from job
         where target_id = any($1) and (state in ('queued','running','waiting','cancelling')
           or (state = 'failed' and kind ~ '^((provision|restore)_|retire$)'))
         order by created_at",
    )
    .bind(&ids)
    .fetch_all(db);
    let inc_q = sqlx::query_as::<_, (Uuid, i64)>("select subject_id, count(*) from incident where state = 'firing' and subject_id = any($1) group by 1")
        .bind(&ids)
        .fetch_all(db);
    // 维护静默：subject_id 为空 = 全局维护，所有机器都算维护中
    let maint_q = sqlx::query_as::<_, (Option<Uuid>,)>(
        "select distinct subject_id from silence where is_maintenance and deleted_at is null
         and now() between starts_at and ends_at and (subject_id is null or subject_id = any($1))",
    )
    .bind(&ids)
    .fetch_all(db);
    let su_q = sqlx::query_as::<_, S>(
        "select machine_id, extract(epoch from max(ts) - min(ts))::bigint span_s, min(cpu_pct) cpu_min,
                (min(mem_used::float8 * 100 / nullif(mem_total, 0)))::real mem_min, (min(disk_used::float8 * 100 / nullif(disk_total, 0)))::real disk_min
         from host_metric where machine_id = any($1) and ts > now() - interval '5 minutes' group by machine_id",
    )
    .bind(&ids)
    .fetch_all(db);
    let period_of: HashMap<Uuid, Date> = periods.iter().map(|k| (k.0, k.1)).collect();
    let (hs, (es, ps), (traffic, removed), jobs, inc, maint, su, policies, period_used) =
        tokio::try_join!(hs_q, entries_q, traffic_q, jobs_q, inc_q, maint_q, su_q, crate::policy::load_current(db), quota_used(st, periods))?;

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

    let mut probes: HashMap<(Uuid, String), Probe> = HashMap::new();
    for p in ps {
        probes.insert((p.entry_id, p.kind), Probe { ok: p.ok, latency_ms: p.latency_ms, detail: p.detail, ts: p.ts });
    }

    // 每条已移除入口的流量同时记到它的节点和线路机上（两端是不同的机器，不会在同一台上重复计）
    let mut removed_by: HashMap<Uuid, Traffic> = HashMap::new();
    for (id, node, relay) in removed {
        let x = &traffic[&id];
        for m in [node, relay] {
            let t = removed_by.entry(m).or_default();
            t.today_up += x.today_up;
            t.today_down += x.today_down;
            t.month_up += x.month_up;
            t.month_down += x.month_down;
        }
    }

    let mut entries_by: HashMap<Uuid, Vec<EntryView>> = HashMap::new();
    for e in es {
        let v = EntryView {
            id: e.id,
            relay: Ref { id: e.relay_id, name: e.relay_label.clone() },
            node: Ref {
                id: e.node_id,
                name: display_name("vps", e.node_label.as_deref(), e.node_team.as_deref(), e.node_number),
            },
            port: e.port,
            lifecycle: e.lifecycle,
            applied: e.applied,
            apply_error: e.apply_error,
            relay_xray_down: e.relay_xray_down,
            tcp: probes.get(&(e.id, "tcp".into())).cloned(),
            egress: probes.get(&(e.id, "egress".into())).cloned(),
            traffic: traffic.get(&e.id).cloned().unwrap_or_default(),
        };
        entries_by.entry(e.node_id).or_default().push(v.clone());
        entries_by.entry(e.relay_id).or_default().push(v);
    }

    // 按创建先后：同一台机器后建的覆盖先建的
    let (mut live_jobs, mut failed_jobs) = (HashMap::new(), HashMap::new());
    for (t, id, kind, state) in jobs {
        let m = if state == "failed" { &mut failed_jobs } else { &mut live_jobs };
        m.insert(t, JobRef { id, kind, state });
    }
    let inc: HashMap<Uuid, i64> = inc.into_iter().collect();
    let maint_all = maint.iter().any(|(s,)| s.is_none());
    let maint: std::collections::HashSet<Uuid> = maint.into_iter().filter_map(|(id,)| id).collect();
    let sustained: HashMap<Uuid, Sustained> =
        su.into_iter().map(|r| (r.machine_id, Sustained { span_s: r.span_s.unwrap_or(0), cpu_min: r.cpu_min, mem_min: r.mem_min, disk_min: r.disk_min })).collect();

    let today = now.to_offset(time::macros::offset!(+8)).date();
    Ok(rows
        .into_iter()
        .map(|r| {
            let name = display_name(&r.kind, r.label.as_deref(), r.team_name.as_deref(), r.number);
            let agent = (r.kind != "proxy").then(|| Agent {
                version: r.a_version.clone(),
                last_report_at: r.a_last,
                online: r.a_last.is_some_and(|t| (now - t).whole_seconds() < AGENT_OFFLINE_S),
                xray_running: r.a_xray,
                cores: r.a_cores,
            });
            let subscription = r.s_price.map(|price| Subscription {
                price_cents: price,
                currency: r.s_currency.clone().unwrap_or_default(),
                cycle: r.s_cycle.clone().unwrap_or_default(),
                due_date: r.s_due.unwrap_or(today),
                // 按订阅自己的账单时区算「今天」（取不到时退回北京时间）
                due_days: (r.s_due.unwrap_or(today) - r.s_today.unwrap_or(today)).whole_days(),
                billing_tz: r.s_tz.clone().unwrap_or_default(),
                auto_renew: r.s_auto.unwrap_or(false),
                remind_days: r.s_remind.unwrap_or(7),
                traffic_gb: r.s_traffic,
                traffic_reset_day: r.s_reset,
            });
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
            let quota = match period_of.get(&r.id) {
                Some(&start) => Quota { used: period_used[&r.id], period_start: start },
                None => Quota {
                    used: entries.iter().map(|e| e.traffic.month_up + e.traffic.month_down).sum::<i64>()
                        + removed_traffic.month_up
                        + removed_traffic.month_down,
                    period_start: today.replace_day(1).expect("1 号"),
                },
            };
            let maintenance = maint_all || maint.contains(&r.id);
            let policy_blocked = r.kind != "relay" && policies.effective(r.id, r.team_id).0.mode == crate::policy::Mode::Blocked;
            // 上架失败：最近失败的上架/恢复作业；退役中：失败的退役作业（等重试）
            let failed_job = failed_jobs
                .get(&r.id)
                .filter(|j: &&JobRef| match r.lifecycle.as_str() {
                    "failed" => j.kind != "retire",
                    "retiring" => j.kind == "retire",
                    _ => false,
                })
                .cloned();
            let mut v = MachineView {
                id: r.id,
                kind: r.kind,
                name,
                team: r.team_id.map(|id| Ref { id, name: r.team_name.clone().unwrap_or_default() }),
                number: r.number,
                label: r.label,
                ip: r.ip,
                ssh_port: r.ssh_port,
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
                agent,
                host: host.remove(&r.id),
                entries,
                removed_traffic,
                quota,
                health: Health::default(),
                active_job: live_jobs.get(&r.id).cloned(),
                failed_job,
                open_incidents: inc.get(&r.id).copied().unwrap_or(0),
                maintenance,
                relay,
                sustained: sustained.get(&r.id).copied().unwrap_or_default(),
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
    let (mut provisioning, mut retiring, mut failed, mut due_7d, mut up, mut down, mut rx, mut tx) = (0, 0, 0, 0, 0i64, 0i64, 0f64, 0f64);
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
            // 需要续费的：同告警规则，自动续费的只在已过期时算
            if s.due_days <= 7 && (s.due_days < 0 || !s.auto_renew) {
                due_7d += 1;
            }
            // 折成月费
            let months = match s.cycle.as_str() {
                "quarterly" => 3,
                "semiannual" => 6,
                "yearly" => 12,
                _ => 1,
            };
            *cost.entry(s.currency.clone()).or_default() += s.price_cents as i64 / months;
        }
        if m.kind != "relay" {
            for e in &m.entries {
                up += e.traffic.month_up;
                down += e.traffic.month_down;
            }
            up += m.removed_traffic.month_up;
            down += m.removed_traffic.month_down;
        }
        // 实时速率只算数据没过期的：agent 离线的机器，最后一份上报的速率不算（同前端 liveRate）。
        // 告警（哪怕严重）不影响：机器照常每 10 秒上报
        let stale = m.agent.as_ref().is_some_and(|a| !a.online);
        if stale {
            offline += 1;
        } else {
            online += 1;
        }
        if let Some(h) = m.host.as_ref().filter(|_| !stale) {
            rx += h.net_rx_bps.unwrap_or(0.0);
            tx += h.net_tx_bps.unwrap_or(0.0);
        }
    }
    json!({
        "nodes": nodes, "relays": relays, "provisioning": provisioning, "retiring": retiring, "failed": failed, "due_7d": due_7d, "online": online, "offline": offline,
        "monthly_cost": cost.into_iter().map(|(c, v)| json!({"currency": c, "cents": v})).collect::<Vec<_>>(), "traffic_month": {"up": up, "down": down}, "net_rx_bps": rx, "net_tx_bps": tx,
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
    fn due_7d_skips_auto_renew_until_overdue() {
        let with = |due: i64, auto: bool| {
            let mut m = base("vps");
            m.subscription = Some(Subscription { auto_renew: auto, ..sub(due) });
            m
        };
        let ov = overview_of(&[with(3, false), with(3, true), with(-1, true), with(10, false)]);
        assert_eq!(ov["due_7d"], 2);
    }
}
