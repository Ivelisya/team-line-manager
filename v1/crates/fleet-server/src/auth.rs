//! 浏览器会话（Cookie）与 API 密钥鉴权。
//!
//! 写请求（非 GET/HEAD）走会话时必须带 `X-Fleet-Request: 1`：浏览器跨站表单发不出自定义头，借此防 CSRF。
//! API 密钥不是浏览器发起的，不要求这个头。
//!
//! 密钥的 scope 矩阵（会话登录的用户拥有全部）：
//! - `read`：所有 GET，但不含凭据类（`/entries/{id}/share`）与远程执行类（日志）
//! - `write`：非 GET 的业务写操作
//! - `secrets`：凭据类 GET（分享链接）
//! - `exec`：远程执行（`/machines/{id}/exec`、各类日志）
//! - 密钥管理（`/api-keys`）只允许会话用户，API 密钥一律 403

use crate::crypto::{random_token, sha256};
use crate::db::audit;
use crate::error::ApiJson;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{ConnectInfo, FromRequestParts, OriginalUri, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::Json;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;
use time::{Duration, OffsetDateTime};
use tokio::sync::Semaphore;
use uuid::Uuid;

pub const COOKIE: &str = "fleet_session";
const SESSION_DAYS: i64 = 14;

/// 全部 scope（会话用户拥有；创建密钥时只能从中选）
pub const ALL_SCOPES: [&str; 4] = ["read", "write", "secrets", "exec"];

/// 登录失败限速窗口与阈值
const FAIL_WINDOW_MIN: i64 = 15;
const FAIL_PER_USER: i64 = 5;
const FAIL_PER_IP: i64 = 20;

/// argon2 很吃 CPU：放 spawn_blocking，并限制同时在算的个数，免得登录风暴挤占 runtime 与内存
static HASH_SEM: Semaphore = Semaphore::const_new(2);

pub fn hash_password(pw: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Ok(Argon2::default().hash_password(pw.as_bytes(), &salt).map_err(|e| anyhow::anyhow!("{e}"))?.to_string())
}

fn verify_password(pw: &str, phc: &str) -> bool {
    PasswordHash::new(phc).map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok()).unwrap_or(false)
}

/// 在阻塞线程池里校验密码（调用方先拿 HASH_SEM 的许可）。`phc` 为 None（用户不存在）时对固定哈希做一次等价计算，抹平耗时差。
async fn verify_blocking(pw: String, phc: Option<String>) -> bool {
    tokio::task::spawn_blocking(move || match phc {
        Some(p) => verify_password(&pw, &p),
        None => {
            static DUMMY: OnceLock<String> = OnceLock::new();
            let d = DUMMY.get_or_init(|| hash_password("fleet-dummy-password").unwrap_or_default());
            let _ = verify_password(&pw, d);
            false
        }
    })
    .await
    .unwrap_or(false)
}

/// 已登录的操作者
#[derive(Debug, Clone)]
pub struct Actor {
    pub name: String,
    /// 会话用户才有；API 密钥为 None
    pub user_id: Option<Uuid>,
    pub scopes: Vec<String>,
}

impl Actor {
    pub fn is_session(&self) -> bool { self.user_id.is_some() }

    /// 要求拥有某个 scope（会话用户恒有）
    pub fn require_scope(&self, scope: &str) -> ApiResult<()> {
        if self.scopes.iter().any(|s| s == scope) {
            Ok(())
        } else {
            Err(ApiError::forbidden(format!("这个 API 密钥没有 {scope} 权限")))
        }
    }

    /// 要求是会话登录的用户（密钥管理等）
    pub fn require_session(&self) -> ApiResult<()> {
        if self.is_session() { Ok(()) } else { Err(ApiError::forbidden("这个操作只能由登录用户执行，API 密钥不行")) }
    }
}

/// 一个请求需要满足的鉴权要求
#[derive(Debug, PartialEq, Eq)]
pub enum Need {
    /// 只允许会话用户
    Session,
    Scope(&'static str),
}

/// 按 方法 + 路径 决定所需 scope（路径可带或不带 `/api/v1` 前缀）
pub fn needed(method: &Method, path: &str) -> Need {
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    let segs: &[&str] = if segs.len() >= 2 && segs[0] == "api" && segs[1] == "v1" { &segs[2..] } else { &segs };
    let get = matches!(*method, Method::GET | Method::HEAD);
    match segs {
        ["api-keys", ..] => Need::Session,
        ["entries", _, "share"] if get => Need::Scope("secrets"),
        ["machines", _, "exec"] if !get => Need::Scope("exec"),
        ["machines", _, "logs"] | ["server", "logs"] if get => Need::Scope("exec"),
        _ if get => Need::Scope("read"),
        _ => Need::Scope("write"),
    }
}

impl FromRequestParts<AppState> for Actor {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let write = !matches!(parts.method, Method::GET | Method::HEAD);
        // API 密钥
        if let Some(h) = parts.headers.get("authorization").and_then(|v| v.to_str().ok()) {
            if let Some(k) = h.strip_prefix("Bearer ") {
                // 最近使用时间一分钟内不重复写
                let row: Option<(String, Vec<String>)> = sqlx::query_as(
                    "with k as (select name, scopes from api_key where key_hash = $1 and revoked_at is null and (expires_at is null or expires_at > now())),
                          _u as (update api_key set last_used_at = now() where key_hash = $1 and revoked_at is null and (expires_at is null or expires_at > now())
                                 and (last_used_at is null or last_used_at < now() - interval '1 minute'))
                     select name, scopes from k",
                )
                .bind(sha256(k.as_bytes()))
                .fetch_optional(&st.db)
                .await?;
                let (name, scopes) = row.ok_or_else(ApiError::unauthorized)?;
                // nest 会剥掉前缀，原始路径在 OriginalUri 里
                let path = parts.extensions.get::<OriginalUri>().map(|u| u.0.path().to_string()).unwrap_or_else(|| parts.uri.path().to_string());
                match needed(&parts.method, &path) {
                    Need::Session => return Err(ApiError::forbidden("密钥管理只能由登录用户操作，API 密钥不行")),
                    Need::Scope(s) => {
                        if !scopes.iter().any(|x| x == s) {
                            return Err(ApiError::forbidden(format!("这个 API 密钥没有 {s} 权限")));
                        }
                    }
                }
                return Ok(Actor { name: format!("apikey:{name}"), user_id: None, scopes });
            }
        }
        // 会话
        let jar = CookieJar::from_headers(&parts.headers);
        let c = jar.get(COOKIE).ok_or_else(ApiError::unauthorized)?;
        let row: Option<(Uuid, String)> = sqlx::query_as(
            "with u as (select u.id, u.username from session s join app_user u on u.id = s.user_id
                        where s.id_hash = $1 and s.expires_at > now() and u.disabled_at is null),
                  _s as (update session set last_seen_at = now() where id_hash = $1 and expires_at > now() and last_seen_at < now() - interval '1 minute'
                         and exists (select 1 from u))
             select id, username from u",
        )
        .bind(sha256(c.value().as_bytes()))
        .fetch_optional(&st.db)
        .await?;
        let (id, name) = row.ok_or_else(ApiError::unauthorized)?;
        if write && parts.headers.get("x-fleet-request").and_then(|v| v.to_str().ok()) != Some("1") {
            return Err(ApiError::forbidden("写请求缺少 X-Fleet-Request 头"));
        }
        Ok(Actor { name, user_id: Some(id), scopes: ALL_SCOPES.iter().map(|s| s.to_string()).collect() })
    }
}

/// 请求来源 IP：`trust` 时取 X-Forwarded-For 的最后一项（多行头取最后一行的最后一项：反代追加的那一项；客户端或沿途 HTTP 代理塞的在前面，不可信），
/// 取不到或不是合法 IP 时用连接对端地址。登录限速与出口回显共用
pub fn client_ip(h: &HeaderMap, peer: Option<IpAddr>, trust: bool) -> Option<IpAddr> {
    let fwd = trust
        .then(|| h.get_all("x-forwarded-for").iter().next_back()?.to_str().ok()?.rsplit(',').next()?.trim().parse::<IpAddr>().ok())
        .flatten();
    fwd.or(peer).map(|ip| ip.to_canonical())
}

/// 请求来源 IP（登录限速用），规则见 [`client_ip`]
pub struct ClientIp(pub Option<IpAddr>);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
        Ok(ClientIp(client_ip(&parts.headers, peer, st.cfg.trust_forwarded_for)))
    }
}

#[derive(Deserialize)]
pub struct LoginReq {
    username: String,
    password: String,
}

fn wait_text(secs: i64) -> String {
    if secs < 60 { format!("{} 秒", secs.max(1)) } else { format!("{} 分钟", (secs + 59) / 60) }
}

/// 窗口内失败次数（含本次先记下的这一条）超过上限时，返回还要等多少秒（之前第 N 条要滑出窗口才会降到上限以下）
async fn blocked_secs(st: &AppState, user: &str, ip: Option<IpAddr>) -> ApiResult<Option<i64>> {
    let secs = format!("{FAIL_WINDOW_MIN} minutes");
    let by_user: Option<(f64,)> = sqlx::query_as(
        "select extract(epoch from (ts + $3::text::interval - now()))::float8 from login_failure
         where username = $1 and ts > now() - $3::text::interval order by ts desc offset $2 limit 1",
    )
    .bind(user)
    .bind(FAIL_PER_USER)
    .bind(&secs)
    .fetch_optional(&st.db)
    .await?;
    let by_ip: Option<(f64,)> = match ip {
        Some(ip) => {
            sqlx::query_as(
                "select extract(epoch from (ts + $3::text::interval - now()))::float8 from login_failure
                 where ip = $1 and ts > now() - $3::text::interval order by ts desc offset $2 limit 1",
            )
            .bind(ipnetwork::IpNetwork::from(ip))
            .bind(FAIL_PER_IP)
            .bind(&secs)
            .fetch_optional(&st.db)
            .await?
        }
        None => None,
    };
    Ok([by_user, by_ip].into_iter().flatten().map(|(s,)| s.ceil() as i64).max())
}

pub async fn login(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    jar: CookieJar,
    ApiJson(r): ApiJson<LoginReq>,
) -> ApiResult<(CookieJar, Json<Value>)> {
    // 限速键：用户名截断，避免超长字符串灌库
    let uname: String = r.username.chars().take(64).collect();
    let _permit = HASH_SEM.acquire().await.map_err(|e| ApiError::internal(format!("登录限流信号量：{e}")))?;
    // 先按失败记下这次尝试再判断：并发的尝试各占一个名额（后提交的一定看得到先提交的），不会一起挤过限速；
    // 被限速的这条删掉（不延长封禁），登录成功时连同该用户的失败记录一起清掉
    let attempt: i64 = sqlx::query_scalar("insert into login_failure (username, ip) values ($1, $2) returning id")
        .bind(&uname)
        .bind(ip.map(ipnetwork::IpNetwork::from))
        .fetch_one(&st.db)
        .await?;
    if let Some(wait) = blocked_secs(&st, &uname, ip).await? {
        sqlx::query("delete from login_failure where id = $1").bind(attempt).execute(&st.db).await?;
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", format!("登录失败次数过多，请 {}后再试", wait_text(wait)))
            .with_detail(json!({"retry_after_s": wait})));
    }
    let row: Option<(Uuid, String)> =
        sqlx::query_as("select id, password_hash from app_user where username = $1 and disabled_at is null")
            .bind(&r.username)
            .fetch_optional(&st.db)
            .await?;
    let (uid, phc) = match row {
        Some((u, p)) => (Some(u), Some(p)),
        None => (None, None),
    };
    let good = verify_blocking(r.password.clone(), phc).await;
    let ip_s = ip.map(|i| i.to_string());
    let Some(uid) = uid.filter(|_| good) else {
        let mut tx = st.db.begin().await?;
        // 失败记录开头已经写了；顺手清掉一天前的旧记录
        sqlx::query("delete from login_failure where ts < now() - interval '1 day'").execute(&mut *tx).await?;
        audit(&mut tx, &uname, "auth.login_failed", "user", None, json!({"ip": ip_s})).await?;
        tx.commit().await?;
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "bad_credentials", "用户名或密码不对"));
    };
    let tok = random_token(32);
    let exp = OffsetDateTime::now_utc() + Duration::days(SESSION_DAYS);
    let mut tx = st.db.begin().await?;
    sqlx::query("insert into session (id_hash, user_id, expires_at) values ($1, $2, $3)")
        .bind(sha256(tok.as_bytes()))
        .bind(uid)
        .bind(exp)
        .execute(&mut *tx)
        .await?;
    sqlx::query("delete from login_failure where username = $1").bind(&uname).execute(&mut *tx).await?;
    audit(&mut tx, &r.username, "auth.login", "user", Some(uid), json!({"ip": ip_s})).await?;
    tx.commit().await?;
    let secure = st.cfg.cookie_secure.unwrap_or_else(|| st.cfg.public_url.starts_with("https://"));
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
        let gone: Option<(Uuid, String)> = sqlx::query_as(
            "with d as (delete from session where id_hash = $1 returning user_id)
             select u.id, u.username from d join app_user u on u.id = d.user_id",
        )
        .bind(sha256(c.value().as_bytes()))
        .fetch_optional(&st.db)
        .await?;
        if let Some((uid, name)) = gone {
            let mut tx = st.db.begin().await?;
            audit(&mut tx, &name, "auth.logout", "user", Some(uid), json!({})).await?;
            tx.commit().await?;
        }
    }
    Ok((jar.remove(Cookie::build(COOKIE).path("/").build()), Json(json!({"data": {"ok": true}}))))
}

pub async fn me(a: Actor) -> Json<Value> {
    Json(json!({"data": {"username": a.name, "kind": if a.is_session() { "session" } else { "api_key" }, "scopes": a.scopes}}))
}

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
    use super::*;

    #[test]
    fn client_ip_takes_last_forwarded_entry() {
        let peer: Option<std::net::IpAddr> = Some("10.0.0.1".parse().unwrap());
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4, 203.0.113.7".parse().unwrap());
        assert_eq!(client_ip(&h, peer, true), Some("203.0.113.7".parse().unwrap()), "取反代追加的最后一项，前面的可伪造");
        assert_eq!(client_ip(&h, peer, false), peer, "不信任转发头时用连接对端");
        h.insert("x-forwarded-for", "1.2.3.4, garbage".parse().unwrap());
        assert_eq!(client_ip(&h, peer, true), peer, "最后一项不是 IP：用连接对端，不退到前面的");
        assert_eq!(client_ip(&HeaderMap::new(), Some("::ffff:198.51.100.2".parse().unwrap()), true), Some("198.51.100.2".parse().unwrap()));
        // 多行 X-Forwarded-For：取最后一行（反代追加的）的最后一项
        let mut h = HeaderMap::new();
        h.append("x-forwarded-for", "6.6.6.6".parse().unwrap());
        h.append("x-forwarded-for", "1.2.3.4, 203.0.113.9".parse().unwrap());
        assert_eq!(client_ip(&h, peer, true), Some("203.0.113.9".parse().unwrap()));
    }

    #[test]
    fn password_roundtrip() {
        let h = super::hash_password("s3cret").unwrap();
        assert!(super::verify_password("s3cret", &h));
        assert!(!super::verify_password("nope", &h));
    }

    #[test]
    fn scope_matrix() {
        let g = Method::GET;
        let p = Method::POST;
        assert_eq!(needed(&g, "/api/v1/machines"), Need::Scope("read"));
        assert_eq!(needed(&g, "/api/v1/entries/abc/share"), Need::Scope("secrets"));
        assert_eq!(needed(&g, "/entries/abc/share"), Need::Scope("secrets"));
        assert_eq!(needed(&p, "/api/v1/entries/abc/actions"), Need::Scope("write"));
        assert_eq!(needed(&p, "/api/v1/machines/x/exec"), Need::Scope("exec"));
        assert_eq!(needed(&g, "/api/v1/machines/x/logs"), Need::Scope("exec"));
        assert_eq!(needed(&g, "/api/v1/server/logs"), Need::Scope("exec"));
        assert_eq!(needed(&g, "/api/v1/api-keys"), Need::Session);
        assert_eq!(needed(&Method::DELETE, "/api/v1/api-keys/x"), Need::Session);
        assert_eq!(needed(&g, "/api/v1/jobs/x/logs"), Need::Scope("read"));
    }

    #[test]
    fn wait_text_units() {
        assert_eq!(wait_text(30), "30 秒");
        assert_eq!(wait_text(61), "2 分钟");
    }
}
