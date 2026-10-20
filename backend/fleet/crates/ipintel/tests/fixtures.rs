//! 真实形态抽查：deploy/v2/live/fixtures 里旧系统的 /health 与 /activity（IPINTEL_FIXTURES 指向 api/ 目录）。
//! 只打印统计与分布，不把地址写进断言。
mod common;
use ipintel::classify::ConnType;
use ipintel::{legacy, Config, IpIntel};
use std::collections::BTreeMap;

#[test]
fn legacy_fixtures() {
    let dir = need_dbs!();
    let Some(fx) = common::fixtures_dir() else {
        eprintln!("跳过：没有设置 IPINTEL_FIXTURES");
        return;
    };
    let health: serde_json::Value = serde_json::from_slice(&std::fs::read(fx.join("health.json")).unwrap()).unwrap();
    let activity: serde_json::Value = serde_json::from_slice(&std::fs::read(fx.join("activity__limit=500.json")).unwrap()).unwrap();
    let nodes = legacy::health_nodes(&health);
    let egress = legacy::health_egress(&health);
    let events = legacy::activity_events(&activity);
    assert!(nodes.len() >= 30, "nodes {}", nodes.len());
    assert!(egress.len() >= 30, "egress {}", egress.len());
    assert!(events.len() >= 100, "events {}", events.len());
    assert!(nodes.iter().any(|n| n.short == "t1-01"));
    assert!(nodes.iter().any(|n| n.short.starts_with('r')));

    let cfg = Config { db_dir: dir, ..Config::default() };
    let s = IpIntel::with_conn(cfg, rusqlite::Connection::open_in_memory().unwrap()).unwrap();
    let now = events.iter().map(|e| e.ts).fold(0.0, f64::max);
    s.upsert_nodes(nodes.clone(), now).unwrap();
    let r = s.ingest_sources(events.clone(), now).unwrap();
    let re = s.ingest_egress(egress.clone(), now).unwrap();
    eprintln!("activity: {} 条，新 (节点, 来源) {} 对，异常 {:?}；egress 观测 {}", r.events, r.new_pairs, r.anomalies_opened, re.events);
    // 首次运行：学习期内，来源类检测器只学习；出口首次只建基线
    assert!(r.anomalies_opened.is_empty(), "{:?}", r.anomalies_opened);
    assert!(re.anomalies_opened.is_empty());

    // 出口：按 ASN / 类型统计
    let mut by: BTreeMap<String, usize> = BTreeMap::new();
    for n in &nodes {
        if let Some(ip) = n.egress_ip {
            let x = s.intel(ip, now);
            *by.entry(format!("AS{} {} · {} · {} {}", x.asn.unwrap_or(0), x.isp, x.type_word, x.country.clone().unwrap_or_default(), x.region.clone().unwrap_or_default())).or_default() += 1;
            assert!(x.internal.is_some(), "节点出口应标为内部地址");
        }
    }
    eprintln!("出口分布：{by:#?}");
    // 来源：按运营商 / 城市 / 类型统计
    let mut src: BTreeMap<String, usize> = BTreeMap::new();
    let mut cn = 0;
    let mut total = 0;
    for e in &events {
        let x = s.intel(e.source_ip, now);
        total += 1;
        if x.country.as_deref() == Some("CN") {
            cn += 1;
        }
        *src.entry(format!("{} · {} {} · {} ({})", x.isp, x.region.clone().unwrap_or_default(), x.city.clone().unwrap_or_default(), x.type_word, x.type_confidence)).or_default() += 1;
        if x.internal.is_none() && x.country.as_deref() == Some("CN") {
            assert!(matches!(x.conn_type, ConnType::Residential | ConnType::Mobile), "{} {}", x.isp, x.type_reason);
        }
    }
    eprintln!("来源分布（连接数）：{src:#?}");
    assert!(cn * 10 >= total * 8, "国内来源占比过低 {cn}/{total}");
    for a in s.anomalies(&Default::default()).unwrap()["anomalies"].as_array().unwrap() {
        eprintln!("异常：{} {} · {}", a["severity"], a["kind"], a["summary"]);
    }
    let st = s.status();
    eprintln!("status: {}", serde_json::to_string(&st["rows"]).unwrap());
}
