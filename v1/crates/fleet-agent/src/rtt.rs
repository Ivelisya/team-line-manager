//! 放置矩阵（仅线路机）：按 `GET /agent/v1/rtt`（中控给的全部在用 VPS 节点，见 fleet-server 的 placement.rs）测这台到每个节点的网络往返，
//! 中控据此算「节点放在哪台线路机最好」。挂在这台上的入口另有 TCP / 出口探测（collect.rs），这里只看路由到节点 IP 的往返。
//!
//! 每 `interval_s`（300）秒一轮，最多 4 个节点同时测：系统 ping 发 3 次（`ping -n -c 3 -i 0.5 -W 2`，同三网 PING 的 icmp），
//! 有回应就通，往返取回应的中位数。ufw 默认放行 ICMP 回显请求。只用 ICMP：
//! - 不连 SS 端口：节点防火墙只放行它所挂的线路机，别的线路机连 SS 端口被挡，量到的是防火墙而不是链路；
//! - 不连 SSH 端口（系统 ping 用不了时也不退回去连）：每 5 分钟几台线路机各握手一次，fail2ban 开了 aggressive / ddos 模式时
//!   会把线路机 IP 封掉，连带断了这台线路机到节点的业务流量。系统 ping 用不了（没有、没权限，见 ping.rs）就不测，日志提示一次。
//!
//! 结果进上报缓冲，随下一份上报带走（`Report::rtt`）；离线时同其它样本一起排队补传。

use crate::ping::{icmp, median};
use crate::server::{Outcome, ServerClient};
use crate::state::Shared;
use crate::util::now_ms;
use fleet_proto::{RttConfig, RttSample, RttTarget};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Semaphore};

/// 同时测几个节点
const PARALLEL: usize = 4;
/// 每轮每个节点发几次
const COUNT: u32 = 3;

pub async fn rtt_task(sh: Arc<Shared>, client: Arc<ServerClient>, mut sd: watch::Receiver<bool>) {
    // 第一轮等 40 秒：网络就绪，也和三网 PING 的第一轮错开
    let mut next = tokio::time::Instant::now() + Duration::from_secs(40);
    // 关停用 wait_for 等（见 desired.rs 的 report_applied）。取清单、测一轮也要听关停：节点多时一轮超过关停宽限（25 秒），
    // 不听的话 agent 停止 / 升级要白等到被强杀；丢掉 round 时 JoinSet 一起中止，系统 ping 是 kill_on_drop
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(next) => {}
            _ = sd.wait_for(|v| *v) => return,
        }
        // 每轮测之前向中控取一次节点清单（不随期望状态下发，见 fleet-proto 的 RttConfig）；取不到（含旧中控没有这个接口）这一轮不测
        let res = tokio::select! {
            r = client.get_rtt() => r,
            _ = sd.wait_for(|v| *v) => return,
        };
        let cfg = match res {
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
        let samples = tokio::select! {
            s = round(&cfg) => s,
            _ = sd.wait_for(|v| *v) => return,
        };
        sh.buf.lock().unwrap().push_rtt(samples);
    }
}

/// 测一轮：最多 PARALLEL 个节点同时测；系统 ping 用不了的节点不出样本
pub async fn round(cfg: &RttConfig) -> Vec<RttSample> {
    let ts = now_ms();
    let gate = Arc::new(Semaphore::new(PARALLEL));
    let mut set = tokio::task::JoinSet::new();
    for t in cfg.targets.clone() {
        let gate = gate.clone();
        set.spawn(async move {
            let _p = gate.acquire_owned().await;
            measure(&t).await.map(|(ok, rtt_ms)| RttSample { node_id: t.node_id, ts_ms: ts, ok, rtt_ms })
        });
    }
    let mut out = Vec::with_capacity(cfg.targets.len());
    while let Some(r) = set.join_next().await {
        if let Ok(Some(s)) = r {
            out.push(s);
        }
    }
    out
}

/// None = 系统 ping 用不了：不测，也不退回连节点的端口（见模块说明）
async fn measure(t: &RttTarget) -> Option<(bool, Option<f32>)> {
    if t.host.parse::<IpAddr>().is_err() {
        return Some((false, None));
    }
    match icmp(&t.host, COUNT).await {
        Ok((rtts, lost)) => Some((lost < COUNT, median(rtts))),
        Err(why) => {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!("放置矩阵：系统 ping 用不了（{why}），不测（之后不再提示）");
            }
            None
        }
    }
}
