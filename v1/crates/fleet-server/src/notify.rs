//! 告警通知：渠道配置校验、消息格式化、发送（Telegram / 企业微信群机器人 / 通用 webhook）。
//!
//! 渠道配置（`notify_channel.config_enc`，SecretBox 加密的 JSON）：
//! - telegram：`{bot_token, chat_id, api_base?}`（`api_base` 默认 `https://api.telegram.org`，可指向反代）
//! - wecom：`{url}`（群机器人 webhook 地址）
//! - webhook：`{url}`，POST JSON
//!
//! 触发时机在 `alerts.rs`（中控自身的告警在 `selfmon.rs`）；这里管「给定一条通知，排进所有符合条件的渠道、发出去，并记 `notify_log`」。
//! 待发的写在库里（`notify_outbox`）：中控重启、部署后接着发；失败的按退避重试，入队满 30 分钟还没发出才放弃。

use crate::state::AppState;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

/// 单次发送超时
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const TELEGRAM_BASE: &str = "https://api.telegram.org";

/// 入队满这么久还没发出（渠道一直失败）就放弃，记一条失败：渠道在这段时间里恢复了就补发。
/// 渠道连续失败另开告警（central_channel_failing，见 selfmon.rs），经别的渠道发
const MAX_AGE: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// 新触发
    Firing,
    /// 持续未恢复的提醒
    Repeat,
    Resolved,
    /// 渠道测试
    Test,
    /// 有人新建 / 删除了静默或维护窗口（发给所有启用的渠道，不受静默影响，见 api/misc.rs `tell_channels`）
    Silence,
    /// 中控自动做了会回收机器的操作（备用团队的机器到期自动回收，见 spare.rs）：同静默变更，发给所有启用的渠道，不看级别、不受静默影响
    Auto,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Firing => "firing",
            Kind::Repeat => "repeat",
            Kind::Resolved => "resolved",
            Kind::Test => "test",
            Kind::Silence => "silence",
            Kind::Auto => "auto",
        }
    }

    /// 发给所有启用的渠道（不看渠道的最低级别）
    fn to_all(self) -> bool { matches!(self, Kind::Silence | Kind::Auto) }
}

/// 一条通知。入队时整条存进 `notify_outbox.notice`（JSON），发送时按入队时的样子发
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notice {
    pub kind: Kind,
    pub incident_id: Option<Uuid>,
    pub rule: String,
    /// info / warning / critical
    pub severity: String,
    pub machine: String,
    /// 不带机器名前缀的原因描述
    pub summary: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// 告警开始时间（持续提醒和恢复通知里算持续时长）
    #[serde(with = "time::serde::rfc3339::option")]
    pub opened_at: Option<OffsetDateTime>,
}

// ── 格式化 ──────────────────────────────────────────────────────────────────

/// 告警规则的中文名（表在 meta::RULES，网页、fleetctl 经 /meta 用同一套）
pub fn rule_name(key: &str) -> &str { crate::meta::rule_label(key) }

pub fn severity_name(s: &str) -> &'static str { crate::meta::severity_label(s) }

/// 组织时区（默认北京时间）的 `2026-10-20 15:04:05`
pub fn beijing(t: OffsetDateTime) -> String { crate::meta::org_time(t) }

/// 通知里的时间：`2026-10-20 15:04:05（北京时间）`，括注是组织时区的叫法
fn when(t: OffsetDateTime) -> String { format!("{}（{}）", crate::meta::org_time(t), crate::meta::org_tz_label()) }

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

/// 字节数，二进制单位，写法同网页 format.ts 的 mem（网页的流量用十进制，那是 bytes）：`1.84 TiB`、`950 MiB`、`2 TiB`
pub fn fmt_bytes(n: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let (mut x, mut i) = (n.max(0.0), 0);
    while x >= 1024.0 && i + 1 < UNITS.len() {
        x /= 1024.0;
        i += 1;
    }
    let digits = if i == 0 || x >= 100.0 { 0 } else if x >= 10.0 { 1 } else { 2 };
    let s = format!("{x:.digits$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { &s };
    format!("{s} {}", UNITS[i])
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
        (Kind::Silence, _) => "静默 / 维护变更".into(),
        (Kind::Auto, _) => "自动回收".into(),
    }
}

/// 纯文本（Telegram、webhook 的 text 字段）
pub fn format_text(n: &Notice) -> String {
    if n.kind == Kind::Test {
        return format!("【测试通知】团队线路管理的告警通知渠道可用\n时间：{}", when(n.at));
    }
    // 静默 / 维护变更：摘要是写好的几行（谁、新建还是删除、对象、起止、备注）
    if n.kind == Kind::Silence {
        return format!("【{}】{}\n时间：{}", title(n), n.summary, when(n.at));
    }
    // 自动回收：标题后面接机器名，摘要写为什么回收、做到哪一步
    if n.kind == Kind::Auto {
        return format!("【{}】{}\n{}\n时间：{}", title(n), n.machine, n.summary, when(n.at));
    }
    format!(
        "【{}】{}\n机器：{}\n规则：{}\n摘要：{}\n时间：{}",
        crate::meta::severity_label(&n.severity),
        title(n),
        n.machine,
        crate::meta::rule_label(&n.rule),
        n.summary,
        when(n.at)
    )
}

/// 插进企业微信 markdown 的文本（机器名、摘要里有 agent 上报的内容）：markdown 控制字符换成全角、换行换成空格，截到 `max` 个字，
/// 免得拼出链接、标题、字体标签
fn md(s: &str, max: usize) -> String {
    s.chars()
        .take(max)
        .map(|c| match c {
            '[' => '［',
            ']' => '］',
            '(' => '（',
            ')' => '）',
            '<' => '＜',
            '>' => '＞',
            '*' => '＊',
            '_' => '＿',
            '`' => '｀',
            '#' => '＃',
            '\\' => '＼',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

/// 企业微信 markdown（颜色：warning 橙红、comment 灰、info 绿）
pub fn format_wecom(n: &Notice) -> String {
    if n.kind == Kind::Test {
        return format!("### 测试通知\n> 团队线路管理的告警通知渠道可用\n> 时间：{}", when(n.at));
    }
    // 摘要里有备注、机器名：逐行转义
    if n.kind == Kind::Silence {
        let body: Vec<String> = n.summary.lines().map(|l| format!("> {}", md(l, 300))).collect();
        return format!("### <font color=\"warning\">{}</font>\n{}\n> 时间：{}", title(n), body.join("\n"), when(n.at));
    }
    if n.kind == Kind::Auto {
        return format!("### <font color=\"warning\">【{}】{}</font>\n> {}\n> 时间：{}", title(n), md(&n.machine, 64), md(&n.summary, 500), when(n.at));
    }
    let color = match (n.kind, n.severity.as_str()) {
        (Kind::Resolved, _) => "info",
        (_, "critical") | (_, "warning") => "warning",
        _ => "comment",
    };
    format!(
        "### <font color=\"{color}\">【{}】{}</font>\n> 机器：**{}**\n> 规则：{}\n> 摘要：{}\n> 时间：{}",
        crate::meta::severity_label(&n.severity),
        title(n),
        md(&n.machine, 64),
        md(crate::meta::rule_label(&n.rule), 64),
        md(&n.summary, 500),
        when(n.at)
    )
}

/// 通用 webhook 的 JSON 体
pub fn webhook_body(n: &Notice) -> Value {
    json!({
        "event": n.kind.as_str(),
        "incident_id": n.incident_id,
        "rule": n.rule,
        "rule_name": crate::meta::rule_label(&n.rule),
        "severity": n.severity,
        "machine": n.machine,
        "summary": n.summary,
        "time": crate::meta::fmt_ts(n.at),
        "time_beijing": crate::meta::org_time(n.at),
        "text": format_text(n),
    })
}

// ── 渠道配置 ────────────────────────────────────────────────────────────────

pub const KINDS: [&str; 3] = ["telegram", "wecom", "webhook"];

/// 校验 http(s) 地址（供应商控制台地址也用它）
pub fn http_url(s: &str, what: &str) -> Result<String, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err(format!("{what} 不能为空"));
    }
    match reqwest::Url::parse(s) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => Ok(s.to_string()),
        _ => Err(format!("{what} 必须是 http(s) 地址")),
    }
}

fn http_url_v(v: &Value, what: &str) -> Result<String, String> { http_url(v.as_str().unwrap_or(""), what) }

/// 不是公网地址：环回、未指定、私网、CGNAT、链路本地（含云元数据 169.254.169.254）、ULA、组播、广播与保留、文档、基准测试网段；
/// IPv4 映射的 IPv6 按其中的 IPv4 判断；内嵌 IPv4 的转换地址（NAT64、6to4、Teredo：经转换网关能到任意 IPv4，含内网与云元数据）一律不算
pub fn non_global(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(a) => {
            let o = a.octets();
            a.is_loopback()
                || a.is_unspecified()
                || a.is_private()
                || a.is_link_local()
                || a.is_multicast()
                || a.is_broadcast()
                || a.is_documentation()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 100 && o[1] & 0xc0 == 64)
                || (o[0] == 198 && o[1] & 0xfe == 18)
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || s[0] & 0xfe00 == 0xfc00
                || s[0] & 0xffc0 == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                // NAT64：64:ff9b::/96 与本地用的 64:ff9b:1::/48（整个 64:ff9b::/32 都没有普通公网地址）
                || (s[0] == 0x0064 && s[1] == 0xff9b)
                // Teredo 2001::/32、6to4 2002::/16
                || (s[0] == 0x2001 && s[1] == 0)
                || s[0] == 0x2002
        }
    }
}

/// 渠道地址（webhook / 企业微信的 url、Telegram 的 api_base）的主机是字面 IP 且不是公网地址时拒绝。
/// 域名在发送时由 [`PublicOnly`] 解析时过滤；字面 IP 不经过解析，在这里拦
pub fn check_destination(c: &Value, allow_private: bool) -> Result<(), String> {
    if allow_private {
        return Ok(());
    }
    for k in ["url", "api_base"] {
        let Some(u) = c[k].as_str().and_then(|s| reqwest::Url::parse(s).ok()) else { continue };
        let host = u.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']');
        if host.parse::<IpAddr>().is_ok_and(non_global) {
            return Err(format!("{k} 指向内网 / 保留地址（{host}），不能用作通知渠道"));
        }
    }
    Ok(())
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
                out["api_base"] = json!(http_url_v(&c["api_base"], "api_base")?.trim_end_matches('/'));
            }
            Ok(out)
        }
        "wecom" | "webhook" => Ok(json!({"url": http_url_v(&c["url"], "url")?})),
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

/// 只给出公网地址的 DNS 解析：渠道地址由有 write 权限的人填，不能借它打到中控所在的内网、环回、云元数据
struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0)).await?.filter(|a| !non_global(a.ip())).collect();
            if addrs.is_empty() {
                return Err(format!("{} 只解析到内网 / 保留地址，不发", name.as_str()).into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// `allow_private`（配置 `[notify] allow_private`）为假时用只解析公网地址的客户端。
/// 不走环境变量里的代理（HTTP(S)_PROXY、ALL_PROXY）：经代理时目标域名由代理去解析，PublicOnly 就管不住了
fn client(allow_private: bool) -> &'static reqwest::Client {
    static OPEN: OnceLock<reqwest::Client> = OnceLock::new();
    static PUBLIC: OnceLock<reqwest::Client> = OnceLock::new();
    let build = |public: bool| {
        let b = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent(concat!("fleet-server/", env!("CARGO_PKG_VERSION")));
        let b = if public { b.dns_resolver(Arc::new(PublicOnly)) } else { b };
        b.build().expect("构建 HTTP 客户端")
    };
    if allow_private { OPEN.get_or_init(|| build(false)) } else { PUBLIC.get_or_init(|| build(true)) }
}

fn short(s: &str) -> String { s.chars().take(300).collect() }

/// 响应体最多读这么多（只为取出错误说明），多的不读
const MAX_BODY: usize = 64 * 1024;

async fn read_capped(mut res: reqwest::Response) -> Vec<u8> {
    let mut buf = Vec::new();
    while let Ok(Some(chunk)) = res.chunk().await {
        buf.extend_from_slice(&chunk[..chunk.len().min(MAX_BODY - buf.len())]);
        if buf.len() >= MAX_BODY {
            break;
        }
    }
    buf
}

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

/// 可重试的失败之后隔多久再发同一条（429 按渠道给的等待时长）：渠道已连续失败 n 次就等第 n 项，再往后都等最后一项，
/// 直到入队满 MAX_AGE。按渠道而不是按条计：渠道一直不通时新排进来的不会从 0.5 秒重新密集地试
const RETRY_BACKOFF: [Duration; 6] = [
    Duration::from_millis(500),
    Duration::from_secs(2),
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
];
/// 429 要求的等待时长最多等这么久
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// 429 的等待时长：Telegram 在 JSON 的 `parameters.retry_after`，其它看 `Retry-After` 头（秒数）
fn retry_after(header: Option<&str>, body: &Value) -> Option<Duration> {
    let secs = body["parameters"]["retry_after"].as_u64().or_else(|| header.and_then(|h| h.trim().parse().ok()))?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

/// 发一条通知到一个渠道（只发一次）。错误文本里不带令牌（reqwest 错误会带 URL，这里去掉）。
/// 渠道的回复只取 HTTP 状态和它 JSON 里的错误说明（截短），不记原始响应体：发送记录 read 密钥也能看
async fn send_once(kind: &str, config: &Value, n: &Notice, allow_private: bool) -> Result<(), SendErr> {
    let cfg = normalize_config(kind, config).map_err(SendErr::fatal)?;
    check_destination(&cfg, allow_private).map_err(SendErr::fatal)?;
    let c = client(allow_private);
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
    let res = req.send().await.map_err(|e| {
        // 最里层的原因（连接被拒、只解析到内网地址…），reqwest 自己的文字只有「error sending request」
        let cause = std::iter::successors(std::error::Error::source(&e), |s| s.source()).last().map(|s| format!("（{s}）")).unwrap_or_default();
        SendErr { msg: short(&format!("发送失败：{}{cause}", e.without_url())), retry: true, wait: None }
    })?;
    let status = res.status();
    let limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS;
    let retry = status.is_server_error() || limited;
    let header = res.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).map(String::from);
    let j: Value = serde_json::from_slice(&read_capped(res).await).unwrap_or(Value::Null);
    let wait = if limited { retry_after(header.as_deref(), &j) } else { None };
    let fail = |msg: String| Err(SendErr { msg: short(&msg), retry, wait });
    let why = |k: &str| j[k].as_str().map(|s| s.chars().take(200).collect::<String>()).unwrap_or_else(|| "（没有说明）".into());
    match expect_json_code {
        // Telegram：HTTP 非 2xx 或 ok=false，说明在 description
        "telegram" if !status.is_success() || j["ok"] == json!(false) => fail(format!("Telegram 拒绝（HTTP {}）：{}", status.as_u16(), why("description"))),
        // 企业微信：HTTP 200 但 errcode != 0 也是失败
        "wecom" if !status.is_success() || j["errcode"].as_i64().is_some_and(|c| c != 0) => {
            fail(format!("企业微信拒绝（HTTP {}）：{}", status.as_u16(), why("errmsg")))
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

/// 发一次，并记下渠道的连续失败状态（`notify_channel.failing_since / fail_count / last_error`：发成功清零，失败累加；
/// 连续失败满 5 分钟开告警，见 selfmon.rs）。渠道测试也算：测试发成功了，渠道就算恢复了
async fn attempt(st: &AppState, ch: &Channel, n: &Notice) -> Result<(), SendErr> {
    let res = match st.secrets.open_str(&ch.config_enc).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
        Some(cfg) => send_once(&ch.kind, &cfg, n, st.cfg.notify.allow_private).await,
        None => Err(SendErr::fatal("渠道配置无法解密（密钥变了？）")),
    };
    let q = match &res {
        Ok(()) => sqlx::query("update notify_channel set failing_since = null, fail_count = 0, last_error = null where id = $1 and fail_count > 0").bind(ch.id),
        Err(e) => sqlx::query("update notify_channel set failing_since = coalesce(failing_since, now()), fail_count = fail_count + 1, last_error = $2 where id = $1")
            .bind(ch.id)
            .bind(&e.msg),
    };
    if let Err(e) = q.execute(&st.db).await {
        tracing::warn!("记录渠道发送结果失败：{e}");
    }
    if let Err(e) = &res {
        tracing::warn!(channel = %ch.id, "通知发送失败：{}", e.msg);
    }
    res
}

/// 记一条发送记录，同一条语句删掉这条待发（`outbox`）。一次投递记一条：重试不重复记，放弃时记最后一次失败的原因。
/// 渠道、告警在发送期间被删了的照样记（对应字段为 null），不会因外键写不进去
async fn record(st: &AppState, outbox: Option<i64>, ch: Uuid, n: &Notice, err: Option<&str>) -> sqlx::Result<()> {
    sqlx::query(
        "with d as (delete from notify_outbox where id = $6)
         insert into notify_log (channel_id, incident_id, ok, error, kind)
         select (select id from notify_channel where id = $1), (select id from incident where id = $2), $3, $4, $5",
    )
    .bind(ch)
    .bind(n.incident_id)
    .bind(err.is_none())
    .bind(err)
    .bind(n.kind.as_str())
    .bind(outbox)
    .execute(&st.db)
    .await
    .map(|_| ())
}

/// 这个渠道收到过这条告警的触发或提醒、之后还没收到它的恢复（看它最近一条送达的触发 / 提醒 / 恢复）。
/// 同一渠道按入队顺序发：轮到恢复时，排在它前面的触发已经发出或放弃了
async fn got_firing(st: &AppState, ch: Uuid, incident: Option<Uuid>) -> sqlx::Result<bool> {
    let last: Option<String> = sqlx::query_scalar(
        "select kind from notify_log where channel_id = $1 and incident_id = $2 and ok and kind in ('firing', 'repeat', 'resolved')
         order by id desc limit 1",
    )
    .bind(ch)
    .bind(incident)
    .fetch_optional(&st.db)
    .await?;
    Ok(last.is_some_and(|k| k != "resolved"))
}

/// 一个渠道排在最前面的一条待发，连同渠道此刻的配置（排队期间渠道可能被改、停用：发送时以库里的当前配置为准）
#[derive(sqlx::FromRow)]
struct Pending {
    id: i64,
    notice: sqlx::types::Json<Notice>,
    created_at: OffsetDateTime,
    kind: String,
    config_enc: Vec<u8>,
    enabled: bool,
    fail_count: i32,
    last_error: Option<String>,
}

/// 一个渠道的发送任务：按入队顺序逐条发，发完（没有待发的了）就退出。读写库出错时歇 5 秒再退出，交回发送循环重新起（不空转）
async fn drain(st: &AppState, ch: Uuid) {
    if let Err(e) = drain_queue(st, ch).await {
        tracing::warn!(channel = %ch, "发送通知时读写库失败：{e}");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// 可重试的失败（网络错误、5xx、429）退避后重发同一条，后面的接着排（保证顺序）；渠道拒绝、配置有误之类重试也没用的记失败、发下一条；
/// 入队满 MAX_AGE 还没发出的放弃。恢复通知只发给收到过这条告警触发的渠道。渠道停用了就丢掉它排着的（再启用后只发新的），删了的随外键一起没了
async fn drain_queue(st: &AppState, ch: Uuid) -> sqlx::Result<()> {
    loop {
        let p: Option<Pending> = sqlx::query_as(
            "select o.id, o.notice, o.created_at, c.kind, c.config_enc, c.enabled, c.fail_count, c.last_error
             from notify_outbox o join notify_channel c on c.id = o.channel_id
             where o.channel_id = $1 order by o.id limit 1",
        )
        .bind(ch)
        .fetch_optional(&st.db)
        .await?;
        let Some(p) = p else { return Ok(()) };
        if !p.enabled {
            sqlx::query("delete from notify_outbox where channel_id = $1").bind(ch).execute(&st.db).await?;
            return Ok(());
        }
        let n = &p.notice.0;
        // 这个渠道没收到过这条告警的触发（触发没发出去，或渠道是后来才加的、后来才收这个级别的）：不发恢复，删掉就是
        if n.kind == Kind::Resolved && !got_firing(st, ch, n.incident_id).await? {
            sqlx::query("delete from notify_outbox where id = $1").bind(p.id).execute(&st.db).await?;
            continue;
        }
        let err = if OffsetDateTime::now_utc() - p.created_at > MAX_AGE {
            Some(short(&format!("入队 {} 分钟还没发出，放弃（最近一次失败：{}）", MAX_AGE.as_secs() / 60, p.last_error.as_deref().unwrap_or("-"))))
        } else {
            let c = Channel { id: ch, kind: p.kind, config_enc: p.config_enc };
            match attempt(st, &c, n).await {
                Ok(()) => None,
                Err(e) if e.retry => {
                    let backoff = RETRY_BACKOFF[(p.fail_count.max(0) as usize).min(RETRY_BACKOFF.len() - 1)];
                    tokio::time::sleep(e.wait.unwrap_or(backoff)).await;
                    continue;
                }
                Err(e) => Some(e.msg),
            }
        };
        // 发出去了（或放弃了）就要记下、删掉这条。库写不进去（磁盘满之类）时隔 10 秒再写，不退出：退出会被重新领走、把已经发出的再发一遍
        while let Err(e) = record(st, Some(p.id), ch, n, err.as_deref()).await {
            tracing::warn!(channel = %ch, "写通知记录失败，10 秒后再写：{e}");
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }
}

/// 发送循环（每个进程一个，`start` 起）：notify_outbox 里有待发的渠道各起一个发送任务（`drain`）——渠道之间互不等待，
/// 一个渠道一直超时不拖慢别的；同一渠道内逐条按顺序发，一批告警同时触发时不会并发打到同一个渠道（Telegram 等不容易被限流）。
/// 入队时唤醒（`AppState::notify_wake`），兜底每 5 秒查一次：中控重启、部署后接着发上次没发完的
pub async fn run_forever(st: AppState) {
    let mut tasks = tokio::task::JoinSet::new();
    let mut busy: HashSet<Uuid> = HashSet::new();
    loop {
        match sqlx::query_scalar::<_, Uuid>("select distinct channel_id from notify_outbox").fetch_all(&st.db).await {
            Ok(chs) => {
                for ch in chs {
                    if !busy.insert(ch) {
                        continue;
                    }
                    let st = st.clone();
                    tasks.spawn(async move {
                        // 发送任务 panic 不能让这个渠道从此停发：记下来，歇 10 秒（不空转）再交回发送循环重新起
                        if AssertUnwindSafe(drain(&st, ch)).catch_unwind().await.is_err() {
                            tracing::error!(channel = %ch, "通知发送任务 panic，10 秒后重来");
                            tokio::time::sleep(Duration::from_secs(10)).await;
                        }
                        ch
                    });
                }
            }
            Err(e) => tracing::warn!("读取待发通知失败：{e}"),
        }
        tokio::select! {
            _ = st.notify_wake.notified() => {}
            Some(Ok(ch)) = tasks.join_next() => {
                busy.remove(&ch);
            }
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
}

/// 起发送循环（每个进程一次）：spawn_background 里起；第一次入队时也起（测试不起后台循环）
pub fn start(st: &AppState) {
    if !st.notify_started.swap(true, Ordering::Relaxed) {
        crate::supervise(st, "通知发送", run_forever);
    }
}

/// 不阻塞告警循环：给每个已启用、`min_severity` 不高于它的渠道各写一行待发（`notify_outbox`，中控重启、部署后照样发），唤醒发送循环。
/// 静默 / 维护变更、自动回收发给所有已启用的渠道，不看级别。告警对象就是某个渠道时（渠道连续失败的告警）不发给它自己。
/// 触发 / 持续提醒在同一事务里刷新 `incident.notified_at`：持续提醒从这时起计时，恢复时据此判断报过触发没有（见 alerts.rs）。
/// 写进去了才算通知过：入队失败（库出错）不刷新，下一轮评估补发「触发」
pub async fn enqueue(st: &AppState, n: Notice) {
    start(st);
    match enqueue_tx(st, &n).await {
        Ok(()) => st.notify_wake.notify_one(),
        Err(e) => tracing::warn!(rule = %n.rule, "通知入队失败：{e}"),
    }
}

async fn enqueue_tx(st: &AppState, n: &Notice) -> sqlx::Result<()> {
    let mut tx = st.db.begin().await?;
    sqlx::query(
        "insert into notify_outbox (channel_id, incident_id, notice)
         select id, $1, $2 from notify_channel
         where enabled and ($4 or min_severity <= $3::severity) and id is distinct from (select subject_id from incident where id = $1)",
    )
    .bind(n.incident_id)
    .bind(sqlx::types::Json(n))
    .bind(&n.severity)
    .bind(n.kind.to_all())
    .execute(&mut *tx)
    .await?;
    if let (Kind::Firing | Kind::Repeat, Some(id)) = (n.kind, n.incident_id) {
        sqlx::query("update incident set notified_at = now() where id = $1").bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await
}

/// 渠道测试：发一条测试消息（只发一次、不排队，让用户马上看到结果），记入 `notify_log`（kind = test），返回发送结果
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
    let res = attempt(st, &ch, &n).await.map_err(|e| e.msg);
    if let Err(e) = record(st, None, ch.id, &n, res.as_ref().err().map(String::as_str)).await {
        tracing::warn!("写通知记录失败：{e}");
    }
    res
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
    fn wecom_escapes_reported_text() {
        let n = Notice { machine: "a](http://x)".into(), summary: "**粗** <font>\n### 标题 `x` _y_ #z \\".into(), ..notice(Kind::Firing) };
        let w = format_wecom(&n);
        assert!(w.contains("> 机器：**a］（http://x）**"), "{w}");
        assert!(w.contains("> 摘要：＊＊粗＊＊ ＜font＞ ＃＃＃ 标题 ｀x｀ ＿y＿ ＃z ＼\n"), "{w}");
        let long = Notice { summary: "x".repeat(2000), ..notice(Kind::Firing) };
        assert!(format_wecom(&long).chars().count() < 700);
    }

    #[test]
    fn silence_change_text() {
        let n = Notice {
            kind: Kind::Silence,
            rule: String::new(),
            severity: "info".into(),
            machine: "全部机器".into(),
            summary: "apikey:ops 新建了维护窗口\n对象：全部机器（全部规则）\n备注：换 **内核** [点我](http://x)".into(),
            opened_at: None,
            ..notice(Kind::Firing)
        };
        let t = format_text(&n);
        assert!(t.starts_with("【静默 / 维护变更】apikey:ops 新建了维护窗口\n对象：全部机器（全部规则）\n"), "{t}");
        assert!(t.ends_with("\n时间：2026-10-20 15:04:05（北京时间）"), "{t}");
        let w = format_wecom(&n);
        assert!(w.contains("\n> 对象：全部机器（全部规则）\n> 备注：换 ＊＊内核＊＊ ［点我］（http://x）\n> 时间："), "{w}");
        assert_eq!((webhook_body(&n)["event"].as_str(), webhook_body(&n)["machine"].as_str()), (Some("silence"), Some("全部机器")));
    }

    #[test]
    fn auto_recycle_text() {
        let n = Notice {
            kind: Kind::Auto,
            rule: crate::spare::AUTO.into(),
            severity: "warning".into(),
            machine: "备用-03".into(),
            summary: "备用节点已过期满 1 天（到期日 2026-10-20）：已自动发起退役".into(),
            opened_at: None,
            ..notice(Kind::Firing)
        };
        let t = format_text(&n);
        assert!(t.starts_with("【自动回收】备用-03\n备用节点已过期满 1 天（到期日 2026-10-20）：已自动发起退役\n时间："), "{t}");
        let w = format_wecom(&n);
        assert!(w.starts_with("### <font color=\"warning\">【自动回收】备用-03</font>\n> 备用节点已过期满 1 天（到期日 2026-10-20）：已自动发起退役\n> 时间："), "{w}");
        assert_eq!(webhook_body(&n)["event"], "auto");
        assert!(Kind::Auto.to_all() && Kind::Silence.to_all() && !Kind::Firing.to_all());
    }

    #[test]
    fn private_destinations() {
        for ip in ["127.0.0.1", "10.0.0.1", "172.16.5.1", "192.168.1.1", "100.64.0.1", "169.254.169.254", "0.0.0.0", "255.255.255.255", "224.0.0.1", "192.0.2.1", "198.18.0.1", "::1", "::", "fe80::1", "fd00::1", "ff02::1", "2001:db8::1", "::ffff:10.0.0.1"] {
            assert!(non_global(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "149.154.167.220", "100.128.0.1", "2001:4860:4860::8888"] {
            assert!(!non_global(ip.parse().unwrap()), "{ip}");
        }
        // 内嵌 IPv4 的转换地址：NAT64（含本地用的 /48）、Teredo、6to4，哪怕里面是公网 IPv4 也不连
        for ip in ["64:ff9b::a9fe:a9fe", "64:ff9b::808:808", "64:ff9b:1::a00:1", "2001:0:4136:e378:8000:63bf:3fff:fdd2", "2002:a9fe:a9fe::1", "2002:808:808::1"] {
            assert!(non_global(ip.parse().unwrap()), "{ip}");
        }
        assert!(check_destination(&json!({"url": "http://[64:ff9b::a9fe:a9fe]/latest/meta-data"}), false).is_err());
        assert!(check_destination(&json!({"url": "http://[::ffff:127.0.0.1]:8080/x"}), false).is_err());
        assert!(check_destination(&json!({"api_base": "http://2130706433"}), false).is_err());
        assert!(check_destination(&json!({"url": "https://qyapi.weixin.qq.com/x"}), false).is_ok());
        assert!(check_destination(&json!({"url": "http://127.0.0.1/x"}), true).is_ok());
    }

    #[test]
    fn helpers() {
        assert_eq!(fmt_duration(30), "1 分钟");
        assert_eq!(fmt_duration(0), "0 分钟");
        assert_eq!(fmt_duration(7800), "2 小时 10 分钟");
        assert_eq!(fmt_duration(90_000), "1 天 1 小时");
        let gib = 1024f64.powi(3);
        assert_eq!(fmt_bytes(1.84 * 1024.0 * gib), "1.84 TiB");
        assert_eq!(fmt_bytes(2048.0 * gib), "2 TiB");
        assert_eq!(fmt_bytes(950.0 * 1024.0 * 1024.0), "950 MiB");
        assert_eq!(fmt_bytes(12.5 * gib), "12.5 GiB");
        assert_eq!(fmt_bytes(512.0), "512 B");
        assert_eq!(strip_machine("团队1-02：5 分钟没有上报", "团队1-02"), "5 分钟没有上报");
        assert_eq!(strip_machine("别的：x", "团队1-02"), "别的：x");
        assert_eq!(crate::meta::rule_label("relay_xray_down"), "Xray 未运行");
        assert_eq!(crate::meta::rule_label("something_new"), "something_new");
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
