//! API 密钥管理。只有会话登录的用户能管理（API 密钥调用一律 403）。
//! 库里只存 sha256 与前缀；明文 token 只在创建时返回一次。

use super::account::Recheck;
use super::ok;
use crate::auth::{Actor, ClientIp, ALL_SCOPES};
use crate::crypto::{random_token, sha256};
use crate::db::{audit, new_id};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

#[derive(Serialize, sqlx::FromRow)]
struct KeyOut {
    id: Uuid,
    name: String,
    prefix: String,
    scopes: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    last_used_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    revoked_at: Option<OffsetDateTime>,
}

pub async fn list(a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    a.require_session()?;
    let v: Vec<KeyOut> = sqlx::query_as(
        "select id, name, prefix, scopes, created_at, expires_at, last_used_at, revoked_at from api_key order by created_at desc",
    )
    .fetch_all(&st.db)
    .await?;
    Ok(ok(v))
}

/// 新密钥的（前缀，令牌）：`flk_<前缀>_<随机串>`。前缀用十六进制（不含 _），便于在列表里认出是哪把。local-admin 共用
pub(crate) fn new_token() -> (String, String) {
    let mut p = [0u8; 4];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut p);
    let prefix = hex::encode(p);
    let token = format!("flk_{prefix}_{}", random_token(32));
    (prefix, token)
}

#[derive(Deserialize)]
pub struct CreateIn {
    name: String,
    scopes: Vec<String>,
    expires_days: Option<i64>,
    /// 当前密码（开了两步验证还要 totp：验证码或恢复码）
    #[serde(default)]
    password: String,
    #[serde(default)]
    totp: Option<String>,
}

/// 新建密钥要再输一次当前密码（开了两步验证还要验证码）：偷到 Cookie 的人不能借它铸一把全权限、改密码后也不失效的密钥
pub async fn create(a: Actor, State(st): State<AppState>, ClientIp(ip): ClientIp, ApiJson(b): ApiJson<CreateIn>) -> ApiResult<Json<Value>> {
    a.require_session()?;
    let name = b.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(ApiError::bad_request("密钥名称要 1 到 64 个字符"));
    }
    if name == crate::local_admin::KEY_NAME {
        return Err(ApiError::bad_request(format!("「{name}」留给中控本机的 fleetctl（部署时由 fleet-server local-admin 建），请换个名字")));
    }
    let mut scopes: Vec<String> = vec![];
    for s in &b.scopes {
        if !ALL_SCOPES.contains(&s.as_str()) {
            return Err(ApiError::bad_request(format!("未知的 scope「{s}」，可选：{}", ALL_SCOPES.join(" / "))));
        }
        if !scopes.contains(s) {
            scopes.push(s.clone());
        }
    }
    if scopes.is_empty() {
        return Err(ApiError::bad_request("至少选一个 scope"));
    }
    let expires = match b.expires_days {
        None => None,
        Some(d) if (1..=3650).contains(&d) => Some(OffsetDateTime::now_utc() + Duration::days(d)),
        Some(_) => return Err(ApiError::bad_request("expires_days 要在 1 到 3650 之间")),
    };
    let (r, f) = Recheck::password_and_code(&st, &a, ip, "apikey_create", &b.password, b.totp.as_deref()).await?;
    let (prefix, token) = new_token();
    let id = new_id();
    let mut tx = st.db.begin().await?;
    r.done(&mut tx, f).await?;
    sqlx::query("insert into api_key (id, name, prefix, key_hash, scopes, expires_at) values ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(&name)
        .bind(&prefix)
        .bind(sha256(token.as_bytes()))
        .bind(&scopes)
        .bind(expires)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &a.name, "apikey.create", "api_key", Some(id), json!({"name": name, "prefix": prefix, "scopes": scopes, "expires_days": b.expires_days})).await?;
    tx.commit().await?;
    st.emit(Event::ApiKeys);
    Ok(ok(json!({"id": id, "token": token})))
}

pub async fn revoke(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    a.require_session()?;
    let mut tx = st.db.begin().await?;
    let cur: (String, String, Option<OffsetDateTime>) = sqlx::query_as("select name, prefix, revoked_at from api_key where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个 API 密钥"))?;
    // 重复撤销视为成功（幂等）
    if cur.2.is_none() {
        sqlx::query("update api_key set revoked_at = now() where id = $1").bind(id).execute(&mut *tx).await?;
        audit(&mut tx, &a.name, "apikey.revoke", "api_key", Some(id), json!({"name": cur.0, "prefix": cur.1})).await?;
    }
    tx.commit().await?;
    st.emit(Event::ApiKeys);
    st.revoked();
    Ok(ok(json!({"ok": true})))
}
