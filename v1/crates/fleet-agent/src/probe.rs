//! 探测：TCP 握手延迟；出口探测 = 经本机回环 SOCKS 入站（路由到该入口的真实出站）访问回显服务，核对出口 IP。

use anyhow::{ensure, Result};
use fleet_proto::{ProbeKind, ProbeSample};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use uuid::Uuid;

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

// ── 出口探测 ────────────────────────────────────────────────────────────────

/// 每个 SOCKS 入站端口复用一个 reqwest 客户端：新建客户端要初始化 TLS 配置、加载根证书，在小内存机器上不便宜。
/// 关掉连接池：出口探测要量的是"新建一条经代理的连接"的完整延迟，复用旧连接会让延迟偏低、也可能掩盖链路故障。
fn client_for(proxy: SocketAddr) -> Result<reqwest::Client> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    /// 入口被删除后端口会遗留在表里，超过这个数量就挤掉一个（用到时会按需重建）
    const MAX_CLIENTS: usize = 64;
    static CLIENTS: OnceLock<Mutex<HashMap<SocketAddr, reqwest::Client>>> = OnceLock::new();
    let mut m = CLIENTS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = m.get(&proxy) {
        return Ok(c.clone());
    }
    let c = reqwest::Client::builder().proxy(reqwest::Proxy::all(format!("socks5h://{proxy}"))?).pool_max_idle_per_host(0).build()?;
    if m.len() >= MAX_CLIENTS {
        if let Some(k) = m.keys().next().copied() {
            m.remove(&k);
        }
    }
    m.insert(proxy, c.clone());
    Ok(c)
}

/// 经 SOCKS5 代理（socks5h，域名由代理解析）发 GET，返回正文。http / https 同一条路径。
async fn http_get_via_socks(proxy: SocketAddr, url: &str, timeout: Duration) -> Result<String> {
    tokio::time::timeout(timeout, async {
        let r = client_for(proxy)?.get(url).send().await?;
        ensure!(r.status().is_success(), "HTTP {}", r.status());
        Ok(r.text().await?)
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
        assert!(!s.ok && s.latency_ms.is_none() && s.detail.is_some());
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
