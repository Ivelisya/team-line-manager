//! 与中控的 HTTP 交互：上报 / 长轮询期望状态 / 回报应用结果

use crate::util::error_chain;
use anyhow::{Context, Result};
use fleet_proto::{ApplyReport, DesiredState, ReportAck};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// 收到 401（令牌无效 / 被撤销）后停发这么久再试
pub const REVOKED_SLEEP: Duration = Duration::from_secs(600);

pub struct ServerClient {
    http: reqwest::Client,
    base: String,
    token_file: PathBuf,
    /// 缓存的令牌：第一次请求时读文件；收到 401 就丢掉，下次请求重读（手工换了令牌文件不必重启。
    /// 中控换令牌走「装 agent」那一步，会重启 agent）
    token: Mutex<Option<String>>,
    /// 令牌文件权限过宽的警告只打一次
    perm_warned: AtomicBool,
}

/// 小于这个字节数的请求体不压缩
const GZIP_MIN_BYTES: usize = 1024;

fn gzip(data: &[u8]) -> Vec<u8> {
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;
    let mut e = GzEncoder::new(Vec::with_capacity(data.len() / 4 + 64), Compression::new(3));
    e.write_all(data).and_then(|()| e.finish()).expect("写入内存 Vec 不会失败")
}

/// 回环 / 内网（RFC1918、链路本地、ULA）地址
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// 中控地址是明文 http:// 且目标不是回环/内网（主机名无法判断内外网，一律算公网）→ 令牌和报告会明文过公网
pub fn plain_http_over_public(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    if u.scheme() != "http" {
        return false;
    }
    let host = u.host_str().unwrap_or("").trim_matches(|c| c == '[' || c == ']');
    match host.parse::<IpAddr>() {
        Ok(ip) => !is_private_ip(ip),
        Err(_) => !host.eq_ignore_ascii_case("localhost"),
    }
}

/// 一次上报请求的分类结果
#[derive(Debug, PartialEq)]
pub enum Outcome<T> {
    Ok(T),
    /// 可重试（网络、全部 5xx、408、429、403、404），带建议等待；受队列的字节与时长上限约束
    Retry { wait: Option<Duration>, why: String },
    /// 令牌无效/被撤销：停止发送，长睡
    Revoked,
    /// 永久拒绝（400/413/422 等其它 4xx）：只有这类移入 dead/。服务端把数据问题（坏字节、违反约束）回 422，
    /// 基础设施问题（连接池超时、磁盘满、只读）回 503
    Rejected(String),
}

impl<T> Outcome<T> {
    /// Ok 里的值交给 `f` 继续分类（例如 2xx 但正文解析不了 → Retry），其余分类原样带过
    pub fn and_then<U>(self, f: impl FnOnce(T) -> Outcome<U>) -> Outcome<U> {
        match self {
            Outcome::Ok(v) => f(v),
            Outcome::Retry { wait, why } => Outcome::Retry { wait, why },
            Outcome::Revoked => Outcome::Revoked,
            Outcome::Rejected(s) => Outcome::Rejected(s),
        }
    }
}

/// 长轮询的结果
#[derive(Debug)]
pub enum Desired {
    /// 204：没有比 after 新的代数
    Unchanged,
    State(DesiredState),
    /// 有新代数，但本版本 agent 解析不了（中控新增了枚举值等）：只知道代数和解析错误
    Unreadable { generation: u64, error: String },
}

/// 先按 JSON 取出代数，再做强类型解析：解析不了也知道是哪一代，能把"版本过旧"回报给中控，
/// 而不是只记重试日志、永远停在旧代数
fn parse_desired(text: &str) -> Outcome<Desired> {
    let retry = |why: String| Outcome::Retry { wait: None, why };
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return retry(format!("DesiredState 不是合法 JSON：{e}")),
    };
    let Some(generation) = v.get("generation").and_then(|g| g.as_u64()) else { return retry("DesiredState 缺少 generation".into()) };
    match serde_json::from_value(v) {
        Ok(d) => Outcome::Ok(Desired::State(d)),
        Err(e) => Outcome::Ok(Desired::Unreadable { generation, error: e.to_string() }),
    }
}

/// HTTP 状态码 → 分类。纯函数，便于测试。
pub fn classify(status: u16, retry_after_s: Option<u64>, body: &str) -> Outcome<()> {
    let short = || format!("HTTP {status}: {}", body.chars().take(200).collect::<String>());
    match status {
        200..=299 => Outcome::Ok(()),
        401 => Outcome::Revoked,
        429 => Outcome::Retry { wait: Some(Duration::from_secs(retry_after_s.unwrap_or(5).clamp(1, 600))), why: short() },
        // 403：令牌与机器不匹配等，不丢数据，响亮地重试
        403 | 404 | 408 => Outcome::Retry { wait: None, why: short() },
        400..=499 => Outcome::Rejected(short()),
        _ => Outcome::Retry { wait: None, why: short() },
    }
}

/// 启动时调用一次：明文 http 过公网要打醒目警告（不拒绝，实验室用内网 http）
pub fn warn_if_plain_http(url: &str) {
    if plain_http_over_public(url) {
        tracing::warn!("!!! 安全警告：中控地址 {url} 是明文 http:// 且不在内网/回环：令牌与全部遥测会明文经公网传输，可被窃听、劫持。生产环境请改用 https:// !!!");
    }
}

impl ServerClient {
    pub fn new(base: &str, token_file: PathBuf) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("fleet-agent/{}", fleet_proto::build_version()))
            .build()?;
        Ok(Self { http, base: base.trim_end_matches('/').to_string(), token_file, token: Mutex::new(None), perm_warned: AtomicBool::new(false) })
    }

    /// 令牌：缓存里有就直接用，没有才读文件（读失败不缓存，下次再读）
    fn token(&self) -> Result<String> {
        let mut cached = self.token.lock().unwrap();
        if let Some(t) = cached.as_ref() {
            return Ok(t.clone());
        }
        let t = std::fs::read_to_string(&self.token_file).with_context(|| format!("读取令牌 {}", self.token_file.display()))?;
        self.warn_loose_perms();
        let t = t.trim().to_string();
        anyhow::ensure!(!t.is_empty(), "令牌文件为空");
        *cached = Some(t.clone());
        Ok(t)
    }

    /// 令牌文件应为 0600：组/其他用户可读就警告（只警告一次）
    fn warn_loose_perms(&self) {
        use std::os::unix::fs::PermissionsExt;
        let Ok(md) = std::fs::metadata(&self.token_file) else { return };
        let mode = md.permissions().mode() & 0o777;
        if mode & 0o077 != 0 && !self.perm_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(mode = format!("{mode:o}"), "令牌文件 {} 权限过宽，应为 0600（chmod 600）", self.token_file.display());
        }
    }

    fn retry_after(r: &reqwest::Response) -> Option<u64> {
        r.headers().get("retry-after")?.to_str().ok()?.trim().parse().ok()
    }

    /// 带上令牌发请求、读完正文，按状态码分类；Ok 里是 (状态码, 正文)。
    /// 读不到令牌：30 s 后重试；401：丢掉缓存的令牌，下次请求重读文件
    async fn call(&self, req: reqwest::RequestBuilder) -> Outcome<(u16, String)> {
        let token = match self.token() {
            Ok(t) => t,
            Err(e) => return Outcome::Retry { wait: Some(Duration::from_secs(30)), why: format!("{e:#}") },
        };
        // 拿到响应头之前的任何错误（含连接超时、长轮询时服务端挂起到客户端超时）都是失败。
        // reqwest 的顶层只有「error sending request for url (…)」，真正原因（连接被拒、DNS…）在 source 链里，整条拼上
        let r = match req.bearer_auth(token).send().await {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return Outcome::Retry { wait: None, why: format!("等待响应超时（服务端挂起？）：{}", error_chain(&e).0) },
            Err(e) => return Outcome::Retry { wait: None, why: format!("网络错误：{}", error_chain(&e).0) },
        };
        let (status, ra) = (r.status().as_u16(), Self::retry_after(&r));
        let text = match r.text().await {
            Ok(t) => t,
            // 2xx 之后读正文失败（超时、连接中断）：按失败计数退避。长轮询当成"无变化"的话失败被静默吞掉、立刻重拉
            Err(e) if (200..300).contains(&status) => return Outcome::Retry { wait: None, why: format!("HTTP {status} 但读取正文失败：{}", error_chain(&e).0) },
            Err(_) => String::new(),
        };
        let out = classify(status, ra, &text);
        if out == Outcome::Revoked {
            *self.token.lock().unwrap() = None;
        }
        out.and_then(|()| Outcome::Ok((status, text)))
    }

    /// 上报。请求体较大时 gzip 压缩（`Content-Encoding: gzip`，队列文件仍是明文 JSON，压缩只在发送时做）。
    /// 中控始终带解压层，agent 与中控同一版本发布，不做不支持压缩的回退。
    pub async fn post_report(&self, body: Vec<u8>) -> Outcome<ReportAck> {
        let gz = body.len() >= GZIP_MIN_BYTES;
        let body = if gz { tokio::task::spawn_blocking(move || gzip(&body)).await.expect("gzip 压缩任务异常终止") } else { body };
        let mut req = self.http.post(format!("{}/agent/v1/report", self.base)).header("content-type", "application/json").timeout(Duration::from_secs(15)).body(body);
        if gz {
            req = req.header("content-encoding", "gzip");
        }
        self.call(req).await.and_then(|(_, text)| match serde_json::from_str::<ReportAck>(&text) {
            Ok(a) => Outcome::Ok(a),
            // 2xx 但正文不对（中间代理的页面等）：照常重试
            Err(e) => Outcome::Retry { wait: None, why: format!("ReportAck 解析失败：{e}") },
        })
    }

    /// 长轮询：服务端 hold 到有比 `after` 新的代数，或到期回 204
    pub async fn get_desired(&self, after: u64) -> Outcome<Desired> {
        let req = self.http.get(format!("{}/agent/v1/desired", self.base)).query(&[("after", after)]).timeout(Duration::from_secs(50));
        match self.call(req).await {
            // 拉取被拒（4xx）没有可丢弃的数据，照常重试
            Outcome::Rejected(why) => Outcome::Retry { wait: None, why },
            o => o.and_then(|(status, text)| if status == 204 { Outcome::Ok(Desired::Unchanged) } else { parse_desired(&text) }),
        }
    }

    pub async fn post_applied(&self, rep: &ApplyReport) -> Outcome<()> {
        let req = self.http.post(format!("{}/agent/v1/applied", self.base)).timeout(Duration::from_secs(15)).json(rep);
        self.call(req).await.and_then(|_| Outcome::Ok(()))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 请求记录：(是否带 content-encoding: gzip, 解压后的正文)
    pub type Seen = Arc<Mutex<Vec<(bool, Vec<u8>)>>>;

    /// 最小 HTTP 服务端：每个连接一个请求，一律回 200 和 `{"accepted_seq":9,"interval_s":10}`。
    pub async fn mock_server() -> (String, Seen) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let seen: Seen = Default::default();
        let seen2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let seen = seen2.clone();
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 8192];
                    let (head_end, cl) = loop {
                        let n = s.read(&mut buf).await.unwrap();
                        raw.extend_from_slice(&buf[..n]);
                        if let Some(h) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&raw[..h]).to_ascii_lowercase();
                            let cl = head.lines().find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                            break (h + 4, cl);
                        }
                    };
                    while raw.len() < head_end + cl {
                        let n = s.read(&mut buf).await.unwrap();
                        raw.extend_from_slice(&buf[..n]);
                    }
                    let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
                    let gz = head.contains("content-encoding: gzip");
                    let body = raw[head_end..head_end + cl].to_vec();
                    let plain = if gz {
                        let mut out = Vec::new();
                        flate2::read::GzDecoder::new(&body[..]).read_to_end(&mut out).unwrap();
                        out
                    } else {
                        body
                    };
                    seen.lock().unwrap().push((gz, plain));
                    let ack = r#"{"accepted_seq":9,"interval_s":10}"#;
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ack}", ack.len());
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// 令牌文件放在临时目录里；目录一并返回，测试结束时删掉
    pub fn client_for(base: &str, tag: &str) -> (ServerClient, PathBuf) {
        let d = std::env::temp_dir().join(format!("fa-srv-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let tok = d.join("token");
        std::fs::write(&tok, "t0k\n").unwrap();
        (ServerClient::new(base, tok).unwrap(), d)
    }

    #[test]
    fn gzip_roundtrip() {
        let data = br#"{"host":[{"cpu_pct":1.5}]}"#.repeat(200);
        let gz = gzip(&data);
        assert!(gz.len() < data.len() / 4);
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn large_reports_are_gzipped_and_small_ones_are_not() {
        let (base, seen) = mock_server().await;
        let (c, d) = client_for(&base, "new");
        let big = br#"{"seq":1,"pad":"xxxxxxxxxxxxxxxx"}"#.repeat(100);
        assert!(matches!(c.post_report(big.clone()).await, Outcome::Ok(a) if a.accepted_seq == 9));
        assert!(matches!(c.post_report(b"{}".to_vec()).await, Outcome::Ok(_)));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].0 && seen[0].1 == big, "大报告应以 gzip 发出，服务端解压后与原文一致");
        assert!(!seen[1].0 && seen[1].1 == b"{}", "小报告不压缩");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unreadable_desired_still_yields_generation() {
        use crate::xray::render::tests::{desired, entry};
        let mut v = serde_json::to_value(desired(vec![entry(1, 20001, true)])).unwrap();
        assert!(matches!(parse_desired(&v.to_string()), Outcome::Ok(Desired::State(d)) if d.generation == 7));
        // 中控新增了出站协议：强类型解析失败，但代数照样取得到，可以回报"版本过旧"
        v["entries"][0]["outbound"]["protocol"] = "wireguard".into();
        assert!(matches!(parse_desired(&v.to_string()), Outcome::Ok(Desired::Unreadable { generation: 7, .. })));
        assert!(matches!(parse_desired("<html>"), Outcome::Retry { .. }));
    }

    #[test]
    fn status_classification() {
        assert_eq!(classify(200, None, ""), Outcome::Ok(()));
        assert_eq!(classify(401, None, ""), Outcome::Revoked);
        assert!(matches!(classify(429, Some(7), ""), Outcome::Retry { wait: Some(d), .. } if d.as_secs() == 7));
        assert!(matches!(classify(429, Some(9999), ""), Outcome::Retry { wait: Some(d), .. } if d.as_secs() == 600));
        assert!(matches!(classify(403, None, "x"), Outcome::Retry { .. }));
        // 5xx 一律重试，永不移入 dead/（数据问题服务端回 422）
        for s in [500, 502, 503, 504] {
            assert!(matches!(classify(s, None, "x"), Outcome::Retry { .. }), "{s}");
        }
        for s in [408, 429, 403, 404] {
            assert!(matches!(classify(s, None, "x"), Outcome::Retry { .. }), "{s}");
        }
        for s in [400, 413, 422] {
            assert!(matches!(classify(s, None, "bad"), Outcome::Rejected(_)), "{s}");
        }
    }

    #[test]
    fn plain_http_warning_rules() {
        // 实验室：内网明文 http 不警告，也不拒绝
        assert!(!plain_http_over_public("http://192.168.31.60:8080"));
        assert!(!plain_http_over_public("http://10.1.2.3"));
        assert!(!plain_http_over_public("http://172.16.0.9:80/x"));
        assert!(!plain_http_over_public("http://127.0.0.1:8080"));
        assert!(!plain_http_over_public("http://localhost:8080"));
        assert!(!plain_http_over_public("http://[::1]:8080"));
        assert!(!plain_http_over_public("http://[fd00::1]:8080"));
        // 公网 / 域名的明文 http 要警告
        assert!(plain_http_over_public("http://203.0.113.5:8080"));
        assert!(plain_http_over_public("http://172.32.0.1"));
        assert!(plain_http_over_public("http://fleet.example.com"));
        assert!(plain_http_over_public("http://[2001:db8::1]:80"));
        // https 不警告；无法解析的不警告（别处会报错）
        assert!(!plain_http_over_public("https://fleet.example.com"));
        assert!(!plain_http_over_public("ftp://x"));
    }
}
