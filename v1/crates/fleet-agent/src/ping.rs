//! 三网 PING（仅线路机）：按期望状态里的 `ping`（中控按全局 + 这台的覆盖合成，见 fleet-server 的 ping.rs）往外测线路质量。
//!
//! 每 `interval_s`（60）秒一轮，各目标并发，每个目标发 `count`（5）次：
//! - tcp（默认）：建连，目标不写端口时连 80（`fleet_proto::PING_TCP_DEFAULT_PORT`）；每次超时 3 秒、间隔 0.2 秒；延迟是内核量的往返时间（同 TCP 探测）。
//! - icmp：跑系统的 `ping -n -c 5 -i 0.5 -W 2`，从输出里取每次的 time=。agent 的 systemd 单元不给 CAP_NET_RAW、还带 NoNewPrivileges，
//!   iputils 的 ping 拿不到原始套接字时自己改用 ICMP 数据报套接字（要 `net.ipv4.ping_group_range` 含 root 组，systemd 默认就是全部组）；
//!   系统没有 ping、或它报没权限 / 建不了套接字时，这一轮退回 tcp（样本的 `mode` 写 tcp，界面看得出来），日志 10 分钟最多一条。
//!
//! 结果（通了的那几次的延迟中位数、丢了几次）进上报缓冲，随下一份上报带走（`Report::ping`）；离线时同其它样本一起排队补传。

use crate::probe::tcp_probe;
use crate::state::Shared;
use crate::util::now_ms;
use fleet_proto::{split_target, PingConfig, PingSample, PingTarget, PING_TCP_DEFAULT_PORT};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// TCP 每次建连的超时、两次之间的间隔
const TCP_TIMEOUT: Duration = Duration::from_secs(3);
const TCP_GAP: Duration = Duration::from_millis(200);

pub async fn ping_task(sh: Arc<Shared>, mut sd: watch::Receiver<bool>) {
    // 第一轮等 20 秒：网络、期望状态就绪
    let mut next = Instant::now() + Duration::from_secs(20);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(next) => {}
            _ = sd.changed() => return,
        }
        let cfg = sh.applied.read().unwrap().as_ref().and_then(|d| d.ping.clone());
        let Some(cfg) = cfg.filter(|c| !c.targets.is_empty()) else {
            next = Instant::now() + Duration::from_secs(60);
            continue;
        };
        next = Instant::now() + Duration::from_secs(u64::from(cfg.interval_s.clamp(10, 3600)));
        let samples = round(&cfg).await;
        sh.buf.lock().unwrap().push_ping(samples);
    }
}

/// 测一轮：各目标并发
pub async fn round(cfg: &PingConfig) -> Vec<PingSample> {
    let ts = now_ms();
    let count = cfg.count.clamp(1, 20);
    let mut set = tokio::task::JoinSet::new();
    for t in cfg.targets.clone() {
        let mode = cfg.mode.clone();
        set.spawn(async move { measure(&t, &mode, count, ts).await });
    }
    let mut out = Vec::with_capacity(cfg.targets.len());
    while let Some(r) = set.join_next().await {
        if let Ok(s) = r {
            out.push(s);
        }
    }
    out.sort_by_key(|s| s.slot);
    out
}

async fn measure(t: &PingTarget, mode: &str, count: u32, ts_ms: i64) -> PingSample {
    let sample = |mode: &str, rtts: Vec<f32>, lost: u32| PingSample {
        slot: t.slot,
        ts_ms,
        target: t.target.clone(),
        mode: mode.into(),
        sent: count as u16,
        lost: lost.min(count) as u16,
        rtt_ms: median(rtts),
    };
    let Some((host, port)) = split_target(&t.target) else { return sample(mode, vec![], count) };
    if mode == "icmp" {
        match icmp(&host, count).await {
            Ok((rtts, lost)) => return sample("icmp", rtts, lost),
            Err(why) => warn_fallback(&why),
        }
    }
    let port = port.unwrap_or(PING_TCP_DEFAULT_PORT);
    let mut rtts = vec![];
    for i in 0..count {
        if i > 0 {
            tokio::time::sleep(TCP_GAP).await;
        }
        let s = tcp_probe(uuid::Uuid::nil(), &host, port, TCP_TIMEOUT, 0).await;
        if s.ok {
            // 内核没量到往返时间（极少）时按通了、不计延迟
            rtts.extend(s.latency_ms);
        } else if i == 0 && s.detail.as_deref() == Some("无法解析主机") {
            return sample("tcp", vec![], count);
        }
    }
    let ok = rtts.len() as u32;
    sample("tcp", rtts, count.saturating_sub(ok))
}

/// ICMP 用不了、退回 TCP 的日志：10 分钟最多一条
fn warn_fallback(why: &str) {
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now = now_ms();
    if now - LAST.load(Ordering::Relaxed) >= 600_000 {
        LAST.store(now, Ordering::Relaxed);
        tracing::warn!("三网 PING：ICMP 用不了（{why}），这一轮退回 TCP");
    }
}

/// 跑系统 ping。Ok((通了的各次延迟, 丢了几次))；Err = ping 用不了（没有这个程序、没权限、建不了套接字），调用方退回 TCP
async fn icmp(host: &str, count: u32) -> Result<(Vec<f32>, u32), String> {
    let mut c = tokio::process::Command::new("ping");
    c.args(["-n", "-c", &count.to_string(), "-i", "0.5", "-W", "2", host]).kill_on_drop(true);
    let wait = Duration::from_secs(u64::from(count) + 5);
    let out = match tokio::time::timeout(wait, c.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("跑不了 ping：{e}")),
        // 超时：按全丢算（ping 自己的 -W 盖住了正常情况）
        Err(_) => return Ok((vec![], count)),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    if ["permission", "not permitted", "socket", "invalid option", "usage"].iter().any(|k| stderr.contains(k)) {
        return Err(stderr.lines().next().unwrap_or("").trim().chars().take(120).collect());
    }
    Ok(parse_ping(&stdout, count))
}

/// 从 ping 的输出里取每次的 `time=` 与 `N packets transmitted, M received`（iputils、busybox 都是这个写法）
pub fn parse_ping(out: &str, count: u32) -> (Vec<f32>, u32) {
    let rtts: Vec<f32> = out
        .lines()
        .filter_map(|l| l.split("time=").nth(1))
        .filter_map(|t| t.trim_start().split(|c: char| !(c.is_ascii_digit() || c == '.')).next()?.parse().ok())
        .collect();
    let received = out
        .lines()
        .find(|l| l.contains("transmitted"))
        .and_then(|l| l.split(',').nth(1))
        .and_then(|p| p.split_whitespace().next()?.parse::<u32>().ok())
        .unwrap_or(rtts.len() as u32);
    (rtts, count.saturating_sub(received.min(count)))
}

fn median(mut v: Vec<f32>) -> Option<f32> {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let m = v.len() / 2;
    Some(if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iputils_and_busybox() {
        let iputils = "PING 1.1.1.1 (1.1.1.1) 56(84) bytes of data.\n64 bytes from 1.1.1.1: icmp_seq=1 ttl=57 time=160 ms\n64 bytes from 1.1.1.1: icmp_seq=3 ttl=57 time=158.4 ms\n\n--- 1.1.1.1 ping statistics ---\n5 packets transmitted, 2 received, 60% packet loss, time 2003ms\nrtt min/avg/max/mdev = 158.4/159.2/160.0/0.8 ms\n";
        assert_eq!(parse_ping(iputils, 5), (vec![160.0, 158.4], 3));
        let busybox = "PING 1.1.1.1 (1.1.1.1): 56 data bytes\n64 bytes from 1.1.1.1: seq=0 ttl=57 time=12.345 ms\n\n--- 1.1.1.1 ping statistics ---\n1 packets transmitted, 1 packets received, 0% packet loss\n";
        assert_eq!(parse_ping(busybox, 1), (vec![12.345], 0));
        assert_eq!(parse_ping("", 5), (vec![], 5));
    }

    #[test]
    fn median_of_successes() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
    }

    /// TCP：连得上的口记延迟，连不上的全丢；没写端口的连 80（这里用写了端口的本机口）
    #[tokio::test]
    async fn tcp_mode_counts_losses() {
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let op = open.local_addr().unwrap().port();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let cfg = PingConfig {
            interval_s: 60,
            count: 3,
            mode: "tcp".into(),
            targets: vec![
                PingTarget { slot: 0, name: "通".into(), target: format!("127.0.0.1:{op}") },
                PingTarget { slot: 2, name: "不通".into(), target: format!("127.0.0.1:{closed}") },
            ],
        };
        let r = round(&cfg).await;
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].slot, r[0].sent, r[0].lost, r[0].mode.as_str()), (0, 3, 0, "tcp"));
        assert!(r[0].rtt_ms.is_some());
        assert_eq!((r[1].slot, r[1].lost, r[1].rtt_ms), (2, 3, None));
    }
}
