//! 采集任务：主机遥测（两种角色）、Xray 流量与状态、探测调度（线路机）

use crate::host::{self, HostCollector};
use crate::probe;
use crate::state::{stats_path, Shared};
use crate::util::{load_json, now_ms};
use crate::xray::cli::XrayCli;
use crate::xray::{conns, ctl, render, stats};
use fleet_proto::{tags, DesiredEntry, XrayStatus};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Semaphore};
use tokio::time::{interval_at, Instant, MissedTickBehavior};

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

fn ticker(period: Duration) -> tokio::time::Interval {
    let mut t = interval_at(Instant::now() + period, period);
    t.set_missed_tick_behavior(MissedTickBehavior::Skip);
    t
}

/// 主机采样 + SystemInfo（启动时、变化时、每小时兜底重发）
pub async fn host_task(sh: Arc<Shared>, mut sd: watch::Receiver<bool>) {
    let mut hc = HostCollector::new();
    let mut last_info = host::collect_info();
    sh.buf.lock().unwrap().info = Some(last_info.clone());
    let mut info_sent_at = Instant::now();
    let mut tick = ticker(SAMPLE_INTERVAL);
    let mut n = 0u32;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = sd.changed() => return,
        }
        let s = hc.sample();
        let mut b = sh.buf.lock().unwrap();
        b.push_host(s);
        n += 1;
        if n % 30 == 0 {
            let cur = host::collect_info();
            if cur != last_info || info_sent_at.elapsed() > Duration::from_secs(3600) {
                last_info = cur.clone();
                b.info = Some(cur);
                info_sent_at = Instant::now();
            }
        }
    }
}

/// 当前启用的入口 (id, port)
fn enabled_entries(sh: &Shared) -> Vec<DesiredEntry> {
    sh.applied.read().unwrap().as_ref().map(|d| d.entries.iter().filter(|e| e.enabled).cloned().collect()).unwrap_or_default()
}

/// Xray 流量增量 + 连接数 + 进程状态
pub async fn stats_task(sh: Arc<Shared>, cli: XrayCli, mut sd: watch::Receiver<bool>) {
    let cfg = sh.cfg.clone();
    let mut st: stats::StatsState = load_json(&stats_path(&cfg)).unwrap_or_default();
    let boot = host::boot_id();
    let mut tick = ticker(SAMPLE_INTERVAL);
    let mut version: Option<(u32, String)> = None; // (pid, version)
    let mut was_up: Option<bool> = None;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = sd.changed() => return,
        }
        let entries = enabled_entries(&sh);
        let pid = ctl::main_pid(&cfg.xray_service).await;
        let start = pid.and_then(ctl::proc_start);
        let core_id = match (pid, start) {
            (Some(p), Some((t, _))) => format!("{boot}:{p}:{t}"),
            _ => format!("{boot}:nopid"),
        };
        let res = cli.user_stats().await;
        let up = res.is_ok();
        if was_up != Some(up) {
            match &res {
                Ok(_) => tracing::info!("Xray API 可达"),
                Err(e) => tracing::warn!("Xray API 不可达：{e:#}"),
            }
            was_up = Some(up);
        }
        let ts = now_ms();
        if let Ok(list) = res {
            let cur: HashMap<String, u64> = list.into_iter().collect();
            let deltas = stats::compute(&mut st, &core_id, &cur);
            let ports: Vec<u16> = entries.iter().map(|e| e.port).collect();
            let cn = conns::count_now(&ports);
            let with_conns: Vec<_> = entries.iter().map(|e| (e.id, cn.get(&e.port).copied().unwrap_or(0))).collect();
            let samples = stats::to_samples(&deltas, &with_conns, ts);
            let mut b = sh.buf.lock().unwrap();
            b.push_entries(samples);
            b.stats_commit = Some(st.clone());
        }
        // 版本：进程换代时取一次
        if up {
            if let Some(p) = pid {
                if version.as_ref().map(|v| v.0) != Some(p) {
                    if let Ok(v) = cli.version().await {
                        version = Some((p, v));
                    }
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
}

fn stagger(id: uuid::Uuid, period: Duration) -> Duration {
    Duration::from_millis((id.as_u128() % (period.as_millis().max(1))) as u64)
}

fn entry_sig(e: &DesiredEntry) -> String {
    format!("{}|{:?}", e.port, e.outbound)
}

fn outbound_hostport(e: &DesiredEntry) -> (String, u16) {
    use fleet_proto::Outbound::*;
    match &e.outbound {
        Shadowsocks { host, port, .. } | Socks5 { host, port, .. } | Http { host, port, .. } => (host.clone(), *port),
    }
}

/// 探测调度：TCP 每 probe_interval_s（默认 30 s），出口每 2 倍间隔（默认 60 s）
pub async fn probe_task(sh: Arc<Shared>, mut sd: watch::Receiver<bool>) {
    let mut sched: HashMap<uuid::Uuid, Sched> = HashMap::new();
    let sem = Arc::new(Semaphore::new(4));
    let mut tick = interval_at(Instant::now() + Duration::from_secs(2), Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut first_pass = true;
    let api_port = sh.cfg.api_port();
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = sd.changed() => return,
        }
        let Some(d) = sh.applied.read().unwrap().clone() else { continue };
        let tcp_every = Duration::from_secs(d.relay.probe_interval_s.clamp(5, 3600) as u64);
        let egress_every = tcp_every * 2;
        let probes = render::probe_ports(&d.entries, api_port);
        let now = Instant::now();
        let live: HashSet<_> = d.entries.iter().filter(|e| e.enabled).map(|e| e.id).collect();
        sched.retain(|id, _| live.contains(id));
        for e in d.entries.iter().filter(|e| e.enabled) {
            let sig = entry_sig(e);
            let s = sched.entry(e.id).or_insert_with(|| {
                // 启动时错峰；运行中新增/变化的入口尽快探一次，让界面马上有结果
                let (a, b) = if first_pass { (stagger(e.id, tcp_every), Duration::from_secs(5) + stagger(e.id, egress_every)) } else { (Duration::from_secs(1), Duration::from_secs(4)) };
                Sched { sig: sig.clone(), next_tcp: now + a, next_egress: now + b }
            });
            if s.sig != sig {
                s.sig = sig;
                s.next_tcp = now + Duration::from_secs(1);
                s.next_egress = now + Duration::from_secs(4);
            }
            if now >= s.next_tcp {
                s.next_tcp = now + tcp_every;
                let (host, port) = outbound_hostport(e);
                let (sh2, id) = (sh.clone(), e.id);
                tokio::spawn(async move {
                    let r = probe::tcp_probe(id, &host, port, Duration::from_secs(5), now_ms()).await;
                    sh2.buf.lock().unwrap().push_probe(r);
                });
            }
            if now >= s.next_egress {
                s.next_egress = now + egress_every;
                if let Some(pp) = probes.get(&e.id).copied() {
                    let (sh2, sem2, id, url, ip) = (sh.clone(), sem.clone(), e.id, d.relay.echo_url.clone(), e.expect_egress_ip);
                    tokio::spawn(async move {
                        let Ok(_permit) = sem2.acquire().await else { return };
                        let r = probe::egress_probe(id, pp, &url, ip, Duration::from_secs(12), now_ms()).await;
                        if !r.ok {
                            tracing::warn!(entry = %tags::inbound(id), "出口探测失败：{}", r.detail.as_deref().unwrap_or(""));
                        }
                        sh2.buf.lock().unwrap().push_probe(r);
                    });
                }
            }
        }
        first_pass = false;
    }
}
