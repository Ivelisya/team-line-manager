//! 网页终端的接口：开票（先再验证一次身份）与 WebSocket。只给会话用户，API 密钥一律 403（auth::needed 把 /terminal/ 整个划给会话）。
//! 会话本身、协议与各项限制见 crate::terminal。

use super::ok;
use crate::auth::{self, Actor, ClientIp, Factor, COOKIE, HASH_SEM};
use crate::crypto::sha256;
use crate::db::audit;
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::state::AppState;
use crate::terminal::{self, Kind, Who};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::IpAddr;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Deserialize)]
pub struct TicketIn {
    /// machine | router
    kind: String,
    id: Uuid,
    /// 开了两步验证时：6 位验证码或一个恢复码
    #[serde(default)]
    code: Option<String>,
    /// 没开两步验证时：当前密码
    #[serde(default)]
    password: Option<String>,
}

#[derive(Serialize)]
struct TicketOut {
    ticket: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

fn limit(m: &'static str) -> ApiError { ApiError::new(StatusCode::TOO_MANY_REQUESTS, "terminal_limit", m) }

/// 开一张终端票据（60 秒内在 WebSocket 上用一次）。先看目标能不能开、名额够不够（不必为打不开的终端先输验证码），
/// 再验证身份：这个会话 5 分钟内验证过的免输
pub async fn create_ticket(
    a: Actor,
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    jar: CookieJar,
    ApiJson(b): ApiJson<TicketIn>,
) -> ApiResult<Json<Value>> {
    a.require_session()?;
    let uid = a.user_id.ok_or_else(ApiError::unauthorized)?;
    let session = jar.get(COOKIE).map(|c| sha256(c.value().as_bytes())).ok_or_else(ApiError::unauthorized)?;
    let kind = Kind::parse(&b.kind).ok_or_else(|| ApiError::bad_request("kind 只能是 machine 或 router"))?;
    terminal::resolve(&st, kind, b.id).await?;
    st.terminals.room(uid).map_err(limit)?;
    if !st.terminals.verified_recently(&session) {
        reverify(&st, &a, uid, ip, b.code.as_deref(), b.password.as_deref()).await?;
        st.terminals.mark_verified(&session);
    }
    let ticket = st.terminals.issue(&session, uid, kind, b.id).map_err(|m| ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", m))?;
    Ok(ok(TicketOut { ticket, expires_at: OffsetDateTime::now_utc() + terminal::TICKET_TTL }))
}

fn password_required() -> ApiError { ApiError::new(StatusCode::UNAUTHORIZED, "password_required", "请输入当前密码") }
fn password_invalid() -> ApiError { ApiError::new(StatusCode::UNAUTHORIZED, "password_invalid", "当前密码不对") }

/// 开票前的再验证：开了两步验证只要验证码（或恢复码），没开只要当前密码。缺了直接回 totp_required / password_required（不算失败）；
/// 输错回 totp_invalid / password_invalid，与登录共用失败计数和限速（auth::begin_attempt，验证码错另计每小时上限），
/// 写审计 auth.verify_failed（op = terminal）。做法同 account.rs 的 Recheck，只是两样只要一样、错误码按终端的约定
async fn reverify(st: &AppState, a: &Actor, uid: Uuid, ip: Option<IpAddr>, code: Option<&str>, password: Option<&str>) -> ApiResult<()> {
    let enc: Option<Vec<u8>> = sqlx::query_scalar("select totp_secret_enc from app_user where id = $1").bind(uid).fetch_one(&st.db).await?;
    let code = code.map(str::trim).filter(|s| !s.is_empty());
    let password = password.filter(|s| !s.is_empty());
    match (&enc, code, password) {
        (Some(_), None, _) => return Err(auth::totp_required()),
        (None, _, None) => return Err(password_required()),
        _ => {}
    }
    // 限速键同登录：用户名截到 64 个字符
    let key: String = a.name.chars().take(64).collect();
    let group = ip.map(auth::ip_group);
    let attempt = auth::begin_attempt(st, &key, group).await?;
    let ip_s = ip.map(|i| i.to_string());
    let fail = |reason: &'static str| auth::record_failure(st, &a.name, "auth.verify_failed", Some(uid), json!({"ip": ip_s, "op": "terminal", "reason": reason}));
    let factor = match (&enc, code, password) {
        (Some(enc), Some(code), _) => match auth::check_factor(st, uid, enc, code).await? {
            Some(f) => Some(f),
            None => {
                auth::mark_wrong_code(st, attempt).await?;
                fail("totp").await?;
                return Err(auth::totp_invalid());
            }
        },
        (_, _, Some(pw)) => {
            let phc: String = sqlx::query_scalar("select password_hash from app_user where id = $1").bind(uid).fetch_one(&st.db).await?;
            let good = {
                let _p = HASH_SEM.acquire().await.map_err(|e| ApiError::internal(format!("限流信号量：{e}")))?;
                auth::verify_blocking(pw.to_string(), Some(phc)).await
            };
            if !good {
                fail("password").await?;
                return Err(password_invalid());
            }
            None
        }
        // 上面已经按缺什么回过了
        _ => return Err(password_required()),
    };
    // 都对了：清掉本来源的失败记录；用的是恢复码时记一笔
    let mut tx = st.db.begin().await?;
    auth::clear_failures(&mut tx, &key, group).await?;
    if factor == Some(Factor::Recovery) {
        let left: i64 = sqlx::query_scalar("select count(*) from recovery_code where user_id = $1 and used_at is null").bind(uid).fetch_one(&mut *tx).await?;
        audit(&mut tx, &a.name, "auth.totp_recovery_used", "user", Some(uid), json!({"ip": ip_s, "op": "terminal", "left": left})).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// WebSocket。握手时核对 Origin 与会话 Cookie（失败是普通的 HTTP 401 / 403）；升级之后第一条消息送票据，见 crate::terminal
pub async fn ws(a: Actor, State(st): State<AppState>, ClientIp(ip): ClientIp, jar: CookieJar, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let Some(uid) = a.user_id else {
        return ApiError::forbidden("网页终端只能由登录用户使用，API 密钥不行").into_response();
    };
    // WebSocket 握手不受同源策略约束：Origin 必须是中控自己（public_url 的协议 + 主机 + 端口），别的网页借浏览器带上的 Cookie 连不上来
    let want = reqwest::Url::parse(&st.cfg.public_url).map(|u| u.origin().ascii_serialization()).unwrap_or_default();
    if headers.get("origin").and_then(|v| v.to_str().ok()) != Some(want.as_str()) {
        return ApiError::forbidden("网页终端只接受本站页面的连接（Origin 不符）").into_response();
    }
    let Some(c) = jar.get(COOKIE) else { return ApiError::unauthorized().into_response() };
    let who = Who { session: sha256(c.value().as_bytes()), user_id: uid, user: a.name, ip };
    // 握手前就订阅撤销通知：等票据期间会话被撤销也不漏
    let events = st.events.subscribe();
    ws.max_message_size(terminal::MAX_MESSAGE)
        .max_frame_size(terminal::MAX_MESSAGE)
        .on_upgrade(move |socket| terminal::serve(socket, st, who, events))
}
