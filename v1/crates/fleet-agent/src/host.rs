//! 主机遥测：/proc 解析 + 采样器（两种角色共用）。
//! 容器里（cgroup v2、非根 cgroup）CPU 与内存改读 cgroup：/proc/stat 是宿主机的，/proc/meminfo 没有 lxcfs 时也是。

use crate::util::{load_json, now_ms, save_json};
use fleet_proto::{HostSample, SystemInfo};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    [
        "lo", "docker", "veth", "br-", "virbr", "lxdbr", "lxcbr", "incusbr", "cni", "flannel", "vxlan", "geneve", "cilium", "kube", "tun", "tap", "wg",
        "tailscale", "zt", "gre", "sit", "ip6tnl", "ipip", "ifb", "dummy",
    ]
    .iter()
    .any(|p| n.starts_with(p))
}

/// 默认路由所在的网卡：/proc/net/route 里 Destination、Mask 都是 00000000 的行，/proc/net/ipv6_route 里目的为 ::/0 的行。
/// lo 上的 ::/0 是 unreachable 路由，和其它虚拟网卡一样由调用方排除
pub fn parse_default_route(route: &str, ipv6_route: &str) -> Vec<String> {
    let v4 = route.lines().skip(1).filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() > 7 && f[1] == "00000000" && f[7] == "00000000").then(|| f[0].to_string())
    });
    let v6 = ipv6_route.lines().filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() > 9 && f[0].bytes().all(|b| b == b'0') && f[1] == "00").then(|| f[9].to_string())
    });
    let mut v: Vec<String> = v4.chain(v6).collect();
    v.sort();
    v.dedup();
    v
}

/// 同机 IP 节点的 ssserver 配置所在目录（中控 jobs/scripts.rs 写的 /etc/shadowsocks-rust/<端口>.json）
const SS_CONF_DIR: &str = "/etc/shadowsocks-rust";

/// 同机 IP 节点的出站地址（<端口>.json 里的 outbound_bind_addr）。1:1 NAT 的供应商（Blossom）把附加公网 IP 映射到第二块网卡的内网地址上：
/// 那块网卡只有内网地址，按「有公网地址」挑不到，可 IP 节点的流量全走它、供应商照样计进这台的额度。配着这些地址的网卡也算「公网」
fn ss_bind_addrs(dir: &Path) -> Vec<IpAddr> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    rd.flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".json") && n.starts_with(|c: char| c.is_ascii_digit())))
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(e.path()).ok()?).ok())
        .filter_map(|v| v["outbound_bind_addr"].as_str()?.parse().ok())
        .collect()
}

/// 选取计量的网卡（供应商按它计费的那几块）：
/// 1. 默认路由所在的网卡 ∪ 有物理设备（/sys/class/net/<n>/device）且配有公网地址的网卡（附加公网 IP 配在第二块网卡上时；
///    1:1 NAT 的附加 IP 只有内网地址，配着同机 IP 节点出站地址的也算，见 `ss_bind_addrs`）；
///    只有内网地址的 VPC 网卡不算，默认路由指向虚拟网卡（全局 WireGuard 的 wg0 等）时跳过它；
/// 2. 以上为空：有物理设备的网卡，再没有（容器、OpenVZ）则取所有非虚拟网卡。
///
/// agent.toml 写了 nic_ifaces 时不走这里
pub fn pick_ifaces(all: &[String], default_route: &[String], has_device: impl Fn(&str) -> bool, has_public: impl Fn(&str) -> bool) -> Vec<String> {
    let v: Vec<String> =
        all.iter().filter(|n| !virtual_iface(n) && (default_route.contains(n) || (has_device(n) && has_public(n)))).cloned().collect();
    if !v.is_empty() {
        return v;
    }
    let phys: Vec<String> = all.iter().filter(|n| *n != "lo" && has_device(n)).cloned().collect();
    if !phys.is_empty() {
        return phys;
    }
    all.iter().filter(|n| !virtual_iface(n)).cloned().collect()
}

/// 网卡流量的持久累计值（data_dir/nic.json）：自 agent 首次安装起单调累加选中网卡的增量，跨 agent 重启、跨开机都接得上。
/// 网卡集合会变（每 300 s 重新挑选，容器里网卡也会增减）：所以按网卡分别记上一次读数、只累加各自的增量，
/// 而不是直接上报各网卡开机累计值之和（网卡一少总和变小、一多新网卡的历史也被算进来）。
/// 存盘稍旧也不要紧：下次启动按旧读数求差，结果一样，不重复也不丢
#[derive(Default, Serialize, Deserialize)]
pub struct NicMeter {
    boot_id: String,
    /// 网卡 → 上一次的 (rx, tx) 内核累计读数
    ifaces: HashMap<String, (u64, u64)>,
    pub rx: u64,
    pub tx: u64,
}

impl NicMeter {
    /// 启动时载入。同一次开机（agent 重启）：按文件里各网卡读数求差，停机期间的流量也算上；
    /// 开机了（boot_id 变了）：选中网卡的当前内核读数全部计入（开机以来的量），只丢关机前最后一次存盘之后的那一段；
    /// 文件不存在（首次安装、状态丢失）：只记基线，从 0 起——不能把开机以来的读数当初值，否则升级那一刻中控看到一个 TB 级的跳变
    pub fn load(path: &Path, boot_id: &str, readings: impl IntoIterator<Item = (String, u64, u64)>) -> Self {
        let Some(mut m) = load_json::<NicMeter>(path) else {
            let mut m = Self { boot_id: boot_id.to_string(), ..Default::default() };
            m.absorb(readings, false);
            return m;
        };
        let rebooted = m.boot_id != boot_id;
        m.boot_id = boot_id.to_string();
        m.absorb(readings, rebooted);
        m
    }

    /// 喂入这一轮选中网卡的内核读数 (网卡, rx, tx)：新选中的网卡只记基线（它的历史不属于我们），不再选中的丢掉基线；
    /// 某网卡读数变小 = 它的计数器重置了（网卡重建等），增量取新值
    pub fn update(&mut self, readings: impl IntoIterator<Item = (String, u64, u64)>) {
        self.absorb(readings, false);
    }

    /// from_zero：开机后第一次，各网卡的前值按 0 算
    fn absorb(&mut self, readings: impl IntoIterator<Item = (String, u64, u64)>, from_zero: bool) {
        let delta = |cur: u64, prev: u64| if cur >= prev { cur - prev } else { cur };
        let mut next = HashMap::new();
        for (n, r, t) in readings {
            let prev = if from_zero { Some((0, 0)) } else { self.ifaces.get(&n).copied() };
            if let Some((pr, pt)) = prev {
                self.rx += delta(r, pr);
                self.tx += delta(t, pt);
            }
            next.insert(n, (r, t));
        }
        self.ifaces = next;
    }

    /// 不 fsync：异常断电只丢最后一次存盘之后的量
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        save_json(path, self, false)
    }
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

/// nic_ifaces：计量用的网卡（`HostCollector::ifaces`）
pub fn collect_info(nic_ifaces: &[String]) -> SystemInfo {
    let os = read("/etc/os-release")
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_default();
    // 可用核数（认 cgroup 配额与 CPU 亲和性）：容器里 /proc/stat 列的是宿主机全部 CPU
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    let (ipv4, ipv6) = local_ips();
    SystemInfo {
        hostname: read("/proc/sys/kernel/hostname").trim().to_string(),
        kernel: read("/proc/sys/kernel/osrelease").trim().to_string(),
        os,
        cpu_cores: cores,
        arch: std::env::consts::ARCH.to_string(),
        ipv4,
        ipv6,
        nic_ifaces: nic_ifaces.to_vec(),
    }
}

/// 非回环、非链路本地的地址，排序去重以便比较"是否变化"
fn local_ips() -> (Vec<IpAddr>, Vec<IpAddr>) {
    let (mut v4, mut v6): (Vec<IpAddr>, Vec<IpAddr>) = if_addrs().into_iter().map(|x| x.1).partition(|ip| ip.is_ipv4());
    v4.sort();
    v4.dedup();
    v6.sort();
    v6.dedup();
    (v4, v6)
}

/// getifaddrs：已启用的非回环网卡上、非链路本地的 (网卡, 地址)
fn if_addrs() -> Vec<(String, IpAddr)> {
    let mut v = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return v;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let a = &*cur;
            cur = a.ifa_next;
            if a.ifa_addr.is_null() || a.ifa_flags & (libc::IFF_LOOPBACK as u32) != 0 || a.ifa_flags & (libc::IFF_UP as u32) == 0 {
                continue;
            }
            let ip = match (*a.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let sa = &*(a.ifa_addr as *const libc::sockaddr_in);
                    let ip = std::net::Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr));
                    (!ip.is_link_local()).then_some(IpAddr::V4(ip))
                }
                libc::AF_INET6 => {
                    let sa = &*(a.ifa_addr as *const libc::sockaddr_in6);
                    let ip = std::net::Ipv6Addr::from(sa.sin6_addr.s6_addr);
                    ((ip.segments()[0] & 0xffc0) != 0xfe80).then_some(IpAddr::V6(ip))
                }
                _ => None,
            };
            if let Some(ip) = ip {
                v.push((std::ffi::CStr::from_ptr(a.ifa_name).to_string_lossy().into_owned(), ip));
            }
        }
        libc::freeifaddrs(ifap);
    }
    v
}

fn disk_root() -> (u64, u64) {
    let c = std::ffi::CString::new("/").unwrap();
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return (0, 0);
    }
    // 同 df 的 Use%：总量 = 已用 + 普通进程还能用的（f_bavail），不含 root 预留块（ext4 默认 5%），写满时显示 100% 而不是约 95%
    let fr = s.f_frsize as u64;
    let used = (s.f_blocks as u64).saturating_sub(s.f_bfree as u64) * fr;
    let total = used + s.f_bavail as u64 * fr;
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
    /// 在容器里才用：/proc/meminfo 的 MemTotal（memory.max 不限时的总量）与可用核数（cpu.max 不限时）。
    /// 很少变，随网卡一起刷新（available_parallelism 每次调用都重读 cgroup 文件）
    host_mem_total: u64,
    parallelism: f64,
    ifaces_at: std::time::Instant,
    net: NicMeter,
    /// nic.json；None（`once`）时不落盘、从 0 起
    nic_path: Option<PathBuf>,
    /// agent.toml 的 nic_ifaces：写了就只计这些网卡
    fixed_ifaces: Vec<String>,
}

impl HostCollector {
    pub fn new(nic_path: Option<PathBuf>, fixed_ifaces: Vec<String>) -> Self {
        let cg = Cgroup::detect(Path::new("/sys/fs/cgroup"));
        let mut c = Self {
            prev_cpu: None,
            cg,
            ifaces: vec![],
            threads: 0,
            host_mem_total: 0,
            parallelism: 1.0,
            ifaces_at: std::time::Instant::now(),
            net: NicMeter::default(),
            nic_path,
            fixed_ifaces,
        };
        c.refresh_slow();
        c.prev_cpu = parse_cpu(&read("/proc/stat"));
        // 载入累计值并记下各网卡基线，第一个样本就有增量
        let readings = c.net_readings();
        match &c.nic_path {
            Some(p) => c.net = NicMeter::load(p, &boot_id(), readings),
            None => c.net.update(readings),
        }
        c
    }

    /// 选中网卡的内核累计读数
    fn net_readings(&self) -> Vec<(String, u64, u64)> {
        parse_net_dev(&read("/proc/net/dev")).into_iter().filter(|(n, _, _)| self.ifaces.contains(n)).collect()
    }

    /// 计量用的网卡（随 SystemInfo 上报）
    pub fn ifaces(&self) -> &[String] {
        &self.ifaces
    }

    /// 网卡累计值存盘（每 60 s 一次 + 关停时）
    pub fn save_nic(&self) {
        if let Some(p) = &self.nic_path {
            if let Err(e) = self.net.save(p) {
                tracing::warn!("保存 {} 失败：{e:#}", p.display());
            }
        }
    }

    /// 每 300 s 刷新一次的：网卡集合、可见线程数，容器里还有 MemTotal 与可用核数
    fn refresh_slow(&mut self) {
        self.ifaces = if self.fixed_ifaces.is_empty() {
            let names: Vec<String> = parse_net_dev(&read("/proc/net/dev")).into_iter().map(|x| x.0).collect();
            let routes = parse_default_route(&read("/proc/net/route"), &read("/proc/net/ipv6_route"));
            let addrs = if_addrs();
            let bound = ss_bind_addrs(Path::new(SS_CONF_DIR));
            pick_ifaces(
                &names,
                &routes,
                |n| Path::new(&format!("/sys/class/net/{n}/device")).exists(),
                |n| addrs.iter().any(|(i, ip)| i == n && (!crate::server::is_private_ip(*ip) || bound.contains(ip))),
            )
        } else {
            self.fixed_ifaces.clone()
        };
        self.threads = visible_threads(std::path::Path::new("/proc"));
        if self.cg.is_some() {
            self.host_mem_total = parse_meminfo(&read("/proc/meminfo")).0;
            self.parallelism = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
        }
        self.ifaces_at = std::time::Instant::now();
    }

    pub fn sample(&mut self) -> HostSample {
        if self.ifaces_at.elapsed().as_secs() > 300 {
            self.refresh_slow();
        }
        let (pct, mem_used, mem_total) = match &mut self.cg {
            Some(cg) => {
                let (used, total) = cg.mem(self.host_mem_total);
                (cg.cpu_pct(self.parallelism), used, total)
            }
            None => {
                let (total, avail) = parse_meminfo(&read("/proc/meminfo"));
                (proc_cpu_pct(&mut self.prev_cpu), total.saturating_sub(avail), total)
            }
        };
        let (disk_used, disk_total) = disk_root();
        let readings = self.net_readings();
        self.net.update(readings);
        HostSample {
            ts_ms: now_ms(),
            cpu_pct: pct,
            load1: parse_load1(&read("/proc/loadavg"), self.threads),
            mem_used,
            mem_total,
            disk_used,
            disk_total,
            net_rx_bytes: self.net.rx,
            net_tx_bytes: self.net.tx,
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
        let root = crate::util::TmpDir::new("proc");
        for d in ["1/task/1", "77/task/77", "77/task/78", "77/task/79", "self/task/1", "sys/kernel", "123"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let n = visible_threads(&root);
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

    const ROUTE: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
        ens3\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
        ens3\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
        lxdbr0\t0084FB0A\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
        docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n";
    /// lo 上的 ::/0 是 unreachable 路由
    const ROUTE6: &str = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo\n\
        fe800000000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000001 00000000 00000001     ens3\n";

    /// 同机 IP 节点的出站地址：只读 <端口>.json 的 outbound_bind_addr（宿主机自己的 config.json、acl、坏文件不算）
    #[test]
    fn ss_bind_addrs_from_instance_configs() {
        let d = crate::util::TmpDir::new("ssbind");
        let w = |n: &str, s: &str| std::fs::write(d.join(n), s).unwrap();
        w("config.json", r#"{"server":"0.0.0.0","server_port":8388}"#);
        w("8389.json", r#"{"server":"10.116.0.109","server_port":8389,"outbound_bind_addr":"10.116.0.109"}"#);
        w("8390.json", "{坏的");
        w("fleet.acl", "[outbound_block_list]");
        assert_eq!(ss_bind_addrs(&d), vec!["10.116.0.109".parse::<IpAddr>().unwrap()]);
        assert!(ss_bind_addrs(&d.join("nope")).is_empty());
    }

    #[test]
    fn net_dev_and_ifaces() {
        let v = parse_net_dev(NETDEV);
        assert_eq!(v, vec![("lo".into(), 500, 500), ("eth0".into(), 1000, 2000)]);
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let all = s(&["lo", "ens3", "lxdbr0", "docker0", "veth12ab", "ens4"]);
        // 默认路由所在网卡；lo 上的 unreachable ::/0 由虚拟网卡规则排除
        let routes = parse_default_route(ROUTE, ROUTE6);
        assert_eq!(routes, s(&["ens3", "lo"]));
        // ens4 是只有内网地址的 VPC 网卡：不计
        assert_eq!(pick_ifaces(&all, &routes, |n| n.starts_with("ens"), |_| false), s(&["ens3"]));
        // ens4 配了公网地址（附加 IP）：一起计
        assert_eq!(pick_ifaces(&all, &routes, |n| n.starts_with("ens"), |n| n == "ens4"), s(&["ens3", "ens4"]));
        // 默认路由在 wg0（全局 WireGuard）：跳过它，退回物理网卡
        let wg = parse_default_route("Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n wg0\t00000000\t00000000\t0001\t0\t0\t0\t00000000\n", "");
        assert_eq!(wg, s(&["wg0"]));
        let all_wg = s(&["lo", "ens3", "wg0"]);
        assert_eq!(pick_ifaces(&all_wg, &wg, |n| n == "ens3", |_| false), s(&["ens3"]));
        // 都没有（容器、读不到路由）：旧规则，容器网络与隧道的前缀都排除
        let ct = s(&["lo", "eth0", "lxcbr0", "cni0", "flannel.1", "tailscale0", "docker0"]);
        assert_eq!(pick_ifaces(&ct, &[], |_| false, |_| false), s(&["eth0"]));
    }

    #[test]
    fn nic_meter_persists_across_restart_and_boot() {
        let r = |v: &[(&str, u64, u64)]| v.iter().map(|&(n, a, b)| (n.to_string(), a, b)).collect::<Vec<_>>();
        let d = crate::util::TmpDir::new("nic");
        let p = d.join("nic.json");
        // 首次安装：只记基线，开机以来的读数不当初值（升级那一刻不能有 TB 级跳变）
        let mut m = NicMeter::load(&p, "boot-a", r(&[("eth0", 1_000_000, 2_000_000)]));
        assert_eq!((m.rx, m.tx), (0, 0));
        m.update(r(&[("eth0", 1_000_100, 2_000_200)]));
        assert_eq!((m.rx, m.tx), (100, 200));
        m.save(&p).unwrap();
        // 同一次开机里 agent 重启：停机期间的内核增量计入
        let mut m = NicMeter::load(&p, "boot-a", r(&[("eth0", 1_000_400, 2_000_600)]));
        assert_eq!((m.rx, m.tx), (400, 600));
        // 新选中的网卡只记基线；eth0 读数变小（网卡重建）：增量取新值
        m.update(r(&[("eth0", 1_000_450, 2_000_650), ("eth1", 9_000_000, 9_000_000)]));
        assert_eq!((m.rx, m.tx), (450, 650));
        m.update(r(&[("eth0", 5, 7), ("eth1", 9_000_010, 9_000_020)]));
        assert_eq!((m.rx, m.tx), (465, 677));
        m.save(&p).unwrap();
        // 开机了：选中网卡开机以来的读数全部计入
        let m = NicMeter::load(&p, "boot-b", r(&[("eth0", 30, 40)]));
        assert_eq!((m.rx, m.tx), (495, 717));
    }

    #[test]
    fn cgroup_v2_cpu_and_mem() {
        assert_eq!(parse_cpu_max("200000 100000\n"), Some(2.0));
        assert_eq!(parse_cpu_max("50000 100000"), Some(0.5));
        assert_eq!(parse_cpu_max("max 100000\n"), None);
        assert_eq!(parse_usage_usec("usage_usec 16189068909\nuser_usec 14881704451\n"), Some(16_189_068_909));

        // 假 cgroup 目录：根 cgroup（没有 memory.current）不用
        let root = crate::util::TmpDir::new("cg");
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
    }

    #[test]
    fn estab() {
        let s = "Ip: Forwarding\nIp: 1 64\nTcp: RtoAlgorithm RtoMin MaxConn CurrEstab InSegs\nTcp: 1 200 -1 42 999\n";
        assert_eq!(parse_curr_estab(s), 42);
    }
}
