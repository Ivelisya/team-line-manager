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
/// "重启路径"失败后的退避：首次 15 分钟，翻倍，封顶 6 小时（带抖动）
const RESTART_RETRY_BASE_S: u64 = 15 * 60;
const RESTART_RETRY_CAP_S: u64 = 6 * 3600;

/// 同一代数里"需要重启 Xray"的那条路径失败后的重试闸门。
/// 重启路径失败 = 先用新配置重启一次、再回滚用旧配置重启一次，每次都断开全部客户连接；
/// 对同一代数按普通退避（15 s…300 s）无限重试会把客户连接反复打断，所以这里拉长到小时级。
/// 期间收到新代数（配置有了变化）会立即应用，热增删路径的失败不受影响。
#[derive(Default)]
struct RestartGate {
    gen: u64,
    fails: u32,
    until: Option<Instant>,
}

impl RestartGate {
    /// 记一次重启路径失败，返回下次允许重试前要等多久
    fn record_failure(&mut self, gen: u64) -> Duration {
        if self.gen != gen {
            self.gen = gen;
            self.fails = 0;
        }
        self.fails += 1;
        let wait = backoff(self.fails - 1, RESTART_RETRY_BASE_S, RESTART_RETRY_CAP_S);
        self.until = Some(Instant::now() + wait);
        wait
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    /// 闸门是否还关着（距离允许重试还有多久）
    fn remaining(&self) -> Option<Duration> {
        self.until.and_then(|u| u.checked_duration_since(Instant::now())).filter(|d| !d.is_zero())
    }

    fn blocks(&self, gen: u64) -> bool {
        self.gen == gen && self.remaining().is_some()
    }
}

/// 记录最近一次应用结果（成功清空、失败留下错误），随 agent_health 上报。
/// `note`：失败时附加在错误后面的说明（例如"已暂停重试"），让中控界面能看到当前状态。
fn note_apply(sh: &Shared, rep: &ApplyReport, note: Option<String>) {
    *sh.health.last_apply_error.lock().unwrap() = if rep.ok {
        None
    } else {
        let e = rep.error.clone().unwrap_or_else(|| "未知错误".into());
        Some(match note {
            Some(n) => format!("{e}；{n}"),
            None => e,
        })
    };
}

fn paused_note(gen: u64, fails: u32, wait: Duration) -> String {
    format!("代数 {gen} 需要重启 Xray，已失败 {fails} 次，暂停重试约 {} 分钟（期间收到新代数会立即应用）", wait.as_secs().div_ceil(60))
}

/// 启动对齐：applied.json 说"已应用"，但 xray.json 可能缺失（只写了空骨架）或 Xray 是新起的 ——
/// 把上次成功应用的期望状态幂等地再应用一遍，确保磁盘与运行态都对得上，之后才敢报告这个代数。
async fn startup_align(sh: &Shared, applier: &Applier) {
    let Some(a) = load_applied(&sh.cfg) else { return };
    tracing::info!(generation = a.generation, "启动对齐：幂等应用上次的期望状态");
    let rep = applier.apply(&a.desired).await;
    note_apply(sh, &rep, None);
    if rep.ok {
        tracing::info!(generation = a.generation, "启动对齐完成");
        sh.applied_gen.store(a.generation, Ordering::Relaxed);
    } else {
        // 没对齐就不声称已应用该代（applied_gen 保持 0）：长轮询会向中控重新要当前期望状态并再应用
        tracing::error!(generation = a.generation, "启动对齐失败，将向中控重新拉取期望状态：{}", rep.error.as_deref().unwrap_or(""));
    }
}

/// 周期对齐：轻量检查（不跑 xray -test），不一致才完整应用一次修复，并记日志。
async fn periodic_align(sh: &Shared, applier: &Applier, gate: &mut RestartGate) {
    let Some(d) = sh.applied.read().unwrap().clone() else { return };
    let Err(why) = applier.check_aligned(&d).await else { return };
    if gate.remaining().is_some() {
        tracing::warn!(generation = d.generation, "对齐检查发现不一致，但重启路径刚失败过，暂不重试：{why}");
        return;
    }
    tracing::warn!(generation = d.generation, "对齐检查发现不一致，重新应用：{why}");
    let res = applier.apply_detailed(&d).await;
    let rep = res.report;
    if rep.ok {
        gate.clear();
        note_apply(sh, &rep, None);
        sh.health.realign_fixes.fetch_add(1, Ordering::Relaxed);
        tracing::info!(generation = d.generation, "已修复到与期望状态一致");
    } else {
        let note = res.disruptive_restart.then(|| {
            let wait = gate.record_failure(d.generation);
            paused_note(d.generation, gate.fails, wait)
        });
        note_apply(sh, &rep, note);
        tracing::error!(generation = d.generation, "对齐修复失败：{}", rep.error.as_deref().unwrap_or(""));
    }
}

/// 启动时恢复上次成功应用的期望状态（探测、流量统计先据此知道有哪些入口）。
/// 代数 applied_gen 不在这里设：要等 `startup_align` 把磁盘与运行态对齐之后才报告。
pub fn restore(sh: &Shared) {
    if let Some(a) = load_applied(&sh.cfg) {
        tracing::info!(generation = a.generation, entries = a.desired.entries.len(), "恢复已应用的期望状态");
        *sh.applied.write().unwrap() = Some(Arc::new(a.desired));
    }
}

/// 回报应用结果；送不到（或重试等待中收到关停信号）就落盘，下个循环重发
async fn report_applied(sh: &Shared, client: &ServerClient, rep: &ApplyReport, sd: &mut watch::Receiver<bool>) -> bool {
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
                tokio::select! {
                    _ = tokio::time::sleep(wait.unwrap_or_else(|| backoff(attempt, 2, 20))) => {}
                    _ = sd.changed() => break,
                }
            }
        }
    }
    let (path, rep) = (pending_ack_path(&sh.cfg), rep.clone());
    let _ = tokio::task::spawn_blocking(move || save_json(&path, &rep, true)).await;
    false
}

pub async fn desired_task(sh: Arc<Shared>, applier: Arc<Applier>, client: Arc<ServerClient>, mut sd: watch::Receiver<bool>) {
    let mut fails = 0u32;
    let mut gate = RestartGate::default();
    startup_align(&sh, &applier).await;
    let mut last_align = Instant::now();
    loop {
        if *sd.borrow() {
            return;
        }
        if last_align.elapsed() >= REALIGN_EVERY {
            last_align = Instant::now();
            periodic_align(&sh, &applier, &mut gate).await;
        }
        // 先补发上次没送达的回报
        if let Some(p) = load_json::<ApplyReport>(&pending_ack_path(&sh.cfg)) {
            report_applied(&sh, &client, &p, &mut sd).await;
        }
        // 重启路径的闸门关着时：带着失败的那一代去长轮询，服务端代数一变（有新配置）就返回；
        // 不变就 30 s 一次的 204，不会重复拿到同一代去重试
        let after = if gate.remaining().is_some() { gate.gen } else { sh.applied_gen.load(Ordering::Relaxed) };
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
            Outcome::Ok(Some(d)) if gate.blocks(d.generation) => {
                // 正常不会走到（上面已带着这一代去轮询）；防御：别在闸门期内重试
                Some(Duration::from_secs(30))
            }
            Outcome::Ok(Some(d)) => {
                tracing::info!(generation = d.generation, entries = d.entries.len(), "收到期望状态");
                let res = applier.apply_detailed(&d).await;
                let rep = res.report;
                let mut pause = None;
                if rep.ok {
                    note_apply(&sh, &rep, None);
                    gate.clear();
                    let generation = d.generation;
                    let d = Arc::new(d);
                    let (cfg, d2) = (sh.cfg.clone(), d.clone());
                    let _ = tokio::task::spawn_blocking(move || save_applied(&cfg, &d2)).await;
                    *sh.applied.write().unwrap() = Some(d);
                    sh.applied_gen.store(generation, Ordering::Relaxed);
                    fails = 0;
                } else {
                    tracing::error!(generation = d.generation, "应用失败：{}", rep.error.as_deref().unwrap_or(""));
                    if res.disruptive_restart {
                        // 重启路径失败：拉长重试间隔，错误状态留在 agent_health.last_apply_error 里让中控可见
                        let wait = gate.record_failure(d.generation);
                        tracing::warn!(generation = d.generation, fails = gate.fails, wait_s = wait.as_secs(), "重启路径失败，暂停重试这一代");
                        note_apply(&sh, &rep, Some(paused_note(d.generation, gate.fails, wait)));
                    } else {
                        note_apply(&sh, &rep, None);
                        fails += 1;
                        // 失败后退避再重试同一代，避免确定性失败时空转
                        pause = Some(backoff(fails, 15, 300));
                    }
                }
                report_applied(&sh, &client, &rep, &mut sd).await;
                pause
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_gate_blocks_same_generation_for_long() {
        let mut g = RestartGate::default();
        assert!(!g.blocks(7));
        let w1 = g.record_failure(7);
        // 首次 15 分钟 ×[0.5,1.5)，远大于普通的 15 s 起步
        assert!(w1 >= Duration::from_secs(7 * 60), "{w1:?}");
        assert!(g.blocks(7));
        // 新代数不受影响
        assert!(!g.blocks(8));
        // 连续失败：退避上升但有封顶
        for _ in 0..20 {
            let w = g.record_failure(7);
            assert!(w <= Duration::from_secs(RESTART_RETRY_CAP_S * 3 / 2));
        }
        assert!(g.fails >= 21);
        // 换代数后计数重来
        g.record_failure(8);
        assert_eq!(g.fails, 1);
        g.clear();
        assert!(!g.blocks(8) && g.remaining().is_none());
    }

    #[test]
    fn note_apply_appends_pause_info() {
        let sh = Shared::new(Arc::new(crate::config::Config::relay_default()));
        let rep = ApplyReport { generation: 3, ok: false, error: Some("重启应用失败：x".into()), entries: vec![] };
        note_apply(&sh, &rep, Some(paused_note(3, 2, Duration::from_secs(1800))));
        let e = sh.health.last_apply_error.lock().unwrap().clone().unwrap();
        assert!(e.starts_with("重启应用失败：x；") && e.contains("暂停重试约 30 分钟"), "{e}");
        let ok = ApplyReport { ok: true, error: None, ..rep };
        note_apply(&sh, &ok, None);
        assert!(sh.health.last_apply_error.lock().unwrap().is_none());
    }
}
