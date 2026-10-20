//! 单次检查。每个函数只做一次尝试并返回一条 [`ProbeResult`]；重试、并发与节奏由 scheduler 负责。
//!
//! 与旧 probe.py 的对应：
//! - `tcp_check`：`socket.create_connection((target, port), timeout=5)`，成功值取整毫秒且至少 1；
//! - `egress_attempt`：curl `--socks5-hostname` 依次访问 Cloudflare trace、ipify，
//!   任一返回期望 IP 即成功；返回了别的 IP 立即判 `egress_mismatch`（不再试下一个来源）；都失败取最后一个错误。
use std::net::IpAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::http::{self, Phase, Transport};
use crate::ss::{self, ServerCfg, SsStream, Target};
use crate::stun;
use crate::targets::NodeTarget;
use crate::types::{legacy_rc, reason, CheckKind, Phases, ProbeResult, Status, CONTROL_NODE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    /// Cloudflare `/cdn-cgi/trace`：`ip=…` 行
    Trace,
    /// 正文就是 IP
    Plain,
}

/// 公共出口来源
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    pub name: &'static str,
    pub host: &'static str,
    pub path: &'static str,
    pub kind: BodyKind,
}

pub const CLOUDFLARE: Source = Source { name: "cloudflare", host: "www.cloudflare.com", path: "/cdn-cgi/trace", kind: BodyKind::Trace };
pub const IPIFY: Source = Source { name: "ipify", host: "api.ipify.org", path: "/", kind: BodyKind::Plain };
/// L2 来源顺序与旧系统一致：① Cloudflare trace ② ipify
pub const SOURCES: [Source; 2] = [CLOUDFLARE, IPIFY];
/// 只有 AAAA 记录的回显端点
pub const IPV6_SOURCE: Source = Source { name: "ipify6", host: "api6.ipify.org", path: "/", kind: BodyKind::Plain };
/// UDP 出口一致性用的公共 STUN
pub const STUN_SOURCES: [(&str, &str, u16); 2] = [("stun.cloudflare", "stun.cloudflare.com", 3478), ("stun.google", "stun.l.google.com", 19302)];

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn ms(start: Instant) -> f64 {
    (start.elapsed().as_secs_f64() * 1000.0 * 10.0).round() / 10.0
}

/// 旧系统口径：整毫秒、至少 1
fn legacy_ms(start: Instant) -> f64 {
    (start.elapsed().as_secs_f64() * 1000.0).round().max(1.0)
}

fn trim(s: impl AsRef<str>) -> String {
    s.as_ref().chars().take(200).collect()
}

fn finish(mut r: ProbeResult) -> ProbeResult {
    r.legacy_rc = Some(legacy_rc(r.status, r.reason_code.as_deref()));
    r
}

/// 从响应正文取 IP
pub fn extract_ip(kind: BodyKind, body: &[u8]) -> Option<IpAddr> {
    let s = String::from_utf8_lossy(body);
    let v = match kind {
        BodyKind::Trace => s.lines().find_map(|l| l.strip_prefix("ip="))?.trim().to_string(),
        BodyKind::Plain => s.trim().to_string(),
    };
    v.parse().ok()
}

/// L1：TCP connect 到落地 SS 端口
pub async fn tcp_check(node: &NodeTarget, limit: Duration) -> ProbeResult {
    let start = Instant::now();
    let res = timeout(limit, async {
        let addr = node.socket_addr().await?;
        TcpStream::connect(addr).await
    })
    .await;
    let mut r = ProbeResult::new(&node.node, CheckKind::Tcp, Status::Ok, now());
    match res {
        Ok(Ok(_s)) => r.value_ms = Some(legacy_ms(start)),
        Ok(Err(e)) => {
            r.status = Status::Fail;
            r.reason_code = Some(reason::UPSTREAM_DOWN.into());
            r.detail = Some(trim(format!("{:?}: {e}", e.kind())));
        }
        Err(_) => {
            r.status = Status::Timeout;
            r.reason_code = Some(reason::UPSTREAM_DOWN.into());
            r.detail = Some("connect timeout".into());
        }
    }
    finish(r)
}

enum Via<'a> {
    Direct,
    Ss(&'a ServerCfg),
    /// SOCKS5 代理（NodeMach）：(服务器地址, 用户名, 密码)
    Socks(std::net::SocketAddr, &'a str, &'a str),
}

/// SOCKS5 CONNECT（RFC 1928/1929，按域名）。错误：(reason, detail)——连不上代理本身 = upstream_down；
/// 协商、认证或 CONNECT 被拒 = auth（旧系统经隔离 xray 链到 SOCKS 时 curl 得到 35）
async fn socks5_connect(addr: std::net::SocketAddr, user: &str, pass: &str, host: &str, port: u16) -> Result<TcpStream, (&'static str, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = TcpStream::connect(addr).await.map_err(|e| (reason::UPSTREAM_DOWN, format!("connect: {e}")))?;
    let _ = s.set_nodelay(true);
    let io = |e: std::io::Error| (reason::AUTH, format!("socks: {e}"));
    let auth = !user.is_empty();
    s.write_all(if auth { &[5, 1, 2] } else { &[5, 1, 0] }).await.map_err(io)?;
    let mut b = [0u8; 2];
    s.read_exact(&mut b).await.map_err(io)?;
    match (b, auth) {
        ([5, 0], _) => {}
        ([5, 2], true) => {
            if user.len() > 255 || pass.len() > 255 {
                return Err((reason::AUTH, "socks: 用户名或密码过长".into()));
            }
            let mut m = vec![1u8, user.len() as u8];
            m.extend_from_slice(user.as_bytes());
            m.push(pass.len() as u8);
            m.extend_from_slice(pass.as_bytes());
            s.write_all(&m).await.map_err(io)?;
            s.read_exact(&mut b).await.map_err(io)?;
            if b[1] != 0 {
                return Err((reason::AUTH, "socks: 用户名/密码被拒".into()));
            }
        }
        _ => return Err((reason::AUTH, format!("socks: 不支持的认证方式 {:?}", b))),
    }
    let mut req = vec![5u8, 1, 0, 3, host.len() as u8];
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await.map_err(io)?;
    let mut h = [0u8; 4];
    s.read_exact(&mut h).await.map_err(io)?;
    if h[1] != 0 {
        return Err((reason::AUTH, format!("socks: CONNECT 被拒（REP={}）", h[1])));
    }
    let rest = match h[3] {
        1 => 4 + 2,
        4 => 16 + 2,
        3 => {
            let mut l = [0u8; 1];
            s.read_exact(&mut l).await.map_err(io)?;
            l[0] as usize + 2
        }
        x => return Err((reason::AUTH, format!("socks: 未知地址类型 {x}"))),
    };
    let mut skip = vec![0u8; rest];
    s.read_exact(&mut skip).await.map_err(io)?;
    Ok(s)
}

struct Fetched {
    ip: Option<IpAddr>,
    status: u16,
    phases: Phases,
    ms: f64,
}

/// 一个来源的一次请求：返回 Ok(Fetched) 或 Err((status, reason, detail, phases))
async fn fetch(via: Via<'_>, src: &Source, limit: Duration) -> Result<Fetched, (Status, &'static str, String, Phases)> {
    let start = Instant::now();
    let mut phases = Phases::default();
    let fut = async {
        let mut t = match via {
            Via::Direct => {
                let addr = tokio::net::lookup_host((src.host, 443u16))
                    .await
                    .map_err(|e| (reason::UPSTREAM_DOWN, format!("dns: {e}")))?
                    .next()
                    .ok_or((reason::UPSTREAM_DOWN, "dns: 无结果".to_string()))?;
                let s = TcpStream::connect(addr).await.map_err(|e| (reason::UPSTREAM_DOWN, format!("connect: {e}")))?;
                let _ = s.set_nodelay(true);
                Transport::Plain(s)
            }
            Via::Ss(cfg) => {
                let s = SsStream::connect(cfg, Target::Domain(src.host.to_string(), 443))
                    .await
                    .map_err(|e| (reason::UPSTREAM_DOWN, format!("connect: {e}")))?;
                Transport::Ss(s)
            }
            Via::Socks(addr, user, pass) => Transport::Plain(socks5_connect(addr, user, pass, src.host, 443).await?),
        };
        let connect_ms = ms(start);
        let resp = http::get(&mut t, src.host, src.path, start).await.map_err(|e| {
            let r = match e.phase {
                Phase::Tls => reason::AUTH,
                Phase::Cert => reason::TLS_INVALID,
                Phase::Http => reason::HTTP_ERROR,
            };
            (r, e.message)
        });
        Ok::<_, (&'static str, String)>((connect_ms, resp))
    };
    match timeout(limit, fut).await {
        Err(_) => Err((Status::Timeout, reason::TIMEOUT, "timeout".into(), phases)),
        Ok(Err((r, d))) => Err((Status::Fail, r, trim(d), phases)),
        Ok(Ok((c, Err((r, d))))) => {
            phases.connect_ms = Some(c);
            Err((Status::Fail, r, trim(d), phases))
        }
        Ok(Ok((c, Ok(resp)))) => {
            phases.connect_ms = Some(c);
            phases.tls_ms = Some(resp.tls_ms);
            phases.first_byte_ms = Some(resp.first_byte_ms);
            Ok(Fetched { ip: extract_ip(src.kind, &resp.body), status: resp.status, phases, ms: legacy_ms(start) })
        }
    }
}

/// L2 单次尝试：经 SS 依次访问 `sources`
pub async fn egress_attempt(node: &NodeTarget, sources: &[Source], limit: Duration) -> ProbeResult {
    let mut r = ProbeResult::new(&node.node, CheckKind::Egress, Status::Fail, 0.0);
    r.expected_ip = Some(node.expected_ip.clone());
    let addr = match node.socket_addr().await {
        Ok(a) => a,
        Err(e) => {
            r.reason_code = Some(reason::UPSTREAM_DOWN.into());
            r.detail = Some(trim(format!("resolve: {e}")));
            r.measured_at = now();
            return finish(r);
        }
    };
    // SOCKS5（NodeMach）不走 SS；其余按 SS 加密方式建立配置
    let cfg = if node.method == "socks5" {
        None
    } else {
        match ServerCfg::new(addr, &node.method, &node.password) {
            Ok(c) => Some(c),
            Err(e) => {
                r.status = Status::Unknown;
                r.reason_code = Some(reason::UNSUPPORTED_CIPHER.into());
                r.detail = Some(trim(e));
                r.measured_at = now();
                return finish(r);
            }
        }
    };
    let mut errors = Vec::new();
    for src in sources {
        let via = match &cfg {
            Some(c) => Via::Ss(c),
            None => Via::Socks(addr, &node.username, &node.password),
        };
        match fetch(via, src, limit).await {
            Ok(f) if f.status == 200 && f.ip.is_some() => {
                let ip = f.ip.unwrap().to_string();
                r.source = Some(src.name.into());
                r.phases = Some(f.phases);
                // 出口检查只看能不能经节点上网；看到的 IP 只作为数据上报，不与期望值比较（2026-09-24 按负责人要求）。
                // 是否“漏到线路机自己的出口”由 relayd 按线路机公网 IP 标记（egress_leak）。
                r.actual_ip = Some(ip);
                r.status = Status::Ok;
                r.value_ms = Some(f.ms);
                r.reason_code = None;
                r.measured_at = now();
                return finish(r);
            }
            Ok(f) => {
                let code = if f.status != 200 { reason::HTTP_ERROR } else { reason::BAD_RESPONSE };
                errors.push(format!("{}:{}", src.name, code));
                r.status = Status::Fail;
                r.reason_code = Some(code.into());
                r.phases = Some(f.phases);
            }
            Err((st, code, detail, phases)) => {
                errors.push(format!("{}:{code}({detail})", src.name));
                r.status = st;
                r.reason_code = Some(code.into());
                r.phases = Some(phases);
                if code == reason::UPSTREAM_DOWN {
                    break; // 连不上落地本身，换来源没有意义
                }
            }
        }
    }
    r.detail = Some(trim(errors.join(", ")));
    r.measured_at = now();
    finish(r)
}

/// 控制路径：线路机不经 SS 直接访问来源
pub async fn control_check(src: &Source, limit: Duration) -> ProbeResult {
    let mut r = ProbeResult::new(CONTROL_NODE, CheckKind::Control, Status::Fail, 0.0);
    r.source = Some(src.name.into());
    match fetch(Via::Direct, src, limit).await {
        Ok(f) if f.status == 200 && f.ip.is_some() => {
            r.status = Status::Ok;
            r.value_ms = Some(f.ms);
            r.actual_ip = f.ip.map(|i| i.to_string());
            r.phases = Some(f.phases);
        }
        Ok(f) => {
            r.reason_code = Some(if f.status != 200 { reason::HTTP_ERROR } else { reason::BAD_RESPONSE }.into());
            r.phases = Some(f.phases);
        }
        Err((st, code, detail, phases)) => {
            r.status = st;
            r.reason_code = Some(code.into());
            r.detail = Some(detail);
            r.phases = Some(phases);
        }
    }
    r.measured_at = now();
    finish(r)
}

/// IPv6 出口：经 SS 访问只有 AAAA 的端点。能连通 → fail(ipv6_egress_present)；
/// 握手阶段被断开或超时 → ok(ipv6_absent)（节点解析不到或连不出 v6）；连不上落地 → unknown。
pub async fn ipv6_check(node: &NodeTarget, limit: Duration) -> ProbeResult {
    let mut r = ProbeResult::new(&node.node, CheckKind::Ipv6, Status::Unknown, 0.0);
    let addr = match node.socket_addr().await {
        Ok(a) => a,
        Err(e) => {
            r.reason_code = Some(reason::UPSTREAM_DOWN.into());
            r.detail = Some(trim(format!("resolve: {e}")));
            r.measured_at = now();
            return finish(r);
        }
    };
    let cfg = if node.method == "socks5" {
        None
    } else {
        match ServerCfg::new(addr, &node.method, &node.password) {
            Ok(c) => Some(c),
            Err(e) => {
                r.reason_code = Some(reason::UNSUPPORTED_CIPHER.into());
                r.detail = Some(trim(e));
                r.measured_at = now();
                return finish(r);
            }
        }
    };
    let via = match &cfg {
        Some(c) => Via::Ss(c),
        None => Via::Socks(addr, &node.username, &node.password),
    };
    match fetch(via, &IPV6_SOURCE, limit).await {
        Ok(f) if f.ip.is_some_and(|i| i.is_ipv6()) => {
            r.status = Status::Fail;
            r.reason_code = Some(reason::IPV6_EGRESS_PRESENT.into());
            r.actual_ip = f.ip.map(|i| i.to_string());
            r.value_ms = Some(f.ms);
        }
        Ok(_) => r.reason_code = Some(reason::BAD_RESPONSE.into()),
        Err((_, code, detail, _)) if code == reason::UPSTREAM_DOWN => {
            r.reason_code = Some(code.into());
            r.detail = Some(detail);
        }
        Err((_, _, detail, _)) => {
            r.status = Status::Ok;
            r.reason_code = Some(reason::IPV6_ABSENT.into());
            r.detail = Some(detail);
        }
    }
    r.measured_at = now();
    finish(r)
}

/// UDP 出口一致性：经 SS UDP relay 向每个公共 STUN 最多发 2 次 Binding
pub async fn udp_check(node: &NodeTarget, wait: Duration) -> ProbeResult {
    let mut r = ProbeResult::new(&node.node, CheckKind::Udp, Status::Unknown, 0.0);
    r.expected_ip = Some(node.expected_ip.clone());
    let cfg = match node.socket_addr().await.map_err(|e| e.to_string()).and_then(|a| ServerCfg::new(a, &node.method, &node.password)) {
        Ok(c) if !c.cipher.is_2022() => c,
        Ok(_) => {
            r.reason_code = Some(reason::UNSUPPORTED_CIPHER.into());
            r.detail = Some("2022 UDP 未实现".into());
            r.measured_at = now();
            return finish(r);
        }
        Err(e) => {
            r.reason_code = Some(reason::UNSUPPORTED_CIPHER.into());
            r.detail = Some(trim(e));
            r.measured_at = now();
            return finish(r);
        }
    };
    let start = Instant::now();
    let mut seen: Vec<(String, String)> = Vec::new();
    for (name, host, port) in STUN_SOURCES {
        for _ in 0..2 {
            let mut txid = [0u8; 12];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut txid).ok();
            let req = stun::binding_request(txid);
            match ss::udp_exchange(&cfg, &Target::Domain(host.to_string(), port), &req, wait).await {
                Ok(Some((_, body))) => {
                    if let Some(a) = stun::parse_response(&body, txid) {
                        seen.push((name.to_string(), a.ip().to_string()));
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    r.detail = Some(trim(e.to_string()));
                    break;
                }
            }
        }
    }
    let mut ips: Vec<&str> = seen.iter().map(|(_, ip)| ip.as_str()).collect();
    ips.sort();
    ips.dedup();
    r.source = Some(seen.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(","));
    if ips.is_empty() {
        r.reason_code = Some(reason::UDP_NO_REPLY.into());
    } else {
        // 同上：映射地址只作为数据（多个来源看到不同地址时用逗号拼接），不判失败
        r.status = Status::Ok;
        r.actual_ip = Some(ips.join(","));
        r.value_ms = Some(legacy_ms(start));
    }
    r.measured_at = now();
    finish(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extracts_ip() {
        assert_eq!(extract_ip(BodyKind::Trace, b"fl=1\nh=www.cloudflare.com\nip=75.37.237.38\nts=1\n").unwrap().to_string(), "75.37.237.38");
        assert_eq!(extract_ip(BodyKind::Plain, b" 1.2.3.4\n").unwrap().to_string(), "1.2.3.4");
        assert!(extract_ip(BodyKind::Plain, b"<html>").is_none());
    }
}
