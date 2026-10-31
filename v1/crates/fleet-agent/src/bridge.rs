//! 节点：穿透的 bridge（`fleet-xray-bridge`，节点上主动连线路机入口的那个 Xray）。程序、配置、单元都由中控经 SSH 装
//! （fleet-server 的 jobs/scripts.rs `BRIDGE`），agent 只做一件事：中控在回执里要它重启（`ReportAck::bridge_kick`）时重启一次。
//!
//! 为什么要中控来叫：线路机上这个入口的入站被换掉时（轮换凭据、停用后再启用、改 REALITY 参数），Xray 连带删掉隧道的反向出站，
//! 但 bridge 那条旧连接不会断（对面还在发心跳），bridge 不会重连，隧道一直不通。线路机 agent 发现反向出站缺席 30 秒以上就报给中控，
//! 中控转给这里。同一个值只重启一次（记在 data_dir/bridge_kick.json，agent 重启后不重复）；没装 bridge 的节点什么都不做

use crate::config::Config;
use crate::util::{load_json, save_json};
use crate::xray::ctl;
use std::path::PathBuf;

pub const SERVICE: &str = "fleet-xray-bridge.service";

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
