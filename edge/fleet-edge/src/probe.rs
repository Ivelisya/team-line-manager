//! relayd 内嵌的逐项探测（fleet-probe，见 fleet/crates/probe/WIRING.md）。
//!
//! - 后台线程 "probe"：自己的 tokio current_thread 运行时，跑 `Prober`（L1 TCP 30 s、L2 经 SS 60 s、控制路径、
//!   UDP STUN 5 min、IPv6 15 min），结果每秒（或满 64 条）写进状态目录的 `probe.db`（24 h）；每 60 s 重读探测对象。
//! - 主线程：组批时取游标之后的结果放进批次顶层 `"probe"`（`fleet.probe.v1`），批次写进 outbox 之后前移游标。
//!   投递由 outbox 负责；中控按自然键 (node, check, measured_at, attempt) 去重。
//!
//! 探测对象只读自 Xray 配置里的 SS 出站（含密码，只在内存里；批次里的 targets 不含密码）。
use crate::config::ProbeCfg;
use crate::{info, warn};
use fleet_probe::scheduler::{ProbeConfig, Prober};
use fleet_probe::store::ProbeStore;
use fleet_probe::targets::{self, NodeTarget};
use fleet_probe::types::TargetInfo;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 线路机公网 IP：本机 + 互探对象
pub fn leak_ips_of(rs: Option<&crate::config::RelaySelfCfg>) -> Vec<String> {
    let Some(r) = rs else { return vec![] };
    let mut v = vec![r.public_ip.clone()];
    for p in &r.peers {
        if let Some((_, t)) = p.split_once('@') {
            if let Some((ip, _)) = t.rsplit_once(':') {
                v.push(ip.to_string());
            }
        }
    }
    v
}

pub fn open_store_pub(db: &Path) -> Result<ProbeStore, String> {
    open_store(db)
}

/// 探测线程、全链路线程、主线程各开一个连接：busy_timeout 必须先设（切 WAL 要排他锁，几个线程同时启动会撞上），
/// 失败再重试几次，避免某个线程在启动竞争里静默退出
fn open_store(db: &Path) -> Result<ProbeStore, String> {
    let mut last = String::new();
    for i in 0..20 {
        let r = (|| -> Result<ProbeStore, String> {
            let c = rusqlite::Connection::open(db).map_err(|e| e.to_string())?;
            c.busy_timeout(Duration::from_secs(10)).map_err(|e| e.to_string())?;
            c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").map_err(|e| e.to_string())?;
            ProbeStore::with_connection(c).map_err(|e| e.to_string())
        })();
        match r {
            Ok(s) => return Ok(s),
            Err(e) => {
                last = e;
                std::thread::sleep(Duration::from_millis(200 * (i + 1)));
            }
        }
    }
    Err(last)
}

pub fn load_targets(sources: &[String], ids: Option<&str>) -> (Vec<NodeTarget>, Vec<String>) {
    let mut errs = vec![];
    let mut lists = vec![];
    for s in sources {
        match targets::from_xray_file(Path::new(s)) {
            Ok(l) => lists.push(l),
            Err(e) => errs.push(e),
        }
    }
    let idmap = match ids {
        Some(p) if Path::new(p).exists() => targets::identities_from_legacy_assets(Path::new(p)).unwrap_or_else(|e| {
            errs.push(e);
            BTreeMap::new()
        }),
        _ => BTreeMap::new(),
    };
    (targets::merge(lists, &idmap), errs)
}

fn key(l: &[NodeTarget]) -> Vec<String> {
    l.iter().map(|t| format!("{}|{}|{}|{}|{}|{}|{}|{}", t.node, t.tag, t.addr, t.port, t.method, t.password, t.expected_ip, t.udp)).collect()
}

pub struct ProbeLink {
    cfg: ProbeCfg,
    sources: Vec<String>,
    db: PathBuf,
    host_id: String,
    targets: Arc<Mutex<Vec<TargetInfo>>>,
    store: Option<ProbeStore>,
    pending_last: Option<i64>,
    started: bool,
    /// (xray 进程的 argv[0] 文件名, 命令行片段) —— 全链路检查用同一个 xray 可执行文件
    pub xray_proc: (String, String),
    /// 线路机自检与互探
    pub relay_self: Option<crate::config::RelaySelfCfg>,
}

impl ProbeLink {
    pub fn new(cfg: ProbeCfg, xray_config: Option<String>, state_dir: &Path, host_id: &str) -> ProbeLink {
        let sources = if cfg.xray_configs.is_empty() { xray_config.into_iter().collect() } else { cfg.xray_configs.clone() };
        ProbeLink {
            cfg,
            sources,
            db: state_dir.join("probe.db"),
            host_id: host_id.to_string(),
            targets: Arc::new(Mutex::new(vec![])),
            store: None,
            pending_last: None,
            started: false,
            xray_proc: ("xray".into(), "/etc/fleet/xray/config.json".into()),
            relay_self: None,
        }
    }

    /// --once 用：只列出探测对象（不探测）
    pub fn describe(&self) -> Value {
        let (l, errs) = load_targets(&self.sources, self.cfg.identities.as_deref());
        serde_json::json!({"sources": self.sources, "targets": l.iter().map(|t| t.info()).collect::<Vec<_>>(), "errors": errs})
    }

    pub fn start(&mut self) {
        if self.started || !self.cfg.enabled {
            return;
        }
        self.started = true;
        let (sources, ids, db, tinfo) = (self.sources.clone(), self.cfg.identities.clone(), self.db.clone(), self.targets.clone());
        let peers = self.relay_self.as_ref().map(|r| (r.peers.clone(), r.peer_interval_s)).unwrap_or_default();
        let leak_ips = leak_ips_of(self.relay_self.as_ref());
        let mut pc = ProbeConfig::default();
        if !self.cfg.udp {
            pc.udp_interval = None;
        }
        if !self.cfg.ipv6 {
            pc.ipv6_interval = None;
        }
        let r = std::thread::Builder::new().name("probe".into()).stack_size(512 * 1024).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    warn!("probe_runtime_failed", "err={}", crate::log::q(&e.to_string()));
                    return;
                }
            };
            rt.block_on(probe_main(pc, sources, ids, db, tinfo, peers, leak_ips));
        });
        if let Err(e) = r {
            warn!("probe_thread_failed", "err={}", crate::log::q(&e.to_string()));
        }
        if self.cfg.path {
            let (src, ids, db, (bin, pm)) = (self.sources.clone(), self.cfg.identities.clone(), self.db.clone(), self.xray_proc.clone());
            let rs = self.relay_self.clone();
            let tmp = self.db.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("/tmp"));
            let iv = Duration::from_secs(self.cfg.path_interval_s.clamp(30, 3600));
            let r = std::thread::Builder::new()
                .name("path".into())
                .stack_size(512 * 1024)
                .spawn(move || crate::pathcheck::run_loop(iv, src, ids, bin, pm, db, tmp, rs));
            if let Err(e) = r {
                warn!("path_thread_failed", "err={}", crate::log::q(&e.to_string()));
            }
        }
    }

    /// 组批：游标之后最多 batch_limit 条
    pub fn batch(&mut self) -> Option<Value> {
        if !self.started {
            return None;
        }
        if self.store.is_none() {
            match open_store(&self.db) {
                Ok(s) => self.store = Some(s),
                Err(e) => {
                    warn!("probe_store_open_failed", "err={}", crate::log::q(&e));
                    return None;
                }
            }
        }
        let st = self.store.as_ref()?;
        let tinfo = self.targets.lock().map(|g| g.clone()).unwrap_or_default();
        match st.pending(&self.host_id, self.cfg.batch_limit, tinfo, crate::util::round2(crate::util::now_f64())) {
            Ok(Some(b)) => {
                self.pending_last = Some(b.last_id);
                serde_json::to_value(&b).ok()
            }
            Ok(None) => None,
            Err(e) => {
                warn!("probe_pending_failed", "err={}", crate::log::q(&e.to_string()));
                None
            }
        }
    }

    /// 批次已写进 outbox：前移游标
    pub fn commit(&mut self) {
        if let (Some(id), Some(st)) = (self.pending_last.take(), self.store.as_ref()) {
            if let Err(e) = st.ack(id) {
                warn!("probe_ack_failed", "err={}", crate::log::q(&e.to_string()));
            }
        }
    }
}

/// 线路机互探：TCP connect 到其他线路机的主节点入口（从外部看它是否活着）
async fn peer_tcp(uuid: String, target: String, limit: Duration) -> fleet_probe::types::ProbeResult {
    use fleet_probe::types::{CheckKind, ProbeResult, Status};
    let t0 = std::time::Instant::now();
    let r = tokio::time::timeout(limit, tokio::net::TcpStream::connect(target.as_str())).await;
    let mut p = ProbeResult::new(uuid, CheckKind::Tcp, Status::Ok, crate::util::now_f64());
    match r {
        // 与节点 L1 同口径：整毫秒且 ≥ 1
        Ok(Ok(_)) => p.value_ms = Some((t0.elapsed().as_secs_f64() * 1000.0).round().max(1.0)),
        Ok(Err(e)) => {
            p.status = Status::Fail;
            p.reason_code = Some("upstream_down".into());
            p.detail = Some(format!("{:?}: {e}", e.kind()).chars().take(200).collect());
        }
        Err(_) => {
            p.status = Status::Timeout;
            p.reason_code = Some("upstream_down".into());
            p.detail = Some("connect timeout".into());
        }
    }
    p.legacy_rc = Some(if p.status == Status::Ok { 0 } else { 7 });
    p.target = Some(target);
    p
}

/// 漏出标记：节点的出口（egress / udp / path 看到的 IP）等于某台线路机的公网 IP，说明流量没走落地、从线路机直接出去了。
/// 这是唯一保留的出口 IP 判定（负责人：不做出口 IP 比对，只保留漏出标记）。
/// `skip_ports`：主节点入口（出口 freedom，本来就从线路机出去）的端口，不标记
pub fn mark_leaks(rs: &mut [fleet_probe::types::ProbeResult], leak_ips: &[String], skip_ports: &[u16]) {
    use fleet_probe::types::{CheckKind, Status};
    if leak_ips.is_empty() {
        return;
    }
    for r in rs.iter_mut() {
        if !matches!(r.check, CheckKind::Egress | CheckKind::Udp | CheckKind::Path) || r.status != Status::Ok {
            continue;
        }
        // 线路机自检（node = 线路机资产、target 是本机主节点入口）本来就该从本机出去
        if r.target.as_deref().is_some_and(|t| t.starts_with("127.0.0.1:")) && r.check == CheckKind::Egress {
            continue;
        }
        let hit = r.actual_ip.as_deref().is_some_and(|a| a.split(',').any(|ip| leak_ips.iter().any(|l| l == ip)));
        let direct_entry = r.path.as_ref().is_some_and(|p| skip_ports.contains(&p.entry_port));
        if hit && !direct_entry {
            r.status = Status::Fail;
            r.reason_code = Some("egress_leak".into());
            r.value_ms = None;
            if let Some(p) = r.path.as_mut() {
                p.healthy = false;
            }
        }
    }
}

async fn probe_main(pc: ProbeConfig, sources: Vec<String>, ids: Option<String>, db: PathBuf, tinfo: Arc<Mutex<Vec<TargetInfo>>>, peers: (Vec<String>, u64), leak_ips: Vec<String>) {
    let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<fleet_probe::types::ProbeResult>();
    let peer_list: Vec<(String, String)> = peers.0.iter().filter_map(|p| p.split_once('@').map(|(u, t)| (u.to_string(), t.to_string()))).collect();
    if !peer_list.is_empty() {
        info!("peer_probe_start", "peers={}", peer_list.len());
        let iv = Duration::from_secs(peers.1.clamp(10, 600));
        tokio::spawn(async move {
            loop {
                for (u, t) in &peer_list {
                    let r = peer_tcp(u.clone(), t.clone(), Duration::from_secs(5)).await;
                    if ptx.send(r).is_err() {
                        return;
                    }
                }
                tokio::time::sleep(iv).await;
            }
        });
    }
    let mut store = match open_store(&db) {
        Ok(s) => s,
        Err(e) => {
            warn!("probe_store_open_failed", "err={}", crate::log::q(&e));
            return;
        }
    };
    let (list, errs) = load_targets(&sources, ids.as_deref());
    for e in &errs {
        warn!("probe_targets_error", "err={}", crate::log::q(e));
    }
    info!("probe_start", "targets={} sources={}", list.len(), crate::log::q(&sources.join(",")));
    *tinfo.lock().unwrap() = list.iter().map(|t| t.info()).collect();
    let mut cur_key = key(&list);
    let (prober, mut rx) = Prober::new(pc);
    let (tx, trx) = tokio::sync::watch::channel(list);
    // 自检：控制路径（线路机自己能否直连两个公共来源）
    {
        let p = prober.clone();
        tokio::spawn(async move {
            let rs = p.control_round().await;
            let ok = rs.iter().filter(|r| r.status.is_ok()).count();
            if ok == 0 {
                warn!("probe_selfcheck", "control_ok=0 note={}", crate::log::q("线路机直连两个公共来源都失败"));
            } else {
                info!("probe_selfcheck", "control_ok={ok}/{}", rs.len());
            }
        });
    }
    tokio::spawn(prober.clone().run(trx));
    let mut buf = Vec::with_capacity(64);
    // 批量落库：每 10 s（或攒满 64 条）一次事务，而不是每秒一次（SQLite 每次提交都要写 WAL 页）
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    let mut last_reload = tokio::time::Instant::now();
    let mut last_prune = tokio::time::Instant::now();
    loop {
        tokio::select! {
            r = rx.recv() => {
                match r {
                    Some(r) => { buf.push(r); if buf.len() < 64 { continue; } }
                    None => break,
                }
            }
            Some(r) = prx.recv() => { buf.push(r); if buf.len() < 64 { continue; } }
            _ = tick.tick() => {}
        }
        if !buf.is_empty() {
            mark_leaks(&mut buf, &leak_ips, &[]);
            if let Err(e) = store.insert(&mut buf) {
                warn!("probe_insert_failed", "n={} err={}", buf.len(), crate::log::q(&e.to_string()));
            }
            buf.clear();
        }
        if last_reload.elapsed() >= Duration::from_secs(60) {
            last_reload = tokio::time::Instant::now();
            let (l, errs) = load_targets(&sources, ids.as_deref());
            let k = key(&l);
            // 读失败（例如配置正在被重写）时保持原来的对象，不清空
            if k != cur_key && (errs.is_empty() || !l.is_empty()) {
                info!("probe_targets_changed", "targets={}", l.len());
                *tinfo.lock().unwrap() = l.iter().map(|t| t.info()).collect();
                cur_key = k;
                let _ = tx.send(l);
            }
        }
        if last_prune.elapsed() >= Duration::from_secs(3600) {
            last_prune = tokio::time::Instant::now();
            let _ = store.prune(crate::util::now_f64());
        }
    }
}
