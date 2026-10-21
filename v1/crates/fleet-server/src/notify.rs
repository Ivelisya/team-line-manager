//! 告警通知：渠道配置校验、消息格式化、发送（Telegram / 企业微信群机器人 / 通用 webhook）。
//!
//! 渠道配置（`notify_channel.config_enc`，SecretBox 加密的 JSON）：
//! - telegram：`{bot_token, chat_id, api_base?}`（`api_base` 默认 `https://api.telegram.org`，可指向反代）
//! - wecom：`{url}`（群机器人 webhook 地址）
//! - webhook：`{url}`，POST JSON
//!
//! 触发时机在 `alerts.rs`；这里只管「给定一条通知，发给所有符合条件的渠道，并记 `notify_log`」。

use crate::state::AppState;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::sync::mpsc::error::TrySendError;
use uuid::Uuid;

/// 单次发送超时
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const TELEGRAM_BASE: &str = "https://api.telegram.org";

/// 每个渠道的发送队列最多积压这么多条（渠道一直超时、被限流时），满了新的直接丢弃
const QUEUE_CAP: usize = 500;
/// 排队超过这么久还没轮到的通知不再发：告警多半已经恢复或又提醒过了，晚到的旧消息只会误导
const MAX_QUEUED: Duration = Duration::from_secs(600);
/// 因队列满丢弃的通知累计条数（进程启动起），写进 warn 日志
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// 一个渠道的发送队列：(入队时间, 通知)
pub type Queue = tokio::sync::mpsc::Sender<(Instant, Notice)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 新触发
    Firing,
    /// 持续未恢复的提醒
    Repeat,
    Resolved,
    /// 渠道测试
    Test,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Firing => "firing",
            Kind::Repeat => "repeat",
            Kind::Resolved => "resolved",
            Kind::Test => "test",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub kind: Kind,
    pub incident_id: Option<Uuid>,
    pub rule: String,
    /// info / warning / critical
    pub severity: String,
    pub machine: String,
    /// 不带机器名前缀的原因描述
    pub summary: String,
    pub at: OffsetDateTime,
    /// 告警开始时间（持续提醒和恢复通知里算持续时长）
    pub opened_at: Option<OffsetDateTime>,
}

// ── 格式化 ──────────────────────────────────────────────────────────────────

/// 告警规则的中文名（前端展示也用同一套）
pub fn rule_name(key: &str) -> &str {
    match key {
        "agent_offline" => "agent 离线",
        "clock_skew" => "agent 时钟不准",
        "no_entry" => "没有在用入口",
        "egress_fail" => "出口不通",
        "tcp_fail" => "线路到节点不通",
        "renewal_due" => "即将到期",
        "renewal_overdue" => "已过期",
        "relay_apply_lag" => "配置未应用",
        "relay_apply_error" => "配置应用失败",
        "relay_xray_down" => "Xray 未运行",
        "host_cpu_high" => "CPU 持续偏高",
        "host_mem_high" => "内存持续偏高",
        "host_disk_high" => "磁盘空间不足",
        "ipintel_new_isp" => "来源出现新运营商",
        "ipintel_new_city" => "来源出现新城市",
        "ipintel_datacenter_source" => "机房 / VPN 来源",
        "policy_blocked" => "访问策略暂停",
        other => other,
    }
}

pub fn severity_name(s: &str) -> &'static str {
    match s {
        "critical" => "严重",
        "warning" => "警告",
        _ => "提示",
    }
}

/// 北京时间 `2026-10-20 15:04:05`
pub fn beijing(t: OffsetDateTime) -> String {
    let f = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    t.to_offset(time::macros::offset!(+8)).format(&f).unwrap_or_default()
}

/// `5 分钟` / `2 小时 10 分钟` / `1 天 3 小时`
pub fn fmt_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if d > 0 {
        format!("{d} 天 {h} 小时")
    } else if h > 0 {
        format!("{h} 小时 {m} 分钟")
    } else {
        format!("{} 分钟", m.max(if secs > 0 { 1 } else { 0 }))
    }
}

/// 去掉摘要开头的「机器名：」（通知里机器名单独成行）
pub fn strip_machine<'a>(summary: &'a str, machine: &str) -> &'a str {
    summary.strip_prefix(machine).and_then(|r| r.strip_prefix('：')).unwrap_or(summary)
}

fn title(n: &Notice) -> String {
    let last = n.opened_at.map(|o| fmt_duration((n.at - o).whole_seconds()));
    match (n.kind, last) {
        (Kind::Firing, _) => "告警触发".into(),
        (Kind::Repeat, Some(d)) => format!("告警仍未恢复（已持续 {d}）"),
        (Kind::Repeat, None) => "告警仍未恢复".into(),
        (Kind::Resolved, Some(d)) => format!("告警已恢复（持续 {d}）"),
        (Kind::Resolved, None) => "告警已恢复".into(),
        (Kind::Test, _) => "测试通知".into(),
    }
}

/// 纯文本（Telegram、webhook 的 text 字段）
pub fn format_text(n: &Notice) -> String {
    if n.kind == Kind::Test {
        return format!("【测试通知】团队线路管理的告警通知渠道可用\n时间：{}（北京时间）", beijing(n.at));
    }
    format!(
        "【{}】{}\n机器：{}\n规则：{}\n摘要：{}\n时间：{}（北京时间）",
        severity_name(&n.severity),
        title(n),
        n.machine,
        rule_name(&n.rule),
        n.summary,
        beijing(n.at)
    )
}

/// 企业微信 markdown（颜色：warning 橙红、comment 灰、info 绿）
pub fn format_wecom(n: &Notice) -> String {
    if n.kind == Kind::Test {
        return format!("### 测试通知\n> 团队线路管理的告警通知渠道可用\n> 时间：{}（北京时间）", beijing(n.at));
    }
    let color = match (n.kind, n.severity.as_str()) {
        (Kind::Resolved, _) => "info",
        (_, "critical") | (_, "warning") => "warning",
        _ => "comment",
    };
    format!(
        "### <font color=\"{color}\">【{}】{}</font>\n> 机器：**{}**\n> 规则：{}\n> 摘要：{}\n> 时间：{}（北京时间）",
        severity_name(&n.severity),
        title(n),
        n.machine,
        rule_name(&n.rule),
        n.summary,
        beijing(n.at)
    )
}

/// 通用 webhook 的 JSON 体
pub fn webhook_body(n: &Notice) -> Value {
    json!({
        "event": n.kind.as_str(),
        "incident_id": n.incident_id,
        "rule": n.rule,
        "rule_name": rule_name(&n.rule),
        "severity": n.severity,
        "machine": n.machine,
        "summary": n.summary,
        "time": n.at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
        "time_beijing": beijing(n.at),
        "text": format_text(n),
    })
}

// ── 渠道配置 ────────────────────────────────────────────────────────────────

pub const KINDS: [&str; 3] = ["telegram", "wecom", "webhook"];

fn http_url(v: &Value, what: &str) -> Result<String, String> {
    let s = v.as_str().map(str::trim).unwrap_or("");
    if s.is_empty() {
        return Err(format!("{what} 不能为空"));
    }
    match reqwest::Url::parse(s) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => Ok(s.to_string()),
        _ => Err(format!("{what} 必须是 http(s) 地址")),
    }
}

/// 校验并规范化渠道配置（只保留该类型用得到的字段；chat_id 数字统一成字符串）
pub fn normalize_config(kind: &str, c: &Value) -> Result<Value, String> {
    match kind {
        "telegram" => {
            let token = c["bot_token"].as_str().map(str::trim).unwrap_or("");
            if token.is_empty() || token.contains(|ch: char| ch.is_whitespace() || matches!(ch, '/' | '?' | '#')) {
                return Err("bot_token 不对（形如 123456:ABC…）".into());
            }
            let chat = match &c["chat_id"] {
                Value::String(s) => s.trim().to_string(),
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            };
            if chat.is_empty() {
                return Err("chat_id 不能为空".into());
            }
            let mut out = json!({"bot_token": token, "chat_id": chat});
            if c["api_base"].as_str().is_some_and(|s| !s.trim().is_empty()) {
                out["api_base"] = json!(http_url(&c["api_base"], "api_base")?.trim_end_matches('/'));
            }
            Ok(out)
        }
        "wecom" | "webhook" => Ok(json!({"url": http_url(&c["url"], "url")?})),
        _ => Err("不支持的渠道类型（telegram / wecom / webhook）".into()),
    }
}

/// 列表里显示的提示：telegram 显示 `chat 123456`，其它显示 URL 的主机名
pub fn config_hint(kind: &str, c: &Value) -> String {
    match kind {
        "telegram" => format!("chat {}", c["chat_id"].as_str().unwrap_or("?")),
        _ => c["url"].as_str().and_then(|u| reqwest::Url::parse(u).ok()).and_then(|u| u.host_str().map(String::from)).unwrap_or_default(),
    }
}

// ── 发送 ────────────────────────────────────────────────────────────────────

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("fleet-server/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("构建 HTTP 客户端")
    })
}

fn short(s: &str) -> String { s.chars().take(300).collect() }

/// 发送失败：`retry` 表示值得再试（网络错误 / 5xx / 429）；渠道拒绝、配置有误之类重试也没用。
/// `wait`：429 时渠道要求等多久再发
struct SendErr {
    msg: String,
    retry: bool,
    wait: Option<Duration>,
}

impl SendErr {
    fn fatal(msg: impl Into<String>) -> Self {
        Self { msg: msg.into(), retry: false, wait: None }
    }
}

/// 发送失败后的重试间隔：最多再试两次（0.5 秒、2 秒），不拖住后面的通知太久
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(500), Duration::from_secs(2)];
/// 429 要求的等待时长最多等这么久
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// 429 的等待时长：Telegram 在 JSON 的 `parameters.retry_after`，其它看 `Retry-After` 头（秒数）
fn retry_after(header: Option<&str>, body: &Value) -> Option<Duration> {
    let secs = body["parameters"]["retry_after"].as_u64().or_else(|| header.and_then(|h| h.trim().parse().ok()))?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

/// 发一条通知到一个渠道（只发一次，渠道测试用）。错误文本里不带令牌（reqwest 错误会带 URL，这里去掉）。
pub async fn send(kind: &str, config: &Value, n: &Notice) -> Result<(), String> {
    send_once(kind, config, n).await.map_err(|e| e.msg)
}

/// 发送，网络错误 / 5xx 时退避重试；429 按渠道给的等待时长再试
async fn send_retry(kind: &str, config: &Value, n: &Notice) -> Result<(), String> {
    let mut res = send_once(kind, config, n).await;
    for wait in RETRY_BACKOFF {
        match &res {
            Err(e) if e.retry => {
                tokio::time::sleep(e.wait.unwrap_or(wait)).await;
                res = send_once(kind, config, n).await;
            }
            _ => break,
        }
    }
    res.map_err(|e| e.msg)
}

async fn send_once(kind: &str, config: &Value, n: &Notice) -> Result<(), SendErr> {
    let cfg = normalize_config(kind, config).map_err(SendErr::fatal)?;
    let c = client();
    let (req, expect_json_code) = match kind {
        "telegram" => {
            let base = cfg["api_base"].as_str().unwrap_or(TELEGRAM_BASE);
            let url = format!("{base}/bot{}/sendMessage", cfg["bot_token"].as_str().unwrap_or_default());
            (c.post(url).json(&json!({"chat_id": cfg["chat_id"], "text": format_text(n), "disable_web_page_preview": true})), "telegram")
        }
        "wecom" => {
            let url = cfg["url"].as_str().unwrap_or_default();
            (c.post(url).json(&json!({"msgtype": "markdown", "markdown": {"content": format_wecom(n)}})), "wecom")
        }
        _ => (c.post(cfg["url"].as_str().unwrap_or_default()).json(&webhook_body(n)), "webhook"),
    };
    let res = req.send().await.map_err(|e| SendErr { msg: short(&format!("发送失败：{}", e.without_url())), retry: true, wait: None })?;
    let status = res.status();
    let limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS;
    let retry = status.is_server_error() || limited;
    let header = res.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).map(String::from);
    let body = res.text().await.unwrap_or_default();
    let j: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let wait = if limited { retry_after(header.as_deref(), &j) } else { None };
    let fail = |msg: String| Err(SendErr { msg: short(&msg), retry, wait });
    match expect_json_code {
        // Telegram：HTTP 非 2xx 或 ok=false，说明在 description
        "telegram" if !status.is_success() || j["ok"] == json!(false) => {
            fail(format!("Telegram 拒绝（HTTP {}）：{}", status.as_u16(), j["description"].as_str().unwrap_or(&body)))
        }
        // 企业微信：HTTP 200 但 errcode != 0 也是失败
        "wecom" if !status.is_success() || j["errcode"].as_i64().is_some_and(|c| c != 0) => {
            fail(format!("企业微信拒绝（HTTP {}）：{}", status.as_u16(), j["errmsg"].as_str().unwrap_or(&body)))
        }
        "webhook" if !status.is_success() => fail(format!("webhook 返回 HTTP {}", status.as_u16())),
        _ => Ok(()),
    }
}

#[derive(sqlx::FromRow)]
struct Channel {
    id: Uuid,
    kind: String,
    config_enc: Vec<u8>,
}

/// 发送并记 `notify_log`（一次投递记一条，重试不重复记）。渠道测试不重试，让用户马上看到结果。
async fn deliver(st: &AppState, ch: &Channel, n: &Notice) -> Result<(), String> {
    let res = match st.secrets.open_str(&ch.config_enc).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
        Some(cfg) if n.kind == Kind::Test => send(&ch.kind, &cfg, n).await,
        Some(cfg) => send_retry(&ch.kind, &cfg, n).await,
        None => Err("渠道配置无法解密（密钥变了？）".into()),
    };
    let logged = sqlx::query("insert into notify_log (channel_id, incident_id, ok, error, kind) values ($1, $2, $3, $4, $5)")
        .bind(ch.id)
        .bind(n.incident_id)
        .bind(res.is_ok())
        .bind(res.as_ref().err())
        .bind(n.kind.as_str())
        .execute(&st.db)
        .await;
    if let Err(e) = logged {
        tracing::warn!("写通知记录失败：{e}");
    }
    if let Err(e) = &res {
        tracing::warn!(channel = %ch.id, "通知发送失败：{e}");
    }
    res
}

/// 一个渠道的发送任务：逐条按顺序发，排队超过 MAX_QUEUED 的丢弃。排队期间渠道可能被删除或停用，发送时以数据库里的当前配置为准；
/// 查不到（已删除或停用）就把自己从队列表里移除并退出，队列里剩下的一起丢掉（再启用后下一条通知会重建任务）
fn channel_worker(st: &AppState, id: Uuid) -> Queue {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(Instant, Notice)>(QUEUE_CAP);
    let st = st.clone();
    tokio::spawn(async move {
        while let Some((queued, n)) = rx.recv().await {
            if queued.elapsed() > MAX_QUEUED {
                tracing::warn!(channel = %id, rule = %n.rule, "通知排队超过 {} 分钟还没发出，丢弃", MAX_QUEUED.as_secs() / 60);
                continue;
            }
            match sqlx::query_as::<_, Channel>("select id, kind, config_enc from notify_channel where id = $1 and enabled").bind(id).fetch_optional(&st.db).await {
                Ok(Some(ch)) => {
                    let _ = deliver(&st, &ch, &n).await;
                }
                Ok(None) => {
                    st.notify_queues.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                    break;
                }
                Err(e) => tracing::warn!(channel = %id, "读取通知渠道失败：{e}"),
            }
        }
    });
    tx
}

/// 不阻塞告警循环：放进所有已启用、且 `min_severity` 不高于它的渠道各自的发送队列（队列满了丢弃这条并记 warn）。
/// 同一渠道内逐条按顺序发（一批告警同时触发时不会并发打到同一个渠道，Telegram 等不容易被限流）；
/// 渠道之间互不等待，一个渠道一直超时不拖慢别的。
/// 触发/持续提醒在入队时就刷新 `incident.notified_at`（持续提醒的计时起点）：队列积压时下一轮评估不会重复入队。
pub async fn enqueue(st: &AppState, n: Notice) {
    if matches!(n.kind, Kind::Firing | Kind::Repeat) {
        if let Some(id) = n.incident_id {
            if let Err(e) = sqlx::query("update incident set notified_at = now() where id = $1").bind(id).execute(&st.db).await {
                tracing::warn!("记录通知时间失败：{e}");
            }
        }
    }
    let ids: Vec<Uuid> = match sqlx::query_scalar("select id from notify_channel where enabled and min_severity <= $1::severity")
        .bind(&n.severity)
        .fetch_all(&st.db)
        .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("读取通知渠道失败：{e}");
            return;
        }
    };
    let now = Instant::now();
    let mut qs = st.notify_queues.lock().unwrap_or_else(|e| e.into_inner());
    for id in ids {
        if let Err(TrySendError::Full(_)) = qs.entry(id).or_insert_with(|| channel_worker(st, id)).try_send((now, n.clone())) {
            let total = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::warn!(channel = %id, rule = %n.rule, "通知发送队列已满（{QUEUE_CAP} 条），丢弃这条（累计丢弃 {total} 条）");
        }
    }
}

/// 渠道测试：发一条测试消息，记入 `notify_log`（kind = test），返回发送结果
pub async fn send_test(st: &AppState, channel_id: Uuid) -> Result<(), String> {
    let ch: Option<Channel> = sqlx::query_as("select id, kind, config_enc from notify_channel where id = $1")
        .bind(channel_id)
        .fetch_optional(&st.db)
        .await
        .map_err(|e| format!("读取渠道失败：{e}"))?;
    let ch = ch.ok_or("没有这个渠道")?;
    let n = Notice {
        kind: Kind::Test,
        incident_id: None,
        rule: String::new(),
        severity: "info".into(),
        machine: String::new(),
        summary: String::new(),
        at: OffsetDateTime::now_utc(),
        opened_at: None,
    };
    deliver(st, &ch, &n).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(kind: Kind) -> Notice {
        Notice {
            kind,
            incident_id: None,
            rule: "egress_fail".into(),
            severity: "critical".into(),
            machine: "团队1-02".into(),
            summary: "线路机1:20001 出口不通：timeout".into(),
            // UTC 07:04:05 = 北京时间 15:04:05
            at: time::macros::datetime!(2026-10-20 07:04:05 UTC),
            opened_at: Some(time::macros::datetime!(2026-10-20 04:54:05 UTC)),
        }
    }

    #[test]
    fn text_has_machine_rule_summary_and_beijing_time() {
        let t = format_text(&notice(Kind::Firing));
        assert_eq!(
            t,
            "【严重】告警触发\n机器：团队1-02\n规则：出口不通\n摘要：线路机1:20001 出口不通：timeout\n时间：2026-10-20 15:04:05（北京时间）"
        );
        let r = format_text(&notice(Kind::Resolved));
        assert!(r.starts_with("【严重】告警已恢复（持续 2 小时 10 分钟）\n"), "{r}");
        let rep = format_text(&notice(Kind::Repeat));
        assert!(rep.contains("告警仍未恢复（已持续 2 小时 10 分钟）"), "{rep}");
    }

    #[test]
    fn wecom_markdown_and_webhook_body() {
        let w = format_wecom(&notice(Kind::Firing));
        assert!(w.starts_with("### <font color=\"warning\">【严重】告警触发</font>"), "{w}");
        assert!(w.contains("> 机器：**团队1-02**") && w.contains("> 规则：出口不通"));
        assert!(format_wecom(&notice(Kind::Resolved)).contains("color=\"info\""));
        let b = webhook_body(&notice(Kind::Firing));
        assert_eq!(b["event"], "firing");
        assert_eq!(b["rule"], "egress_fail");
        assert_eq!(b["rule_name"], "出口不通");
        assert_eq!(b["machine"], "团队1-02");
        assert_eq!(b["time_beijing"], "2026-10-20 15:04:05");
        assert_eq!(b["time"], "2026-10-20T07:04:05Z");
        assert!(b["text"].as_str().unwrap().contains("团队1-02"));
    }

    #[test]
    fn helpers() {
        assert_eq!(fmt_duration(30), "1 分钟");
        assert_eq!(fmt_duration(0), "0 分钟");
        assert_eq!(fmt_duration(7800), "2 小时 10 分钟");
        assert_eq!(fmt_duration(90_000), "1 天 1 小时");
        assert_eq!(strip_machine("团队1-02：5 分钟没有上报", "团队1-02"), "5 分钟没有上报");
        assert_eq!(strip_machine("别的：x", "团队1-02"), "别的：x");
        assert_eq!(rule_name("relay_xray_down"), "Xray 未运行");
        assert_eq!(rule_name("something_new"), "something_new");
    }

    #[test]
    fn retry_after_from_telegram_body_or_header() {
        let tg = json!({"ok": false, "error_code": 429, "parameters": {"retry_after": 7}});
        assert_eq!(retry_after(None, &tg), Some(Duration::from_secs(7)));
        assert_eq!(retry_after(Some("3"), &Value::Null), Some(Duration::from_secs(3)));
        assert_eq!(retry_after(Some("3600"), &Value::Null), Some(MAX_RETRY_AFTER));
        assert_eq!(retry_after(None, &Value::Null), None);
    }

    #[test]
    fn config_validation() {
        let tg = normalize_config("telegram", &json!({"bot_token": " 123:ABC ", "chat_id": 987654, "junk": 1})).unwrap();
        assert_eq!(tg, json!({"bot_token": "123:ABC", "chat_id": "987654"}));
        assert_eq!(config_hint("telegram", &tg), "chat 987654");
        assert!(normalize_config("telegram", &json!({"bot_token": "a/b", "chat_id": "1"})).is_err());
        assert!(normalize_config("telegram", &json!({"bot_token": "1:a"})).is_err());
        let base = normalize_config("telegram", &json!({"bot_token": "1:a", "chat_id": "1", "api_base": "https://tg.example.com/"})).unwrap();
        assert_eq!(base["api_base"], "https://tg.example.com");
        let wc = normalize_config("wecom", &json!({"url": "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=k"})).unwrap();
        assert_eq!(config_hint("wecom", &wc), "qyapi.weixin.qq.com");
        assert!(normalize_config("webhook", &json!({"url": "ftp://x"})).is_err());
        assert!(normalize_config("webhook", &json!({})).is_err());
        assert!(normalize_config("email", &json!({})).is_err());
    }
}
