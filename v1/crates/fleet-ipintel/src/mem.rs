//! 读 /proc/self/status 的内存字段（Linux）；mmap 的库页算在 RssFile，堆在 RssAnon。
use serde_json::{json, Value};

pub fn rss() -> Value {
    let Ok(s) = std::fs::read_to_string("/proc/self/status") else { return Value::Null };
    let kb = |k: &str| {
        s.lines()
            .find(|l| l.starts_with(k))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
    };
    json!({"vm_rss_kb": kb("VmRSS:"), "rss_anon_kb": kb("RssAnon:"), "rss_file_kb": kb("RssFile:"), "vm_hwm_kb": kb("VmHWM:"), "vm_size_kb": kb("VmSize:")})
}
