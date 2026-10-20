//! 主机采样：只读 /proc、/sys/class/net 与 statvfs("/")。每次约 8 次小文件读取。
use crate::util::{read_trim, round2};
use serde::Serialize;
use std::time::Instant;

#[derive(Serialize, Clone, Debug, Default)]
pub struct Cpu {
    /// 忙碌占比 = 1 − (idle + iowait) / total，%
    pub pct: f64,
    pub user: f64,
    pub system: f64,
    pub iowait: f64,
    pub steal: f64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Mem {
    pub total: u64,
    pub used: u64,
    pub avail: u64,
    pub pct: f64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Swap {
    pub total: u64,
    pub used: u64,
    pub pct: f64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Disk {
    pub total: u64,
    pub used: u64,
    pub pct: f64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Net {
    /// 参与求和的网卡（物理网卡；没有物理网卡时退回默认路由网卡）
    pub ifaces: Vec<String>,
    pub default_iface: Option<String>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_bps: f64,
    pub tx_bps: f64,
    /// 公网网卡（默认路由网卡）：开机以来累计字节（内核计数，重启清零）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pub_iface: Option<String>,
    pub pub_rx_bytes: u64,
    pub pub_tx_bytes: u64,
    /// 公网网卡本样本的增量（字节）：跨进程重启按持久化的基线续算；同一 boot_id 内计数回退视为清零（增量 = 当前值）；
    /// 开机后第一次采样增量 = 开机以来累计值。按样本求和即得任意账期的出站（tx）/入站（rx），供供应商流量配额使用。
    pub pub_rx_delta: u64,
    pub pub_tx_delta: u64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Sock {
    pub tcp: u64,
    pub udp: u64,
    pub tcp_tw: u64,
    pub tcp_orphan: u64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Sample {
    pub ts: f64,
    pub cpu: Option<Cpu>,
    pub load: [f64; 3],
    pub mem: Mem,
    pub swap: Swap,
    pub disk: Option<Disk>,
    pub net: Net,
    pub sock: Sock,
    pub procs: u64,
    pub uptime: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xray: Option<serde_json::Value>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Meta {
    pub kernel: String,
    pub os: String,
    pub ncpu: u64,
    pub boot_id: String,
}

pub fn boot_id() -> String {
    read_trim("/proc/sys/kernel/random/boot_id").unwrap_or_else(|| "unknown".into())
}

pub fn meta() -> Meta {
    let os = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
        })
        .unwrap_or_default();
    let ncpu = std::fs::read_to_string("/proc/stat")
        .map(|s| s.lines().filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(|b| b.is_ascii_digit())).count() as u64)
        .unwrap_or(0);
    Meta { kernel: read_trim("/proc/sys/kernel/osrelease").unwrap_or_default(), os, ncpu, boot_id: boot_id() }
}

pub struct Sampler {
    prev_cpu: Option<[u64; 8]>,
    prev_net: Option<(Instant, u64, u64)>,
    phys: Vec<String>,
    phys_at: Option<Instant>,
    /// 公网网卡计数的基线 (iface, rx, tx)；持久化在状态目录，进程重启不丢增量
    pub_base: Option<(String, u64, u64)>,
    pub_state: Option<std::path::PathBuf>,
    boot_id: String,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Sampler {
        Sampler { prev_cpu: None, prev_net: None, phys: vec![], phys_at: None, pub_base: None, pub_state: None, boot_id: boot_id() }
    }

    /// 启用公网网卡基线持久化（状态目录下 net.json）。同一次开机的基线才沿用；换了 boot_id 则基线为 0（增量 = 开机以来累计）。
    pub fn with_net_state(mut self, dir: &std::path::Path) -> Sampler {
        let p = dir.join("net.json");
        let mut base = None;
        if let Ok(v) = std::fs::read_to_string(&p).map(|s| serde_json::from_str::<serde_json::Value>(&s).unwrap_or_default()) {
            if let (Some(i), Some(r), Some(t)) = (v["iface"].as_str(), v["rx"].as_u64(), v["tx"].as_u64()) {
                base = Some(if v["boot_id"].as_str() == Some(self.boot_id.as_str()) { (i.to_string(), r, t) } else { (i.to_string(), 0, 0) });
            }
        }
        // 第一次运行（没有状态）：base 为 None，第一个样本只记基线，不把开机以来的历史算进本账期
        self.pub_base = base;
        self.pub_state = Some(p);
        self
    }

    /// 批次进 outbox 之后调用：保存公网网卡基线
    pub fn commit_net(&self) {
        if let (Some(p), Some((i, r, t))) = (&self.pub_state, &self.pub_base) {
            let v = serde_json::json!({"boot_id": self.boot_id, "iface": i, "rx": r, "tx": t});
            let _ = crate::util::write_atomic(p, v.to_string().as_bytes());
        }
    }

    pub fn sample(&mut self, ts: f64) -> Sample {
        let mut s = Sample { ts: round2(ts), ..Default::default() };
        s.cpu = self.cpu();
        s.load = load();
        let (mem, swap) = meminfo();
        s.mem = mem;
        s.swap = swap;
        s.disk = disk("/");
        s.net = self.net();
        s.sock = sockstat();
        s.procs = procs();
        s.uptime = read_trim("/proc/uptime")
            .and_then(|u| u.split_whitespace().next().and_then(|x| x.parse::<f64>().ok()))
            .map(|x| x.floor())
            .unwrap_or(0.0);
        s
    }

    fn cpu(&mut self) -> Option<Cpu> {
        let st = std::fs::read_to_string("/proc/stat").ok()?;
        let line = st.lines().next()?;
        let mut v = [0u64; 8];
        for (i, x) in line.split_whitespace().skip(1).take(8).enumerate() {
            v[i] = x.parse().unwrap_or(0);
        }
        let prev = self.prev_cpu.replace(v)?;
        let d: Vec<u64> = (0..8).map(|i| v[i].saturating_sub(prev[i])).collect();
        let total: u64 = d.iter().sum();
        if total == 0 {
            return None;
        }
        let t = total as f64;
        // user nice system idle iowait irq softirq steal
        let pct = |x: u64| round2(x as f64 * 100.0 / t);
        Some(Cpu {
            pct: pct(total - d[3] - d[4]),
            user: pct(d[0] + d[1]),
            system: pct(d[2] + d[5] + d[6]),
            iowait: pct(d[4]),
            steal: pct(d[7]),
        })
    }

    fn refresh_phys(&mut self) {
        if self.phys_at.is_some_and(|t| t.elapsed().as_secs() < 300) {
            return;
        }
        self.phys_at = Some(Instant::now());
        let mut v = vec![];
        if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name != "lo" && e.path().join("device").exists() {
                    v.push(name);
                }
            }
        }
        v.sort();
        self.phys = v;
    }

    fn net(&mut self) -> Net {
        self.refresh_phys();
        let def = default_iface();
        let mut ifaces = self.phys.clone();
        if ifaces.is_empty() {
            if let Some(d) = &def {
                ifaces.push(d.clone());
            }
        }
        let (mut rx, mut tx) = (0u64, 0u64);
        if let Ok(s) = std::fs::read_to_string("/proc/net/dev") {
            for l in s.lines().skip(2) {
                let Some((name, rest)) = l.split_once(':') else { continue };
                if !ifaces.iter().any(|i| i == name.trim()) {
                    continue;
                }
                let f: Vec<u64> = rest.split_whitespace().map(|x| x.parse().unwrap_or(0)).collect();
                if f.len() >= 9 {
                    rx += f[0];
                    tx += f[8];
                }
            }
        }
        let now = Instant::now();
        let (mut rbps, mut tbps) = (0.0, 0.0);
        if let Some((t0, r0, x0)) = self.prev_net {
            let dt = now.duration_since(t0).as_secs_f64();
            if dt > 0.5 && rx >= r0 && tx >= x0 {
                rbps = ((rx - r0) as f64 / dt).round();
                tbps = ((tx - x0) as f64 / dt).round();
            }
        }
        self.prev_net = Some((now, rx, tx));
        let mut n = Net { ifaces, default_iface: def.clone(), rx_bytes: rx, tx_bytes: tx, rx_bps: rbps, tx_bps: tbps, ..Default::default() };
        if let Some(d) = def {
            if let Some((prx, ptx)) = iface_counters(&d) {
                n.pub_iface = Some(d.clone());
                n.pub_rx_bytes = prx;
                n.pub_tx_bytes = ptx;
                match &self.pub_base {
                    Some((i, r0, t0)) if *i == d => {
                        n.pub_rx_delta = if prx >= *r0 { prx - r0 } else { prx };
                        n.pub_tx_delta = if ptx >= *t0 { ptx - t0 } else { ptx };
                    }
                    // 第一次运行或公网网卡换了：只记基线
                    _ => {}
                }
                self.pub_base = Some((d, prx, ptx));
            }
        }
        n
    }
}

fn iface_counters(name: &str) -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/net/dev").ok()?;
    for l in s.lines().skip(2) {
        let (n, rest) = l.split_once(':')?;
        if n.trim() == name {
            let f: Vec<u64> = rest.split_whitespace().map(|x| x.parse().unwrap_or(0)).collect();
            return (f.len() >= 9).then(|| (f[0], f[8]));
        }
    }
    None
}

fn load() -> [f64; 3] {
    let mut out = [0.0; 3];
    if let Some(s) = read_trim("/proc/loadavg") {
        for (i, x) in s.split_whitespace().take(3).enumerate() {
            out[i] = x.parse().unwrap_or(0.0);
        }
    }
    out
}

fn meminfo() -> (Mem, Swap) {
    let mut total = 0;
    let mut avail = None;
    let mut free = 0;
    let mut buffers = 0;
    let mut cached = 0;
    let mut st = 0;
    let mut sf = 0;
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        for l in s.lines() {
            let mut it = l.split_whitespace();
            let k = it.next().unwrap_or("");
            let v: u64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0) * 1024;
            match k {
                "MemTotal:" => total = v,
                "MemAvailable:" => avail = Some(v),
                "MemFree:" => free = v,
                "Buffers:" => buffers = v,
                "Cached:" => cached = v,
                "SwapTotal:" => st = v,
                "SwapFree:" => sf = v,
                _ => {}
            }
        }
    }
    let avail = avail.unwrap_or(free + buffers + cached).min(total);
    let used = total - avail;
    let mp = if total > 0 { round2(used as f64 * 100.0 / total as f64) } else { 0.0 };
    let su = st.saturating_sub(sf);
    let sp = if st > 0 { round2(su as f64 * 100.0 / st as f64) } else { 0.0 };
    (Mem { total, used, avail, pct: mp }, Swap { total: st, used: su, pct: sp })
}

pub fn disk(path: &str) -> Option<Disk> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let f = st.f_frsize as u64;
    let total = st.f_blocks as u64 * f;
    let used = (st.f_blocks as u64).saturating_sub(st.f_bfree as u64) * f;
    let avail = st.f_bavail as u64 * f;
    // 与 df 同口径：used / (used + avail)
    let pct = if used + avail > 0 { round2(used as f64 * 100.0 / (used + avail) as f64) } else { 0.0 };
    Some(Disk { total, used, pct })
}

fn default_iface() -> Option<String> {
    let s = std::fs::read_to_string("/proc/net/route").ok()?;
    let mut best: Option<(u32, String)> = None;
    for l in s.lines().skip(1) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() >= 8 && f[1] == "00000000" && f[7] == "00000000" {
            let metric: u32 = f[6].parse().unwrap_or(u32::MAX);
            if best.as_ref().is_none_or(|b| metric < b.0) {
                best = Some((metric, f[0].to_string()));
            }
        }
    }
    if best.is_none() {
        // 只有 IPv6 默认路由
        if let Ok(s6) = std::fs::read_to_string("/proc/net/ipv6_route") {
            for l in s6.lines() {
                let f: Vec<&str> = l.split_whitespace().collect();
                if f.len() >= 10 && f[0] == "00000000000000000000000000000000" && f[1] == "00" && f[9] != "lo" {
                    return Some(f[9].to_string());
                }
            }
        }
    }
    best.map(|b| b.1)
}

fn sockstat() -> Sock {
    let mut s = Sock::default();
    for p in ["/proc/net/sockstat", "/proc/net/sockstat6"] {
        let Ok(txt) = std::fs::read_to_string(p) else { continue };
        for l in txt.lines() {
            let Some((proto, rest)) = l.split_once(':') else { continue };
            let f: Vec<&str> = rest.split_whitespace().collect();
            let get = |k: &str| f.chunks(2).find(|c| c.len() == 2 && c[0] == k).and_then(|c| c[1].parse::<u64>().ok()).unwrap_or(0);
            match proto {
                "TCP" => {
                    s.tcp += get("inuse");
                    s.tcp_tw += get("tw");
                    s.tcp_orphan += get("orphan");
                }
                "TCP6" => s.tcp += get("inuse"),
                "UDP" | "UDP6" => s.udp += get("inuse"),
                _ => {}
            }
        }
    }
    s
}

fn procs() -> u64 {
    std::fs::read_dir("/proc")
        .map(|rd| rd.flatten().filter(|e| e.file_name().as_encoded_bytes().first().is_some_and(|b| b.is_ascii_digit())).count() as u64)
        .unwrap_or(0)
}
