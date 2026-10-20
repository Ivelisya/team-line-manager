//! 运维 / 验收小工具：
//!   cargo run --release --example ipintel -- lookup <db_dir> <ip>...      查询并打印 JSON
//!   cargo run --release --example ipintel -- bench  <db_dir> <n>          随机地址压测 + 打印内存（RssAnon / RssFile）
//!   cargo run --release --example ipintel -- mem    <db_dir>              只加载库，打印内存
use ipintel::lookup::Dbs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let cmd = a.get(1).map(String::as_str).unwrap_or("");
    let dir = Path::new(a.get(2).map(String::as_str).unwrap_or("/var/lib/fleetd/ipintel"));
    let before = ipintel::mem::rss();
    let t = Instant::now();
    let dbs = Dbs::open(dir);
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    match cmd {
        "lookup" => {
            for ip in &a[3..] {
                let ip: IpAddr = ip.parse().expect("ip");
                println!("{}", serde_json::to_string_pretty(&dbs.lookup(ip, None)).unwrap());
            }
        }
        "bench" | "mem" => {
            let n: usize = a.get(3).and_then(|x| x.parse().ok()).unwrap_or(if cmd == "mem" { 0 } else { 200_000 });
            let after_load = ipintel::mem::rss();
            let mut x: u64 = 0x9E3779B97F4A7C15;
            let t = Instant::now();
            let mut hits = 0usize;
            for i in 0..n {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let ip = if i % 5 == 4 {
                    IpAddr::V6(Ipv6Addr::new(0x2400 | ((x >> 48) as u16 & 0x0fff), (x >> 32) as u16, (x >> 16) as u16, x as u16, 0, 0, 0, 1))
                } else {
                    IpAddr::V4(Ipv4Addr::from((x >> 16) as u32))
                };
                let r = dbs.lookup(ip, None);
                if r.asn.is_some() {
                    hits += 1;
                }
            }
            let el = t.elapsed().as_secs_f64();
            println!(
                "{}",
                serde_json::json!({"loaded": dbs.loaded(), "errors": dbs.errors, "files": dbs.files, "load_ms": load_ms,
                    "hosting_asns_from_iptoasn": dbs.hosting_scan.len(),
                    "lookups": n, "with_asn": hits, "us_per_lookup": if n > 0 { el * 1e6 / n as f64 } else { 0.0 },
                    "rss_before_load": before, "rss_after_load": after_load, "rss_after_lookups": ipintel::mem::rss()})
            );
        }
        _ => eprintln!("usage: ipintel lookup|bench|mem <db_dir> [...]"),
    }
}
