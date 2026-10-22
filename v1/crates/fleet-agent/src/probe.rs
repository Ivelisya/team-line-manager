//! 探测：线路机 → 节点的 TCP 建连（往返时间 + SYN 重发次数）；出口探测 = 经本机回环 SOCKS 入站（路由到该入口的真实出站）访问回显服务，核对出口 IP（只判通不通、IP 对不对）。

use crate::util::error_chain;
use anyhow::Result;
use fleet_proto::{tags, ProbeKind, ProbeSample};
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

/// TCP 探测的超时。SYN 丢了内核按 1 s、2 s、4 s 退避重发（第 0、1、3、7 秒各发一次）：原来的 5 秒只等到第二次重发，
/// 第三次重发才连上的被记成「超时」；10 秒盖住第三次重发和它的回包
pub const TCP_TIMEOUT: Duration = Duration::from_secs(10);

/// 已连上的套接字的 (往返时间 µs, SYN 重发次数)，取自内核 TCP_INFO。往返时间是内核拿 SYN-ACK 量的：
/// 重发过的建连按时间戳选项算（只算最后那次 SYN 的往返），量不到时为 0
#[cfg(target_os = "linux")]
fn tcp_info(s: &TcpStream) -> Option<(u32, u32)> {
    use std::os::fd::AsRawFd;
    // SAFETY: tcp_info 是纯数据结构，全零合法；getsockopt 最多写 len 字节
    let mut info: libc::tcp_info = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::tcp_info>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::IPPROTO_TCP, libc::TCP_INFO, (&mut info as *mut libc::tcp_info).cast(), &mut len) };
    (r == 0).then_some((info.tcpi_rtt, info.tcpi_total_retrans))
}

#[cfg(not(target_os = "linux"))]
fn tcp_info(_: &TcpStream) -> Option<(u32, u32)> {
    None
}

/// 建连延迟：有内核往返时间用它（不含重发等待、不受 tokio 调度影响）；内核没量到时，没重发过才用墙钟耗时，
/// 重发过就不给延迟（墙钟耗时里是 1 s / 3 s 的重发等待，不是线路延迟）
fn connect_latency(elapsed_ms: f32, info: Option<(u32, u32)>) -> Option<f32> {
    match info {
        Some((rtt_us, _)) if rtt_us > 0 => Some(rtt_us as f32 / 1000.0),
        Some((_, retrans)) if retrans > 0 => None,
        _ => Some(elapsed_ms),
    }
}

/// 线路机 → 节点出站地址的 TCP 建连。计时只算建连：域名先解析（不计时，解析失败照样算不通）；
/// 连上后读内核的往返时间与 SYN 重发次数（见 `connect_latency`、`ProbeSample::retrans`）
pub async fn tcp_probe(entry_id: Uuid, host: &str, port: u16, timeout: Duration, ts_ms: i64) -> ProbeSample {
    let fail = |detail: String| ProbeSample { entry_id, ts_ms, kind: ProbeKind::Tcp, ok: false, latency_ms: None, detail: Some(detail), retrans: None };
    let deadline = tokio::time::Instant::now() + timeout;
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => match tokio::time::timeout_at(deadline, tokio::net::lookup_host((host, port))).await {
            Ok(Ok(a)) => a.collect(),
            Ok(Err(e)) => {
                tracing::debug!(host, port, "TCP 探测解析失败：{e}");
                let (full, inner) = error_chain(&e);
                return fail(reason_cn(&full, &inner, timeout.as_secs()));
            }
            Err(_) => return fail(timeout_text(timeout.as_secs())),
        },
    };
    // 同 TcpStream::connect((host, port))：逐个地址试，用连上的那个；计时从这个地址开始
    let mut last = None;
    for a in addrs {
        let t = Instant::now();
        match tokio::time::timeout_at(deadline, TcpStream::connect(a)).await {
            Ok(Ok(s)) => {
                let info = tcp_info(&s);
                return ProbeSample {
                    entry_id,
                    ts_ms,
                    kind: ProbeKind::Tcp,
                    ok: true,
                    latency_ms: connect_latency(ms(t), info),
                    detail: None,
                    retrans: info.map(|i| i.1),
                };
            }
            Ok(Err(e)) => {
                tracing::debug!(host, port, "TCP 探测失败：{e}");
                last = Some(e);
            }
            Err(_) => return fail(timeout_text(timeout.as_secs())),
        }
    }
    // 失败时不给延迟，避免被当成慢而不是挂
    match last {
        Some(e) => {
            let (full, inner) = error_chain(&e);
            fail(reason_cn(&full, &inner, timeout.as_secs()))
        }
        None => fail("无法解析主机".into()),
    }
}

// ── 出口探测 ────────────────────────────────────────────────────────────────

/// 经本机 SOCKS 入站端口出去的客户端（socks5h，域名由代理解析）。新建客户端要初始化 TLS、加载根证书，
/// 调用方按端口留一个、随入口集合增删（collect::probe_task）。
/// 关掉连接池：出口探测要量的是"新建一条经代理的连接"的完整延迟，复用旧连接会让延迟偏低、也可能掩盖链路故障。
pub fn socks_client(port: u16) -> Result<reqwest::Client> {
    let proxy = SocketAddr::from(([127, 0, 0, 1], port));
    Ok(reqwest::Client::builder().proxy(reqwest::Proxy::all(format!("socks5h://{proxy}"))?).pool_max_idle_per_host(0).build()?)
}

/// 不走代理、从本机直连的客户端（不读环境变量里的代理）：出口探测失败时用它确认回显服务本身可达。
/// 同样关掉连接池，每次核对都是一条新连接
pub fn direct_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().no_proxy().pool_max_idle_per_host(0).build()?)
}

/// 直连回显服务的超时：线路机本来就直连中控上报，不必等出口探测那么久
const ECHO_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

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

/// 出口探测的结果：`answered` = 回显服务经这个入口回了一个 IP（对不对另看 ok）。回了 IP 却不对是实打实的出口错了，
/// 不必再核对回显服务本身通不通
struct Egress {
    sample: ProbeSample,
    answered: bool,
}

async fn egress_try(entry_id: Uuid, client: &reqwest::Client, echo_url: &str, expect: IpAddr, timeout: Duration, ts_ms: i64) -> Egress {
    let t = Instant::now();
    let (ok, answered, detail) = match http_get(client, echo_url, timeout).await {
        Ok(body) => match body.trim().parse::<IpAddr>() {
            Ok(seen) => {
                let ok = normalize(seen) == normalize(expect);
                (ok, true, Some(if ok { seen.to_string() } else { format!("出口 IP {seen}，期望 {expect}") }))
            }
            // 去掉控制字符：detail 进中控的 PG text 列，一个 NUL 就让整份报告被拒
            Err(_) => (false, false, Some(format!("回显内容不是 IP：{}", body.trim().chars().filter(|c| !c.is_control()).take(80).collect::<String>()))),
        },
        Err(why) => (false, false, Some(why)),
    };
    // latency_ms 是这次出口验证的耗时（经节点去中控的回显服务），不是线路延迟：界面上不当延迟读数
    Egress { sample: ProbeSample { entry_id, ts_ms, kind: ProbeKind::Egress, ok, latency_ms: ok.then(|| ms(t)), detail, retrans: None }, answered }
}

/// `client` 来自 `socks_client`（该入口的探测入站端口）。只有测试直接用；探测任务走 `egress_probe_checked`
#[cfg(test)]
pub async fn egress_probe(entry_id: Uuid, client: &reqwest::Client, echo_url: &str, expect: IpAddr, timeout: Duration, ts_ms: i64) -> ProbeSample {
    egress_try(entry_id, client, echo_url, expect, timeout, ts_ms).await.sample
}

/// 经入口访问的备用目标：回显服务经入口不通时，用它判断是入口的出口坏了还是只有回显服务这条路不通。
/// 美国就近、轻量（204 无正文）；探测入站的放行规则也放行它（render::probe_rules）
pub const EGRESS_ALT_URL: &str = "https://www.gstatic.com/generate_204";

/// 备用目标的超时：只判通不通
const ALT_CHECK_TIMEOUT: Duration = Duration::from_secs(8);

/// 出口探测；经入口没拿到 IP 时核对两次，任一说明问题不在这个入口的出口，就返回 None（这次不上报，只记 debug 日志）：
/// 1. 用 `direct`（`direct_client`）从线路机直连回显服务：也拿不到 IP，是回显服务本身（中控的 /echo：重启、部署）不可达；
/// 2. 经入口访问备用目标 `alt_url`：通，说明出口好好的，只是从节点去回显服务的那条路不通——10-22 22:50 起半小时，
///    边缘机的反代配置被部署覆盖，所有节点经出口访问 /echo 都 TLS 握手失败，而线路机直连仍拿得到 IP，全部入口被记成出口不通。
///
/// 回了 IP 但不是期望的（出口错了）不核对，照常上报失败
#[allow(clippy::too_many_arguments)]
pub async fn egress_probe_checked(entry_id: Uuid, client: &reqwest::Client, direct: &reqwest::Client, echo_url: &str, alt_url: &str, expect: IpAddr, timeout: Duration, ts_ms: i64) -> Option<ProbeSample> {
    let Egress { sample: s, answered } = egress_try(entry_id, client, echo_url, expect, timeout, ts_ms).await;
    if s.ok || answered {
        return Some(s);
    }
    let why = match http_get(direct, echo_url, ECHO_CHECK_TIMEOUT).await {
        Ok(body) if body.trim().parse::<IpAddr>().is_ok() => match http_get(client, alt_url, ALT_CHECK_TIMEOUT).await {
            Ok(_) => format!("经入口访问 {alt_url} 是通的"),
            Err(_) => return Some(s),
        },
        Ok(_) => "直连回显服务，回的不是 IP".to_string(),
        Err(why) => format!("直连回显服务也不通（{why}）"),
    };
    tracing::debug!(entry = %tags::inbound(entry_id), "出口探测失败（{}），但{why}，这次不上报", s.detail.as_deref().unwrap_or(""));
    None
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

    /// 测试用回显服务：每个连接回同一段正文（HTTP 200）
    async fn fake_echo(body: &'static str) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = vec![0u8; 1024];
                let _ = s.read(&mut buf).await;
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn egress_failure_counts_only_when_echo_reachable_directly() {
        let direct = direct_client().unwrap();
        let expect: IpAddr = "203.0.113.9".parse().unwrap();
        let check = |url: String| {
            let direct = direct.clone();
            async move {
                // 代理握手失败：经这个入口出不去（备用目标经它也出不去：假 SOCKS 只接一个连接）
                let (addr, _h) = fake_socks(false, "").await;
                egress_probe_checked(Uuid::nil(), &socks_client(addr.port()).unwrap(), &direct, &url, "http://alt.test/generate_204", expect, Duration::from_secs(3), 1).await
            }
        };
        // 直连回显服务拿得到 IP：失败照常上报
        let s = check(format!("http://{}/ip", fake_echo("198.51.100.7").await)).await.expect("要上报");
        assert_eq!((s.ok, s.detail.as_deref()), (false, Some("代理握手失败")));
        // 回显服务本身连不上（中控重启中）、或回的不是 IP（前面的反代回了错误页）：不上报
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        assert!(check(format!("http://{closed}/ip")).await.is_none());
        assert!(check(format!("http://{}/ip", fake_echo("<html>502</html>").await)).await.is_none());
        // 成功不必核对（echo.test 直连解析不了，核对了就会被当成不上报）
        let (addr, _h) = fake_socks(true, "203.0.113.9").await;
        let s = egress_probe_checked(Uuid::nil(), &socks_client(addr.port()).unwrap(), &direct, "http://echo.test/ip", "http://alt.test/", expect, Duration::from_secs(3), 1).await;
        assert!(s.is_some_and(|s| s.ok));
    }

    /// 测试用 SOCKS5：依次接受连接，第 n 个连接（不管目标）回 `resps[n]` 这段原始 HTTP 响应；记下每个连接的目标主机
    async fn fake_socks_seq(resps: Vec<String>) -> (SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let h = tokio::spawn(async move {
            let mut hosts = vec![];
            for resp in resps {
                let (mut s, _) = l.accept().await.unwrap();
                let mut g = [0u8; 3];
                s.read_exact(&mut g).await.unwrap();
                s.write_all(&[5, 0]).await.unwrap();
                let mut h = [0u8; 4];
                s.read_exact(&mut h).await.unwrap();
                let host = if h[3] == 3 {
                    let mut n = [0u8; 1];
                    s.read_exact(&mut n).await.unwrap();
                    let mut v = vec![0u8; n[0] as usize];
                    s.read_exact(&mut v).await.unwrap();
                    String::from_utf8(v).unwrap()
                } else {
                    let mut v = [0u8; 4];
                    s.read_exact(&mut v).await.unwrap();
                    std::net::Ipv4Addr::from(v).to_string()
                };
                let mut p = [0u8; 2];
                s.read_exact(&mut p).await.unwrap();
                hosts.push(host);
                s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
                let mut buf = vec![0u8; 1024];
                let _ = s.read(&mut buf).await.unwrap();
                s.write_all(resp.as_bytes()).await.unwrap();
            }
            hosts
        });
        (addr, h)
    }

    #[tokio::test]
    async fn egress_failure_not_counted_when_alt_target_reachable_via_entry() {
        let direct = direct_client().unwrap();
        let expect: IpAddr = "203.0.113.9".parse().unwrap();
        let echo = format!("http://{}/ip", fake_echo("198.51.100.7").await);
        let bad = "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n".to_string();
        let no_content = "HTTP/1.1 204 No Content\r\n\r\n".to_string();
        // 经入口去回显服务回 502，经入口去备用目标是通的：出口好好的，只是去回显服务的那条路不通 → 不上报
        let (addr, h) = fake_socks_seq(vec![bad.clone(), no_content]).await;
        assert!(egress_probe_checked(Uuid::nil(), &socks_client(addr.port()).unwrap(), &direct, &echo, "http://alt.test/generate_204", expect, Duration::from_secs(3), 1).await.is_none());
        assert_eq!(h.await.unwrap(), vec!["127.0.0.1".to_string(), "alt.test".to_string()]);
        // 备用目标经入口也不通：照常上报失败
        let (addr, _h) = fake_socks_seq(vec![bad.clone(), bad]).await;
        let s = egress_probe_checked(Uuid::nil(), &socks_client(addr.port()).unwrap(), &direct, &echo, "http://alt.test/generate_204", expect, Duration::from_secs(3), 1).await.expect("要上报");
        assert_eq!((s.ok, s.detail.as_deref()), (false, Some("HTTP 状态码 502")));
        // 回显服务经入口回了别的 IP：出口错了，不核对（直连地址是关着的端口，核对了就会变成不上报）
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let (addr, _h) = fake_socks_seq(vec!["HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\n198.51.100.1".into()]).await;
        let s = egress_probe_checked(Uuid::nil(), &socks_client(addr.port()).unwrap(), &direct, &format!("http://{closed}/ip"), "http://alt.test/", expect, Duration::from_secs(3), 1).await.expect("要上报");
        assert!(!s.ok && s.detail.unwrap().contains("198.51.100.1"));
    }

    #[test]
    fn connect_latency_excludes_syn_retransmit_waits() {
        // 内核量到的往返时间优先（重发过也是最后那次 SYN 的往返）
        assert_eq!(connect_latency(3091.0, Some((66_500, 2))), Some(66.5));
        assert_eq!(connect_latency(5.2, Some((4_800, 0))), Some(4.8));
        // 内核没量到：没重发过用墙钟耗时，重发过不给延迟（耗时里是重发等待）
        assert_eq!(connect_latency(5.2, Some((0, 0))), Some(5.2));
        assert_eq!(connect_latency(1062.0, Some((0, 1))), None);
        // 读不到 TCP_INFO（非 Linux）：墙钟耗时
        assert_eq!(connect_latency(7.0, None), Some(7.0));
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
        #[cfg(target_os = "linux")]
        assert_eq!(s.retrans, Some(0), "{s:?}");
        // 域名先解析再建连
        let s = tcp_probe(Uuid::nil(), "localhost", p, Duration::from_secs(2), 1).await;
        assert!(s.ok && s.latency_ms.is_some_and(|x| x < 1000.0), "{s:?}");
        drop(l);
        let s = tcp_probe(Uuid::nil(), "127.0.0.1", p, Duration::from_secs(2), 1).await;
        assert!(!s.ok && s.latency_ms.is_none());
        assert_eq!(s.detail.as_deref(), Some("连接被拒绝"));
    }
}
