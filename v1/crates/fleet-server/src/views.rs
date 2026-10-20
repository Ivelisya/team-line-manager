//! 机器视图：一次把列表页、卡片、抽屉需要的东西组装好（约百台规模，分几条查询再在内存里拼）。

use crate::health::{self, Health};
use crate::names::display_name;
use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 未退役（含上架中、下线中）
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
    pub health: Health,
    pub active_job: Option<JobRef>,
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

pub async fn load(db: &PgPool, scope: Scope, only: Option<Uuid>) -> sqlx::Result<Vec<MachineView>> {
    load_with(db, scope, only, true).await
}

/// `with_traffic = false` 跳过本月流量聚合（告警循环只要健康判定，不需要流量）
pub async fn load_with(db: &PgPool, scope: Scope, only: Option<Uuid>, with_traffic: bool) -> sqlx::Result<Vec<MachineView>> {
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
                a.agent_version a_version, a.last_report_at a_last, (a.info->'xray'->>'running')::bool a_xray,
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
    let hs: Vec<H> = sqlx::query_as(
        "select * from (select machine_id, row_number() over (partition by machine_id order by ts desc) rn, ts, cpu_pct, load1,
                mem_used, mem_total, disk_used, disk_total, net_rx_bytes, net_tx_bytes, tcp_conns
         from host_metric where machine_id = any($1) and ts > now() - interval '15 minutes') x where rn <= 2",
    )
    .bind(&ids)
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

    // 入口（作为节点或线路机）
    #[derive(sqlx::FromRow)]
    struct E {
        id: Uuid,
        relay_id: Uuid,
        relay_label: String,
        node_id: Uuid,
        node_team: Option<String>,
        node_number: Option<i32>,
        port: i32,
        lifecycle: String,
        applied: bool,
        apply_error: Option<String>,
        relay_xray_down: bool,
    }
    let es: Vec<E> = sqlx::query_as(
        "select e.id, e.relay_id, rm.label relay_label, e.node_id, t.name node_team, nm.number node_number, e.port,
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

    #[derive(sqlx::FromRow)]
    struct P {
        entry_id: Uuid,
        kind: String,
        ok: bool,
        latency_ms: Option<f32>,
        detail: Option<String>,
        ts: OffsetDateTime,
    }
    let ps: Vec<P> = sqlx::query_as(
        "select distinct on (entry_id, kind) entry_id, kind, ok, latency_ms, detail, ts from probe_result
         where entry_id = any($1) and ts > now() - interval '15 minutes' order by entry_id, kind, ts desc",
    )
    .bind(&eids)
    .fetch_all(db)
    .await?;
    let mut probes: HashMap<(Uuid, String), Probe> = HashMap::new();
    for p in ps {
        probes.insert((p.entry_id, p.kind), Probe { ok: p.ok, latency_ms: p.latency_ms, detail: p.detail, ts: p.ts });
    }

    // 流量：小时汇总（今天、本月按北京时间）
    let tr: Vec<(Uuid, i64, i64, i64, i64)> = if !with_traffic { vec![] } else { sqlx::query_as(
        "select entry_id,
           coalesce(sum(up_bytes) filter (where hour >= date_trunc('day', now() at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai'), 0)::bigint,
           coalesce(sum(down_bytes) filter (where hour >= date_trunc('day', now() at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai'), 0)::bigint,
           coalesce(sum(up_bytes), 0)::bigint, coalesce(sum(down_bytes), 0)::bigint
         from entry_traffic_hourly
         where entry_id = any($1) and hour >= date_trunc('month', now() at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai'
         group by entry_id",
    )
    .bind(&eids)
    .fetch_all(db)
    .await? };
    let traffic: HashMap<Uuid, Traffic> = tr
        .into_iter()
        .map(|(id, tu, td, mu, md)| (id, Traffic { today_up: tu, today_down: td, month_up: mu, month_down: md }))
        .collect();

    let mut entries_by: HashMap<Uuid, Vec<EntryView>> = HashMap::new();
    for e in es {
        let v = EntryView {
            id: e.id,
            relay: Ref { id: e.relay_id, name: e.relay_label.clone() },
            node: Ref {
                id: e.node_id,
                name: display_name("vps", None, e.node_team.as_deref(), e.node_number),
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

    let jobs: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "select target_id, id, kind, state::text from job
         where target_id = any($1) and state in ('queued','running','waiting','cancelling')",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let jobs: HashMap<Uuid, JobRef> = jobs.into_iter().map(|(t, id, kind, state)| (t, JobRef { id, kind, state })).collect();

    let inc: Vec<(Uuid, i64)> = sqlx::query_as(
        "select subject_id, count(*) from incident where state = 'firing' and subject_id = any($1) group by 1",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let inc: HashMap<Uuid, i64> = inc.into_iter().collect();

    // 维护静默：subject_id 为空 = 全局维护，所有机器都算维护中
    let maint: Vec<(Option<Uuid>,)> = sqlx::query_as(
        "select distinct subject_id from silence where is_maintenance and deleted_at is null
         and now() between starts_at and ends_at and (subject_id is null or subject_id = any($1))",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let maint_all = maint.iter().any(|(s,)| s.is_none());
    let maint: std::collections::HashSet<Uuid> = maint.into_iter().filter_map(|(id,)| id).collect();

    // 最近 5 分钟主机负载的最低值（持续高负载判定）
    #[derive(sqlx::FromRow)]
    struct S {
        machine_id: Uuid,
        span_s: Option<i64>,
        cpu_min: Option<f32>,
        mem_min: Option<f32>,
        disk_min: Option<f32>,
    }
    let su: Vec<S> = sqlx::query_as(
        "select machine_id, extract(epoch from max(ts) - min(ts))::bigint span_s, min(cpu_pct) cpu_min,
                (min(mem_used::float8 * 100 / nullif(mem_total, 0)))::real mem_min, (min(disk_used::float8 * 100 / nullif(disk_total, 0)))::real disk_min
         from host_metric where machine_id = any($1) and ts > now() - interval '5 minutes' group by machine_id",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let sustained: HashMap<Uuid, Sustained> =
        su.into_iter().map(|r| (r.machine_id, Sustained { span_s: r.span_s.unwrap_or(0), cpu_min: r.cpu_min, mem_min: r.mem_min, disk_min: r.disk_min })).collect();

    let policies = crate::policy::load_current(db).await?;

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
            let maintenance = maint_all || maint.contains(&r.id);
            let policy_blocked = r.kind != "relay" && policies.effective(r.id, r.team_id).0.mode == crate::policy::Mode::Blocked;
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
                health: Health::default(),
                active_job: jobs.get(&r.id).cloned(),
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
