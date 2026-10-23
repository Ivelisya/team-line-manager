//! 中控自检：软路由隧道回连的主机是不是中控自己（selfmon.rs 开 `central_tunnel_host`）。
//!
//! 路由上的 fleet-tunnel 每次重连都按主机名重新解析，连 `ssh_host:ssh_port` 上中控的 sshd（routers/scripts.rs）。回连主机是 [routers] ssh_host，
//! 没配时取 public_url 的主机名（config.rs）。public_url 的域名经 CDN / 边缘机（只转发 80/443）时，路由断线后就回连不上中控；
//! 现在连着的隧道是长连接，不受影响，断了才出事。所以中控自己定期查（启动时一次，之后每 10 分钟；没有软路由时不查）：
//! - 解析回连主机（写的是 IP 就用它）；解析不了、解析不出地址都算不对；
//! - 解析到的每个地址：是中控本机网卡上的地址（能在它上面 bind）就对；不是（中控在 NAT 后，网卡上是内网地址）就 keyscan 它的 ssh_port，
//!   SSH 主机公钥是中控 sshd 的（[routers] sshd_host_key）才算对。
//!
//! 结论（不对时的告警摘要）放在 `AppState::tunnel_host`，告警循环里 selfmon::check_once 照它开、关告警。解析与比较经 `Net`，测试注入。

use crate::state::AppState;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;

const EVERY: Duration = Duration::from_secs(600);

/// 解析与比较：真的走系统（`Sys`），测试换成假的
pub(crate) trait Net {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>>;
    /// 中控本机网卡上的地址
    fn is_local(&self, ip: IpAddr) -> bool;
    /// `ip:port` 上 sshd 的 ed25519 主机公钥（「ssh-ed25519 base64」）；连不上、没有 ed25519 为 None
    async fn host_key(&self, ip: IpAddr, port: u16) -> anyhow::Result<Option<String>>;
}

struct Sys;

impl Net for Sys {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
        let mut v: Vec<IpAddr> = tokio::net::lookup_host((host, port)).await?.map(|a| a.ip()).collect();
        v.dedup();
        Ok(v)
    }

    fn is_local(&self, ip: IpAddr) -> bool { UdpSocket::bind(SocketAddr::new(ip, 0)).is_ok() }

    async fn host_key(&self, ip: IpAddr, port: u16) -> anyhow::Result<Option<String>> { super::tunnel::keyscan(&ip.to_string(), port).await }
}

pub async fn run_forever(st: AppState) {
    let mut t = tokio::time::interval(EVERY);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        t.tick().await;
        match check(&st).await {
            Ok(found) => *st.tunnel_host.lock().unwrap_or_else(|e| e.into_inner()) = found,
            Err(e) => tracing::warn!("检查软路由隧道回连主机失败：{e:#}"),
        }
    }
}

/// 查一次：不对时返回告警摘要；没有软路由时不查（None）
pub async fn check(st: &AppState) -> anyhow::Result<Option<String>> {
    let any: bool = sqlx::query_scalar("select exists (select 1 from router)").fetch_one(&st.db).await?;
    if !any {
        return Ok(None);
    }
    let r = &st.cfg.routers;
    let host = r.ssh_host_of(&st.cfg.public_url)?;
    let own = tokio::fs::read_to_string(&r.sshd_host_key).await.ok().and_then(|s| super::key_of(&s));
    Ok(judge(&Sys, &host, r.ssh_port, own.as_deref(), r.ssh_host.is_some()).await)
}

const FIX: &str = "路由断线重连时会连不上中控（现在连着的隧道断开之后才受影响）。把 [routers] ssh_host 配成中控源站的 IP，\
                   或一个直指源站、不经 CDN 的域名（灰云），重启中控后给各路由重新生成接入命令或重新部署";

/// `host:port` 是不是中控：不是时返回告警摘要。`own`：中控 sshd 的主机公钥（读不了为 None）；`configured`：配了 [routers] ssh_host
pub(crate) async fn judge(net: &impl Net, host: &str, port: u16, own: Option<&str>, configured: bool) -> Option<String> {
    let from = if configured { "[routers] ssh_host" } else { "没配 [routers] ssh_host，用的是 public_url 的主机名" };
    let addrs = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => match net.resolve(host, port).await {
            Ok(a) if !a.is_empty() => a,
            Ok(_) => return Some(format!("软路由隧道回连主机 {host}（{from}）解析不出地址：{FIX}")),
            Err(e) => return Some(format!("软路由隧道回连主机 {host}（{from}）解析不了（{e}）：{FIX}")),
        },
    };
    let mut bad = vec![];
    for ip in &addrs {
        if net.is_local(*ip) {
            continue;
        }
        let why = match own {
            None => "不是中控本机网卡上的地址，读不了中控 sshd 的主机公钥，没法核对".to_string(),
            Some(own) => match net.host_key(*ip, port).await {
                Ok(Some(k)) if k == own => continue,
                Ok(Some(_)) => format!("{port} 口的 SSH 主机公钥不是中控 sshd 的（多半是 CDN / 边缘机）"),
                Ok(None) => format!("{port} 口连不上 SSH"),
                Err(e) => format!("核对 {port} 口的 SSH 主机公钥失败：{e:#}"),
            },
        };
        bad.push(format!("{ip} {why}"));
    }
    let all: Vec<String> = addrs.iter().map(IpAddr::to_string).collect();
    (!bad.is_empty()).then(|| format!("软路由隧道回连 {host}:{port}（{from}）解析到 {}，不是中控：{}。{FIX}", all.join("、"), bad.join("；")))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        dns: Result<Vec<IpAddr>, String>,
        local: Vec<IpAddr>,
        keys: Vec<(IpAddr, &'static str)>,
    }

    impl Net for Fake {
        async fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<IpAddr>> { self.dns.clone().map_err(std::io::Error::other) }
        fn is_local(&self, ip: IpAddr) -> bool { self.local.contains(&ip) }
        async fn host_key(&self, ip: IpAddr, _: u16) -> anyhow::Result<Option<String>> { Ok(self.keys.iter().find(|k| k.0 == ip).map(|k| k.1.to_string())) }
    }

    const OWN: &str = "ssh-ed25519 OWN";

    fn ip(s: &str) -> IpAddr { s.parse().unwrap() }

    #[tokio::test]
    async fn local_or_same_host_key_is_central() {
        // 解析到本机网卡上的地址
        let f = Fake { dns: Ok(vec![ip("198.51.100.10")]), local: vec![ip("198.51.100.10")], keys: vec![] };
        assert_eq!(judge(&f, "fleet.example.com", 22, Some(OWN), false).await, None);
        // 中控在 NAT 后：网卡上是内网地址，公网地址上的 sshd 主机公钥是中控的
        let f = Fake { dns: Ok(vec![ip("198.51.100.10")]), local: vec![ip("10.0.0.5")], keys: vec![(ip("198.51.100.10"), OWN)] };
        assert_eq!(judge(&f, "ssh.example.com", 22, Some(OWN), true).await, None);
        // 写的是 IP：不解析
        let f = Fake { dns: Err("不该解析".into()), local: vec![ip("198.51.100.10")], keys: vec![] };
        assert_eq!(judge(&f, "198.51.100.10", 22, Some(OWN), true).await, None);
    }

    #[tokio::test]
    async fn edge_or_unresolvable_is_reported() {
        // 域名经边缘机：一个地址的 22 口是别人的 sshd，另一个连不上
        let f = Fake { dns: Ok(vec![ip("203.0.113.7"), ip("203.0.113.8")]), local: vec![ip("10.0.0.5")], keys: vec![(ip("203.0.113.7"), "ssh-ed25519 EDGE")] };
        let s = judge(&f, "fleet.example.com", 22, Some(OWN), false).await.unwrap();
        assert!(s.starts_with("软路由隧道回连 fleet.example.com:22（没配 [routers] ssh_host，用的是 public_url 的主机名）解析到 203.0.113.7、203.0.113.8，不是中控："), "{s}");
        assert!(s.contains("203.0.113.7 22 口的 SSH 主机公钥不是中控 sshd 的（多半是 CDN / 边缘机）；203.0.113.8 22 口连不上 SSH"), "{s}");
        assert!(s.contains("把 [routers] ssh_host 配成中控源站的 IP") && s.contains("现在连着的隧道断开之后才受影响"), "{s}");
        // 读不了中控自己的主机公钥：没法核对，照报
        let f = Fake { dns: Ok(vec![ip("203.0.113.7")]), local: vec![], keys: vec![(ip("203.0.113.7"), OWN)] };
        assert!(judge(&f, "fleet.example.com", 22, None, true).await.unwrap().contains("没法核对"));
        let f = Fake { dns: Err("no such host".into()), local: vec![], keys: vec![] };
        assert!(judge(&f, "fleet.example.com", 22, Some(OWN), true).await.unwrap().starts_with("软路由隧道回连主机 fleet.example.com（[routers] ssh_host）解析不了（no such host）"));
    }

    #[test]
    fn loopback_is_local() {
        assert!(Sys.is_local(ip("127.0.0.1")));
        assert!(!Sys.is_local(ip("192.0.2.1")), "TEST-NET 地址不在本机");
    }
}
