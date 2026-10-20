//! 上报循环：每 interval_s 把缓冲打成一个 Report → 先落盘进队列 → 按序发送，确认后删除。
//!
//! 磁盘 IO（读访问日志、fsync 写队列、删文件）全部放在 `spawn_blocking` 里，不占 async 运行时的工作线程；
//! 队列 / seq / 访问日志尾读器收在一个 `Core` 里由 std Mutex 保护，锁只在阻塞线程或不跨 await 的短临界区里持有。

use crate::config::Role;
use crate::host;
use crate::queue::Queue;
use crate::server::{Outcome, ServerClient};
use crate::state::{access_cursor_path, stats_path, SeqState, Shared};
use crate::util::{backoff, load_json, now_ms, save_json};
use crate::xray::access::{local_utc_offset_s, Cursor, Tailer};
use fleet_proto::{AgentHealth, Report};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

pub const MAX_ACCESS_PER_REPORT: usize = 2000;
const MAX_HOST_PER_REPORT: usize = 360;
const MAX_ENTRIES_PER_REPORT: usize = 3000;
const MAX_PROBES_PER_REPORT: usize = 1000;
const QUEUE_MAX_BYTES: u64 = 64 * 1024 * 1024;
const QUEUE_MAX_AGE_MS: i64 = 6 * 3600 * 1000;
const REVOKED_SLEEP: Duration = Duration::from_secs(600);
const FLUSH_MAX_REPORTS: usize = 200;
const FLUSH_MAX_TIME: Duration = Duration::from_secs(20);
/// 同一个 seq 连续被服务端回 5xx 达到这个次数（或持续这么久）：当作"毒丸"移入 dead/，不再堵住后面的报告
const POISON_MAX_HITS: u32 = 20;
const POISON_MAX_AGE: Duration = Duration::from_secs(30 * 60);
/// 上报连续失败（断网 / 服务端不可达）时，建报间隔至少拉长到这么久：
/// 合并更多样本进同一份报告，少产生队列文件（每份离线报告恢复后都要一次 RTT 重放）
const OFFLINE_BUILD_INTERVAL: Duration = Duration::from_secs(60);

/// 跟踪"最旧的那个 seq 反复 5xx"
#[derive(Debug)]
struct Poison {
    seq: u64,
    hits: u32,
    since: Instant,
}

impl Poison {
    /// 记一次 5xx。返回 true = 该 seq 该移入 dead/ 了
    fn hit(slot: &mut Option<Poison>, seq: u64, now: Instant) -> bool {
        let p = match slot {
            Some(p) if p.seq == seq => p,
            _ => slot.insert(Poison { seq, hits: 0, since: now }),
        };
        p.hits += 1;
        p.hits >= POISON_MAX_HITS || now.duration_since(p.since) >= POISON_MAX_AGE
    }
}

/// 磁盘相关状态：队列、序号、访问日志尾读器
struct Core {
    queue: Queue,
    seq: SeqState,
    tailer: Option<Tailer>,
}

fn lock(core: &Mutex<Core>) -> MutexGuard<'_, Core> {
    core.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Reporter {
    sh: Arc<Shared>,
    core: Arc<Mutex<Core>>,
    client: ServerClient,
    boot_id: String,
    interval_s: u32,
    fails: u32,
    next_send: Instant,
    poison: Option<Poison>,
}

/// 失败期间建报的间隔：正常 interval_s，连续失败时至少 OFFLINE_BUILD_INTERVAL
fn build_interval(interval_s: u32, fails: u32) -> Duration {
    let normal = Duration::from_secs(interval_s as u64);
    if fails > 0 {
        normal.max(OFFLINE_BUILD_INTERVAL)
    } else {
        normal
    }
}

impl Reporter {
    pub fn new(sh: Arc<Shared>, client: ServerClient) -> anyhow::Result<Self> {
        let cfg = sh.cfg.clone();
        let queue = Queue::open(cfg.data_dir.join("queue"), QUEUE_MAX_BYTES, QUEUE_MAX_AGE_MS)?;
        let seq = SeqState::load(crate::state::state_path(&cfg), queue.newest());
        let tailer = (cfg.role == Role::Relay).then(|| Tailer::new(cfg.access_log.clone(), load_json::<Cursor>(&access_cursor_path(&cfg))));
        tracing::info!(pending = queue.len(), seq = seq.seq, "上报器启动");
        let core = Arc::new(Mutex::new(Core { queue, seq, tailer }));
        Ok(Self { sh, core, client, boot_id: host::boot_id(), interval_s: 10, fails: 0, next_send: Instant::now(), poison: None })
    }

    pub async fn run(mut self, mut sd: watch::Receiver<bool>) {
        let mut next_build = Instant::now() + build_interval(self.interval_s, self.fails);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = sd.changed() => {
                    // 退出前把缓冲里的数据落盘，下次启动继续发
                    self.build_and_push(true).await;
                    return;
                }
            }
            let now = Instant::now();
            if now >= next_build {
                next_build = now + build_interval(self.interval_s, self.fails);
                let mut more = self.build_and_push(true).await;
                // 访问日志有积压：追加几个只含访问事件的报告，尽快追上
                for _ in 0..4 {
                    if !more {
                        break;
                    }
                    more = self.build_and_push(false).await;
                }
            }
            if now >= self.next_send && !lock(&self.core).queue.is_empty() {
                self.flush(&mut sd).await;
                // flush 里等 POST 时收到了关停信号：直接落盘缓冲退出，不再等那个请求
                if *sd.borrow() {
                    self.build_and_push(true).await;
                    return;
                }
            }
        }
    }

    /// 在阻塞线程里操作 Core（磁盘 IO）。阻塞任务 panic 时让上报任务也 panic：进程退出由 systemd 重启
    async fn blocking<R: Send + 'static>(&self, f: impl FnOnce(&mut Core) -> R + Send + 'static) -> R {
        let core = self.core.clone();
        tokio::task::spawn_blocking(move || f(&mut lock(&core))).await.expect("上报器的阻塞任务异常终止")
    }

    /// 取走缓冲 → 分配 seq → 落盘入队。返回访问日志是否还有积压。
    async fn build_and_push(&self, with_samples: bool) -> bool {
        let (sh, core, boot_id) = (self.sh.clone(), self.core.clone(), self.boot_id.clone());
        tokio::task::spawn_blocking(move || build_and_push_blocking(&sh, &core, &boot_id, with_samples)).await.expect("上报器的阻塞任务异常终止")
    }

    fn note_failure(&mut self, why: &str) {
        self.fails += 1;
        if self.fails <= 3 || self.fails.is_multiple_of(10) {
            tracing::warn!(fails = self.fails, pending = lock(&self.core).queue.len(), "上报失败：{why}");
        }
    }

    /// 严格有序补传：最旧的先发，确认后再发下一个。
    /// 不并发：服务端按 seq 大小去重（seq <= 已收到的最大值就当重复丢弃），乱序到达会让较小的 seq 被静默丢掉。
    async fn flush(&mut self, sd: &mut watch::Receiver<bool>) {
        let t0 = Instant::now();
        let mut sent = 0;
        loop {
            let (seq, path) = {
                let c = lock(&self.core);
                let Some(seq) = c.queue.oldest() else { break };
                (seq, c.queue.file_path(seq))
            };
            if sent >= FLUSH_MAX_REPORTS || t0.elapsed() > FLUSH_MAX_TIME {
                self.next_send = Instant::now() + Duration::from_secs(1);
                return;
            }
            let read = tokio::task::spawn_blocking(move || std::fs::read(path)).await.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| e.to_string()));
            let body = match read {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(seq, "读取队列文件失败，丢弃：{e}");
                    self.blocking(move |c| c.queue.ack_through(seq)).await;
                    continue;
                }
            };
            let out = tokio::select! {
                o = self.client.post_report(body) => o,
                // 关停：放弃这次请求（报告还在磁盘队列里，下次启动继续发），让上层赶紧落盘缓冲
                _ = sd.changed() => return,
            };
            match out {
                Outcome::Ok(ack) => {
                    self.poison = None;
                    if ack.interval_s > 0 {
                        self.interval_s = ack.interval_s.clamp(2, 300);
                    }
                    let local_seq = lock(&self.core).seq.seq;
                    if ack.accepted_seq > local_seq {
                        // 服务端已确认的序号比本地发出过的最大序号还大（时钟回拨 / 本地状态丢失）：
                        // 把本地推到它之后，否则之后的新报告会被当重复丢弃
                        tracing::warn!(accepted = ack.accepted_seq, local = local_seq, "服务端确认的 seq 大于本地最新 seq，推进本地序号");
                        let n = ack.accepted_seq;
                        if let Err(e) = self.blocking(move |c| c.seq.advance_to(n)).await {
                            tracing::error!("推进本地 seq 失败：{e:#}");
                        }
                    }
                    if ack.accepted_seq >= seq {
                        let through = ack.accepted_seq;
                        let n = self.blocking(move |c| c.queue.ack_through(through)).await;
                        if self.fails > 0 {
                            tracing::info!("上报恢复，已确认 {n} 个报告");
                        }
                        self.fails = 0;
                        sent += 1;
                    } else {
                        self.note_failure(&format!("服务端回 accepted_seq={} < 已发 seq={seq}", ack.accepted_seq));
                        self.next_send = Instant::now() + backoff(self.fails, 5, 60);
                        return;
                    }
                }
                Outcome::Retry { wait, why, server_err } => {
                    if server_err && Poison::hit(&mut self.poison, seq, Instant::now()) {
                        tracing::error!(seq, "同一报告被服务端连续 5xx，疑似该报告本身触发服务端错误，移入 dead/ 继续发后面的：{why}");
                        self.blocking(move |c| c.queue.dead_letter(seq)).await;
                        self.poison = None;
                        self.fails = 0;
                        continue;
                    }
                    if !server_err {
                        self.poison = None;
                    }
                    self.note_failure(&why);
                    self.next_send = Instant::now() + wait.unwrap_or_else(|| backoff(self.fails, 5, 60));
                    return;
                }
                Outcome::Revoked => {
                    self.poison = None;
                    tracing::error!("服务端回 401：令牌已被撤销或无效。停止上报 {} 秒后重试（数据继续在本地排队）", REVOKED_SLEEP.as_secs());
                    self.next_send = Instant::now() + REVOKED_SLEEP;
                    return;
                }
                Outcome::Rejected(why) => {
                    // 永久拒绝：移入 dead/ 留证，继续发后面的，不堵队列
                    tracing::error!(seq, "服务端永久拒绝该报告（已移入 dead/ 目录）：{why}");
                    self.blocking(move |c| c.queue.dead_letter(seq)).await;
                }
            }
        }
    }
}

/// 报告已落盘：这时才前移访问日志游标、持久化流量基线
fn commit(sh: &Shared, tailer: &mut Option<Tailer>, cursor: Option<Cursor>, stats: Option<crate::xray::stats::StatsState>) {
    if let Some(t) = tailer.as_mut() {
        let changed = cursor.is_some() && cursor != t.cursor;
        t.commit(cursor);
        if changed {
            if let Some(c) = t.cursor {
                let _ = save_json(&access_cursor_path(&sh.cfg), &c, false);
            }
        }
    }
    if let Some(s) = stats {
        let _ = save_json(&stats_path(&sh.cfg), &s, false);
    }
}

/// 阻塞线程里的建报：取走缓冲 → 读访问日志 → 分配 seq → 落盘入队。返回访问日志是否还有积压。
fn build_and_push_blocking(sh: &Shared, core: &Mutex<Core>, boot_id: &str, with_samples: bool) -> bool {
    let (mut host, mut entries, mut probes, mut info, mut xray, mut stats_commit) = (vec![], vec![], vec![], None, None, None);
    if with_samples {
        let mut b = sh.buf.lock().unwrap();
        let n = b.host.len().min(MAX_HOST_PER_REPORT);
        host = b.host.drain(..n).collect();
        let n = b.entries.len().min(MAX_ENTRIES_PER_REPORT);
        entries = b.entries.drain(..n).collect();
        let n = b.probes.len().min(MAX_PROBES_PER_REPORT);
        probes = b.probes.drain(..n).collect();
        info = b.info.take();
        xray = b.xray.clone();
        stats_commit = b.stats_commit.take();
    }
    let mut guard = lock(core);
    let Core { queue, seq, tailer } = &mut *guard;
    // 访问日志：队列积压过半时暂停读取（日志文件就是缓冲，不丢行）
    let (mut access, mut next_cursor, mut more) = (vec![], None, false);
    if let Some(t) = tailer.as_mut() {
        if queue.pressure() < 0.5 {
            match t.read_batch(MAX_ACCESS_PER_REPORT, local_utc_offset_s()) {
                Ok(b) => {
                    access = b.events;
                    next_cursor = b.next;
                    more = b.more;
                }
                Err(e) => tracing::warn!("读取访问日志失败：{e:#}"),
            }
        }
    }
    if host.is_empty() && entries.is_empty() && probes.is_empty() && access.is_empty() && info.is_none() && xray.is_none() {
        // 没有内容；但访问日志游标（例如首次定位到文件末尾）仍要提交
        commit(sh, tailer, next_cursor, stats_commit);
        return more;
    }
    let seq = match seq.next() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("分配 seq 失败（磁盘问题？）：{e:#}，数据留在内存缓冲");
            sh.buf.lock().unwrap().restore(host, entries, probes);
            return false;
        }
    };
    let report = Report {
        seq,
        boot_id: boot_id.to_string(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        sent_at_ms: now_ms(),
        info: info.clone(),
        host,
        entries,
        probes,
        access,
        xray,
        agent_health: with_samples.then(|| health_of(sh, queue, tailer.as_ref())),
    };
    match queue.push(&report) {
        Ok(()) => {
            commit(sh, tailer, next_cursor, stats_commit);
            more
        }
        Err(e) => {
            tracing::error!("报告落盘失败：{e:#}，数据留在内存缓冲");
            let Report { host, entries, probes, .. } = report;
            let mut b = sh.buf.lock().unwrap();
            b.restore(host, entries, probes);
            if info.is_some() && b.info.is_none() {
                b.info = info;
            }
            false
        }
    }
}

/// `health` 的拆借用版本：建报时 queue / tailer 已被解构借出，不能再传整个 Core
fn health_of(sh: &Shared, queue: &Queue, tailer: Option<&Tailer>) -> AgentHealth {
    let cnt = tailer.map(|t| t.counters.clone()).unwrap_or_default();
    let h = &sh.health;
    AgentHealth {
        queue_len: queue.len() as u32,
        queue_dropped: queue.dropped,
        queue_dead: queue.dead,
        access_lines: cnt.lines,
        access_events: cnt.events,
        access_unparsed: cnt.unparsed,
        access_dropped_lines: cnt.dropped_lines,
        access_rotation_gaps: cnt.rotation_gaps,
        access_truncations: cnt.truncations,
        probes_skipped: h.probes_skipped.load(Ordering::Relaxed),
        realign_fixes: h.realign_fixes.load(Ordering::Relaxed),
        last_apply_error: h.last_apply_error.lock().unwrap().clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poison_after_n_consecutive_5xx() {
        let mut slot = None;
        let t0 = Instant::now();
        for _ in 1..POISON_MAX_HITS {
            assert!(!Poison::hit(&mut slot, 7, t0));
        }
        assert!(Poison::hit(&mut slot, 7, t0));
    }

    #[test]
    fn poison_counter_resets_on_other_seq() {
        let mut slot = None;
        let t0 = Instant::now();
        for _ in 1..POISON_MAX_HITS {
            assert!(!Poison::hit(&mut slot, 7, t0));
        }
        // 换了个 seq（7 已确认/移走）：从头计
        assert!(!Poison::hit(&mut slot, 8, t0));
        assert_eq!(slot.as_ref().unwrap().hits, 1);
    }

    #[test]
    fn poison_after_age_even_with_few_hits() {
        let mut slot = None;
        let t0 = Instant::now();
        assert!(!Poison::hit(&mut slot, 7, t0));
        assert!(Poison::hit(&mut slot, 7, t0 + POISON_MAX_AGE));
    }

    #[test]
    fn build_interval_stretches_only_when_failing() {
        assert_eq!(build_interval(10, 0), Duration::from_secs(10));
        assert_eq!(build_interval(10, 1), OFFLINE_BUILD_INTERVAL);
        // 服务端要求更长的间隔时尊重它
        assert_eq!(build_interval(120, 3), Duration::from_secs(120));
    }
}
