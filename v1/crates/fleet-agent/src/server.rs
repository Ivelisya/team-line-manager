//! 与中控的 HTTP 交互：上报 / 长轮询期望状态 / 回报应用结果

use anyhow::{Context, Result};
use fleet_proto::{ApplyReport, DesiredState, ReportAck};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

pub struct ServerClient {
    http: reqwest::Client,
    base: String,
    token_file: PathBuf,
    /// 令牌文件权限过宽的警告只打一次
    perm_warned: AtomicBool,
    /// 服务端不认 gzip 请求体时，这个时刻（unix ms）之前不再压缩
    gzip_off_until_ms: AtomicI64,
}

/// 小于这个字节数的请求体不压缩
const GZIP_MIN_BYTES: usize = 1024;
const GZIP_RETRY_AFTER_MS: i64 = 3_600_000;

fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;
    let mut e = GzEncoder::new(Vec::with_capacity(data.len() / 4 + 64), Compression::new(3));
    e.write_all(data)?;
    e.finish()
}

/// 服务端不认识 `Content-Encoding: gzip` 时的典型响应
fn gzip_unsupported_status(status: u16) -> bool {
    matches!(status, 400 | 415 | 422)
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
    let Ok(u) = crate::probe::parse_url(url) else { return false };
    if u.https {
        return false;
    }
    match u.host.parse::<IpAddr>() {
        Ok(ip) => !is_private_ip(ip),
        Err(_) => !u.host.eq_ignore_ascii_case("localhost"),
    }
}

/// 一次上报请求的分类结果
#[derive(Debug, PartialEq)]
pub enum Outcome<T> {
    Ok(T),
    /// 可重试（网络、5xx、429、403…），带建议等待
    /// `server_err` = 服务端 5xx（区别于网络错误等）：同一 seq 反复 5xx 时上报器据此把它移入 dead/
    Retry { wait: Option<Duration>, why: String, server_err: bool },
    /// 令牌无效/被撤销：停止发送，长睡
    Revoked,
    /// 永久拒绝（400/413/422 等其它 4xx）
    Rejected(String),
}

/// HTTP 状态码 → 分类。纯函数，便于测试。
pub fn classify(status: u16, retry_after_s: Option<u64>, body: &str) -> Outcome<()> {
    let short = || format!("HTTP {status}: {}", body.chars().take(200).collect::<String>());
    match status {
        200..=299 => Outcome::Ok(()),
        401 => Outcome::Revoked,
        429 => Outcome::Retry { wait: Some(Duration::from_secs(retry_after_s.unwrap_or(5).clamp(1, 600))), why: short(), server_err: false },
        // 403：令牌与机器不匹配等，不丢数据，响亮地重试
        403 | 404 | 408 => Outcome::Retry { wait: None, why: short(), server_err: false },
        400..=499 => Outcome::Rejected(short()),
        _ => Outcome::Retry { wait: None, why: short(), server_err: status >= 500 },
    }
}

/// 启动时调用一次：明文 http 过公网要打醒目警告（不拒绝，实验室用内网 http）
pub fn warn_if_plain_http(url: &str) {
    if plain_http_over_public(url) {
        tracing::warn!("!!! 安全警告：中控地址 {url} 是明文 http:// 且不在内网/回环：令牌与全部遥测会明文经公网传输，可被窃听、劫持。生产环境请改用 https:// !!!");
    }
}

impl ServerClient {
    pub fn new(base: &str, token_file: PathBuf, ca_file: Option<&PathBuf>) -> Result<Self> {
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("fleet-agent/{}", env!("CARGO_PKG_VERSION")));
        if let Some(ca) = ca_file {
            let pem = std::fs::read(ca).with_context(|| format!("读取 CA {}", ca.display()))?;
            b = b.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
        }
        Ok(Self { http: b.build()?, base: base.trim_end_matches('/').to_string(), token_file, perm_warned: AtomicBool::new(false), gzip_off_until_ms: AtomicI64::new(0) })
    }

    /// 每次请求重读令牌，换令牌无需重启
    fn token(&self) -> Result<String> {
        let t = std::fs::read_to_string(&self.token_file).with_context(|| format!("读取令牌 {}", self.token_file.display()))?;
        self.warn_loose_perms();
        let t = t.trim().to_string();
        anyhow::ensure!(!t.is_empty(), "令牌文件为空");
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

    /// 上报。请求体较大时 gzip 压缩（`Content-Encoding: gzip`，队列文件仍是明文 JSON，压缩只在发送时做）。
    /// 兼容旧服务端：旧服务端不认 Content-Encoding，会把压缩体当 JSON 解析失败回 400/415/422。
    /// 这时同一份报告立刻明文重发，明文成功才断定对端不支持压缩，之后 1 小时内不再压缩（避免把报告当毒丸扔进 dead/）。
    pub async fn post_report(&self, body: Vec<u8>) -> Outcome<ReportAck> {
        let token = match self.token() {
            Ok(t) => t,
            Err(e) => return Outcome::Retry { wait: Some(Duration::from_secs(30)), why: format!("{e:#}"), server_err: false },
        };
        if body.len() < GZIP_MIN_BYTES || crate::util::now_ms() < self.gzip_off_until_ms.load(Ordering::Relaxed) {
            return self.send_report(&token, body, false).await.0;
        }
        let (body, gz) = tokio::task::spawn_blocking(move || {
            let gz = gzip(&body);
            (body, gz)
        })
        .await
        .expect("gzip 压缩任务异常终止");
        let Ok(gz) = gz else { return self.send_report(&token, body, false).await.0 };
        let (out, status) = self.send_report(&token, gz, true).await;
        if !(matches!(out, Outcome::Rejected(_)) && gzip_unsupported_status(status)) {
            return out;
        }
        let (plain, _) = self.send_report(&token, body, false).await;
        if matches!(plain, Outcome::Ok(_)) {
            tracing::warn!("服务端不接受 gzip 请求体（HTTP {status}），改发明文，1 小时后再试压缩");
            self.gzip_off_until_ms.store(crate::util::now_ms() + GZIP_RETRY_AFTER_MS, Ordering::Relaxed);
        }
        plain
    }

    /// 发一次上报请求，返回分类结果和 HTTP 状态码（网络错误等没有响应时为 0）
    async fn send_report(&self, token: &str, body: Vec<u8>, gz: bool) -> (Outcome<ReportAck>, u16) {
        let mut req = self.http.post(format!("{}/agent/v1/report", self.base)).bearer_auth(token).header("content-type", "application/json");
        if gz {
            req = req.header("content-encoding", "gzip");
        }
        let res = req.timeout(Duration::from_secs(15)).body(body).send().await;
        let r = match res {
            Ok(r) => r,
            Err(e) => return (Outcome::Retry { wait: None, why: format!("网络错误：{e}"), server_err: false }, 0),
        };
        let (status, ra) = (r.status().as_u16(), Self::retry_after(&r));
        let text = r.text().await.unwrap_or_default();
        let out = match classify(status, ra, &text) {
            Outcome::Ok(()) => match serde_json::from_str::<ReportAck>(&text) {
                Ok(a) => Outcome::Ok(a),
                Err(e) => Outcome::Retry { wait: None, why: format!("ReportAck 解析失败：{e}"), server_err: true },
            },
            Outcome::Retry { wait, why, server_err } => Outcome::Retry { wait, why, server_err },
            Outcome::Revoked => Outcome::Revoked,
            Outcome::Rejected(s) => Outcome::Rejected(s),
        };
        (out, status)
    }

    /// 长轮询；`Ok(None)` = 204（无变化）
    pub async fn get_desired(&self, after: u64) -> Outcome<Option<DesiredState>> {
        let token = match self.token() {
            Ok(t) => t,
            Err(e) => return Outcome::Retry { wait: Some(Duration::from_secs(30)), why: format!("{e:#}"), server_err: false },
        };
        let res = self
            .http
            .get(format!("{}/agent/v1/desired", self.base))
            .query(&[("after", after)])
            .bearer_auth(token)
            .timeout(Duration::from_secs(50))
            .send()
            .await;
        // 服务端自己会在 hold 到期后回 204；这里在拿到响应头之前的任何错误（含连接超时、服务端挂起到客户端超时）都是失败
        let r = match res {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return Outcome::Retry { wait: None, why: format!("等待响应头超时（服务端挂起？）：{e}"), server_err: false },
            Err(e) => return Outcome::Retry { wait: None, why: format!("网络错误：{e}"), server_err: false },
        };
        let (status, ra) = (r.status().as_u16(), Self::retry_after(&r));
        if status == 204 {
            return Outcome::Ok(None);
        }
        let text = match r.text().await {
            Ok(t) => t,
            // 已收到响应头之后读正文超时：当作无变化（下一轮带同一个 after 重新拉）
            Err(e) if e.is_timeout() && (200..300).contains(&status) => return Outcome::Ok(None),
            Err(_) => String::new(),
        };
        match classify(status, ra, &text) {
            Outcome::Ok(()) => match serde_json::from_str::<DesiredState>(&text) {
                Ok(d) => Outcome::Ok(Some(d)),
                Err(e) => Outcome::Retry { wait: None, why: format!("DesiredState 解析失败：{e}"), server_err: false },
            },
            Outcome::Retry { wait, why, server_err } => Outcome::Retry { wait, why, server_err },
            Outcome::Revoked => Outcome::Revoked,
            Outcome::Rejected(s) => Outcome::Retry { wait: None, why: s, server_err: false },
        }
    }

    pub async fn post_applied(&self, rep: &ApplyReport) -> Outcome<()> {
        let token = match self.token() {
            Ok(t) => t,
            Err(e) => return Outcome::Retry { wait: Some(Duration::from_secs(30)), why: format!("{e:#}"), server_err: false },
        };
        let res = self
            .http
            .post(format!("{}/agent/v1/applied", self.base))
            .bearer_auth(token)
            .timeout(Duration::from_secs(15))
            .json(rep)
            .send()
            .await;
        let r = match res {
            Ok(r) => r,
            Err(e) => return Outcome::Retry { wait: None, why: format!("网络错误：{e}"), server_err: false },
        };
        let (status, ra) = (r.status().as_u16(), Self::retry_after(&r));
        let text = r.text().await.unwrap_or_default();
        classify(status, ra, &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 请求记录：(是否带 content-encoding: gzip, 解压后的正文)
    type Seen = Arc<Mutex<Vec<(bool, Vec<u8>)>>>;

    /// 最小 HTTP 服务端：每个连接一个请求。`understands_gzip=false` 模拟旧服务端（拿压缩体当 JSON 解析失败回 400）。
    async fn mock_server(understands_gzip: bool) -> (String, Seen) {
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
                    let resp = if gz && !understands_gzip {
                        "HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_string()
                    } else {
                        let plain = if gz {
                            let mut out = Vec::new();
                            flate2::read::GzDecoder::new(&body[..]).read_to_end(&mut out).unwrap();
                            out
                        } else {
                            body
                        };
                        seen.lock().unwrap().push((gz, plain));
                        let ack = r#"{"accepted_seq":9,"interval_s":10}"#;
                        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ack}", ack.len())
                    };
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn client_for(base: &str, tag: &str) -> ServerClient {
        let d = std::env::temp_dir().join(format!("fa-srv-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let tok = d.join("token");
        std::fs::write(&tok, "t0k\n").unwrap();
        ServerClient::new(base, tok, None).unwrap()
    }

    #[test]
    fn gzip_roundtrip() {
        let data = br#"{"host":[{"cpu_pct":1.5}]}"#.repeat(200);
        let gz = gzip(&data).unwrap();
        assert!(gz.len() < data.len() / 4);
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn large_reports_are_gzipped_and_small_ones_are_not() {
        let (base, seen) = mock_server(true).await;
        let c = client_for(&base, "new");
        let big = br#"{"seq":1,"pad":"xxxxxxxxxxxxxxxx"}"#.repeat(100);
        assert!(matches!(c.post_report(big.clone()).await, Outcome::Ok(a) if a.accepted_seq == 9));
        assert!(matches!(c.post_report(b"{}".to_vec()).await, Outcome::Ok(_)));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].0 && seen[0].1 == big, "大报告应以 gzip 发出，服务端解压后与原文一致");
        assert!(!seen[1].0 && seen[1].1 == b"{}", "小报告不压缩");
    }

    #[tokio::test]
    async fn old_server_without_gzip_support_falls_back_to_plain() {
        let (base, seen) = mock_server(false).await;
        let c = client_for(&base, "old");
        let big = br#"{"seq":1,"pad":"xxxxxxxxxxxxxxxx"}"#.repeat(100);
        // 旧服务端对压缩体回 400：同一次调用里明文重发并成功，不能把报告判为永久拒绝
        assert!(matches!(c.post_report(big.clone()).await, Outcome::Ok(_)));
        assert!(c.gzip_off_until_ms.load(Ordering::Relaxed) > crate::util::now_ms());
        // 之后一段时间直接发明文
        assert!(matches!(c.post_report(big.clone()).await, Outcome::Ok(_)));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|(gz, body)| !gz && *body == big));
    }

    #[test]
    fn gzip_unsupported_statuses() {
        assert!(gzip_unsupported_status(400) && gzip_unsupported_status(415) && gzip_unsupported_status(422));
        assert!(!gzip_unsupported_status(413) && !gzip_unsupported_status(500));
    }

    #[test]
    fn status_classification() {
        assert_eq!(classify(200, None, ""), Outcome::Ok(()));
        assert_eq!(classify(401, None, ""), Outcome::Revoked);
        assert!(matches!(classify(429, Some(7), ""), Outcome::Retry { wait: Some(d), .. } if d.as_secs() == 7));
        assert!(matches!(classify(429, Some(9999), ""), Outcome::Retry { wait: Some(d), .. } if d.as_secs() == 600));
        assert!(matches!(classify(403, None, "x"), Outcome::Retry { .. }));
        assert!(matches!(classify(503, None, "x"), Outcome::Retry { server_err: true, .. }));
        assert!(matches!(classify(429, None, "x"), Outcome::Retry { server_err: false, .. }));
        assert!(matches!(classify(403, None, "x"), Outcome::Retry { server_err: false, .. }));
        assert!(matches!(classify(400, None, "bad"), Outcome::Rejected(_)));
        assert!(matches!(classify(413, None, "big"), Outcome::Rejected(_)));
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
