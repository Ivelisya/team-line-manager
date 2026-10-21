//! 进程内共享状态 + data_dir 里的持久化小文件

use crate::config::Config;
use crate::util::{load_json, save_json};
use crate::xray::stats::StatsState;
use fleet_proto::{DesiredState, EntrySample, HostSample, ProbeSample, SystemInfo, XrayStatus};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

const HOST_CAP: usize = 4320; // 12 h @10 s
const ENTRY_CAP: usize = 200_000;
const PROBE_CAP: usize = 50_000;

/// 采集任务写、上报任务取的缓冲
#[derive(Default)]
pub struct Buffers {
    pub host: Vec<HostSample>,
    pub entries: Vec<EntrySample>,
    pub probes: Vec<ProbeSample>,
    pub info: Option<SystemInfo>,
    pub xray: Option<XrayStatus>,
    /// 流量基线：报告落盘成功后才持久化，崩溃时不会"基线前移但增量丢失"
    pub stats_commit: Option<StatsState>,
}

/// 缓冲满时每次推入都会丢弃：日志只在前几次和之后每 100 次打一条（`hits` 为该缓冲的丢弃次数）
fn push_capped<T>(v: &mut Vec<T>, items: impl IntoIterator<Item = T>, cap: usize, what: &str, hits: &AtomicU64) {
    v.extend(items);
    if v.len() > cap {
        let n = v.len() - cap;
        v.drain(..n);
        let h = hits.fetch_add(1, Ordering::Relaxed) + 1;
        if h <= 3 || h.is_multiple_of(100) {
            tracing::warn!(dropped_times = h, "缓冲 {what} 超过上限 {cap}，丢弃最旧 {n} 条");
        }
    }
}

impl Buffers {
    pub fn push_host(&mut self, s: HostSample) {
        static HITS: AtomicU64 = AtomicU64::new(0);
        push_capped(&mut self.host, [s], HOST_CAP, "host", &HITS);
    }
    pub fn push_entries(&mut self, s: Vec<EntrySample>) {
        static HITS: AtomicU64 = AtomicU64::new(0);
        push_capped(&mut self.entries, s, ENTRY_CAP, "entries", &HITS);
    }
    pub fn push_probe(&mut self, s: ProbeSample) {
        static HITS: AtomicU64 = AtomicU64::new(0);
        push_capped(&mut self.probes, [s], PROBE_CAP, "probes", &HITS);
    }
    /// 建报失败时把取走的东西放回去（样本放在最前面保持时间顺序；info / 流量基线只在缓冲里还没有更新的值时放回）
    pub fn restore(&mut self, host: Vec<HostSample>, entries: Vec<EntrySample>, probes: Vec<ProbeSample>, info: Option<SystemInfo>, stats_commit: Option<StatsState>) {
        self.host.splice(0..0, host);
        self.entries.splice(0..0, entries);
        self.probes.splice(0..0, probes);
        if self.info.is_none() {
            self.info = info;
        }
        if self.stats_commit.is_none() {
            self.stats_commit = stats_commit;
        }
    }
}

/// Agent 自身健康里不属于队列/尾读器的那部分（各任务写，上报器读）
#[derive(Default)]
pub struct Health {
    pub probes_skipped: AtomicU64,
    pub realign_fixes: AtomicU64,
    /// 访问日志连续读取失败次数（成功即清零，仅用于日志限流）
    pub access_read_errs: AtomicU64,
    pub last_apply_error: Mutex<Option<String>>,
}

pub struct Shared {
    pub cfg: Arc<Config>,
    pub health: Health,
    pub buf: Mutex<Buffers>,
    /// 线路机：最近一次成功应用的期望状态（探测、流量统计据此知道有哪些入口）
    /// 用 Arc 共享：探测任务每秒读一次，不必每次深拷贝整个期望状态
    pub applied: RwLock<Option<Arc<DesiredState>>>,
    pub applied_gen: AtomicU64,
}

impl Shared {
    pub fn new(cfg: Arc<Config>) -> Arc<Self> {
        Arc::new(Self { cfg, health: Health::default(), buf: Mutex::new(Buffers::default()), applied: RwLock::new(None), applied_gen: AtomicU64::new(0) })
    }
}

// ── 持久化文件 ───────────────────────────────────────────────────────────────

pub fn state_path(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("state.json")
}
pub fn applied_path(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("applied.json")
}
pub fn pending_ack_path(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("pending_ack.json")
}
pub fn stats_path(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("stats.json")
}
pub fn access_cursor_path(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("access.json")
}

#[derive(Serialize, Deserialize, Default)]
struct SeqFile {
    seq: u64,
}

/// 号段预留：每预留 SEQ_BLOCK 个序号才 fsync 落盘一次
const SEQ_BLOCK: u64 = 1000;

/// 单调递增的上报序号。
/// 落盘的是"已预留的上界"：本进程发出过的任何序号都 ≤ 它。重启时从 max(落盘上界, 队列里最大序号) 之后继续，
/// 序号会跳过一段（服务端只比较大小去重，空洞无妨）；崩溃只会浪费号段，不会复用序号。
/// 状态丢失时从当前 unix 毫秒起算（每个报告至少隔 1 s，所以必然大于之前用过的任何序号）。
pub struct SeqState {
    path: PathBuf,
    pub seq: u64,
    /// 已落盘的上界
    reserved: u64,
}

impl SeqState {
    pub fn load(path: PathBuf, queue_newest: Option<u64>) -> Self {
        let saved = load_json::<SeqFile>(&path).map(|f| f.seq);
        let base = saved.unwrap_or_else(|| crate::util::now_ms() as u64);
        // 上界取"落盘值"：此后第一次分配就会超过它，立刻预留新号段
        Self { path, seq: base.max(queue_newest.unwrap_or(0)), reserved: saved.unwrap_or(0) }
    }
    fn reserve(&mut self, upto: u64) -> anyhow::Result<()> {
        save_json(&self.path, &SeqFile { seq: upto }, true)?;
        self.reserved = upto;
        Ok(())
    }
    /// 服务端确认的序号比本地发出过的还大（时钟回拨 / 状态丢失后）：把本地推到它之后，
    /// 否则后续报告会被服务端当重复丢弃。
    pub fn advance_to(&mut self, n: u64) -> anyhow::Result<()> {
        if n > self.seq {
            if n > self.reserved {
                self.reserve(n)?;
            }
            self.seq = n;
        }
        Ok(())
    }
    pub fn next(&mut self) -> anyhow::Result<u64> {
        let n = self.seq + 1;
        if n > self.reserved {
            self.reserve(n + SEQ_BLOCK - 1)?;
        }
        self.seq = n;
        Ok(n)
    }
}

#[derive(Serialize, Deserialize)]
pub struct AppliedFile {
    pub generation: u64,
    pub desired: DesiredState,
}

pub fn load_applied(cfg: &Config) -> Option<AppliedFile> {
    load_json(&applied_path(cfg))
}

/// 含 REALITY 私钥和出站密码，0600
pub fn save_applied(cfg: &Config, d: &DesiredState) {
    if let Err(e) = save_json(&applied_path(cfg), &AppliedFile { generation: d.generation, desired: d.clone() }, true) {
        tracing::error!("保存 applied.json 失败：{e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_is_monotonic_across_reload() {
        let d = std::env::temp_dir().join(format!("fa-seq-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("state.json");
        let mut s = SeqState::load(p.clone(), None);
        let a = s.next().unwrap();
        let b = s.next().unwrap();
        assert_eq!(b, a + 1);
        // 号段预留：重启后跳过本段剩余序号，但绝不回退/复用
        let mut s2 = SeqState::load(p.clone(), None);
        assert!(s2.next().unwrap() > b);
        // 状态文件丢了：以队列里最大序号为下限，且起点是时间戳量级
        std::fs::remove_file(&p).unwrap();
        let s3 = SeqState::load(p, Some(u64::MAX / 2));
        assert_eq!(s3.seq, u64::MAX / 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn advance_to_only_moves_forward() {
        let d = std::env::temp_dir().join(format!("fa-seq-adv-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("state.json");
        let mut s = SeqState::load(p.clone(), Some(100));
        let before = s.seq; // 无状态文件：从当前时间戳起算
        s.advance_to(50).unwrap();
        assert_eq!(s.seq, before); // 往回推无效
        s.advance_to(before + 5000).unwrap(); // 服务端确认的比本地大（时钟回拨后）
        assert_eq!(s.next().unwrap(), before + 5001);
        // 落盘了：重新加载后仍在它之后
        assert!(SeqState::load(p, None).seq >= before + 5001);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn seq_reserves_in_blocks_and_never_reuses() {
        let d = std::env::temp_dir().join(format!("fa-seq-blk-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("state.json");
        let mut s = SeqState::load(p.clone(), None);
        let first = s.next().unwrap();
        let on_disk = |p: &PathBuf| load_json::<SeqFile>(p).unwrap().seq;
        let bound = on_disk(&p);
        assert_eq!(bound, first + SEQ_BLOCK - 1);
        // 一个号段内不再写盘
        for i in 1..SEQ_BLOCK {
            assert_eq!(s.next().unwrap(), first + i);
        }
        assert_eq!(on_disk(&p), bound);
        // 号段用完：预留下一段
        let n = s.next().unwrap();
        assert_eq!(n, bound + 1);
        assert_eq!(on_disk(&p), n + SEQ_BLOCK - 1);
        // "崩溃"重启：新序号一定大于崩溃前发出过的任何序号
        let used = n + 10;
        for _ in 0..10 {
            s.next().unwrap();
        }
        assert_eq!(s.seq, used);
        assert!(SeqState::load(p.clone(), None).seq >= used);
        // 队列里有更大的序号时以队列为准
        let mut s3 = SeqState::load(p, Some(used + 50_000));
        assert_eq!(s3.next().unwrap(), used + 50_001);
        let _ = std::fs::remove_dir_all(&d);
    }
}
