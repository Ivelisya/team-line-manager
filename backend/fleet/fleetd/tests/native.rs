//! 原生数据接入：relayd 批次（探测 + Xray 遥测）→ 逐线路机覆盖判断 → 切换到原生判定 → relay_xray_down。
//! 底图是 a395 抓的真实 /overview + /health（FLEETD_FIXTURES），没有就跳过。
use fleetd::config::EdgeCfg;
use fleetd::native::{read_snapshot, relay_xray_observations, NativeHealth};
use fleetd_ingest::{Ingest, IngestCfg};
use serde_json::{json, Value};
use std::path::PathBuf;

fn fixtures() -> Option<PathBuf> {
    let p = std::env::var("FLEETD_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/v2/live/fixtures"));
    p.join("api/overview.json").exists().then_some(p)
}

fn load(p: &PathBuf, name: &str) -> Value {
    let v: Value = serde_json::from_slice(&std::fs::read(p.join("api").join(name)).unwrap()).unwrap();
    v["body"]["data"].clone()
}

fn sample(ts: f64) -> Value {
    json!({"ts": ts, "cpu": {"pct": 20.0, "steal": 0.0, "iowait": 0.0}, "load": [0.1, 0.1, 0.1], "mem": {"pct": 30.0},
           "swap": {"pct": 0.0}, "disk": {"pct": 20.0}, "net": {"rx_bps": 1.0, "tx_bps": 1.0}, "sock": {"tcp": 1, "udp": 0}, "procs": 50, "uptime": 100.0,
           "xray": {"ok": true, "d": [], "conns": {}, "latency_ms": 0.3}})
}

#[allow(clippy::too_many_arguments)]
fn relay_batch(seq: u64, t: f64, nodes: &[(String, String)], egress_ok: &dyn Fn(&str) -> bool, running: bool) -> Value {
    let mut res = vec![];
    for (uuid, ip) in nodes {
        res.push(json!({"node": uuid, "check": "tcp", "status": "ok", "value_ms": 8.0, "measured_at": t - 2.0}));
        let ok = egress_ok(uuid);
        res.push(json!({"node": uuid, "check": "egress", "status": if ok { "ok" } else { "timeout" }, "value_ms": if ok { json!(400.0) } else { Value::Null },
            "actual_ip": if ok { json!(ip) } else { Value::Null }, "reason_code": if ok { Value::Null } else { json!("timeout") },
            "measured_at": t - 1.0, "attempt": 3, "final": true, "expected_ip": ip}));
    }
    let targets: Vec<Value> = nodes.iter().map(|(u, ip)| json!({"node": u, "name": "", "tag": "t", "addr": ip, "port": 8388,
        "method": "aes-256-gcm", "expected_ip": ip})).collect();
    json!({"v": 1, "kind": "relay", "host_id": "line3", "boot_id": "b", "seq": seq, "agent": "relayd/0.1", "created_at": t,
        "meta": {}, "outbox": {"pending": 0, "dropped": 0}, "samples": [sample(t - 1.0)],
        "xray": {"api": "127.0.0.1:62789", "xray": {"running": running, "pid": 1, "version": "26.7.28"}, "xui": {"running": false},
                 "last_ok": t - 1.0, "entries": []},
        "probe": {"schema": "fleet.probe.v1", "relay": "line3", "generated_at": t, "first_id": seq as i64 * 100, "last_id": seq as i64 * 100 + 99,
                  "results": res, "targets": targets}})
}

#[test]
fn per_relay_native_switch_and_xray_down() {
    let Some(p) = fixtures() else { return };
    let ov = load(&p, "overview.json");
    let health = load(&p, "health.json");
    let t0 = health["monitor"]["checked_at"].as_f64().unwrap();
    let nodes: Vec<(String, String)> = ov["assets"].as_array().unwrap().iter()
        .filter(|a| a["relay"] == "线路机3" && a["team"] != "线路机")
        .map(|a| (a["uuid"].as_str().unwrap().to_string(), a["ip"].as_str().unwrap().to_string())).collect();
    assert!(nodes.len() >= 3);
    let ing = Ingest::open_memory(IngestCfg::default());
    let cfg = EdgeCfg::default();
    let mut nh = NativeHealth::default();
    let all_ok = |_: &str| true;

    // 1) 只测了一部分节点：覆盖不完整，仍用旧判定
    let half: Vec<(String, String)> = nodes[..nodes.len() / 2].to_vec();
    ing.accept("line3", "relay", &relay_batch(1, t0, &half, &all_ok, true), t0).unwrap();
    let snap = read_snapshot(&ing, nh.last_land(), !nh.loaded_once(), t0);
    assert!(nh.step(&cfg, &snap, &ov, &health, t0).is_none());
    let c = nh.coverage["线路机3"].clone();
    assert!(!c.complete && !c.missing.is_empty(), "{c:?}");
    assert_eq!(c.source, "legacy");

    // 2) 全部在测且新鲜：complete，但要持续 native_after_s 才切
    let mut t = t0 + 30.0;
    let mut seq = 2;
    ing.accept("line3", "relay", &relay_batch(seq, t, &nodes, &all_ok, true), t).unwrap();
    let snap = read_snapshot(&ing, nh.last_land(), !nh.loaded_once(), t);
    assert!(nh.step(&cfg, &snap, &ov, &health, t).is_none());
    assert!(nh.coverage["线路机3"].complete);
    while t < t0 + 30.0 + cfg.native_after_s as f64 {
        t += 60.0;
        seq += 1;
        ing.accept("line3", "relay", &relay_batch(seq, t, &nodes, &all_ok, true), t).unwrap();
        let snap = read_snapshot(&ing, nh.last_land(), false, t);
        let _ = nh.step(&cfg, &snap, &ov, &health, t);
    }
    t += 60.0;
    seq += 1;
    ing.accept("line3", "relay", &relay_batch(seq, t, &nodes, &all_ok, true), t).unwrap();
    let snap = read_snapshot(&ing, nh.last_land(), false, t);
    let h = nh.step(&cfg, &snap, &ov, &health, t).expect("线路机3 应已切到原生判定");
    let c = nh.coverage["线路机3"].clone();
    assert_eq!(c.source, "native");
    // 重启：覆盖窗口从 meta 恢复，间隔 ≤ 300 s 时不重新等 10 分钟
    let saved = nh.persist_state();
    let mut nh2 = NativeHealth::with_restore(&saved);
    let snap2 = read_snapshot(&ing, 0, true, t + 30.0);
    assert!(nh2.step(&cfg, &snap2, &ov, &health, t + 30.0).is_some(), "恢复后应立即是原生来源");
    assert_eq!(nh2.coverage["线路机3"].complete_since, c.complete_since);
    for (u, _) in &nodes {
        assert_eq!(h["monitor"]["nodes"][u]["source"], "native");
        assert_eq!(h["monitor"]["nodes"][u]["status"], "ok", "{}", h["monitor"]["nodes"][u]);
    }
    // 其他线路机的节点仍是旧判定
    let other = ov["assets"].as_array().unwrap().iter().find(|a| a["relay"] == "线路机2").unwrap()["uuid"].as_str().unwrap();
    assert!(h["monitor"]["nodes"][other].get("source").is_none());
    assert!(c.agree > 0);

    // 3) 一个节点的认证出口连续两轮最终失败 → 原生判为 fail(timeout)
    let bad = nodes[0].0.clone();
    let fail_one = |u: &str| u != bad;
    let mut last = Value::Null;
    for _ in 0..2 {
        t += 60.0;
        seq += 1;
        ing.accept("line3", "relay", &relay_batch(seq, t, &nodes, &fail_one, true), t).unwrap();
        let snap = read_snapshot(&ing, nh.last_land(), false, t);
        last = nh.step(&cfg, &snap, &ov, &health, t).unwrap();
    }
    assert_eq!(last["monitor"]["nodes"][&bad]["status"], "fail", "{}", last["monitor"]["nodes"][&bad]);

    // 4) relayd 报告 Xray 进程不在 → relay_xray_down(xray_process_down)
    t += 30.0;
    seq += 1;
    ing.accept("line3", "relay", &relay_batch(seq, t, &nodes, &all_ok, false), t).unwrap();
    let snap = read_snapshot(&ing, nh.last_land(), false, t);
    let obs = relay_xray_observations(&cfg, &snap, &health, t);
    assert_eq!(obs.len(), 1);
    assert_eq!(obs[0].labels["cause"], "xray_process_down");
    assert_eq!(obs[0].labels["relay"], "线路机3");

    // 5) relayd 沉默 + 名下节点失败 → relayd_silent_nodes_failing；覆盖断开 → 立即退回旧判定
    let t2 = t + 400.0;
    let mut h2 = health.clone();
    for (u, _) in &nodes {
        h2["monitor"]["nodes"][u]["status"] = json!("fail");
    }
    let snap = read_snapshot(&ing, nh.last_land(), false, t2);
    let obs = relay_xray_observations(&cfg, &snap, &h2, t2);
    assert_eq!(obs.len(), 1);
    assert_eq!(obs[0].labels["cause"], "relayd_silent_nodes_failing");
    assert!(nh.step(&cfg, &snap, &ov, &health, t2).is_none());
    assert_eq!(nh.coverage["线路机3"].source, "legacy");
}
