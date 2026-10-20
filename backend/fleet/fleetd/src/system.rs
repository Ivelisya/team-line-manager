//! 自身资源读数（/proc/self）：CPU%（相对一个核）、RSS、线程数。
use std::collections::VecDeque;
use std::time::Instant;

const CLK_TCK: f64 = 100.0;

#[derive(Default)]
pub struct SysStats {
    last: Option<(u64, Instant)>,
    pub cpu_pct: f64,
    pub rss_kb: u64,
    /// 堆等匿名内存；RssFile 是 mmap 的文件页（legacy.db / ingest.db 的 SQLite mmap、ipintel 离线库；干净页，可回收）
    pub rss_anon_kb: u64,
    pub rss_file_kb: u64,
    pub threads: u64,
    /// 最近 30 个采样（10 s 一次 → 5 min）
    pub history: VecDeque<f64>,
    pub total_cpu_s: f64,
}

pub fn read_proc_self() -> Option<(u64, u64, u64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let after = stat.rsplit_once(')')?.1;
    let f: Vec<&str> = after.split_whitespace().collect();
    // after ')' : state(0) ppid(1) ... utime(11) stime(12) ... num_threads(17)
    let ticks = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
    let threads = f.get(17)?.parse::<u64>().ok()?;
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let rss = status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|x| x.parse::<u64>().ok())
        .unwrap_or(0);
    Some((ticks, rss, threads))
}

impl SysStats {
    pub fn sample(&mut self) {
        let Some((ticks, rss, threads)) = read_proc_self() else { return };
        let now = Instant::now();
        if let Some((t0, i0)) = self.last {
            let dt = now.duration_since(i0).as_secs_f64();
            if dt > 0.5 {
                self.cpu_pct = (ticks.saturating_sub(t0)) as f64 / CLK_TCK / dt * 100.0;
                self.history.push_back(self.cpu_pct);
                while self.history.len() > 30 {
                    self.history.pop_front();
                }
            }
        }
        self.total_cpu_s = ticks as f64 / CLK_TCK;
        self.last = Some((ticks, now));
        self.rss_kb = rss;
        if let Ok(st) = std::fs::read_to_string("/proc/self/status") {
            let get = |k: &str| st.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|x| x.parse().ok()).unwrap_or(0);
            self.rss_anon_kb = get("RssAnon:");
            self.rss_file_kb = get("RssFile:");
        }
        self.threads = threads;
    }

    pub fn avg(&self) -> Option<f64> {
        if self.history.is_empty() {
            None
        } else {
            Some(self.history.iter().sum::<f64>() / self.history.len() as f64)
        }
    }
}

/// /proc/self/status 里的 RssAnon（kB）
pub fn rss_anon_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|st| st.lines().find(|l| l.starts_with("RssAnon:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|x| x.parse().ok()))
        .unwrap_or(0)
}

extern "C" {
    fn mi_collect(force: bool);
    fn mi_process_info(elapsed: *mut usize, user: *mut usize, system: *mut usize, current_rss: *mut usize, peak_rss: *mut usize,
        current_commit: *mut usize, peak_commit: *mut usize, page_faults: *mut usize);
}

/// mimalloc 把空闲页还给系统（force：包括各线程缓存）
pub fn allocator_collect() {
    unsafe { mi_collect(true) }
}

/// mimalloc 的提交量（字节）：(当前, 峰值)
pub fn allocator_commit() -> (usize, usize) {
    let mut v = [0usize; 8];
    unsafe {
        let [a, b, c, d, e, f, g, h] = &mut v;
        mi_process_info(a, b, c, d, e, f, g, h);
    }
    (v[5], v[6])
}

/// SQLite 进程内全部连接的堆内存（字节）：(当前, 峰值)
pub fn sqlite_memory() -> (i64, i64) {
    unsafe { (rusqlite::ffi::sqlite3_memory_used(), rusqlite::ffi::sqlite3_memory_highwater(0)) }
}
