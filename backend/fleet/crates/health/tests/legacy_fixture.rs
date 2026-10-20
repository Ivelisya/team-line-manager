//! 对照 1：生产快照。同一份 overview 输入，Rust 移植的判定与旧 monitor 当时的输出逐节点、逐检查一致。
use fleet_health::legacy::Monitor;
use serde_json::Value;

fn load(name: &str) -> Value {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn production_snapshot_matches_legacy_monitor() {
    let ov = load("legacy-overview.json");
    let hl = load("legacy-health.json");
    let now = hl["checked_at"].as_f64().unwrap();
    let assets = ov["assets"].as_array().unwrap().clone();
    let mut m = Monitor::new();
    let ev = m.evaluate(&assets, &ov["relays"], now);
    assert_eq!(ev.nodes.len(), 41);
    let mut compared = 0;
    for n in &ev.nodes {
        let want = &hl["nodes"][&n.uuid];
        assert_eq!(n.status, want["status"].as_str().unwrap(), "{} 状态", n.name);
        assert_eq!(n.reason.as_deref(), want["reason"].as_str(), "{} 原因", n.name);
        assert_eq!(n.availability, want["availability"].as_i64(), "{} 可用性", n.name);
        let wc = want["checks"].as_array().unwrap();
        assert_eq!(n.checks.len(), wc.len(), "{} 检查数", n.name);
        for (c, w) in n.checks.iter().zip(wc) {
            assert_eq!(c.key, w["key"].as_str().unwrap());
            assert_eq!(c.status, w["status"].as_str().unwrap(), "{} / {}", n.name, c.key);
            assert_eq!(c.reason.as_deref(), w["reason"].as_str(), "{} / {} 原因", n.name, c.key);
            assert_eq!(c.critical, w["critical"].as_bool().unwrap());
            assert_eq!(c.pending, w["pending"].as_bool().unwrap());
            compared += 1;
        }
    }
    eprintln!("生产快照：41 个节点、{compared} 项检查逐项一致");
}
