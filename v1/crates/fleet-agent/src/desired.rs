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
/// 周期对齐检查间隔：磁盘配置 / Xray 运行态与已应用的期望状态是否一致
const REALIGN_EVERY: Duration = Duration::from_secs(600);

/// 记录最近一次应用结果（成功清空、失败留下错误），随 agent_health 上报
fn note_apply(sh: &Shared, rep: &ApplyReport) {
    *sh.health.last_apply_error.lock().unwrap() = if rep.ok { None } else { Some(rep.error.clone().unwrap_or_else(|| "未知错误".into())) };
}

/// 启动对齐：applied.json 说"已应用"，但 xray.json 可能缺失（只写了空骨架）或 Xray 是新起的 ——
/// 把上次成功应用的期望状态幂等地再应用一遍，确保磁盘与运行态都对得上，之后才敢报告这个代数。
async fn startup_align(sh: &Shared, applier: &Applier) {
    let Some(a) = load_applied(&sh.cfg) else { return };
    tracing::info!(generation = a.generation, "启动对齐：幂等应用上次的期望状态");
    let rep = applier.apply(&a.desired).await;
    note_apply(sh, &rep);
    if rep.ok {
        tracing::info!(generation = a.generation, "启动对齐完成");
        sh.applied_gen.store(a.generation, Ordering::Relaxed);
    } else {
        // 没对齐就不声称已应用该代（applied_gen 保持 0）：长轮询会向中控重新要当前期望状态并再应用
        tracing::error!(generation = a.generation, "启动对齐失败，将向中控重新拉取期望状态：{}", rep.error.as_deref().unwrap_or(""));
    }
}

/// 周期对齐：轻量检查（不跑 xray -test），不一致才完整应用一次修复，并记日志。
async fn periodic_align(sh: &Shared, applier: &Applier) {
    let Some(d) = sh.applied.read().unwrap().clone() else { return };
    let Err(why) = applier.check_aligned(&d).await else { return };
    tracing::warn!(generation = d.generation, "对齐检查发现不一致，重新应用：{why}");
    let rep = applier.apply(&d).await;
    note_apply(sh, &rep);
    if rep.ok {
        sh.health.realign_fixes.fetch_add(1, Ordering::Relaxed);
        tracing::info!(generation = d.generation, "已修复到与期望状态一致");
    } else {
        tracing::error!(generation = d.generation, "对齐修复失败：{}", rep.error.as_deref().unwrap_or(""));
    }
}

/// 启动时恢复上次成功应用的期望状态（探测、流量统计先据此知道有哪些入口）。
/// 代数 applied_gen 不在这里设：要等 `startup_align` 把磁盘与运行态对齐之后才报告。
pub fn restore(sh: &Shared) {
    if let Some(a) = load_applied(&sh.cfg) {
        tracing::info!(generation = a.generation, entries = a.desired.entries.len(), "恢复已应用的期望状态");
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
            Outcome::Retry { wait, why, .. } => {
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
    startup_align(&sh, &applier).await;
    let mut last_align = Instant::now();
    loop {
        if *sd.borrow() {
            return;
        }
        if last_align.elapsed() >= REALIGN_EVERY {
            last_align = Instant::now();
            periodic_align(&sh, &applier).await;
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
                note_apply(&sh, &rep);
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
            Outcome::Retry { wait, why, .. } => {
                fails += 1;
                if fails <= 3 || fails.is_multiple_of(10) {
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
