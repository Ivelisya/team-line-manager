//! 上报循环：每 interval_s 把缓冲打成一个 Report → 先落盘进队列 → 按序发送，确认后删除。

use crate::config::Role;
use crate::host;
use crate::queue::Queue;
use crate::server::{Outcome, ServerClient};
use crate::state::{access_cursor_path, stats_path, SeqState, Shared};
use crate::util::{backoff, load_json, now_ms, save_json};
use crate::xray::access::{local_utc_offset_s, Cursor, Tailer};
use fleet_proto::{AgentHealth, Report};
use std::sync::atomic::Ordering;
use std::sync::Arc;
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

pub struct Reporter {
    sh: Arc<Shared>,
    queue: Queue,
    client: ServerClient,
    seq: SeqState,
    tailer: Option<Tailer>,
    boot_id: String,
    interval_s: u32,
    fails: u32,
    next_send: Instant,
    poison: Option<Poison>,
}

impl Reporter {
    pub fn new(sh: Arc<Shared>, client: ServerClient) -> anyhow::Result<Self> {
        let cfg = sh.cfg.clone();
        let queue = Queue::open(cfg.data_dir.join("queue"), QUEUE_MAX_BYTES, QUEUE_MAX_AGE_MS)?;
        let seq = SeqState::load(crate::state::state_path(&cfg), queue.newest());
        let tailer = (cfg.role == Role::Relay).then(|| Tailer::new(cfg.access_log.clone(), load_json::<Cursor>(&access_cursor_path(&cfg))));
        tracing::info!(pending = queue.len(), seq = seq.seq, "上报器启动");
        Ok(Self { sh, queue, client, seq, tailer, boot_id: host::boot_id(), interval_s: 10, fails: 0, next_send: Instant::now(), poison: None })
    }

    pub async fn run(mut self, mut sd: watch::Receiver<bool>) {
        let mut next_build = Instant::now() + Duration::from_secs(self.interval_s as u64);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = sd.changed() => {
                    // 退出前把缓冲里的数据落盘，下次启动继续发
                    self.build_and_push(true);
                    return;
                }
            }
            let now = Instant::now();
            if now >= next_build {
                next_build = now + Duration::from_secs(self.interval_s as u64);
                let mut more = self.build_and_push(true);
                // 访问日志有积压：追加几个只含访问事件的报告，尽快追上
                for _ in 0..4 {
                    if !more {
                        break;
                    }
                    more = self.build_and_push(false);
                }
            }
            if now >= self.next_send && !self.queue.is_empty() {
                self.flush(&mut sd).await;
                // flush 里等 POST 时收到了关停信号：直接落盘缓冲退出，不再等那个请求
                if *sd.borrow() {
                    self.build_and_push(true);
                    return;
                }
            }
        }
    }

    fn health(&self) -> AgentHealth {
        let c = self.tailer.as_ref().map(|t| t.counters.clone()).unwrap_or_default();
        let h = &self.sh.health;
        AgentHealth {
            queue_len: self.queue.len() as u32,
            queue_dropped: self.queue.dropped,
            queue_dead: self.queue.dead,
            access_lines: c.lines,
            access_events: c.events,
            access_unparsed: c.unparsed,
            access_dropped_lines: c.dropped_lines,
            access_rotation_gaps: c.rotation_gaps,
            access_truncations: c.truncations,
            probes_skipped: h.probes_skipped.load(Ordering::Relaxed),
            realign_fixes: h.realign_fixes.load(Ordering::Relaxed),
            last_apply_error: h.last_apply_error.lock().unwrap().clone(),
        }
    }

    /// 取走缓冲 → 分配 seq → 落盘入队。返回访问日志是否还有积压。
    fn build_and_push(&mut self, with_samples: bool) -> bool {
        let (mut host, mut entries, mut probes, mut info, mut xray, mut stats_commit) = (vec![], vec![], vec![], None, None, None);
        if with_samples {
            let mut b = self.sh.buf.lock().unwrap();
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
        // 访问日志：队列积压过半时暂停读取（日志文件就是缓冲，不丢行）
        let (mut access, mut next_cursor, mut more) = (vec![], None, false);
        if let Some(t) = self.tailer.as_mut() {
            if self.queue.pressure() < 0.5 {
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
            self.commit(next_cursor, stats_commit);
            return more;
        }
        let seq = match self.seq.next() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("分配 seq 失败（磁盘问题？）：{e:#}，数据留在内存缓冲");
                self.sh.buf.lock().unwrap().restore(host, entries, probes);
                return false;
            }
        };
        let report = Report {
            seq,
            boot_id: self.boot_id.clone(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            sent_at_ms: now_ms(),
            info: info.clone(),
            host,
            entries,
            probes,
            access,
            xray,
            agent_health: with_samples.then(|| self.health()),
        };
        match self.queue.push(&report) {
            Ok(()) => {
                self.commit(next_cursor, stats_commit);
                more
            }
            Err(e) => {
                tracing::error!("报告落盘失败：{e:#}，数据留在内存缓冲");
                let Report { host, entries, probes, .. } = report;
                let mut b = self.sh.buf.lock().unwrap();
                b.restore(host, entries, probes);
                if info.is_some() && b.info.is_none() {
                    b.info = info;
                }
                false
            }
        }
    }

    /// 报告已落盘：这时才前移访问日志游标、持久化流量基线
    fn commit(&mut self, cursor: Option<Cursor>, stats: Option<crate::xray::stats::StatsState>) {
        let cfg = self.sh.cfg.clone();
        if let Some(t) = self.tailer.as_mut() {
            let changed = cursor.is_some() && cursor != t.cursor;
            t.commit(cursor);
            if changed {
                if let Some(c) = t.cursor {
                    let _ = save_json(&access_cursor_path(&cfg), &c, false);
                }
            }
        }
        if let Some(s) = stats {
            let _ = save_json(&stats_path(&cfg), &s, false);
        }
    }

    fn note_failure(&mut self, why: &str) {
        self.fails += 1;
        if self.fails <= 3 || self.fails.is_multiple_of(10) {
            tracing::warn!(fails = self.fails, pending = self.queue.len(), "上报失败：{why}");
        }
    }

    /// 严格有序补传：最旧的先发，确认后再发下一个
    async fn flush(&mut self, sd: &mut watch::Receiver<bool>) {
        let t0 = Instant::now();
        let mut sent = 0;
        while let Some(seq) = self.queue.oldest() {
            if sent >= FLUSH_MAX_REPORTS || t0.elapsed() > FLUSH_MAX_TIME {
                self.next_send = Instant::now() + Duration::from_secs(1);
                return;
            }
            let body = match self.queue.read(seq) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(seq, "读取队列文件失败，丢弃：{e:#}");
                    self.queue.ack_through(seq);
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
                    if ack.accepted_seq > self.seq.seq {
                        // 服务端已确认的序号比本地发出过的最大序号还大（时钟回拨 / 本地状态丢失）：
                        // 把本地推到它之后，否则之后的新报告会被当重复丢弃
                        tracing::warn!(accepted = ack.accepted_seq, local = self.seq.seq, "服务端确认的 seq 大于本地最新 seq，推进本地序号");
                        if let Err(e) = self.seq.advance_to(ack.accepted_seq) {
                            tracing::error!("推进本地 seq 失败：{e:#}");
                        }
                    }
                    if ack.accepted_seq >= seq {
                        let n = self.queue.ack_through(ack.accepted_seq);
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
                        self.queue.dead_letter(seq);
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
                    self.queue.dead_letter(seq);
                }
            }
        }
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
}
