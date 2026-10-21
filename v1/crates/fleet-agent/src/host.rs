//! 主机遥测：/proc 解析 + 采样器（两种角色共用）。
//! 容器里（cgroup v2、非根 cgroup）CPU 与内存改读 cgroup：/proc/stat 是宿主机的，/proc/meminfo 没有 lxcfs 时也是。

use crate::util::now_ms;
use fleet_proto::{HostSample, SystemInfo};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Instant;

// ── 纯解析函数（便于测试）──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

/// /proc/stat 首行：user nice system idle iowait irq softirq steal …
pub fn parse_cpu(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().next()?;
    let mut it = line.split_whitespace();
    if it.next()? != "cpu" {
        return None;
    }
    let v: Vec<u64> = it.take(8).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 5 {
        return None;
    }
    Some(CpuTimes { total: v.iter().sum(), idle: v[3] + v[4] })
}

pub fn cpu_pct(prev: CpuTimes, cur: CpuTimes) -> f32 {
    let dt = cur.total.saturating_sub(prev.total);
    if dt == 0 {
        return 0.0;
    }
    let di = cur.idle.saturating_sub(prev.idle).min(dt);
    (100.0 * (dt - di) as f64 / dt as f64) as f32
}

/// /proc/loadavg（"0.52 0.58 0.59 2/1323 12345"）的 1 分钟负载。
/// 第 4 列的 total 是全系统的调度实体（线程）数：容器（LXC 等，没有 lxcfs 虚拟化 loadavg）里读到的是宿主机的 loadavg，
/// total 远大于本命名空间里能看到的线程数，这个负载不代表本机，返回 None。
pub fn parse_load1(loadavg: &str, visible_threads: usize) -> Option<f32> {
    let mut f = loadavg.split_whitespace();
    let load1 = f.next()?.parse().ok()?;
    let total: usize = f.nth(2)?.split_once('/')?.1.parse().ok()?;
    (total <= 2 * visible_threads + 10).then_some(load1)
}

/// 本命名空间里能看到的线程数：<proc>/<pid>/task 的条目合计（loadavg 的 total 数的是线程，不是进程）
pub fn visible_threads(proc: &std::path::Path) -> usize {
    let Ok(rd) = std::fs::read_dir(proc) else { return 0 };
    rd.flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())))
        .map(|e| std::fs::read_dir(e.path().join("task")).map_or(0, |t| t.count()))
        .sum()
}

/// 返回 (total, available) 字节
pub fn parse_meminfo(s: &str) -> (u64, u64) {
    let mut total = 0;
    let (mut avail, mut free, mut buffers, mut cached) = (None, 0, 0, 0);
    for l in s.lines() {
        let mut it = l.split_whitespace();
        let (Some(k), Some(v)) = (it.next(), it.next()) else { continue };
        let v: u64 = v.parse().unwrap_or(0) * 1024;
        match k {
            "MemTotal:" => total = v,
            "MemAvailable:" => avail = Some(v),
            "MemFree:" => free = v,
            "Buffers:" => buffers = v,
            "Cached:" => cached = v,
            _ => {}
        }
    }
    (total, avail.unwrap_or(free + buffers + cached))
}

/// /proc/net/dev：返回 (iface, rx, tx)
pub fn parse_net_dev(s: &str) -> Vec<(String, u64, u64)> {
    s.lines()
        .skip(2)
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let f: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if f.len() < 9 {
                return None;
            }
            Some((name.trim().to_string(), f[0], f[8]))
        })
        .collect()
}

/// /proc/net/snmp 的 Tcp: CurrEstab
pub fn parse_curr_estab(s: &str) -> u32 {
    let mut keys: Option<Vec<&str>> = None;
    for l in s.lines() {
        let Some(rest) = l.strip_prefix("Tcp:") else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        match &keys {
            None => keys = Some(f),
            Some(k) => {
                let i = k.iter().position(|x| *x == "CurrEstab");
                return i.and_then(|i| f.get(i)).and_then(|x| x.parse().ok()).unwrap_or(0);
            }
        }
    }
    0
}

fn virtual_iface(n: &str) -> bool {
    ["lo", "docker", "veth", "br-", "virbr", "tun", "tap", "wg", "ifb", "dummy"].iter().any(|p| n.starts_with(p))
}

/// 选取参与求和的网卡：有物理设备的优先（/sys/class/net/<n>/device），没有（容器、虚拟机里常见）则取所有非虚拟网卡。
pub fn pick_ifaces(all: &[String], has_device: impl Fn(&str) -> bool) -> Vec<String> {
    let phys: Vec<String> = all.iter().filter(|n| *n != "lo" && has_device(n)).cloned().collect();
    if !phys.is_empty() {
        return phys;
    }
    all.iter().filter(|n| !virtual_iface(n)).cloned().collect()
}

// ── cgroup v2（容器）──────────────────────────────────────────────────────────

/// cpu.max（"200000 100000" = 配额/周期）折成核数；"max …" 不限为 None
pub fn parse_cpu_max(s: &str) -> Option<f64> {
    let mut f = s.split_whitespace();
    let quota: f64 = f.next()?.parse().ok()?;
    let period: f64 = f.next()?.parse().ok()?;
    (period > 0.0).then_some(quota / period)
}

/// cpu.stat 的 usage_usec
pub fn parse_usage_usec(s: &str) -> Option<u64> {
    s.lines().find_map(|l| l.strip_prefix("usage_usec ")).and_then(|v| v.trim().parse().ok())
}

/// memory.stat 里某一项（字节）
fn stat_field(s: &str, key: &str) -> u64 {
    s.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok()).unwrap_or(0)
}

/// 本 cgroup 的 CPU 与内存
pub struct Cgroup {
    root: PathBuf,
    /// 上次的 (usage_usec, 采样时刻)
    prev: Option<(u64, Instant)>,
}

impl Cgroup {
    /// root（一般是 /sys/fs/cgroup）下有 memory.current 才用：根 cgroup 没有这个文件，宿主机、虚拟机照旧读 /proc
    pub fn detect(root: &Path) -> Option<Self> {
        root.join("memory.current").exists().then(|| {
            let mut c = Self { root: root.to_path_buf(), prev: None };
            c.prev = c.usage().map(|u| (u, Instant::now()));
            c
        })
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(name)).unwrap_or_default()
    }

    fn usage(&self) -> Option<u64> {
        parse_usage_usec(&self.read("cpu.stat"))
    }

    /// (used, total) 字节：used = memory.current − inactive_file（页缓存里可回收的不算）；
    /// total = memory.max，不限（"max"）时取 /proc/meminfo 的 MemTotal（host_total）
    pub fn mem(&self, host_total: u64) -> (u64, u64) {
        let cur: u64 = self.read("memory.current").trim().parse().unwrap_or(0);
        let used = cur.saturating_sub(stat_field(&self.read("memory.stat"), "inactive_file"));
        let total = self.read("memory.max").trim().parse().map_or(host_total, |m: u64| m.min(host_total));
        (used, total)
    }

    /// 两次采样间 CPU 用量 / (墙钟 × 可用核数)；可用核数 = cpu.max 配额，不限时为 parallelism
    pub fn cpu_pct(&mut self, parallelism: f64) -> f32 {
        let (Some(u), now) = (self.usage(), Instant::now()) else { return 0.0 };
        let pct = match self.prev {
            Some((pu, pt)) => {
                let cores = parse_cpu_max(&self.read("cpu.max")).unwrap_or(parallelism);
                let wall = now.duration_since(pt).as_micros() as f64 * cores;
                if wall > 0.0 { (100.0 * u.saturating_sub(pu) as f64 / wall).min(100.0) as f32 } else { 0.0 }
            }
            None => 0.0,
        };
        self.prev = Some((u, now));
        pct
    }
}

// ── 系统信息 ────────────────────────────────────────────────────────────────

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

pub fn boot_id() -> String {
    let s = read("/proc/sys/kernel/random/boot_id");
    let s = s.trim();
    if s.is_empty() { "unknown".into() } else { s.to_string() }
}

pub fn collect_info() -> SystemInfo {
    let os = read("/etc/os-release")
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_default();
    let cpuinfo = read("/proc/cpuinfo");
    let cpu_model = cpuinfo
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    // 可用核数（认 cgroup 配额与 CPU 亲和性）：容器里 /proc/stat 列的是宿主机全部 CPU
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    let (ipv4, ipv6) = local_ips();
    SystemInfo {
        hostname: read("/proc/sys/kernel/hostname").trim().to_string(),
        kernel: read("/proc/sys/kernel/osrelease").trim().to_string(),
        os,
        cpu_model,
        cpu_cores: cores,
        arch: std::env::consts::ARCH.to_string(),
        ipv4,
        ipv6,
    }
}

/// getifaddrs：非回环、非链路本地的地址，排序去重以便比较"是否变化"
fn local_ips() -> (Vec<IpAddr>, Vec<IpAddr>) {
    let (mut v4, mut v6) = (Vec::new(), Vec::new());
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return (v4, v6);
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let a = &*cur;
            cur = a.ifa_next;
            if a.ifa_addr.is_null() || a.ifa_flags & (libc::IFF_LOOPBACK as u32) != 0 || a.ifa_flags & (libc::IFF_UP as u32) == 0 {
                continue;
            }
            match (*a.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let sa = &*(a.ifa_addr as *const libc::sockaddr_in);
                    let ip = std::net::Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr));
                    if !ip.is_link_local() {
                        v4.push(IpAddr::V4(ip));
                    }
                }
                libc::AF_INET6 => {
                    let sa = &*(a.ifa_addr as *const libc::sockaddr_in6);
                    let ip = std::net::Ipv6Addr::from(sa.sin6_addr.s6_addr);
                    if (ip.segments()[0] & 0xffc0) != 0xfe80 {
                        v6.push(IpAddr::V6(ip));
                    }
                }
                _ => {}
            }
        }
        libc::freeifaddrs(ifap);
    }
    v4.sort();
    v4.dedup();
    v6.sort();
    v6.dedup();
    (v4, v6)
}

fn disk_root() -> (u64, u64) {
    let c = std::ffi::CString::new("/").unwrap();
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return (0, 0);
    }
    let fr = s.f_frsize as u64;
    let total = s.f_blocks as u64 * fr;
    let used = (s.f_blocks as u64).saturating_sub(s.f_bfree as u64) * fr;
    (used, total)
}

// ── 采样器 ──────────────────────────────────────────────────────────────────

pub struct HostCollector {
    prev_cpu: Option<CpuTimes>,
    /// 在容器里：CPU、内存读 cgroup
    cg: Option<Cgroup>,
    ifaces: Vec<String>,
    /// 本命名空间里能看到的线程数（判断 loadavg 是不是宿主机的）：遍历 /proc 较重，随网卡一起刷新
    threads: usize,
    ifaces_at: std::time::Instant,
}

impl HostCollector {
    pub fn new() -> Self {
        let cg = Cgroup::detect(Path::new("/sys/fs/cgroup"));
        let mut c = Self { prev_cpu: None, cg, ifaces: vec![], threads: 0, ifaces_at: std::time::Instant::now() };
        c.refresh_ifaces();
        c.prev_cpu = parse_cpu(&read("/proc/stat"));
        c
    }

    fn refresh_ifaces(&mut self) {
        let names: Vec<String> = parse_net_dev(&read("/proc/net/dev")).into_iter().map(|x| x.0).collect();
        self.ifaces = pick_ifaces(&names, |n| std::path::Path::new(&format!("/sys/class/net/{n}/device")).exists());
        self.threads = visible_threads(std::path::Path::new("/proc"));
        self.ifaces_at = std::time::Instant::now();
    }

    pub fn sample(&mut self) -> HostSample {
        if self.ifaces_at.elapsed().as_secs() > 300 {
            self.refresh_ifaces();
        }
        let (host_total, avail) = parse_meminfo(&read("/proc/meminfo"));
        let (pct, mem_used, mem_total) = match &mut self.cg {
            Some(cg) => {
                let par = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
                let (used, total) = cg.mem(host_total);
                (cg.cpu_pct(par), used, total)
            }
            None => (proc_cpu_pct(&mut self.prev_cpu), host_total.saturating_sub(avail), host_total),
        };
        let (disk_used, disk_total) = disk_root();
        let (mut rx, mut tx) = (0u64, 0u64);
        for (n, r, t) in parse_net_dev(&read("/proc/net/dev")) {
            if self.ifaces.contains(&n) {
                rx += r;
                tx += t;
            }
        }
        HostSample {
            ts_ms: now_ms(),
            cpu_pct: pct,
            load1: parse_load1(&read("/proc/loadavg"), self.threads),
            mem_used,
            mem_total,
            disk_used,
            disk_total,
            net_rx_bytes: rx,
            net_tx_bytes: tx,
            tcp_conns: parse_curr_estab(&read("/proc/net/snmp")),
        }
    }
}

/// /proc/stat 两次采样间的 CPU 占用
fn proc_cpu_pct(prev: &mut Option<CpuTimes>) -> f32 {
    let cur = parse_cpu(&read("/proc/stat"));
    let pct = match (*prev, cur) {
        (Some(p), Some(c)) => cpu_pct(p, c),
        _ => 0.0,
    };
    if cur.is_some() {
        *prev = cur;
    }
    pct
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load1_host_vs_container() {
        // 宿主机：total 与可见线程数相当
        assert_eq!(parse_load1("0.52 0.58 0.59 3/1323 4242
", 1310), Some(0.52));
        // LXC 容器：loadavg 是宿主机的（1323 个线程），容器里只看得到 40 个
        assert_eq!(parse_load1("0.52 0.58 0.59 3/1323 4242
", 40), None);
        // 很小的系统：+10 的余量
        assert_eq!(parse_load1("0.01 0.02 0.00 1/30 99", 12), Some(0.01));
        assert_eq!(parse_load1("", 40), None);
        assert_eq!(parse_load1("0.5 0.5 0.5", 40), None);
    }

    #[test]
    fn count_visible_threads() {
        // 假 /proc：两个进程（1 个、3 个线程），非数字目录与没有 task 的条目不算
        let root = std::env::temp_dir().join(format!("fleet-proc-{}", std::process::id()));
        for d in ["1/task/1", "77/task/77", "77/task/78", "77/task/79", "self/task/1", "sys/kernel", "123"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let n = visible_threads(&root);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(n, 4);
        assert_eq!(parse_load1("0.1 0.1 0.1 1/4 77", n), Some(0.1));
    }

    #[test]
    fn cpu_delta() {
        let a = parse_cpu("cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 1 2 3").unwrap();
        let b = parse_cpu("cpu  200 0 200 800 200 0 0 0 0 0").unwrap();
        // total 增 400，idle 增 200 → 50%
        assert_eq!(a.total, 1000);
        assert!((cpu_pct(a, b) - 50.0).abs() < 0.01);
        assert_eq!(cpu_pct(a, a), 0.0);
    }

    #[test]
    fn meminfo_prefers_available() {
        let (t, a) = parse_meminfo("MemTotal: 1000 kB\nMemFree: 100 kB\nMemAvailable: 400 kB\nBuffers: 1 kB\nCached: 2 kB\n");
        assert_eq!((t, a), (1000 * 1024, 400 * 1024));
        let (_, a) = parse_meminfo("MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 10 kB\nCached: 20 kB\n");
        assert_eq!(a, 130 * 1024);
    }

    const NETDEV: &str = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo: 500 5 0 0 0 0 0 0 500 5 0 0 0 0 0 0\n  eth0: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0\n";

    #[test]
    fn net_dev_and_ifaces() {
        let v = parse_net_dev(NETDEV);
        assert_eq!(v, vec![("lo".into(), 500, 500), ("eth0".into(), 1000, 2000)]);
        let names: Vec<String> = v.into_iter().map(|x| x.0).collect();
        assert_eq!(pick_ifaces(&names, |_| false), vec!["eth0".to_string()]);
        let all = vec!["lo".to_string(), "eth0".to_string(), "docker0".to_string(), "ens3".to_string()];
        assert_eq!(pick_ifaces(&all, |n| n == "ens3"), vec!["ens3".to_string()]);
    }

    #[test]
    fn cgroup_v2_cpu_and_mem() {
        assert_eq!(parse_cpu_max("200000 100000\n"), Some(2.0));
        assert_eq!(parse_cpu_max("50000 100000"), Some(0.5));
        assert_eq!(parse_cpu_max("max 100000\n"), None);
        assert_eq!(parse_usage_usec("usage_usec 16189068909\nuser_usec 14881704451\n"), Some(16_189_068_909));

        // 假 cgroup 目录：根 cgroup（没有 memory.current）不用
        let root = std::env::temp_dir().join(format!("fleet-cg-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        assert!(Cgroup::detect(&root).is_none());
        let w = |n: &str, v: &str| std::fs::write(root.join(n), v).unwrap();
        w("memory.current", "3000\n");
        w("memory.stat", "anon 1000\nfile 2500\nactive_file 1500\ninactive_file 1000\n");
        w("memory.max", "max\n");
        w("cpu.max", "max 100000\n");
        w("cpu.stat", "usage_usec 1000\nuser_usec 800\n");
        let mut cg = Cgroup::detect(&root).unwrap();
        // 不限内存：总量取宿主机（/proc/meminfo）的；有上限取上限
        assert_eq!(cg.mem(8000), (2000, 8000));
        w("memory.max", "4000\n");
        assert_eq!(cg.mem(8000), (2000, 4000));

        // CPU：用量增量 / (墙钟 × 核数)。2 核配额下用满 1 核 ≈ 50%
        w("cpu.max", "200000 100000\n");
        let t0 = cg.prev.unwrap().1;
        std::thread::sleep(std::time::Duration::from_millis(200));
        let used = t0.elapsed().as_micros() as u64; // 这段时间里用满 1 核
        w("cpu.stat", &format!("usage_usec {}\n", 1000 + used));
        let p = cg.cpu_pct(8.0);
        assert!((p - 50.0).abs() < 5.0, "{p}");
        // 不限配额：按 parallelism 算，4 核里用满 2 核 ≈ 50%
        w("cpu.max", "max 100000\n");
        let (u1, t1) = cg.prev.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        w("cpu.stat", &format!("usage_usec {}\n", u1 + 2 * t1.elapsed().as_micros() as u64));
        let p = cg.cpu_pct(4.0);
        assert!((p - 50.0).abs() < 5.0, "{p}");
        // 没有新用量为 0
        assert_eq!(cg.cpu_pct(4.0), 0.0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn estab() {
        let s = "Ip: Forwarding\nIp: 1 64\nTcp: RtoAlgorithm RtoMin MaxConn CurrEstab InSegs\nTcp: 1 200 -1 42 999\n";
        assert_eq!(parse_curr_estab(s), 42);
    }
}
