//! 线路机只留 AnyTLS 时，穿透入口的 Reality 端口只对它的节点开放（nft 表 `inet fleet_reality_guard`）。
//!
//! 只留 AnyTLS（`RelayParams::reality_public` 为 false）时普通入口的 Reality 入站听回环；穿透入口不能：节点上的 bridge 连的就是
//! 入口的公网 Reality 端口（美国节点 → 美国线路机，不经过 GFW），所以它照旧听 0.0.0.0（见 xray/render.rs），由这张表在 input 钩子上
//! 只放行这条入口的节点出站 IP（`DesiredEntry::expect_egress_ip` = 机器的 egress_ip，没填就是 primary_ip；出口动态的节点是中控最近收到它 agent 上报的来源 IP，变了线路机代数 +1、这里随之重写：bridge 从它连过来），
//! 别的来源直接丢：不回 RST、不完成握手（Reality 对探测会完成 TLS 握手，只靠 Xray 的路由规则挡不住探测）。
//!
//! 整表随期望状态重写：同一个 nft 事务里先建再删再按新内容建（原子、幂等，表不在也不报错）；没有要守的端口时只删表。
//! 线路机 Reality 对公网开着（protocols 为 both / reality）时没有要守的端口，不建表。

use fleet_proto::{DesiredState, Outbound};
use std::net::IpAddr;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub const TABLE: &str = "fleet_reality_guard";

/// 要守的 (端口, 放行的来源)：只留 AnyTLS 时启用着的穿透入口的入口端口，指定了 bridge 端口的再加上它（Xray 入站同时听两个，
/// 两个都只放行节点），按端口排序（同样的入口渲染出同样的规则）
pub fn guarded(d: &DesiredState) -> Vec<(u16, IpAddr)> {
    if d.relay.reality_public {
        return vec![];
    }
    let mut v: Vec<(u16, IpAddr)> = d
        .entries
        .iter()
        .filter(|e| e.enabled && matches!(e.outbound, Outbound::Reverse { .. }))
        .flat_map(|e| [Some(e.port), e.extra_bridge_port()].into_iter().flatten().map(|p| (p, e.expect_egress_ip)))
        .collect();
    v.sort();
    v
}

/// `nft -f` 的脚本：先建再删（表不在也不报错），有要守的端口时再整表建一遍。
/// 每个端口两条：节点来的放行，别的丢；IPv4 / IPv6 都覆盖（Xray 听 0.0.0.0 实际是双栈）
pub fn script(g: &[(u16, IpAddr)]) -> String {
    let mut s = format!("table inet {TABLE}\ndelete table inet {TABLE}\n");
    if g.is_empty() {
        return s;
    }
    s.push_str(&format!("table inet {TABLE} {{\n\tchain input {{\n\t\ttype filter hook input priority filter - 10; policy accept;\n"));
    for (port, ip) in g {
        let fam = if ip.is_ipv4() { "ip" } else { "ip6" };
        s.push_str(&format!("\t\ttcp dport {port} {fam} saddr {ip} accept\n\t\ttcp dport {port} drop\n"));
    }
    s.push_str("\t}\n}\n");
    s
}

/// 上一次写成功的脚本：一样就不再跑 nft
#[derive(Default)]
pub struct RealityGuard {
    last: Option<String>,
}

impl RealityGuard {
    /// 按期望状态写表。和上次写成功的一样就不动；`recheck`（周期对齐）时有端口要守就照样重写一遍：
    /// 表可能被 `nft flush ruleset`、nftables 服务重启清掉。没装 nft 而又没有要守的端口时不算错（表也不可能是它建的）
    pub async fn apply(&mut self, d: &DesiredState, recheck: bool) -> Result<(), String> {
        let g = guarded(d);
        let s = script(&g);
        if self.last.as_deref() == Some(s.as_str()) && (g.is_empty() || !recheck) {
            return Ok(());
        }
        match nft(&s).await {
            Ok(()) => {}
            Err(e) if g.is_empty() && e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("穿透入口的 Reality 端口放行规则（nft 表 {TABLE}）写不进：{e}")),
        }
        // 内容变了才记（启动时本来就没有要守的不记）
        if self.last.as_deref() != Some(s.as_str()) && (self.last.is_some() || !g.is_empty()) {
            tracing::info!(ports = ?g.iter().map(|x| x.0).collect::<Vec<_>>(), "已更新穿透入口的 Reality 放行规则");
        }
        self.last = Some(s);
        Ok(())
    }
}

/// `nft -f -`，脚本从标准输入给
async fn nft(script: &str) -> std::io::Result<()> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin 是 piped");
    stdin.write_all(script.as_bytes()).await?;
    drop(stdin);
    let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "nft 超时"))??;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr).trim().chars().take(400).collect::<String>();
        Err(std::io::Error::other(format!("nft 失败（{}）：{msg}", out.status)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xray::render::tests::{desired, entry, reverse};

    fn anytls_only(entries: Vec<fleet_proto::DesiredEntry>) -> DesiredState {
        let mut d = desired(entries);
        d.relay.reality_public = false;
        d
    }

    /// 只留 AnyTLS + 穿透：每个启用的穿透入口一对规则（节点 IP 放行、别的丢），普通入口、停用的穿透入口不在里面
    #[test]
    fn anytls_only_reverse_ports_are_guarded() {
        let mut off = reverse(3, 20003);
        off.enabled = false;
        let mut v6 = reverse(4, 20004);
        v6.expect_egress_ip = "2001:db8::4".parse().unwrap();
        let d = anytls_only(vec![v6, reverse(2, 20002), entry(1, 20001, true), off]);
        let g = guarded(&d);
        assert_eq!(g, vec![(20002, "203.0.113.9".parse().unwrap()), (20004, "2001:db8::4".parse().unwrap())]);
        assert_eq!(
            script(&g),
            "table inet fleet_reality_guard
delete table inet fleet_reality_guard
table inet fleet_reality_guard {
\tchain input {
\t\ttype filter hook input priority filter - 10; policy accept;
\t\ttcp dport 20002 ip saddr 203.0.113.9 accept
\t\ttcp dport 20002 drop
\t\ttcp dport 20004 ip6 saddr 2001:db8::4 accept
\t\ttcp dport 20004 drop
\t}
}
"
        );
    }

    /// 带 bridge 端口的穿透入口：入口端口和 bridge 端口各一对规则，都只放行节点 IP；不带的同上一个用例
    #[test]
    fn bridge_port_is_guarded_too() {
        let mut e = reverse(2, 20002);
        e.bridge_port = Some(8443);
        let mut same = reverse(3, 20003);
        same.bridge_port = Some(20003);
        let mut plain = entry(1, 20001, true);
        plain.bridge_port = Some(8080);
        let g = guarded(&anytls_only(vec![e, same, plain]));
        let ip: IpAddr = "203.0.113.9".parse().unwrap();
        assert_eq!(g, vec![(8443, ip), (20002, ip), (20003, ip)], "普通入口的 bridge 端口不算；与入口端口相同的不重复");
        assert_eq!(
            script(&g[..2]),
            "table inet fleet_reality_guard
delete table inet fleet_reality_guard
table inet fleet_reality_guard {
\tchain input {
\t\ttype filter hook input priority filter - 10; policy accept;
\t\ttcp dport 8443 ip saddr 203.0.113.9 accept
\t\ttcp dport 8443 drop
\t\ttcp dport 20002 ip saddr 203.0.113.9 accept
\t\ttcp dport 20002 drop
\t}
}
"
        );
    }

    /// Reality 对公网开着（both / reality）：有穿透入口也不守；只留 AnyTLS 但没有穿透入口同样没有。脚本只删表
    #[test]
    fn nothing_guarded_when_reality_public_or_no_reverse() {
        assert!(guarded(&desired(vec![reverse(2, 20002)])).is_empty());
        assert!(guarded(&anytls_only(vec![entry(1, 20001, true)])).is_empty());
        assert_eq!(script(&[]), "table inet fleet_reality_guard\ndelete table inet fleet_reality_guard\n");
    }

    /// 切回 both / 入口切回普通 / 入口删掉：下一次写的脚本只删表，规则跟着去掉
    #[tokio::test]
    async fn switching_back_clears_rules() {
        let mut g = RealityGuard { last: Some(script(&guarded(&anytls_only(vec![reverse(2, 20002)])))) };
        // 内容一样、不复查：不跑 nft（测试机上没有 nft 也能过）
        assert!(g.apply(&anytls_only(vec![reverse(2, 20002)]), false).await.is_ok());
        for d in [desired(vec![reverse(2, 20002)]), anytls_only(vec![entry(2, 20002, true)]), anytls_only(vec![])] {
            assert!(guarded(&d).is_empty());
            assert_eq!(script(&guarded(&d)), script(&[]));
        }
        // 没装 nft、又没有要守的：不算错，记下已清
        if Command::new("nft").arg("--version").output().await.is_err() {
            assert!(g.apply(&desired(vec![reverse(2, 20002)]), false).await.is_ok());
            assert_eq!(g.last.as_deref(), Some(script(&[]).as_str()));
            assert!(g.apply(&anytls_only(vec![reverse(2, 20002)]), false).await.unwrap_err().contains(TABLE));
        }
    }
}
