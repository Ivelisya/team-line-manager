//! 调度器：每个节点三条（可选五条）独立循环，带 ±10% 抖动并按节点哈希错峰；L2 全局并发上限 4；
//! L2 每周期最多 3 次尝试、间隔 30 s，只有最后一次标 `final`（HL-04）。
//!
//! 控制路径：每 60 s（抖动）线路机直接访问两个公共来源；L2 失败时若两个来源的控制路径都失败，
//! 这一轮直接记 `unknown(probe_target_down)` 并结束本周期（不重试，不计入可用率）；
//! 只有一个来源的控制路径失败时，本轮 L2 只用另一个来源。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch, Semaphore};
use tokio::task::JoinHandle;

use crate::checks::{self, now, Source, SOURCES};
use crate::targets::NodeTarget;
use crate::types::{reason, CheckKind, ProbeResult, Status};

#[derive(Clone, Debug)]
pub struct ProbeConfig {
    pub tcp_interval: Duration,
    pub tcp_timeout: Duration,
    pub egress_interval: Duration,
    pub egress_attempts: u8,
    pub retry_gap: Duration,
    /// 每个来源一次请求的上限（含经 SS 的 TCP、TLS、HTTP）
    pub check_timeout: Duration,
    /// L2 / 控制路径 / IPv6 共用的并发上限
    pub concurrency: usize,
    pub jitter: f64,
    pub control_interval: Duration,
    /// 控制路径结果多久内算数
    pub control_max_age: Duration,
    /// None = 不做 UDP；Some 时只对 `NodeTarget.udp == true` 的节点做
    pub udp_interval: Option<Duration>,
    pub udp_wait: Duration,
    /// None = 不做 IPv6
    pub ipv6_interval: Option<Duration>,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        ProbeConfig {
            tcp_interval: Duration::from_secs(30),
            tcp_timeout: Duration::from_secs(5),
            egress_interval: Duration::from_secs(60),
            egress_attempts: 3,
            retry_gap: Duration::from_secs(30),
            check_timeout: Duration::from_secs(8),
            concurrency: 4,
            jitter: 0.10,
            control_interval: Duration::from_secs(60),
            control_max_age: Duration::from_secs(150),
            udp_interval: Some(Duration::from_secs(300)),
            udp_wait: Duration::from_secs(2),
            ipv6_interval: Some(Duration::from_secs(900)),
        }
    }
}

fn rand_u32() -> u32 {
    let mut b = [0u8; 4];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b);
    u32::from_le_bytes(b)
}

fn jittered(d: Duration, j: f64) -> Duration {
    let f = 1.0 + j * ((rand_u32() as f64 / u32::MAX as f64) * 2.0 - 1.0);
    d.mul_f64(f.max(0.0))
}

/// 按节点名哈希得到 [0, d) 内的固定错峰偏移
fn offset(key: &str, salt: u8, d: Duration) -> Duration {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ salt as u64;
    for b in key.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    Duration::from_millis(h % d.as_millis().max(1) as u64)
}

pub struct Prober {
    pub cfg: ProbeConfig,
    sem: Arc<Semaphore>,
    control: Mutex<HashMap<&'static str, (bool, f64)>>,
    tx: mpsc::UnboundedSender<ProbeResult>,
}

impl Prober {
    pub fn new(cfg: ProbeConfig) -> (Arc<Prober>, mpsc::UnboundedReceiver<ProbeResult>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let sem = Arc::new(Semaphore::new(cfg.concurrency.max(1)));
        (Arc::new(Prober { cfg, sem, control: Mutex::new(HashMap::new()), tx }), rx)
    }

    fn emit(&self, r: &ProbeResult) {
        let _ = self.tx.send(r.clone());
    }

    /// 一轮控制路径（两个来源）
    pub async fn control_round(&self) -> Vec<ProbeResult> {
        let mut out = Vec::new();
        for src in SOURCES.iter() {
            let r = {
                let _p = self.sem.acquire().await.expect("semaphore 不会关闭");
                checks::control_check(src, self.cfg.check_timeout).await
            };
            self.control.lock().unwrap().insert(src.name, (r.status.is_ok(), r.measured_at));
            self.emit(&r);
            out.push(r);
        }
        out
    }

    /// 当前可用的来源；None = 两个来源的控制路径都（在有效期内）失败
    fn usable_sources(&self) -> Option<Vec<Source>> {
        let c = self.control.lock().unwrap();
        let t = now();
        let max = self.cfg.control_max_age.as_secs_f64();
        let usable: Vec<Source> = SOURCES
            .iter()
            .filter(|s| match c.get(s.name) {
                Some((ok, at)) if t - at <= max => *ok,
                _ => true, // 没有新鲜的控制路径结果：不能判来源故障
            })
            .copied()
            .collect();
        if usable.is_empty() {
            None
        } else {
            Some(usable)
        }
    }

    /// 一个 L2 周期（最多 `egress_attempts` 次尝试）
    pub async fn egress_cycle(&self, node: &NodeTarget) -> Vec<ProbeResult> {
        let mut out = Vec::new();
        let n = self.cfg.egress_attempts.max(1);
        for attempt in 1..=n {
            let srcs = self.usable_sources();
            let mut r = {
                let _p = self.sem.acquire().await.expect("semaphore 不会关闭");
                checks::egress_attempt(node, srcs.as_deref().unwrap_or(&SOURCES), self.cfg.check_timeout).await
            };
            r.attempt = Some(attempt);
            let failed = !r.status.is_ok();
            let mismatch = r.reason_code.as_deref() == Some(reason::EGRESS_MISMATCH);
            if failed && !mismatch && r.reason_code.as_deref() != Some(reason::UPSTREAM_DOWN) && self.usable_sources().is_none() {
                r.detail = Some(format!("控制路径也失败；原始：{}", r.reason_code.as_deref().unwrap_or("?")));
                r.status = Status::Unknown;
                r.reason_code = Some(reason::PROBE_TARGET_DOWN.into());
                r.legacy_rc = Some(-1);
                r.final_attempt = Some(true);
                self.emit(&r);
                out.push(r);
                break;
            }
            let last = !failed || attempt == n;
            r.final_attempt = Some(last);
            self.emit(&r);
            out.push(r);
            if last {
                break;
            }
            tokio::time::sleep(self.cfg.retry_gap).await;
        }
        out
    }

    pub async fn tcp_one(&self, node: &NodeTarget) -> ProbeResult {
        let r = checks::tcp_check(node, self.cfg.tcp_timeout).await;
        self.emit(&r);
        r
    }

    pub async fn udp_one(&self, node: &NodeTarget) -> ProbeResult {
        let r = checks::udp_check(node, self.cfg.udp_wait).await;
        self.emit(&r);
        r
    }

    pub async fn ipv6_one(&self, node: &NodeTarget) -> ProbeResult {
        let r = {
            let _p = self.sem.acquire().await.expect("semaphore 不会关闭");
            checks::ipv6_check(node, self.cfg.check_timeout).await
        };
        self.emit(&r);
        r
    }

    /// 跑一轮（`--once`）：控制路径 → 所有节点的 TCP 与 L2 周期（并发受限）→ 可选 UDP / IPv6
    pub async fn run_once(self: &Arc<Self>, targets: &[NodeTarget], kinds: &[CheckKind]) -> Vec<ProbeResult> {
        let mut out = Vec::new();
        if kinds.contains(&CheckKind::Control) || kinds.contains(&CheckKind::Egress) {
            out.extend(self.control_round().await);
        }
        let mut hs: Vec<JoinHandle<Vec<ProbeResult>>> = Vec::new();
        for t in targets {
            let (p, t, kinds) = (self.clone(), t.clone(), kinds.to_vec());
            hs.push(tokio::spawn(async move {
                let mut v = Vec::new();
                if kinds.contains(&CheckKind::Tcp) {
                    v.push(p.tcp_one(&t).await);
                }
                if kinds.contains(&CheckKind::Egress) {
                    v.extend(p.egress_cycle(&t).await);
                }
                if kinds.contains(&CheckKind::Udp) {
                    v.push(p.udp_one(&t).await);
                }
                if kinds.contains(&CheckKind::Ipv6) {
                    v.push(p.ipv6_one(&t).await);
                }
                v
            }));
        }
        for h in hs {
            if let Ok(v) = h.await {
                out.extend(v);
            }
        }
        out
    }

    /// 常驻：按 `targets` 的当前值维护每个节点的循环；targets 变化时增删（relayd 在 RelaySpec 变化时更新）
    pub async fn run(self: Arc<Self>, mut targets: watch::Receiver<Vec<NodeTarget>>) {
        let ctl = {
            let p = self.clone();
            tokio::spawn(async move {
                loop {
                    p.control_round().await;
                    tokio::time::sleep(jittered(p.cfg.control_interval, p.cfg.jitter)).await;
                }
            })
        };
        let mut tasks: HashMap<String, Vec<JoinHandle<()>>> = HashMap::new();
        loop {
            let list = targets.borrow_and_update().clone();
            let mut want: HashMap<String, NodeTarget> = HashMap::new();
            for t in list {
                let key = format!("{}|{}|{}|{}|{}|{}|{}|{}|{}", t.node, t.tag, t.addr, t.port, t.method, t.username, t.password, t.expected_ip, t.udp);
                want.insert(key, t);
            }
            tasks.retain(|k, hs| {
                let keep = want.contains_key(k);
                if !keep {
                    hs.iter().for_each(|h| h.abort());
                }
                keep
            });
            for (k, t) in want {
                if tasks.contains_key(&k) {
                    continue;
                }
                tasks.insert(k, self.spawn_node(t));
            }
            if targets.changed().await.is_err() {
                break;
            }
        }
        ctl.abort();
        for hs in tasks.values() {
            hs.iter().for_each(|h| h.abort());
        }
    }

    fn spawn_node(self: &Arc<Self>, t: NodeTarget) -> Vec<JoinHandle<()>> {
        let mut hs = Vec::new();
        let cfg = self.cfg.clone();
        {
            let (p, t) = (self.clone(), t.clone());
            hs.push(tokio::spawn(async move {
                tokio::time::sleep(offset(&t.node, 1, cfg.tcp_interval)).await;
                loop {
                    p.tcp_one(&t).await;
                    tokio::time::sleep(jittered(p.cfg.tcp_interval, p.cfg.jitter)).await;
                }
            }));
        }
        {
            let (p, t) = (self.clone(), t.clone());
            let d = self.cfg.egress_interval;
            hs.push(tokio::spawn(async move {
                tokio::time::sleep(offset(&t.node, 2, d)).await;
                loop {
                    let start = tokio::time::Instant::now();
                    p.egress_cycle(&t).await;
                    // 下一周期从本周期开始算 60 s（±10%）；重试拖过 60 s 时立即开始下一周期
                    tokio::time::sleep_until(start + jittered(d, p.cfg.jitter)).await;
                }
            }));
        }
        if let (Some(d), true) = (self.cfg.udp_interval, t.udp) {
            let (p, t) = (self.clone(), t.clone());
            hs.push(tokio::spawn(async move {
                tokio::time::sleep(offset(&t.node, 3, d)).await;
                loop {
                    p.udp_one(&t).await;
                    tokio::time::sleep(jittered(d, p.cfg.jitter)).await;
                }
            }));
        }
        if let Some(d) = self.cfg.ipv6_interval {
            let (p, t) = (self.clone(), t.clone());
            hs.push(tokio::spawn(async move {
                tokio::time::sleep(offset(&t.node, 4, d)).await;
                loop {
                    p.ipv6_one(&t).await;
                    tokio::time::sleep(jittered(d, p.cfg.jitter)).await;
                }
            }));
        }
        hs
    }
}
