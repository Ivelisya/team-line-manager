//! 浏览器会话（Cookie）与 API 密钥鉴权。
//!
//! 写请求（非 GET/HEAD）走会话时必须带 `X-Fleet-Request: 1`：浏览器跨站表单发不出自定义头，借此防 CSRF。
//! API 密钥不是浏览器发起的，不要求这个头。
//!
//! 密钥的 scope 矩阵（会话登录的用户拥有全部）：
//! - `read`：所有 GET，但不含凭据类（`/entries/{id}/share`）与远程执行类（日志）
//! - `write`：非 GET 的业务写操作
//! - `secrets`：凭据类 GET（分享链接）
//! - `exec`：远程执行（`/machines/{id}/exec`、各类日志、`fleetctl ssh` 登录前报的审计 `POST /audit/ssh`）
//! - 密钥管理（`/api-keys`）与账户安全（`/account/...`：两步验证、会话、改密码）只允许会话用户，API 密钥一律 403

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
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use time::{Duration, OffsetDateTime};
use tokio::sync::Semaphore;
use uuid::Uuid;

pub const COOKIE: &str = "fleet_session";
const SESSION_DAYS: i64 = 14;

/// 全部 scope（会话用户拥有；创建密钥时只能从中选）
pub const ALL_SCOPES: [&str; 4] = ["read", "write", "secrets", "exec"];

/// 登录失败限速窗口与阈值：同一用户名 + 同一来源 5 次、同一来源 20 次、同一用户名（不分来源）100 次。
/// 只按用户名封的话，谁都能用错误密码把 admin 一直锁在外面；按来源分开后只封攻击者自己，100 次的总上限挡分布式猜密码
/// （本人 30 天内成功登录过的来源不受这个总上限，见 blocked_secs）
const FAIL_WINDOW_MIN: i64 = 15;
const FAIL_PER_USER_IP: i64 = 5;
const FAIL_PER_IP: i64 = 20;
const FAIL_PER_USER: i64 = 100;
/// 两步验证码输错（登录与再次校验）另有上限：同一用户名 1 小时内 10 次，不分来源、本人登录成功过的来源也不例外。
/// 知道密码的人换 /64 猜 6 位码时，按来源的上限挡不住
const WRONG_CODE_PER_USER: i64 = 10;

/// 记失败、判断限速这一步同时在跑的个数：登录风暴时不占满主连接池
static CHECK_SEM: Semaphore = Semaphore::const_new(4);

/// argon2 很吃 CPU：放 spawn_blocking，并限制同时在算的个数，免得登录风暴挤占 runtime 与内存
pub(crate) static HASH_SEM: Semaphore = Semaphore::const_new(2);

pub fn hash_password(pw: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Ok(Argon2::default().hash_password(pw.as_bytes(), &salt).map_err(|e| anyhow::anyhow!("{e}"))?.to_string())
}

fn verify_password(pw: &str, phc: &str) -> bool {
    PasswordHash::new(phc).map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok()).unwrap_or(false)
}

/// 在阻塞线程池里校验密码（调用方先拿 HASH_SEM 的许可）。`phc` 为 None（用户不存在）时对固定哈希做一次等价计算，抹平耗时差。
pub(crate) async fn verify_blocking(pw: String, phc: Option<String>) -> bool {
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
        ["api-keys", ..] | ["account", ..] => Need::Session,
        ["entries", _, "share"] if get => Need::Scope("secrets"),
        ["machines", _, "exec"] if !get => Need::Scope("exec"),
        ["machines", _, "logs"] | ["server", "logs"] if get => Need::Scope("exec"),
        // fleetctl ssh 登录前报的审计：只有能执行命令的密钥能写，别的密钥伪造不了登录记录
        ["audit", "ssh"] if !get => Need::Scope("exec"),
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
                    Need::Session => return Err(ApiError::forbidden("密钥管理与账户安全只能由登录用户操作，API 密钥不行")),
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
    /// 开了两步验证时必填：6 位验证码或一个恢复码
    #[serde(default)]
    totp: Option<String>,
}

fn wait_text(secs: i64) -> String {
    if secs < 60 { format!("{} 秒", secs.max(1)) } else { format!("{} 分钟", (secs + 59) / 60) }
}

/// 限速按来源分组：IPv4 按单个地址，IPv6 按 /64（一台机器通常能随意换同一 /64 里的地址）。`login_failure.ip` 存的就是分组后的地址
pub(crate) fn ip_group(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & !(u64::MAX as u128))),
        v4 => v4,
    }
}

/// 窗口内失败次数（含本次先记下的这一条）超过任一上限时，返回还要等多少秒（之前第 N 条要滑出窗口才会降到上限以下）。
/// 来源未知（没有对端地址）时算作同一个来源，只是不查按来源的上限。
/// 该用户 30 天内登录成功过的来源（login_source）不受「同一用户名不分来源」的上限：否则攻击者换 20 个 /64 各错 5 次就能把本人一直锁在外面。
/// 验证码输错（wrong_code）另按 1 小时 WRONG_CODE_PER_USER 次封，不看来源
async fn blocked_secs(st: &AppState, user: &str, group: Option<IpAddr>) -> ApiResult<Option<i64>> {
    let secs: Option<f64> = sqlx::query_scalar(
        "select extract(epoch from greatest(
           (select ts from login_failure where username = $1 and ip is not distinct from $2 and ts > now() - $6::text::interval order by ts desc offset $3 limit 1) + $6::text::interval,
           (select ts from login_failure where ip = $2 and ts > now() - $6::text::interval order by ts desc offset $4 limit 1) + $6::text::interval,
           (select ts from login_failure where username = $1 and ts > now() - $6::text::interval
              and not exists (select 1 from login_source s join app_user u on u.id = s.user_id
                              where u.username = $1 and s.ip = $2 and s.last_ok_at > now() - interval '30 days')
            order by ts desc offset $5 limit 1) + $6::text::interval,
           (select ts from login_failure where username = $1 and wrong_code and ts > now() - interval '1 hour' order by ts desc offset $7 limit 1) + interval '1 hour'
         ) - now())::float8",
    )
    .bind(user)
    .bind(group.map(ipnetwork::IpNetwork::from))
    .bind(FAIL_PER_USER_IP)
    .bind(FAIL_PER_IP)
    .bind(FAIL_PER_USER)
    .bind(format!("{FAIL_WINDOW_MIN} minutes"))
    .bind(WRONG_CODE_PER_USER - 1)
    .fetch_one(&st.db)
    .await?;
    Ok(secs.map(|s| s.ceil() as i64))
}

/// 记下一次要校验密码 / 验证码的尝试（先按失败记）再判断限速：并发的尝试各占一个名额（后提交的一定看得到先提交的），不会一起挤过限速；
/// 被限速的这条删掉（不延长封禁）并回 429。成功时调用方用 [`clear_failures`] 连同该用户在本来源的失败记录一起清掉；
/// 验证码不对时用 [`mark_wrong_code`] 标上，另计验证码的上限。登录与账户安全里的再次校验共用
pub(crate) async fn begin_attempt(st: &AppState, uname: &str, group: Option<IpAddr>) -> ApiResult<i64> {
    let _check = CHECK_SEM.acquire().await.map_err(|e| ApiError::internal(format!("登录限流信号量：{e}")))?;
    let attempt: i64 = sqlx::query_scalar("insert into login_failure (username, ip) values ($1, $2) returning id")
        .bind(uname)
        .bind(group.map(ipnetwork::IpNetwork::from))
        .fetch_one(&st.db)
        .await?;
    if let Some(wait) = blocked_secs(st, uname, group).await? {
        forget_attempt(st, attempt).await?;
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", format!("登录失败次数过多，请 {}后再试", wait_text(wait)))
            .with_detail(json!({"retry_after_s": wait})));
    }
    Ok(attempt)
}

async fn forget_attempt(st: &AppState, attempt: i64) -> sqlx::Result<()> {
    sqlx::query("delete from login_failure where id = $1").bind(attempt).execute(&st.db).await.map(|_| ())
}

/// 这次尝试是验证码输错：计入 WRONG_CODE_PER_USER
pub(crate) async fn mark_wrong_code(st: &AppState, attempt: i64) -> sqlx::Result<()> {
    sqlx::query("update login_failure set wrong_code = true where id = $1").bind(attempt).execute(&st.db).await.map(|_| ())
}

/// 只清本来源的：别处（攻击者）的失败照样计数，不因为本人成功了一次就重新给他名额
pub(crate) async fn clear_failures(tx: &mut crate::db::Tx<'_>, uname: &str, group: Option<IpAddr>) -> sqlx::Result<()> {
    sqlx::query("delete from login_failure where username = $1 and ip is not distinct from $2")
        .bind(uname)
        .bind(group.map(ipnetwork::IpNetwork::from))
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// 失败的那条记录开头已经写了，这里写审计、顺手清掉一天前的旧记录
pub(crate) async fn record_failure(st: &AppState, actor: &str, action: &str, uid: Option<Uuid>, detail: Value) -> ApiResult<()> {
    let mut tx = st.db.begin().await?;
    sqlx::query("delete from login_failure where ts < now() - interval '1 day'").execute(&mut *tx).await?;
    audit(&mut tx, actor, action, "user", uid, detail).await?;
    tx.commit().await?;
    Ok(())
}

/// 第二步验证用的是哪种
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Factor {
    Totp,
    Recovery,
}

/// 校验用户的第二步：6 位数字按 TOTP（时间步要晚于上次接受的那一步，接受后记下，同一个码不能用两次），其它按恢复码（用掉）。
/// 对不上返回 None。`secret_enc` 是该用户已启用的 TOTP 密钥
pub(crate) async fn check_factor(st: &AppState, uid: Uuid, secret_enc: &[u8], input: &str) -> ApiResult<Option<Factor>> {
    if let Some(code) = crate::totp::parse_code(input) {
        let secret = st.secrets.open(secret_enc).map_err(|e| ApiError::internal(format!("解密 TOTP 密钥：{e}")))?;
        let last: Option<i64> = sqlx::query_scalar("select totp_last_step from app_user where id = $1").bind(uid).fetch_one(&st.db).await?;
        let Some(step) = crate::totp::matching_step(&secret, code, crate::totp::now_step(), last) else { return Ok(None) };
        // 条件更新：同一个码并发提交两次，只有一次能把 last_step 推上去
        let n = sqlx::query("update app_user set totp_last_step = $2 where id = $1 and (totp_last_step is null or totp_last_step < $2)")
            .bind(uid)
            .bind(step)
            .execute(&st.db)
            .await?
            .rows_affected();
        return Ok((n == 1).then_some(Factor::Totp));
    }
    let n = sqlx::query("update recovery_code set used_at = now() where user_id = $1 and code_hash = $2 and used_at is null")
        .bind(uid)
        .bind(crate::totp::recovery_hash(input))
        .execute(&st.db)
        .await?
        .rows_affected();
    Ok((n == 1).then_some(Factor::Recovery))
}

pub(crate) fn totp_required() -> ApiError { ApiError::new(StatusCode::UNAUTHORIZED, "totp_required", "请输入两步验证码") }
pub(crate) fn totp_invalid() -> ApiError { ApiError::new(StatusCode::UNAUTHORIZED, "totp_invalid", "两步验证码不对或已经用过") }

/// 会话记下的 User-Agent 最多 200 个字符
fn user_agent(h: &HeaderMap) -> Option<String> {
    h.get("user-agent").and_then(|v| v.to_str().ok()).map(|s| s.chars().take(200).collect())
}

pub async fn login(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    ApiJson(r): ApiJson<LoginReq>,
) -> ApiResult<(CookieJar, Json<Value>)> {
    // 限速键：用户名截断，避免超长字符串灌库
    let uname: String = r.username.chars().take(64).collect();
    let group = ip.map(ip_group);
    // 这一步在等算哈希的许可之前：被限速的请求不排进 argon2 的队，挤不掉别人的登录
    let attempt = begin_attempt(&st, &uname, group).await?;
    let permit = HASH_SEM.acquire().await.map_err(|e| ApiError::internal(format!("登录限流信号量：{e}")))?;
    let row: Option<(Uuid, String, Option<Vec<u8>>)> =
        sqlx::query_as("select id, password_hash, totp_secret_enc from app_user where username = $1 and disabled_at is null")
            .bind(&r.username)
            .fetch_optional(&st.db)
            .await?;
    let (uid, phc, totp_enc) = match row {
        Some((u, p, t)) => (Some(u), Some(p), t),
        None => (None, None, None),
    };
    let good = verify_blocking(r.password.clone(), phc).await;
    drop(permit);
    let ip_s = ip.map(|i| i.to_string());
    let Some(uid) = uid.filter(|_| good) else {
        record_failure(&st, &uname, "auth.login_failed", None, json!({"ip": ip_s})).await?;
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "bad_credentials", "用户名或密码不对"));
    };
    // 两步验证：只在密码对了之后才提，猜密码的人看不出账户有没有开
    let mut recovery = false;
    if let Some(enc) = totp_enc {
        let Some(code) = r.totp.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            // 密码对了、还差验证码：这一条照样计数（不写审计）。不计的话知道密码的人能不限次数地让中控算 argon2，
            // 占满 HASH_SEM 挤掉别人的登录；本人先不带码、再带码登录只多一条，成功后随本来源的记录一起清掉
            return Err(totp_required());
        };
        match check_factor(&st, uid, &enc, code).await? {
            None => {
                mark_wrong_code(&st, attempt).await?;
                record_failure(&st, &uname, "auth.login_failed", Some(uid), json!({"ip": ip_s, "reason": "totp"})).await?;
                return Err(totp_invalid());
            }
            Some(f) => recovery = f == Factor::Recovery,
        }
    }
    let tok = random_token(32);
    let exp = OffsetDateTime::now_utc() + Duration::days(SESSION_DAYS);
    let mut tx = st.db.begin().await?;
    sqlx::query("insert into session (id_hash, user_id, expires_at, ip, user_agent) values ($1, $2, $3, $4, $5)")
        .bind(sha256(tok.as_bytes()))
        .bind(uid)
        .bind(exp)
        .bind(ip.map(ipnetwork::IpNetwork::from))
        .bind(user_agent(&headers))
        .execute(&mut *tx)
        .await?;
    clear_failures(&mut tx, &uname, group).await?;
    // 记下这个来源登录成功过（含两步验证）：之后不受「同一用户名不分来源」的总上限
    if let Some(g) = group {
        sqlx::query("insert into login_source (user_id, ip) values ($1, $2) on conflict (user_id, ip) do update set last_ok_at = now()")
            .bind(uid)
            .bind(ipnetwork::IpNetwork::from(g))
            .execute(&mut *tx)
            .await?;
    }
    audit(&mut tx, &r.username, "auth.login", "user", Some(uid), json!({"ip": ip_s})).await?;
    if recovery {
        let left: i64 = sqlx::query_scalar("select count(*) from recovery_code where user_id = $1 and used_at is null").bind(uid).fetch_one(&mut *tx).await?;
        audit(&mut tx, &r.username, "auth.totp_recovery_used", "user", Some(uid), json!({"ip": ip_s, "left": left})).await?;
    }
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
            st.revoked();
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

/// 跑着的中控里实时流的连接马上复核（命令行在别的进程里删会话，经 PostgreSQL 的 NOTIFY 转过去，见 lib::spawn_background）
pub const REVOKED_CHANNEL: &str = "fleet_revoked";

/// CLI：建用户 / 改密码（清掉该用户的会话、登录失败记录与登录成功过的来源：重设密码就是为了让他马上能登进来，
/// 旧来源可能是盗用密码的人的，不再免于「同一用户名不分来源」的上限）
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
    sqlx::query("delete from login_source where user_id = (select id from app_user where username = $1)").bind(username).execute(db).await?;
    sqlx::query("delete from login_failure where username = $1").bind(username).execute(db).await?;
    sqlx::query("select pg_notify($1, '')").bind(REVOKED_CHANNEL).execute(db).await?;
    Ok(())
}

/// CLI：停用某个用户的两步验证（手机和恢复码都丢了时由中控上的 root 执行），清掉登录成功过的来源。用户不存在时报错
pub async fn totp_reset(db: &sqlx::PgPool, username: &str) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    let uid: Option<Uuid> = sqlx::query_scalar(
        "update app_user set totp_secret_enc = null, totp_pending_enc = null, totp_pending_expires = null, totp_last_step = null
         where username = $1 returning id",
    )
    .bind(username)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(uid) = uid else { anyhow::bail!("没有用户 {username}") };
    sqlx::query("delete from recovery_code where user_id = $1").bind(uid).execute(&mut *tx).await?;
    // 登录成功过的来源一并清掉：其中可能有盗用账户的人的，不再免于总上限
    sqlx::query("delete from login_source where user_id = $1").bind(uid).execute(&mut *tx).await?;
    audit(&mut tx, "cli", "auth.totp_disabled", "user", Some(uid), json!({"via": "totp-reset", "username": username})).await?;
    tx.commit().await?;
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
        assert_eq!(needed(&g, "/api/v1/account/sessions"), Need::Session);
        assert_eq!(needed(&p, "/api/v1/account/totp/setup"), Need::Session);
        assert_eq!(needed(&g, "/api/v1/jobs/x/logs"), Need::Scope("read"));
        assert_eq!(needed(&p, "/api/v1/audit/ssh"), Need::Scope("exec"));
        assert_eq!(needed(&g, "/api/v1/audit"), Need::Scope("read"));
    }

    #[test]
    fn ip_group_masks_v6_to_64() {
        let g = |s: &str| ip_group(s.parse().unwrap()).to_string();
        assert_eq!(g("203.0.113.7"), "203.0.113.7");
        assert_eq!(g("2001:db8:1:2:aaaa:bbbb:cccc:dddd"), "2001:db8:1:2::");
        assert_eq!(g("2001:db8:1:2::1"), g("2001:db8:1:2:ffff::9"));
    }

    #[test]
    fn wait_text_units() {
        assert_eq!(wait_text(30), "30 秒");
        assert_eq!(wait_text(61), "2 分钟");
    }
}
