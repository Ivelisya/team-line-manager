use crate::error::ApiJson;
use super::ok;
use crate::auth::Actor;
use crate::db::{audit, bump_relays_of_nodes, new_id};
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::jobs;
use crate::names::{compact_numbers, node_name};
use crate::state::{AppState, Event};
use crate::views::{self, Scope};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

#[derive(Deserialize)]
pub struct ListQ {
    scope: Option<String>,
}

pub async fn list(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<ListQ>) -> ApiResult<Json<Value>> {
    let scope = match q.scope.as_deref() {
        Some("retired") => Scope::Retired,
        Some("all") => Scope::All,
        _ => Scope::Live,
    };
    if scope == Scope::Live {
        return Ok(ok(&*views::live(&st).await?));
    }
    Ok(ok(views::load(&st, scope, None).await?))
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
}

pub async fn patch(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<PatchIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let cur: (i32, String, Option<Uuid>, Option<i32>, Option<String>, String) = sqlx::query_as(
        "select revision, kind::text, team_id, number, label, lifecycle::text from machine where id = $1 for update",
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
    let renamed = team_id != cur.2 || number != cur.3 || label != cur.4;

    sqlx::query(
        "update machine set team_id = $2, number = $3, label = $4,
            note = coalesce($5, note),
            provider_id = case when $6 then $7 else provider_id end,
            plan_id = case when $8 then $9 else plan_id end,
            service_ref = case when $10 then $11 else service_ref end,
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
    .execute(&mut *tx)
    .await?;
    let relays = if renamed && !is_relay { bump_relays_of_nodes(&mut tx, &[id]).await? } else { vec![] };
    audit(
        &mut tx,
        &a.name,
        if renamed { "machine.rename" } else { "machine.update" },
        "machine",
        Some(id),
        json!({"before": {"team_id": cur.2, "number": cur.3, "label": cur.4}, "after": {"team_id": team_id, "number": number, "label": label}}),
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
    // 这台机器上其它没做完或失败的作业（上架做到一半、加入口失败…）全部作废，否则之后重试会把退役的机器又拉回去
    let dead: Vec<(Uuid,)> = sqlx::query_as(
        "update job set state = 'cancelled', finished_at = now(), lease_owner = null, lease_until = null,
            params = params - 'login', error = coalesce(error, '机器进入退役，作业已作废')
         where target_id = $1 and state in ('queued', 'running', 'waiting', 'cancelling', 'failed') returning id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    for (jid,) in &dead {
        sqlx::query("insert into job_log (job_id, level, msg) values ($1, 'warn', '机器进入退役，本作业已作废（不会再重试）')").bind(jid).execute(&mut *tx).await?;
    }
    // 退役中的机器退出监控（不再评估，也不当成恢复）：未结告警现在就关（不发恢复通知）。
    // 退役作业失败会停在退役中等重试（步骤不可回滚），告警不能等到收尾才关
    let closed = sqlx::query(
        "update incident set state = 'resolved', resolved_at = now(), detail = detail || '{\"closed_by\":\"retire\"}'
         where state = 'firing' and subject_id = $1",
    )
    .bind(id)
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
        json!({"name": name, "reason": b.reason, "job_id": job, "cancelled_jobs": dead.iter().map(|(j,)| j).collect::<Vec<_>>()}),
    )
    .await?;
    tx.commit().await?;
    // 在用 → 退役中：列表要变
    st.emit(Event::Machines);
    st.emit(Event::Job { id: job, target: Some(id) });
    if closed > 0 {
        st.emit(Event::Incidents);
    }
    for (j,) in dead {
        st.emit(Event::Job { id: j, target: Some(id) });
    }
    Ok(ok(json!({"job_id": job})))
}

/// 退役预览：同团队里会因压缩而改名的节点
pub async fn retire_preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let (name, _kind, _lc) = display_name_of(&st.db, id).await?;
    let row: Option<(Option<Uuid>, Option<String>)> =
        sqlx::query_as("select m.team_id, t.name from machine m left join team t on t.id = m.team_id where m.id = $1")
            .bind(id)
            .fetch_optional(&st.db)
            .await?;
    let mut renames = vec![];
    if let (true, Some((Some(team_id), Some(team)))) = (st.cfg.numbering.auto_compact, row) {
        let nodes: Vec<(Uuid, i32)> = sqlx::query_as(
            "select id, number from machine where team_id = $1 and kind <> 'relay' and lifecycle <> 'retired' and id <> $2 order by number",
        )
        .bind(team_id)
        .bind(id)
        .fetch_all(&st.db)
        .await?;
        renames = compact_numbers(nodes)
            .into_iter()
            .map(|(mid, o, n)| json!({"machine_id": mid, "old_name": node_name(&team, o), "new_name": node_name(&team, n)}))
            .collect();
    }
    Ok(ok(json!({"name": name, "renames": renames})))
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
    st.emit(Event::Job { id: job, target: Some(id) });
    Ok(ok(json!({"job_id": job})))
}

// ── agent 升级 ────────────────────────────────────────────────────────────

/// 返回 (建好的作业, 跳过的机器)。每台一个 savepoint：一台冲突（期间别人建了作业）只跳过这台，不整批回滚。
async fn enqueue_upgrade(st: &AppState, actor: &str, ids: &[Uuid]) -> ApiResult<(Vec<Value>, Vec<Value>)> {
    let mut tx = st.db.begin().await?;
    let (mut out, mut skipped) = (vec![], vec![]);
    for id in ids {
        // 已有进行中作业的机器跳过（唯一索引也会挡住）
        let busy: Option<(Uuid,)> = sqlx::query_as("select id from job where target_id = $1 and state in ('queued','running','waiting','cancelling')")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        if busy.is_some() {
            skipped.push(json!({"machine_id": id, "reason": "还有进行中的作业"}));
            continue;
        }
        let mut sp = sqlx::Connection::begin(&mut *tx).await?;
        match jobs::enqueue(&mut sp, "upgrade_agent", Some(*id), json!({}), actor).await {
            Ok(job) => {
                sp.commit().await?;
                out.push(json!({"machine_id": id, "job_id": job}));
            }
            Err(e) if e.status == StatusCode::CONFLICT => {
                sp.rollback().await?;
                skipped.push(json!({"machine_id": id, "reason": e.message}));
            }
            Err(e) => return Err(e),
        }
    }
    audit(&mut tx, actor, "agent.upgrade", "machine", None, json!({"count": out.len(), "skipped": skipped.len()})).await?;
    tx.commit().await?;
    for j in &out {
        let uuid = |k: &str| j[k].as_str().and_then(|s| Uuid::try_parse(s).ok());
        if let Some(id) = uuid("job_id") {
            st.emit(Event::Job { id, target: uuid("machine_id") });
        }
    }
    Ok((out, skipped))
}

/// 单台：推送中控上当前版本的 agent 并重启（令牌顺带轮换）
pub async fn upgrade_agent(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let (kind, lc): (String, String) = sqlx::query_as("select kind::text, lifecycle::text from machine where id = $1")
        .bind(id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    if kind == "proxy" || lc != "active" {
        return Err(ApiError::conflict("只有在用的线路机和落地机有 agent"));
    }
    let (v, _) = enqueue_upgrade(&st, &a.name, &[id]).await?;
    v.into_iter().next().map(ok).ok_or_else(|| ApiError::conflict("这台机器还有进行中的作业"))
}

/// 全部在用的线路机与落地机
pub async fn upgrade_all_agents(a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let ids: Vec<(Uuid,)> = sqlx::query_as("select id from machine where kind <> 'proxy' and lifecycle = 'active' order by kind = 'relay' desc")
        .fetch_all(&st.db)
        .await?;
    let ids: Vec<Uuid> = ids.into_iter().map(|(i,)| i).collect();
    let (jobs, skipped) = enqueue_upgrade(&st, &a.name, &ids).await?;
    Ok(ok(json!({"jobs": jobs, "skipped": skipped})))
}

// ── 账单 ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, Serialize)]
pub struct SubIn {
    price_cents: i32,
    #[serde(default = "usd")]
    currency: String,
    #[serde(default = "monthly")]
    cycle: String,
    due_date: Date,
    #[serde(default = "sh")]
    billing_tz: String,
    #[serde(default)]
    auto_renew: bool,
    #[serde(default = "seven")]
    remind_days: i32,
    traffic_gb: Option<i32>,
    traffic_reset_day: Option<i32>,
}
fn usd() -> String { "USD".into() }
fn monthly() -> String { "monthly".into() }
fn sh() -> String { "Asia/Shanghai".into() }
fn seven() -> i32 { 7 }

/// 时区（账单时区、供应商时区）必须能被 `at time zone` 接受：views 里的 local_date 就这么用，不再兜底。
/// 不认识的时区 PG 报 22023（invalid_parameter_value）
pub async fn check_tz(db: &mut sqlx::PgConnection, tz: &str) -> ApiResult<()> {
    match sqlx::query("select now() at time zone $1").bind(tz).execute(db).await {
        Err(e) if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("22023") => {
            Err(ApiError::bad_request(format!("账单时区「{tz}」不存在：请填 IANA 时区名，如 Asia/Shanghai")))
        }
        r => r.map(drop).map_err(Into::into),
    }
}

pub async fn put_subscription(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<SubIn>) -> ApiResult<Json<Value>> {
    if b.price_cents < 0 || !(0..=60).contains(&b.remind_days) {
        return Err(ApiError::bad_request("金额或提醒天数不合法"));
    }
    if b.traffic_reset_day.is_some_and(|d| !(1..=31).contains(&d)) {
        return Err(ApiError::bad_request("流量重置日要在 1–31 之间"));
    }
    let mut tx = st.db.begin().await?;
    check_tz(&mut tx, &b.billing_tz).await?;
    sqlx::query(
        "insert into subscription (machine_id, price_cents, currency, cycle, due_date, billing_tz, auto_renew, remind_days, traffic_gb, traffic_reset_day)
         values ($1,$2,$3,$4::billing_cycle,$5,$6,$7,$8,$9,$10)
         on conflict (machine_id) do update set price_cents = excluded.price_cents, currency = excluded.currency, cycle = excluded.cycle,
           due_date = excluded.due_date, billing_tz = excluded.billing_tz, auto_renew = excluded.auto_renew,
           remind_days = excluded.remind_days, traffic_gb = excluded.traffic_gb, traffic_reset_day = excluded.traffic_reset_day, updated_at = now()",
    )
    .bind(id)
    .bind(b.price_cents)
    .bind(&b.currency)
    .bind(&b.cycle)
    .bind(b.due_date)
    .bind(&b.billing_tz)
    .bind(b.auto_renew)
    .bind(b.remind_days)
    .bind(b.traffic_gb)
    .bind(b.traffic_reset_day)
    .execute(&mut *tx)
    .await?;
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
    note: String,
    actor: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

pub async fn list_renewals(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let v: Vec<Renewal> = sqlx::query_as(
        "select id, old_due, new_due, amount_cents, note, actor, created_at from renewal where machine_id = $1 order by created_at desc",
    )
    .bind(id)
    .fetch_all(&st.db)
    .await?;
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct RenewIn {
    new_due: Date,
    amount_cents: Option<i32>,
    note: Option<String>,
}

pub async fn add_renewal(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<RenewIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let (old,): (Date,) = sqlx::query_as("select due_date from subscription where machine_id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::conflict("这台机器还没有账单信息，先填写费用"))?;
    if b.new_due <= old {
        return Err(ApiError::bad_request("新到期日必须晚于当前到期日"));
    }
    let rid = new_id();
    sqlx::query("insert into renewal (id, machine_id, old_due, new_due, amount_cents, note, actor) values ($1,$2,$3,$4,$5,$6,$7)")
        .bind(rid)
        .bind(id)
        .bind(old)
        .bind(b.new_due)
        .bind(b.amount_cents)
        .bind(b.note.as_deref().unwrap_or(""))
        .bind(&a.name)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update subscription set due_date = $2, updated_at = now() where machine_id = $1")
        .bind(id)
        .bind(b.new_due)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &a.name, "renewal", "machine", Some(id), json!({"old_due": old, "new_due": b.new_due, "amount_cents": b.amount_cents})).await?;
    tx.commit().await?;
    st.emit(Event::Machine { id });
    Ok(ok(json!({"id": rid})))
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
