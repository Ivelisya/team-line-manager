//! 主机遥测：/proc 解析 + 采样器（两种角色共用）

use crate::util::now_ms;
use fleet_proto::{HostSample, SystemInfo};
use std::net::IpAddr;

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

pub fn parse_load1(s: &str) -> f32 {
    s.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0.0)
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
    let cores = read("/proc/stat").lines().filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(|c| c.is_ascii_digit())).count() as u32;
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
    ifaces: Vec<String>,
    ifaces_at: std::time::Instant,
}

impl HostCollector {
    pub fn new() -> Self {
        let mut c = Self { prev_cpu: None, ifaces: vec![], ifaces_at: std::time::Instant::now() };
        c.refresh_ifaces();
        c.prev_cpu = parse_cpu(&read("/proc/stat"));
        c
    }

    fn refresh_ifaces(&mut self) {
        let names: Vec<String> = parse_net_dev(&read("/proc/net/dev")).into_iter().map(|x| x.0).collect();
        self.ifaces = pick_ifaces(&names, |n| std::path::Path::new(&format!("/sys/class/net/{n}/device")).exists());
        self.ifaces_at = std::time::Instant::now();
    }

    pub fn sample(&mut self) -> HostSample {
        if self.ifaces_at.elapsed().as_secs() > 300 {
            self.refresh_ifaces();
        }
        let cur = parse_cpu(&read("/proc/stat"));
        let pct = match (self.prev_cpu, cur) {
            (Some(p), Some(c)) => cpu_pct(p, c),
            _ => 0.0,
        };
        if cur.is_some() {
            self.prev_cpu = cur;
        }
        let (mem_total, avail) = parse_meminfo(&read("/proc/meminfo"));
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
            load1: parse_load1(&read("/proc/loadavg")),
            mem_used: mem_total.saturating_sub(avail),
            mem_total,
            disk_used,
            disk_total,
            net_rx_bytes: rx,
            net_tx_bytes: tx,
            tcp_conns: parse_curr_estab(&read("/proc/net/snmp")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn estab() {
        let s = "Ip: Forwarding\nIp: 1 64\nTcp: RtoAlgorithm RtoMin MaxConn CurrEstab InSegs\nTcp: 1 200 -1 42 999\n";
        assert_eq!(parse_curr_estab(s), 42);
    }
}
