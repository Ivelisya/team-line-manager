//! 采集任务：主机遥测（两种角色）、Xray 流量与状态、探测调度（线路机）

use crate::host::{self, HostCollector};
use crate::probe;
use crate::state::{stats_path, Shared};
use crate::util::{load_json, now_ms};
use crate::xray::cli::XrayCli;
use crate::xray::{conns, ctl, render, stats};
use fleet_proto::{tags, DesiredEntry, DesiredState, XrayStatus};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Semaphore};
use tokio::time::{interval_at, Instant, MissedTickBehavior};

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);
/// Xray 流量统计（xray api statsquery 子进程）的采样间隔。计数器是累计值，拉长不丢流量，只是样本更粗。
/// 服务端按分钟汇总（sum/max），不依赖 10 s 粒度。
pub const STATS_INTERVAL: Duration = Duration::from_secs(30);
/// Xray 不可达（或取版本失败）后，重新探查 API 的最短间隔
const DOWN_RECHECK: Duration = Duration::from_secs(60);
/// 进程还在时，API 连续失败这么多次才算不可达（单次 statsquery 失败不报 Xray 未运行）
const DOWN_AFTER_FAILS: u32 = 3;

fn ticker(period: Duration) -> tokio::time::Interval {
    let mut t = interval_at(Instant::now() + period, period);
    t.set_missed_tick_behavior(MissedTickBehavior::Skip);
    t
}

/// 主机采样 + SystemInfo（启动时、变化时、每小时兜底重发）
pub async fn host_task(sh: Arc<Shared>, mut sd: watch::Receiver<bool>) {
    // 采样读一串 /proc 文件，每 300 s 还要遍历整个 /proc 数线程：都放阻塞线程，采集器在两边来回移交
    let (mut hc, mut last_info) = tokio::task::spawn_blocking(|| (HostCollector::new(), host::collect_info())).await.expect("主机采样任务异常终止");
    sh.buf.lock().unwrap().info = Some(last_info.clone());
    let mut info_sent_at = Instant::now();
    let mut tick = ticker(SAMPLE_INTERVAL);
    let mut n = 0u32;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = sd.changed() => return,
        }
        n += 1;
        let want_info = n.is_multiple_of(30);
        let (back, s, cur) = tokio::task::spawn_blocking(move || {
            let s = hc.sample();
            (hc, s, want_info.then(host::collect_info))
        })
        .await
        .expect("主机采样任务异常终止");
        hc = back;
        // 读 /proc 等 IO 在拿锁之前做完，持锁只做内存操作
        let mut b = sh.buf.lock().unwrap();
        b.push_host(s);
        if let Some(cur) = cur {
            if cur != last_info || info_sent_at.elapsed() > Duration::from_secs(3600) {
                last_info = cur.clone();
                b.info = Some(cur);
                info_sent_at = Instant::now();
            }
        }
    }
}

/// 当前启用的入口 (id, port)
fn enabled_entries(sh: &Shared) -> Vec<(uuid::Uuid, u16)> {
    sh.applied.read().unwrap().as_ref().map(|d| d.entries.iter().filter(|e| e.enabled).map(|e| (e.id, e.port)).collect()).unwrap_or_default()
}

/// Xray 流量增量 + 连接数 + 进程状态
pub async fn stats_task(sh: Arc<Shared>, cli: XrayCli, mut sd: watch::Receiver<bool>) {
    let cfg = sh.cfg.clone();
    let mut st: stats::StatsState = load_json(&stats_path(&cfg)).unwrap_or_default();
    let boot = host::boot_id();
    let mut tick = ticker(SAMPLE_INTERVAL);
    let mut version: Option<(u32, String)> = None; // (pid, version)
    let mut was_up: Option<bool> = None;
    // 缓存的 Xray 进程身份：(MainPID, (starttime ticks, 启动时刻 ms))。只在 /proc 里对不上时才重新问 systemctl
    let mut proc: Option<(u32, (u64, i64))> = None;
    let mut last_query: Option<Instant> = None;
    let mut last_core: Option<String> = None; // 上次成功计算增量时的进程纪元
    let mut up = false;
    let mut api_fails = 0u32; // xray api 连续失败次数
    let mut down_check_at: Option<Instant> = None; // Xray 不可达时，下次允许重查的时刻
    let mut version_retry_at = Instant::now();
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = sd.changed() => return,
        }
        // 每个 SAMPLE_INTERVAL 确认一次进程：缓存的进程还在只读 /proc，对不上才问 systemctl
        if !proc.is_some_and(|(p, started)| ctl::proc_start_ticks(p) == Some(started.0)) {
            proc = match ctl::main_pid(&cfg.xray_service).await {
                Some(p) => ctl::proc_start(p).map(|s| (p, s)),
                None => None,
            };
            // 进程（重新）出现：结束退避，本轮就查 API
            if proc.is_some() {
                down_check_at = None;
            }
        }
        // Xray 已确认不可达时，xray api 子进程至多每 DOWN_RECHECK 试一次；其余 tick 只更新状态
        let backoff = !up && down_check_at.is_some_and(|t| Instant::now() < t);
        let pid = proc.map(|p| p.0);
        let start = proc.map(|p| p.1);
        let core_id = stats::core_id_of(&boot, pid, start.map(|s| s.0));
        // 流量计数器是累计值，降低采样频率不丢流量。以下情况立即查：首次、到点、API 不可达或刚失败过（尽快发现恢复）、
        // 进程身份未知、进程换代（Xray 重启后尽早记下新基线）
        let due = !backoff
            && (last_query.is_none_or(|t| t.elapsed() >= STATS_INTERVAL) || !up || api_fails > 0 || proc.is_none() || (core_id.is_some() && core_id != last_core));
        if due {
            last_query = Some(Instant::now());
            let res = cli.user_stats().await;
            api_fails = if res.is_ok() { 0 } else { api_fails + 1 };
            // 进程不在，或进程在但连续失败 DOWN_AFTER_FAILS 次，才算不可达并开始退避
            up = api_fails == 0 || (proc.is_some() && api_fails < DOWN_AFTER_FAILS);
            down_check_at = (!up).then(|| Instant::now() + DOWN_RECHECK);
            if was_up != Some(up) {
                match &res {
                    Ok(_) => tracing::info!("Xray API 可达"),
                    Err(e) => tracing::warn!("Xray API 不可达：{e:#}"),
                }
                was_up = Some(up);
            }
            let ts = now_ms();
            // pid/启动时间取不到：本轮不算增量（保留基线和上一轮 core_id），免得被误判成 Xray 重启而重复计数
            if let Ok(list) = res {
                let cur: HashMap<String, u64> = list.into_iter().collect();
                if let Some(deltas) = stats::compute_if_known(&mut st, core_id.as_deref(), &cur) {
                    last_core = core_id.clone();
                    let entries = enabled_entries(&sh);
                    let ports: Vec<u16> = entries.iter().map(|&(_, port)| port).collect();
                    let cn = tokio::task::spawn_blocking(move || conns::count_now(&ports)).await.unwrap_or_default();
                    let with_conns: Vec<_> = entries.iter().map(|&(id, port)| (id, cn.get(&port).copied().unwrap_or(0))).collect();
                    let samples = stats::to_samples(&deltas, &with_conns, ts);
                    let mut b = sh.buf.lock().unwrap();
                    b.push_entries(samples);
                    b.stats_commit = Some(st.clone());
                } else {
                    tracing::debug!("取不到 Xray 的 pid/启动时间，本轮不算流量增量");
                }
            }
        }
        // 版本：进程换代时取一次；取不到则 DOWN_RECHECK 之后再试
        if let (true, Some(p)) = (up, pid) {
            if version.as_ref().map(|v| v.0) != Some(p) && Instant::now() >= version_retry_at {
                match cli.version().await {
                    Ok(v) => version = Some((p, v)),
                    Err(_) => version_retry_at = Instant::now() + DOWN_RECHECK,
                }
            }
        }
        let status = XrayStatus {
            running: up,
            version: version.as_ref().map(|v| v.1.clone()),
            started_at_ms: start.map(|s| s.1),
            applied_generation: sh.applied_gen.load(Ordering::Relaxed),
        };
        sh.buf.lock().unwrap().xray = Some(status);
    }
}

struct Sched {
    sig: String,
    next_tcp: Instant,
    next_egress: Instant,
    /// 该入口的 TCP 探测是否还在途（排队等信号量也算）：上一次没结束就等它结束再派，免得目标全超时时任务越积越多
    tcp_busy: Arc<AtomicBool>,
    /// 该入口的出口探测是否还在途（排队等信号量也算）：同一入口最多一个，免得任务堆积
    egress_busy: Arc<AtomicBool>,
    /// 上一次出口探测是否成功：只在成功/失败切换时打日志（初始当作成功）
    egress_ok: Arc<AtomicBool>,
}

/// 探测任务结束（含 panic）时清掉在途标记
struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn stagger(id: uuid::Uuid, period: Duration) -> Duration {
    Duration::from_millis((id.as_u128() % (period.as_millis().max(1))) as u64)
}

fn entry_sig(e: &DesiredEntry) -> String {
    format!("{}|{:?}", e.port, e.outbound)
}

/// 入口 → (探测端口, 经该端口出去的客户端)
type ProbeClients = HashMap<uuid::Uuid, (u16, reqwest::Client)>;

/// 探测调度：TCP 每 probe_interval_s（默认 30 s），出口每 2 倍间隔（默认 60 s）
pub async fn probe_task(sh: Arc<Shared>, mut sd: watch::Receiver<bool>) {
    let mut sched: HashMap<uuid::Uuid, Sched> = HashMap::new();
    let sem = Arc::new(Semaphore::new(4)); // 出口探测并发上限
    let tcp_sem = Arc::new(Semaphore::new(8)); // TCP 探测并发上限（单独一个，不排在 12 s 的出口探测后面）
    let mut tick = interval_at(Instant::now() + Duration::from_secs(2), Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut first_pass = true;
    let api_port = render::RenderCtx::of(&sh.cfg).api_port();
    let mut probes_cache: Option<(Arc<DesiredState>, ProbeClients)> = None;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = sd.changed() => return,
        }
        // Arc 克隆很轻；端口表只在期望状态换代（Arc 变了）时重算：仍在的端口沿用客户端，新端口新建，去掉的随旧表丢掉
        let Some(d) = sh.applied.read().unwrap().clone() else { continue };
        let tcp_every = Duration::from_secs(d.relay.probe_interval_s.clamp(5, 3600) as u64);
        let egress_every = tcp_every * 2;
        let now = Instant::now();
        if probes_cache.as_ref().is_none_or(|(a, _)| !Arc::ptr_eq(a, &d)) {
            let old: HashMap<u16, reqwest::Client> = probes_cache.take().map(|(_, m)| m.into_values().collect()).unwrap_or_default();
            let mut m = HashMap::new();
            for (id, port) in render::probe_ports(&d.entries, api_port) {
                match old.get(&port).cloned().map_or_else(|| probe::socks_client(port), Ok) {
                    Ok(c) => {
                        m.insert(id, (port, c));
                    }
                    Err(e) => tracing::warn!(entry = %tags::inbound(id), "建出口探测客户端失败：{e:#}"),
                }
            }
            probes_cache = Some((d.clone(), m));
            // 端口或出站变了的入口尽快重探一次。入口只随期望状态换代而变，签名（要格式化整个出站）不必每秒逐个比
            for e in d.entries.iter().filter(|e| e.enabled) {
                if let Some(s) = sched.get_mut(&e.id) {
                    let sig = entry_sig(e);
                    if s.sig != sig {
                        s.sig = sig;
                        s.next_tcp = now + Duration::from_secs(1);
                        s.next_egress = now + Duration::from_secs(4);
                    }
                }
            }
        }
        let probes = &probes_cache.as_ref().expect("刚填充").1;
        let live: HashSet<_> = d.entries.iter().filter(|e| e.enabled).map(|e| e.id).collect();
        sched.retain(|id, _| live.contains(id));
        for e in d.entries.iter().filter(|e| e.enabled) {
            let s = sched.entry(e.id).or_insert_with(|| {
                // 启动时错峰；运行中新增的入口尽快探一次，让界面马上有结果
                let (a, b) = if first_pass { (stagger(e.id, tcp_every), Duration::from_secs(5) + stagger(e.id, egress_every)) } else { (Duration::from_secs(1), Duration::from_secs(4)) };
                Sched { sig: entry_sig(e), next_tcp: now + a, next_egress: now + b, tcp_busy: Arc::new(AtomicBool::new(false)), egress_busy: Arc::new(AtomicBool::new(false)), egress_ok: Arc::new(AtomicBool::new(true)) }
            });
            if now >= s.next_tcp && !s.tcp_busy.swap(true, Ordering::AcqRel) {
                s.next_tcp = now + tcp_every;
                let busy = BusyGuard(s.tcp_busy.clone());
                let (host, port) = e.outbound.addr();
                let (sh2, id, host) = (sh.clone(), e.id, host.to_string());
                let tcp_sem2 = tcp_sem.clone();
                tokio::spawn(async move {
                    let _busy = busy;
                    let Ok(_permit) = tcp_sem2.acquire().await else { return };
                    let r = probe::tcp_probe(id, &host, port, Duration::from_secs(5), now_ms()).await;
                    sh2.buf.lock().unwrap().push_probe(r);
                });
            }
            if now >= s.next_egress {
                s.next_egress = now + egress_every;
                if let Some((_, client)) = probes.get(&e.id) {
                    if s.egress_busy.swap(true, Ordering::AcqRel) {
                        let n = sh.health.probes_skipped.fetch_add(1, Ordering::Relaxed) + 1;
                        if n <= 3 || n.is_multiple_of(100) {
                            tracing::warn!(entry = %tags::inbound(e.id), skipped_total = n, "上一次出口探测还没结束，跳过本次");
                        }
                        continue;
                    }
                    let busy = BusyGuard(s.egress_busy.clone());
                    let (sh2, sem2, id, url, ip, client) = (sh.clone(), sem.clone(), e.id, d.relay.echo_url.clone(), e.expect_egress_ip, client.clone());
                    let last_ok = s.egress_ok.clone();
                    tokio::spawn(async move {
                        let _busy = busy;
                        let Ok(_permit) = sem2.acquire().await else { return };
                        let r = probe::egress_probe(id, &client, &url, ip, Duration::from_secs(12), now_ms()).await;
                        let detail = r.detail.as_deref().unwrap_or("");
                        // 只在成功/失败切换时 warn / info，持续失败降为 debug
                        if last_ok.swap(r.ok, Ordering::Relaxed) == r.ok {
                            if !r.ok {
                                tracing::debug!(entry = %tags::inbound(id), "出口探测仍失败：{detail}");
                            }
                        } else if r.ok {
                            tracing::info!(entry = %tags::inbound(id), "出口探测恢复");
                        } else {
                            tracing::warn!(entry = %tags::inbound(id), "出口探测失败：{detail}");
                        }
                        sh2.buf.lock().unwrap().push_probe(r);
                    });
                }
            }
        }
        first_pass = false;
    }
}
