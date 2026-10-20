//! 按类别累计 CPU 时间（线程 CPU 时钟，不含等待）：/system 的 `cpu_by`。只统计阻塞线程里的工作（喂数各阶段、渲染、评估…），
//! 其余（异步请求处理、SSE、ingest 解析等）= 进程总 CPU − 已统计。
use std::collections::BTreeMap;
use std::sync::Mutex;

static ACC: Mutex<BTreeMap<String, (u64, f64)>> = Mutex::new(BTreeMap::new());

/// 当前线程已用 CPU（毫秒）
pub fn thread_ms() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime 只写入 ts
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 * 1000.0 + ts.tv_nsec as f64 / 1e6
}

pub fn add(cat: &str, ms: f64) {
    let mut g = ACC.lock().unwrap();
    let e = g.entry(cat.to_string()).or_insert((0, 0.0));
    e.0 += 1;
    e.1 += ms;
}

/// 作用域计时：离开作用域时把这段线程 CPU 记到类别上
pub struct Span(String, f64);
impl Span {
    pub fn new(cat: impl Into<String>) -> Span {
        Span(cat.into(), thread_ms())
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        add(&self.0, thread_ms() - self.1);
    }
}

pub fn snapshot() -> serde_json::Value {
    let g = ACC.lock().unwrap();
    let mut v: Vec<(&String, &(u64, f64))> = g.iter().collect();
    v.sort_by(|a, b| b.1 .1.partial_cmp(&a.1 .1).unwrap_or(std::cmp::Ordering::Equal));
    serde_json::Value::Array(v.into_iter().map(|(k, (n, ms))| serde_json::json!({"cat": k, "count": n, "cpu_ms": ms.round()})).collect())
}
