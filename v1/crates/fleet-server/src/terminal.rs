//! 网页终端：在浏览器里直接打开机器的 shell（用户常换设备，登录密钥只放在中控）。
//!
//! 中控本地开 PTY 跑 `ssh -tt`（中控自己的密钥与 known_hosts，见 `ssh::Target::interactive`），WebSocket 上二进制帧双向搬
//! 终端字节，文本帧是 JSON 控制消息。接口在 api/terminal.rs，协议见 docs/API.md「网页终端」。安全要求落在这里的部分：
//! - 票据：随机 32 字节，只存 sha256，60 秒有效，取出即删（对不对都删），绑定开票的会话与目标（`Hub::issue` / `Hub::redeem`）；
//!   握手之后的第一条消息才送票据，不进地址：反代与中控的请求日志都记不到它
//! - 开票前的再验证按会话记在内存里，5 分钟内验证过的免输（`Hub::verified_recently`，验证本身在 api/terminal.rs）
//! - 每人同时最多 3 个、全中控最多 8 个（`Hub::enter`）；空闲 15 分钟（浏览器没有输入）、最长 2 小时自动断开
//! - 会话失效即断开（`recheck` → `Hub::kick_session`）：收到撤销通知（`Event::Revoked`）时马上复核，另外每 60 秒复核一次兜底
//! - ssh 子进程：清空环境变量只留 PATH、TERM、LANG；自成进程组，结束时整组杀掉（`pty::kill_group`），远端 shell 随连接断开收到 SIGHUP
//! - 浏览器发来的一条消息最多 64 KiB；输出经有界通道，浏览器 15 秒收不走就断开；输入也有界，PTY 写不进去时先不读浏览器（TCP 背压）
//! - 开、关都写审计：目标（同机 IP 节点写明实际登录的宿主机）、时长、上下行字节数、断开原因；终端里输入的内容不记

use crate::crypto::{random_token, sha256};
use crate::error::{ApiError, ApiResult};
use crate::pty::{self, Pty};
use crate::ssh;
use crate::state::{AppState, Event};
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::IpAddr;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

/// 票据有效期
pub const TICKET_TTL: Duration = Duration::from_secs(60);
/// 再验证过之后这么久内开票免输
pub const VERIFY_TTL: Duration = Duration::from_secs(5 * 60);
/// 每人同时开着的终端
pub const PER_USER: usize = 3;
/// 全中控同时开着的终端（每个都是一个 ssh 进程加一对 PTY）
pub const GLOBAL: usize = 8;
/// 每人手里没用掉的票据（60 秒过期）：拿着会话反复开票也撑不大内存
const TICKETS_PER_USER: usize = 10;
/// 浏览器这么久没有输入（按键、粘贴；改窗口大小、光有输出都不算）就断开
pub const IDLE: Duration = Duration::from_secs(15 * 60);
/// 一个终端最长开这么久
pub const MAX_AGE: Duration = Duration::from_secs(2 * 3600);
const RECHECK_EVERY: Duration = Duration::from_secs(60);
/// 心跳：每隔这么久发一次 ping，到下一次还没收到 pong 就当连接已断
const PING_EVERY: Duration = Duration::from_secs(15);
/// 浏览器发来的一条消息最多这么大（粘贴大段文字时前端按这个切开发）
pub const MAX_MESSAGE: usize = 64 * 1024;
/// 握手之后等票据
const AUTH_WAIT: Duration = Duration::from_secs(10);
/// 一次从 PTY 读多少
const READ_CHUNK: usize = 16 * 1024;
/// 输出通道最多积这么多段（每段 ≤ READ_CHUNK）
const OUT_QUEUE: usize = 16;
/// 输入通道最多积这么多条浏览器消息（每条 ≤ MAX_MESSAGE）
const IN_QUEUE: usize = 4;
/// 浏览器这么久收不走输出就断开
const STALL: Duration = Duration::from_secs(15);
/// 结束时 SIGTERM 之后等 ssh 自己退出，过了再 SIGKILL
const KILL_GRACE: Duration = Duration::from_secs(3);
/// 窗口行、列数夹在 1..=DIM_MAX
const DIM_MAX: i64 = 500;

/// 网页要知道的上限（`GET /meta` 的 terminal）：粘贴按 max_message 切开发，提示文案里的数字也取这里
pub fn meta() -> Value {
    json!({"per_user": PER_USER, "global": GLOBAL, "idle_s": IDLE.as_secs(), "max_age_s": MAX_AGE.as_secs(),
           "max_message": MAX_MESSAGE, "max_dim": DIM_MAX, "ticket_ttl_s": TICKET_TTL.as_secs(), "verify_ttl_s": VERIFY_TTL.as_secs()})
}

/// 终端连的是什么
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Machine,
    /// 软路由（另一个分支做；合并后在 `resolve` 里接上）
    Router,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "machine" => Some(Kind::Machine),
            "router" => Some(Kind::Router),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Machine => "machine",
            Kind::Router => "router",
        }
    }
}

// ── 票据、再验证、名额 ─────────────────────────────────────────────────────

struct Ticket {
    session: Vec<u8>,
    user_id: Uuid,
    kind: Kind,
    id: Uuid,
    expires: Instant,
}

/// 兑换成功的票据
#[derive(Debug)]
pub struct Grant {
    pub user_id: Uuid,
    pub kind: Kind,
    pub id: Uuid,
}

struct Live {
    user_id: Uuid,
    session: Vec<u8>,
    /// 会话失效时发断开原因
    kick: watch::Sender<Option<String>>,
}

#[derive(Default)]
struct HubInner {
    /// sha256(票据) → 票据
    tickets: HashMap<Vec<u8>, Ticket>,
    /// 会话（id_hash）→ 最近一次开票时再验证通过的时间
    verified: HashMap<Vec<u8>, Instant>,
    /// 在开的终端
    live: HashMap<u64, Live>,
    next: u64,
}

/// 网页终端在内存里的全部状态（不落库：中控重启后票据作废、终端全断、再验证重来）
#[derive(Default)]
pub struct Hub {
    inner: Arc<Mutex<HubInner>>,
}

fn lock(m: &Mutex<HubInner>) -> MutexGuard<'_, HubInner> { m.lock().unwrap_or_else(|e| e.into_inner()) }

impl Hub {
    /// 这个会话 5 分钟内在开票时再验证过
    pub fn verified_recently(&self, session: &[u8]) -> bool { self.verified_at(session, Instant::now()) }

    fn verified_at(&self, session: &[u8], now: Instant) -> bool {
        let mut g = lock(&self.inner);
        g.verified.retain(|_, t| now.duration_since(*t) < VERIFY_TTL);
        g.verified.contains_key(session)
    }

    pub fn mark_verified(&self, session: &[u8]) { lock(&self.inner).verified.insert(session.to_vec(), Instant::now()); }

    /// 开一张票，返回票据明文（只交给浏览器；这里只存它的 sha256）
    pub fn issue(&self, session: &[u8], user_id: Uuid, kind: Kind, id: Uuid) -> Result<String, &'static str> {
        self.issue_at(session, user_id, kind, id, Instant::now())
    }

    fn issue_at(&self, session: &[u8], user_id: Uuid, kind: Kind, id: Uuid, now: Instant) -> Result<String, &'static str> {
        let tok = random_token(32);
        let mut g = lock(&self.inner);
        g.tickets.retain(|_, t| t.expires > now);
        if g.tickets.values().filter(|t| t.user_id == user_id).count() >= TICKETS_PER_USER {
            return Err("开终端太频繁，稍后再试");
        }
        g.tickets.insert(sha256(tok.as_bytes()), Ticket { session: session.to_vec(), user_id, kind, id, expires: now + TICKET_TTL });
        Ok(tok)
    }

    /// 兑换票据：取出即删（对不对都删，同一张票只有一次机会），过期的、不是这个会话开的不认
    pub fn redeem(&self, ticket: &str, session: &[u8]) -> Result<Grant, &'static str> { self.redeem_at(ticket, session, Instant::now()) }

    fn redeem_at(&self, ticket: &str, session: &[u8], now: Instant) -> Result<Grant, &'static str> {
        let t = lock(&self.inner).tickets.remove(&sha256(ticket.as_bytes()));
        match t {
            None => Err("票据不对或已经用过，请重新打开终端"),
            Some(t) if t.expires <= now => Err("票据已过期（60 秒内有效），请重新打开终端"),
            Some(t) if t.session != session => Err("票据不是这个登录会话开的，请重新打开终端"),
            Some(t) => Ok(Grant { user_id: t.user_id, kind: t.kind, id: t.id }),
        }
    }

    /// 还能不能再开一个：开票时先看一眼，连上时 `enter` 再算一次为准
    pub fn room(&self, user_id: Uuid) -> Result<(), &'static str> { room_in(&lock(&self.inner), user_id) }

    /// 占一个名额；返回的 Slot 放掉时归还
    pub fn enter(&self, user_id: Uuid, session: &[u8]) -> Result<Slot, &'static str> {
        let mut g = lock(&self.inner);
        room_in(&g, user_id)?;
        g.next += 1;
        let id = g.next;
        let (kick, kicked) = watch::channel(None);
        g.live.insert(id, Live { user_id, session: session.to_vec(), kick });
        Ok(Slot { hub: self.inner.clone(), id, kicked })
    }

    /// 会话失效（退出、被撤销、过期、用户被停用）：断开它开着的全部终端，它没用掉的票据与再验证记录一并作废。返回断了几个
    pub fn kick_session(&self, session: &[u8], reason: &str) -> usize {
        let mut g = lock(&self.inner);
        g.tickets.retain(|_, t| t.session != session);
        g.verified.remove(session);
        let mut n = 0;
        for l in g.live.values().filter(|l| l.session == session) {
            let _ = l.kick.send(Some(reason.to_string()));
            n += 1;
        }
        n
    }

    /// 在开的终端数
    pub fn live(&self) -> usize { lock(&self.inner).live.len() }
}

fn room_in(g: &HubInner, user_id: Uuid) -> Result<(), &'static str> {
    if g.live.values().filter(|l| l.user_id == user_id).count() >= PER_USER {
        return Err("每人最多同时开 3 个终端，先关掉一个再开");
    }
    if g.live.len() >= GLOBAL {
        return Err("中控同时最多开 8 个终端，现在已经满了，稍后再试");
    }
    Ok(())
}

/// 一个在开终端占的名额，放掉（drop）时归还
pub struct Slot {
    hub: Arc<Mutex<HubInner>>,
    id: u64,
    /// 会话失效被踢时收到原因
    pub kicked: watch::Receiver<Option<String>>,
}

impl Drop for Slot {
    fn drop(&mut self) { lock(&self.hub).live.remove(&self.id); }
}

// ── 目标 ───────────────────────────────────────────────────────────────────

/// 解析出来的 ssh 目标
pub struct Target {
    pub kind: Kind,
    pub id: Uuid,
    /// 显示名（审计、日志里写它）
    pub name: String,
    /// 同机 IP 节点：实际登录的宿主机（id、显示名）
    pub via: Option<(Uuid, String)>,
    pub ssh: ssh::Target,
}

impl Target {
    /// 审计里的目标：名字、实际登录的地址与端口、经哪台宿主机
    fn audit_detail(&self) -> Value {
        json!({"kind": self.kind.as_str(), "name": self.name, "ip": self.ssh.ip, "port": self.ssh.port,
               "host": self.via.as_ref().map(|(id, name)| json!({"id": id, "name": name}))})
    }
}

/// 按 kind 解析出 ssh 目标。开票时与连上时各查一次（中间可能已经退役）
pub async fn resolve(st: &AppState, kind: Kind, id: Uuid) -> ApiResult<Target> {
    match kind {
        Kind::Machine => machine(st, id).await,
        Kind::Router => Err(ApiError::bad_request("软路由终端稍后开放")),
    }
}

/// 机器：中控现有的密钥与 known_hosts（StrictHostKeyChecking=yes，同远程执行）；同机 IP 节点没有自己的 SSH，登录它的宿主机。
/// 已退役的、还没上架完成的（草稿、上架中、上架失败）、家宽代理（没有 SSH）不行
async fn machine(st: &AppState, id: Uuid) -> ApiResult<Target> {
    type Row = (String, String, String, String, i32, Option<Uuid>, Option<String>, Option<String>);
    let row: Option<Row> = sqlx::query_as(
        "select display_name(m.kind::text, m.label, t.name, m.number), m.kind::text, m.lifecycle::text,
                host(coalesce(h.primary_ip, m.primary_ip)), coalesce(h.ssh_port, m.ssh_port),
                h.id, display_name(h.kind::text, h.label, ht.name, h.number), h.lifecycle::text
         from machine m left join team t on t.id = m.team_id
              left join machine h on h.id = m.host_id left join team ht on ht.id = h.team_id
         where m.id = $1",
    )
    .bind(id)
    .fetch_optional(&st.db)
    .await?;
    let (name, kind, lifecycle, ip, port, host_id, host_name, host_lifecycle) = row.ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    match lifecycle.as_str() {
        "retired" => return Err(ApiError::conflict(format!("{name} 已退役，不能打开终端"))),
        "draft" | "provisioning" | "failed" => {
            return Err(ApiError::conflict(format!("{name} 还没上架完成，不能打开终端（上架失败的先在作业里重试或取消）")));
        }
        _ => {}
    }
    if kind == "proxy" {
        return Err(ApiError::conflict(format!("{name} 是家宽代理，没有 SSH，不能打开终端")));
    }
    if host_lifecycle.as_deref() == Some("retired") {
        return Err(ApiError::conflict(format!("{name} 的宿主机 {} 已退役，不能打开终端", host_name.as_deref().unwrap_or("?"))));
    }
    Ok(Target {
        kind: Kind::Machine,
        id,
        name,
        via: host_id.zip(host_name),
        ssh: ssh::Target { ip, port: port as u16, key: st.cfg.ssh_key.clone(), known_hosts: st.cfg.known_hosts.clone(), password: None },
    })
}

// ── 会话复核 ───────────────────────────────────────────────────────────────

/// 会话失效时的断开原因
const SESSION_GONE: &str = "登录会话已失效（已退出、被撤销、过期或账户被停用），终端已断开";

async fn session_valid(db: &sqlx::PgPool, session: &[u8]) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "select exists (select 1 from session s join app_user u on u.id = s.user_id
         where s.id_hash = $1 and s.expires_at > now() and u.disabled_at is null)",
    )
    .bind(session)
    .fetch_one(db)
    .await
}

/// 复核一个会话：失效了就断开它开着的全部终端（`Hub::kick_session`），返回是否仍有效。
/// 每个终端收到撤销通知（`Event::Revoked`，或广播落后、可能丢了它）时马上调，另外每 60 秒调一次
pub async fn recheck(st: &AppState, session: &[u8]) -> sqlx::Result<bool> {
    let ok = session_valid(&st.db, session).await?;
    if !ok {
        st.terminals.kick_session(session, SESSION_GONE);
    }
    Ok(ok)
}

// ── WebSocket 会话 ─────────────────────────────────────────────────────────

/// 连上来的是谁（握手时从会话 Cookie 得到）
pub struct Who {
    /// 会话的 id_hash（sha256(Cookie)）
    pub session: Vec<u8>,
    pub user_id: Uuid,
    pub user: String,
    pub ip: Option<IpAddr>,
}

/// 浏览器发来的文本消息
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Control {
    /// 第一条：票据，可带初始窗口大小
    Auth {
        ticket: String,
        #[serde(default)]
        cols: Option<i64>,
        #[serde(default)]
        rows: Option<i64>,
    },
    Resize {
        cols: i64,
        rows: i64,
    },
}

/// 行、列数夹在 1..=500
fn dim(v: i64) -> u16 { v.clamp(1, DIM_MAX) as u16 }

/// 文本消息里的改窗口大小（列, 行）；别的（不认识的类型、格式不对）不理
fn resize_of(t: &str) -> Option<(u16, u16)> {
    match serde_json::from_str::<Control>(t) {
        Ok(Control::Resize { cols, rows }) => Some((dim(cols), dim(rows))),
        _ => None,
    }
}

/// 告诉浏览器的结束原因与 WebSocket 关闭码
struct End {
    code: u16,
    reason: String,
}

impl End {
    fn normal(reason: impl Into<String>) -> End { End { code: 1000, reason: reason.into() } }
    /// 接口错误：4xx 原样给浏览器看，5xx 只进日志
    fn api(e: ApiError) -> End {
        if e.status.is_server_error() {
            tracing::error!(code = e.code, "网页终端：{}", e.message);
            return End { code: 1011, reason: "中控内部错误，请稍后再试".into() };
        }
        End::normal(e.message)
    }
    fn internal(what: &str, e: impl std::fmt::Display) -> End {
        tracing::error!("网页终端：{what}：{e}");
        End { code: 1011, reason: format!("{what}，请稍后再试") }
    }
}

/// WebSocket 升级之后：等票据、核对、起 ssh、搬运，结束时先告诉浏览器原因（`{"type":"exit","reason":…}`）再关。
/// `events` 在握手前就订阅了：等票据期间的撤销通知也不漏
pub async fn serve(mut ws: WebSocket, st: AppState, who: Who, events: broadcast::Receiver<Event>) {
    let end = run(&mut ws, &st, &who, events).await;
    let msg = json!({"type": "exit", "reason": end.reason}).to_string();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        ws.send(Message::Text(msg.into())).await?;
        ws.send(Message::Close(Some(CloseFrame { code: end.code, reason: "".into() }))).await
    })
    .await;
}

/// 第一条消息：`{"type":"auth","ticket":"…","cols"?,"rows"?}`。返回票据与初始窗口大小（没给按 80 × 24）
async fn first(ws: &mut WebSocket) -> Result<(String, u16, u16), End> {
    let m = match tokio::time::timeout(AUTH_WAIT, ws.recv()).await {
        Err(_) => return Err(End::normal("10 秒内没有收到票据，请重新打开终端")),
        Ok(Some(Ok(m))) => m,
        Ok(_) => return Err(End::normal("浏览器连接中断")),
    };
    if let Message::Text(t) = &m {
        if let Ok(Control::Auth { ticket, cols, rows }) = serde_json::from_str::<Control>(t.as_str()) {
            return Ok((ticket, cols.map_or(80, dim), rows.map_or(24, dim)));
        }
    }
    Err(End::normal("第一条消息应当是票据：{\"type\":\"auth\",\"ticket\":\"…\"}"))
}

async fn run(ws: &mut WebSocket, st: &AppState, who: &Who, mut events: broadcast::Receiver<Event>) -> End {
    let (ticket, cols, rows) = match first(ws).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let grant = match st.terminals.redeem(&ticket, &who.session) {
        Ok(g) => g,
        Err(m) => {
            tracing::info!(user = %who.user, "网页终端没打开：{m}");
            return End::normal(m);
        }
    };
    let target = match resolve(st, grant.kind, grant.id).await {
        Ok(t) => t,
        Err(e) => return End::api(e),
    };
    // 撤销通知从握手前就在收（之后的循环里处理）；握手到这里之间被撤销的这里再查一次
    match session_valid(&st.db, &who.session).await {
        Ok(true) => {}
        Ok(false) => return End { code: crate::api::misc::CLOSE_REVOKED, reason: SESSION_GONE.into() },
        Err(e) => return End::internal("核对登录会话失败", e),
    }
    let mut slot = match st.terminals.enter(who.user_id, &who.session) {
        Ok(s) => s,
        Err(m) => return End::normal(m),
    };
    let (pty, slave) = match Pty::open(cols, rows) {
        Ok(p) => p,
        Err(e) => return End::internal("打开伪终端失败", e),
    };
    let mut cmd = target.ssh.interactive();
    // 不把中控进程的环境变量（数据库地址、代理设置…）带给 ssh：只留这三个
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()))
        .env("TERM", "xterm-256color")
        .env("LANG", std::env::var_os("LANG").unwrap_or_else(|| "C.UTF-8".into()));
    // 先记「打开」再起 ssh：中控中途重启也留有记录
    let open_id = match audit_open(st, who, &target).await {
        Ok(id) => id,
        Err(e) => return End::internal("写审计失败，没有打开终端", e),
    };
    let t0 = Instant::now();
    let mut child = match pty::spawn(cmd, slave) {
        Ok(c) => c,
        Err(e) => {
            let reason = format!("启动 ssh 失败：{e}");
            audit_close(st, who, &target, open_id, &Closed { secs: 0, bytes_in: 0, bytes_out: 0, why: "spawn_failed", reason: &reason, exit_code: None }).await;
            return End { code: 1011, reason };
        }
    };
    tracing::info!(user = %who.user, target = %target.name, ip = %target.ssh.ip, "打开网页终端");
    let pty = Arc::new(pty);
    let (stop, bytes_in, bytes_out) = pump(ws, st, who, &pty, &mut slot.kicked, &mut events).await;
    let status = pty::kill_group(&mut child, KILL_GRACE).await;
    drop(slot);
    let status = match status {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("网页终端：等 ssh 退出失败：{e}");
            None
        }
    };
    let reason = stop.describe(status);
    let secs = t0.elapsed().as_secs();
    tracing::info!(user = %who.user, target = %target.name, secs, bytes_in, bytes_out, "网页终端断开：{reason}");
    let exit_code = status.and_then(|s| s.code());
    audit_close(st, who, &target, open_id, &Closed { secs, bytes_in, bytes_out, why: stop.code(), reason: &reason, exit_code }).await;
    let code = match stop {
        Stop::Kicked(_) => crate::api::misc::CLOSE_REVOKED,
        Stop::TooLarge => 1009,
        Stop::RecheckFailed | Stop::Error(_) => 1011,
        _ => 1000,
    };
    End { code, reason }
}

/// 为什么停
enum Stop {
    /// PTY 读到 EOF：ssh 退出了（远端 shell 退出、连不上、连接断了）
    RemoteExit,
    ClientClosed,
    ClientGone,
    Idle,
    MaxAge,
    /// 会话失效被踢
    Kicked(String),
    /// 浏览器收不走输出
    Stalled,
    /// 浏览器发来的消息太大
    TooLarge,
    RecheckFailed,
    Error(String),
}

impl Stop {
    /// 审计里的原因代码
    fn code(&self) -> &'static str {
        match self {
            Stop::RemoteExit => "remote_exit",
            Stop::ClientClosed => "client_closed",
            Stop::ClientGone => "client_gone",
            Stop::Idle => "idle",
            Stop::MaxAge => "max_age",
            Stop::Kicked(_) => "session_invalid",
            Stop::Stalled => "stalled",
            Stop::TooLarge => "too_large",
            Stop::RecheckFailed => "recheck_failed",
            Stop::Error(_) => "error",
        }
    }

    /// 给人看的原因（浏览器、审计）
    fn describe(&self, status: Option<ExitStatus>) -> String {
        match self {
            Stop::RemoteExit => match status.and_then(|s| s.code()) {
                Some(0) => "远端 shell 已退出".into(),
                Some(255) => "ssh 断开了（退出码 255：连不上、登录被拒、主机指纹不对，或中控在重启），详细原因见终端里 ssh 的提示".into(),
                Some(c) => format!("远端 shell 已退出（退出码 {c}）"),
                None => "ssh 被信号结束".into(),
            },
            Stop::ClientClosed => "浏览器关闭了终端".into(),
            Stop::ClientGone => "浏览器连接中断".into(),
            Stop::Idle => "15 分钟没有输入，已自动断开".into(),
            Stop::MaxAge => "已连续开了 2 小时，自动断开（可以重新打开）".into(),
            Stop::Kicked(r) => r.clone(),
            Stop::Stalled => "浏览器 15 秒收不走终端输出，已断开".into(),
            Stop::TooLarge => format!("浏览器发来的消息太大（一条最多 {} KiB），已断开", MAX_MESSAGE / 1024),
            Stop::RecheckFailed => "复核登录会话失败，已断开，请重新打开".into(),
            Stop::Error(e) => e.clone(),
        }
    }
}

/// 发一帧。发不出去时返回为什么停：浏览器 STALL 内收不走（不读、TCP 窗口满）是 Stalled，连接已断是 ClientGone
async fn send(ws: &mut WebSocket, m: Message) -> Result<(), Stop> {
    match tokio::time::timeout(STALL, ws.send(m)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(Stop::ClientGone),
        Err(_) => Err(Stop::Stalled),
    }
}

/// PTY → 输出通道的任务为什么结束
enum ReadEnd {
    Eof,
    Stalled,
    Failed(String),
}

/// PTY → 输出通道。通道满了先等（停读期间 ssh 的输出积在内核的 PTY 缓冲里，ssh 随之不再读网络、远端被流控），
/// STALL 内还腾不出位置就结束（主循环随即断开这个终端）
async fn read_pty(pty: Arc<Pty>, tx: mpsc::Sender<Bytes>) -> ReadEnd {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match pty.read(&mut buf).await {
            Ok(0) => return ReadEnd::Eof,
            Ok(n) => match tx.send_timeout(Bytes::copy_from_slice(&buf[..n]), STALL).await {
                Ok(()) => {}
                Err(mpsc::error::SendTimeoutError::Timeout(_)) => return ReadEnd::Stalled,
                // 主循环已经结束
                Err(mpsc::error::SendTimeoutError::Closed(_)) => return ReadEnd::Eof,
            },
            Err(e) => return ReadEnd::Failed(format!("读终端输出失败：{e}")),
        }
    }
}

/// 输入通道 → PTY。ssh 不读时在这里等，通道满了主循环就先不读浏览器
async fn write_pty(pty: Arc<Pty>, mut rx: mpsc::Receiver<Bytes>) {
    while let Some(b) = rx.recv().await {
        // 写不进去（EIO）就是 ssh 已经退出：读那边随即读到 EOF，由它结束
        if pty.write_all(&b).await.is_err() {
            return;
        }
    }
}

/// 结束时取消两个搬运任务
struct AbortOnDrop(Vec<tokio::task::AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

/// 搬运，直到有一方结束。返回结束原因与上行（浏览器 → 机器）、下行（机器 → 浏览器）字节数
async fn pump(
    ws: &mut WebSocket,
    st: &AppState,
    who: &Who,
    pty: &Arc<Pty>,
    kicked: &mut watch::Receiver<Option<String>>,
    events: &mut broadcast::Receiver<Event>,
) -> (Stop, u64, u64) {
    let (out_tx, mut out_rx) = mpsc::channel::<Bytes>(OUT_QUEUE);
    let (in_tx, in_rx) = mpsc::channel::<Bytes>(IN_QUEUE);
    let mut reader = tokio::spawn(read_pty(pty.clone(), out_tx));
    let writer = tokio::spawn(write_pty(pty.clone(), in_rx));
    let _abort = AbortOnDrop(vec![reader.abort_handle(), writer.abort_handle()]);
    let now = tokio::time::Instant::now();
    let max_age = tokio::time::sleep(MAX_AGE);
    let idle = tokio::time::sleep(IDLE);
    tokio::pin!(max_age, idle);
    let mut ping = tokio::time::interval_at(now + PING_EVERY, PING_EVERY);
    let mut check = tokio::time::interval_at(now + RECHECK_EVERY, RECHECK_EVERY);
    let mut pong_due = false;
    // 输入通道满了时手里那一条：送进去之前不再读浏览器
    let mut pending: Option<Bytes> = None;
    let (mut bytes_in, mut bytes_out) = (0u64, 0u64);
    // 不加 biased：输出一直有（比如 `yes`）时，输入（Ctrl-C）照样轮得到
    let stop = loop {
        tokio::select! {
            r = kicked.changed() => {
                let why = if r.is_ok() { kicked.borrow().clone() } else { None };
                break Stop::Kicked(why.unwrap_or_else(|| SESSION_GONE.to_string()));
            }
            _ = &mut max_age => break Stop::MaxAge,
            _ = &mut idle => break Stop::Idle,
            e = events.recv() => match e {
                // 有会话被撤销，或广播落后、可能丢了撤销通知：马上复核自己的会话，失效的由 recheck 踢掉（下一轮从 kicked 取到）
                Ok(Event::Revoked) | Err(RecvError::Lagged(_)) => {
                    if let Err(e) = recheck(st, &who.session).await {
                        tracing::warn!("网页终端复核会话失败：{e}");
                        break Stop::RecheckFailed;
                    }
                }
                Ok(_) => {}
                Err(RecvError::Closed) => break Stop::Error("中控正在关闭".into()),
            },
            o = out_rx.recv() => match o {
                Some(b) => {
                    bytes_out += b.len() as u64;
                    if let Err(s) = send(ws, Message::Binary(b)).await {
                        break s;
                    }
                }
                // 读的任务结束了（它拿着唯一的发送端）
                None => break match (&mut reader).await {
                    Ok(ReadEnd::Eof) => Stop::RemoteExit,
                    Ok(ReadEnd::Stalled) => Stop::Stalled,
                    Ok(ReadEnd::Failed(e)) => Stop::Error(e),
                    Err(e) => Stop::Error(format!("读终端输出的任务异常结束：{e}")),
                },
            },
            p = in_tx.reserve(), if pending.is_some() => match p {
                Ok(permit) => {
                    if let Some(b) = pending.take() {
                        permit.send(b);
                    }
                }
                // 写的任务已结束（ssh 退出了）：丢掉，等读那边的 EOF
                Err(_) => pending = None,
            },
            m = ws.recv(), if pending.is_none() => match m {
                Some(Ok(Message::Binary(b))) => {
                    bytes_in += b.len() as u64;
                    idle.as_mut().reset(tokio::time::Instant::now() + IDLE);
                    if let Err(mpsc::error::TrySendError::Full(b)) = in_tx.try_send(b) {
                        pending = Some(b);
                    }
                }
                // 改窗口大小不算输入：前端布局变化也会发，不能让忘了关的终端一直挂着
                Some(Ok(Message::Text(t))) => {
                    if let Some((cols, rows)) = resize_of(t.as_str()) {
                        if let Err(e) = pty.resize(cols, rows) {
                            tracing::debug!("网页终端改窗口大小失败：{e}");
                        }
                    }
                }
                Some(Ok(Message::Pong(_))) => pong_due = false,
                Some(Ok(Message::Ping(_))) => {}
                Some(Ok(Message::Close(_))) => break Stop::ClientClosed,
                // 超过 MAX_MESSAGE 的消息在收的时候就被拒（握手时设的上限），报的是「Message too long」
                Some(Err(e)) if e.to_string().contains("too long") => break Stop::TooLarge,
                Some(Err(_)) | None => break Stop::ClientGone,
            },
            _ = ping.tick() => {
                if pong_due {
                    break Stop::ClientGone;
                }
                if let Err(s) = send(ws, Message::Ping(Bytes::new())).await {
                    break s;
                }
                pong_due = true;
            }
            _ = check.tick() => {
                if let Err(e) = recheck(st, &who.session).await {
                    tracing::warn!("网页终端复核会话失败：{e}");
                    break Stop::RecheckFailed;
                }
            }
        }
    };
    (stop, bytes_in, bytes_out)
}

// ── 审计 ───────────────────────────────────────────────────────────────────

fn session_id(session: &[u8]) -> String { hex::encode(session.get(..8).unwrap_or(session)) }

/// 「打开」：谁（actor）、哪台、实际登录的地址（同机 IP 节点是宿主机的）、从哪个 IP、哪个登录会话（同「账户安全」里会话的 id）
async fn audit_open(st: &AppState, who: &Who, t: &Target) -> sqlx::Result<i64> {
    let mut d = t.audit_detail();
    d["client_ip"] = json!(who.ip.map(|i| i.to_string()));
    d["session"] = json!(session_id(&who.session));
    sqlx::query_scalar("insert into audit_log (actor, action, target_kind, target_id, detail) values ($1, 'terminal.open', $2, $3, $4) returning id")
        .bind(&who.user)
        .bind(t.kind.as_str())
        .bind(t.id)
        .bind(d)
        .fetch_one(&st.db)
        .await
}

struct Closed<'a> {
    secs: u64,
    bytes_in: u64,
    bytes_out: u64,
    why: &'static str,
    reason: &'a str,
    exit_code: Option<i32>,
}

/// 「关闭」：目标同「打开」，另记时长、上下行字节数、为什么断（`why` 是代码、`reason` 是给人看的）、ssh 的退出码。
/// 终端已经开过了：写不进去只记日志
async fn audit_close(st: &AppState, who: &Who, t: &Target, open_id: i64, c: &Closed<'_>) {
    let mut d = t.audit_detail();
    d["open_id"] = json!(open_id);
    d["session"] = json!(session_id(&who.session));
    d["duration_s"] = json!(c.secs);
    d["bytes_in"] = json!(c.bytes_in);
    d["bytes_out"] = json!(c.bytes_out);
    d["why"] = json!(c.why);
    d["reason"] = json!(c.reason);
    d["exit_code"] = json!(c.exit_code);
    let r = sqlx::query("insert into audit_log (actor, action, target_kind, target_id, detail) values ($1, 'terminal.close', $2, $3, $4)")
        .bind(&who.user)
        .bind(t.kind.as_str())
        .bind(t.id)
        .bind(d)
        .execute(&st.db)
        .await;
    if let Err(e) = r {
        tracing::error!(open_id, "网页终端：写「关闭」审计失败：{e}");
    }
}

/// 字节数（十进制，同流量）
fn size(n: u64) -> String {
    match n {
        n if n < 1000 => format!("{n} 字节"),
        n if n < 1_000_000 => format!("{:.1} KB", n as f64 / 1e3),
        n => format!("{:.1} MB", n as f64 / 1e6),
    }
}

fn duration(s: u64) -> String {
    match s {
        s if s < 60 => format!("{s} 秒"),
        s if s < 3600 => format!("{} 分 {} 秒", s / 60, s % 60),
        s => format!("{} 小时 {} 分", s / 3600, s % 3600 / 60),
    }
}

/// 审计「打开 / 关闭网页终端」的一句话摘要（api::misc::audit_text 用）
pub fn audit_summary(action: &str, d: &Value) -> String {
    let name = d["name"].as_str().unwrap_or("");
    let target = match d["host"]["name"].as_str() {
        Some(h) => format!("{name}（登录宿主机 {h}）"),
        None => name.to_string(),
    };
    if action != "terminal.close" {
        return target;
    }
    let n = |k: &str| d[k].as_u64().unwrap_or(0);
    format!(
        "{target} · {} · 上行 {} / 下行 {} · {}",
        duration(n("duration_s")),
        size(n("bytes_in")),
        size(n("bytes_out")),
        d["reason"].as_str().unwrap_or("")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: u8) -> Vec<u8> { vec![b; 32] }

    #[test]
    fn ticket_is_one_time_bound_and_expiring() {
        let h = Hub::default();
        let (u, m) = (Uuid::now_v7(), Uuid::now_v7());
        let t0 = Instant::now();
        let tk = h.issue_at(&s(1), u, Kind::Machine, m, t0).unwrap();
        assert_eq!(tk.len(), 43, "32 字节 base64url");
        let g = h.redeem_at(&tk, &s(1), t0 + Duration::from_secs(59)).unwrap();
        assert_eq!((g.user_id, g.kind, g.id), (u, Kind::Machine, m));
        assert!(h.redeem_at(&tk, &s(1), t0).is_err(), "只能用一次");
        // 别的会话拿去用：不认，而且这一下就把票作废了
        let tk = h.issue_at(&s(1), u, Kind::Machine, m, t0).unwrap();
        assert!(h.redeem_at(&tk, &s(2), t0).unwrap_err().contains("会话"));
        assert!(h.redeem_at(&tk, &s(1), t0).is_err());
        // 60 秒过期
        let tk = h.issue_at(&s(1), u, Kind::Machine, m, t0).unwrap();
        assert!(h.redeem_at(&tk, &s(1), t0 + TICKET_TTL).unwrap_err().contains("过期"));
        assert!(h.redeem_at("瞎编的", &s(1), t0).is_err());
        // 内存里只有 sha256
        let tk = h.issue_at(&s(1), u, Kind::Machine, m, t0).unwrap();
        assert!(lock(&h.inner).tickets.keys().all(|k| k.len() == 32 && k != tk.as_bytes()));
    }

    #[test]
    fn outstanding_tickets_are_capped_per_user() {
        let h = Hub::default();
        let (u, m) = (Uuid::now_v7(), Uuid::now_v7());
        let t0 = Instant::now();
        for _ in 0..TICKETS_PER_USER {
            h.issue_at(&s(1), u, Kind::Machine, m, t0).unwrap();
        }
        assert!(h.issue_at(&s(1), u, Kind::Machine, m, t0).is_err());
        assert!(h.issue_at(&s(2), Uuid::now_v7(), Kind::Machine, m, t0).is_ok(), "别人不受影响");
        // 过期的不算
        assert!(h.issue_at(&s(1), u, Kind::Machine, m, t0 + TICKET_TTL).is_ok());
    }

    #[test]
    fn verification_lasts_five_minutes_per_session() {
        let h = Hub::default();
        let t0 = Instant::now();
        lock(&h.inner).verified.insert(s(1), t0);
        assert!(h.verified_at(&s(1), t0 + Duration::from_secs(299)));
        assert!(!h.verified_at(&s(2), t0), "按会话记，别的会话不算");
        assert!(!h.verified_at(&s(1), t0 + VERIFY_TTL));
        assert!(lock(&h.inner).verified.is_empty(), "过期的顺手清掉");
    }

    #[test]
    fn slots_per_user_and_global() {
        let h = Hub::default();
        let a = Uuid::now_v7();
        let mut held: Vec<Slot> = (0..PER_USER).map(|_| h.enter(a, &s(1)).unwrap()).collect();
        assert!(h.enter(a, &s(9)).err().unwrap().contains("每人"), "同一用户换个会话也一样");
        assert!(h.room(a).is_err());
        held.pop();
        assert!(h.room(a).is_ok(), "放掉一个就能再开");
        held.push(h.enter(a, &s(1)).unwrap());
        // 全中控 8 个
        for i in 0..(GLOBAL - PER_USER) {
            let u = if i < 3 { Uuid::from_u128(1) } else { Uuid::from_u128(2) };
            held.push(h.enter(u, &s(2)).unwrap());
        }
        assert_eq!(h.live(), GLOBAL);
        assert!(h.enter(Uuid::now_v7(), &s(3)).err().unwrap().contains("中控"));
        drop(held);
        assert_eq!(h.live(), 0);
    }

    #[test]
    fn kick_session_reaches_only_its_terminals() {
        let h = Hub::default();
        let u = Uuid::now_v7();
        let a1 = h.enter(u, &s(1)).unwrap();
        let a2 = h.enter(u, &s(1)).unwrap();
        let b = h.enter(u, &s(2)).unwrap();
        h.issue(&s(1), u, Kind::Machine, u).unwrap();
        let other = h.issue(&s(2), u, Kind::Machine, u).unwrap();
        lock(&h.inner).verified.insert(s(1), Instant::now());
        assert_eq!(h.kick_session(&s(1), "会话没了"), 2);
        assert_eq!(a1.kicked.borrow().as_deref(), Some("会话没了"));
        assert_eq!(a2.kicked.borrow().as_deref(), Some("会话没了"));
        assert_eq!(*b.kicked.borrow(), None);
        // 那个会话没用掉的票据、再验证记录一并作废，别的会话的不动
        assert_eq!(lock(&h.inner).tickets.len(), 1);
        assert!(!h.verified_recently(&s(1)));
        assert!(h.redeem(&other, &s(2)).is_ok());
    }

    #[test]
    fn control_messages() {
        assert_eq!(resize_of(r#"{"type":"resize","cols":120,"rows":40}"#), Some((120, 40)));
        // 夹在 1..=500
        assert_eq!(resize_of(r#"{"type":"resize","cols":0,"rows":-3}"#), Some((1, 1)));
        assert_eq!(resize_of(r#"{"type":"resize","cols":100000,"rows":501}"#), Some((500, 500)));
        // 不是整数、缺字段、不认识的类型、不是 JSON：不理
        assert_eq!(resize_of(r#"{"type":"resize","cols":"120","rows":40}"#), None);
        assert_eq!(resize_of(r#"{"type":"resize","cols":12.5,"rows":40}"#), None);
        assert_eq!(resize_of(r#"{"type":"resize","cols":120}"#), None);
        assert_eq!(resize_of(r#"{"type":"bell"}"#), None);
        assert_eq!(resize_of("stty size"), None);
        assert!(matches!(
            serde_json::from_str::<Control>(r#"{"type":"auth","ticket":"x","cols":132}"#),
            Ok(Control::Auth { cols: Some(132), rows: None, .. })
        ));
    }

    #[test]
    fn audit_summaries() {
        let open = json!({"name": "团队1-03", "ip": "203.0.113.5", "port": 22, "host": {"id": Uuid::nil(), "name": "团队1-01"}});
        assert_eq!(audit_summary("terminal.open", &open), "团队1-03（登录宿主机 团队1-01）");
        let close = json!({"name": "线路机2", "ip": "198.51.100.2", "port": 22, "host": null, "duration_s": 754, "bytes_in": 1234,
                           "bytes_out": 3_456_789, "reason": "浏览器关闭了终端"});
        assert_eq!(audit_summary("terminal.close", &close), "线路机2 · 12 分 34 秒 · 上行 1.2 KB / 下行 3.5 MB · 浏览器关闭了终端");
        assert_eq!(duration(7385), "2 小时 3 分");
        assert_eq!(size(999), "999 字节");
    }
}
