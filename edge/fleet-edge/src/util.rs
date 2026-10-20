//! 杂项：时间、随机数（抖动用，不用于密码学）、信号屏蔽与可被信号打断的睡眠、原子写文件。
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now_f64() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// xorshift64*，种子来自时间与 pid；只用于退避抖动和错峰。
pub struct Rng(u64);
impl Rng {
    pub fn new(extra: u64) -> Rng {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        Rng(t ^ ((std::process::id() as u64) << 32) ^ extra.wrapping_mul(0x9E3779B97F4A7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    /// [0, 1)
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 屏蔽 SIGTERM/SIGINT/SIGHUP，改由 `wait_signal` 同步接收（单线程，无信号处理函数）。
pub fn block_signals() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGHUP);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// 睡 `d`，期间收到 SIGTERM/SIGINT 立即返回 true；SIGHUP 忽略（继续睡剩余时间）。
pub fn wait_signal(d: Duration) -> bool {
    let deadline = std::time::Instant::now() + d;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        let ts = libc::timespec { tv_sec: left.as_secs() as _, tv_nsec: left.subsec_nanos() as libc::c_long };
        let r = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::sigaddset(&mut set, libc::SIGHUP);
            libc::sigtimedwait(&set, std::ptr::null_mut(), &ts)
        };
        if r == libc::SIGTERM || r == libc::SIGINT {
            return true;
        }
        // -1：超时（EAGAIN）或 EINTR；SIGHUP：继续
        if r < 0 {
            let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if e == libc::EAGAIN {
                return false;
            }
        }
    }
}

/// 写临时文件再 rename（同目录），权限 0600。不 fsync：本进程崩溃不丢页缓存，
/// 断电会换 boot_id，序号语义不受影响。
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(data)?;
    }
    std::fs::rename(&tmp, path)
}

pub fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}
