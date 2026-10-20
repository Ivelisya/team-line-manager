//! 浏览器会话（Cookie）与 API 密钥鉴权。
//!
//! 写请求（非 GET/HEAD）必须带 `X-Fleet-Request: 1`：浏览器跨站表单发不出自定义头，借此防 CSRF。

use crate::error::ApiJson;
use crate::crypto::{random_token, sha256};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::Method;
use axum::Json;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

pub const COOKIE: &str = "fleet_session";
const SESSION_DAYS: i64 = 14;

pub fn hash_password(pw: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Ok(Argon2::default().hash_password(pw.as_bytes(), &salt).map_err(|e| anyhow::anyhow!("{e}"))?.to_string())
}

fn verify_password(pw: &str, phc: &str) -> bool {
    PasswordHash::new(phc).map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok()).unwrap_or(false)
}

/// 已登录的操作者
#[derive(Debug, Clone)]
pub struct Actor {
    pub name: String,
    pub user_id: Option<Uuid>,
}

impl FromRequestParts<AppState> for Actor {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let write = !matches!(parts.method, Method::GET | Method::HEAD);
        // API 密钥
        if let Some(h) = parts.headers.get("authorization").and_then(|v| v.to_str().ok()) {
            if let Some(k) = h.strip_prefix("Bearer ") {
                let row: Option<(String, Vec<String>)> = sqlx::query_as(
                    "update api_key set last_used_at = now() where key_hash = $1 and revoked_at is null
                     and (expires_at is null or expires_at > now()) returning name, scopes",
                )
                .bind(sha256(k.as_bytes()))
                .fetch_optional(&st.db)
                .await?;
                let (name, scopes) = row.ok_or_else(ApiError::unauthorized)?;
                if write && !scopes.iter().any(|s| s == "write") {
                    return Err(ApiError::forbidden("这个 API 密钥只读"));
                }
                return Ok(Actor { name: format!("apikey:{name}"), user_id: None });
            }
        }
        // 会话
        let jar = CookieJar::from_headers(&parts.headers);
        let c = jar.get(COOKIE).ok_or_else(ApiError::unauthorized)?;
        let row: Option<(Uuid, String)> = sqlx::query_as(
            "update session s set last_seen_at = now() from app_user u
             where s.id_hash = $1 and s.expires_at > now() and u.id = s.user_id and u.disabled_at is null
             returning u.id, u.username",
        )
        .bind(sha256(c.value().as_bytes()))
        .fetch_optional(&st.db)
        .await?;
        let (id, name) = row.ok_or_else(ApiError::unauthorized)?;
        if write && parts.headers.get("x-fleet-request").and_then(|v| v.to_str().ok()) != Some("1") {
            return Err(ApiError::forbidden("写请求缺少 X-Fleet-Request 头"));
        }
        Ok(Actor { name, user_id: Some(id) })
    }
}

#[derive(Deserialize)]
pub struct LoginReq {
    username: String,
    password: String,
}

pub async fn login(State(st): State<AppState>, jar: CookieJar, ApiJson(r): ApiJson<LoginReq>) -> ApiResult<(CookieJar, Json<Value>)> {
    let row: Option<(Uuid, String)> =
        sqlx::query_as("select id, password_hash from app_user where username = $1 and disabled_at is null")
            .bind(&r.username)
            .fetch_optional(&st.db)
            .await?;
    let Some((uid, phc)) = row.filter(|(_, phc)| verify_password(&r.password, phc)) else {
        // 固定耗时，避免用户名枚举
        let _ = hash_password(&r.password);
        return Err(ApiError::new(axum::http::StatusCode::UNAUTHORIZED, "bad_credentials", "用户名或密码不对"));
    };
    let _ = phc;
    let tok = random_token(32);
    let exp = OffsetDateTime::now_utc() + Duration::days(SESSION_DAYS);
    sqlx::query("insert into session (id_hash, user_id, expires_at) values ($1, $2, $3)")
        .bind(sha256(tok.as_bytes()))
        .bind(uid)
        .bind(exp)
        .execute(&st.db)
        .await?;
    let secure = st.cfg.public_url.starts_with("https://");
    let c = Cookie::build((COOKIE, tok))
        .http_only(true)
        .same_site(SameSite::Strict)
        .secure(secure)
        .path("/")
        .max_age(Duration::days(SESSION_DAYS))
        .build();
    Ok((jar.add(c), Json(json!({"data": {"username": r.username}}))))
}

pub async fn logout(State(st): State<AppState>, jar: CookieJar) -> ApiResult<(CookieJar, Json<Value>)> {
    if let Some(c) = jar.get(COOKIE) {
        sqlx::query("delete from session where id_hash = $1").bind(sha256(c.value().as_bytes())).execute(&st.db).await?;
    }
    Ok((jar.remove(Cookie::build(COOKIE).path("/").build()), Json(json!({"data": {"ok": true}}))))
}

pub async fn me(a: Actor) -> Json<Value> { Json(json!({"data": {"username": a.name}})) }

/// CLI：建用户 / 改密码
pub async fn upsert_user(db: &sqlx::PgPool, username: &str, password: &str) -> anyhow::Result<()> {
    let h = hash_password(password)?;
    sqlx::query(
        "insert into app_user (id, username, password_hash) values ($1, $2, $3)
         on conflict (username) do update set password_hash = excluded.password_hash, disabled_at = null",
    )
    .bind(Uuid::now_v7())
    .bind(username)
    .bind(h)
    .execute(db)
    .await?;
    sqlx::query("delete from session where user_id = (select id from app_user where username = $1)").bind(username).execute(db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn password_roundtrip() {
        let h = super::hash_password("s3cret").unwrap();
        assert!(super::verify_password("s3cret", &h));
        assert!(!super::verify_password("nope", &h));
    }
}
