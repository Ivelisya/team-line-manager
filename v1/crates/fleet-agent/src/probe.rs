//! 探测：TCP 握手延迟；出口探测 = 经本机回环 SOCKS 入站（路由到该入口的真实出站）访问回显服务，核对出口 IP。

use anyhow::{bail, ensure, Context, Result};
use fleet_proto::{ProbeKind, ProbeSample};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uuid::Uuid;

const MAX_BODY: usize = 64 * 1024;

fn ms(t: Instant) -> f32 {
    (t.elapsed().as_secs_f64() * 1000.0) as f32
}

// ── TCP 探测 ────────────────────────────────────────────────────────────────

pub async fn tcp_probe(entry_id: Uuid, host: &str, port: u16, timeout: Duration, ts_ms: i64) -> ProbeSample {
    let t = Instant::now();
    let r = tokio::time::timeout(timeout, TcpStream::connect((host, port))).await;
    let (ok, detail) = match r {
        Ok(Ok(_)) => (true, None),
        Ok(Err(e)) => (false, Some(format!("connect: {e}"))),
        Err(_) => (false, Some(format!("timeout {}s", timeout.as_secs()))),
    };
    // 失败时不给延迟，避免被当成慢而不是挂
    ProbeSample { entry_id, ts_ms, kind: ProbeKind::Tcp, ok, latency_ms: ok.then(|| ms(t)), detail }
}

// ── 最小 SOCKS5 客户端（无认证，CONNECT）─────────────────────────────────────

pub async fn socks5_connect(proxy: SocketAddr, host: &str, port: u16) -> Result<TcpStream> {
    let mut s = TcpStream::connect(proxy).await.context("连接 SOCKS 入站")?;
    s.write_all(&[5, 1, 0]).await?;
    let mut r = [0u8; 2];
    s.read_exact(&mut r).await?;
    ensure!(r == [5, 0], "SOCKS5 方法协商失败：{r:?}");
    let mut req = vec![5, 1, 0];
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => {
            req.push(1);
            req.extend_from_slice(&a.octets());
        }
        Ok(IpAddr::V6(a)) => {
            req.push(4);
            req.extend_from_slice(&a.octets());
        }
        Err(_) => {
            ensure!(host.len() <= 255, "域名过长");
            req.push(3);
            req.push(host.len() as u8);
            req.extend_from_slice(host.as_bytes());
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await?;
    let mut h = [0u8; 4];
    s.read_exact(&mut h).await?;
    ensure!(h[0] == 5, "SOCKS5 响应版本错误");
    if h[1] != 0 {
        bail!("SOCKS5 CONNECT 被拒绝（REP={}）", h[1]);
    }
    let skip = match h[3] {
        1 => 4 + 2,
        4 => 16 + 2,
        3 => {
            let mut l = [0u8; 1];
            s.read_exact(&mut l).await?;
            l[0] as usize + 2
        }
        t => bail!("SOCKS5 响应地址类型未知：{t}"),
    };
    let mut junk = vec![0u8; skip];
    s.read_exact(&mut junk).await?;
    Ok(s)
}

// ── 极简 HTTP/1.1 GET ───────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

pub fn parse_url(u: &str) -> Result<Url> {
    let (https, rest) = if let Some(r) = u.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = u.strip_prefix("http://") {
        (false, r)
    } else {
        bail!("不支持的 URL：{u}");
    };
    let (auth, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = if let Some(v6) = auth.strip_prefix('[') {
        let (h, p) = v6.split_once(']').context("IPv6 URL 格式错误")?;
        (h.to_string(), p.strip_prefix(':').map(|x| x.parse()).transpose()?)
    } else if let Some((h, p)) = auth.rsplit_once(':') {
        (h.to_string(), Some(p.parse()?))
    } else {
        (auth.to_string(), None)
    };
    ensure!(!host.is_empty(), "URL 缺少主机");
    Ok(Url { https, host, port: port.unwrap_or(if https { 443 } else { 80 }), path: path.to_string() })
}

/// 解析 HTTP 响应：返回 (状态码, 正文)；支持 Content-Length、chunked、读到关闭
pub fn parse_http_response(raw: &[u8]) -> Result<(u16, Vec<u8>)> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").context("HTTP 响应头不完整")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];
    let mut lines = head.lines();
    let status: u16 = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).context("HTTP 状态行无效")?;
    let chunked = lines.any(|l| l.to_ascii_lowercase().replace(' ', "").starts_with("transfer-encoding:chunked"));
    if !chunked {
        return Ok((status, body.to_vec()));
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    loop {
        // 全程边界检查：恶意/损坏的响应（超大 chunk 长度、缺 CRLF）只能得到错误，不能 panic
        let rest = body.get(i..).context("chunk 头不完整")?;
        let nl = rest.windows(2).position(|w| w == b"\r\n").context("chunk 头不完整")?;
        let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..nl]).split(';').next().unwrap_or("").trim(), 16).context("chunk 长度无效")?;
        i = i.checked_add(nl + 2).context("chunk 偏移溢出")?;
        if size == 0 {
            break;
        }
        let end = i.checked_add(size).filter(|e| *e <= body.len()).context("chunk 数据不完整")?;
        out.extend_from_slice(&body[i..end]);
        i = end.checked_add(2).context("chunk 偏移溢出")?;
    }
    Ok((status, out))
}

/// 经 SOCKS5 代理发 GET，返回正文。http:// 走自带的最小实现；https:// 交给 reqwest（socks5h 代理）。
pub async fn http_get_via_socks(proxy: SocketAddr, url: &str, timeout: Duration) -> Result<String> {
    let u = parse_url(url)?;
    tokio::time::timeout(timeout, async {
        if u.https {
            let client = reqwest::Client::builder().proxy(reqwest::Proxy::all(format!("socks5h://{proxy}"))?).build()?;
            let r = client.get(url).send().await?;
            ensure!(r.status().is_success(), "HTTP {}", r.status());
            return Ok(r.text().await?);
        }
        let mut s = socks5_connect(proxy, &u.host, u.port).await?;
        let req = format!("GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: fleet-agent\r\nAccept: */*\r\nConnection: close\r\n\r\n", u.path, u.host);
        s.write_all(req.as_bytes()).await?;
        // 读满 Content-Length 就停：不等连接关闭（关闭要经 Xray 和 ss 两层传回来，会把延迟拉长约 1 s）
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = s.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            ensure!(raw.len() <= MAX_BODY, "响应太大");
            if let Some(h) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&raw[..h]).to_ascii_lowercase();
                let cl = head.lines().find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()));
                if cl.is_some_and(|cl| raw.len() >= h + 4 + cl) {
                    break;
                }
            }
        }
        let (status, body) = parse_http_response(&raw)?;
        ensure!((200..300).contains(&status), "HTTP {status}");
        Ok(String::from_utf8_lossy(&body).to_string())
    })
    .await
    .map_err(|_| anyhow::anyhow!("超时 {}s", timeout.as_secs()))?
}

fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

pub async fn egress_probe(entry_id: Uuid, probe_port: u16, echo_url: &str, expect: IpAddr, timeout: Duration, ts_ms: i64) -> ProbeSample {
    let t = Instant::now();
    let proxy = SocketAddr::from(([127, 0, 0, 1], probe_port));
    let (ok, detail) = match http_get_via_socks(proxy, echo_url, timeout).await {
        Ok(body) => match body.trim().parse::<IpAddr>() {
            Ok(seen) => {
                let ok = normalize(seen) == normalize(expect);
                (ok, Some(if ok { seen.to_string() } else { format!("出口 IP {seen}，期望 {expect}") }))
            }
            Err(_) => (false, Some(format!("回显内容不是 IP：{}", body.trim().chars().take(80).collect::<String>()))),
        },
        Err(e) => (false, Some(format!("{e:#}"))),
    };
    ProbeSample { entry_id, ts_ms, kind: ProbeKind::Egress, ok, latency_ms: ok.then(|| ms(t)), detail }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn urls() {
        assert_eq!(parse_url("http://a.com/ip").unwrap(), Url { https: false, host: "a.com".into(), port: 80, path: "/ip".into() });
        assert_eq!(parse_url("https://a.com:8443").unwrap(), Url { https: true, host: "a.com".into(), port: 8443, path: "/".into() });
        assert_eq!(parse_url("http://[::1]:81/x").unwrap().host, "::1");
        assert!(parse_url("ftp://a").is_err());
    }

    #[test]
    fn http_parsing() {
        let (s, b) = parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n1.2.3.4").unwrap();
        assert_eq!((s, b.as_slice()), (200, &b"1.2.3.4"[..]));
        let (_, b) = parse_http_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n1.2.\r\n3\r\n3.4\r\n0\r\n\r\n").unwrap();
        assert_eq!(b, b"1.2.3.4");
        assert!(parse_http_response(b"garbage").is_err());
        // 畸形 chunk：长度超大 / 数据后缺 CRLF / 只有半个头，都必须返回错误而不是 panic
        for bad in [
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\nx"[..],
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            assert!(parse_http_response(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }

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
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET /ip HTTP/1.1\r\nHost: echo.test"));
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
            s.write_all(resp.as_bytes()).await.unwrap();
            (host, port)
        });
        (addr, h)
    }

    #[tokio::test]
    async fn socks_client_and_egress_ok() {
        let (addr, h) = fake_socks(true, "203.0.113.9\n").await;
        let s = egress_probe(Uuid::nil(), addr.port(), "http://echo.test:8080/ip", "203.0.113.9".parse().unwrap(), Duration::from_secs(3), 1).await;
        assert!(s.ok, "{s:?}");
        assert!(s.latency_ms.is_some());
        assert_eq!(h.await.unwrap(), ("echo.test".to_string(), 8080));
    }

    #[tokio::test]
    async fn egress_mismatch_and_refused() {
        let (addr, _h) = fake_socks(true, "198.51.100.1").await;
        let s = egress_probe(Uuid::nil(), addr.port(), "http://echo.test/ip", "203.0.113.9".parse().unwrap(), Duration::from_secs(3), 1).await;
        assert!(!s.ok);
        assert!(s.latency_ms.is_none());
        assert!(s.detail.unwrap().contains("198.51.100.1"));
        let (addr, _h) = fake_socks(false, "").await;
        let s = egress_probe(Uuid::nil(), addr.port(), "http://echo.test/ip", "203.0.113.9".parse().unwrap(), Duration::from_secs(3), 1).await;
        assert!(!s.ok);
        assert!(s.detail.unwrap().contains("REP=5"));
    }

    #[tokio::test]
    async fn tcp_probe_ok_and_refused() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        let s = tcp_probe(Uuid::nil(), "127.0.0.1", p, Duration::from_secs(2), 1).await;
        assert!(s.ok && s.latency_ms.is_some());
        drop(l);
        let s = tcp_probe(Uuid::nil(), "127.0.0.1", p, Duration::from_secs(2), 1).await;
        assert!(!s.ok && s.latency_ms.is_none() && s.detail.is_some());
    }
}
