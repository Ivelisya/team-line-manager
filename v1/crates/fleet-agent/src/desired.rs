//! 线路机：长轮询期望状态 → 应用 → 回报。

use crate::server::{Desired, Outcome, ServerClient, REVOKED_SLEEP};
use crate::singbox::Singbox;
use crate::state::{load_applied, pending_ack_path, save_applied, Shared};
use crate::util::{backoff, load_json, save_json};
use crate::xray::apply::Applier;
use fleet_proto::ApplyReport;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// 周期对齐检查间隔：磁盘配置 / Xray 运行态与已应用的期望状态是否一致
const REALIGN_EVERY: Duration = Duration::from_secs(600);
/// "重启路径"失败后的退避：首次 15 分钟，翻倍，封顶 6 小时（带抖动）
const RESTART_RETRY_BASE_S: u64 = 15 * 60;
const RESTART_RETRY_CAP_S: u64 = 6 * 3600;

/// "需要重启 Xray"的那条路径失败后的重试闸门。
/// 重启路径失败 = 先用新配置重启一次、再回滚用旧配置重启一次，每次都断开全部客户连接；
/// 按普通退避（15 s…300 s）无限重试会把客户连接反复打断，所以这里拉长到小时级。
/// 按渲染出的骨架（`Applier::static_part`，它才决定要不要重启）判断，不按代数：期间收到的新代数只要骨架没变
/// （改了三网 PING、入口等与骨架无关的东西）就仍然拦着，不再绕过闸门重启一次；骨架变了（agent 换了版本等）才放行。热增删路径的失败不受影响。
#[derive(Default)]
struct RestartGate {
    /// 最近一次见到的代数：长轮询带着它，服务端代数再变才返回
    gen: u64,
    skel: Option<serde_json::Value>,
    fails: u32,
    until: Option<Instant>,
}

impl RestartGate {
    /// 记一次重启路径失败，返回下次允许重试前要等多久
    fn record_failure(&mut self, gen: u64, skel: Option<serde_json::Value>) -> Duration {
        if self.skel != skel {
            self.skel = skel;
            self.fails = 0;
        }
        self.gen = gen;
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

    /// 闸门关着，且这份期望状态的骨架与失败的那次相同（渲染失败的不拦：应用时会报渲染错误，不重启）
    fn blocks(&self, skel: Option<&serde_json::Value>) -> bool {
        skel.is_some() && self.skel.as_ref() == skel && self.remaining().is_some()
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
    format!("代数 {gen} 需要重启 Xray，已失败 {fails} 次，暂停重试约 {} 分钟（期间新代数的 Xray 骨架配置若相同也不重试）", wait.as_secs().div_ceil(60))
}

/// 启动对齐：applied.json 说"已应用"，但 xray.json 可能缺失（只写了空骨架）或 Xray 是新起的 ——
/// 把上次成功应用的期望状态幂等地再应用一遍，确保磁盘与运行态都对得上，之后才敢报告这个代数。
async fn startup_align(sh: &Shared, applier: &Applier, sb: &Singbox) {
    let Some(a) = load_applied(&sh.cfg) else { return };
    tracing::info!(generation = a.generation, "启动对齐：幂等应用上次的期望状态");
    let rep = applier.apply(&a.desired).await;
    note_apply(sh, &rep, None);
    if rep.ok {
        sb.apply(&a.desired).await;
        tracing::info!(generation = a.generation, "启动对齐完成");
        sh.applied_gen.store(a.generation, Ordering::Relaxed);
    } else {
        // 没对齐就不声称已应用该代（applied_gen 保持 0）：长轮询会向中控重新要当前期望状态并再应用
        tracing::error!(generation = a.generation, "启动对齐失败，将向中控重新拉取期望状态：{}", rep.error.as_deref().unwrap_or(""));
    }
}

/// 周期对齐：轻量检查（不跑 xray -test），不一致才完整应用一次修复，并记日志。
/// AnyTLS（sing-box）每次都按已应用的那份对一遍：配置一样、单元在跑就什么都不做；它只跟着 Xray 已应用的状态走，不受 Xray 这次修没修好影响
/// `written`：上一次对齐修复失败时写进 last_apply_error 的那条。之后检查通过（比如那次只是 Xray API 偶发查询失败）就把它清掉；
/// 只在 last_apply_error 还是这一条时才清，期间长轮询应用写了别的（真实的应用失败）就留着
async fn periodic_align(sh: &Shared, applier: &Applier, sb: &Singbox, gate: &mut RestartGate, written: &mut Option<String>) {
    let Some(d) = sh.applied.read().unwrap().clone() else { return };
    sb.apply(&d).await;
    let Err(why) = applier.check_aligned(&d).await else {
        if let Some(w) = written.take() {
            let mut cur = sh.health.last_apply_error.lock().unwrap();
            if cur.as_deref() == Some(w.as_str()) {
                *cur = None;
            }
        }
        return;
    };
    // 闸门只为不反复断开客户连接：Xray 确实没在跑（崩了 / 被停了）就没有客户可断，照常应用把它拉起来
    if gate.remaining().is_some() && !applier.is_down().await {
        tracing::warn!(generation = d.generation, "对齐检查发现不一致，但重启路径刚失败过，暂不重试：{why}");
        return;
    }
    tracing::warn!(generation = d.generation, "对齐检查发现不一致，重新应用：{why}");
    let skel = applier.static_part(&d).await;
    let res = applier.apply_detailed(&d).await;
    let rep = res.report;
    if rep.ok {
        gate.clear();
        note_apply(sh, &rep, None);
        *written = None;
        tracing::info!(generation = d.generation, "已修复到与期望状态一致");
    } else {
        let note = res.disruptive_restart.then(|| {
            let wait = gate.record_failure(d.generation, skel);
            paused_note(d.generation, gate.fails, wait)
        });
        note_apply(sh, &rep, note);
        *written = sh.health.last_apply_error.lock().unwrap().clone();
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

/// 回报应用结果；送不到（或重试等待中收到关停信号）就落盘，下个循环重发。
/// 关停一律用 `wait_for(|v| *v)` 等：它看当前值；`changed()` 会把共享的接收端标成已读，之后别处的 select 就醒不来
async fn report_applied(sh: &Shared, client: &ServerClient, rep: &ApplyReport, sd: &mut watch::Receiver<bool>) -> bool {
    for attempt in 0..3u32 {
        let wait = match client.post_applied(rep).await {
            Outcome::Ok(()) => {
                let _ = tokio::fs::remove_file(pending_ack_path(&sh.cfg)).await;
                return true;
            }
            Outcome::Rejected(why) => {
                tracing::error!(generation = rep.generation, "服务端拒绝应用回报：{why}（丢弃）");
                let _ = tokio::fs::remove_file(pending_ack_path(&sh.cfg)).await;
                return true;
            }
            Outcome::Revoked => break,
            Outcome::Busy { wait, why } => {
                tracing::debug!("应用回报：中控正忙，{} 秒后再发：{why}", wait.as_secs());
                wait
            }
            Outcome::Retry { wait, why } => {
                tracing::warn!("应用回报失败（第 {} 次）：{why}", attempt + 1);
                wait.unwrap_or_else(|| backoff(attempt, 2, 20))
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = sd.wait_for(|v| *v) => break,
        }
    }
    let (path, rep) = (pending_ack_path(&sh.cfg), rep.clone());
    let _ = tokio::task::spawn_blocking(move || save_json(&path, &rep, true)).await;
    false
}

pub async fn desired_task(sh: Arc<Shared>, applier: Arc<Applier>, sb: Arc<Singbox>, client: Arc<ServerClient>, mut sd: watch::Receiver<bool>) {
    let mut fails = 0u32;
    let mut gate = RestartGate::default();
    let mut align_err = None;
    // 本版本解析不了的最新代数：已回报"版本过旧"，之后带着它去长轮询，直到中控出了更新的一代。
    // 应用成功就清零：中控代数回退（比它小）后不能再带着它去轮询，否则每次都立刻拿回同一代、无间隔空转
    let mut unreadable = 0u64;
    // 刚升级过（applied.json 是别的 agent 版本写的）：第一次拉取从代数 0 起，向中控要一份完整的期望状态，
    // 拿到新版本才认识的字段；中控的代数没变时只按代数比，会一直沿用旧版本存下的那份（见 AppliedFile::agent_version）
    let mut refetch = load_applied(&sh.cfg).is_some_and(|a| a.written_by_other_version());
    if refetch {
        tracing::info!("期望状态是别的 agent 版本存下的（刚升级过），向中控重新要一份完整的");
    }
    startup_align(&sh, &applier, &sb).await;
    let mut last_align = Instant::now();
    loop {
        if *sd.borrow() {
            return;
        }
        if last_align.elapsed() >= REALIGN_EVERY {
            last_align = Instant::now();
            periodic_align(&sh, &applier, &sb, &mut gate, &mut align_err).await;
        }
        // 先补发上次没送达的回报
        let path = pending_ack_path(&sh.cfg);
        if let Some(p) = tokio::task::spawn_blocking(move || load_json::<ApplyReport>(&path)).await.expect("读回报文件的任务异常终止") {
            report_applied(&sh, &client, &p, &mut sd).await;
        }
        // 重启路径的闸门关着时：带着失败的那一代去长轮询，服务端代数一变（有新配置）就返回；
        // 不变就 30 s 一次的 204，不会重复拿到同一代去重试。解析不了的那一代同理
        let after = if refetch { 0 } else { (if gate.remaining().is_some() { gate.gen } else { sh.applied_gen.load(Ordering::Relaxed) }).max(unreadable) };
        let t0 = Instant::now();
        let res = tokio::select! {
            r = client.get_desired(after) => r,
            _ = sd.wait_for(|v| *v) => return,
        };
        let pause = match res {
            Outcome::Ok(Desired::Unchanged) => {
                refetch = false;
                fails = 0;
                // 防止服务端异常地立刻返回 204 导致空转
                (t0.elapsed() < Duration::from_secs(1)).then_some(Duration::from_secs(2))
            }
            Outcome::Ok(Desired::Unreadable { generation, error }) => {
                // 中控的期望状态用了本版本不认识的东西（新枚举值等）：重拉多少次都一样，回报失败让中控看到，等升级
                tracing::error!(generation, "无法解析期望状态，agent 版本过旧？不再重拉这一代：{error}");
                let rep = ApplyReport { generation, ok: false, error: Some(format!("agent 版本过旧，无法理解新的期望状态：{error}")), entries: vec![] };
                note_apply(&sh, &rep, None);
                report_applied(&sh, &client, &rep, &mut sd).await;
                unreadable = generation;
                refetch = false;
                fails = 0;
                None
            }
            Outcome::Ok(Desired::State(d)) if gate.remaining().is_some() && gate.blocks(applier.static_part(&d).await.as_ref()) && !applier.is_down().await => {
                // 骨架没变的新代数（改了三网 PING、入口等）：闸门期内不重试，重启路径会再断一次客户连接。
                // 记下这一代，长轮询带着它等更新的；解析不了的那一代更大时带的是那一代（中控代数回退后），
                // 中控按「不等于」立刻发回来，所以仍要睡一会。Xray 没在运行就没有客户可断，不拦
                tracing::info!(generation = d.generation, "收到期望状态，但 Xray 骨架配置与重启失败的那次相同，闸门期内暂不应用");
                gate.gen = d.generation;
                Some(Duration::from_secs(30))
            }
            Outcome::Ok(Desired::State(d)) => {
                tracing::info!(generation = d.generation, entries = d.entries.len(), "收到期望状态");
                let res = applier.apply_detailed(&d).await;
                let rep = res.report;
                let mut pause = None;
                if rep.ok {
                    refetch = false;
                    note_apply(&sh, &rep, None);
                    gate.clear();
                    // Xray 那一侧（含 AnyTLS 的桥接入站）已就位，再换 sing-box：它连的桥接端口这时都在。失败只记在 sing-box 的状态里，
                    // 不影响这次的应用回报（Reality 照常可用）
                    sb.apply(&d).await;
                    let generation = d.generation;
                    let d = Arc::new(d);
                    let (cfg, d2) = (sh.cfg.clone(), d.clone());
                    let _ = tokio::task::spawn_blocking(move || save_applied(&cfg, &d2)).await;
                    *sh.applied.write().unwrap() = Some(d);
                    sh.applied_gen.store(generation, Ordering::Relaxed);
                    unreadable = 0;
                    fails = 0;
                } else {
                    tracing::error!(generation = d.generation, "应用失败：{}", rep.error.as_deref().unwrap_or(""));
                    if res.disruptive_restart {
                        // 重启路径失败：拉长重试间隔，错误状态留在 agent_health.last_apply_error 里让中控可见
                        let wait = gate.record_failure(d.generation, applier.static_part(&d).await);
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
            // 中控正忙（429）：不算失败
            Outcome::Busy { wait, .. } => Some(wait),
            Outcome::Retry { wait, why } => {
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
                _ = sd.wait_for(|v| *v) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restart_gate_blocks_new_generation_with_same_skeleton() {
        use crate::xray::render::tests::{desired, entry};
        let applier = Applier::new(Arc::new(crate::config::Config::relay_default()));
        let d7 = desired(vec![entry(1, 20001, true)]);
        // 新代数：加了入口、带了三网 PING，骨架不变
        let mut d8 = desired(vec![entry(1, 20001, true), entry(2, 20002, true)]);
        d8.generation = 8;
        d8.ping = Some(fleet_proto::PingConfig { interval_s: 60, count: 5, mode: "tcp".into(), targets: vec![] });
        let (s7, s8) = (applier.static_part(&d7).await, applier.static_part(&d8).await);
        assert!(s7.is_some() && s7 == s8);

        let mut g = RestartGate::default();
        assert!(!g.blocks(s7.as_ref()));
        let w1 = g.record_failure(7, s7.clone());
        // 首次 15 分钟 ×[0.5,1.5)，远大于普通的 15 s 起步
        assert!(w1 >= Duration::from_secs(7 * 60), "{w1:?}");
        assert!(g.blocks(s8.as_ref()), "骨架相同的新代数要继续拦");
        // 骨架变了（比如 agent 换了版本）放行；渲染失败的也不拦
        assert!(!g.blocks(Some(&serde_json::json!({"log": "other"}))));
        assert!(!g.blocks(None));
        // 连续失败：退避上升但有封顶；骨架不变计数不重来
        for _ in 0..20 {
            let w = g.record_failure(8, s8.clone());
            assert!(w <= Duration::from_secs(RESTART_RETRY_CAP_S * 3 / 2));
        }
        assert!(g.fails >= 21);
        g.record_failure(9, Some(serde_json::json!({"log": "other"})));
        assert_eq!(g.fails, 1);
        g.clear();
        assert!(!g.blocks(s8.as_ref()) && g.remaining().is_none());
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
