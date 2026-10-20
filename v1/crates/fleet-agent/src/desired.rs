//! 线路机：长轮询期望状态 → 应用 → 回报。

use crate::server::{Outcome, ServerClient};
use crate::state::{load_applied, pending_ack_path, save_applied, Shared};
use crate::util::{backoff, load_json, save_json};
use crate::xray::apply::Applier;
use fleet_proto::ApplyReport;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

const REVOKED_SLEEP: Duration = Duration::from_secs(600);

/// 启动时恢复上次成功应用的期望状态
pub fn restore(sh: &Shared) {
    if let Some(a) = load_applied(&sh.cfg) {
        tracing::info!(generation = a.generation, entries = a.desired.entries.len(), "恢复已应用的期望状态");
        sh.applied_gen.store(a.generation, Ordering::Relaxed);
        *sh.applied.write().unwrap() = Some(a.desired);
    }
}

/// 回报应用结果；送不到就落盘，下个循环重发
async fn report_applied(sh: &Shared, client: &ServerClient, rep: &ApplyReport) -> bool {
    for attempt in 0..3u32 {
        match client.post_applied(rep).await {
            Outcome::Ok(()) => {
                let _ = std::fs::remove_file(pending_ack_path(&sh.cfg));
                return true;
            }
            Outcome::Rejected(why) => {
                tracing::error!(generation = rep.generation, "服务端拒绝应用回报：{why}（丢弃）");
                let _ = std::fs::remove_file(pending_ack_path(&sh.cfg));
                return true;
            }
            Outcome::Revoked => break,
            Outcome::Retry { wait, why } => {
                tracing::warn!("应用回报失败（第 {} 次）：{why}", attempt + 1);
                tokio::time::sleep(wait.unwrap_or_else(|| backoff(attempt, 2, 20))).await;
            }
        }
    }
    let _ = save_json(&pending_ack_path(&sh.cfg), rep, true);
    false
}

pub async fn desired_task(sh: Arc<Shared>, applier: Arc<Applier>, client: ServerClient, mut sd: watch::Receiver<bool>) {
    let mut fails = 0u32;
    loop {
        if *sd.borrow() {
            return;
        }
        // 先补发上次没送达的回报
        if let Some(p) = load_json::<ApplyReport>(&pending_ack_path(&sh.cfg)) {
            report_applied(&sh, &client, &p).await;
        }
        let after = sh.applied_gen.load(Ordering::Relaxed);
        let t0 = Instant::now();
        let res = tokio::select! {
            r = client.get_desired(after) => r,
            _ = sd.changed() => return,
        };
        let pause = match res {
            Outcome::Ok(None) => {
                fails = 0;
                // 防止服务端异常地立刻返回 204 导致空转
                (t0.elapsed() < Duration::from_secs(1)).then_some(Duration::from_secs(2))
            }
            Outcome::Ok(Some(d)) => {
                tracing::info!(generation = d.generation, entries = d.entries.len(), "收到期望状态");
                let rep = applier.apply(&d).await;
                if rep.ok {
                    save_applied(&sh.cfg, &d);
                    *sh.applied.write().unwrap() = Some(d.clone());
                    sh.applied_gen.store(d.generation, Ordering::Relaxed);
                    fails = 0;
                } else {
                    tracing::error!(generation = d.generation, "应用失败：{}", rep.error.as_deref().unwrap_or(""));
                    fails += 1;
                }
                report_applied(&sh, &client, &rep).await;
                // 失败后退避再重试同一代，避免确定性失败时空转
                (!rep.ok).then(|| backoff(fails, 15, 300))
            }
            Outcome::Revoked => {
                tracing::error!("拉取期望状态收到 401：令牌无效。{} 秒后重试", REVOKED_SLEEP.as_secs());
                Some(REVOKED_SLEEP)
            }
            Outcome::Retry { wait, why } => {
                fails += 1;
                if fails <= 3 || fails % 10 == 0 {
                    tracing::warn!(fails, "拉取期望状态失败：{why}");
                }
                Some(wait.unwrap_or_else(|| backoff(fails, 5, 60)))
            }
            Outcome::Rejected(why) => {
                fails += 1;
                tracing::error!("拉取期望状态被拒绝：{why}");
                Some(backoff(fails, 5, 60))
            }
        };
        if let Some(p) = pause {
            tokio::select! {
                _ = tokio::time::sleep(p) => {}
                _ = sd.changed() => return,
            }
        }
    }
}
