//! 上报循环：每 `REPORT_INTERVAL_S`（10 秒）把缓冲打成一个 Report → 先落盘进队列 → 按序发送，确认后删除。
//! 探测结果翻转时探测任务叫它马上发一份（`Shared::report_now`，两份之间至少 2 秒）：中控的告警据此马上评估。
//! 写不进磁盘（磁盘满等）时这份留在内存里、排在队列之后直接发送（`Unsaved`）：中控照样收得到遥测，健康信息里带着写盘错误。
//!
//! 磁盘 IO（读访问日志、fsync 写队列、删文件）全部放在 `spawn_blocking` 里，不占 async 运行时的工作线程；
//! 队列 / seq / 访问日志尾读器收在一个 `Core` 里由 std Mutex 保护，锁只在阻塞线程或不跨 await 的短临界区里持有。

use crate::config::Role;
use crate::host;
use crate::queue::Queue;
use crate::server::{Outcome, ServerClient, REVOKED_SLEEP};
use crate::state::{access_cursor_path, stats_path, SeqState, Shared};
use crate::util::{backoff, load_json, now_ms, save_json};
use crate::xray::access::{local_utc_offset_s, Cursor, Tailer};
use crate::xray::stats::{add_carry, carry_samples, StatsFile, StatsState};
use fleet_proto::{AgentHealth, Report};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

pub const MAX_ACCESS_PER_REPORT: usize = 2000;
const MAX_HOST_PER_REPORT: usize = 360;
const MAX_ENTRIES_PER_REPORT: usize = 3000;
const MAX_PROBES_PER_REPORT: usize = 1000;
/// 三网 PING：一份最多这么多条（中控一份收 2000 条）
const MAX_PING_PER_REPORT: usize = 1000;
/// 放置矩阵：一份最多这么多条（中控一份收 2000 条）
const MAX_RTT_PER_REPORT: usize = 1000;
const QUEUE_MAX_BYTES: u64 = 64 * 1024 * 1024;
const QUEUE_MAX_AGE_MS: i64 = 6 * 3600 * 1000;
/// 服务端连续拒收（4xx）这么多份报告就按上报失败处理
const REJECTS_AS_FAILURE: u32 = 3;
const FLUSH_MAX_REPORTS: usize = 200;
const FLUSH_MAX_TIME: Duration = Duration::from_secs(20);
/// 访问日志有积压时，一轮里追加只含访问事件的报告最多花这么久（之后先去发送，下一轮再追）
const CATCHUP_MAX_TIME: Duration = Duration::from_secs(3);
/// 上报连续失败（断网 / 服务端不可达 / 连续拒收）时，建报间隔至少拉长到这么久：
/// 合并更多样本进同一份报告，少产生队列文件（每份离线报告恢复后都要一次 RTT 重放）
const OFFLINE_BUILD_INTERVAL: Duration = Duration::from_secs(60);
/// 被叫去马上发（`Shared::report_now`）时，离上一份建报至少隔这么久
const REPORT_NOW_GAP: Duration = Duration::from_secs(2);
/// 报告持续写不进磁盘时，错误日志每隔这么久再打一条
const DISK_LOG_EVERY: Duration = Duration::from_secs(3600);

/// 磁盘相关状态：队列、序号、访问日志尾读器、stats.json（最近落盘的流量基线 + 结转表）、写盘失败的日志限流
struct Core {
    queue: Queue,
    seq: SeqState,
    tailer: Option<Tailer>,
    stats: StatsFile,
    disk: DiskLog,
}

fn lock(core: &Mutex<Core>) -> MutexGuard<'_, Core> {
    core.lock().unwrap_or_else(|e| e.into_inner())
}

/// 报告写盘（预留序号、入队）失败的日志限流：开始失败、恢复时各打一条，持续失败每 DISK_LOG_EVERY 再打一条
#[derive(Default)]
struct DiskLog {
    /// 失败中：(连续失败次数, 上次打日志的时刻)
    failing: Option<(u64, std::time::Instant)>,
}

impl DiskLog {
    fn fail(&mut self, e: &anyhow::Error) {
        match &mut self.failing {
            None => {
                tracing::error!("报告写不进磁盘：{e:#}；改为留在内存里直接发送（持续失败时每小时再提醒一次）");
                self.failing = Some((1, std::time::Instant::now()));
            }
            Some((n, at)) => {
                *n += 1;
                if at.elapsed() >= DISK_LOG_EVERY {
                    tracing::error!(fails = *n, "报告仍写不进磁盘：{e:#}");
                    *at = std::time::Instant::now();
                }
            }
        }
    }

    fn ok(&mut self) {
        if let Some((n, _)) = self.failing.take() {
            tracing::info!(fails = n, "报告恢复写入磁盘队列");
        }
    }
}

/// 写不进磁盘队列（磁盘满等）的那份报告：留在内存里，排在队列之后直接发送（`Reporter::flush`），有了结果才提交它的
/// 访问日志游标与流量基线。只活一轮：这一轮发不出去（退避中、前面的还没发完、发送失败）就把内容放回缓冲，下一轮重建
/// （换了 seq：中控其实已入库、只是响应丢了的那份会再入库一次——只有写不进磁盘时才走这条路，不为它在内存里排队）。
/// 不带结转补报的流量（结转表留给下一份写得进磁盘的报告），健康信息里带着写盘错误
struct Unsaved {
    report: Report,
    cursor: Option<Cursor>,
    stats_commit: Option<StatsState>,
}

impl Unsaved {
    /// 样本、系统信息、流量基线放回缓冲；访问事件不放（游标没前移，下次从原处重读）
    fn restore(self, sh: &Shared) {
        let Report { host, entries, probes, ping, rtt, info, .. } = self.report;
        sh.buf.lock().unwrap().restore(host, entries, probes, ping, rtt, info, self.stats_commit);
    }
}

pub struct Reporter {
    sh: Arc<Shared>,
    core: Arc<Mutex<Core>>,
    client: Arc<ServerClient>,
    boot_id: String,
    fails: u32,
    /// 服务端连续拒收的报告数（被接收一次就清零）
    rejects: u32,
    next_send: Instant,
    unsaved: Option<Unsaved>,
}

/// 失败期间建报的间隔：正常 `fleet_proto::REPORT_INTERVAL_S`（常量，不听服务端的 `ReportAck.interval_s`），连续失败时至少 OFFLINE_BUILD_INTERVAL
fn build_interval(fails: u32) -> Duration {
    let normal = Duration::from_secs(fleet_proto::REPORT_INTERVAL_S as u64);
    if fails > 0 {
        normal.max(OFFLINE_BUILD_INTERVAL)
    } else {
        normal
    }
}

impl Reporter {
    pub fn new(sh: Arc<Shared>, client: Arc<ServerClient>) -> anyhow::Result<Self> {
        let cfg = sh.cfg.clone();
        let queue = Queue::open(cfg.data_dir.join("queue"), QUEUE_MAX_BYTES, QUEUE_MAX_AGE_MS)?;
        let seq = SeqState::load(crate::state::state_path(&cfg), queue.newest());
        let tailer = (cfg.role == Role::Relay).then(|| Tailer::new(cfg.access_log.clone(), load_json::<Cursor>(&access_cursor_path(&cfg))));
        let stats = load_json::<StatsFile>(&stats_path(&cfg)).unwrap_or_default();
        tracing::info!(pending = queue.len(), seq = seq.seq, carried_entries = stats.carry.len(), "上报器启动");
        let core = Arc::new(Mutex::new(Core { queue, seq, tailer, stats, disk: DiskLog::default() }));
        Ok(Self { sh, core, client, boot_id: host::boot_id(), fails: 0, rejects: 0, next_send: Instant::now(), unsaved: None })
    }

    pub async fn run(mut self, mut sd: watch::Receiver<bool>) {
        let mut next_build = Instant::now() + build_interval(self.fails);
        let mut last_build = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let sh = self.sh.clone();
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                // 探测结果翻转：提前建报发出去（上报失败中不赶，照离线的间隔攒）
                _ = sh.report_now.notified() => {
                    if self.fails == 0 {
                        next_build = next_build.min(last_build + REPORT_NOW_GAP);
                    }
                }
                _ = sd.changed() => {
                    // 退出前把缓冲里的数据落盘，下次启动继续发
                    self.build_and_push(true).await;
                    return;
                }
            }
            let now = Instant::now();
            if now >= next_build {
                next_build = now + build_interval(self.fails);
                last_build = now;
                let mut more = self.build_and_push(true).await;
                // 访问日志有积压：在时间预算内追加只含访问事件的报告，尽快追上。
                // 上报连续失败时不追：离线期间日志文件本身就是缓冲，不必把它大量搬进队列
                while more && self.fails == 0 && now.elapsed() < CATCHUP_MAX_TIME {
                    more = self.build_and_push(false).await;
                }
            }
            if now >= self.next_send && (self.unsaved.is_some() || !lock(&self.core).queue.is_empty()) {
                self.flush(&mut sd).await;
            }
            // 写不进磁盘的那份这一轮没发出去（退避中、前面的还没发完、发送失败）：内容放回缓冲，下一轮重建
            if let Some(u) = self.unsaved.take() {
                u.restore(&self.sh);
            }
            // flush 里等 POST 时收到了关停信号：直接落盘缓冲退出，不再等那个请求
            if *sd.borrow() {
                self.build_and_push(true).await;
                return;
            }
        }
    }

    /// 在阻塞线程里操作 Core（磁盘 IO）。阻塞任务 panic 时让上报任务也 panic：进程退出由 systemd 重启
    async fn blocking<R: Send + 'static>(&self, f: impl FnOnce(&mut Core) -> R + Send + 'static) -> R {
        let core = self.core.clone();
        tokio::task::spawn_blocking(move || f(&mut lock(&core))).await.expect("上报器的阻塞任务异常终止")
    }

    /// 取走缓冲 → 分配 seq → 落盘入队；写不进磁盘的留在 `self.unsaved` 待发。返回访问日志是否还有积压。
    async fn build_and_push(&mut self, with_samples: bool) -> bool {
        let (sh, core, boot_id, online) = (self.sh.clone(), self.core.clone(), self.boot_id.clone(), self.fails == 0);
        let (more, unsaved) =
            tokio::task::spawn_blocking(move || build_and_push_blocking(&sh, &core, &boot_id, with_samples, online)).await.expect("上报器的阻塞任务异常终止");
        // 一轮至多一份：写不进磁盘时 more 为 false，本轮不再追加建报
        if unsaved.is_some() {
            self.unsaved = unsaved;
        }
        more
    }

    /// 内存里的那份有了结果（入库，或被永久拒绝）：这时才提交它的访问日志游标与流量基线。
    /// 被拒时写不进磁盘、没法留进 dead/，只计数；其中的入口流量照样结转（它不带结转补报的流量，见 `Queue::dead_letter`）
    async fn settle_unsaved(&mut self, rejected: bool) {
        let (u, sh) = (self.unsaved.take().expect("刚发出的就是它"), self.sh.clone());
        self.blocking(move |c| {
            if rejected {
                c.queue.dead += 1;
                add_carry(&mut c.stats.carry, &u.report.entries);
            }
            commit(&sh, &mut c.tailer, u.cursor, &mut c.stats, u.stats_commit, rejected);
        })
        .await;
    }

    fn note_failure(&mut self, why: &str) {
        self.fails += 1;
        if self.fails <= 3 || self.fails.is_multiple_of(10) {
            tracing::warn!(fails = self.fails, pending = lock(&self.core).queue.len(), "上报失败：{why}");
        }
    }

    /// 严格有序补传：最旧的先发，确认后再发下一个；队列发空了再发写不进磁盘、留在内存里的那份（序号最大）。
    /// 不并发：服务端按 seq 大小去重（seq <= 已收到的最大值就当重复丢弃），乱序到达会让较小的 seq 被静默丢掉。
    async fn flush(&mut self, sd: &mut watch::Receiver<bool>) {
        let t0 = Instant::now();
        let mut sent = 0;
        loop {
            // path 为 None：发内存里的那份
            let (seq, path) = {
                let c = lock(&self.core);
                match (c.queue.oldest(), &self.unsaved) {
                    (Some(seq), _) => (seq, Some(c.queue.file_path(seq))),
                    (None, Some(u)) => (u.report.seq, None),
                    (None, None) => break,
                }
            };
            if sent >= FLUSH_MAX_REPORTS || t0.elapsed() > FLUSH_MAX_TIME {
                self.next_send = Instant::now() + Duration::from_secs(1);
                return;
            }
            let in_memory = path.is_none();
            let body = match path {
                Some(path) => {
                    let read = tokio::task::spawn_blocking(move || std::fs::read(path)).await.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| e.to_string()));
                    match read {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::error!(seq, "读取队列文件失败，丢弃：{e}");
                            self.blocking(move |c| c.queue.ack_through(seq)).await;
                            continue;
                        }
                    }
                }
                None => serde_json::to_vec(&self.unsaved.as_ref().expect("刚取到").report).expect("报告序列化成 JSON 不会失败"),
            };
            let out = tokio::select! {
                o = self.client.post_report(body) => o,
                // 关停：放弃这次请求（报告还在磁盘队列里，下次启动继续发），让上层赶紧落盘缓冲
                _ = sd.changed() => return,
            };
            match out {
                Outcome::Ok(ack) => {
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
                    // 成功响应就说明这份已入库：新入库的回本份 seq，重复的回中控已存的 seq（不小于本份）
                    let through = seq.max(ack.accepted_seq);
                    let n = self.blocking(move |c| c.queue.ack_through(through)).await + usize::from(in_memory);
                    if in_memory {
                        self.settle_unsaved(false).await;
                    }
                    if self.fails > 0 {
                        tracing::info!("上报恢复，已确认 {n} 个报告");
                    }
                    self.fails = 0;
                    self.rejects = 0;
                    sent += 1;
                }
                // 中控那边这台机器的上一份还在处理（或超了限速）：不算失败——不放长建报间隔、不停追赶访问日志、不记「上报失败」，
                // 按 Retry-After 再发这一份
                Outcome::Busy { wait, why } => {
                    tracing::debug!(seq, "中控正忙，{} 秒后再发：{why}", wait.as_secs());
                    self.next_send = Instant::now() + wait;
                    return;
                }
                Outcome::Retry { wait, why } => {
                    self.note_failure(&why);
                    self.next_send = Instant::now() + wait.unwrap_or_else(|| backoff(self.fails, 5, 30));
                    return;
                }
                Outcome::Revoked => {
                    tracing::error!("服务端回 401：令牌已被撤销或无效。停止上报 {} 秒后重试（数据继续在本地排队）", REVOKED_SLEEP.as_secs());
                    // 也算失败：建报间隔随之放长，少攒队列文件
                    self.fails += 1;
                    self.next_send = Instant::now() + REVOKED_SLEEP;
                    return;
                }
                Outcome::Rejected(why) => {
                    if in_memory {
                        tracing::error!(seq, "服务端永久拒绝该报告（写不进磁盘，没有留存）：{why}");
                        self.settle_unsaved(true).await;
                    } else {
                        // 永久拒绝：移入 dead/ 留证，其中的入口流量结转到下一份报告；继续发后面的，不堵队列
                        tracing::error!(seq, "服务端永久拒绝该报告（已移入 dead/ 目录）：{why}");
                        let sh = self.sh.clone();
                        self.blocking(move |c| reject(&sh, c, seq)).await;
                    }
                    self.rejects += 1;
                    if self.rejects >= REJECTS_AS_FAILURE {
                        // 连续拒收多半是系统性问题（agent 与中控版本不配套等）：照常建报、读访问日志只会把数据成批送进 dead/。
                        // 按失败处理，进入离线模式（停读访问日志、停追赶、建报间隔拉长），访问日志留在文件里当缓冲
                        self.note_failure(&format!("服务端连续 {} 次拒收报告", self.rejects));
                        self.next_send = Instant::now() + backoff(self.fails, 5, 30);
                        return;
                    }
                }
            }
        }
    }
}

/// 本地状态文件连续写失败的次数（stats.json、access.json 各一个：一个一直写不进、另一个照常写时不互相清零）：
/// 只在第 1 到 3 次和之后每 100 次打日志，成功就清零
static STATS_SAVE_FAILS: AtomicU64 = AtomicU64::new(0);
static ACCESS_SAVE_FAILS: AtomicU64 = AtomicU64::new(0);

/// 写状态文件（`fails` 是这个文件自己的连续失败计数）。写不进去（磁盘满、权限不对）不能悄悄吞掉：重启后会按旧基线重复算流量、重复上报访问事件
fn save_state<T: serde::Serialize>(path: &Path, v: &T, what: &str, fails: &AtomicU64) {
    match save_json(path, v, false) {
        Ok(()) => fails.store(0, Ordering::Relaxed),
        Err(e) => {
            let n = fails.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 3 || n.is_multiple_of(100) {
                tracing::warn!(fails = n, "{what} 写不进磁盘：{e:#}（重启后可能重复算流量或重复上报访问事件）");
            }
        }
    }
}

/// 服务端永久拒绝的报告移入 dead/；`Queue::dead_letter` 交回的入口流量并进结转表（随 stats.json 落盘），由下一份报告补报
fn reject(sh: &Shared, c: &mut Core, seq: u64) {
    let es = c.queue.dead_letter(seq);
    if !es.is_empty() {
        add_carry(&mut c.stats.carry, &es);
        save_state(&stats_path(&sh.cfg), &c.stats, "stats.json", &STATS_SAVE_FAILS);
    }
}

/// 报告已落盘：这时才前移访问日志游标、持久化流量基线（`base`：stats_task 交来的新基线）与结转表
fn commit(sh: &Shared, tailer: &mut Option<Tailer>, cursor: Option<Cursor>, stats: &mut StatsFile, base: Option<StatsState>, carry_changed: bool) {
    if let Some(t) = tailer.as_mut() {
        let changed = cursor.is_some() && cursor != t.cursor;
        t.commit(cursor);
        if changed {
            if let Some(c) = t.cursor {
                save_state(&access_cursor_path(&sh.cfg), &c, "access.json", &ACCESS_SAVE_FAILS);
            }
        }
    }
    let dirty = carry_changed || base.is_some();
    if let Some(b) = base {
        stats.base = b;
    }
    if dirty {
        save_state(&stats_path(&sh.cfg), &*stats, "stats.json", &STATS_SAVE_FAILS);
    }
}

/// 阻塞线程里的建报：取走缓冲 → 读访问日志 → 分配 seq → 落盘入队。返回访问日志是否还有积压，
/// 以及写不进磁盘、留在内存里待发的这份（见 `Unsaved`）。
/// `online`：上报没有连续失败（Reporter::fails == 0）
fn build_and_push_blocking(sh: &Shared, core: &Mutex<Core>, boot_id: &str, with_samples: bool, online: bool) -> (bool, Option<Unsaved>) {
    let (mut host, mut entries, mut probes, mut ping, mut rtt, mut info, mut xray, mut singbox, mut stats_commit) = (vec![], vec![], vec![], vec![], vec![], None, None, None, None);
    if with_samples {
        let mut b = sh.buf.lock().unwrap();
        let n = b.host.len().min(MAX_HOST_PER_REPORT);
        host = b.host.drain(..n).collect();
        let n = b.entries.len().min(MAX_ENTRIES_PER_REPORT);
        entries = b.entries.drain(..n).collect();
        let n = b.probes.len().min(MAX_PROBES_PER_REPORT);
        probes = b.probes.drain(..n).collect();
        let n = b.ping.len().min(MAX_PING_PER_REPORT);
        ping = b.ping.drain(..n).collect();
        let n = b.rtt.len().min(MAX_RTT_PER_REPORT);
        rtt = b.rtt.drain(..n).collect();
        info = b.info.take();
        xray = b.xray.clone();
        singbox = b.singbox.clone();
        stats_commit = b.stats_commit.take();
    }
    let mut guard = lock(core);
    let Core { queue, seq, tailer, stats, disk } = &mut *guard;
    // 访问日志：上报连续失败或队列积压过半时暂停读取（日志文件就是缓冲，不丢行）。
    // 离线时读进队列的事件会随报告一起被 QUEUE_MAX_AGE_MS 淘汰；留在日志里，恢复后从游标处接着读
    let (mut access, mut next_cursor, mut more) = (vec![], None, false);
    if let Some(t) = tailer.as_mut() {
        if online && queue.pressure() < 0.5 {
            match t.read_batch(MAX_ACCESS_PER_REPORT, local_utc_offset_s()) {
                Ok(b) => {
                    access = b.events;
                    next_cursor = b.next;
                    more = b.more;
                    sh.health.access_read_errs.store(0, Ordering::Relaxed);
                }
                Err(e) => {
                    let n = sh.health.access_read_errs.fetch_add(1, Ordering::Relaxed) + 1;
                    if n <= 3 || n.is_multiple_of(100) {
                        tracing::warn!(fails = n, "读取访问日志失败：{e:#}");
                    }
                }
            }
        }
    }
    if host.is_empty() && entries.is_empty() && probes.is_empty() && ping.is_empty() && rtt.is_empty() && access.is_empty() && info.is_none() && xray.is_none() {
        // 没有内容；但访问日志游标（例如首次定位到文件末尾）仍要提交
        commit(sh, tailer, next_cursor, stats, stats_commit, false);
        return (more, None);
    }
    // 预留号段要写 state.json：写不进去也照样在内存里分配序号，这份不入队、留在内存里发
    let (seq, seq_err) = match seq.next() {
        Ok(s) => (s, None),
        Err(e) => (seq.next_unreserved(), Some(e.context("预留上报序号"))),
    };
    // 之前被淘汰 / 拒收的报告里结转下来的入口流量，接在本轮样本后面随这份报告补报（写不进磁盘时截掉，结转表不动）
    let sampled = entries.len();
    entries.extend(carry_samples(&stats.carry, now_ms()));
    let mut report = Report {
        seq,
        boot_id: boot_id.to_string(),
        agent_version: fleet_proto::build_version().to_string(),
        sent_at_ms: now_ms(),
        info,
        host,
        entries,
        probes,
        access,
        xray,
        singbox,
        agent_health: with_samples.then(|| health_of(sh, queue, tailer.as_ref())),
        ping,
        rtt,
    };
    let err = match seq_err {
        Some(e) => e,
        None => match queue.push(&report, report.entries.len() > sampled) {
            Ok(evicted) => {
                disk.ok();
                // 结转表里的流量已经落进这份报告；换成这次入队时被淘汰的报告里的
                let carry_changed = !(stats.carry.is_empty() && evicted.is_empty());
                stats.carry = evicted;
                commit(sh, tailer, next_cursor, stats, stats_commit, carry_changed);
                return (more, None);
            }
            Err(e) => e,
        },
    };
    disk.fail(&err);
    report.entries.truncate(sampled);
    if let Some(h) = report.agent_health.as_mut() {
        h.disk_write_error = Some(format!("{err:#}"));
    }
    (false, Some(Unsaved { report, cursor: next_cursor, stats_commit }))
}

/// `health` 的拆借用版本：建报时 queue / tailer 已被解构借出，不能再传整个 Core
fn health_of(sh: &Shared, queue: &Queue, tailer: Option<&Tailer>) -> AgentHealth {
    let cnt = tailer.map(|t| t.counters.clone()).unwrap_or_default();
    AgentHealth {
        queue_len: queue.len() as u32,
        queue_dropped: queue.dropped,
        queue_dead: queue.dead,
        access_unparsed: cnt.unparsed,
        access_dropped_lines: cnt.dropped_lines,
        access_rotation_gaps: cnt.rotation_gaps,
        access_truncations: cnt.truncations,
        last_apply_error: sh.health.last_apply_error.lock().unwrap().clone(),
        disk_write_error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_interval_stretches_only_when_failing() {
        assert_eq!(build_interval(0), Duration::from_secs(10));
        assert_eq!(build_interval(1), OFFLINE_BUILD_INTERVAL);
        assert_eq!(build_interval(3), OFFLINE_BUILD_INTERVAL);
    }

    fn tmp(n: &str) -> crate::util::TmpDir { crate::util::TmpDir::new(&format!("rep-{n}")) }

    /// 两个状态文件各记各的连续失败：一个一直写不进、另一个照常写时，前一个的计数不被清零（日志照样限流）
    #[test]
    fn state_save_fails_counted_per_file() {
        let d = tmp("savefails");
        let (a, b) = (AtomicU64::new(0), AtomicU64::new(0));
        let bad = d.join("no-such-dir/stats.json");
        for _ in 0..3 {
            save_state(&bad, &1, "stats.json", &a);
            save_state(&d.join("access.json"), &1, "access.json", &b);
        }
        assert_eq!((a.load(Ordering::Relaxed), b.load(Ordering::Relaxed)), (3, 0));
        save_state(&d.join("stats.json"), &1, "stats.json", &a);
        assert_eq!(a.load(Ordering::Relaxed), 0, "写成功清零");
    }

    fn shared(d: &std::path::Path) -> Arc<Shared> {
        let mut cfg = crate::config::Config::relay_default();
        cfg.data_dir = d.to_path_buf();
        cfg.access_log = d.join("access.log");
        Shared::new(Arc::new(cfg))
    }

    fn core_with(d: &std::path::Path, seq_path: std::path::PathBuf, tailer: Option<Tailer>, max_bytes: u64) -> Mutex<Core> {
        let queue = Queue::open(d.join("queue"), max_bytes, QUEUE_MAX_AGE_MS).unwrap();
        Mutex::new(Core { queue, seq: SeqState::load(seq_path, None), tailer, stats: StatsFile::default(), disk: DiskLog::default() })
    }

    fn core(d: &std::path::Path, seq_path: std::path::PathBuf, tailer: Option<Tailer>) -> Mutex<Core> {
        core_with(d, seq_path, tailer, QUEUE_MAX_BYTES)
    }

    #[test]
    fn evicted_traffic_rides_on_a_later_report() {
        use crate::xray::stats::Carry;
        let d = tmp("carry");
        let sh = shared(&d);
        let id = uuid::Uuid::from_u128(7);
        // 字节上限 1：每次入队都把最新一份以外的全部淘汰
        let core = core_with(&d, d.join("state.json"), None, 1);
        let carried = || load_json::<StatsFile>(&stats_path(&sh.cfg)).map(|f| f.carry);
        sh.buf.lock().unwrap().push_entries(vec![fleet_proto::EntrySample { entry_id: id, ts_ms: 0, up_bytes: 100, down_bytes: 200, conns: 3 }]);
        build_and_push_blocking(&sh, &core, "b", true, true);
        // 第二份入队时第一份被淘汰：它的流量进结转表并随 stats.json 落盘
        sh.buf.lock().unwrap().info = Some(Default::default());
        build_and_push_blocking(&sh, &core, "b", true, true);
        assert_eq!(lock(&core).stats.carry, Carry::from([(id, (100, 200))]));
        assert_eq!(carried(), Some(Carry::from([(id, (100, 200))])));
        // 第三份把它带出去；被淘汰的第二份没有流量，结转表清空
        sh.buf.lock().unwrap().info = Some(Default::default());
        build_and_push_blocking(&sh, &core, "b", true, true);
        let c = lock(&core);
        let r: Report = serde_json::from_slice(&c.queue.read(c.queue.oldest().unwrap()).unwrap()).unwrap();
        assert_eq!(r.entries.iter().map(|e| (e.entry_id, e.up_bytes, e.down_bytes)).collect::<Vec<_>>(), [(id, 100, 200)]);
        assert!(c.stats.carry.is_empty());
        assert_eq!(carried(), Some(Carry::new()));
    }

    #[test]
    fn rejected_traffic_rides_on_the_next_report_once() {
        use crate::xray::stats::Carry;
        let d = tmp("reject");
        let sh = shared(&d);
        let id = uuid::Uuid::from_u128(9);
        let core = core(&d, d.join("state.json"), None);
        sh.buf.lock().unwrap().push_entries(vec![fleet_proto::EntrySample { entry_id: id, ts_ms: 0, up_bytes: 100, down_bytes: 200, conns: 3 }]);
        build_and_push_blocking(&sh, &core, "b", true, true);
        // 被拒：流量进结转表并随 stats.json 落盘
        let first = lock(&core).queue.oldest().unwrap();
        reject(&sh, &mut lock(&core), first);
        assert_eq!(lock(&core).stats.carry, Carry::from([(id, (100, 200))]));
        assert_eq!(load_json::<StatsFile>(&stats_path(&sh.cfg)).map(|f| f.carry), Some(Carry::from([(id, (100, 200))])));
        // 下一份把它带上
        sh.buf.lock().unwrap().info = Some(Default::default());
        build_and_push_blocking(&sh, &core, "b", true, true);
        let second = lock(&core).queue.oldest().unwrap();
        let r: Report = serde_json::from_slice(&lock(&core).queue.read(second).unwrap()).unwrap();
        assert_eq!(r.entries.iter().map(|e| (e.entry_id, e.up_bytes, e.down_bytes)).collect::<Vec<_>>(), [(id, 100, 200)]);
        assert!(lock(&core).stats.carry.is_empty());
        // 带着它的这份也被拒：不再结转
        reject(&sh, &mut lock(&core), second);
        assert!(lock(&core).stats.carry.is_empty());
        assert_eq!(lock(&core).queue.dead, 2);
    }

    #[test]
    fn access_log_stays_in_file_while_offline() {
        use std::os::unix::fs::MetadataExt;
        let d = tmp("offline");
        let sh = shared(&d);
        let log = sh.cfg.access_log.clone();
        let id = uuid::Uuid::from_u128(0xabc).simple();
        std::fs::write(&log, format!("2026/10/20 13:50:56 from 1.2.3.4:5 accepted tcp:a.com:80 [in-{id} -> out-x] email: e-{id}\n")).unwrap();
        let cur = Cursor { inode: std::fs::metadata(&log).unwrap().ino(), offset: 0, fp: None };
        let core = core(&d, d.join("state.json"), Some(Tailer::new(log, Some(cur))));
        // 上报连续失败：不读日志、游标不动
        build_and_push_blocking(&sh, &core, "b", true, false);
        assert_eq!(lock(&core).queue.len(), 0);
        assert_eq!(lock(&core).tailer.as_ref().unwrap().cursor, Some(cur));
        // 恢复后从原游标接着读
        build_and_push_blocking(&sh, &core, "b", true, true);
        let c = lock(&core);
        let r: Report = serde_json::from_slice(&std::fs::read(c.queue.file_path(c.queue.oldest().unwrap())).unwrap()).unwrap();
        assert_eq!(r.access.len(), 1);
    }

    #[test]
    fn report_that_cannot_be_written_stays_in_memory() {
        use crate::xray::stats::Carry;
        let d = tmp("putback");
        let sh = shared(&d);
        let id = uuid::Uuid::from_u128(5);
        {
            let mut b = sh.buf.lock().unwrap();
            b.info = Some(Default::default());
            b.stats_commit = Some(Default::default());
            b.push_entries(vec![fleet_proto::EntrySample { entry_id: id, ts_ms: 0, up_bytes: 1, down_bytes: 2, conns: 0 }]);
        }
        // 留在内存里待发：健康信息带着写盘错误，不带结转的流量（结转表留给写得进磁盘的报告）；
        // 这一轮发不出去就放回缓冲，流量基线也没被提前持久化
        let check = |c: &Mutex<Core>, why: &str| {
            lock(c).stats.carry = Carry::from([(id, (10, 20))]);
            let (more, u) = build_and_push_blocking(&sh, c, "b", true, true);
            let u = u.expect("留在内存里待发");
            let err = u.report.agent_health.as_ref().and_then(|h| h.disk_write_error.clone()).unwrap_or_default();
            assert!(!more && err.contains(why), "{err}");
            assert_eq!(u.report.entries.iter().map(|e| e.up_bytes).collect::<Vec<_>>(), [1]);
            assert_eq!(lock(c).stats.carry.len(), 1);
            u.restore(&sh);
            let b = sh.buf.lock().unwrap();
            assert!(b.info.is_some() && b.stats_commit.is_some() && b.entries.len() == 1);
            assert!(!stats_path(&sh.cfg).exists());
        };
        // 预留序号失败：状态文件所在目录不存在
        check(&core(&d, d.join("missing").join("state.json"), None), "预留上报序号");
        // 入队失败：队列目录没了
        let c = core(&d, d.join("state.json"), None);
        std::fs::remove_dir_all(d.join("queue")).unwrap();
        check(&c, "queue");
    }

    /// 写不进磁盘的那份排在队列之后从内存发出：中控先收到队列里更早的（序号小，否则会被当重复丢掉），再收到它；
    /// 入库后才提交它的流量基线
    #[tokio::test]
    async fn unsaved_report_goes_out_after_the_queue() {
        let d = tmp("unsaved");
        let sh = shared(&d);
        let (base, seen) = crate::server::tests::mock_server().await;
        let (client, _tok_dir) = crate::server::tests::client_for(&base, "unsaved");
        let mut r = Reporter::new(sh.clone(), Arc::new(client)).unwrap();
        sh.buf.lock().unwrap().info = Some(Default::default());
        r.build_and_push(true).await;
        // 下一份的临时文件位置被目录占住：写不进队列
        let next = lock(&r.core).seq.seq + 1;
        std::fs::create_dir_all(d.join("queue").join(format!(".{next:020}.json.tmp"))).unwrap();
        {
            let mut b = sh.buf.lock().unwrap();
            b.info = Some(Default::default());
            b.stats_commit = Some(StatsState { initialized: true, ..Default::default() });
        }
        r.build_and_push(true).await;
        assert!(r.unsaved.is_some() && lock(&r.core).queue.len() == 1);
        let (_tx, mut sd) = watch::channel(false);
        r.flush(&mut sd).await;
        let got: Vec<Report> = seen.lock().unwrap().iter().map(|(_, b)| serde_json::from_slice(b).unwrap()).collect();
        assert_eq!(got.iter().map(|r| r.seq).collect::<Vec<_>>(), [next - 1, next]);
        let errs: Vec<_> = got.iter().map(|r| r.agent_health.as_ref().unwrap().disk_write_error.is_some()).collect();
        assert_eq!(errs, [false, true]);
        assert!(r.unsaved.is_none() && lock(&r.core).queue.is_empty());
        assert!(lock(&r.core).stats.base.initialized, "入库后才提交流量基线");
    }

    /// 429（中控那边这台机器的上一份还在处理）不算失败：fails 不变、按 Retry-After 再发同一份
    #[tokio::test]
    async fn busy_is_not_a_failure() {
        let d = tmp("busy");
        let sh = shared(&d);
        let (base, seen) = crate::server::tests::mock_server_busy(1).await;
        let (client, _tok_dir) = crate::server::tests::client_for(&base, "busy");
        let mut r = Reporter::new(sh.clone(), Arc::new(client)).unwrap();
        sh.buf.lock().unwrap().info = Some(Default::default());
        r.build_and_push(true).await;
        let (_tx, mut sd) = watch::channel(false);
        r.flush(&mut sd).await;
        assert_eq!((r.fails, lock(&r.core).queue.len()), (0, 1));
        assert!(r.next_send > Instant::now() + Duration::from_millis(500) && r.next_send <= Instant::now() + Duration::from_secs(1));
        r.flush(&mut sd).await;
        assert_eq!((r.fails, lock(&r.core).queue.len(), seen.lock().unwrap().len()), (0, 0, 2));
    }
}
