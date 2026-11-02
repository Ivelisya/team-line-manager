//! 采集任务：主机遥测（两种角色）、Xray 流量与状态、探测调度（线路机）

use crate::host::{self, HostCollector};
use crate::probe;
use crate::state::{nic_path, stats_path, Shared};
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
/// 问 systemctl 退避的上限：每这么多个 SAMPLE_INTERVAL 一次（= DOWN_RECHECK）
const PID_RECHECK_TICKS: u32 = (DOWN_RECHECK.as_secs() / SAMPLE_INTERVAL.as_secs()) as u32;
/// 进程还在时，API 连续失败这么多次才算不可达（单次 statsquery 失败不报 Xray 未运行）
const DOWN_AFTER_FAILS: u32 = 3;

fn ticker(period: Duration) -> tokio::time::Interval {
    let mut t = interval_at(Instant::now() + period, period);
    t.set_missed_tick_behavior(MissedTickBehavior::Skip);
    t
}

/// 主机采样 + SystemInfo（启动时、变化时、每小时兜底重发）；网卡累计值每 60 s 存盘一次、关停时再存一次
pub async fn host_task(sh: Arc<Shared>, mut sd: watch::Receiver<bool>) {
    // 采样读一串 /proc 文件，每 300 s 还要遍历整个 /proc 数线程：都放阻塞线程，采集器在两边来回移交
    let (nic, fixed) = (nic_path(&sh.cfg), sh.cfg.nic_ifaces.clone());
    let (mut hc, mut last_info) = tokio::task::spawn_blocking(|| {
        let hc = HostCollector::new(Some(nic), fixed);
        let info = host::collect_info(hc.ifaces());
        (hc, info)
    })
    .await
    .expect("主机采样任务异常终止");
    sh.buf.lock().unwrap().info = Some(last_info.clone());
    let mut info_sent_at = Instant::now();
    let mut tick = ticker(SAMPLE_INTERVAL);
    let mut n = 0u32;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = sd.changed() => {
                // 关停：网卡累计值存盘，下次启动按它求差，停机期间的流量也算上
                let _ = tokio::task::spawn_blocking(move || hc.save_nic()).await;
                return;
            }
        }
        n += 1;
        let want_info = n.is_multiple_of(30);
        let save_nic = n.is_multiple_of(6);
        let (back, s, cur) = tokio::task::spawn_blocking(move || {
            let s = hc.sample();
            if save_nic {
                hc.save_nic();
            }
            let info = want_info.then(|| host::collect_info(hc.ifaces()));
            (hc, s, info)
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

/// 当前启用的入口 (id, 端口, AnyTLS 公网端口)
fn enabled_entries(sh: &Shared) -> Vec<(uuid::Uuid, u16, Option<u16>)> {
    let g = sh.applied.read().unwrap();
    let Some(d) = g.as_ref() else { return vec![] };
    let at = render::anytls_ports(&d.entries);
    d.entries.iter().filter(|e| e.enabled).map(|e| (e.id, e.port, at.get(&e.id).map(|p| p.0))).collect()
}

/// sing-box 多久查一次状态（systemctl is-active 子进程）。状态只用于界面显示和「订阅里给不给 AnyTLS」，不必像 Xray 那样 10 秒一查
const SINGBOX_STATUS_EVERY: Duration = Duration::from_secs(30);

/// sing-box 没在跑时，隔这么久自己再应用一次（带关占端口连接、reset-failed），不等配置变化
const SINGBOX_RETRY_EVERY: Duration = Duration::from_secs(60);

/// AnyTLS 外壳（sing-box）的状态：启动时查一次，之后每 SINGBOX_STATUS_EVERY 一次，放进缓冲随报告带上；
/// 装了却没在跑（端口被占、撞过 StartLimit 等）就每 SINGBOX_RETRY_EVERY 按已应用的期望状态重新拉起一次
pub async fn singbox_task(sh: Arc<Shared>, sb: Arc<crate::singbox::Singbox>, mut sd: watch::Receiver<bool>) {
    let mut tick = interval_at(Instant::now(), SINGBOX_STATUS_EVERY);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_retry: Option<Instant> = None;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = sd.changed() => return,
        }
        let mut st = sb.status().await;
        if st.installed && !st.running && last_retry.is_none_or(|t| t.elapsed() >= SINGBOX_RETRY_EVERY) {
            let applied = sh.applied.read().unwrap().clone();
            if let Some(d) = applied {
                last_retry = Some(Instant::now());
                sb.apply(&d).await;
                st = sb.status().await;
            }
        }
        sh.buf.lock().unwrap().singbox = Some(st);
    }
}

/// Xray 流量增量 + 连接数 + 进程状态
pub async fn stats_task(sh: Arc<Shared>, cli: XrayCli, mut sd: watch::Receiver<bool>) {
    let cfg = sh.cfg.clone();
    let mut st: stats::StatsState = load_json(&stats_path(&cfg)).unwrap_or_default();
    let boot = host::boot_id();
    let mut tick = ticker(SAMPLE_INTERVAL);
    let mut version: Option<(u32, String)> = None; // (pid, version)
    let mut was_up: Option<bool> = None;
    // 缓存的 Xray 进程身份：(MainPID, starttime ticks)。只在 /proc 里对不上时才重新问 systemctl
    let mut proc: Option<(u32, u64)> = None;
    let mut last_query: Option<Instant> = None;
    let mut last_core: Option<String> = None; // 上次成功计算增量时的进程纪元
    let mut up = false;
    let mut api_fails = 0u32; // xray api 连续失败次数
    let mut down_check_at: Option<Instant> = None; // Xray 不可达时，下次允许重查的时刻
    let mut version_retry_at = Instant::now();
    // 问 systemctl 连续几次都没有 Xray 进程、还要跳过几个 tick 才再问
    let (mut pid_misses, mut pid_skip) = (0u32, 0u32);
    // 上一次算增量时启用的入口：这一轮刚停用的照样算、出样本（见 stats::entry_counters）
    let mut last_ids: HashSet<uuid::Uuid> = HashSet::new();
    let mut tunnel_watch = TunnelWatch::default();
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = sd.changed() => return,
        }
        // 每个 SAMPLE_INTERVAL 确认一次进程：缓存的进程还在只读 /proc，对不上才问 systemctl。
        // 问不到进程时退避：下一个 tick、20 s、40 s，之后每 DOWN_RECHECK 一次——重启的空档下一个 tick 就能找回进程，
        // Xray 停着时不每 10 s 起一个 systemctl 子进程
        let alive = proc.is_some_and(|(p, ticks)| ctl::proc_start_ticks(p) == Some(ticks));
        if !alive && pid_skip > 0 {
            pid_skip -= 1;
        } else if !alive {
            proc = match ctl::main_pid(&cfg.xray_service).await {
                Some(p) => ctl::proc_start_ticks(p).map(|t| (p, t)),
                None => None,
            };
            if proc.is_some() {
                // 进程（重新）出现：结束退避，本轮就查 API
                (down_check_at, pid_misses) = (None, 0);
            } else {
                pid_misses += 1;
                pid_skip = (1u32 << (pid_misses.min(4) - 1)).min(PID_RECHECK_TICKS) - 1;
            }
        }
        // Xray 已确认不可达时，xray api 子进程至多每 DOWN_RECHECK 试一次；其余 tick 只更新状态
        let backoff = !up && down_check_at.is_some_and(|t| Instant::now() < t);
        let pid = proc.map(|p| p.0);
        let core_id = stats::core_id_of(&boot, pid, proc.map(|p| p.1));
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
                let entries = enabled_entries(&sh);
                let ids: HashSet<uuid::Uuid> = entries.iter().map(|&(id, ..)| id).collect();
                let cur = stats::entry_counters(list, &ids.union(&last_ids).copied().collect());
                if let Some(deltas) = stats::compute_if_known(&mut st, core_id.as_deref(), &cur) {
                    last_core = core_id.clone();
                    // 连接数按端口数：Reality 的入口端口，加上 AnyTLS 的公网端口（sing-box 在听，客户连的就是它）
                    let ports: Vec<u16> = entries.iter().flat_map(|&(_, port, at)| std::iter::once(port).chain(at)).collect();
                    let cn = tokio::task::spawn_blocking(move || conns::count_now(&ports)).await.expect("统计连接数的任务异常终止");
                    let n = |p: u16| cn.get(&p).copied().unwrap_or(0);
                    // 上一轮启用、这一轮停用的入口补出最后一段流量（端口已不归它，连接数记 0）
                    let with_conns: Vec<_> = entries
                        .iter()
                        .map(|&(id, port, at)| (id, n(port) + at.map_or(0, n)))
                        .chain(last_ids.difference(&ids).map(|&id| (id, 0)))
                        .collect();
                    last_ids = ids;
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
        let status = XrayStatus { running: up, version: version.as_ref().map(|v| v.1.clone()), applied_generation: sh.applied_gen.load(Ordering::Relaxed) };
        sh.buf.lock().unwrap().xray = Some(status);
        // 穿透隧道：有启用的穿透入口时每轮看一次 Xray 里有没有它的反向出站（lso 很轻）。Xray 不可达时不看（全都会缺）
        let rev = reverse_entries(&sh);
        if !rev.is_empty() && up {
            if let Ok(outs) = cli.lso().await {
                let down = tunnel_watch.update(&rev, &outs, Instant::now());
                sh.buf.lock().unwrap().tunnels_down = down;
            }
        } else if rev.is_empty() {
            tunnel_watch.since.clear();
            sh.buf.lock().unwrap().tunnels_down.clear();
        }
    }
}

/// 已应用的期望状态里启用的穿透入口
fn reverse_entries(sh: &Shared) -> Vec<uuid::Uuid> {
    let a = sh.applied.read().unwrap();
    a.as_ref().map(|d| d.entries.iter().filter(|e| e.enabled && matches!(e.outbound, fleet_proto::Outbound::Reverse { .. })).map(|e| e.id).collect()).unwrap_or_default()
}

/// 穿透隧道有没有挂上：Xray 里有 `rvs-<入口>` 出站 = bridge 自这个入站建立以来连上过。
/// 入站被换掉（轮换凭据、停用后再启用、改 REALITY 参数）时 Xray 连带删掉这个出站，但 bridge 那条旧 TCP 连接不会断、
/// 也就不会重连（26.3.27 实测：一直不通），只能让节点重启 bridge——缺席满 `TUNNEL_DOWN_AFTER_S` 秒就随上报报给中控，
/// 中控在节点的下一份回执里要它重启（`ReportAck::bridge_kick`）。bridge 正常重连只要约 2 秒，到不了这个时长
#[derive(Default)]
pub struct TunnelWatch {
    /// 入口 → 第一次发现缺席的时刻
    since: HashMap<uuid::Uuid, Instant>,
}

impl TunnelWatch {
    pub fn update(&mut self, rev: &[uuid::Uuid], outbounds: &[String], now: Instant) -> Vec<uuid::Uuid> {
        let present: HashSet<&str> = outbounds.iter().map(|s| s.as_str()).collect();
        self.since.retain(|id, _| rev.contains(id) && !present.contains(tags::reverse(*id).as_str()));
        let after = Duration::from_secs(fleet_proto::TUNNEL_DOWN_AFTER_S);
        let mut down = vec![];
        for id in rev {
            if present.contains(tags::reverse(*id).as_str()) {
                continue;
            }
            let t0 = *self.since.entry(*id).or_insert(now);
            if now.duration_since(t0) >= after {
                down.push(*id);
            }
        }
        down.sort();
        down
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
    /// 上一次出口探测是否成功（初始当作成功）：只在成功/失败切换时打日志；失败期间出口探测按 TCP 的间隔（默认 30 s）
    egress_ok: Arc<AtomicBool>,
    /// 上一次 TCP 探测是否成功（初始当作成功）
    tcp_ok: Arc<AtomicBool>,
    /// 结果刚翻转（探测任务置上）：约 `RECHECK` 后补探一次确认
    tcp_flipped: Arc<AtomicBool>,
    egress_flipped: Arc<AtomicBool>,
    /// 下一次是确认探测：结果出来马上叫上报器发（`Shared::report_now`）
    tcp_confirm: bool,
    /// 出口探测的确认标记要能让探测任务放回去（回显服务不通、没出样本时），所以是共享的
    egress_confirm: Arc<AtomicBool>,
}

/// 探测结果翻转后这么久补探一次确认（中控的出口 / TCP 告警连续 2 次成功就恢复）
const RECHECK: Duration = Duration::from_secs(10);

/// 翻转或确认探测的结果：记下要补探、叫上报器马上发
fn settle(sh: &Shared, last_ok: &AtomicBool, flipped: &AtomicBool, ok: bool, confirm: bool) -> bool {
    let flip = last_ok.swap(ok, Ordering::Relaxed) != ok;
    if flip {
        flipped.store(true, Ordering::Release);
    }
    if flip || confirm {
        sh.report_now.notify_one();
    }
    flip
}

/// 探测任务结束（含 panic）时清掉在途标记
struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(crate) fn stagger(id: uuid::Uuid, period: Duration) -> Duration {
    Duration::from_millis((id.as_u128() % (period.as_millis().max(1))) as u64)
}

/// 入口在周期里的固定时刻：`t0 + stagger(id) + k·period` 里第一个不早于 `after` 的。每次派完都按它排下一次，
/// 各入口始终落在周期里各自的那一秒，不会因为一起被重探（新入口、出站变了）或调度漂移而挤到同一秒一齐发
fn next_slot(t0: Instant, after: Instant, period: Duration, id: uuid::Uuid) -> Instant {
    let first = t0 + stagger(id, period);
    if after <= first {
        return first;
    }
    let p = period.as_millis().max(1);
    let k = (after - first).as_millis().div_ceil(p);
    first + Duration::from_millis((k * p) as u64)
}

/// 运行中尽快重探（新入口、出站变了）：基础等待加按 id 散开的 0–3 秒，一批入口同时变也不在同一秒发
fn soon(now: Instant, base_s: u64, id: uuid::Uuid) -> Instant {
    now + Duration::from_secs(base_s) + stagger(id, Duration::from_secs(3))
}

fn entry_sig(e: &DesiredEntry) -> String {
    format!("{}|{:?}", e.port, e.outbound)
}

/// 入口 → (探测端口, 经该端口出去的客户端)
type ProbeClients = HashMap<uuid::Uuid, (u16, reqwest::Client)>;

/// 探测调度：TCP 每 probe_interval_s（默认 30 s），出口每 2 倍间隔（默认 60 s，出口探测失败期间同 TCP）；各入口落在周期里各自的时刻（next_slot）。
/// 某入口的结果翻转（失败↔成功）时约 10 秒后补探一次确认，翻转与确认的结果都叫上报器马上发。
/// `direct`：不走代理的客户端，出口探测失败时直连回显服务核对（`probe::egress_probe_checked`）
pub async fn probe_task(sh: Arc<Shared>, direct: reqwest::Client, mut sd: watch::Receiver<bool>) {
    let mut sched: HashMap<uuid::Uuid, Sched> = HashMap::new();
    let t0 = Instant::now(); // 各入口探测时刻的基准（next_slot）
    let sem = Arc::new(Semaphore::new(4)); // 出口探测并发上限
    let tcp_sem = Arc::new(Semaphore::new(8)); // TCP 探测并发上限（单独一个，不排在 12 s 的出口探测后面）
    let mut tick = interval_at(Instant::now() + Duration::from_secs(2), Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut first_pass = true;
    // 因上一次出口探测还没结束而跳过的次数（只用于日志限流）
    let mut skipped = 0u64;
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
            for (id, port) in render::probe_ports(&d.entries) {
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
                        s.next_tcp = soon(now, 1, e.id);
                        s.next_egress = soon(now, 4, e.id);
                    }
                }
            }
        }
        let probes = &probes_cache.as_ref().expect("刚填充").1;
        let live: HashSet<_> = d.entries.iter().filter(|e| e.enabled).map(|e| e.id).collect();
        sched.retain(|id, _| live.contains(id));
        for e in d.entries.iter().filter(|e| e.enabled) {
            let s = sched.entry(e.id).or_insert_with(|| {
                // 启动时按各自的时刻错峰（出口探测至少等 5 秒）；运行中新增的入口尽快探一次，让界面马上有结果
                let (a, b) = if first_pass {
                    (next_slot(t0, now, tcp_every, e.id), next_slot(t0, now + Duration::from_secs(5), egress_every, e.id))
                } else {
                    (soon(now, 1, e.id), soon(now, 4, e.id))
                };
                let flag = |v| Arc::new(AtomicBool::new(v));
                Sched {
                    sig: entry_sig(e),
                    next_tcp: a,
                    next_egress: b,
                    tcp_busy: flag(false),
                    egress_busy: flag(false),
                    egress_ok: flag(true),
                    tcp_ok: flag(true),
                    tcp_flipped: flag(false),
                    egress_flipped: flag(false),
                    tcp_confirm: false,
                    egress_confirm: flag(false),
                }
            });
            if s.tcp_flipped.swap(false, Ordering::AcqRel) {
                (s.next_tcp, s.tcp_confirm) = (now + RECHECK, true);
            }
            if s.egress_flipped.swap(false, Ordering::AcqRel) {
                s.next_egress = now + RECHECK;
                s.egress_confirm.store(true, Ordering::Release);
            }
            // 直出入口没有节点（addr 为 None）：不做 TCP 探测，只靠出口探测
            if let Some((host, port)) = e.outbound.addr().filter(|_| now >= s.next_tcp && !s.tcp_busy.swap(true, Ordering::AcqRel)) {
                // 下一次回到这个入口自己的时刻；至少隔半个周期（刚重探过的不会紧接着又探）
                s.next_tcp = next_slot(t0, now + tcp_every / 2, tcp_every, e.id);
                let busy = BusyGuard(s.tcp_busy.clone());
                let (sh2, id, host) = (sh.clone(), e.id, host.to_string());
                let (tcp_sem2, last_ok, flipped, confirm) = (tcp_sem.clone(), s.tcp_ok.clone(), s.tcp_flipped.clone(), std::mem::take(&mut s.tcp_confirm));
                tokio::spawn(async move {
                    let _busy = busy;
                    let Ok(_permit) = tcp_sem2.acquire().await else { return };
                    let r = probe::tcp_probe(id, &host, port, probe::TCP_TIMEOUT, now_ms()).await;
                    let ok = r.ok;
                    sh2.buf.lock().unwrap().push_probe(r);
                    settle(&sh2, &last_ok, &flipped, ok, confirm);
                });
            }
            if now >= s.next_egress {
                let every = if s.egress_ok.load(Ordering::Relaxed) { egress_every } else { tcp_every };
                s.next_egress = next_slot(t0, now + every / 2, every, e.id);
                if let Some((_, client)) = probes.get(&e.id) {
                    if s.egress_busy.swap(true, Ordering::AcqRel) {
                        skipped += 1;
                        if skipped <= 3 || skipped.is_multiple_of(100) {
                            tracing::warn!(entry = %tags::inbound(e.id), skipped_total = skipped, "上一次出口探测还没结束，跳过本次");
                        }
                        continue;
                    }
                    let busy = BusyGuard(s.egress_busy.clone());
                    let (sh2, sem2, id, url, ip, client, direct) = (sh.clone(), sem.clone(), e.id, d.relay.echo_url.clone(), e.expect_egress_ip, client.clone(), direct.clone());
                    let (last_ok, flipped, confirm) = (s.egress_ok.clone(), s.egress_flipped.clone(), s.egress_confirm.swap(false, Ordering::AcqRel));
                    let confirm_flag = s.egress_confirm.clone();
                    tokio::spawn(async move {
                        let _busy = busy;
                        let Ok(_permit) = sem2.acquire().await else { return };
                        // 回显服务本身不通（中控重启、部署）时不出样本：既不算这个入口出口失败，也不动成功/失败的切换日志；
                        // 确认标记放回去，下一次探测（回显恢复后）仍按确认探测，结果出来马上叫上报器发
                        let Some(r) = probe::egress_probe_checked(id, &client, &direct, &url, probe::EGRESS_ALT_URL, ip, Duration::from_secs(12), now_ms()).await else {
                            if confirm {
                                confirm_flag.store(true, Ordering::Release);
                            }
                            return;
                        };
                        let (ok, detail) = (r.ok, r.detail.clone().unwrap_or_default());
                        sh2.buf.lock().unwrap().push_probe(r);
                        // 只在成功/失败切换时 warn / info，持续失败降为 debug
                        if !settle(&sh2, &last_ok, &flipped, ok, confirm) {
                            if !ok {
                                tracing::debug!(entry = %tags::inbound(id), "出口探测仍失败：{detail}");
                            }
                        } else if ok {
                            tracing::info!(entry = %tags::inbound(id), "出口探测恢复");
                        } else {
                            tracing::warn!(entry = %tags::inbound(id), "出口探测失败：{detail}");
                        }
                    });
                }
            }
        }
        first_pass = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_slots_keep_each_entry_on_its_own_second() {
        let t0 = Instant::now();
        let p = Duration::from_secs(30);
        let id = uuid::Uuid::from_u128(12_345); // 相位 12.345 s
        let ph = Duration::from_millis(12_345);
        assert_eq!(next_slot(t0, t0, p, id), t0 + ph);
        assert_eq!(next_slot(t0, t0 + ph, p, id), t0 + ph, "正好在时刻上：就是它");
        assert_eq!(next_slot(t0, t0 + ph + Duration::from_millis(1), p, id), t0 + ph + p);
        // 派发时刻晚了几秒（排队、重探），下一次仍回到自己的相位，不顺延
        let fired = t0 + ph + p * 3 + Duration::from_secs(4);
        assert_eq!(next_slot(t0, fired + p / 2, p, id), t0 + ph + p * 4);
        // 不同入口落在不同的时刻
        let other = uuid::Uuid::from_u128(12_345 + 7_000);
        assert_ne!(next_slot(t0, fired, p, id), next_slot(t0, fired, p, other));
        // 一批入口同时重探：散在 base..base+3 秒
        let now = t0 + Duration::from_secs(100);
        assert_eq!(soon(now, 1, uuid::Uuid::from_u128(2_500)), now + Duration::from_millis(3_500));
        assert!((0..50u128).map(|n| soon(now, 1, uuid::Uuid::from_u128(n * 977))).all(|s| s >= now + Duration::from_secs(1) && s < now + Duration::from_secs(4)));
    }
}

#[cfg(test)]
mod tunnel_tests {
    use super::*;

    #[test]
    fn tunnel_down_after_30s_absent_and_clears_when_back() {
        let (a, b) = (uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2));
        let mut w = TunnelWatch::default();
        let t0 = Instant::now();
        let outs = |ids: &[uuid::Uuid]| ids.iter().map(|i| tags::reverse(*i)).chain(["deny".to_string()]).collect::<Vec<_>>();
        // a 挂着、b 缺席：刚缺席不报，满 30 秒才报
        assert!(w.update(&[a, b], &outs(&[a]), t0).is_empty());
        assert!(w.update(&[a, b], &outs(&[a]), t0 + Duration::from_secs(29)).is_empty());
        assert_eq!(w.update(&[a, b], &outs(&[a]), t0 + Duration::from_secs(31)), vec![b]);
        // b 回来了：不再报；再缺席从头计时
        assert!(w.update(&[a, b], &outs(&[a, b]), t0 + Duration::from_secs(40)).is_empty());
        assert!(w.update(&[a, b], &outs(&[a]), t0 + Duration::from_secs(50)).is_empty());
        assert_eq!(w.update(&[a, b], &outs(&[a]), t0 + Duration::from_secs(81)), vec![b]);
        // 入口不再是穿透（切回普通 / 删掉）：忘掉
        assert!(w.update(&[a], &outs(&[a]), t0 + Duration::from_secs(90)).is_empty());
        assert!(w.since.is_empty());
    }
}
