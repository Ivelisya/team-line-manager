//! 对照 2：真实 monitor.py 跑出的逐步 golden（脚本化场景 + 固定种子随机场景，见 tools/gen_golden.py）。
//! 每一步比较 status / reason / since / availability，以及每项检查的 status / reason / since / pending；
//! 最后比较 1/7/30 d 可用率窗口。
use fleet_health::legacy::Monitor;
use serde_json::Value;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

#[test]
fn replay_matches_real_legacy_monitor() {
    let p = format!("{}/tests/fixtures/legacy-golden.json", env!("CARGO_MANIFEST_DIR"));
    let g: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    let mut m = Monitor::new();
    let mut diffs: Vec<String> = Vec::new();
    let (mut nodes_cmp, mut checks_cmp) = (0, 0);
    let steps = g["steps"].as_array().unwrap();
    for s in steps {
        let now = s["now"].as_f64().unwrap();
        let label = s["label"].as_str().unwrap();
        let ev = m.evaluate(s["assets"].as_array().unwrap(), &s["relays"], now);
        for n in &ev.nodes {
            let w = &s["expected"][&n.uuid];
            nodes_cmp += 1;
            let mut d = |what: &str, got: String, want: String| {
                if got != want {
                    diffs.push(format!("[{label}] {} {what}: rust={got} legacy={want}", n.name));
                }
            };
            d("status", n.status.clone(), w["status"].as_str().unwrap().into());
            d("reason", format!("{:?}", n.reason.as_deref()), format!("{:?}", w["reason"].as_str()));
            d("availability", format!("{:?}", n.availability), format!("{:?}", w["availability"].as_i64()));
            if !close(n.since, w["since"].as_f64().unwrap()) {
                diffs.push(format!("[{label}] {} since: rust={} legacy={}", n.name, n.since, w["since"]));
            }
            let wc = w["checks"].as_array().unwrap();
            if wc.len() != n.checks.len() {
                diffs.push(format!("[{label}] {} 检查数 {} vs {}", n.name, n.checks.len(), wc.len()));
                continue;
            }
            for (c, x) in n.checks.iter().zip(wc) {
                checks_cmp += 1;
                let (k, st, rs, since, pend) = (x[0].as_str().unwrap(), x[1].as_str().unwrap(), x[2].as_str(), x[3].as_f64().unwrap(), x[4].as_bool().unwrap());
                if c.key != k || c.status != st || c.reason.as_deref() != rs || !close(c.since, since) || c.pending != pend {
                    diffs.push(format!(
                        "[{label}] {} {}: rust=({}, {:?}, {}, {}) legacy=({st}, {rs:?}, {since}, {pend})",
                        n.name, c.key, c.status, c.reason, c.since, c.pending
                    ));
                }
            }
        }
    }
    let final_now = g["final_now"].as_f64().unwrap();
    for (uuid, w) in g["windows"].as_object().unwrap() {
        let got = m.availability_windows(uuid, final_now, &[1, 7, 30]);
        for (d, want) in w.as_object().unwrap() {
            let gw = serde_json::to_value(&got[d]).unwrap();
            for k in ["availability_percent", "coverage_percent", "successful_samples", "failed_samples", "unknown_samples", "missing_samples", "maintenance_samples"] {
                let (a, b) = (&gw[k], &want[k]);
                let same = match (a.as_f64(), b.as_f64()) {
                    (Some(x), Some(y)) => close(x, y),
                    _ => a == b,
                };
                if !same {
                    diffs.push(format!("windows {uuid} {d}d {k}: rust={a} legacy={b}"));
                }
            }
        }
    }
    for x in diffs.iter().take(30) {
        eprintln!("{x}");
    }
    eprintln!("golden：{} 步，{nodes_cmp} 次节点判定、{checks_cmp} 次检查判定，差异 {} 处", steps.len(), diffs.len());
    assert!(diffs.is_empty(), "与旧 monitor.py 有 {} 处差异", diffs.len());
}
