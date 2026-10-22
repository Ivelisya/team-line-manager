//! 账户安全：两步验证（TOTP + 恢复码）、登录会话、改密码。只对会话登录的用户开放（API 密钥一律 403，见 auth::needed）。
//! 要再次输入密码 / 验证码的操作与登录共用失败限速（按用户名 + 来源）：拿到会话的人也不能借这些接口无限次猜。

use super::ok;
use crate::auth::{self, Actor, ClientIp, Factor, COOKIE, HASH_SEM};
use crate::crypto::sha256;
use crate::db::{audit, Tx};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::state::AppState;
use crate::totp;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::IpAddr;
use time::OffsetDateTime;
use uuid::Uuid;

const MIN_PASSWORD: usize = 12;

fn uid(a: &Actor) -> ApiResult<Uuid> {
    a.require_session()?;
    a.user_id.ok_or_else(ApiError::unauthorized)
}

/// 当前会话的 id_hash
fn current_hash(jar: &CookieJar) -> ApiResult<Vec<u8>> {
    jar.get(COOKIE).map(|c| sha256(c.value().as_bytes())).ok_or_else(ApiError::unauthorized)
}

/// 已启用的 TOTP 密钥（密文）
async fn totp_enc(st: &AppState, uid: Uuid) -> ApiResult<Option<Vec<u8>>> {
    Ok(sqlx::query_scalar("select totp_secret_enc from app_user where id = $1").bind(uid).fetch_one(&st.db).await?)
}

/// 换一组新的恢复码（旧的全部作废），返回明文（只这一次）
async fn replace_recovery(tx: &mut Tx<'_>, uid: Uuid) -> sqlx::Result<Vec<String>> {
    sqlx::query("delete from recovery_code where user_id = $1").bind(uid).execute(&mut **tx).await?;
    let codes = totp::new_recovery_codes();
    let hashes: Vec<Vec<u8>> = codes.iter().map(|c| totp::recovery_hash(c)).collect();
    sqlx::query("insert into recovery_code (user_id, code_hash) select $1, h from unnest($2::bytea[]) h").bind(uid).bind(&hashes).execute(&mut **tx).await?;
    Ok(codes)
}

/// 一次「再次校验」：开头按失败记一条（限速同登录），密码 / 验证码都对了在 `done` 里清掉，任何一步不对都记审计并回 401
struct Recheck<'a> {
    st: &'a AppState,
    name: &'a str,
    uid: Uuid,
    /// 限速键：同登录，用户名截到 64 个字符
    key: String,
    ip: Option<IpAddr>,
    op: &'static str,
}

impl<'a> Recheck<'a> {
    async fn begin(st: &'a AppState, a: &'a Actor, uid: Uuid, ip: Option<IpAddr>, op: &'static str) -> ApiResult<Self> {
        let key: String = a.name.chars().take(64).collect();
        auth::begin_attempt(st, &key, ip.map(auth::ip_group)).await?;
        Ok(Self { st, name: &a.name, uid, key, ip, op })
    }

    async fn fail(&self, reason: &str, e: ApiError) -> ApiError {
        let d = json!({"ip": self.ip.map(|i| i.to_string()), "op": self.op, "reason": reason});
        match auth::record_failure(self.st, self.name, "auth.verify_failed", Some(self.uid), d).await {
            Ok(()) => e,
            Err(x) => x,
        }
    }

    async fn password(&self, pw: &str) -> ApiResult<()> {
        let phc: String = sqlx::query_scalar("select password_hash from app_user where id = $1").bind(self.uid).fetch_one(&self.st.db).await?;
        let good = {
            let _p = HASH_SEM.acquire().await.map_err(|e| ApiError::internal(format!("限流信号量：{e}")))?;
            auth::verify_blocking(pw.to_string(), Some(phc)).await
        };
        if good {
            return Ok(());
        }
        Err(self.fail("password", ApiError::new(StatusCode::UNAUTHORIZED, "bad_password", "当前密码不对")).await)
    }

    async fn factor(&self, enc: &[u8], code: &str) -> ApiResult<Factor> {
        match auth::check_factor(self.st, self.uid, enc, code).await? {
            Some(f) => Ok(f),
            None => Err(self.fail("totp", auth::totp_invalid()).await),
        }
    }

    /// 都过了：清掉本来源的失败记录；用的是恢复码时记一笔
    async fn done(&self, tx: &mut Tx<'_>, f: Option<Factor>) -> ApiResult<()> {
        auth::clear_failures(tx, &self.key, self.ip.map(auth::ip_group)).await?;
        if f == Some(Factor::Recovery) {
            let left: i64 = sqlx::query_scalar("select count(*) from recovery_code where user_id = $1 and used_at is null").bind(self.uid).fetch_one(&mut **tx).await?;
            audit(tx, self.name, "auth.totp_recovery_used", "user", Some(self.uid), json!({"ip": self.ip.map(|i| i.to_string()), "op": self.op, "left": left})).await?;
        }
        Ok(())
    }
}

// ── 两步验证 ───────────────────────────────────────────────────────────────

pub async fn totp_status(a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let uid = uid(&a)?;
    let (enabled, left): (bool, i64) = sqlx::query_as(
        "select totp_secret_enc is not null, (select count(*) from recovery_code r where r.user_id = u.id and r.used_at is null) from app_user u where id = $1",
    )
    .bind(uid)
    .fetch_one(&st.db)
    .await?;
    Ok(ok(json!({"enabled": enabled, "recovery_codes_left": left})))
}

/// 认证器里显示的账户名：用户名@中控域名（实验环境与生产分得开）
fn account_label(st: &AppState, user: &str) -> String {
    let host = st.cfg.public_url.split("://").nth(1).unwrap_or("").split(['/', ':']).next().unwrap_or("");
    if host.is_empty() { user.to_string() } else { format!("{user}@{host}") }
}

/// 生成一把待确认的密钥（10 分钟内用 enable 确认），返回给认证器扫的二维码与手动输入用的 base32
pub async fn totp_setup(a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let uid = uid(&a)?;
    if totp_enc(&st, uid).await?.is_some() {
        return Err(ApiError::conflict("两步验证已经开着；要换手机请先停用再重新开启"));
    }
    let secret = totp::new_secret();
    sqlx::query("update app_user set totp_pending_enc = $2, totp_pending_expires = now() + interval '10 minutes' where id = $1")
        .bind(uid)
        .bind(st.secrets.seal(&secret))
        .execute(&st.db)
        .await?;
    let b32 = totp::base32(&secret);
    let url = totp::otpauth_url("Fleet", &account_label(&st, &a.name), &b32);
    let svg = totp::qr_svg(&url).map_err(|e| ApiError::internal(format!("生成二维码：{e}")))?;
    Ok(ok(json!({"secret": b32, "otpauth_url": url, "qr_svg": svg})))
}

#[derive(Deserialize)]
pub struct CodeIn {
    code: String,
}

/// 用认证器上的验证码确认待定的密钥，启用两步验证，返回一组恢复码（只这一次）
pub async fn totp_enable(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<CodeIn>) -> ApiResult<Json<Value>> {
    let uid = uid(&a)?;
    let code = totp::parse_code(&b.code).ok_or_else(|| ApiError::bad_request("验证码是 6 位数字"))?;
    let mut tx = st.db.begin().await?;
    let (enabled, pending): (bool, Option<Vec<u8>>) = sqlx::query_as(
        "select totp_secret_enc is not null, case when totp_pending_expires > now() then totp_pending_enc end from app_user where id = $1 for update",
    )
    .bind(uid)
    .fetch_one(&mut *tx)
    .await?;
    if enabled {
        return Err(ApiError::conflict("两步验证已经开着"));
    }
    let pending = pending.ok_or_else(|| ApiError::bad_request("没有待确认的密钥，或已超过 10 分钟，请重新开始"))?;
    let secret = st.secrets.open(&pending).map_err(|e| ApiError::internal(format!("解密 TOTP 密钥：{e}")))?;
    let step = totp::matching_step(&secret, code, totp::now_step(), None)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "totp_invalid", "验证码不对：确认扫的是刚才的二维码、手机时间准确"))?;
    // 确认用的这个码同时记作「已用过」，不能紧接着拿它登录
    sqlx::query(
        "update app_user set totp_secret_enc = totp_pending_enc, totp_pending_enc = null, totp_pending_expires = null, totp_last_step = $2 where id = $1",
    )
    .bind(uid)
    .bind(step)
    .execute(&mut *tx)
    .await?;
    let codes = replace_recovery(&mut tx, uid).await?;
    audit(&mut tx, &a.name, "auth.totp_enabled", "user", Some(uid), json!({})).await?;
    tx.commit().await?;
    Ok(ok(json!({"recovery_codes": codes})))
}

#[derive(Deserialize)]
pub struct DisableIn {
    password: String,
    code: String,
}

/// 停用两步验证：要当前密码 + 验证码（或恢复码）
pub async fn totp_disable(a: Actor, State(st): State<AppState>, ClientIp(ip): ClientIp, ApiJson(b): ApiJson<DisableIn>) -> ApiResult<Json<Value>> {
    let uid = uid(&a)?;
    let enc = totp_enc(&st, uid).await?.ok_or_else(|| ApiError::conflict("两步验证没有开"))?;
    if b.code.trim().is_empty() {
        return Err(ApiError::bad_request("请输入两步验证码或恢复码"));
    }
    let r = Recheck::begin(&st, &a, uid, ip, "totp_disable").await?;
    r.password(&b.password).await?;
    let f = r.factor(&enc, b.code.trim()).await?;
    let mut tx = st.db.begin().await?;
    r.done(&mut tx, Some(f)).await?;
    sqlx::query("update app_user set totp_secret_enc = null, totp_pending_enc = null, totp_pending_expires = null, totp_last_step = null where id = $1")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("delete from recovery_code where user_id = $1").bind(uid).execute(&mut *tx).await?;
    audit(&mut tx, &a.name, "auth.totp_disabled", "user", Some(uid), json!({"ip": ip.map(|i| i.to_string())})).await?;
    tx.commit().await?;
    Ok(ok(json!({"ok": true})))
}

/// 重新生成恢复码（旧的全部作废）：要验证码（或一个恢复码）
pub async fn recovery_regenerate(a: Actor, State(st): State<AppState>, ClientIp(ip): ClientIp, ApiJson(b): ApiJson<CodeIn>) -> ApiResult<Json<Value>> {
    let uid = uid(&a)?;
    let enc = totp_enc(&st, uid).await?.ok_or_else(|| ApiError::conflict("两步验证没有开"))?;
    if b.code.trim().is_empty() {
        return Err(ApiError::bad_request("请输入两步验证码"));
    }
    let r = Recheck::begin(&st, &a, uid, ip, "recovery_regenerate").await?;
    let f = r.factor(&enc, b.code.trim()).await?;
    let mut tx = st.db.begin().await?;
    r.done(&mut tx, Some(f)).await?;
    let codes = replace_recovery(&mut tx, uid).await?;
    audit(&mut tx, &a.name, "auth.totp_recovery_regenerated", "user", Some(uid), json!({"ip": ip.map(|i| i.to_string())})).await?;
    tx.commit().await?;
    Ok(ok(json!({"recovery_codes": codes})))
}

// ── 登录会话 ───────────────────────────────────────────────────────────────

#[derive(Serialize, sqlx::FromRow)]
struct SessionOut {
    /// id_hash 前 8 字节的十六进制：只用来指认会话，推不出 Cookie
    id: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    last_seen_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    ip: Option<String>,
    user_agent: Option<String>,
    current: bool,
}

pub async fn sessions(a: Actor, State(st): State<AppState>, jar: CookieJar) -> ApiResult<Json<Value>> {
    let uid = uid(&a)?;
    let v: Vec<SessionOut> = sqlx::query_as(
        "select encode(substring(id_hash from 1 for 8), 'hex') as id, created_at, last_seen_at, expires_at, host(ip) as ip, user_agent,
                id_hash = $2 as current
         from session where user_id = $1 and expires_at > now() order by id_hash = $2 desc, last_seen_at desc",
    )
    .bind(uid)
    .bind(current_hash(&jar)?)
    .fetch_all(&st.db)
    .await?;
    Ok(ok(v))
}

pub async fn revoke_session(a: Actor, State(st): State<AppState>, jar: CookieJar, ApiPath(id): ApiPath<String>) -> ApiResult<Json<Value>> {
    let uid = uid(&a)?;
    let prefix = hex::decode(&id).ok().filter(|b| b.len() == 8).ok_or_else(|| ApiError::not_found("没有这个会话"))?;
    let mut tx = st.db.begin().await?;
    let gone: Option<(Option<String>, bool)> = sqlx::query_as(
        "delete from session where user_id = $1 and substring(id_hash from 1 for 8) = $2 returning host(ip), id_hash = $3",
    )
    .bind(uid)
    .bind(&prefix)
    .bind(current_hash(&jar)?)
    .fetch_optional(&mut *tx)
    .await?;
    let (sip, current) = gone.ok_or_else(|| ApiError::not_found("没有这个会话（可能已经过期或被撤销）"))?;
    audit(&mut tx, &a.name, "auth.session_revoked", "user", Some(uid), json!({"session": id, "ip": sip, "current": current})).await?;
    tx.commit().await?;
    Ok(ok(json!({"ok": true, "current": current})))
}

pub async fn revoke_other_sessions(a: Actor, State(st): State<AppState>, jar: CookieJar) -> ApiResult<Json<Value>> {
    let uid = uid(&a)?;
    let mut tx = st.db.begin().await?;
    let n = sqlx::query("delete from session where user_id = $1 and id_hash <> $2").bind(uid).bind(current_hash(&jar)?).execute(&mut *tx).await?.rows_affected();
    if n > 0 {
        audit(&mut tx, &a.name, "auth.session_revoked", "user", Some(uid), json!({"others": n})).await?;
    }
    tx.commit().await?;
    Ok(ok(json!({"revoked": n})))
}

// ── 改密码 ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PasswordIn {
    current: String,
    new: String,
    #[serde(default)]
    totp: Option<String>,
}

/// 改密码：核对当前密码（开了两步验证还要验证码），成功后撤销本设备以外的所有会话
pub async fn change_password(
    a: Actor,
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    jar: CookieJar,
    ApiJson(b): ApiJson<PasswordIn>,
) -> ApiResult<Json<Value>> {
    let uid = uid(&a)?;
    let cur = current_hash(&jar)?;
    if b.new.chars().count() < MIN_PASSWORD {
        return Err(ApiError::bad_request(format!("新密码至少 {MIN_PASSWORD} 个字符")));
    }
    if b.new.trim().eq_ignore_ascii_case(a.name.trim()) {
        return Err(ApiError::bad_request("新密码不能和用户名一样"));
    }
    let enc = totp_enc(&st, uid).await?;
    let code = b.totp.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if enc.is_some() && code.is_none() {
        return Err(auth::totp_required());
    }
    let r = Recheck::begin(&st, &a, uid, ip, "password_change").await?;
    r.password(&b.current).await?;
    let f = match (&enc, code) {
        (Some(e), Some(c)) => Some(r.factor(e, c).await?),
        _ => None,
    };
    // 当前密码核对过之后才比：否则这里就成了不限次数猜当前密码的捷径
    if b.new == b.current {
        let mut tx = st.db.begin().await?;
        r.done(&mut tx, f).await?;
        tx.commit().await?;
        return Err(ApiError::bad_request("新密码和当前密码一样"));
    }
    let phc = {
        let _p = HASH_SEM.acquire().await.map_err(|e| ApiError::internal(format!("限流信号量：{e}")))?;
        let pw = b.new.clone();
        tokio::task::spawn_blocking(move || auth::hash_password(&pw))
            .await
            .map_err(|e| ApiError::internal(format!("计算密码哈希：{e}")))?
            .map_err(|e| ApiError::internal(format!("计算密码哈希：{e}")))?
    };
    let mut tx = st.db.begin().await?;
    r.done(&mut tx, f).await?;
    sqlx::query("update app_user set password_hash = $2 where id = $1").bind(uid).bind(phc).execute(&mut *tx).await?;
    let n = sqlx::query("delete from session where user_id = $1 and id_hash <> $2").bind(uid).bind(&cur).execute(&mut *tx).await?.rows_affected();
    audit(&mut tx, &a.name, "auth.password_changed", "user", Some(uid), json!({"ip": ip.map(|i| i.to_string()), "revoked_sessions": n})).await?;
    tx.commit().await?;
    Ok(ok(json!({"ok": true, "revoked_sessions": n})))
}
