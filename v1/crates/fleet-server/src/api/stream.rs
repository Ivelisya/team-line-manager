//! 实时流（WebSocket）：资源变更通知、实时指标，连接期间复核会话 / 密钥
use crate::auth::Actor;
use crate::crypto::sha256;
use crate::error::ApiError;
use crate::state::{AppState, Event};
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::CookieJar;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::broadcast::{self, error::RecvError};

/// 实时流连接用的凭据，用来定期复核「会话/密钥还有效吗」
enum Cred {
    Session(Vec<u8>),
    Key(Vec<u8>),
}

impl Cred {
    fn hash(&self) -> &[u8] {
        match self {
            Cred::Session(h) | Cred::Key(h) => h,
        }
    }
}

/// 同一个凭据（会话 / 密钥）最多同时开这么多条实时流：多个标签页、网页终端旁的那条、断线重连时还没被服务端发现的旧连接加起来也到不了
const MAX_PER_CRED: usize = 32;
/// 各凭据（哈希）现在开着几条
static OPEN: LazyLock<Mutex<HashMap<Vec<u8>, usize>>> = LazyLock::new(Default::default);

/// 一条实时流占的名额；连接结束（正常关闭、断线、任务 panic）时 Drop 还回去
struct Slot(Vec<u8>);

impl Slot {
    /// 满了返回 None
    fn take(c: &Cred) -> Option<Self> {
        let mut m = crate::lock(&OPEN);
        let n = m.entry(c.hash().to_vec()).or_default();
        (*n < MAX_PER_CRED).then(|| {
            *n += 1;
            Self(c.hash().to_vec())
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut m = crate::lock(&OPEN);
        if let Some(n) = m.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.0);
            }
        }
    }
}

async fn still_valid(db: &sqlx::PgPool, c: &Cred) -> sqlx::Result<bool> {
    match c {
        Cred::Session(h) => {
            sqlx::query_scalar::<_, bool>(
                "select exists (select 1 from session s join app_user u on u.id = s.user_id
                 where s.id_hash = $1 and s.expires_at > now() and u.disabled_at is null)",
            )
            .bind(h)
            .fetch_one(db)
            .await
        }
        Cred::Key(h) => {
            sqlx::query_scalar::<_, bool>(
                "select exists (select 1 from api_key where key_hash = $1 and revoked_at is null and (expires_at is null or expires_at > now()))",
            )
            .bind(h)
            .fetch_one(db)
            .await
        }
    }
}

/// 心跳：每隔这么久发一次 ping（另带一个文本心跳 `{"type":"hb"}`），到下一次 ping 还没收到 pong 就当连接已死
const PING_EVERY: Duration = Duration::from_secs(15);
const RECHECK_EVERY: Duration = Duration::from_secs(60);
/// 会话/密钥失效时的关闭码（4000–4999 归应用自用）
pub const CLOSE_REVOKED: u16 = 4401;
/// 同一凭据的连接数到上限时的关闭码（1008 = 违反策略）。握手前拒绝的 HTTP 错误浏览器脚本读不到原因，升级后用关闭帧带上
pub const CLOSE_TOO_MANY: u16 = 1008;

/// 实时流（WebSocket，JSON 文本帧）：资源变更通知 + 实时指标（`live.rs`）。
/// 广播落后丢了消息时发 `resync`（前端让全部查询失效）；每 60 s 复核一次会话/密钥，被撤销就以 4401 关闭；
/// 撤销会话 / 密钥的地方另发 `Event::Revoked`，各连接马上复核。每个凭据最多同时 [`MAX_PER_CRED`] 条，超出的升级后立刻以 1008 关闭。
pub async fn stream(a: Actor, State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    // 会话（Cookie）连接要求 Origin 是中控自己（public_url 的协议 + 主机 + 端口）：WebSocket 握手不受同源策略约束，
    // 别的页面能借浏览器自动带上的 Cookie 连上来读实时数据。API 密钥不是浏览器带的，不查
    if a.is_session() {
        let want = reqwest::Url::parse(&st.cfg.public_url).map(|u| u.origin().ascii_serialization()).unwrap_or_default();
        if headers.get("origin").and_then(|v| v.to_str().ok()) != Some(want.as_str()) {
            return ApiError::forbidden("实时流只接受本站页面的连接（Origin 不符）").into_response();
        }
    }
    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    let cred = match (bearer, jar.get(crate::auth::COOKIE)) {
        (Some(k), _) => Some(Cred::Key(sha256(k.as_bytes()))),
        (None, Some(c)) => Some(Cred::Session(sha256(c.value().as_bytes()))),
        _ => None,
    };
    // 握手前就订阅：升级期间的消息也不漏
    let rx = st.events.subscribe();
    let db = st.db.clone();
    ws.on_upgrade(move |socket| serve_ws(socket, rx, db, cred))
}

async fn serve_ws(mut ws: WebSocket, mut rx: broadcast::Receiver<Event>, db: sqlx::PgPool, cred: Option<Cred>) {
    let _slot = match cred.as_ref().map(Slot::take) {
        Some(None) => {
            let close = CloseFrame { code: CLOSE_TOO_MANY, reason: "同一登录的实时连接过多，请关掉多余的标签页".into() };
            send(&mut ws, Message::Close(Some(close))).await;
            return;
        }
        slot => slot.flatten(),
    };
    let start = tokio::time::Instant::now();
    let mut ping = tokio::time::interval_at(start + PING_EVERY, PING_EVERY);
    let mut check = tokio::time::interval_at(start + RECHECK_EVERY, RECHECK_EVERY);
    let mut pong_due = false;
    loop {
        let text = tokio::select! {
            r = rx.recv() => match r {
                // 有会话 / 密钥被撤销：马上复核，不等下一轮
                Ok(Event::Revoked) => {
                    if !keep(&mut ws, &db, &cred).await {
                        return;
                    }
                    continue;
                }
                Ok(e) => e.to_text(),
                Err(RecvError::Lagged(n)) => {
                    tracing::debug!("实时流订阅者落后 {n} 条消息，通知前端重新同步");
                    // 丢的消息里可能有 Revoked
                    if !keep(&mut ws, &db, &cred).await {
                        return;
                    }
                    Event::Resync.to_text()
                }
                Err(RecvError::Closed) => return,
            },
            m = ws.recv() => match m {
                Some(Ok(Message::Pong(_))) => {
                    pong_due = false;
                    continue;
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => continue,
            },
            _ = ping.tick() => {
                // 协议层的 ping 浏览器脚本看不到：另发一个文本心跳，前端据此发现半开的连接（web/src/lib/stream.ts）
                if pong_due
                    || !send(&mut ws, Message::Ping(Default::default())).await
                    || !send(&mut ws, Message::Text(Utf8Bytes::from_static(r#"{"type":"hb"}"#))).await
                {
                    return;
                }
                pong_due = true;
                continue;
            }
            _ = check.tick() => {
                if !keep(&mut ws, &db, &cred).await {
                    return;
                }
                continue;
            }
        };
        if !send(&mut ws, Message::Text(text)).await {
            return;
        }
    }
}

/// 复核会话 / 密钥：仍有效返回 true；已失效以 4401 关闭、复核查询失败以 1011 关闭（客户端重连时重新鉴权），返回 false
async fn keep(ws: &mut WebSocket, db: &sqlx::PgPool, cred: &Option<Cred>) -> bool {
    let Some(c) = cred else { return true };
    let close = match still_valid(db, c).await {
        Ok(true) => return true,
        Ok(false) => CloseFrame { code: CLOSE_REVOKED, reason: "会话或密钥已失效".into() },
        Err(e) => {
            tracing::warn!("实时流复核会话失败：{e}");
            CloseFrame { code: 1011, reason: "复核会话失败，请重连".into() }
        }
    };
    send(ws, Message::Close(Some(close))).await;
    false
}

/// 发一帧；对端卡住（不读、TCP 窗口满）超过一个心跳周期就当连接已死，返回 false 由调用方断开
async fn send(ws: &mut WebSocket, m: Message) -> bool {
    matches!(tokio::time::timeout(PING_EVERY, ws.send(m)).await, Ok(Ok(())))
}
