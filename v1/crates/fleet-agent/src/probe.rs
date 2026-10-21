//! 探测：TCP 握手延迟；出口探测 = 经本机回环 SOCKS 入站（路由到该入口的真实出站）访问回显服务，核对出口 IP。

use crate::util::error_chain;
use anyhow::Result;
use fleet_proto::{ProbeKind, ProbeSample};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use uuid::Uuid;

fn ms(t: Instant) -> f32 {
    (t.elapsed().as_secs_f64() * 1000.0) as f32
}

/// 探测失败原因 → 中文（进健康原因、告警、通知）；原始错误只记 agent 日志。
/// `full` 是整条错误链（用来匹配），`inner` 是链上最里层的错误（认不出时原样带上它，顶层往往只有「error sending request」）
fn reason_cn(full: &str, inner: &str, timeout_s: u64) -> String {
    let l = full.to_ascii_lowercase();
    let has = |ks: &[&str]| ks.iter().any(|k| l.contains(k));
    if has(&["socks"]) {
        "代理握手失败".into()
    } else if has(&["refused"]) {
        "连接被拒绝".into()
    } else if has(&["reset"]) {
        "连接被重置".into()
    } else if has(&["unexpected eof", "incomplete message", "closed before message completed", "end of file"]) {
        "对端提前断开".into()
    } else if has(&["connection closed", "broken pipe", "connection aborted"]) {
        "连接被关闭".into()
    } else if has(&["tls", "ssl", "certificate", "handshake"]) {
        "TLS 握手失败".into()
    } else if has(&["unreachable"]) {
        "网络不可达".into()
    } else if has(&["lookup", "resolve", "dns", "not known"]) {
        "无法解析主机".into()
    } else if has(&["timed out", "timeout"]) {
        timeout_text(timeout_s)
    } else {
        format!("其他：{}", inner.chars().take(80).collect::<String>())
    }
}

/// 超时的统一说法
fn timeout_text(secs: u64) -> String { format!("超时 {secs} 秒") }

// ── TCP 探测 ────────────────────────────────────────────────────────────────

pub async fn tcp_probe(entry_id: Uuid, host: &str, port: u16, timeout: Duration, ts_ms: i64) -> ProbeSample {
    let t = Instant::now();
    let r = tokio::time::timeout(timeout, TcpStream::connect((host, port))).await;
    let (ok, detail) = match r {
        Ok(Ok(_)) => (true, None),
        Ok(Err(e)) => {
            tracing::debug!(host, port, "TCP 探测失败：{e}");
            let (full, inner) = error_chain(&e);
            (false, Some(reason_cn(&full, &inner, timeout.as_secs())))
        }
        Err(_) => (false, Some(timeout_text(timeout.as_secs()))),
    };
    // 失败时不给延迟，避免被当成慢而不是挂
    ProbeSample { entry_id, ts_ms, kind: ProbeKind::Tcp, ok, latency_ms: ok.then(|| ms(t)), detail }
}

// ── 出口探测 ────────────────────────────────────────────────────────────────

/// 经本机 SOCKS 入站端口出去的客户端（socks5h，域名由代理解析）。新建客户端要初始化 TLS、加载根证书，
/// 调用方按端口留一个、随入口集合增删（collect::probe_task）。
/// 关掉连接池：出口探测要量的是"新建一条经代理的连接"的完整延迟，复用旧连接会让延迟偏低、也可能掩盖链路故障。
pub fn socks_client(port: u16) -> Result<reqwest::Client> {
    let proxy = SocketAddr::from(([127, 0, 0, 1], port));
    Ok(reqwest::Client::builder().proxy(reqwest::Proxy::all(format!("socks5h://{proxy}"))?).pool_max_idle_per_host(0).build()?)
}

/// 回显正文最多读这么多字节：正常只有一个 IP，回显服务出错时可能回一整页，不整段读进内存
const ECHO_BODY_MAX: usize = 1024;

/// 发 GET，返回正文（最多 ECHO_BODY_MAX 字节）；失败返回中文原因。http / https 同一条路径。
async fn http_get(client: &reqwest::Client, url: &str, timeout: Duration) -> Result<String, String> {
    let r = tokio::time::timeout(timeout, async {
        let mut r = client.get(url).send().await?;
        if !r.status().is_success() {
            return Ok(Err(r.status().as_u16()));
        }
        let mut body = Vec::new();
        while body.len() < ECHO_BODY_MAX {
            let Some(c) = r.chunk().await? else { break };
            body.extend_from_slice(&c);
        }
        body.truncate(ECHO_BODY_MAX);
        Ok::<_, reqwest::Error>(Ok(String::from_utf8_lossy(&body).into_owned()))
    })
    .await;
    match r {
        Err(_) => Err(timeout_text(timeout.as_secs())),
        Ok(Ok(Ok(body))) => Ok(body),
        Ok(Ok(Err(code))) => Err(format!("HTTP 状态码 {code}")),
        Ok(Err(e)) => {
            let (full, inner) = error_chain(&e);
            tracing::debug!(url, "出口探测失败：{full}");
            Err(reason_cn(&full, &inner, timeout.as_secs()))
        }
    }
}

fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// `client` 来自 `socks_client`（该入口的探测入站端口）
pub async fn egress_probe(entry_id: Uuid, client: &reqwest::Client, echo_url: &str, expect: IpAddr, timeout: Duration, ts_ms: i64) -> ProbeSample {
    let t = Instant::now();
    let (ok, detail) = match http_get(client, echo_url, timeout).await {
        Ok(body) => match body.trim().parse::<IpAddr>() {
            Ok(seen) => {
                let ok = normalize(seen) == normalize(expect);
                (ok, Some(if ok { seen.to_string() } else { format!("出口 IP {seen}，期望 {expect}") }))
            }
            // 去掉控制字符：detail 进中控的 PG text 列，一个 NUL 就让整份报告被拒
            Err(_) => (false, Some(format!("回显内容不是 IP：{}", body.trim().chars().filter(|c| !c.is_control()).take(80).collect::<String>()))),
        },
        Err(why) => (false, Some(why)),
    };
    ProbeSample { entry_id, ts_ms, kind: ProbeKind::Egress, ok, latency_ms: ok.then(|| ms(t)), detail }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 测试用最小 SOCKS5 服务器：接受 CONNECT，记录目标，然后回一个固定 HTTP 响应
    async fn fake_socks(reply_ok: bool, body: &'static str) -> (SocketAddr, tokio::task::JoinHandle<(String, u16)>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let h = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut g = [0u8; 3];
            s.read_exact(&mut g).await.unwrap();
            assert_eq!(g, [5, 1, 0]);
            s.write_all(&[5, 0]).await.unwrap();
            let mut h = [0u8; 4];
            s.read_exact(&mut h).await.unwrap();
            assert_eq!(&h[..3], &[5, 1, 0]);
            let host = match h[3] {
                3 => {
                    let mut n = [0u8; 1];
                    s.read_exact(&mut n).await.unwrap();
                    let mut v = vec![0u8; n[0] as usize];
                    s.read_exact(&mut v).await.unwrap();
                    String::from_utf8(v).unwrap()
                }
                1 => {
                    let mut v = [0u8; 4];
                    s.read_exact(&mut v).await.unwrap();
                    std::net::Ipv4Addr::from(v).to_string()
                }
                _ => panic!(),
            };
            let mut p = [0u8; 2];
            s.read_exact(&mut p).await.unwrap();
            let port = u16::from_be_bytes(p);
            if !reply_ok {
                s.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
                return (host, port);
            }
            s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
            let mut buf = vec![0u8; 1024];
            let n = s.read(&mut buf).await.unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET /ip HTTP/1.1\r\n"));
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
            s.write_all(resp.as_bytes()).await.unwrap();
            (host, port)
        });
        (addr, h)
    }

    #[tokio::test]
    async fn socks_client_and_egress_ok() {
        let (addr, h) = fake_socks(true, "203.0.113.9\n").await;
        let s = egress_probe(Uuid::nil(), &socks_client(addr.port()).unwrap(), "http://echo.test:8080/ip", "203.0.113.9".parse().unwrap(), Duration::from_secs(3), 1).await;
        assert!(s.ok, "{s:?}");
        assert!(s.latency_ms.is_some());
        assert_eq!(h.await.unwrap(), ("echo.test".to_string(), 8080));
    }

    #[tokio::test]
    async fn egress_mismatch_and_refused() {
        let (addr, _h) = fake_socks(true, "198.51.100.1").await;
        let s = egress_probe(Uuid::nil(), &socks_client(addr.port()).unwrap(), "http://echo.test/ip", "203.0.113.9".parse().unwrap(), Duration::from_secs(3), 1).await;
        assert!(!s.ok);
        assert!(s.latency_ms.is_none());
        assert!(s.detail.unwrap().contains("198.51.100.1"));
        let (addr, _h) = fake_socks(false, "").await;
        let s = egress_probe(Uuid::nil(), &socks_client(addr.port()).unwrap(), "http://echo.test/ip", "203.0.113.9".parse().unwrap(), Duration::from_secs(3), 1).await;
        assert!(!s.ok && s.latency_ms.is_none());
        assert_eq!(s.detail.as_deref(), Some("代理握手失败"), "{s:?}");
    }

    #[tokio::test]
    async fn echo_body_capped_and_control_chars_dropped() {
        // 回显服务回了一大段带 NUL / 控制字符的正文
        let body: &'static str = Box::leak(format!("bad\0ip\u{7}\r\n{}", "x".repeat(4000)).into_boxed_str());
        let (addr, _h) = fake_socks(true, body).await;
        let got = http_get(&socks_client(addr.port()).unwrap(), "http://echo.test/ip", Duration::from_secs(3)).await.unwrap();
        assert_eq!(got.len(), ECHO_BODY_MAX);
        let (addr, _h) = fake_socks(true, body).await;
        let s = egress_probe(Uuid::nil(), &socks_client(addr.port()).unwrap(), "http://echo.test/ip", "203.0.113.9".parse().unwrap(), Duration::from_secs(3), 1).await;
        let d = s.detail.unwrap();
        assert!(!s.ok && !d.chars().any(char::is_control), "{d:?}");
        assert!(d.starts_with("回显内容不是 IP：badipxxx"), "{d}");
    }

    #[test]
    fn reasons_are_chinese() {
        let r = |s: &str| reason_cn(s, s, 5);
        assert_eq!(r("Connection refused (os error 111)"), "连接被拒绝");
        assert_eq!(r("failed to lookup address information: Name or service not known"), "无法解析主机");
        assert_eq!(r("Connection timed out (os error 110)"), "超时 5 秒");
        assert_eq!(r("error sending request: client error (Connect): socks connect error: Connection refused"), "代理握手失败");
        assert_eq!(r("Network is unreachable (os error 101)"), "网络不可达");
        assert_eq!(r("error sending request: connection error: Connection reset by peer (os error 104)"), "连接被重置");
        assert_eq!(r("error sending request: connection closed before message completed"), "对端提前断开");
        assert_eq!(r("error reading a body from connection: unexpected EOF during handshake"), "对端提前断开");
        assert_eq!(r("Broken pipe (os error 32)"), "连接被关闭");
        assert_eq!(r("error sending request: invalid peer certificate: UnknownIssuer"), "TLS 握手失败");
        // 认不出：带最里层的错误，不是顶层的「error sending request」
        assert_eq!(reason_cn("error sending request for url (http://x/): weird thing", "weird thing", 5), "其他：weird thing");
    }

    #[tokio::test]
    async fn tcp_probe_ok_and_refused() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        let s = tcp_probe(Uuid::nil(), "127.0.0.1", p, Duration::from_secs(2), 1).await;
        assert!(s.ok && s.latency_ms.is_some());
        drop(l);
        let s = tcp_probe(Uuid::nil(), "127.0.0.1", p, Duration::from_secs(2), 1).await;
        assert!(!s.ok && s.latency_ms.is_none());
        assert_eq!(s.detail.as_deref(), Some("连接被拒绝"));
    }
}
