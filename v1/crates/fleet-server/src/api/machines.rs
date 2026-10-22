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
use time::{Date, OffsetDateTime};
use uuid::Uuid;

#[derive(Deserialize)]
pub struct ListQ {
    scope: Option<String>,
    /// `brief`：只给按名字找机器要用的字段（见 `views::brief`）
    view: Option<String>,
}

pub async fn list(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<ListQ>) -> ApiResult<Response> {
    let scope = match q.scope.as_deref() {
        Some("retired") => Scope::Retired,
        Some("all") => Scope::All,
        _ => Scope::Live,
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

/// PATCH 前锁住的当前值：(revision, kind, team_id, number, label, lifecycle, country)
type PatchCur = (i32, String, Option<Uuid>, Option<i32>, Option<String>, String, Option<String>);

pub async fn patch(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<PatchIn>) -> ApiResult<Json<Value>> {
    let country = match &b.country {
        Some(c) => Some(crate::country::normalize(c.as_deref().unwrap_or("")).map_err(ApiError::bad_request)?),
        None => None,
    };
    let mut tx = st.db.begin().await?;
    let cur: PatchCur = sqlx::query_as(
        "select revision, kind::text, team_id, number, label, lifecycle::text, country from machine where id = $1 for update",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
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
    let number = if is_relay { None } else { b.number.or(cur.3) };
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
    .execute(&mut *tx)
    .await?;
    // 有套餐时供应商就是套餐的：只换了套餐的跟着换供应商，明确给了另一个供应商的报错
    let other: Option<(Uuid,)> = sqlx::query_as(
        "select pl.provider_id from machine m join plan pl on pl.id = m.plan_id where m.id = $1 and m.provider_id is distinct from pl.provider_id",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((pv,)) = other {
        if b.provider_id.is_some() {
            return Err(ApiError::bad_request("套餐不属于所选的供应商"));
        }
        sqlx::query("update machine set provider_id = $2 where id = $1").bind(id).bind(pv).execute(&mut *tx).await?;
    }
    // 入口备注随节点名变：节点改名刷新它挂的线路机；线路机改名只影响它自己的直出入口（「线路机名-直出」），没有就不动
    let relays = if renamed { bump_relays_of_nodes(&mut tx, &[id]).await? } else { vec![] };
    // 审计写上改前改后的显示名（「团队1-07 → 团队1-03」）：记录保持当时的样子，之后再改名也不跟着变
    let (old_team, new_team): (Option<String>, Option<String>) = sqlx::query_as("select (select name from team where id = $1), (select name from team where id = $2)")
        .bind(cur.2)
        .bind(team_id)
        .fetch_one(&mut *tx)
        .await?;
    let old_name = crate::names::display_name(&cur.1, cur.4.as_deref(), old_team.as_deref(), cur.3);
    let name = crate::names::display_name(&cur.1, label.as_deref(), new_team.as_deref(), number);
    audit(
        &mut tx,
        &a.name,
        if renamed { "machine.rename" } else { "machine.update" },
        "machine",
        Some(id),
        json!({"name": name, "old_name": old_name,
               "before": {"team_id": cur.2, "number": cur.3, "label": cur.4, "country": cur.6},
               "after": {"team_id": team_id, "number": number, "label": label, "country": country.clone().unwrap_or(cur.6.clone())}}),
    )
    .await?;
    tx.commit().await?;
    for r in relays {
        st.relay_bumped(r);
    }
    st.emit(Event::Machine { id });
    get_one(a, State(st), ApiPath(id)).await
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

// ── 退役与彻底删除 ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RetireIn {
    confirm: String,
    reason: Option<String>,
    cleanup: Option<bool>,
}

pub async fn retire(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<RetireIn>) -> ApiResult<Json<Value>> {
    let (name, kind, _) = display_name_of(&st.db, id).await?;
    if b.confirm.trim() != name {
        return Err(ApiError::bad_request(format!("确认文字不对：请输入「{name}」")));
    }
    // 退役原因同备注的上限（写进机器的状态原因）
    check_text(None, b.reason.as_deref()).map_err(|_| ApiError::bad_request(format!("退役原因最多 {MAX_NOTE_CHARS} 个字符")))?;
    let mut tx = st.db.begin().await?;
    // 先锁机器行再判断、再计数：和「给线路机加入口」（对同一行 for share）互斥，守卫不会被并发挤穿
    let (lifecycle,): (String,) = sqlx::query_as("select lifecycle::text from machine where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    if lifecycle == "retired" {
        return Err(ApiError::conflict("已经退役"));
    }
    if lifecycle == "retiring" {
        return Err(ApiError::conflict("这台机器已经在退役中：到作业页看进度，失败了点重试"));
    }
    if kind == "relay" {
        let (n,): (i64,) = sqlx::query_as(
            "select count(*) from entry e join machine m on m.id = e.node_id
             where e.relay_id = $1 and e.lifecycle <> 'removed' and m.id <> $1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if n > 0 {
            return Err(ApiError::conflict(format!("这台线路机上还有 {n} 个节点入口：先把节点迁走或退役。可先用「搬空线路机」把节点换到其他线路机")));
        }
    }
    // 宿主机退役：它上面的同机 IP 节点一并退役（它们的 SSH、agent、ssserver 都在这台机器上）
    let ip_nodes = ip_nodes_of(&mut tx, id, true).await?;
    let ip_ids: Vec<Uuid> = ip_nodes.iter().map(|x| x.0).collect();
    let all: Vec<Uuid> = std::iter::once(id).chain(ip_ids.iter().copied()).collect();
    // 有作业正在执行一步（租约未过期）：库里标成作废拦不住它，那一步还会接着跑（装 agent、签令牌…），和退役的清理撞在一起。
    // 先锁住这些机器上没结束的作业（领取跳过锁住的行），再看租约
    let open: Vec<(String, bool)> = sqlx::query_as(
        "select kind, state in ('running', 'cancelling') and lease_until > now() from job
         where target_id = any($1) and state in ('queued', 'running', 'waiting', 'cancelling') for update",
    )
    .bind(&all)
    .fetch_all(&mut *tx)
    .await?;
    if let Some((k, _)) = open.iter().find(|j| j.1) {
        return Err(ApiError::conflict(format!(
            "有作业（{}）正在执行一步：先在作业页取消它，等它停下再退役（升级 agent 不能取消，等它做完）",
            meta::job_kind_label(k)
        )));
    }
    let n = sqlx::query(
        "update machine set lifecycle = 'retiring', lifecycle_reason = $2, revision = revision + 1, updated_at = now()
         where id = $1 and lifecycle not in ('retiring', 'retired')",
    )
    .bind(id)
    .bind(&b.reason)
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
    let job = jobs::enqueue(
        &mut tx,
        "retire",
        Some(id),
        json!({"cleanup": b.cleanup.unwrap_or(true), "reason": b.reason, "prev_lifecycle": lifecycle}),
        &a.name,
    )
    .await?;
    audit(
        &mut tx,
        &a.name,
        "machine.retire",
        "machine",
        Some(id),
        json!({"name": name, "reason": b.reason, "job_id": job, "cancelled_jobs": dead.iter().map(|(j, _)| j).collect::<Vec<_>>(),
               "ip_nodes": ip_nodes.iter().map(|x| &x.1).collect::<Vec<_>>()}),
    )
    .await?;
    // 同机 IP 节点各自一个退役作业：只摘自己的入口、不登录清理（宿主机的清理连同它们的实例一起删）
    let mut ip_jobs = vec![];
    for (xid, xname, xlc) in &ip_nodes {
        let j = jobs::enqueue(&mut tx, "retire", Some(*xid), json!({"cleanup": false, "reason": ip_reason, "prev_lifecycle": xlc}), &a.name).await?;
        audit(&mut tx, &a.name, "machine.retire", "machine", Some(*xid), json!({"name": xname, "reason": ip_reason, "job_id": j, "host": name})).await?;
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
    Ok(ok(json!({"job_id": job, "ip_node_jobs": ip_jobs})))
}

/// 宿主机上还没开始退役的同机 IP 节点：(id, 显示名, 生命周期)，按团队、编号排。`lock`：锁住这些机器行（退役时）
async fn ip_nodes_of(conn: &mut sqlx::PgConnection, host: Uuid, lock: bool) -> sqlx::Result<Vec<(Uuid, String, String)>> {
    sqlx::query_as(&format!(
        "select m.id, display_name(m.kind::text, m.label, t.name, m.number), m.lifecycle::text from machine m left join team t on t.id = m.team_id
         where m.host_id = $1 and m.lifecycle not in ('retiring', 'retired') order by t.sort_order, t.name, m.number{}",
        if lock { " for update of m" } else { "" }
    ))
    .bind(host)
    .fetch_all(conn)
    .await
}

/// 退役预览：会一并退役的同机 IP 节点（宿主机才有），以及涉及的团队里会因压缩而改名的节点
pub async fn retire_preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let (name, _kind, _lc) = display_name_of(&st.db, id).await?;
    let mut conn = st.db.acquire().await?;
    let ip_nodes = ip_nodes_of(&mut conn, id, false).await?;
    let leaving: Vec<Uuid> = std::iter::once(id).chain(ip_nodes.iter().map(|x| x.0)).collect();
    let mut renames = vec![];
    if st.cfg.numbering.auto_compact {
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
    }
    let ip_nodes: Vec<Value> = ip_nodes.iter().map(|(i, n, _)| json!({"id": i, "name": n})).collect();
    Ok(ok(json!({"name": name, "renames": renames, "ip_nodes": ip_nodes})))
}

#[derive(Deserialize)]
pub struct PurgeIn {
    confirm: String,
}

pub async fn purge(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<PurgeIn>) -> ApiResult<Json<Value>> {
    let (name, _kind, lifecycle) = display_name_of(&st.db, id).await?;
    if lifecycle != "retired" {
        return Err(ApiError::conflict("只有已退役的机器才能彻底删除"));
    }
    if b.confirm.trim() != name {
        return Err(ApiError::bad_request(format!("确认文字不对：请输入「{name}」")));
    }
    if let Some(why) = crate::db::purge_blocker(&st.db, id).await? {
        return Err(ApiError::conflict(why));
    }
    let mut tx = st.db.begin().await?;
    let job = jobs::enqueue(&mut tx, "purge", Some(id), json!({"name": name}), &a.name).await?;
    audit(&mut tx, &a.name, "machine.purge", "machine", Some(id), json!({"name": name, "job_id": job})).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    Ok(ok(json!({"job_id": job})))
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
    audit(&mut tx, actor, "agent.upgrade", "machine", None, json!({"count": out.len(), "skipped": skipped.len()})).await?;
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

/// 单台：推送中控上当前版本的 agent 并重启（令牌顺带轮换）；新版本起不来、没上报就换回旧版本（见 jobs::steps）
pub async fn upgrade_agent(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let (kind, lc, name, host): (String, String, String, Option<String>) = sqlx::query_as(
        "select m.kind::text, m.lifecycle::text, display_name(m.kind::text, m.label, mt.name, m.number), display_name(h.kind::text, h.label, t.name, h.number)
         from machine m left join team mt on mt.id = m.team_id left join machine h on h.id = m.host_id left join team t on t.id = h.team_id
         where m.id = $1",
    )
    .bind(id)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    if let Some(h) = host {
        return Err(ApiError::conflict(format!("同机 IP 节点没有自己的 agent，用的是宿主机「{h}」的：升级宿主机的 agent")));
    }
    if kind == "proxy" || lc != "active" {
        return Err(ApiError::conflict("只有在用的线路机和 VPS 节点有 agent"));
    }
    let (v, skipped) = enqueue_upgrade(&st, &a.name, &[(id, kind, name)], None).await?;
    v.into_iter().next().map(ok).ok_or_else(|| ApiError::conflict(skipped[0]["reason"].as_str().unwrap_or("这台机器还有进行中的作业")))
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
/// 流量重置日跟随供应商，见 views::SubTemplate）；和模板相同的值也按跟随存。上架与改订阅共用
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
}

impl SubOverrides {
    pub fn has_price(&self) -> bool { self.price_cents.is_some() }

    /// 校验并规整（币种转大写）：价格、流量上限不为负，周期四选一，提醒 0–60 天，重置日 1–31，时区 at time zone 认
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
        if let Some(c) = &mut self.currency {
            // 同套餐的币种规则（比较「和套餐相同」时口径一致）
            *c = meta::currency(c).map_err(ApiError::bad_request)?;
        }
        if let Some(tz) = &self.billing_tz {
            check_tz(db, tz).await?;
        }
        Ok(())
    }

    /// 写订阅：到期日 + 这些单独设置（整份替换；自动续费、按面板校准不动），再把和模板相同的改回跟随。
    /// 账单日（billing_day）按 meta::billing_day：到期日落在现有账单日上（含原样带回的到期日、31 号账单日的 2-28）不动，否则改成新到期日几号
    pub async fn write(&self, db: &mut sqlx::PgConnection, id: Uuid, due: Date) -> ApiResult<()> {
        let cur: Option<i16> = sqlx::query_scalar("select billing_day from subscription where machine_id = $1").bind(id).fetch_optional(&mut *db).await?.flatten();
        let bday = meta::billing_day(due, cur.and_then(|d| u8::try_from(d).ok()));
        sqlx::query(
            "insert into subscription (machine_id, due_date, price_cents, currency, cycle, billing_tz, remind_days, traffic_gb, traffic_reset_day, billing_day)
             values ($1, $2, $3, $4, $5::billing_cycle, $6, $7, $8, $9, $10)
             on conflict (machine_id) do update set due_date = excluded.due_date, price_cents = excluded.price_cents, currency = excluded.currency,
               cycle = excluded.cycle, billing_tz = excluded.billing_tz, remind_days = excluded.remind_days, traffic_gb = excluded.traffic_gb,
               traffic_reset_day = excluded.traffic_reset_day, billing_day = excluded.billing_day, updated_at = now()",
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
        .bind(i16::from(bday))
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
    /// 按供应商面板校准本周期已用（十进制 GB，可带小数）：记下这个数和现在的时间。没传 = 不改，null = 清掉
    #[serde(default, deserialize_with = "super::some", skip_serializing_if = "Option::is_none")]
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

pub async fn put_subscription(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(mut b): ApiJson<SubIn>) -> ApiResult<Json<Value>> {
    // 校准值：GB → 字节（1 GB = 10⁹ 字节）
    let base = match b.traffic_used_gb {
        Some(Some(g)) if !(g.is_finite() && (0.0..1e9).contains(&g)) => return Err(ApiError::bad_request("本周期已用要填 0 或正数（GB）")),
        Some(Some(g)) => Some(Some((g * 1e9).round() as i64)),
        Some(None) => Some(None),
        None => None,
    };
    let mut tx = st.db.begin().await?;
    b.ov.check(&mut tx).await?;
    let (template_price,): (Option<i32>,) = sqlx::query_as("select price_cents from sub_template where machine_id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    if !b.ov.has_price() && template_price.is_none() {
        return Err(ApiError::bad_request("这台机器没有套餐，价格没有可跟随的：要填费用（免费填 0）"));
    }
    b.ov.write(&mut tx, id, b.due_date).await?;
    if let Some(auto) = b.auto_renew {
        sqlx::query("update subscription set auto_renew = $2 where machine_id = $1").bind(id).bind(auto).execute(&mut *tx).await?;
    }
    if let Some(base) = base {
        sqlx::query(
            "update subscription set traffic_used_base = $2, traffic_used_base_at = case when $2::bigint is null then null else now() end where machine_id = $1",
        )
        .bind(id)
        .bind(base)
        .execute(&mut *tx)
        .await?;
    }
    audit(&mut tx, &a.name, "subscription.set", "machine", Some(id), serde_json::to_value(&b).unwrap_or_default()).await?;
    tx.commit().await?;
    st.emit(Event::Machine { id });
    Ok(ok(json!({"ok": true})))
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
    new_due: Option<Date>,
    cycles: Option<u32>,
    amount_cents: Option<i32>,
    note: Option<String>,
}

pub async fn add_renewal(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<RenewIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    // 币种记登记时生效的（subscription_eff：订阅没单独设置的跟随套餐）；按周期推时也用生效的周期
    let (old, currency, cycle, bday): (Date, String, String, i32) = sqlx::query_as(
        "select s.due_date, e.currency, e.cycle::text, coalesce(s.billing_day::int, extract(day from s.due_date)::int) from subscription s join subscription_eff e on e.machine_id = s.machine_id
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
    let rid = new_id();
    sqlx::query("insert into renewal (id, machine_id, old_due, new_due, amount_cents, currency, note, actor) values ($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(rid)
        .bind(id)
        .bind(old)
        .bind(new_due)
        .bind(b.amount_cents)
        .bind(&currency)
        .bind(b.note.as_deref().unwrap_or(""))
        .bind(&a.name)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update subscription set due_date = $2, billing_day = $3, updated_at = now() where machine_id = $1")
        .bind(id)
        .bind(new_due)
        .bind(bday as i16)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &a.name, "renewal", "machine", Some(id), json!({"old_due": old, "new_due": new_due, "amount_cents": b.amount_cents, "currency": currency, "cycles": b.cycles})).await?;
    tx.commit().await?;
    st.emit(Event::Machine { id });
    Ok(ok(json!({"id": rid, "old_due": old, "new_due": new_due})))
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
    Ok(ok(json!({"changes": list, "token": plan_token(&changes)})))
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
