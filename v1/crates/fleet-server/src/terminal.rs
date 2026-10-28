//! 网页终端：在浏览器里直接打开机器的 shell（用户常换设备，登录密钥只放在中控）。
//!
//! 中控本地开 PTY 跑 `ssh -tt`（中控自己的密钥与 known_hosts，见 `ssh::Target::interactive`；软路由经它的反向隧道，
//! 见 `routers::tunnel::interactive`），WebSocket 上二进制帧双向搬
//! 终端字节，文本帧是 JSON 控制消息。接口在 api/terminal.rs，协议见 docs/API.md「网页终端」。安全要求落在这里的部分：
//! - 票据：随机 32 字节，只存 sha256，60 秒有效，取出即删（对不对都删），绑定开票的会话与目标（`Hub::issue` / `Hub::redeem`）；
//!   握手之后的第一条消息才送票据，不进地址：反代与中控的请求日志都记不到它。等票据的连接每个会话最多 5 个（`Hub::wait_ticket`）
//! - 开票前的再验证按会话记在内存里，5 分钟内验证过的免输（`Hub::verified_recently`，验证本身在 api/terminal.rs）
//! - 每人同时最多 3 个、全中控最多 8 个（`Hub::enter`）；空闲 15 分钟（浏览器没有输入）、最长 2 小时自动断开
//! - 会话失效即断开（`recheck` → `Hub::kick_session`）：收到撤销通知（`Event::Revoked`）时马上复核，另外每 60 秒复核一次兜底；
//!   复核查库、往浏览器写都不在主循环里等（写有单独的任务），踢人、空闲、最长时限随时生效
//! - 中控退出前以「中控正在重启」断开全部终端、写完「关闭」审计（`shutdown`），之后不再开新的
//! - ssh 子进程：清空环境变量只留 PATH、TERM、LANG；自成进程组，结束时整组杀掉（`pty::kill_group`），远端 shell 随连接断开收到 SIGHUP
//! - 浏览器发来的一条消息最多 64 KiB；输出经有界通道，浏览器 15 秒收不走就断开；输入也有界，PTY 写不进去时先不读浏览器（TCP 背压）
//! - 开、关都写审计：目标（同机 IP 节点写明实际登录的宿主机）、时长、上下行字节数、断开原因；终端里输入的内容不记

use crate::crypto::{random_token, sha256};
use crate::error::{ApiError, ApiResult};
use crate::lock;
use crate::pty::{self, Pty};
use crate::ssh;
use crate::state::{AppState, Event};
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::IpAddr;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::{mpsc, oneshot, watch};
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
/// 每个会话握手之后还在等票据的连接（每个占一个任务和一个广播接收端，最多等 AUTH_WAIT）
const WAITING_PER_SESSION: usize = 5;
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
/// 控制帧（心跳、exit）通道
const CTL_QUEUE: usize = 4;
/// 输入通道最多积这么多条浏览器消息（每条 ≤ MAX_MESSAGE）
const IN_QUEUE: usize = 4;
/// 浏览器这么久收不走输出就断开
const STALL: Duration = Duration::from_secs(15);
/// 结束时 SIGTERM 之后等 ssh 自己退出，过了再 SIGKILL
const KILL_GRACE: Duration = Duration::from_secs(3);
/// 中控退出时等 ssh 的时间要短：整个收尾最多等两秒（`shutdown`）
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
/// 窗口行、列数夹在 1..=DIM_MAX
const DIM_MAX: i64 = 500;

/// 网页要知道的上限（`GET /meta` 的 terminal）：粘贴按 max_message 切开发，提示文案里的数字也取这里
pub fn meta() -> Value {
    json!({"per_user": PER_USER, "global": GLOBAL, "idle_s": IDLE.as_secs(), "max_age_s": MAX_AGE.as_secs(),
           "max_message": MAX_MESSAGE, "max_dim": DIM_MAX, "ticket_ttl_s": TICKET_TTL.as_secs(), "verify_ttl_s": VERIFY_TTL.as_secs()})
}

/// 本站的 Origin（public_url 的协议 + 主机 + 端口）。不是 http(s) 这类有主机的地址时为 None：不透明的 origin 序列化出来是
/// 「null」，会和沙箱 iframe、file:// 页面发来的 `Origin: null` 对上
pub fn site_origin(public_url: &str) -> Option<String> {
    let o = reqwest::Url::parse(public_url).ok()?.origin();
    o.is_tuple().then(|| o.ascii_serialization())
}

/// 终端连的是什么
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Machine,
    /// 软路由：经它的反向隧道登录（routers::tunnel::interactive）
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

/// 为什么被踢
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kick {
    /// 登录会话失效（附原因）
    Session(String),
    /// 中控正在重启
    Shutdown,
}

struct Live {
    user_id: Uuid,
    session: Vec<u8>,
    kick: watch::Sender<Option<Kick>>,
}

#[derive(Default)]
struct HubInner {
    /// sha256(票据) → 票据
    tickets: HashMap<Vec<u8>, Ticket>,
    /// 会话（id_hash）→ 最近一次开票时再验证通过的时间
    verified: HashMap<Vec<u8>, Instant>,
    /// 会话 → 握手之后还在等票据的连接数
    waiting: HashMap<Vec<u8>, usize>,
    /// 在开的终端
    live: HashMap<u64, Live>,
    next: u64,
    /// 中控在退出：不再开新的
    closing: bool,
}

/// 网页终端在内存里的全部状态（不落库：中控重启后票据作废、终端全断、再验证重来）
#[derive(Default)]
pub struct Hub {
    inner: Arc<Mutex<HubInner>>,
}

/// 中控在退出时开票、连上的说法
const CLOSING: &str = "中控正在重启，稍后再开终端";

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
        if g.closing {
            return Err(CLOSING);
        }
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

    /// 握手之后等票据的连接占一个位置（每个会话最多 WAITING_PER_SESSION 个），返回的 Waiting 放掉时归还
    pub fn wait_ticket(&self, session: &[u8]) -> Result<Waiting, &'static str> {
        let mut g = lock(&self.inner);
        let n = g.waiting.entry(session.to_vec()).or_default();
        if *n >= WAITING_PER_SESSION {
            return Err("这个登录会话同时在打开的终端太多，稍后再试");
        }
        *n += 1;
        Ok(Waiting { hub: self.inner.clone(), session: session.to_vec() })
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
            let _ = l.kick.send(Some(Kick::Session(reason.to_string())));
            n += 1;
        }
        n
    }

    /// 中控要退出了：断开全部终端（「中控正在重启」），没用掉的票据作废，之后不再开票、不再开新的。返回断了几个
    pub fn close_all(&self) -> usize {
        let mut g = lock(&self.inner);
        g.closing = true;
        g.tickets.clear();
        for l in g.live.values() {
            let _ = l.kick.send(Some(Kick::Shutdown));
        }
        g.live.len()
    }

    /// 中控在退出
    pub fn closing(&self) -> bool { lock(&self.inner).closing }

    /// 在开的终端数
    pub fn live(&self) -> usize { lock(&self.inner).live.len() }
}

fn room_in(g: &HubInner, user_id: Uuid) -> Result<(), &'static str> {
    if g.closing {
        return Err(CLOSING);
    }
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
    /// 被踢时收到原因
    pub kicked: watch::Receiver<Option<Kick>>,
}

impl Drop for Slot {
    fn drop(&mut self) { lock(&self.hub).live.remove(&self.id); }
}

/// 一个等票据的连接占的位置，放掉时归还
pub struct Waiting {
    hub: Arc<Mutex<HubInner>>,
    session: Vec<u8>,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let mut g = lock(&self.hub);
        if let Some(n) = g.waiting.get_mut(&self.session) {
            *n -= 1;
            if *n == 0 {
                g.waiting.remove(&self.session);
            }
        }
    }
}

/// 中控退出前调（收到 SIGTERM / SIGINT 后马上）：以「中控正在重启」断开全部终端（浏览器收到 exit、写「关闭」审计），
/// 之后不再开新的；最多等 `wait` 让它们收完尾（名额在写完「关闭」审计之后才归还）
pub async fn shutdown(st: &AppState, wait: Duration) {
    if st.terminals.close_all() == 0 {
        return;
    }
    let deadline = tokio::time::Instant::now() + wait;
    while st.terminals.live() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let left = st.terminals.live();
    if left > 0 {
        tracing::warn!("还有 {left} 个网页终端没收完尾，「关闭」审计可能没写上");
    }
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
    /// 实际登录的地址与端口（软路由是中控本机的隧道端口）
    pub ip: String,
    pub port: u16,
    login: Login,
}

/// 怎么登录
enum Login {
    Machine(ssh::Target),
    /// 经软路由连到中控的反向隧道（同 `fleetctl ssh router`，routers::tunnel）
    Router,
}

impl Target {
    /// 审计里的目标：名字、实际登录的地址与端口、经哪台宿主机
    fn audit_detail(&self) -> Value {
        json!({"kind": self.kind.as_str(), "name": self.name, "ip": self.ip, "port": self.port,
               "host": self.via.as_ref().map(|(id, name)| json!({"id": id, "name": name}))})
    }

    /// 交给 PTY 的 ssh 命令
    fn command(&self, cfg: &crate::config::Config) -> tokio::process::Command {
        match &self.login {
            Login::Machine(t) => t.interactive(),
            Login::Router => crate::routers::tunnel::interactive(cfg, self.id, self.port.into()),
        }
    }
}

/// 按 kind 解析出 ssh 目标。开票时与连上时各查一次（中间可能已经退役、删除）
pub async fn resolve(st: &AppState, kind: Kind, id: Uuid) -> ApiResult<Target> {
    match kind {
        Kind::Machine => machine(st, id).await,
        Kind::Router => router(st, id).await,
    }
}

/// 软路由：经它的反向隧道（127.0.0.1:<隧道端口>），中控密钥，主机公钥按 router-<id> 在 [routers] known_hosts 里核对。
/// 还没接入过（没记下主机公钥）、现在离线（隧道没连上）的不开：开了也只会连不上
async fn router(st: &AppState, id: Uuid) -> ApiResult<Target> {
    let r = crate::routers::load(&st.db, Some(id)).await?.into_iter().next().ok_or_else(|| ApiError::not_found("没有这台软路由"))?;
    match r.state() {
        "pending" => return Err(ApiError::conflict(format!("软路由「{}」还没接入过，不能打开终端：在路由上执行接入命令、等它在线后再开", r.name))),
        "offline" => {
            let why = r.error.as_deref().map(|e| format!("（{e}）")).unwrap_or_default();
            return Err(ApiError::conflict(format!("软路由「{}」现在离线{why}，连不上：等它在线后再开终端", r.name)));
        }
        _ => {}
    }
    Ok(Target { kind: Kind::Router, id, name: r.name, via: None, ip: "127.0.0.1".into(), port: r.tunnel_port as u16, login: Login::Router })
}

/// 这个生命周期的机器能不能登录：已退役的、还没上架完成的（草稿、上架中、上架失败）不行。`who` 是说法里的主语
fn usable(who: &str, lifecycle: &str) -> ApiResult<()> {
    match lifecycle {
        "retired" => Err(ApiError::conflict(format!("{who} 已退役，不能打开终端"))),
        "draft" | "provisioning" | "failed" => Err(ApiError::conflict(format!("{who} 还没上架完成，不能打开终端（上架失败的先在作业里重试或取消）"))),
        _ => Ok(()),
    }
}

/// 机器：中控现有的密钥与 known_hosts（StrictHostKeyChecking=yes，同远程执行）；同机 IP 节点没有自己的 SSH，登录它的宿主机
/// （宿主机同样要能登录）。家宽代理没有 SSH
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
    usable(&name, &lifecycle)?;
    if kind == "proxy" {
        return Err(ApiError::conflict(format!("{name} 是家宽代理，没有 SSH，不能打开终端")));
    }
    if let (Some(hn), Some(hl)) = (&host_name, &host_lifecycle) {
        usable(&format!("{name} 的宿主机 {hn}"), hl)?;
    }
    Ok(Target {
        kind: Kind::Machine,
        id,
        name,
        via: host_id.zip(host_name),
        ip: ip.clone(),
        port: port as u16,
        login: Login::Machine(ssh::Target { ip, port: port as u16, key: st.cfg.ssh_key.clone(), known_hosts: st.cfg.known_hosts.clone(), password: None }),
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

/// 主循环用的复核：在单独的任务里查库（最多要几秒，不能卡住踢人、空闲、最长时限），同一时刻只跑一次；
/// 跑的期间又要复核（又来了撤销通知）就记下，跑完再来一次。失效的由 `recheck` 踢掉，主循环从 kicked 收到
struct Rechecker {
    tx: mpsc::Sender<sqlx::Result<bool>>,
    rx: mpsc::Receiver<sqlx::Result<bool>>,
    running: bool,
    again: bool,
}

impl Rechecker {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel(1);
        Self { tx, rx, running: false, again: false }
    }

    fn start(&mut self, st: &AppState, session: &[u8]) {
        if self.running {
            self.again = true;
            return;
        }
        self.running = true;
        let (st, session, tx) = (st.clone(), session.to_vec(), self.tx.clone());
        tokio::spawn(async move {
            let _ = tx.send(recheck(&st, &session).await).await;
        });
    }

    /// 一次复核跑完：查库失败返回错误；中途又要复核的接着再跑
    fn finished(&mut self, r: sqlx::Result<bool>, st: &AppState, session: &[u8]) -> sqlx::Result<()> {
        self.running = false;
        r?;
        if std::mem::take(&mut self.again) {
            self.start(st, session);
        }
        Ok(())
    }
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

/// 往浏览器写的终端输出；最后的 exit 也可以走这里，排在已有的输出后面
enum Out {
    Data(Bytes),
    Exit(End),
}

/// 先于积着的输出发的控制帧：心跳；不必等输出发完、马上告诉浏览器为什么断的 exit
enum Ctl {
    Ping,
    Exit(End),
}

/// 写浏览器的任务为什么停下（发完 exit 正常结束的不报）
enum WriteEnd {
    /// 一帧 STALL 内发不出去：浏览器不读、TCP 窗口满
    Stalled,
    /// 连接断了
    Gone,
}

type Sink = SplitSink<WebSocket, Message>;

/// 发一帧，最多等 STALL
async fn send(sink: &mut Sink, m: Message) -> Result<(), WriteEnd> {
    match tokio::time::timeout(STALL, sink.send(m)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(WriteEnd::Gone),
        Err(_) => Err(WriteEnd::Stalled),
    }
}

/// 告诉浏览器为什么断（`{"type":"exit","reason":…}`），再关
async fn bye(sink: &mut Sink, end: End) {
    let msg = json!({"type": "exit", "reason": end.reason}).to_string();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        sink.send(Message::Text(msg.into())).await?;
        sink.send(Message::Close(Some(CloseFrame { code: end.code, reason: "".into() }))).await
    })
    .await;
}

/// 写浏览器的任务：终端输出与控制帧都从这里发，主循环不会因为浏览器收得慢卡在发送上（踢人、空闲、最长时限照常及时生效）。
/// 控制帧优先。一帧发不出去就经 `done` 告诉主循环后结束；发完 exit 与关闭帧也结束。
/// 控制通道关了不结束：远端自己退出时 exit 排在输出后面，`serve` 放进去之后就丢掉两个发送端，
/// 这时输出可能还没发完——先看到控制通道关了就走，剩下的输出和 exit 都发不出去，浏览器只看到连接断了
async fn write_ws(mut sink: Sink, mut out: mpsc::Receiver<Out>, mut ctl: mpsc::Receiver<Ctl>, done: oneshot::Sender<WriteEnd>) {
    let mut ctl_open = true;
    let failed = loop {
        let m = tokio::select! {
            biased;
            c = ctl.recv(), if ctl_open => match c {
                Some(Ctl::Ping) => Message::Ping(Bytes::new()),
                Some(Ctl::Exit(end)) => return bye(&mut sink, end).await,
                None => {
                    ctl_open = false;
                    continue;
                }
            },
            o = out.recv() => match o {
                Some(Out::Data(b)) => Message::Binary(b),
                Some(Out::Exit(end)) => return bye(&mut sink, end).await,
                None => return,
            },
        };
        if let Err(e) = send(&mut sink, m).await {
            break e;
        }
    };
    let _ = done.send(failed);
}

/// WebSocket 升级之后：等票据、核对、起 ssh、搬运，结束时先告诉浏览器原因（`{"type":"exit","reason":…}`）再关。
/// `events` 在握手前就订阅了：等票据期间的撤销通知也不漏。`waiting` 是握手时占的「等票据」位置，收到第一条消息就还
pub async fn serve(ws: WebSocket, st: AppState, who: Who, events: broadcast::Receiver<Event>, waiting: Waiting) {
    let (sink, mut stream) = ws.split();
    let (out, out_rx) = mpsc::channel(OUT_QUEUE);
    let (ctl, ctl_rx) = mpsc::channel(CTL_QUEUE);
    let (done_tx, done) = oneshot::channel();
    let mut writer = tokio::spawn(write_ws(sink, out_rx, ctl_rx, done_tx));
    let mut io = Io { stream: &mut stream, out: out.clone(), ctl: ctl.clone(), done };
    let (end, drain) = run(&mut io, &st, &who, events, waiting).await;
    drop(io);
    // 远端自己退出的（含 ssh 连不上）：exit 排在剩下的输出后面，ssh 的报错、最后几行先到；别的原因不等积着的输出。
    // 写任务已经停了（浏览器收不走、连接断了）就发不出去，不管
    if drain {
        let _ = out.send_timeout(Out::Exit(end), Duration::from_secs(5)).await;
    } else {
        let _ = ctl.send_timeout(Ctl::Exit(end), Duration::from_secs(5)).await;
    }
    drop((out, ctl));
    if tokio::time::timeout(Duration::from_secs(10), &mut writer).await.is_err() {
        writer.abort();
    }
}

/// 主循环这边的 WebSocket：读的一半，与写任务之间的通道
struct Io<'a> {
    stream: &'a mut SplitStream<WebSocket>,
    out: mpsc::Sender<Out>,
    ctl: mpsc::Sender<Ctl>,
    /// 写任务发不出去时的原因
    done: oneshot::Receiver<WriteEnd>,
}

/// 第一条消息：`{"type":"auth","ticket":"…","cols"?,"rows"?}`。返回票据与初始窗口大小（没给按 80 × 24）
async fn first(stream: &mut SplitStream<WebSocket>) -> Result<(String, u16, u16), End> {
    let m = match tokio::time::timeout(AUTH_WAIT, stream.next()).await {
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

/// 返回结束原因，以及 exit 要不要排在剩下的输出后面（远端自己退出的要）
async fn run(io: &mut Io<'_>, st: &AppState, who: &Who, mut events: broadcast::Receiver<Event>, waiting: Waiting) -> (End, bool) {
    let first = first(io.stream).await;
    drop(waiting);
    let (ticket, cols, rows) = match first {
        Ok(v) => v,
        Err(e) => return (e, false),
    };
    let grant = match st.terminals.redeem(&ticket, &who.session) {
        Ok(g) => g,
        Err(m) => {
            tracing::info!(user = %who.user, "网页终端没打开：{m}");
            return (End::normal(m), false);
        }
    };
    let target = match resolve(st, grant.kind, grant.id).await {
        Ok(t) => t,
        Err(e) => return (End::api(e), false),
    };
    // 撤销通知从握手前就在收（之后的循环里处理）；握手到这里之间被撤销的这里再查一次
    match session_valid(&st.db, &who.session).await {
        Ok(true) => {}
        Ok(false) => return (End { code: crate::api::stream::CLOSE_REVOKED, reason: SESSION_GONE.into() }, false),
        Err(e) => return (End::internal("核对登录会话失败", e), false),
    }
    let mut slot = match st.terminals.enter(who.user_id, &who.session) {
        Ok(s) => s,
        Err(m) => return (End::normal(m), false),
    };
    let (pty, slave) = match Pty::open(cols, rows) {
        Ok(p) => p,
        Err(e) => return (End::internal("打开伪终端失败", e), false),
    };
    let mut cmd = target.command(&st.cfg);
    // 不把中控进程的环境变量（数据库地址、代理设置…）带给 ssh：只留这三个
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()))
        .env("TERM", "xterm-256color")
        .env("LANG", std::env::var_os("LANG").unwrap_or_else(|| "C.UTF-8".into()));
    // 先记「打开」再起 ssh：中控中途重启也留有记录
    let open_id = match audit_open(st, who, &target).await {
        Ok(id) => id,
        Err(e) => return (End::internal("写审计失败，没有打开终端", e), false),
    };
    let t0 = Instant::now();
    let mut child = match pty::spawn(cmd, slave) {
        Ok(c) => c,
        Err(e) => {
            let reason = format!("启动 ssh 失败：{e}");
            audit_close(st, who, &target, open_id, &Closed { secs: 0, bytes_in: 0, bytes_out: 0, why: "spawn_failed", reason: &reason, exit_code: None }).await;
            return (End { code: 1011, reason }, false);
        }
    };
    tracing::info!(user = %who.user, target = %target.name, ip = %target.ip, "打开网页终端");
    let pty = Arc::new(pty);
    let tail = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (stop, bytes_in, bytes_out) = pump(io, st, who, &pty, &tail, &mut slot.kicked, &mut events).await;
    // 中控在重启：systemd 停服务时连同 ssh 一起结束，读到 EOF 可能比踢人的通知先到
    let stop = if matches!(stop, Stop::RemoteExit) && st.terminals.closing() { Stop::Shutdown } else { stop };
    let grace = if matches!(stop, Stop::Shutdown) { SHUTDOWN_GRACE } else { KILL_GRACE };
    let status = match pty::kill_group(&mut child, grace).await {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("网页终端：等 ssh 退出失败：{e}");
            None
        }
    };
    let reason = stop.describe(status, &tail.lock().unwrap());
    let secs = t0.elapsed().as_secs();
    tracing::info!(user = %who.user, target = %target.name, secs, bytes_in, bytes_out, "网页终端断开：{reason}");
    let exit_code = status.and_then(|s| s.code());
    audit_close(st, who, &target, open_id, &Closed { secs, bytes_in, bytes_out, why: stop.code(), reason: &reason, exit_code }).await;
    // 名额在写完「关闭」审计之后才还：中控退出时等名额归零，就是等审计写完
    drop(slot);
    let code = match stop {
        Stop::Kicked(_) => crate::api::stream::CLOSE_REVOKED,
        // 1012 = Service Restart
        Stop::Shutdown => 1012,
        Stop::TooLarge => 1009,
        Stop::RecheckFailed | Stop::Error(_) => 1011,
        _ => 1000,
    };
    (End { code, reason }, matches!(stop, Stop::RemoteExit))
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
    /// 中控正在重启
    Shutdown,
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
            Stop::Shutdown => "shutdown",
            Stop::Stalled => "stalled",
            Stop::TooLarge => "too_large",
            Stop::RecheckFailed => "recheck_failed",
            Stop::Error(_) => "error",
        }
    }

    /// 给人看的原因（浏览器、审计）
    fn describe(&self, status: Option<ExitStatus>, tail: &[u8]) -> String {
        match self {
            Stop::RemoteExit => match status.and_then(|s| s.code()) {
                Some(0) => "远端 shell 已退出".into(),
                Some(255) => ssh_failed(tail),
                Some(c) => format!("远端 shell 已退出（退出码 {c}）"),
                None => "ssh 被信号结束".into(),
            },
            Stop::ClientClosed => "浏览器关闭了终端".into(),
            Stop::ClientGone => "浏览器连接中断".into(),
            Stop::Idle => "15 分钟没有输入，已自动断开".into(),
            Stop::MaxAge => "已连续开了 2 小时，自动断开（可以重新打开）".into(),
            Stop::Kicked(r) => r.clone(),
            Stop::Shutdown => "中控正在重启，终端已断开，稍后可以重新打开".into(),
            Stop::Stalled => "浏览器 15 秒收不走终端输出，已断开".into(),
            Stop::TooLarge => format!("浏览器发来的消息太大（一条最多 {} KiB），已断开", MAX_MESSAGE / 1024),
            Stop::RecheckFailed => "复核登录会话失败，已断开，请重新打开".into(),
            Stop::Error(e) => e.clone(),
        }
    }
}

/// 留着终端输出的最后这么多字节：ssh 自己失败（255）时它的报错在最后几行，用来归类（`ssh_failed`）
const TAIL: usize = 1024;

/// ssh 自己失败（退出码 255）时的原因：按输出里最后一行非空的报错归类（P-20），沿用远程执行的 `ssh::classify_ssh_stderr`，
/// 网络类再分出超时。只看最后一行：登录成功后远端打印过的 Permission denied 之类不算
fn ssh_failed(tail: &[u8]) -> String {
    let text = String::from_utf8_lossy(tail);
    let last = text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("");
    let lower = last.to_lowercase();
    let what = if last.is_empty() {
        "ssh 断开了（连不上、登录被拒、主机指纹不对，或被结束）"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "连接超时：节点没有响应（可能离线、被防火墙拦了，或网络不通）"
    } else {
        match ssh::classify_ssh_stderr(last) {
            ssh::ErrKind::Auth => "认证失败：节点拒绝了中控的密钥（authorized_keys 被改了？）",
            ssh::ErrKind::HostKey => "主机指纹不对：节点重装过或被替换了，确认无误后再重新登记主机指纹",
            _ => "节点离线，无法连接（连接被拒、无路由或被重置）",
        }
    };
    format!("{what}，ssh 退出码 255，详细原因见终端里 ssh 的提示")
}

/// PTY → 写浏览器的任务为什么结束
enum ReadEnd {
    Eof,
    Stalled,
    /// 写浏览器的任务先停了（它停之前已经把原因交给主循环）
    WriterGone,
    Failed(String),
}

/// PTY → 写浏览器的任务（有界通道）。通道满了先等（停读期间 ssh 的输出积在内核的 PTY 缓冲里，ssh 随之不再读网络、远端被流控），
/// STALL 内还腾不出位置就结束（主循环随即断开这个终端）。`read` 记从终端读出、交出去的字节数（审计的下行）
async fn read_pty(pty: Arc<Pty>, tx: mpsc::Sender<Out>, read: Arc<AtomicU64>, tail: Arc<std::sync::Mutex<Vec<u8>>>) -> ReadEnd {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match pty.read(&mut buf).await {
            Ok(0) => return ReadEnd::Eof,
            Ok(n) => match tx.send_timeout(Out::Data(Bytes::copy_from_slice(&buf[..n])), STALL).await {
                Ok(()) => {
                    read.fetch_add(n as u64, Ordering::Relaxed);
                    let mut t = tail.lock().unwrap();
                    t.extend_from_slice(&buf[..n]);
                    let over = t.len().saturating_sub(TAIL);
                    t.drain(..over);
                }
                Err(mpsc::error::SendTimeoutError::Timeout(_)) => return ReadEnd::Stalled,
                Err(mpsc::error::SendTimeoutError::Closed(_)) => return ReadEnd::WriterGone,
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

/// 搬运，直到有一方结束。返回结束原因与上行（浏览器 → 机器）、下行（机器 → 浏览器）字节数。
/// 循环里的每个分支都不长时间等待：往浏览器写在写任务里，复核查库在单独的任务里，PTY 的读写也各在自己的任务里
async fn pump(
    io: &mut Io<'_>,
    st: &AppState,
    who: &Who,
    pty: &Arc<Pty>,
    tail: &Arc<std::sync::Mutex<Vec<u8>>>,
    kicked: &mut watch::Receiver<Option<Kick>>,
    events: &mut broadcast::Receiver<Event>,
) -> (Stop, u64, u64) {
    let (in_tx, in_rx) = mpsc::channel::<Bytes>(IN_QUEUE);
    let read = Arc::new(AtomicU64::new(0));
    let mut reader = tokio::spawn(read_pty(pty.clone(), io.out.clone(), read.clone(), tail.clone()));
    let writer = tokio::spawn(write_pty(pty.clone(), in_rx));
    let _abort = AbortOnDrop(vec![reader.abort_handle(), writer.abort_handle()]);
    let mut rc = Rechecker::new();
    let now = tokio::time::Instant::now();
    let max_age = tokio::time::sleep(MAX_AGE);
    let idle = tokio::time::sleep(IDLE);
    tokio::pin!(max_age, idle);
    let mut ping = tokio::time::interval_at(now + PING_EVERY, PING_EVERY);
    let mut check = tokio::time::interval_at(now + RECHECK_EVERY, RECHECK_EVERY);
    let mut pong_due = false;
    // 输入通道满了时手里那一条：送进去之前不再读浏览器
    let mut pending: Option<Bytes> = None;
    let mut bytes_in = 0u64;
    // 不加 biased：输出一直有（比如 `yes`）时，输入（Ctrl-C）照样轮得到
    let stop = loop {
        tokio::select! {
            r = kicked.changed() => {
                let k = if r.is_ok() { kicked.borrow().clone() } else { None };
                break match k {
                    Some(Kick::Shutdown) => Stop::Shutdown,
                    Some(Kick::Session(why)) => Stop::Kicked(why),
                    None => Stop::Kicked(SESSION_GONE.into()),
                };
            }
            _ = &mut max_age => break Stop::MaxAge,
            _ = &mut idle => break Stop::Idle,
            e = events.recv() => match e {
                // 有会话被撤销，或广播落后、可能丢了撤销通知：马上复核自己的会话
                Ok(Event::Revoked) | Err(RecvError::Lagged(_)) => rc.start(st, &who.session),
                Ok(_) => {}
                Err(RecvError::Closed) => break Stop::Error("中控正在关闭".into()),
            },
            Some(r) = rc.rx.recv() => {
                if let Err(e) = rc.finished(r, st, &who.session) {
                    tracing::warn!("网页终端复核会话失败：{e}");
                    break Stop::RecheckFailed;
                }
            }
            // 读 PTY 的任务结束了：ssh 退出（剩下的输出已经都交给写任务）、浏览器 STALL 内收不走、读出错
            r = &mut reader => break match r {
                Ok(ReadEnd::Eof) => Stop::RemoteExit,
                Ok(ReadEnd::Stalled) => Stop::Stalled,
                // 写任务停下之前已经把原因放进 done
                Ok(ReadEnd::WriterGone) => match io.done.try_recv() {
                    Ok(WriteEnd::Stalled) => Stop::Stalled,
                    _ => Stop::ClientGone,
                },
                Ok(ReadEnd::Failed(e)) => Stop::Error(e),
                Err(e) => Stop::Error(format!("读终端输出的任务异常结束：{e}")),
            },
            // 写浏览器的任务发不出去了
            w = &mut io.done => break match w {
                Ok(WriteEnd::Stalled) => Stop::Stalled,
                Ok(WriteEnd::Gone) | Err(_) => Stop::ClientGone,
            },
            p = in_tx.reserve(), if pending.is_some() => match p {
                Ok(permit) => {
                    if let Some(b) = pending.take() {
                        permit.send(b);
                    }
                }
                // 写 PTY 的任务已结束（ssh 退出了）：丢掉，等读那边的 EOF
                Err(_) => pending = None,
            },
            m = io.stream.next(), if pending.is_none() => match m {
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
                // 输入积着（pending）时不读浏览器，也就收不到 pong：这段时间不查心跳（还在往里粘贴的连接是活的；
                // 真断了，空闲超时收尾）
                if pending.is_none() {
                    if pong_due {
                        break Stop::ClientGone;
                    }
                    if io.ctl.try_send(Ctl::Ping).is_ok() {
                        pong_due = true;
                    }
                }
            }
            _ = check.tick() => rc.start(st, &who.session),
        }
    };
    (stop, bytes_in, read.load(Ordering::Relaxed))
}

// ── 审计 ───────────────────────────────────────────────────────────────────

fn session_id(session: &[u8]) -> String { hex::encode(session.get(..8).unwrap_or(session)) }

/// 「打开」：谁（actor）、哪台、实际登录的地址（同机 IP 节点是宿主机的）、从哪个 IP、哪个登录会话（同「账户安全」里会话的 id）
async fn audit_open(st: &AppState, who: &Who, t: &Target) -> sqlx::Result<i64> {
    let mut d = t.audit_detail();
    d["client_ip"] = json!(who.ip.map(|i| i.to_string()));
    d["session"] = json!(session_id(&who.session));
    crate::db::audit_insert(&st.db, &who.user, "terminal.open", t.kind.as_str(), Some(t.id), d).await
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
    if let Err(e) = crate::db::audit_insert(&st.db, &who.user, "terminal.close", t.kind.as_str(), Some(t.id), d).await {
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

/// 审计「打开 / 关闭网页终端」的一句话摘要（api::audit::audit_text 用）
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

    /// P-20：ssh 自己失败（255）时按它最后的报错分 节点离线 / 超时 / 认证失败 / 主机指纹，认不出的照旧笼统说
    #[test]
    fn ssh_failure_is_classified_from_its_last_words() {
        let cases = [
            ("ssh: connect to host 1.2.3.4 port 22: Connection refused\r\n", "节点离线，无法连接"),
            ("ssh: connect to host 1.2.3.4 port 22: No route to host\r\n", "节点离线，无法连接"),
            ("kex_exchange_identification: read: Connection reset by peer\r\n", "节点离线，无法连接"),
            ("ssh: connect to host 1.2.3.4 port 22: Connection timed out\r\n", "连接超时"),
            ("Timeout, server 1.2.3.4 not responding.\r\n", "连接超时"),
            ("root@1.2.3.4: Permission denied (publickey).\r\n", "认证失败"),
            ("Host key verification failed.\r\n", "主机指纹不对"),
            ("", "ssh 断开了"),
        ];
        for (tail, want) in cases {
            let r = ssh_failed(tail.as_bytes());
            assert!(r.starts_with(want) && r.contains("255"), "{tail:?} → {r}");
        }
        // 先登录成功、之后掉线的：按最后的报错算，前面的输出不影响
        let r = ssh_failed(b"Last login: ...\r\n$ Permission denied\r\nclient_loop: send disconnect: Broken pipe\r\n");
        assert!(r.starts_with("节点离线，无法连接"), "{r}");
    }

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
    fn waiting_for_ticket_is_capped_per_session() {
        let h = Hub::default();
        let mut w: Vec<Waiting> = (0..WAITING_PER_SESSION).map(|_| h.wait_ticket(&s(1)).unwrap()).collect();
        assert!(h.wait_ticket(&s(1)).is_err());
        assert!(h.wait_ticket(&s(2)).is_ok(), "按会话计");
        w.pop();
        assert!(h.wait_ticket(&s(1)).is_ok(), "放掉一个就空出一个");
        drop(w);
        assert!(lock(&h.inner).waiting.get(&s(1)).is_none_or(|n| *n == 0));
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
        let gone = Some(Kick::Session("会话没了".into()));
        assert_eq!(*a1.kicked.borrow(), gone);
        assert_eq!(*a2.kicked.borrow(), gone);
        assert_eq!(*b.kicked.borrow(), None);
        // 那个会话没用掉的票据、再验证记录一并作废，别的会话的不动
        assert_eq!(lock(&h.inner).tickets.len(), 1);
        assert!(!h.verified_recently(&s(1)));
        assert!(h.redeem(&other, &s(2)).is_ok());
    }

    #[test]
    fn close_all_kicks_everyone_and_refuses_new() {
        let h = Hub::default();
        let u = Uuid::now_v7();
        let a = h.enter(u, &s(1)).unwrap();
        let b = h.enter(Uuid::now_v7(), &s(2)).unwrap();
        h.issue(&s(1), u, Kind::Machine, u).unwrap();
        assert_eq!(h.close_all(), 2);
        assert_eq!((a.kicked.borrow().clone(), b.kicked.borrow().clone()), (Some(Kick::Shutdown), Some(Kick::Shutdown)));
        assert!(h.closing() && lock(&h.inner).tickets.is_empty());
        assert!(h.enter(u, &s(1)).err().unwrap().contains("重启"));
        assert!(h.issue(&s(1), u, Kind::Machine, u).unwrap_err().contains("重启"));
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
    fn site_origin_rejects_opaque() {
        assert_eq!(site_origin("https://fleet.kismet613.com/").as_deref(), Some("https://fleet.kismet613.com"));
        assert_eq!(site_origin("http://192.168.31.60:18082").as_deref(), Some("http://192.168.31.60:18082"));
        // 不透明的 origin 序列化出来是 "null"，会和 `Origin: null` 对上
        for u in ["file:///srv/fleet", "data:text/plain,x", "fleet.kismet613.com", ""] {
            assert_eq!(site_origin(u), None, "{u}");
        }
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
