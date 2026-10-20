//! 上报循环：每 interval_s 把缓冲打成一个 Report → 先落盘进队列 → 按序发送，确认后删除。

use crate::config::Role;
use crate::host;
use crate::queue::Queue;
use crate::server::{Outcome, ServerClient};
use crate::state::{access_cursor_path, stats_path, SeqState, Shared};
use crate::util::{backoff, load_json, now_ms, save_json};
use crate::xray::access::{local_utc_offset_s, Cursor, Tailer};
use fleet_proto::Report;
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
}

impl Reporter {
    pub fn new(sh: Arc<Shared>, client: ServerClient) -> anyhow::Result<Self> {
        let cfg = sh.cfg.clone();
        let queue = Queue::open(cfg.data_dir.join("queue"), QUEUE_MAX_BYTES, QUEUE_MAX_AGE_MS)?;
        let seq = SeqState::load(crate::state::state_path(&cfg), queue.newest());
        let tailer = (cfg.role == Role::Relay).then(|| Tailer::new(cfg.access_log.clone(), load_json::<Cursor>(&access_cursor_path(&cfg))));
        tracing::info!(pending = queue.len(), seq = seq.seq, "上报器启动");
        Ok(Self { sh, queue, client, seq, tailer, boot_id: host::boot_id(), interval_s: 10, fails: 0, next_send: Instant::now() })
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
                self.flush().await;
            }
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
        if self.fails <= 3 || self.fails % 10 == 0 {
            tracing::warn!(fails = self.fails, pending = self.queue.len(), "上报失败：{why}");
        }
    }

    /// 严格有序补传：最旧的先发，确认后再发下一个
    async fn flush(&mut self) {
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
            match self.client.post_report(body).await {
                Outcome::Ok(ack) => {
                    if ack.interval_s > 0 {
                        self.interval_s = ack.interval_s.clamp(2, 300);
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
                Outcome::Retry { wait, why } => {
                    self.note_failure(&why);
                    self.next_send = Instant::now() + wait.unwrap_or_else(|| backoff(self.fails, 5, 60));
                    return;
                }
                Outcome::Revoked => {
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
