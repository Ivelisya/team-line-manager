//! 腾出 AnyTLS（sing-box）要监听的端口。
//!
//! 线路机把本地端口范围放宽到 65535 后，AnyTLS 的公网端口（默认 40000–49999）会被本机的出站连接（Xray 出站、agent 连中控的长连接）当作本地端口占着；
//! 调优档的 ip_local_reserved_ports 只管以后新建的连接，已经建立的不会走。sing-box 一个入站绑不上整个服务就退出，所以启动前把占着这些端口的
//! 非监听套接字用 SOCK_DESTROY（`ss -K`）关掉，被关的连接由各自的程序自己重连。
//!
//! - 需要单元的能力集里有 CAP_NET_ADMIN 和内核的 CONFIG_INET_DIAG_DESTROY；缺了就关不掉，如实记日志，启动照常试；
//! - TIME-WAIT 不算占用（Go 的监听带 SO_REUSEADDR）；端口上有别的程序在监听的，不动它，也不动它的客户连接，让启动照常报错。

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use tokio::process::Command;

/// `ss` 一行里要用的几列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sock {
    pub state: String,
    pub local: String,
    pub peer: String,
    pub port: u16,
}

/// `ss -tnH` 的输出（State Recv-Q Send-Q Local Peer [Process]）→ 套接字。认不出的行跳过
pub fn parse_ss(out: &str) -> Vec<Sock> {
    out.lines()
        .filter_map(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            let (state, local, peer) = (t.first()?, t.get(3)?, t.get(4)?);
            let port = local.rsplit(':').next()?.parse().ok()?;
            Some(Sock { state: state.to_string(), local: local.to_string(), peer: peer.to_string(), port })
        })
        .collect()
}

/// 占着端口、要关的连接：非监听、非 TIME-WAIT，且该端口上没有别的程序在监听
pub fn blockers<'a>(conns: &'a [Sock], listening: &[u16]) -> Vec<&'a Sock> {
    conns.iter().filter(|c| !c.state.eq_ignore_ascii_case("TIME-WAIT") && !c.state.eq_ignore_ascii_case("LISTEN") && !listening.contains(&c.port)).collect()
}

/// `( sport = :40010 or sport = :40011 )`
fn filter(ports: &[u16]) -> String {
    format!("( {} )", ports.iter().map(|p| format!("sport = :{p}")).collect::<Vec<_>>().join(" or "))
}

/// 跑 `ss`，返回 (标准输出, 标准错误)。退出码不看：`-K` 有一条没关成就非零，结果以事后再查为准
async fn ss(args: &[&str]) -> anyhow::Result<(String, String)> {
    use anyhow::Context;
    let mut cmd = Command::new("ss");
    cmd.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("ss 超时"))?
        .context("启动 ss（没装 iproute2？）")?;
    Ok((String::from_utf8_lossy(&out.stdout).to_string(), String::from_utf8_lossy(&out.stderr).trim().to_string()))
}

/// 这些端口里有哪些正在被监听；`ss` 用不了返回 None（调用方当「不知道」处理）
pub async fn listening(ports: &[u16]) -> Option<Vec<u16>> {
    if ports.is_empty() {
        return Some(vec![]);
    }
    let (out, _) = ss(&["-tlnH", &filter(ports)]).await.ok()?;
    Some(parse_ss(&out).into_iter().map(|s| s.port).collect())
}

async fn connected(ports: &[u16]) -> anyhow::Result<Vec<Sock>> {
    Ok(parse_ss(&ss(&["-tnH", "state", "connected", &filter(ports)]).await?.0))
}

/// 关掉占着 `ports` 的连接并记日志。任何一步出错只记警告：关不掉不拦着启动，启动的报错会说明白
pub async fn free_ports(ports: &[u16]) {
    if ports.is_empty() {
        return;
    }
    if let Err(e) = free_inner(ports).await {
        tracing::warn!("检查 AnyTLS 端口有没有被连接占着失败：{e:#}");
    }
}

async fn free_inner(ports: &[u16]) -> anyhow::Result<()> {
    let listen = listening(ports).await.unwrap_or_default();
    let conns = connected(ports).await?;
    let victims = blockers(&conns, &listen);
    if victims.is_empty() {
        return Ok(());
    }
    let vports: BTreeSet<u16> = victims.iter().map(|c| c.port).collect();
    let vports: Vec<u16> = vports.into_iter().collect();
    let (_, err) = ss(&["-K", "-tnH", "state", "connected", &filter(&vports)]).await?;
    let after = connected(&vports).await?;
    let left = blockers(&after, &listen);
    let killed: Vec<&&Sock> = victims.iter().filter(|c| !left.iter().any(|l| l.local == c.local && l.peer == c.peer)).collect();
    let mut per_port: BTreeMap<u16, u32> = BTreeMap::new();
    for c in &killed {
        *per_port.entry(c.port).or_default() += 1;
    }
    if !killed.is_empty() {
        let ports_txt = per_port.iter().map(|(p, n)| format!("{p}×{n}")).collect::<Vec<_>>().join("、");
        tracing::warn!("关掉 {} 条占着 AnyTLS 端口的连接（端口×条数：{ports_txt}；例 {} → {}），它们的主人会自己重连", killed.len(), killed[0].local, killed[0].peer);
    }
    if !left.is_empty() {
        let ports_txt = left.iter().map(|c| c.port.to_string()).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>().join("、");
        tracing::warn!("仍有 {} 条占着 AnyTLS 端口 {ports_txt} 的连接没关掉（agent 单元缺 CAP_NET_ADMIN，或内核没开 CONFIG_INET_DIAG_DESTROY）：{err}", left.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "\
ESTAB      0      0      78.31.249.198:40000   78.31.249.198:443   users:((\"fleet-agent\",pid=812,fd=9))
ESTAB      0      0      [::ffff:10.0.0.5]:40002  [::ffff:1.2.3.4]:443
TIME-WAIT  0      0      10.0.0.5:40010        1.2.3.4:443
SYN-SENT   0      1      10.0.0.5:40011        1.2.3.4:443
LISTEN     0      4096   0.0.0.0:40020         0.0.0.0:*
ESTAB      0      0      10.0.0.5:40020        9.9.9.9:5555
garbage line
";

    #[test]
    fn parses_ss_lines_and_ports() {
        let v = parse_ss(OUT);
        assert_eq!(v.len(), 6);
        assert_eq!((v[0].state.as_str(), v[0].port, v[0].peer.as_str()), ("ESTAB", 40000, "78.31.249.198:443"));
        assert_eq!(v[1].port, 40002);
        assert_eq!(v[4].port, 40020);
    }

    #[test]
    fn blockers_skip_time_wait_and_ports_with_a_listener() {
        let v = parse_ss(OUT);
        let b: Vec<u16> = blockers(&v, &[40020]).iter().map(|c| c.port).collect();
        // TIME-WAIT（40010）不算；40020 有人在监听，它的客户连接和监听本身都不动
        assert_eq!(b, vec![40000, 40002, 40011]);
        // 没有监听者时 40020 上那条已建立的连接要关，监听行本身仍不算
        let b: Vec<u16> = blockers(&v, &[]).iter().map(|c| c.port).collect();
        assert_eq!(b, vec![40000, 40002, 40011, 40020]);
    }

    #[test]
    fn filter_text() {
        assert_eq!(filter(&[40010, 40011]), "( sport = :40010 or sport = :40011 )");
    }
}
