//! fleet-probe：探测库的独立入口（验收、对照、现场排障用；生产里由 relayd 内嵌调用库）。
//!
//! ```text
//! fleet-probe once  [来源] [选项]                 跑一轮，JSON 行输出到 stdout，汇总到 stderr
//! fleet-probe run   [来源] [选项] --db F [--duration S]   常驻，写 SQLite；每 60 s 在 stderr 打一行 CPU
//! fleet-probe batch --db F --relay NAME [--limit N] [--ack]  打印待推送批次（fleet.probe.v1）
//!
//! 来源（可叠加，按出站 tag 合并）：
//!   --xray-config F   Xray JSON（可重复）
//!   --xui-db F        3x-ui 数据库（只读，取 xrayTemplateConfig）
//!   --assets F        旧 komari-relay-probe config.json（只取 uuid/name/ip/outbound_tag）
//! 选项：
//!   --only S          只测 tag/name/node 含 S 的节点        --limit-nodes N
//!   --expect-all IP   所有节点的期望出口都设为 IP（本地 ssserver 测试用）
//!   --kinds tcp,egress,control,udp,ipv6   （once 默认 tcp,egress,control）
//!   --attempts N  --retry-gap S  --timeout S  --concurrency N  --udp（对所有节点做 UDP）
//!   --tcp-interval S  --egress-interval S  --replicate N（把目标复制 N 份，压测 CPU 用）
//! ```
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fleet_probe::checks::now;
use fleet_probe::scheduler::{ProbeConfig, Prober};
use fleet_probe::store::ProbeStore;
use fleet_probe::targets::{self, NodeTarget};
use fleet_probe::{CheckKind, ProbeResult, Status};

struct Args {
    cmd: String,
    xray: Vec<PathBuf>,
    xui: Option<PathBuf>,
    assets: Option<PathBuf>,
    only: Option<String>,
    limit_nodes: Option<usize>,
    expect_all: Option<String>,
    kinds: Option<Vec<CheckKind>>,
    db: Option<PathBuf>,
    relay: String,
    limit: usize,
    ack: bool,
    duration: Option<u64>,
    udp: bool,
    replicate: usize,
    cfg: ProbeConfig,
}

fn usage() -> ! {
    eprintln!("用法：fleet-probe once|run|batch [--xray-config F]... [--xui-db F] [--assets F] [--only S] [--kinds tcp,egress,control,udp,ipv6] [--db F] …（详见源码头注释）");
    std::process::exit(2)
}

fn parse() -> Args {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_else(|| usage());
    let mut a = Args {
        cmd,
        xray: vec![],
        xui: None,
        assets: None,
        only: None,
        limit_nodes: None,
        expect_all: None,
        kinds: None,
        db: None,
        relay: "relay".into(),
        limit: 2000,
        ack: false,
        duration: None,
        udp: false,
        replicate: 1,
        cfg: ProbeConfig::default(),
    };
    let secs = |v: Option<String>| Duration::from_secs_f64(v.and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()));
    while let Some(k) = it.next() {
        match k.as_str() {
            "--xray-config" => a.xray.push(it.next().unwrap_or_else(|| usage()).into()),
            "--xui-db" => a.xui = it.next().map(Into::into),
            "--assets" => a.assets = it.next().map(Into::into),
            "--only" => a.only = it.next(),
            "--limit-nodes" => a.limit_nodes = it.next().and_then(|s| s.parse().ok()),
            "--expect-all" => a.expect_all = it.next(),
            "--kinds" => {
                a.kinds = Some(it.next().unwrap_or_default().split(',').filter_map(CheckKind::parse).collect());
            }
            "--db" => a.db = it.next().map(Into::into),
            "--relay" => a.relay = it.next().unwrap_or_else(|| usage()),
            "--limit" => a.limit = it.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()),
            "--ack" => a.ack = true,
            "--duration" => a.duration = it.next().and_then(|s| s.parse().ok()),
            "--udp" => a.udp = true,
            "--replicate" => a.replicate = it.next().and_then(|s| s.parse().ok()).unwrap_or(1).max(1),
            "--attempts" => a.cfg.egress_attempts = it.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()),
            "--retry-gap" => a.cfg.retry_gap = secs(it.next()),
            "--timeout" => a.cfg.check_timeout = secs(it.next()),
            "--concurrency" => a.cfg.concurrency = it.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()),
            "--tcp-interval" => a.cfg.tcp_interval = secs(it.next()),
            "--egress-interval" => a.cfg.egress_interval = secs(it.next()),
            "--no-ipv6" => a.cfg.ipv6_interval = None,
            _ => usage(),
        }
    }
    a
}

fn load_targets(a: &Args) -> Vec<NodeTarget> {
    let mut sources = Vec::new();
    for p in &a.xray {
        sources.push(targets::from_xray_file(p).unwrap_or_else(|e| die(&e)));
    }
    if let Some(p) = &a.xui {
        sources.push(targets::from_xui_db(p).unwrap_or_else(|e| die(&e)));
    }
    let ids = match &a.assets {
        Some(p) => targets::identities_from_legacy_assets(p).unwrap_or_else(|e| die(&e)),
        None => Default::default(),
    };
    let mut list = targets::merge(sources, &ids);
    if let Some(s) = &a.only {
        list.retain(|t| t.tag.contains(s.as_str()) || t.name.contains(s.as_str()) || t.node.contains(s.as_str()));
    }
    if let Some(n) = a.limit_nodes {
        list.truncate(n);
    }
    for t in list.iter_mut() {
        if let Some(ip) = &a.expect_all {
            t.expected_ip = ip.clone();
        }
        t.udp |= a.udp;
    }
    if a.replicate > 1 {
        let base = list.clone();
        for i in 1..a.replicate {
            for t in &base {
                let mut c = t.clone();
                c.node = format!("{}#{i}", t.node);
                c.tag = format!("{}#{i}", t.tag);
                list.push(c);
            }
        }
    }
    list
}

fn die(m: &str) -> ! {
    eprintln!("fleet-probe: {m}");
    std::process::exit(1)
}

/// 本进程累计 CPU 秒（/proc/self/stat 的 utime+stime）
fn cpu_seconds() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: f64 = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0)
}

fn rss_kb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.split_whitespace().next()?.parse().ok())
}

fn summarize(rs: &[ProbeResult], names: &std::collections::HashMap<String, String>) {
    eprintln!("{:<34} {:<8} {:<8} {:>5} {:>9} {:<16} {:<18} {}", "节点", "检查", "结果", "尝试", "耗时ms", "出口IP", "原因", "来源");
    for r in rs {
        let name = names.get(&r.node).cloned().unwrap_or_else(|| r.node.clone());
        eprintln!(
            "{:<34} {:<8} {:<8} {:>5} {:>9} {:<16} {:<18} {}",
            name.chars().take(34).collect::<String>(),
            r.check.as_str(),
            r.status.as_str(),
            r.attempt.map(|a| a.to_string()).unwrap_or_default(),
            r.value_ms.map(|v| format!("{v:.0}")).unwrap_or_else(|| "-".into()),
            r.actual_ip.clone().unwrap_or_default(),
            r.reason_code.clone().unwrap_or_default(),
            r.source.clone().unwrap_or_default()
        );
    }
}

fn main() {
    let a = parse();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio");
    match a.cmd.as_str() {
        "once" => {
            let list = load_targets(&a);
            if list.is_empty() {
                die("没有找到任何 SS 出站");
            }
            let kinds = a.kinds.clone().unwrap_or_else(|| vec![CheckKind::Control, CheckKind::Tcp, CheckKind::Egress]);
            let names: std::collections::HashMap<String, String> = list.iter().map(|t| (t.node.clone(), t.name.clone())).collect();
            eprintln!("fleet-probe once：{} 个节点，检查 {:?}", list.len(), kinds);
            let (c0, t0) = (cpu_seconds(), Instant::now());
            let (p, _rx) = Prober::new(a.cfg.clone());
            let mut rs = rt.block_on(p.run_once(&list, &kinds));
            rs.sort_by(|x, y| (x.node.as_str(), x.check, x.attempt).cmp(&(y.node.as_str(), y.check, y.attempt)));
            for r in &rs {
                println!("{}", serde_json::to_string(r).unwrap());
            }
            summarize(&rs, &names);
            if let Some(dbp) = &a.db {
                let mut s = ProbeStore::open(dbp).unwrap_or_else(|e| die(&e.to_string()));
                s.insert(&mut rs).unwrap_or_else(|e| die(&e.to_string()));
            }
            let ok = rs.iter().filter(|r| r.status == Status::Ok).count();
            eprintln!(
                "共 {} 条，ok {}；墙钟 {:.1} s，CPU {:.3} s，RSS {} KB",
                rs.len(),
                ok,
                t0.elapsed().as_secs_f64(),
                cpu_seconds().zip(c0).map(|(a, b)| a - b).unwrap_or(-1.0),
                rss_kb().unwrap_or(0)
            );
        }
        "run" => {
            let list = load_targets(&a);
            if list.is_empty() {
                die("没有找到任何 SS 出站");
            }
            let dbp = a.db.clone().unwrap_or_else(|| die("run 需要 --db"));
            let mut store = ProbeStore::open(&dbp).unwrap_or_else(|e| die(&e.to_string()));
            let mut cfg = a.cfg.clone();
            if let Some(k) = &a.kinds {
                if !k.contains(&CheckKind::Ipv6) {
                    cfg.ipv6_interval = None;
                }
                if !k.contains(&CheckKind::Udp) {
                    cfg.udp_interval = None;
                }
            }
            eprintln!("fleet-probe run：{} 个节点，库 {}", list.len(), dbp.display());
            let (p, mut rx) = Prober::new(cfg);
            let (_tx, trx) = tokio::sync::watch::channel(list);
            let dur = a.duration.map(Duration::from_secs);
            rt.block_on(async move {
                let run = tokio::spawn(p.run(trx));
                let (c0, t0) = (cpu_seconds(), Instant::now());
                let (mut cl, mut tl) = (c0, Instant::now());
                let mut tick = tokio::time::interval(Duration::from_secs(60));
                tick.tick().await;
                let mut buf = Vec::new();
                let mut counts = (0usize, 0usize);
                loop {
                    tokio::select! {
                        Some(r) = rx.recv() => {
                            counts.0 += 1;
                            if r.status == Status::Ok { counts.1 += 1; }
                            buf.push(r);
                            if buf.len() >= 64 { let _ = store.insert(&mut buf); buf.clear(); }
                        }
                        _ = tick.tick() => {
                            let _ = store.insert(&mut buf); buf.clear();
                            let _ = store.prune(now());
                            let c = cpu_seconds();
                            let pct = c.zip(cl).map(|(a, b)| (a - b) / tl.elapsed().as_secs_f64() * 100.0).unwrap_or(-1.0);
                            eprintln!("[{:.0}] 结果 {} 条（ok {}），近 60 s CPU {:.3}%（单核），RSS {} KB", now(), counts.0, counts.1, pct, rss_kb().unwrap_or(0));
                            cl = c; tl = Instant::now();
                            if dur.is_some_and(|d| t0.elapsed() >= d) { break; }
                        }
                    }
                }
                let _ = store.insert(&mut buf);
                let total = cpu_seconds().zip(c0).map(|(a, b)| (a - b) / t0.elapsed().as_secs_f64() * 100.0).unwrap_or(-1.0);
                eprintln!("结束：{:.0} s，结果 {} 条，平均 CPU {:.3}%（单核）= {:.3}%（2 vCPU 总量）", t0.elapsed().as_secs_f64(), counts.0, total, total / 2.0);
                run.abort();
            });
        }
        "batch" => {
            let dbp = a.db.clone().unwrap_or_else(|| die("batch 需要 --db"));
            let s = ProbeStore::open(&dbp).unwrap_or_else(|e| die(&e.to_string()));
            match s.pending(&a.relay, a.limit, vec![], now()).unwrap_or_else(|e| die(&e.to_string())) {
                Some(b) => {
                    println!("{}", serde_json::to_string_pretty(&b).unwrap());
                    if a.ack {
                        s.ack(b.last_id).unwrap_or_else(|e| die(&e.to_string()));
                    }
                }
                None => eprintln!("没有待推送的数据"),
            }
        }
        _ => usage(),
    }
}
