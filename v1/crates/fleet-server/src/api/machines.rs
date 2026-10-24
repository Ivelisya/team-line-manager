use crate::error::ApiJson;
use super::ok;
use crate::auth::Actor;
use crate::db::{audit, bump_relays_of_nodes, new_id};
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::jobs;
use crate::meta;
use crate::names::{compact_numbers, node_name};
use crate::state::{AppState, Event};
use crate::views::{self, Scope};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

#[derive(Deserialize)]
pub struct ListQ {
    scope: Option<String>,
    /// `brief`：只给按名字找机器要用的字段（见 `views::brief`）
    view: Option<String>,
}

pub async fn list(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<ListQ>) -> ApiResult<Response> {
    // 回收站里的机器不在任何范围里（看 GET /recycle-bin）
    let scope = match q.scope.as_deref() {
        None | Some("live") => Scope::Live,
        Some("all") => Scope::All,
        Some(_) => return Err(ApiError::bad_request("scope 只能是 live 或 all（回收站看 /recycle-bin）")),
    };
    if q.view.as_deref() == Some("brief") {
        return Ok(ok(views::brief(&st, scope).await?).into_response());
    }
    if scope == Scope::Live {
        // 在用机器走缓存视图，响应体也随它缓存（同一份视图只序列化一次）
        return Ok(([(header::CONTENT_TYPE, "application/json")], views::live_json(&st).await?).into_response());
    }
    Ok(ok(views::load(&st, scope, None).await?).into_response())
}

pub async fn get_one(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    // 在用机器先找已经算好的那份缓存视图（不等它重算：视图卡住时 live 要等满 VIEW_WAIT）；只用之后没有资源变更、没有新一轮小时汇总、没有作业 / 告警 / 静默变化的（旧不过 STALE_MAX），
    // 改完（含开维护、静默）马上重取的看到的是改动之后的。不在里面的（已退役）、视图旧了或变过的，单独加载这一台
    if let Some(v) = st.live.stale(true, true) {
        if let Some(m) = v.iter().find(|m| m.id == id) {
            return Ok(ok(m));
        }
    }
    let mut v = views::load(&st, Scope::All, Some(id)).await?;
    v.pop().map(ok).ok_or_else(|| ApiError::not_found("没有这台机器"))
}

/// 读出显示名（确认框比对用）
pub async fn display_name_of(db: &sqlx::PgPool, id: Uuid) -> ApiResult<(String, String, String)> {
    let (kind, label, team, number, lifecycle): (String, Option<String>, Option<String>, Option<i32>, String) = sqlx::query_as(
        "select m.kind::text, m.label, t.name, m.number, m.lifecycle::text from machine m left join team t on t.id = m.team_id where m.id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    Ok((crate::names::display_name(&kind, label.as_deref(), team.as_deref(), number), kind, lifecycle))
}

pub const MAX_LABEL_CHARS: usize = 200;
pub const MAX_NOTE_CHARS: usize = 2000;

/// 线路机名、备注的长度上限（上架、修改共用）
pub fn check_text(label: Option<&str>, note: Option<&str>) -> Result<(), String> {
    if label.is_some_and(|l| l.chars().count() > MAX_LABEL_CHARS) {
        return Err(format!("线路机名最多 {MAX_LABEL_CHARS} 个字符"));
    }
    if note.is_some_and(|n| n.chars().count() > MAX_NOTE_CHARS) {
        return Err(format!("备注最多 {MAX_NOTE_CHARS} 个字符"));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct PatchIn {
    revision: i32,
    team_id: Option<Uuid>,
    number: Option<i32>,
    label: Option<String>,
    note: Option<String>,
    #[serde(default, deserialize_with = "super::some")]
    provider_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "super::some")]
    plan_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "super::some")]
    service_ref: Option<Option<String>>,
    /// 国家（ISO 3166-1 两位代码）；null 或空串 = 改回按 IP 自动
    #[serde(default, deserialize_with = "super::some")]
    country: Option<Option<String>>,
}

impl PatchIn {
    /// 只改团队与编号（改名）：替换节点（jobs/steps/replace.rs）用，其余字段不动
    pub fn rename(revision: i32, team_id: Uuid, number: i32) -> Self {
        PatchIn { revision, team_id: Some(team_id), number: Some(number), label: None, note: None, provider_id: None, plan_id: None, service_ref: None, country: None }
    }
}

/// PATCH 前锁住的当前值：(revision, kind, team_id, number, label, lifecycle, country)
type PatchCur = (i32, String, Option<Uuid>, Option<i32>, Option<String>, String, Option<String>, Option<String>);

pub async fn patch(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<PatchIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let relays = patch_tx(&mut tx, &a.name, id, b).await?.relays;
    tx.commit().await?;
    for r in relays {
        st.relay_bumped(r);
    }
    st.emit(Event::Machine { id });
    get_one(a, State(st), ApiPath(id)).await
}

/// `patch_tx` 的结果
pub struct Patched {
    /// 要刷新的线路机（提交后调 `relay_bumped`）
    pub relays: Vec<Uuid>,
    /// 进了备用团队、把开着的自动续费关掉了
    pub auto_renew_off: bool,
}

/// PATCH 的全部校验与写入（含审计），在调用方的事务里做。
/// 改团队 / 编号（改名）只走这里：网页、fleetctl rename 与替换节点（jobs/steps/replace.rs）同一套规则与审计
pub async fn patch_tx(tx: &mut crate::db::Tx<'_>, actor: &str, id: Uuid, b: PatchIn) -> ApiResult<Patched> {
    let country = match &b.country {
        Some(c) => Some(crate::country::normalize(c.as_deref().unwrap_or("")).map_err(ApiError::bad_request)?),
        None => None,
    };
    let cur: PatchCur = sqlx::query_as(
        "select revision, kind::text, team_id, number, label, lifecycle::text, country, note from machine where id = $1 for update",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    if cur.0 != b.revision {
        return Err(ApiError::conflict("这台机器刚被别人改过，请刷新后再改"));
    }
    if cur.5 == "retired" {
        return Err(ApiError::conflict("已退役的机器不能修改"));
    }
    let is_relay = cur.1 == "relay";
    let team_id = if is_relay { None } else { b.team_id.or(cur.2) };
    let number = if is_relay {
        None
    } else if b.number.is_none() && team_id != cur.2 {
        // 换团队没给编号：排到新团队的最小空号（原编号在新团队里多半已经有人，「移出备用」照提示只给团队也能成）
        let n: Option<i32> = sqlx::query_scalar(
            "select min(n)::int from generate_series(1, 999) n
             where not exists (select 1 from machine where team_id = $1 and number = n and kind <> 'relay' and lifecycle <> 'retired')",
        )
        .bind(team_id)
        .fetch_one(&mut **tx)
        .await?;
        Some(n.ok_or_else(|| ApiError::conflict("这个团队的编号用完了"))?)
    } else {
        b.number.or(cur.3)
    };
    if let Some(n) = number {
        if !(1..=999).contains(&n) {
            return Err(ApiError::bad_request("编号范围 1–999"));
        }
    }
    let label = if is_relay {
        match &b.label {
            Some(l) => Some(l.trim().to_string()).filter(|s| !s.is_empty()).ok_or_else(|| ApiError::bad_request("线路机名不能为空"))?.into(),
            None => cur.4.clone(),
        }
    } else {
        cur.4.clone()
    };
    check_text(if is_relay { b.label.as_deref().map(str::trim) } else { None }, b.note.as_deref()).map_err(ApiError::bad_request)?;
    let renamed = team_id != cur.2 || number != cur.3 || label != cur.4;

    sqlx::query(
        "update machine set team_id = $2, number = $3, label = $4,
            note = coalesce($5, note),
            provider_id = case when $6 then $7 else provider_id end,
            plan_id = case when $8 then $9 else plan_id end,
            service_ref = case when $10 then $11 else service_ref end,
            country = case when $12 then $13 else country end,
            revision = revision + 1, updated_at = now()
         where id = $1",
    )
    .bind(id)
    .bind(team_id)
    .bind(number)
    .bind(&label)
    .bind(&b.note)
    .bind(b.provider_id.is_some())
    .bind(b.provider_id.flatten())
    .bind(b.plan_id.is_some())
    .bind(b.plan_id.flatten())
    .bind(b.service_ref.is_some())
    .bind(b.service_ref.clone().flatten())
    .bind(country.is_some())
    .bind(country.clone().flatten())
    .execute(&mut **tx)
    .await?;
    // 有套餐时供应商就是套餐的：只换了套餐的跟着换供应商，明确给了另一个供应商的报错
    let other: Option<(Uuid,)> = sqlx::query_as(
        "select pl.provider_id from machine m join plan pl on pl.id = m.plan_id where m.id = $1 and m.provider_id is distinct from pl.provider_id",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((pv,)) = other {
        if b.provider_id.is_some() {
            return Err(ApiError::bad_request("套餐不属于所选的供应商"));
        }
        sqlx::query("update machine set provider_id = $2 where id = $1").bind(id).bind(pv).execute(&mut **tx).await?;
    }
    // 入口备注随节点名变：节点改名刷新它挂的线路机；线路机改名只影响它自己的直出入口（「线路机名-直出」），没有就不动
    let relays = if renamed { bump_relays_of_nodes(tx, &[id]).await? } else { vec![] };
    // 审计写上改前改后的显示名（「团队1-07 → 团队1-03」）：记录保持当时的样子，之后再改名也不跟着变
    let (old_team, new_team, old_spare, new_spare): (Option<String>, Option<String>, bool, bool) = sqlx::query_as(
        "select (select name from team where id = $1), (select name from team where id = $2),
                coalesce((select spare from team where id = $1), false), coalesce((select spare from team where id = $2), false)",
    )
    .bind(cur.2)
    .bind(team_id)
    .fetch_one(&mut **tx)
    .await?;
    let old_name = crate::names::display_name(&cur.1, cur.4.as_deref(), old_team.as_deref(), cur.3);
    let name = crate::names::display_name(&cur.1, label.as_deref(), new_team.as_deref(), number);
    // 进备用团队（退役、替换下来、手动改到备用）记下时刻和原名，并关掉自动续费（开着的话每到期就顺延一期，永远不会回收）；
    // 移出备用（撤销退役）清掉时刻、原名和退役原因，自动续费不自动打开
    let mut auto_renew_off = false;
    if new_spare && !old_spare {
        sqlx::query("update machine set spare_since = now(), spare_from = $2 where id = $1").bind(id).bind(&old_name).execute(&mut **tx).await?;
        auto_renew_off = sqlx::query("update subscription set auto_renew = false where machine_id = $1 and auto_renew").bind(id).execute(&mut **tx).await?.rows_affected() > 0;
    } else if old_spare && !new_spare {
        sqlx::query("update machine set spare_since = null, spare_from = null, lifecycle_reason = null where id = $1").bind(id).execute(&mut **tx).await?;
    }
    let mut detail = json!({"name": name, "old_name": old_name,
           "before": {"team_id": cur.2, "number": cur.3, "label": cur.4, "country": cur.6, "note": cur.7},
           "after": {"team_id": team_id, "number": number, "label": label, "country": country.clone().unwrap_or(cur.6.clone()), "note": b.note.clone().or(cur.7.clone())}});
    if auto_renew_off {
        detail["auto_renew_off"] = json!(true);
    }
    audit(tx, actor, if renamed { "machine.rename" } else { "machine.update" }, "machine", Some(id), detail).await?;
    Ok(Patched { relays, auto_renew_off })
}

// ── 时序 ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SeriesQ {
    metric: String,
    range: Option<String>,
}

pub async fn series(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<SeriesQ>) -> ApiResult<Json<Value>> {
    let hours: i64 = match q.range.as_deref().unwrap_or("24h") {
        "1h" => 1,
        "6h" => 6,
        "24h" => 24,
        "7d" => 168,
        "30d" => 720,
        _ => return Err(ApiError::bad_request("range 只能是 1h/6h/24h/7d/30d")),
    };
    let raw = hours <= 24;
    let since = OffsetDateTime::now_utc() - time::Duration::hours(hours);
    // 同机 IP 节点没有自己的主机指标：主机曲线是宿主机的
    let id = if q.metric == "host" {
        sqlx::query_scalar("select coalesce(host_id, id) from machine where id = $1").bind(id).fetch_optional(&st.db).await?.unwrap_or(id)
    } else {
        id
    };
    // 返回可直接画图的点：时间 ts，速率 bit/s，占用百分比，延迟 ms（失败为 null）
    let sql = match (q.metric.as_str(), raw) {
        ("host", true) => HOST_RAW,
        ("host", false) => HOST_HOURLY,
        ("traffic", true) => TRAFFIC_RAW,
        ("traffic", false) => TRAFFIC_HOURLY,
        ("probe", true) => PROBE_RAW,
        ("probe", false) => PROBE_HOURLY,
        _ => return Err(ApiError::bad_request("metric 只能是 host/traffic/probe")),
    };
    let v: Value = sqlx::query_scalar(sql).bind(id).bind(since).fetch_one(&st.db).await?;
    Ok(ok(json!({"metric": q.metric, "range": q.range.as_deref().unwrap_or("24h"), "resolution": if raw { "1m" } else { "1h" }, "points": v})))
}

const HOST_RAW: &str = "select coalesce(json_agg(json_build_object('ts', t, 'cpu_pct', cpu, 'mem_pct', mem_pct, 'disk_pct', disk_pct, 'load1', load1,
    'net_rx_bps', case when rx >= prx then (rx - prx) * 8.0 / nullif(extract(epoch from t - pt), 0) end,
    'net_tx_bps', case when tx >= ptx then (tx - ptx) * 8.0 / nullif(extract(epoch from t - pt), 0) end) order by t), '[]')
  from (select *, lag(rx) over w prx, lag(tx) over w ptx, lag(t) over w pt from (
    select date_trunc('minute', ts) t, avg(cpu_pct) cpu, 100.0 * avg(mem_used) / nullif(max(mem_total), 0) mem_pct,
           100.0 * max(disk_used) / nullif(max(disk_total), 0) disk_pct, max(load1) load1, max(net_rx_bytes) rx, max(net_tx_bytes) tx
    from host_metric where machine_id = $1 and ts >= $2 group by 1) a window w as (order by t)) b";

const HOST_HOURLY: &str = "select coalesce(json_agg(json_build_object('ts', hour, 'cpu_pct', cpu_avg, 'cpu_max', cpu_max,
    'mem_pct', 100.0 * mem_used_avg / nullif(mem_total_max, 0), 'disk_pct', 100.0 * disk_used_max / nullif(disk_total_max, 0),
    'net_rx_bps', net_rx * 8.0 / 3600, 'net_tx_bps', net_tx * 8.0 / 3600) order by hour), '[]')
  from host_metric_hourly where machine_id = $1 and hour >= $2";

const TRAFFIC_RAW: &str = "select coalesce(json_agg(json_build_object('ts', t, 'up', up, 'down', down, 'conns', conns) order by t), '[]') from (
    select date_trunc('minute', x.ts) t, sum(up_bytes) up, sum(down_bytes) down, max(conns) conns
    from entry_traffic x join entry e on e.id = x.entry_id where (e.node_id = $1 or e.relay_id = $1) and x.ts >= $2 group by 1) a";

const TRAFFIC_HOURLY: &str = "select coalesce(json_agg(json_build_object('ts', t, 'up', up, 'down', down, 'conns', conns) order by t), '[]') from (
    select h.hour t, sum(up_bytes) up, sum(down_bytes) down, max(conns_max) conns
    from entry_traffic_hourly h join entry e on e.id = h.entry_id where (e.node_id = $1 or e.relay_id = $1) and h.hour >= $2 group by 1) a";

const PROBE_RAW: &str = "select coalesce(json_agg(json_build_object('ts', t, 'tcp_ok', tcp_ok, 'tcp_ms', tcp_ms, 'egress_ok', eg_ok, 'egress_ms', eg_ms,
    'tcp_n', tcp_n, 'tcp_ok_n', tcp_ok_n) order by t), '[]') from (
    select date_trunc('minute', p.ts) t,
      bool_and(p.ok) filter (where p.kind = 'tcp') tcp_ok, avg(p.latency_ms) filter (where p.kind = 'tcp' and p.ok) tcp_ms,
      count(*) filter (where p.kind = 'tcp') tcp_n, count(*) filter (where p.kind = 'tcp' and p.ok) tcp_ok_n,
      bool_and(p.ok) filter (where p.kind = 'egress') eg_ok, avg(p.latency_ms) filter (where p.kind = 'egress' and p.ok) eg_ms
    from probe_result p join entry e on e.id = p.entry_id where (e.node_id = $1 or e.relay_id = $1) and p.kind in ('tcp', 'egress') and p.ts >= $2 group by 1) a";

const PROBE_HOURLY: &str = "select coalesce(json_agg(json_build_object('ts', t, 'tcp_ok', tcp_ok, 'tcp_ms', tcp_ms, 'egress_ok', eg_ok, 'egress_ms', eg_ms,
    'tcp_success_pct', tcp_pct, 'egress_success_pct', eg_pct) order by t), '[]') from (
    select h.hour t,
      sum(h.ok) filter (where h.kind = 'tcp') = sum(h.total) filter (where h.kind = 'tcp') tcp_ok,
      avg(h.latency_avg) filter (where h.kind = 'tcp') tcp_ms,
      100.0 * sum(h.ok) filter (where h.kind = 'tcp') / nullif(sum(h.total) filter (where h.kind = 'tcp'), 0) tcp_pct,
      sum(h.ok) filter (where h.kind = 'egress') = sum(h.total) filter (where h.kind = 'egress') eg_ok,
      avg(h.latency_avg) filter (where h.kind = 'egress') eg_ms,
      100.0 * sum(h.ok) filter (where h.kind = 'egress') / nullif(sum(h.total) filter (where h.kind = 'egress'), 0) eg_pct
    from probe_hourly h join entry e on e.id = h.entry_id where (e.node_id = $1 or e.relay_id = $1) and h.hour >= $2 group by 1) a";

#[derive(Deserialize)]
pub struct AvailabilityQ {
    days: Option<i32>,
}

#[derive(Serialize, sqlx::FromRow)]
struct DayAvailability {
    date: Date,
    tcp_total: i64,
    tcp_ok: i64,
    egress_total: i64,
    egress_ok: i64,
}

/// 近 N 天每天的探测成功率（抽屉「连接质量」的每日格条、fleetctl uptime）：小时汇总按这台机器的账单时区分到日历天
/// （生效的账单时区，同流量额度周期：没有订阅时取模板的，即供应商时区，供应商没填按组织时区），旧→新共 N 天、最后一天是今天（还没过完），没有探测的天各项为 0。
/// 入口同曲线：节点取自己的，线路机取挂在它上面的（含已移除的）
pub async fn availability(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<AvailabilityQ>) -> ApiResult<Json<Value>> {
    let days = q.days.unwrap_or(30);
    if !(1..=90).contains(&days) {
        return Err(ApiError::bad_request("days 只能是 1–90"));
    }
    let tz: String = sqlx::query_scalar("select coalesce(s.billing_tz, t.billing_tz) from sub_template t left join subscription_eff s on s.machine_id = t.machine_id where t.machine_id = $1")
        .bind(id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    let items: Vec<DayAvailability> = sqlx::query_as(AVAILABILITY).bind(id).bind(&tz).bind(days).fetch_all(&st.db).await?;
    let pct = |good: i64, total: i64| (total > 0).then(|| 100.0 * good as f64 / total as f64);
    let sum = |f: fn(&DayAvailability) -> i64| items.iter().map(f).sum::<i64>();
    let tcp_pct = pct(sum(|d| d.tcp_ok), sum(|d| d.tcp_total));
    let egress_pct = pct(sum(|d| d.egress_ok), sum(|d| d.egress_total));
    Ok(ok(json!({"days": days, "items": items, "tcp_pct": tcp_pct, "egress_pct": egress_pct})))
}

/// 小时汇总按 UTC 整点分桶，一桶按起点归到账单时区（$2）的那一天：整点时区正好落在零点边界，半小时时区跨零点的那一桶归前一天
const AVAILABILITY: &str = "with d as (select local_date($2) - i as day from generate_series($3 - 1, 0, -1) i),
  p as (
    select (h.hour at time zone $2)::date as day,
      sum(h.total) filter (where h.kind = 'tcp') tcp_total, sum(h.ok) filter (where h.kind = 'tcp') tcp_ok,
      sum(h.total) filter (where h.kind = 'egress') egress_total, sum(h.ok) filter (where h.kind = 'egress') egress_ok
    from probe_hourly h join entry e on e.id = h.entry_id
    where (e.node_id = $1 or e.relay_id = $1) and h.kind in ('tcp', 'egress')
      and h.hour >= (local_date($2) - ($3 - 1))::timestamp at time zone $2
    group by 1)
  select d.day as date, coalesce(p.tcp_total, 0) tcp_total, coalesce(p.tcp_ok, 0) tcp_ok, coalesce(p.egress_total, 0) egress_total, coalesce(p.egress_ok, 0) egress_ok
  from d left join p using (day) order by d.day";

// ── 退役（进备用 / 立即回收）与物理删除 ─────────────────────────────────────

#[derive(Deserialize)]
pub struct RetireIn {
    confirm: String,
    reason: Option<String>,
    /// 立即回收时清理机器上的服务（默认 true）；进备用时用不上
    cleanup: Option<bool>,
    /// true = 立即回收（退役作业：摘入口 → 清理 → 撤令牌 → 移进回收站）；默认移进备用团队，订阅过期满 1 天后由中控回收
    now: Option<bool>,
}

/// 退役：默认移进备用团队（`move_to_spare`，一个事务、不建作业，机器照常运行）；`now` 走退役作业立即回收（`start_retire`）。
/// 进不了备用的（线路机、没有订阅、已过期满 1 天、没上线、本来就在备用里，见 spare::spare_eligible）不给 now 回 409 并说明原因
pub async fn retire(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<RetireIn>) -> ApiResult<Json<Value>> {
    let (name, kind, _) = display_name_of(&st.db, id).await?;
    if b.confirm.trim() != name {
        return Err(ApiError::bad_request(format!("确认文字不对：请输入「{name}」")));
    }
    // 退役原因同备注的上限（写进机器的状态原因）
    check_text(None, b.reason.as_deref()).map_err(|_| ApiError::bad_request(format!("退役原因最多 {MAX_NOTE_CHARS} 个字符")))?;
    if b.now == Some(true) {
        let mut v = start_retire(&st, &a.name, id, &name, &kind, b.reason.as_deref(), b.cleanup.unwrap_or(true), None).await?;
        v["mode"] = json!("now");
        return Ok(ok(v));
    }
    Ok(ok(move_to_spare(&st, &a.name, id, b.reason.as_deref()).await?))
}

/// 能不能移进备用（spare::spare_eligible），以及订阅的到期日（算自动回收的日子）
async fn spare_check(conn: &mut sqlx::PgConnection, id: Uuid) -> ApiResult<(Result<(), &'static str>, Option<Date>)> {
    let (kind, lc, in_spare, due, today): (String, String, bool, Option<Date>, Option<Date>) = sqlx::query_as(
        "select m.kind::text, m.lifecycle::text, coalesce(t.spare, false), s.due_date, local_date(s.billing_tz)
         from machine m left join team t on t.id = m.team_id left join subscription_eff s on s.machine_id = m.id where m.id = $1",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    Ok((crate::spare::spare_eligible(&kind, &lc, in_spare, due.zip(today)), due))
}

/// 唯一的备用团队 (id, 名字) 与它占着的编号
async fn spare_team(conn: &mut sqlx::PgConnection) -> sqlx::Result<(Uuid, String, HashSet<i32>)> {
    let (id, name): (Uuid, String) = sqlx::query_as("select id, name from team where spare").fetch_one(&mut *conn).await?;
    let taken: Vec<i32> = sqlx::query_scalar("select number from machine where team_id = $1 and number is not null and kind <> 'relay' and lifecycle <> 'retired'")
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
    Ok((id, name, taken.into_iter().collect()))
}

/// 移进备用团队（退役的默认去向）：一个事务里改到备用团队的最小空号（patch_tx：同改名的校验与审计，顺带记下 spare_since / spare_from、
/// 刷新线路机备注），退役原因记进 lifecycle_reason。宿主机上还没退役、不在备用团队的同机 IP 节点一起进（编号挨着，到期随宿主机回收）。
/// 原团队压缩编号（同退役完成）。入口、端口、软路由出口都不动。返回 `{mode:"spare", name, from, recycle_on, ip_nodes}`
pub async fn move_to_spare(st: &AppState, actor: &str, id: Uuid, reason: Option<&str>) -> ApiResult<Value> {
    let mut tx = st.db.begin().await?;
    // 先锁团队（原团队、同机 IP 节点的团队、备用团队，按 id 排）再锁机器：和退役收尾、压缩编号同一个顺序（团队 → 机器）
    let mut teams: Vec<Uuid> = sqlx::query_scalar(
        "select distinct team_id from machine where team_id is not null and (id = $1 or host_id = $1) union select id from team where spare order by 1",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query("select 1 from team where id = any($1) order by id for update").bind(&teams).execute(&mut *tx).await?;
    sqlx::query("select 1 from machine where id = $1 for update").bind(id).execute(&mut *tx).await?;
    let (eligible, due) = spare_check(&mut tx, id).await?;
    eligible.map_err(|why| ApiError::conflict(format!("不能移进备用：{why}。要现在就清理并移进回收站，选「立即回收」（now: true）")))?;
    let ip_nodes: Vec<(Uuid, String, String, bool)> = ip_nodes_of(&mut tx, id, true).await?.into_iter().filter(|x| !x.3).collect();
    let all: Vec<Uuid> = std::iter::once(id).chain(ip_nodes.iter().map(|x| x.0)).collect();
    let busy: Option<String> = sqlx::query_scalar("select kind from job where target_id = any($1) and state in ('queued', 'running', 'waiting', 'cancelling') limit 1")
        .bind(&all)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(k) = busy {
        return Err(ApiError::conflict(format!("有进行中的作业（{}）：等它结束再退役", meta::job_kind_label(&k))));
    }
    let (spare_id, spare_name, taken) = spare_team(&mut tx).await?;
    let mut free = (1..=999).filter(|n| !taken.contains(n));
    let mut revs: Vec<(Uuid, i32, Option<Uuid>, String)> = sqlx::query_as(
        "select m.id, m.revision, m.team_id, display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id
         where m.id = any($1)",
    )
    .bind(&all)
    .fetch_all(&mut *tx)
    .await?;
    // 按 all 的顺序：宿主机在前
    revs.sort_by_key(|r| all.iter().position(|x| *x == r.0));
    let ip_reason = revs.iter().find(|r| r.0 == id).map(|r| format!("随宿主机「{}」退役", r.3)).unwrap_or_default();
    // (id, 原名, 新名)，宿主机在前
    let mut moved: Vec<(Uuid, String, String)> = vec![];
    let mut relays = vec![];
    // patch_tx 进备用时关掉了自动续费的（原名），结果里写明
    let mut renew_off = vec![];
    for (mid, rev, _, from) in &revs {
        let n = free.next().ok_or_else(|| ApiError::conflict(format!("备用团队「{spare_name}」的编号用完了")))?;
        let p = patch_tx(&mut tx, actor, *mid, PatchIn::rename(*rev, spare_id, n)).await?;
        relays.extend(p.relays);
        if p.auto_renew_off {
            renew_off.push(from.clone());
        }
        sqlx::query("update machine set lifecycle_reason = $2 where id = $1")
            .bind(mid)
            .bind(if *mid == id { reason } else { Some(ip_reason.as_str()) })
            .execute(&mut *tx)
            .await?;
        moved.push((*mid, from.clone(), node_name(&spare_name, n)));
    }
    // 原团队少了这几台：压缩编号（同退役完成时）
    teams.retain(|t| *t != spare_id && revs.iter().any(|r| r.2 == Some(*t)));
    let mut compacted = false;
    for t in &teams {
        let (ch, rs) = crate::db::compact_team(&mut tx, *t, actor, "节点退役后自动压缩").await?;
        compacted |= !ch.is_empty();
        relays.extend(rs);
    }
    let recycle_on = due.map(crate::spare::recycle_on);
    let (host_from, host_to) = (moved[0].1.clone(), moved[0].2.clone());
    let ip_out: Vec<Value> = moved[1..].iter().map(|(x, f, t)| json!({"id": x, "name": t, "from": f})).collect();
    audit(
        &mut tx,
        actor,
        "machine.retire",
        "machine",
        Some(id),
        json!({"mode": "spare", "name": host_from, "to": host_to, "reason": reason, "recycle_on": recycle_on,
               "ip_nodes": moved[1..].iter().map(|x| &x.1).collect::<Vec<_>>()}),
    )
    .await?;
    for (x, f, t) in &moved[1..] {
        audit(&mut tx, actor, "machine.retire", "machine", Some(*x), json!({"mode": "spare", "name": f, "to": t, "reason": ip_reason, "host": host_from})).await?;
    }
    tx.commit().await?;
    relays.sort();
    relays.dedup();
    for r in relays {
        st.relay_bumped(r);
    }
    st.emit(Event::Machines);
    let notes = spare_notes(&renew_off, &host_from, compacted.then_some(host_from.as_str()), true);
    Ok(json!({"mode": "spare", "name": host_to, "from": host_from, "recycle_on": recycle_on, "ip_nodes": ip_out, "auto_renew_off": !renew_off.is_empty(), "notes": notes}))
}

/// 进备用的提示（预览 `done = false`、结果 `done = true`）：开着的自动续费会关掉（不关的话每到期顺延一期，永远不回收；`auto_renew` 是开着的那几台，
/// 只有这台自己时不写名字，同机 IP 节点一起进的写上名字）；原团队压缩了编号时，撤销退役（改回原团队）拿到的是那时最小的空号，回不到原来的名字（`renumbered` = 原名）
fn spare_notes(auto_renew: &[String], host: &str, renumbered: Option<&str>, done: bool) -> Vec<String> {
    let mut v = vec![];
    if !auto_renew.is_empty() {
        let who = (auto_renew != [host]).then(|| auto_renew.join("、"));
        v.push(match (done, who) {
            (true, None) => "自动续费已关闭（供应商那边的自动扣款要自己去关）".to_string(),
            (true, Some(w)) => format!("{w} 的自动续费已关闭（供应商那边的自动扣款要自己去关）"),
            (false, None) => "这台开着自动续费：进备用时会关掉，到期后照常回收（供应商那边的自动扣款要自己去关）".to_string(),
            (false, Some(w)) => format!("{w} 开着自动续费：进备用时会关掉，到期后照常回收（供应商那边的自动扣款要自己去关）"),
        });
    }
    if let Some(n) = renumbered {
        v.push(format!("原团队后面的节点{}前移补号：撤销退役（改回原团队）拿到的是那时最小的空号，回不到{n}", if done { "已" } else { "会" }));
    }
    v
}

/// 立即回收：建退役作业（摘入口 → 等线路机 → 清理 → 撤令牌 → 收尾时移进回收站）。手动「立即回收」与备用团队到期自动回收（spare.rs）
/// 走这同一条路径。`name`、`kind` 是调用方刚读出的显示名与种类；
/// `auto`：中控自动发起时的标记（记进作业参数与审计的 auto，同机 IP 节点的作业也带上）与发起这一轮的时刻，手动退役为 None。
/// 备用到期（spare::AUTO）的锁住机器后按这一刻再核对一次还在备用团队、还过期满 1 天，不成立就 409（spare.rs 记 warn，下一轮再看）。
/// 返回 `{job_id, ip_node_jobs}`
#[allow(clippy::too_many_arguments)]
pub async fn start_retire(
    st: &AppState,
    actor: &str,
    id: Uuid,
    name: &str,
    kind: &str,
    reason: Option<&str>,
    cleanup: bool,
    auto: Option<(&str, OffsetDateTime)>,
) -> ApiResult<Value> {
    let mut tx = st.db.begin().await?;
    // 先锁机器行再判断、再计数：和「给线路机加入口」（对同一行 for share）互斥，守卫不会被并发挤穿
    let (lifecycle,): (String,) = sqlx::query_as("select lifecycle::text from machine where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    // 查出过期列表到这里之间，可能有人登记了续费、改了到期日，或者把它改回了原团队
    if let Some((crate::spare::AUTO, now)) = auto {
        let still: bool = sqlx::query_scalar(
            "select exists (select 1 from machine m join team t on t.id = m.team_id join subscription_eff s on s.machine_id = m.id
                            where m.id = $1 and t.spare and ($2::timestamptz at time zone s.billing_tz)::date > s.due_date + 1)",
        )
        .bind(id)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        if !still {
            return Err(ApiError::conflict("已经不在备用团队，或已续费：不自动回收"));
        }
    }
    // 宿主机退役：它上面的同机 IP 节点一并退役（它们的 SSH、agent、ssserver 都在这台机器上）
    let ip_nodes = ip_nodes_of(&mut tx, id, true).await?;
    let ip_ids: Vec<Uuid> = ip_nodes.iter().map(|x| x.0).collect();
    let all: Vec<Uuid> = std::iter::once(id).chain(ip_ids.iter().copied()).collect();
    if let Some(why) = retire_blockers(&mut tx, id, kind, &lifecycle, &all, true).await?.into_iter().next() {
        return Err(ApiError::conflict(why));
    }
    let n = sqlx::query(
        "update machine set lifecycle = 'retiring', lifecycle_reason = $2, revision = revision + 1, updated_at = now()
         where id = $1 and lifecycle not in ('retiring', 'retired')",
    )
    .bind(id)
    .bind(reason)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(ApiError::conflict("这台机器刚刚被别人退役了"));
    }
    let ip_reason = format!("随宿主机「{name}」退役");
    sqlx::query("update machine set lifecycle = 'retiring', lifecycle_reason = $2, revision = revision + 1, updated_at = now() where id = any($1)")
        .bind(&ip_ids)
        .bind(&ip_reason)
        .execute(&mut *tx)
        .await?;
    // 这些机器上其它没做完或失败的作业（上架做到一半、加入口失败…）全部作废，否则之后重试会把退役的机器又拉回去
    let dead: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "update job set state = 'cancelled', finished_at = now(), lease_owner = null, lease_until = null,
            params = params - 'login', error = coalesce(error, '机器进入退役，作业已作废')
         where target_id = any($1) and state in ('queued', 'running', 'waiting', 'cancelling', 'failed') returning id, target_id",
    )
    .bind(&all)
    .fetch_all(&mut *tx)
    .await?;
    for (jid, _) in &dead {
        sqlx::query("insert into job_log (job_id, level, msg) values ($1, 'warn', '机器进入退役，本作业已作废（不会再重试）')").bind(jid).execute(&mut *tx).await?;
    }
    // 退役中的机器退出监控（不再评估，也不当成恢复）：未结告警现在就关（不发恢复通知）。
    // 退役作业失败会停在退役中等重试（步骤不可回滚），告警不能等到收尾才关
    let closed = sqlx::query(
        "update incident set state = 'resolved', resolved_at = now(), detail = detail || '{\"closed_by\":\"retire\"}'
         where state = 'firing' and subject_id = any($1)",
    )
    .bind(&all)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    // 自动发起的：作业参数与审计带上 auto（审计里和手动退役分得开；收尾时记进回收站的审计）
    let tag = |mut v: Value| {
        if let Some((x, _)) = auto {
            v["auto"] = json!(x);
        }
        v
    };
    let job = jobs::enqueue(&mut tx, "retire", Some(id), tag(json!({"cleanup": cleanup, "reason": reason, "prev_lifecycle": lifecycle})), actor).await?;
    audit(
        &mut tx,
        actor,
        "machine.retire",
        "machine",
        Some(id),
        tag(json!({"mode": "now", "name": name, "reason": reason, "job_id": job, "cancelled_jobs": dead.iter().map(|(j, _)| j).collect::<Vec<_>>(),
               "ip_nodes": ip_nodes.iter().map(|x| &x.1).collect::<Vec<_>>()})),
    )
    .await?;
    // 同机 IP 节点各自一个退役作业：只摘自己的入口、不登录清理（宿主机的清理连同它们的实例一起删）
    let mut ip_jobs = vec![];
    for (xid, xname, xlc, _) in &ip_nodes {
        let j = jobs::enqueue(&mut tx, "retire", Some(*xid), tag(json!({"cleanup": false, "reason": ip_reason, "prev_lifecycle": xlc})), actor).await?;
        audit(&mut tx, actor, "machine.retire", "machine", Some(*xid), tag(json!({"mode": "now", "name": xname, "reason": ip_reason, "job_id": j, "host": name}))).await?;
        ip_jobs.push(json!({"machine_id": xid, "name": xname, "job_id": j}));
    }
    tx.commit().await?;
    st.wake_jobs();
    // 在用 → 退役中：列表要变
    st.emit(Event::Machines);
    st.emit(Event::Job { id: job, target: Some(id) });
    for j in &ip_jobs {
        let uuid = |k: &str| j[k].as_str().and_then(|s| Uuid::try_parse(s).ok());
        if let Some(jid) = uuid("job_id") {
            st.emit(Event::Job { id: jid, target: uuid("machine_id") });
        }
    }
    if closed > 0 {
        st.emit(Event::Incidents);
    }
    for (j, t) in dead {
        st.emit(Event::Job { id: j, target: Some(t) });
    }
    Ok(json!({"job_id": job, "ip_node_jobs": ip_jobs}))
}

/// 立即回收会被拒绝的原因（提交时取第一条回 409，预览里全部列在 `blockers`）：已经退役 / 退役中、线路机上还有节点入口、
/// 这台或它的同机 IP 节点（`all`）有作业正在执行一步。`lock`：提交时锁住这些机器上没结束的作业（领取跳过锁住的行）再看租约
async fn retire_blockers(conn: &mut sqlx::PgConnection, id: Uuid, kind: &str, lifecycle: &str, all: &[Uuid], lock: bool) -> sqlx::Result<Vec<String>> {
    let mut out = vec![];
    match lifecycle {
        "retired" => out.push("已经退役".to_string()),
        "retiring" => out.push("这台机器已经在退役中：到作业页看进度，失败了点重试".to_string()),
        _ => {}
    }
    if kind == "relay" {
        let (n,): (i64,) = sqlx::query_as(
            "select count(*) from entry e join machine m on m.id = e.node_id
             where e.relay_id = $1 and e.lifecycle <> 'removed' and m.id <> $1",
        )
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
        if n > 0 {
            out.push(format!("这台线路机上还有 {n} 个节点入口：先把节点迁走或退役。可先用「搬空线路机」把节点换到其他线路机"));
        }
    }
    // 有作业正在执行一步（租约未过期）：库里标成作废拦不住它，那一步还会接着跑（装 agent、签令牌…），和退役的清理撞在一起
    let open: Vec<(String, bool)> = sqlx::query_as(&format!(
        "select kind, state in ('running', 'cancelling') and lease_until > now() from job
         where target_id = any($1) and state in ('queued', 'running', 'waiting', 'cancelling'){}",
        if lock { " for update" } else { "" }
    ))
    .bind(all)
    .fetch_all(&mut *conn)
    .await?;
    if let Some((k, _)) = open.iter().find(|j| j.1) {
        out.push(format!("有作业（{}）正在执行一步：先在作业页取消它，等它停下再退役（升级 agent 不能取消，等它做完）", meta::job_kind_label(k)));
    }
    // 这台是进行中的替换 / 改出口指向的新节点（作业的目标是旧节点，上面作废不到它）：软路由正在（或已经）改成指向这台的入口，退役会把它们摘掉
    let feeding: Option<(String,)> = sqlx::query_as(&format!(
        "select kind from job where kind in ('replace', 'repoint') and params->>'new_id' = any($1::uuid[]::text[])
           and state in ('queued', 'running', 'waiting', 'cancelling') limit 1{}",
        if lock { " for update" } else { "" }
    ))
    .bind(all)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((k,)) = feeding {
        out.push(format!("这台是进行中的作业（{}）的新节点，软路由的出口要改成（或已经改成）指向它：等那个作业结束再退役", meta::job_kind_label(&k)));
    }
    Ok(out)
}

/// 宿主机上还没开始退役的同机 IP 节点：(id, 显示名, 生命周期, 在不在备用团队)，按团队、编号排。`lock`：锁住这些机器行（退役时）
async fn ip_nodes_of(conn: &mut sqlx::PgConnection, host: Uuid, lock: bool) -> sqlx::Result<Vec<(Uuid, String, String, bool)>> {
    sqlx::query_as(&format!(
        "select m.id, display_name(m.kind::text, m.label, t.name, m.number), m.lifecycle::text, coalesce(t.spare, false)
         from machine m left join team t on t.id = m.team_id
         where m.host_id = $1 and m.lifecycle not in ('retiring', 'retired') order by t.sort_order, t.name, m.number{}",
        if lock { " for update of m" } else { "" }
    ))
    .bind(host)
    .fetch_all(conn)
    .await
}

/// 退役预览。立即回收：会一并退役的同机 IP 节点（宿主机才有）、涉及的团队里会因压缩而改名的节点（进备用时原团队同样压缩）；
/// `spare`：能不能移进备用（不能的原因）、进去后的名字、自动回收的日子（账单时区）、一起进备用的同机 IP 节点；
/// `routers`：还指向这台（和它的同机 IP 节点）的软路由出口，只提示不阻断（要换掉就先「替换」）；
/// `blockers`：立即回收提交时会被拒绝的原因（`retire_blockers`），空 = 能回收
pub async fn retire_preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let (name, kind, lc) = display_name_of(&st.db, id).await?;
    let mut conn = st.db.acquire().await?;
    let ip_nodes = ip_nodes_of(&mut conn, id, false).await?;
    let leaving: Vec<Uuid> = std::iter::once(id).chain(ip_nodes.iter().map(|x| x.0)).collect();
    let blockers = retire_blockers(&mut conn, id, &kind, &lc, &leaving, false).await?;
    let mut renames = vec![];
    let teams: Vec<(Uuid, String)> =
        sqlx::query_as("select id, name from team where id in (select team_id from machine where id = any($1)) order by sort_order, name")
            .bind(&leaving)
            .fetch_all(&mut *conn)
            .await?;
    for (team_id, team) in teams {
        let nodes: Vec<(Uuid, i32)> = sqlx::query_as(
            "select id, number from machine where team_id = $1 and kind <> 'relay' and lifecycle <> 'retired' and id <> all($2) order by number",
        )
        .bind(team_id)
        .bind(&leaving)
        .fetch_all(&mut *conn)
        .await?;
        renames.extend(
            compact_numbers(nodes)
                .into_iter()
                .map(|(mid, o, n)| json!({"machine_id": mid, "old_name": node_name(&team, o), "new_name": node_name(&team, n)})),
        );
    }
    // 进备用：编号按 move_to_spare 的分法（宿主机先取最小的空号，同机 IP 节点接着取）
    let (eligible, due) = spare_check(&mut conn, id).await?;
    let (spare_id, spare_name, taken) = spare_team(&mut conn).await?;
    let mut free = (1..=999).filter(|n| !taken.contains(n));
    let to_name = free.next().map(|n| node_name(&spare_name, n));
    let spare_ip: Vec<Value> =
        ip_nodes.iter().filter(|x| !x.3).map(|(i, n, _, _)| json!({"id": i, "name": n, "to_name": free.next().map(|k| node_name(&spare_name, k))})).collect();
    // 一起进备用的（这台和还没退役的同机 IP 节点）里开着自动续费的
    let going: Vec<(Uuid, &str)> = std::iter::once((id, name.as_str())).chain(ip_nodes.iter().filter(|x| !x.3).map(|x| (x.0, x.1.as_str()))).collect();
    let on: Vec<Uuid> = sqlx::query_scalar("select machine_id from subscription where machine_id = any($1) and auto_renew")
        .bind(going.iter().map(|x| x.0).collect::<Vec<_>>())
        .fetch_all(&mut *conn)
        .await?;
    let auto_renew: Vec<String> = going.iter().filter(|x| on.contains(&x.0)).map(|x| x.1.to_string()).collect();
    let spare = json!({
        "ok": eligible.is_ok(), "why": eligible.err(), "team": {"id": spare_id, "name": spare_name}, "to_name": to_name,
        "recycle_on": due.map(crate::spare::recycle_on), "ip_nodes": spare_ip,
        "notes": spare_notes(&auto_renew, &name, (!renames.is_empty()).then_some(name.as_str()), false),
    });
    // 软路由出口：按路由归在一起，写明指向的是哪台
    let names: std::collections::HashMap<Uuid, &str> = std::iter::once((id, name.as_str())).chain(ip_nodes.iter().map(|x| (x.0, x.1.as_str()))).collect();
    let mut routers: Vec<(Uuid, String, Vec<Value>)> = vec![];
    let exits = crate::routers::exits_using(&st.db, &leaving).await?;
    for node in &leaving {
        for (rid, rname, exit) in exits.get(node).into_iter().flatten() {
            let e = json!({"exit": exit, "node": names[node]});
            match routers.iter_mut().find(|r| r.0 == *rid) {
                Some(r) => r.2.push(e),
                None => routers.push((*rid, rname.clone(), vec![e])),
            }
        }
    }
    let routers: Vec<Value> = routers.into_iter().map(|(i, n, e)| json!({"id": i, "name": n, "exits": e})).collect();
    let ip_nodes: Vec<Value> = ip_nodes.iter().map(|(i, n, _, _)| json!({"id": i, "name": n})).collect();
    Ok(ok(json!({"name": name, "renames": renames, "ip_nodes": ip_nodes, "spare": spare, "routers": routers, "blockers": blockers})))
}

/// 建物理删除作业并记审计（调用方已核对过：已进回收站、`db::purge_blocker` 不拦）。只由维护任务调（spare.rs `purge_pass`）；
/// `auto` 同 `start_retire`。同一台已有进行中的作业时由唯一索引挡住（409）
pub async fn start_purge(st: &AppState, actor: &str, id: Uuid, name: &str, auto: Option<&str>) -> ApiResult<Uuid> {
    let mut tx = st.db.begin().await?;
    let mut params = json!({"name": name});
    let mut detail = json!({"name": name});
    if let Some(x) = auto {
        params["auto"] = json!(x);
        detail["auto"] = json!(x);
    }
    let job = jobs::enqueue(&mut tx, "purge", Some(id), params, actor).await?;
    detail["job_id"] = json!(job);
    audit(&mut tx, actor, "machine.purge", "machine", Some(id), detail).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    Ok(job)
}

// ── agent 升级 ────────────────────────────────────────────────────────────

/// 给 `ms`（id, 种类, 显示名）按顺序各建一个升级作业，返回 (建好的作业, 跳过的机器)。已有进行中作业的机器由唯一索引
/// job_one_active_per_target 挡住（409「这台机器还有进行中的作业」），不另外预查；每台一个 savepoint：一台冲突只跳过这台，不整批回滚。
/// 多台时排成编排（作业参数 `after` = 要等的作业，见 jobs::claim）：第一个建成的是金丝雀；线路机一台等前一台；
/// VPS 节点都等最后一台线路机（没有线路机时等金丝雀），之后并发。`rollout`（「全部升级」才有）记进每个作业的参数，开始前核对
async fn enqueue_upgrade(st: &AppState, actor: &str, ms: &[(Uuid, String, String)], rollout: Option<&Value>) -> ApiResult<(Vec<Value>, Vec<Value>)> {
    let mut tx = st.db.begin().await?;
    let (mut out, mut skipped) = (vec![], vec![]);
    let (mut canary, mut last_relay): (Option<Uuid>, Option<Uuid>) = (None, None);
    for (id, kind, name) in ms {
        let after = if kind == "relay" { last_relay } else { last_relay.or(canary) };
        let mut params = after.map_or_else(|| json!({}), |a| json!({"after": a}));
        if let Some(r) = rollout {
            params["rollout"] = r.clone();
        }
        let mut sp = sqlx::Connection::begin(&mut *tx).await?;
        match jobs::enqueue(&mut sp, "upgrade_agent", Some(*id), params, actor).await {
            Ok(job) => {
                sp.commit().await?;
                canary = canary.or(Some(job));
                if kind == "relay" {
                    last_relay = Some(job);
                }
                out.push(json!({"machine_id": id, "name": name, "kind": kind, "job_id": job, "after": after}));
            }
            Err(e) if e.status == StatusCode::CONFLICT => {
                sp.rollback().await?;
                skipped.push(json!({"machine_id": id, "name": name, "reason": e.message}));
            }
            Err(e) => return Err(e),
        }
    }
    // 摘要写出升级了哪几台（最多列 3 台）
    let names: Vec<&Value> = out.iter().take(3).map(|j| &j["name"]).collect();
    audit(&mut tx, actor, "agent.upgrade", "machine", None, json!({"count": out.len(), "skipped": skipped.len(), "names": names})).await?;
    tx.commit().await?;
    st.wake_jobs();
    for j in &out {
        let uuid = |k: &str| j[k].as_str().and_then(|s| Uuid::try_parse(s).ok());
        if let Some(id) = uuid("job_id") {
            st.emit(Event::Job { id, target: uuid("machine_id") });
        }
    }
    Ok((out, skipped))
}

/// 机器的 (种类, 生命周期, 显示名, 宿主机显示名)：升级 agent、系统调优这类只对有自己机器的节点 / 线路机做的操作先看这个
async fn brief_of(st: &AppState, id: Uuid) -> ApiResult<(String, String, String, Option<String>)> {
    sqlx::query_as(
        "select m.kind::text, m.lifecycle::text, display_name(m.kind::text, m.label, mt.name, m.number), display_name(h.kind::text, h.label, t.name, h.number)
         from machine m left join team mt on mt.id = m.team_id left join machine h on h.id = m.host_id left join team t on t.id = h.team_id
         where m.id = $1",
    )
    .bind(id)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(|| ApiError::not_found("没有这台机器"))
}

/// 单台：推送中控上当前版本的 agent 并重启（令牌顺带轮换）；新版本起不来、没上报就换回旧版本（见 jobs::steps）
pub async fn upgrade_agent(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let (kind, lc, name, host) = brief_of(&st, id).await?;
    if let Some(h) = host {
        return Err(ApiError::conflict(format!("同机 IP 节点没有自己的 agent，用的是宿主机「{h}」的：升级宿主机的 agent")));
    }
    if kind == "proxy" || lc != "active" {
        return Err(ApiError::conflict("只有在用的线路机和 VPS 节点有 agent"));
    }
    let (v, skipped) = enqueue_upgrade(&st, &a.name, &[(id, kind, name)], None).await?;
    v.into_iter().next().map(ok).ok_or_else(|| ApiError::conflict(skipped[0]["reason"].as_str().unwrap_or("这台机器还有进行中的作业")))
}

#[derive(Deserialize, Default)]
pub struct TuneIn {
    /// true = 撤销调优：按机器上的原值快照写回、删掉调优写的 sysctl 与开机载入文件（作业参数 `rollback`）
    #[serde(default)]
    rollback: bool,
}

/// 重做系统调优（作业 `tune`，当前调优档见 jobs/tune.rs）：写内核参数、journald 体积、补时间同步，不重启 ssserver / Xray、不断客户连接；
/// `rollback: true` 时撤销（同一个作业类型，参数不同）。
/// 同机 IP 节点和宿主机共用系统设置，家宽代理没有我们的机器；机器上已有进行中的作业时由唯一索引挡住（409）
pub async fn tune(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<TuneIn>) -> ApiResult<Json<Value>> {
    let (kind, lc, name, host) = brief_of(&st, id).await?;
    if let Some(h) = host {
        return Err(ApiError::conflict(format!("同机 IP 节点和宿主机共用系统设置：对宿主机「{h}」调优")));
    }
    if kind == "proxy" {
        return Err(ApiError::conflict("家宽代理没有我们的机器"));
    }
    if lc != "active" {
        return Err(ApiError::conflict("只有在用的机器能调优"));
    }
    let mut tx = st.db.begin().await?;
    let job = jobs::enqueue(&mut tx, "tune", Some(id), if b.rollback { json!({"rollback": true}) } else { json!({}) }, &a.name).await?;
    let detail = if b.rollback {
        json!({"name": name, "rollback": true, "job_id": job})
    } else {
        json!({"name": name, "profile": jobs::tune::PROFILE, "job_id": job})
    };
    audit(&mut tx, &a.name, "machine.tune", "machine", Some(id), detail).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    Ok(ok(json!({"machine_id": id, "name": name, "job_id": job})))
}

/// 重新下发节点的 SS 配置（作业 `ss_refresh`）：在节点上跑与上架同一份 ssserver 配置脚本，把当前的配置模板（出站目标黑名单等）推到存量节点，
/// 同机的同机 IP 节点实例一起刷；配置有变化才重启 ssserver（该节点现有连接断开几秒），最后经入口做出口验收（失败不回滚）。
/// 同机 IP 节点和宿主机共用黑名单文件：对宿主机下发。线路机、家宽代理没有落地节点的 ssserver；没有入口的节点没有放行名单也没法验收。
/// 机器上已有进行中的作业时由唯一索引挡住（409）
pub async fn ss_refresh(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let (kind, lc, name, host) = brief_of(&st, id).await?;
    if let Some(h) = host {
        return Err(ApiError::conflict(format!("同机 IP 节点和宿主机共用 ssserver 的黑名单文件：对宿主机「{h}」下发，同机的实例一起刷")));
    }
    match kind.as_str() {
        "relay" => return Err(ApiError::conflict("线路机上没有落地节点的 ssserver")),
        "proxy" => return Err(ApiError::conflict("家宽代理没有我们的 ssserver")),
        _ => {}
    }
    if lc != "active" {
        return Err(ApiError::conflict("只有在用的节点能下发"));
    }
    let has_entry: bool = sqlx::query_scalar("select exists (select 1 from entry where node_id = $1 and lifecycle <> 'removed')").bind(id).fetch_one(&st.db).await?;
    if !has_entry {
        return Err(ApiError::conflict("这台节点还没有入口（没有线路机在连它），没有可放行的线路机，也没法验收出口"));
    }
    let mut tx = st.db.begin().await?;
    let job = jobs::enqueue(&mut tx, "ss_refresh", Some(id), json!({}), &a.name).await?;
    audit(&mut tx, &a.name, "machine.ss_refresh", "machine", Some(id), json!({"name": name, "job_id": job})).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    Ok(ok(json!({"machine_id": id, "name": name, "job_id": job})))
}

/// 「全部升级」要推的 agent：dist 里程序的 sha256 与版本号。记进每个作业的参数 `rollout`，作业开始前核对（jobs::halted）：
/// 中途重新部署过中控时，后面的机器不能装上金丝雀没验过的版本
async fn dist_agent(st: &AppState) -> ApiResult<Value> {
    let path = st.cfg.dist_dir.join("fleet-agent");
    let sha = jobs::dist_agent_sha256(st).await.map_err(|e| ApiError::conflict(format!("中控上没有可推送的 agent 程序（{}：{e}）", path.display())))?;
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), tokio::process::Command::new(&path).arg("--version").output()).await;
    let version = match out {
        Ok(Ok(o)) if o.status.success() => String::from_utf8_lossy(&o.stdout).split_whitespace().nth(1).map(String::from),
        _ => None,
    };
    let version = version.ok_or_else(|| ApiError::conflict(format!("中控上的 agent 程序（{}）读不出版本号", path.display())))?;
    Ok(json!({"sha256": sha, "version": version}))
}

/// 全部在用的线路机与 VPS 节点（同机 IP 节点没有自己的 agent，不算）。不一起升：先升一台做金丝雀（有线路机先挑线路机），
/// 它成功了线路机再一台接一台，线路机都成功后 VPS 节点并发（引擎的并发上限）。同类里在用入口少的在前（出问题影响最小的先上），再按名字
pub async fn upgrade_all_agents(a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let rollout = dist_agent(&st).await?;
    let ms: Vec<(Uuid, String, String)> = sqlx::query_as(
        "select m.id, m.kind::text, display_name(m.kind::text, m.label, t.name, m.number) name
         from machine m left join team t on t.id = m.team_id
         where m.kind <> 'proxy' and m.lifecycle = 'active' and m.host_id is null
         order by m.kind = 'relay' desc, (select count(*) from entry e where (e.relay_id = m.id or e.node_id = m.id) and e.lifecycle <> 'removed'), name, m.id",
    )
    .fetch_all(&st.db)
    .await?;
    let (jobs, skipped) = enqueue_upgrade(&st, &a.name, &ms, Some(&rollout)).await?;
    let order = upgrade_order(&jobs);
    Ok(ok(json!({"jobs": jobs, "skipped": skipped, "order": order})))
}

/// 「全部升级」的顺序说明（接口返回，fleetctl 与网页照着显示）。`jobs` 是 enqueue_upgrade 建好的、按执行顺序。
/// 「出问题」指换上新版本之后的失败（起不来、没上报、线路机检查没通过）：推送之前就失败的只算那一台，见 jobs::halted
fn upgrade_order(jobs: &[Value]) -> String {
    let Some(first) = jobs.first() else { return "没有要升级的机器".into() };
    let canary = first["name"].as_str().unwrap_or("");
    let relays = jobs.iter().filter(|j| j["kind"] == "relay").count();
    let nodes = jobs.len() - relays;
    match (relays, nodes) {
        _ if jobs.len() == 1 => format!("只有「{canary}」一台"),
        (0, n) => format!("先升「{canary}」做金丝雀，成功后其余 {} 台 VPS 节点并发升级；新版本在金丝雀上出问题的话其余的都会取消", n - 1),
        (1, n) => format!("先升「{canary}」做金丝雀，成功后 {n} 台 VPS 节点并发升级；新版本在金丝雀上出问题的话其余的都会取消"),
        (r, 0) => format!("先升「{canary}」做金丝雀，成功后其余 {} 台线路机逐台升级；新版本在任何一台上出问题，没开始的都会取消", r - 1),
        (r, n) => format!(
            "先升「{canary}」做金丝雀，成功后其余 {} 台线路机逐台升级，线路机都成功后 {n} 台 VPS 节点并发升级；新版本在任何一台线路机上出问题，没开始的都会取消",
            r - 1
        ),
    }
}

// ── 账单 ──────────────────────────────────────────────────────────────────

/// 订阅里可以「单独设置」的字段：没传或 null = 跟随模板（价格、币种、周期、流量上限跟随套餐，账单时区、提前提醒、
/// 流量重置日、流量计费方式跟随供应商，见 views::SubTemplate）；和模板相同的值也按跟随存。上架与改订阅共用
#[derive(Deserialize, Serialize, Clone, Default)]
pub struct SubOverrides {
    price_cents: Option<i32>,
    currency: Option<String>,
    cycle: Option<String>,
    billing_tz: Option<String>,
    remind_days: Option<i32>,
    /// 0 = 不限
    traffic_gb: Option<i32>,
    traffic_reset_day: Option<i32>,
    /// 流量计费方式：sum 双向 / max、min、out、in 单向（meta::TRAFFIC_RULES）；null = 跟随供应商。
    /// 后加的字段：没传 = 保留现值（不认识它的旧 fleetctl、没刷新的旧网页改别的项时不把单独设置清掉）
    #[serde(default, deserialize_with = "super::some", skip_serializing_if = "Option::is_none")]
    traffic_rule: Option<Option<String>>,
}

impl SubOverrides {
    pub fn has_price(&self) -> bool { self.price_cents.is_some() }

    /// 校验并规整（币种转大写）：价格、流量上限不为负，周期四选一，提醒 0–60 天，重置日 1–31，计费方式五选一，时区 at time zone 认
    pub async fn check(&mut self, db: &mut sqlx::PgConnection) -> ApiResult<()> {
        if self.price_cents.is_some_and(|p| p < 0) || self.traffic_gb.is_some_and(|g| g < 0) {
            return Err(ApiError::bad_request("金额、流量上限不能为负"));
        }
        if let Some(c) = &self.cycle {
            meta::check_cycle(c).map_err(ApiError::bad_request)?;
        }
        if self.remind_days.is_some_and(|d| !(0..=60).contains(&d)) {
            return Err(ApiError::bad_request("提醒天数要在 0–60 之间"));
        }
        if self.traffic_reset_day.is_some_and(|d| !(1..=31).contains(&d)) {
            return Err(ApiError::bad_request("流量重置日要在 1–31 之间"));
        }
        super::catalog::traffic_rule(&self.traffic_rule.clone().flatten())?;
        if let Some(c) = &mut self.currency {
            // 同套餐的币种规则（比较「和套餐相同」时口径一致）
            *c = meta::currency(c).map_err(ApiError::bad_request)?;
        }
        if let Some(tz) = &self.billing_tz {
            check_tz(db, tz).await?;
        }
        Ok(())
    }

    /// 写订阅：到期日 + 这些单独设置（整份替换，流量计费方式没传的保留现值；自动续费、接入前读数不动），再把和模板相同的改回跟随。
    /// 账单日（billing_day）按 meta::billing_day：到期日落在现有账单日上（含原样带回的到期日、31 号账单日的 2-28）不动，否则改成新到期日几号
    pub async fn write(&self, db: &mut sqlx::PgConnection, id: Uuid, due: Date) -> ApiResult<()> {
        let cur: Option<i16> = sqlx::query_scalar("select billing_day from subscription where machine_id = $1").bind(id).fetch_optional(&mut *db).await?.flatten();
        let bday = meta::billing_day(due, cur.and_then(|d| u8::try_from(d).ok()));
        sqlx::query(
            "insert into subscription (machine_id, due_date, price_cents, currency, cycle, billing_tz, remind_days, traffic_gb, traffic_reset_day, traffic_rule, billing_day)
             values ($1, $2, $3, $4, $5::billing_cycle, $6, $7, $8, $9, $10, $11)
             on conflict (machine_id) do update set due_date = excluded.due_date, price_cents = excluded.price_cents, currency = excluded.currency,
               cycle = excluded.cycle, billing_tz = excluded.billing_tz, remind_days = excluded.remind_days, traffic_gb = excluded.traffic_gb,
               traffic_reset_day = excluded.traffic_reset_day,
               traffic_rule = case when $12 then subscription.traffic_rule else excluded.traffic_rule end, billing_day = excluded.billing_day, updated_at = now()",
        )
        .bind(id)
        .bind(due)
        .bind(self.price_cents)
        .bind(&self.currency)
        .bind(&self.cycle)
        .bind(&self.billing_tz)
        .bind(self.remind_days)
        .bind(self.traffic_gb)
        .bind(self.traffic_reset_day)
        .bind(self.traffic_rule.clone().flatten())
        .bind(i16::from(bday))
        .bind(self.traffic_rule.is_none())
        .execute(&mut *db)
        .await?;
        sqlx::query("select subscription_normalize($1)").bind(id).execute(&mut *db).await?;
        Ok(())
    }
}

/// PUT 订阅：到期日 + 单独设置（整份替换，没传 = 跟随）。没有套餐的机器要给 price_cents
#[derive(Deserialize, Serialize)]
pub struct SubIn {
    due_date: Date,
    #[serde(flatten)]
    ov: SubOverrides,
    /// 没传 = 不改（新建为否）
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_renew: Option<bool>,
    /// 已不支持（流量已用改由 agent 按网卡统计）：带了就 400，旧 fleetctl 的 --used 明确报错而不是静默忽略。
    /// 割接时录的接入前读数只读保留，进入下一个周期自动失效（views::Quota.base）
    #[serde(default, deserialize_with = "super::some", skip_serializing)]
    traffic_used_gb: Option<Option<f64>>,
}

/// 时区（账单时区、供应商时区）必须能被 `at time zone` 接受：views 里的 local_date 就这么用，不再兜底。
/// 不认识的时区 PG 报 22023（invalid_parameter_value）
pub async fn check_tz(db: &mut sqlx::PgConnection, tz: &str) -> ApiResult<()> {
    match sqlx::query("select now() at time zone $1").bind(tz).execute(db).await {
        Err(e) if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("22023") => {
            Err(ApiError::bad_request(format!("账单时区「{tz}」不存在：请填 IANA 时区名，如 {}", meta::org_tz())))
        }
        r => r.map(drop).map_err(Into::into),
    }
}

/// 改订阅审计里记的生效值（改动前后各一份）
const SUB_AUDIT: &str = "select jsonb_build_object('due_date', due_date, 'price_cents', price_cents, 'currency', currency, 'cycle', cycle, 'billing_tz', billing_tz,
    'remind_days', remind_days, 'traffic_gb', traffic_gb, 'traffic_reset_day', traffic_reset_day, 'traffic_rule', traffic_rule, 'auto_renew', auto_renew)
    from subscription_eff where machine_id = $1";

/// 订阅按 `subscription_eff` 算出的生效值，带 `overrides`（单独设置了哪几项）：改订阅预览的返回
const SUB_EFFECTIVE: &str = "select jsonb_build_object('due_date', due_date, 'price_cents', price_cents, 'currency', currency, 'cycle', cycle, 'billing_tz', billing_tz,
    'remind_days', remind_days, 'traffic_gb', traffic_gb, 'traffic_reset_day', traffic_reset_day, 'traffic_rule', traffic_rule, 'auto_renew', auto_renew, 'overrides', to_jsonb(overrides))
    from subscription_eff where machine_id = $1";

/// 校验、写入、规整订阅（`put_subscription` 与预览共用，都在调用方的事务里），返回改动前的生效值（审计用）
async fn write_subscription(tx: &mut sqlx::PgConnection, id: Uuid, b: &mut SubIn) -> ApiResult<Option<Value>> {
    if b.traffic_used_gb.is_some() {
        return Err(ApiError::bad_request("流量已用改为由 agent 按网卡统计，不再按面板校准"));
    }
    b.ov.check(tx).await?;
    let (template_price,): (Option<i32>,) = sqlx::query_as("select price_cents from sub_template where machine_id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    if !b.ov.has_price() && template_price.is_none() {
        return Err(ApiError::bad_request("这台机器没有套餐，价格没有可跟随的：要填费用（免费填 0）"));
    }
    let before: Option<Value> = sqlx::query_scalar(SUB_AUDIT).bind(id).fetch_optional(&mut *tx).await?;
    b.ov.write(tx, id, b.due_date).await?;
    if let Some(auto) = b.auto_renew {
        sqlx::query("update subscription set auto_renew = $2 where machine_id = $1").bind(id).bind(auto).execute(&mut *tx).await?;
    }
    Ok(before)
}

pub async fn put_subscription(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(mut b): ApiJson<SubIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let before = write_subscription(&mut tx, id, &mut b).await?;
    // 审计详情：请求本身 + 机器名 + 改动前后的生效值（摘要写出改了哪几项，见 audit::audit_text）
    let after: Option<Value> = sqlx::query_scalar(SUB_AUDIT).bind(id).fetch_optional(&mut *tx).await?;
    let name: String = sqlx::query_scalar("select display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let mut detail = serde_json::to_value(&b).unwrap_or_default();
    detail["name"] = json!(name);
    detail["before"] = json!(before);
    detail["after"] = json!(after);
    audit(&mut tx, &a.name, "subscription.set", "machine", Some(id), detail).await?;
    tx.commit().await?;
    st.emit(Event::Machine { id });
    Ok(ok(json!({"ok": true})))
}

/// 预览改订阅（`POST /machines/{id}/subscription/preview`，请求体同 PUT）：照常校验、写、规整（和模板相同的改回跟随，账单日照规则算），
/// 读出生效值后整个回滚，什么都不留（不记审计、不广播）。返回 `{dry_run: true, effective: {…生效值…, overrides: […]}}`，
/// fleetctl 用它在确认前显示「改前 → 改后」，不在客户端重算跟随规则。独立路由而不是 PUT 的 query：旧中控没有这个路由（404），
/// 不会把预览当成真改
pub async fn preview_subscription(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(mut b): ApiJson<SubIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    write_subscription(&mut tx, id, &mut b).await?;
    let effective: Option<Value> = sqlx::query_scalar(SUB_EFFECTIVE).bind(id).fetch_optional(&mut *tx).await?;
    tx.rollback().await?;
    Ok(ok(json!({"dry_run": true, "effective": effective})))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Renewal {
    id: Uuid,
    old_due: Date,
    new_due: Date,
    amount_cents: Option<i32>,
    /// 登记时订阅的币种（之后改订阅币种，这条的金额不跟着变）
    currency: String,
    note: String,
    actor: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

pub async fn list_renewals(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let v: Vec<Renewal> = sqlx::query_as(
        "select id, old_due, new_due, amount_cents, currency, note, actor, created_at from renewal where machine_id = $1 order by created_at desc",
    )
    .bind(id)
    .fetch_all(&st.db)
    .await?;
    Ok(ok(v))
}

/// 登记续费：`new_due`（直接给新到期日；不落在账单日上的，账单日改成它几号，见 meta::billing_day）与 `cycles`（从当前到期日按订阅周期推几个周期，中控算新到期日：
/// 落在账单日，小月按月末；账单日不变，所以 31 号的 1-31 → 2-28 → 3-31）二选一
#[derive(Deserialize)]
pub struct RenewIn {
    pub new_due: Option<Date>,
    pub cycles: Option<u32>,
    pub amount_cents: Option<i32>,
    pub note: Option<String>,
}

/// 一次登记续费的结果
pub struct Renewed {
    pub id: Uuid,
    pub old_due: Date,
    pub new_due: Date,
    pub currency: String,
    /// 登记的金额（分）：没给时 `price_default` 取订阅价 × 周期数，否则为 None
    pub amount_cents: Option<i32>,
}

/// 在调用方的事务里给一台登记续费（单台与批量共用）：锁住订阅、算新到期日、写续费记录、顺延到期日；审计与广播由调用方做。
/// `price_default`：没给金额时按订阅价格（生效价）× 周期数登记（批量用；单台不给金额就记空）
pub(crate) async fn renew_in_tx(tx: &mut sqlx::PgConnection, actor: &str, id: Uuid, b: &RenewIn, price_default: bool) -> ApiResult<Renewed> {
    // 币种记登记时生效的（subscription_eff：订阅没单独设置的跟随套餐）；按周期推时也用生效的周期
    let (old, currency, cycle, bday, price): (Date, String, String, i32, i32) = sqlx::query_as(
        "select s.due_date, e.currency, e.cycle::text, coalesce(s.billing_day::int, extract(day from s.due_date)::int), e.price_cents from subscription s join subscription_eff e on e.machine_id = s.machine_id
         where s.machine_id = $1 for update of s",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::conflict("这台机器还没有账单信息，先填写费用"))?;
    let (new_due, bday) = match (b.new_due, b.cycles) {
        (Some(d), None) => (d, i32::from(meta::billing_day(d, Some(bday as u8)))),
        (None, Some(n @ 1..=120)) => {
            let months = meta::cycle_months(&cycle).ok_or_else(|| ApiError::conflict(format!("不认识订阅的计费周期「{cycle}」：直接给 new_due")))?;
            let d = meta::add_months_on(old, i64::from(months) * i64::from(n), bday as u8).ok_or_else(|| ApiError::bad_request("新到期日超出范围"))?;
            (d, bday)
        }
        (None, Some(_)) => return Err(ApiError::bad_request("cycles 要在 1–120 之间")),
        _ => return Err(ApiError::bad_request("new_due（新到期日）与 cycles（推几个周期）给一个")),
    };
    if new_due <= old {
        return Err(ApiError::bad_request("新到期日必须晚于当前到期日"));
    }
    let amount = match (b.amount_cents, price_default) {
        (Some(a), _) => Some(a),
        (None, true) => Some(price.checked_mul(b.cycles.unwrap_or(1) as i32).ok_or_else(|| ApiError::bad_request("金额超出范围"))?),
        (None, false) => None,
    };
    let rid = new_id();
    sqlx::query("insert into renewal (id, machine_id, old_due, new_due, amount_cents, currency, note, actor) values ($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(rid)
        .bind(id)
        .bind(old)
        .bind(new_due)
        .bind(amount)
        .bind(&currency)
        .bind(b.note.as_deref().unwrap_or(""))
        .bind(actor)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update subscription set due_date = $2, billing_day = $3, updated_at = now() where machine_id = $1")
        .bind(id)
        .bind(new_due)
        .bind(bday as i16)
        .execute(&mut *tx)
        .await?;
    Ok(Renewed { id: rid, old_due: old, new_due, currency, amount_cents: amount })
}

pub async fn add_renewal(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<RenewIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let r = renew_in_tx(&mut tx, &a.name, id, &b, false).await?;
    audit(&mut tx, &a.name, "renewal", "machine", Some(id), json!({"old_due": r.old_due, "new_due": r.new_due, "amount_cents": b.amount_cents, "currency": r.currency, "cycles": b.cycles})).await?;
    tx.commit().await?;
    st.emit(Event::Machine { id });
    Ok(ok(json!({"id": r.id, "old_due": r.old_due, "new_due": r.new_due})))
}

// ── 重新编号 ──────────────────────────────────────────────────────────────

async fn renumber_plan(db: &mut sqlx::PgConnection, team_id: Uuid) -> ApiResult<(String, Vec<(Uuid, i32, i32)>)> {
    let (team,): (String,) = sqlx::query_as("select name from team where id = $1")
        .bind(team_id)
        .fetch_optional(&mut *db)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个团队"))?;
    let nodes: Vec<(Uuid, i32)> = sqlx::query_as(
        "select id, number from machine where team_id = $1 and kind <> 'relay' and lifecycle <> 'retired' order by number",
    )
    .bind(team_id)
    .fetch_all(&mut *db)
    .await?;
    Ok((team, compact_numbers(nodes)))
}

fn plan_token(changes: &[(Uuid, i32, i32)]) -> String {
    let mut h = Sha256::new();
    for (id, o, n) in changes {
        h.update(format!("{id}:{o}:{n};"));
    }
    hex::encode(&h.finalize()[..12])
}

pub async fn renumber_preview(_a: Actor, State(st): State<AppState>, ApiPath(team_id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut conn = st.db.acquire().await?;
    let (team, changes) = renumber_plan(&mut conn, team_id).await?;
    let list: Vec<Value> = changes
        .iter()
        .map(|(id, o, n)| json!({"machine_id": id, "old_name": node_name(&team, *o), "new_name": node_name(&team, *n)}))
        .collect();
    // 软路由的出口名是路由上自己起的（「节点05」），不跟着改：指向要改名的节点的列出来，对不上的自己去改
    let ids: Vec<Uuid> = changes.iter().map(|c| c.0).collect();
    let exits = crate::routers::exits_using(&st.db, &ids).await?;
    let notes: Vec<String> = changes
        .iter()
        .flat_map(|(id, o, n)| {
            let (from, to) = (node_name(&team, *o), node_name(&team, *n));
            exits.get(id).into_iter().flatten().map(move |(_, r, x)| format!("软路由「{r}」的出口「{x}」指向 {from}，改名为 {to} 后出口名不跟着改"))
        })
        .collect();
    Ok(ok(json!({"changes": list, "notes": notes, "token": plan_token(&changes)})))
}

#[derive(Deserialize)]
pub struct RenumberIn {
    token: String,
}

pub async fn renumber_apply(a: Actor, State(st): State<AppState>, ApiPath(team_id): ApiPath<Uuid>, ApiJson(b): ApiJson<RenumberIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    // 锁住团队，防止并发上架插号
    sqlx::query("select 1 from team where id = $1 for update").bind(team_id).fetch_one(&mut *tx).await?;
    let (_team, planned) = renumber_plan(&mut tx, team_id).await?;
    if plan_token(&planned) != b.token {
        return Err(ApiError::conflict("编号在预览之后变了，请重新预览"));
    }
    let (changes, relays) = crate::db::compact_team(&mut tx, team_id, &a.name, "手动重新编号").await?;
    tx.commit().await?;
    for r in relays {
        st.relay_bumped(r);
    }
    st.emit(Event::Machines);
    Ok(ok(json!({"changed": changes.len()})))
}
