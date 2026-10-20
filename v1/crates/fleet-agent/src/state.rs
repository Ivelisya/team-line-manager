//! 进程内共享状态 + data_dir 里的持久化小文件

use crate::config::Config;
use crate::util::{load_json, save_json};
use crate::xray::stats::StatsState;
use fleet_proto::{DesiredState, EntrySample, HostSample, ProbeSample, SystemInfo, XrayStatus};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
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

fn push_capped<T>(v: &mut Vec<T>, items: impl IntoIterator<Item = T>, cap: usize, what: &str) {
    v.extend(items);
    if v.len() > cap {
        let n = v.len() - cap;
        v.drain(..n);
        tracing::warn!("缓冲 {what} 超过上限 {cap}，丢弃最旧 {n} 条");
    }
}

impl Buffers {
    pub fn push_host(&mut self, s: HostSample) {
        push_capped(&mut self.host, [s], HOST_CAP, "host");
    }
    pub fn push_entries(&mut self, s: Vec<EntrySample>) {
        push_capped(&mut self.entries, s, ENTRY_CAP, "entries");
    }
    pub fn push_probe(&mut self, s: ProbeSample) {
        push_capped(&mut self.probes, [s], PROBE_CAP, "probes");
    }
    /// 落盘失败时把取走的东西放回去（放在最前面保持时间顺序）
    pub fn restore(&mut self, host: Vec<HostSample>, entries: Vec<EntrySample>, probes: Vec<ProbeSample>) {
        self.host.splice(0..0, host);
        self.entries.splice(0..0, entries);
        self.probes.splice(0..0, probes);
    }
}

pub struct Shared {
    pub cfg: Arc<Config>,
    pub buf: Mutex<Buffers>,
    /// 线路机：最近一次成功应用的期望状态（探测、流量统计据此知道有哪些入口）
    pub applied: RwLock<Option<DesiredState>>,
    pub applied_gen: AtomicU64,
}

impl Shared {
    pub fn new(cfg: Arc<Config>) -> Arc<Self> {
        Arc::new(Self { cfg, buf: Mutex::new(Buffers::default()), applied: RwLock::new(None), applied_gen: AtomicU64::new(0) })
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

/// 单调递增的上报序号，每次分配都 fsync。
/// 状态丢失时从当前 unix 毫秒起算（每个报告至少隔 1 s，所以必然大于之前用过的任何序号）。
pub struct SeqState {
    path: PathBuf,
    pub seq: u64,
}

impl SeqState {
    pub fn load(path: PathBuf, queue_newest: Option<u64>) -> Self {
        let base = load_json::<SeqFile>(&path).map(|f| f.seq).unwrap_or_else(|| crate::util::now_ms() as u64);
        Self { path, seq: base.max(queue_newest.unwrap_or(0)) }
    }
    pub fn next(&mut self) -> anyhow::Result<u64> {
        let n = self.seq + 1;
        save_json(&self.path, &SeqFile { seq: n }, true)?;
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
        let mut s2 = SeqState::load(p.clone(), None);
        assert_eq!(s2.next().unwrap(), b + 1);
        // 状态文件丢了：以队列里最大序号为下限，且起点是时间戳量级
        std::fs::remove_file(&p).unwrap();
        let s3 = SeqState::load(p, Some(u64::MAX / 2));
        assert_eq!(s3.seq, u64::MAX / 2);
        let _ = std::fs::remove_dir_all(&d);
    }
}
