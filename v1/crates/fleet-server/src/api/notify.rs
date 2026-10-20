//! 告警通知渠道：增删改查、发测试消息、查发送记录。配置（令牌、webhook 地址）用 SecretBox 加密存库，接口永不返回明文。

use super::ok;
use crate::auth::Actor;
use crate::db::{audit, new_id};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::notify;
use crate::state::{AppState, Event};
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

const SEVERITIES: [&str; 3] = ["info", "warning", "critical"];

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    name: String,
    kind: String,
    min_severity: String,
    enabled: bool,
    created_at: OffsetDateTime,
    config_enc: Vec<u8>,
}

#[derive(Serialize)]
struct ChannelOut {
    id: Uuid,
    name: String,
    kind: String,
    min_severity: String,
    enabled: bool,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    config_hint: String,
}

const COLS: &str = "id, name, kind, min_severity::text min_severity, enabled, created_at, config_enc";

fn open_config(st: &AppState, enc: &[u8]) -> Option<Value> {
    st.secrets.open_str(enc).ok().and_then(|s| serde_json::from_str(&s).ok())
}

fn out(st: &AppState, r: Row) -> ChannelOut {
    let hint = open_config(st, &r.config_enc).map(|c| notify::config_hint(&r.kind, &c)).unwrap_or_else(|| "（无法解密）".into());
    ChannelOut { id: r.id, name: r.name, kind: r.kind, min_severity: r.min_severity, enabled: r.enabled, created_at: r.created_at, config_hint: hint }
}

fn check_name(n: &str) -> ApiResult<String> {
    let n = n.trim();
    if n.is_empty() || n.chars().count() > 60 {
        return Err(ApiError::bad_request("名称不能为空，最长 60 个字"));
    }
    Ok(n.to_string())
}

fn check_severity(s: &str) -> ApiResult<()> {
    if SEVERITIES.contains(&s) {
        Ok(())
    } else {
        Err(ApiError::bad_request("min_severity 只能是 info / warning / critical"))
    }
}

pub async fn list(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let rows: Vec<Row> = sqlx::query_as(&format!("select {COLS} from notify_channel order by created_at, id")).fetch_all(&st.db).await?;
    Ok(ok(rows.into_iter().map(|r| out(&st, r)).collect::<Vec<_>>()))
}

#[derive(Deserialize)]
pub struct NewChannel {
    name: String,
    kind: String,
    config: Value,
    #[serde(default = "d_severity")]
    min_severity: String,
    #[serde(default = "d_true")]
    enabled: bool,
}
fn d_severity() -> String { "warning".into() }
fn d_true() -> bool { true }

pub async fn create(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<NewChannel>) -> ApiResult<Json<Value>> {
    let name = check_name(&b.name)?;
    check_severity(&b.min_severity)?;
    let cfg = notify::normalize_config(&b.kind, &b.config).map_err(ApiError::bad_request)?;
    let id = new_id();
    let mut tx = st.db.begin().await?;
    sqlx::query("insert into notify_channel (id, kind, name, config_enc, min_severity, enabled) values ($1, $2, $3, $4, $5::severity, $6)")
        .bind(id)
        .bind(&b.kind)
        .bind(&name)
        .bind(st.secrets.seal_str(&cfg.to_string()))
        .bind(&b.min_severity)
        .bind(b.enabled)
        .execute(&mut *tx)
        .await?;
    // 审计里只记名字和类型，不记配置
    audit(&mut tx, &a.name, "notify.create", "notify_channel", Some(id), json!({"name": name, "kind": b.kind})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}

#[derive(Deserialize)]
pub struct PatchChannel {
    name: Option<String>,
    config: Option<Value>,
    min_severity: Option<String>,
    enabled: Option<bool>,
}

pub async fn patch(a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>, ApiJson(b): ApiJson<PatchChannel>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let cur: Row = sqlx::query_as(&format!("select {COLS} from notify_channel where id = $1 for update")).bind(id).fetch_optional(&mut *tx).await?.ok_or_else(|| ApiError::not_found("没有这个通知渠道"))?;
    let name = match &b.name {
        Some(n) => check_name(n)?,
        None => cur.name.clone(),
    };
    let sev = b.min_severity.clone().unwrap_or_else(|| cur.min_severity.clone());
    check_severity(&sev)?;
    let enabled = b.enabled.unwrap_or(cur.enabled);
    let enc = match &b.config {
        Some(newc) => {
            // 令牌/地址不会回显给前端，所以编辑时没填的字段沿用旧值，填了的覆盖
            let mut merged = open_config(&st, &cur.config_enc).unwrap_or_else(|| json!({}));
            for (k, v) in newc.as_object().into_iter().flatten() {
                let blank = v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty());
                if !blank {
                    merged[k] = v.clone();
                } else if k == "api_base" {
                    if let Some(m) = merged.as_object_mut() {
                        m.remove("api_base");
                    }
                }
            }
            let cfg = notify::normalize_config(&cur.kind, &merged).map_err(ApiError::bad_request)?;
            st.secrets.seal_str(&cfg.to_string())
        }
        None => cur.config_enc.clone(),
    };
    sqlx::query("update notify_channel set name = $2, config_enc = $3, min_severity = $4::severity, enabled = $5 where id = $1")
        .bind(id)
        .bind(&name)
        .bind(enc)
        .bind(&sev)
        .bind(enabled)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &a.name, "notify.update", "notify_channel", Some(id), json!({"name": name, "config_changed": b.config.is_some(), "enabled": enabled, "min_severity": sev})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"ok": true})))
}

pub async fn delete(a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let name: Option<(String,)> = sqlx::query_as("delete from notify_channel where id = $1 returning name").bind(id).fetch_optional(&mut *tx).await?;
    let (name,) = name.ok_or_else(|| ApiError::not_found("没有这个通知渠道"))?;
    audit(&mut tx, &a.name, "notify.delete", "notify_channel", Some(id), json!({"name": name})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"ok": true})))
}

/// 发一条测试消息。发送失败不是接口错误：返回 `{ok:false, error}`。
pub async fn test(_a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let exists: Option<(Uuid,)> = sqlx::query_as("select id from notify_channel where id = $1").bind(id).fetch_optional(&st.db).await?;
    if exists.is_none() {
        return Err(ApiError::not_found("没有这个通知渠道"));
    }
    let r = notify::send_test(&st, id).await;
    Ok(ok(json!({"ok": r.is_ok(), "error": r.err()})))
}

#[derive(Deserialize)]
pub struct LogQ {
    limit: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct LogOut {
    id: i64,
    channel_id: Option<Uuid>,
    channel_name: Option<String>,
    incident_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    ts: OffsetDateTime,
    ok: bool,
    error: Option<String>,
    /// firing / repeat / resolved / test
    kind: String,
}

pub async fn log(_a: Actor, State(st): State<AppState>, Query(q): Query<LogQ>) -> ApiResult<Json<Value>> {
    let v: Vec<LogOut> = sqlx::query_as(
        "select l.id, l.channel_id, c.name channel_name, l.incident_id, l.ts, l.ok, l.error, l.kind
         from notify_log l left join notify_channel c on c.id = l.channel_id order by l.id desc limit $1",
    )
    .bind(q.limit.unwrap_or(100).clamp(1, 1000))
    .fetch_all(&st.db)
    .await?;
    Ok(ok(v))
}
