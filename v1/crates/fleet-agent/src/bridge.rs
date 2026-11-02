//! 节点：穿透的 bridge（`fleet-xray-bridge`，节点上主动连线路机入口的那个 Xray）。程序、配置、单元都由中控经 SSH 装
//! （fleet-server 的 jobs/scripts.rs `BRIDGE`），agent 只做两件事。一：中控在回执里要它重启（`ReportAck::bridge_kick`）时重启一次。
//!
//! 为什么要中控来叫：线路机上这个入口的入站被换掉时（轮换凭据、停用后再启用、改 REALITY 参数），Xray 连带删掉隧道的反向出站，
//! 但 bridge 那条旧连接不会断（对面还在发心跳），bridge 不会重连，隧道一直不通。线路机 agent 发现反向出站缺席 30 秒以上就报给中控，
//! 中控转给这里。同一个值只重启一次（记在 data_dir/bridge_kick.json，agent 重启后不重复）；没装 bridge 的节点什么都不做
//!
//! 第二件事：量节点 → 线路机的 TCP 建连（`tunnel_probe_task`）。穿透入口线路机不连节点，没有线路机 → 节点的 TCP 探测；
//! 这里反过来由节点量 bridge 实际连的那个「线路机 IP:入口端口」，样本照常叫 `ProbeKind::Tcp`、按入口上报，中控入同一张表，
//! 曲线、近 1 小时中位数、心跳格、可用率报表都跟着有数据（方向看入口当前的模式：穿透 = 节点 → 线路机）。
//! 目标直接读 bridge 的配置（中控经 SSH 写的 /etc/fleet-bridge/xray.json，每条隧道一个 `t-<入口>` 出站），不另设下发通道

use crate::config::Config;
use crate::probe;
use crate::state::Shared;
use crate::util::{load_json, now_ms, save_json};
use crate::xray::ctl;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::{interval_at, Instant, MissedTickBehavior};
use uuid::Uuid;

pub const SERVICE: &str = "fleet-xray-bridge.service";
/// 中控写的 bridge 配置（jobs/scripts.rs `BRIDGE`）
const CONFIG: &str = "/etc/fleet-bridge/xray.json";
/// 隧道出站的标签前缀（fleet-server reverse::render：`t-<入口 id>`，simple 格式）
const TUNNEL_TAG: &str = "t-";
/// 探测周期：同线路机 agent 的 TCP 探测（期望状态 `relay.probe_interval_s`，目前固定 30 秒，fleet-server entries.rs `PROBE_INTERVAL_S`）
const PROBE_EVERY: Duration = Duration::from_secs(30);

pub struct Kicker {
    path: PathBuf,
    last: Option<i64>,
}

impl Kicker {
    pub fn new(cfg: &Config) -> Self {
        let path = cfg.data_dir.join("bridge_kick.json");
        let last = load_json(&path);
        Self { path, last }
    }

    /// 回执里的 `bridge_kick`：没见过的值才重启（先记下再重启：重启失败也不在每份回执里反复重来，中控过几分钟会给新值）
    pub fn on_ack(&mut self, kick: Option<i64>) {
        let Some(k) = kick.filter(|k| Some(*k) != self.last) else { return };
        self.last = Some(k);
        if let Err(e) = save_json(&self.path, &k, false) {
            tracing::warn!("记下 bridge 重启请求失败（之后可能重复重启一次）：{e:#}");
        }
        tokio::spawn(async move {
            // 没装（单元不存在 / 没启用）就不动：入口切回普通模式后中控已经把它停掉删掉
            if ctl::systemctl(&["is-enabled", "--quiet", SERVICE]).await.is_err() {
                tracing::info!("中控要重启 bridge，但这台没装 {SERVICE}，忽略");
                return;
            }
            match ctl::systemctl(&["restart", SERVICE]).await {
                Ok(_) => tracing::warn!("中控发现穿透隧道没挂上，已重启 {SERVICE}"),
                Err(e) => tracing::error!("重启 {SERVICE} 失败：{e:#}"),
            }
        });
    }
}

/// bridge 配置里的隧道：(入口, 线路机地址, 入口端口)。读不到 / 不是 JSON / 没有隧道都是空
fn tunnels(path: &Path) -> Vec<(Uuid, String, u16)> {
    let Some(v) = std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return vec![] };
    tunnels_in(&v)
}

fn tunnels_in(cfg: &Value) -> Vec<(Uuid, String, u16)> {
    cfg["outbounds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let id = Uuid::parse_str(o["tag"].as_str()?.strip_prefix(TUNNEL_TAG)?).ok()?;
            let s = &o["settings"];
            Some((id, s["address"].as_str()?.to_string(), u16::try_from(s["port"].as_u64()?).ok()?))
        })
        .collect()
}

/// 节点 → 线路机的 TCP 探测：每 `PROBE_EVERY` 读一次 bridge 配置，对每条隧道量一次建连（`probe::tcp_probe`，同线路机侧：内核往返时间、SYN 重发次数），
/// 各入口按 id 错开在周期里。没装 bridge 的节点（普通模式）读不到配置，什么都不做。
/// 停用 / 待应用 / 已切回普通的入口配置里可能还留着，样本由中控入库时按入口当前状态筛掉（agent_api.rs）
pub async fn tunnel_probe_task(sh: Arc<Shared>, mut sd: watch::Receiver<bool>) {
    let mut tick = interval_at(Instant::now() + Duration::from_secs(5), PROBE_EVERY);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = sd.changed() => return,
        }
        for (id, host, port) in tunnels(Path::new(CONFIG)) {
            let sh = sh.clone();
            tokio::spawn(async move {
                tokio::time::sleep(crate::collect::stagger(id, PROBE_EVERY / 2)).await;
                let r = probe::tcp_probe(id, &host, port, probe::TCP_TIMEOUT, now_ms()).await;
                sh.buf.lock().unwrap().push_probe(r);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_tunnel_targets_from_bridge_config() {
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let cfg = json!({"outbounds": [
            {"tag": "blocked", "protocol": "blackhole"},
            {"tag": "direct", "protocol": "freedom"},
            {"tag": format!("t-{}", a.simple()), "protocol": "vless", "settings": {"address": "198.51.100.2", "port": 20001, "id": "x"}},
            {"tag": format!("t-{}", b.simple()), "protocol": "vless", "settings": {"address": "198.51.100.3", "port": 20002}},
            {"tag": "t-not-a-uuid", "settings": {"address": "1.1.1.1", "port": 1}},
            {"tag": format!("t-{}", Uuid::from_u128(3).simple()), "settings": {"address": "1.1.1.1", "port": 70000}},
        ]});
        assert_eq!(tunnels_in(&cfg), vec![(a, "198.51.100.2".to_string(), 20001), (b, "198.51.100.3".to_string(), 20002)]);
        assert!(tunnels_in(&json!({})).is_empty());
        assert!(tunnels(Path::new("/nonexistent/fleet-bridge.json")).is_empty());
    }
}
