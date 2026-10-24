//! 放置矩阵（仅线路机）：按 `GET /agent/v1/rtt`（中控给的全部在用 VPS 节点，见 fleet-server 的 placement.rs）测这台到每个节点的网络往返，
//! 中控据此算「节点放在哪台线路机最好」。挂在这台上的入口另有 TCP / 出口探测（collect.rs），这里只看路由到节点 IP 的往返。
//!
//! 每 `interval_s`（300）秒一轮，最多 4 个节点同时测，每个 3 秒内有回应算通：
//! - ICMP（默认）：系统 ping 发 3 次（`ping -n -c 3 -i 0.5 -W 2`，同三网 PING 的 icmp），有回应就通，往返取回应的中位数。
//!   不连 SS 端口：节点防火墙只放行它所挂的线路机，别的线路机连 SS 端口被挡，量到的是防火墙而不是链路；
//!   也不连 SSH 端口：每 5 分钟几台线路机各握手一次，节点的 auth.log 一天多出近千行，fail2ban 开了 aggressive / ddos 模式时还会把线路机封掉。
//!   ufw 默认放行 ICMP 回显请求。
//! - 系统 ping 用不了（没有、没权限，见 ping.rs）时退回 TCP 建连节点的 SSH 端口：连上或被拒（RST 回来说明路由可达）都算通，只有超时、不可达算不通。
//!
//! 结果进上报缓冲，随下一份上报带走（`Report::rtt`）；离线时同其它样本一起排队补传。

use crate::ping::{icmp, median};
use crate::probe::{connect_latency, tcp_info};
use crate::server::{Outcome, ServerClient};
use crate::state::Shared;
use crate::util::now_ms;
use fleet_proto::{RttConfig, RttSample, RttTarget};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::{watch, Semaphore};

/// 同时测几个节点、每个节点的超时
const PARALLEL: usize = 4;
const TIMEOUT: Duration = Duration::from_secs(3);
/// ICMP 每轮每个节点发几次
const COUNT: u32 = 3;

pub async fn rtt_task(sh: Arc<Shared>, client: Arc<ServerClient>, mut sd: watch::Receiver<bool>) {
    // 第一轮等 40 秒：网络就绪，也和三网 PING 的第一轮错开
    let mut next = tokio::time::Instant::now() + Duration::from_secs(40);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(next) => {}
            _ = sd.changed() => return,
        }
        // 每轮测之前向中控取一次节点清单（不随期望状态下发，见 fleet-proto 的 RttConfig）；取不到（含旧中控没有这个接口）这一轮不测
        let cfg = match client.get_rtt().await {
            Outcome::Ok(c) => c,
            o => {
                tracing::debug!("放置矩阵：取节点清单失败，1 分钟后再取：{o:?}");
                next = tokio::time::Instant::now() + Duration::from_secs(60);
                continue;
            }
        };
        next = tokio::time::Instant::now() + Duration::from_secs(u64::from(cfg.interval_s.clamp(60, 3600)));
        if cfg.targets.is_empty() {
            continue;
        }
        let samples = round(&cfg).await;
        sh.buf.lock().unwrap().push_rtt(samples);
    }
}

/// 测一轮：最多 PARALLEL 个节点同时测
pub async fn round(cfg: &RttConfig) -> Vec<RttSample> {
    let ts = now_ms();
    let gate = Arc::new(Semaphore::new(PARALLEL));
    let mut set = tokio::task::JoinSet::new();
    for t in cfg.targets.clone() {
        let gate = gate.clone();
        set.spawn(async move {
            let _p = gate.acquire_owned().await;
            let (ok, rtt_ms) = measure(&t).await;
            RttSample { node_id: t.node_id, ts_ms: ts, ok, rtt_ms }
        });
    }
    let mut out = Vec::with_capacity(cfg.targets.len());
    while let Some(r) = set.join_next().await {
        if let Ok(s) = r {
            out.push(s);
        }
    }
    out
}

async fn measure(t: &RttTarget) -> (bool, Option<f32>) {
    let Ok(ip) = t.host.parse::<IpAddr>() else { return (false, None) };
    match icmp(&t.host, COUNT).await {
        Ok((rtts, lost)) => (lost < COUNT, median(rtts)),
        Err(why) => {
            warn_fallback(&why);
            tcp(SocketAddr::new(ip, t.port)).await
        }
    }
}

/// TCP 建连：连上用内核量的往返时间；被拒（RST）用墙钟耗时——拒绝只要一个往返，超过 1 秒多半是 SYN 重发过，不给延迟
async fn tcp(a: SocketAddr) -> (bool, Option<f32>) {
    let t = Instant::now();
    let elapsed = || (t.elapsed().as_secs_f64() * 1000.0) as f32;
    match tokio::time::timeout(TIMEOUT, TcpStream::connect(a)).await {
        Ok(Ok(s)) => (true, connect_latency(elapsed(), tcp_info(&s))),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            let ms = elapsed();
            (true, (ms < 1000.0).then_some(ms))
        }
        _ => (false, None),
    }
}

/// ICMP 用不了、退回 TCP 的日志：10 分钟最多一条
fn warn_fallback(why: &str) {
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now = now_ms();
    if now - LAST.load(Ordering::Relaxed) >= 600_000 {
        LAST.store(now, Ordering::Relaxed);
        tracing::warn!("放置矩阵：ICMP 用不了（{why}），这一轮退回 TCP 建连节点的 SSH 端口");
    }
}
