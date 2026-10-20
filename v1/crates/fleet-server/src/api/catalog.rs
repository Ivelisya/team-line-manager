use crate::error::ApiJson;
use super::ok;
use crate::auth::Actor;
use crate::db::{audit, new_id};
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Event};
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

// ── 团队 ──────────────────────────────────────────────────────────────────

#[derive(Serialize, sqlx::FromRow)]
pub struct Team {
    id: Uuid,
    name: String,
    sort_order: i32,
    node_count: i64,
}

pub async fn list_teams(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let v: Vec<Team> = sqlx::query_as(
        "select t.id, t.name, t.sort_order,
                (select count(*) from machine m where m.team_id = t.id and m.lifecycle <> 'retired') node_count
         from team t order by t.sort_order, t.name",
    )
    .fetch_all(&st.db)
    .await?;
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct TeamIn {
    name: Option<String>,
    sort_order: Option<i32>,
}

fn clean_name(s: &str, what: &str) -> ApiResult<String> {
    let s = s.trim();
    if s.is_empty() || s.chars().count() > 32 {
        return Err(ApiError::bad_request(format!("{what}不能为空且不超过 32 个字")));
    }
    Ok(s.to_string())
}

pub async fn create_team(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<TeamIn>) -> ApiResult<Json<Value>> {
    let name = clean_name(b.name.as_deref().unwrap_or(""), "团队名")?;
    let id = new_id();
    let mut tx = st.db.begin().await?;
    sqlx::query("insert into team (id, name, sort_order) values ($1, $2, $3)")
        .bind(id)
        .bind(&name)
        .bind(b.sort_order.unwrap_or(0))
        .execute(&mut *tx)
        .await
        .map_err(|e| match ApiError::from(e) {
            x if x.code == "conflict" => ApiError::conflict("已有同名团队"),
            x => x,
        })?;
    audit(&mut tx, &a.name, "team.create", "team", Some(id), json!({"name": name})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}

pub async fn patch_team(a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>, ApiJson(b): ApiJson<TeamIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let old: (String, i32) = sqlx::query_as("select name, sort_order from team where id = $1 for update")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let name = match &b.name {
        Some(n) => clean_name(n, "团队名")?,
        None => old.0.clone(),
    };
    sqlx::query("update team set name = $2, sort_order = $3 where id = $1")
        .bind(id)
        .bind(&name)
        .bind(b.sort_order.unwrap_or(old.1))
        .execute(&mut *tx)
        .await?;
    // 团队改名 → 该团队所有节点显示名变化 → 线路机备注同步
    let mut relays = vec![];
    if name != old.0 {
        let nodes: Vec<(Uuid,)> = sqlx::query_as("select id from machine where team_id = $1 and lifecycle <> 'retired'")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
        for (n,) in nodes {
            relays.extend(crate::db::bump_relays_of_node(&mut tx, n).await?);
        }
    }
    audit(&mut tx, &a.name, "team.update", "team", Some(id), json!({"before": {"name": old.0, "sort_order": old.1}, "after": {"name": name}})).await?;
    tx.commit().await?;
    relays.sort();
    relays.dedup();
    for r in relays {
        st.relay_bumped(r);
    }
    st.emit(Event::Catalog);
    st.emit(Event::Machines);
    Ok(ok(json!({"id": id})))
}

// ── 供应商 ────────────────────────────────────────────────────────────────

#[derive(Serialize, sqlx::FromRow)]
pub struct Provider {
    id: Uuid,
    name: String,
    code: String,
    panel_url: Option<String>,
    timezone: String,
}

pub async fn list_providers(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let v: Vec<Provider> = sqlx::query_as("select id, name, code, panel_url, timezone from provider order by name")
        .fetch_all(&st.db)
        .await?;
    Ok(ok(v))
}

/// 新建与 PATCH 共用：没传 = 不改；`panel_url` 传 null / 空串 = 清空；`timezone` null / 空串 = 列默认值（UTC）
#[derive(Deserialize)]
pub struct ProviderIn {
    name: Option<String>,
    code: Option<String>,
    #[serde(default, deserialize_with = "super::some")]
    panel_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::some")]
    timezone: Option<Option<String>>,
}

/// 空白当没填
fn blank(v: &Option<String>) -> Option<&str> { v.as_deref().map(str::trim).filter(|s| !s.is_empty()) }

/// 名称 / 缩写撞了唯一约束
fn provider_conflict(e: sqlx::Error) -> ApiError {
    let c = e.as_database_error().and_then(|d| d.constraint()).map(String::from);
    match (ApiError::from(e), c.as_deref()) {
        (x, Some("provider_name_key")) if x.code == "conflict" => ApiError::conflict("已有同名供应商"),
        (x, Some("provider_code_key")) if x.code == "conflict" => ApiError::conflict("已有同缩写的供应商"),
        (x, _) => x,
    }
}

pub async fn create_provider(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<ProviderIn>) -> ApiResult<Json<Value>> {
    let id = new_id();
    let name = clean_name(b.name.as_deref().unwrap_or(""), "供应商名")?;
    let code = clean_name(b.code.as_deref().unwrap_or(""), "供应商缩写")?;
    let tz = b.timezone.as_ref().and_then(blank);
    let mut tx = st.db.begin().await?;
    // 上架时供应商时区会被抄进账单时区，按同一标准校验
    if let Some(tz) = tz {
        super::machines::check_tz(&mut tx, tz).await?;
    }
    sqlx::query("insert into provider (id, name, code, panel_url) values ($1, $2, $3, $4)")
        .bind(id)
        .bind(&name)
        .bind(&code)
        .bind(b.panel_url.as_ref().and_then(blank))
        .execute(&mut *tx)
        .await
        .map_err(provider_conflict)?;
    if let Some(tz) = tz {
        sqlx::query("update provider set timezone = $2 where id = $1").bind(id).bind(tz).execute(&mut *tx).await?;
    }
    audit(&mut tx, &a.name, "provider.create", "provider", Some(id), json!({"name": name, "code": code})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}

pub async fn patch_provider(a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>, ApiJson(b): ApiJson<ProviderIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let old: Provider = sqlx::query_as("select id, name, code, panel_url, timezone from provider where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个供应商"))?;
    let name = match &b.name {
        Some(n) => clean_name(n, "供应商名")?,
        None => old.name.clone(),
    };
    let code = match &b.code {
        Some(c) => clean_name(c, "供应商缩写")?,
        None => old.code.clone(),
    };
    let panel_url = match &b.panel_url {
        Some(u) => blank(u).map(String::from),
        None => old.panel_url.clone(),
    };
    let tz = match &b.timezone {
        Some(t) => blank(t).map(String::from),
        None => Some(old.timezone.clone()),
    };
    if let Some(tz) = &tz {
        super::machines::check_tz(&mut tx, tz).await?;
    }
    sqlx::query("update provider set name = $2, code = $3, panel_url = $4 where id = $1")
        .bind(id)
        .bind(&name)
        .bind(&code)
        .bind(&panel_url)
        .execute(&mut *tx)
        .await
        .map_err(provider_conflict)?;
    // 时区清空 = 回到列默认值；已上架机器的账单时区是当时抄过去的，这里不跟着改
    let (tz,): (String,) = match &tz {
        Some(tz) => sqlx::query_as("update provider set timezone = $2 where id = $1 returning timezone").bind(id).bind(tz),
        None => sqlx::query_as("update provider set timezone = default where id = $1 returning timezone").bind(id),
    }
    .fetch_one(&mut *tx)
    .await?;
    audit(&mut tx, &a.name, "provider.update", "provider", Some(id),
        json!({"before": {"name": old.name, "code": old.code, "panel_url": old.panel_url, "timezone": old.timezone},
               "after": {"name": name, "code": code, "panel_url": panel_url, "timezone": tz}})).await?;
    tx.commit().await?;
    // 机器视图里的供应商名 / 缩写：Catalog 事件已让视图缓存和前端机器列表作废
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}

// ── 套餐 ──────────────────────────────────────────────────────────────────

#[derive(Serialize, sqlx::FromRow)]
pub struct Plan {
    id: Uuid,
    provider_id: Uuid,
    name: String,
    cpu_cores: Option<i32>,
    mem_mb: Option<i32>,
    disk_gb: Option<i32>,
    bandwidth_mbps: Option<i32>,
    traffic_gb: Option<i32>,
    price_cents: i32,
    currency: String,
    cycle: String,
    notes: String,
    #[serde(with = "time::serde::rfc3339::option")]
    archived_at: Option<OffsetDateTime>,
    machine_count: i64,
}

pub async fn list_plans(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let v: Vec<Plan> = sqlx::query_as(
        "select p.id, p.provider_id, p.name, p.cpu_cores, p.mem_mb, p.disk_gb, p.bandwidth_mbps, p.traffic_gb,
                p.price_cents, p.currency, p.cycle::text cycle, p.notes, p.archived_at,
                (select count(*) from machine m where m.plan_id = p.id and m.lifecycle <> 'retired') machine_count
         from plan p order by p.archived_at nulls first, p.name",
    )
    .fetch_all(&st.db)
    .await?;
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct PlanIn {
    provider_id: Option<Uuid>,
    name: Option<String>,
    #[serde(default, deserialize_with = "super::some")]
    cpu_cores: Option<Option<i32>>,
    #[serde(default, deserialize_with = "super::some")]
    mem_mb: Option<Option<i32>>,
    #[serde(default, deserialize_with = "super::some")]
    disk_gb: Option<Option<i32>>,
    #[serde(default, deserialize_with = "super::some")]
    bandwidth_mbps: Option<Option<i32>>,
    #[serde(default, deserialize_with = "super::some")]
    traffic_gb: Option<Option<i32>>,
    price_cents: Option<i32>,
    currency: Option<String>,
    cycle: Option<String>,
    notes: Option<String>,
    archived: Option<bool>,
}

pub async fn create_plan(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<PlanIn>) -> ApiResult<Json<Value>> {
    let id = new_id();
    let provider = b.provider_id.ok_or_else(|| ApiError::bad_request("缺少 provider_id"))?;
    let name = clean_name(b.name.as_deref().unwrap_or(""), "套餐名")?;
    let price = b.price_cents.ok_or_else(|| ApiError::bad_request("缺少 price_cents"))?;
    let mut tx = st.db.begin().await?;
    sqlx::query(
        "insert into plan (id, provider_id, name, cpu_cores, mem_mb, disk_gb, bandwidth_mbps, traffic_gb, price_cents, currency, cycle, notes)
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11::billing_cycle,$12)",
    )
    .bind(id)
    .bind(provider)
    .bind(&name)
    .bind(b.cpu_cores.flatten())
    .bind(b.mem_mb.flatten())
    .bind(b.disk_gb.flatten())
    .bind(b.bandwidth_mbps.flatten())
    .bind(b.traffic_gb.flatten())
    .bind(price)
    .bind(b.currency.as_deref().unwrap_or("USD"))
    .bind(b.cycle.as_deref().unwrap_or("monthly"))
    .bind(b.notes.as_deref().unwrap_or(""))
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, &a.name, "plan.create", "plan", Some(id), json!({"name": name, "price_cents": price})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}

pub async fn patch_plan(a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>, ApiJson(b): ApiJson<PlanIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    sqlx::query("select 1 from plan where id = $1 for update").bind(id).fetch_one(&mut *tx).await?;
    // 逐字段更新：Option<Option<T>> 区分「没传」与「设为空」
    macro_rules! set {
        ($col:literal, $v:expr) => {
            if let Some(v) = $v {
                sqlx::query(concat!("update plan set ", $col, " = $2 where id = $1")).bind(id).bind(v).execute(&mut *tx).await?;
            }
        };
    }
    if let Some(n) = &b.name {
        let n = clean_name(n, "套餐名")?;
        set!("name", Some(n));
    }
    set!("provider_id", b.provider_id);
    set!("cpu_cores", b.cpu_cores);
    set!("mem_mb", b.mem_mb);
    set!("disk_gb", b.disk_gb);
    set!("bandwidth_mbps", b.bandwidth_mbps);
    set!("traffic_gb", b.traffic_gb);
    set!("price_cents", b.price_cents);
    set!("currency", b.currency.clone());
    set!("notes", b.notes.clone());
    if let Some(c) = &b.cycle {
        sqlx::query("update plan set cycle = $2::billing_cycle where id = $1").bind(id).bind(c).execute(&mut *tx).await?;
    }
    if let Some(ar) = b.archived {
        sqlx::query("update plan set archived_at = case when $2 then coalesce(archived_at, now()) else null end where id = $1")
            .bind(id)
            .bind(ar)
            .execute(&mut *tx)
            .await?;
    }
    audit(&mut tx, &a.name, "plan.update", "plan", Some(id), json!({})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}
