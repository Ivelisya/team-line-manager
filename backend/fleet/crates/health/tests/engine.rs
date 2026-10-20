//! 新架构输入（fleet-probe 结果 + 遥测）下的判定，包括与旧系统有意不同的几条规则。
use fleet_health::engine::{EntryObs, HostObs, RelayObs};
use fleet_health::{Engine, EngineConfig, Inputs, NodeProbes, NodeSpec};
use fleet_probe::{reason, CheckKind, ProbeResult, Status};

const T0: f64 = 1_790_000_000.0;

fn spec(i: usize, relay: &str) -> NodeSpec {
    let ip = format!("75.37.239.{}", 10 + i);
    NodeSpec {
        uid: format!("n{i}"),
        name: format!("团队1-{i:02} · ATT · {ip}"),
        team: "团队1".into(),
        relay: Some(relay.into()),
        provider: "ATTVPS".into(),
        ip: ip.clone(),
        relay_port: Some(20000 + i as u16),
        outbound_tag: Some(format!("ss-{}", ip.replace('.', "-"))),
        target: Some(ip),
        target_port: Some(8388),
        protocol: Some("shadowsocks".into()),
        host_required: true,
        policy: serde_json::json!({"mode": "observe", "domains": []}),
        profile: serde_json::Value::Null,
        l3: false,
    }
}

fn result(node: &str, check: CheckKind, status: Status, at: f64) -> ProbeResult {
    let mut r = ProbeResult::new(node, check, status, at);
    if status == Status::Ok {
        r.value_ms = Some(40.0);
    }
    r
}

fn egress(s: &NodeSpec, ok: bool, at: f64, attempt: u8, last: bool, reason_code: Option<&str>) -> ProbeResult {
    let mut r = result(&s.uid, CheckKind::Egress, if ok { Status::Ok } else { Status::Fail }, at);
    r.attempt = Some(attempt);
    r.final_attempt = Some(last);
    r.expected_ip = Some(s.ip.clone());
    if ok {
        r.actual_ip = Some(s.ip.clone());
    } else {
        r.reason_code = reason_code.map(Into::into);
        if reason_code == Some(reason::TIMEOUT) {
            r.status = Status::Timeout;
        }
    }
    r.legacy_rc = Some(fleet_probe::legacy_rc(r.status, r.reason_code.as_deref()));
    r
}

fn inputs(n: usize, now: f64) -> Inputs {
    let mut inp = Inputs::default();
    inp.relays.insert("线路机3".into(), RelayObs { fresh: true, received_at: now - 2.0, ts: now - 2.0 });
    for i in 0..n {
        let s = spec(i, "线路机3");
        inp.hosts.insert(s.uid.clone(), HostObs { received_at: now - 3.0, online: true, cpu: Some(3.0), ram: Some(1.0), ram_total: Some(4.0), disk: Some(1.0), disk_total: Some(10.0) });
        inp.entries.insert(
            s.uid.clone(),
            EntryObs { enable: true, outbound_tag: s.outbound_tag.clone(), target: s.target.clone(), target_port: s.target_port, protocol: s.protocol.clone() },
        );
        let mut p = NodeProbes::default();
        p.apply(&result(&s.uid, CheckKind::Tcp, Status::Ok, now - 5.0));
        p.apply(&egress(&s, true, now - 20.0, 1, true, None));
        inp.probes.insert(s.uid.clone(), p);
        inp.nodes.push(s);
    }
    let mut c = result("_relay", CheckKind::Control, Status::Ok, now - 10.0);
    c.source = Some("cloudflare".into());
    inp.control.insert("线路机3".into(), vec![c]);
    inp
}

#[test]
fn all_ok_without_l3_is_ok() {
    let mut e = Engine::new(EngineConfig::default());
    let out = e.evaluate(&inputs(5, T0), T0);
    for n in &out.evaluation.nodes {
        assert_eq!((n.status.as_str(), n.reason.as_deref()), ("ok", None), "{}", n.name);
        assert_eq!(n.check("ingress").unwrap().status, "na");
    }
    assert!(out.correlations.is_empty());
}

#[test]
fn hl04_non_final_attempt_is_pending_then_escalates_per_cycle() {
    let mut e = Engine::new(EngineConfig::default());
    let mut now = T0;
    let mut inp = inputs(1, now);
    e.evaluate(&inp, now);
    let s = inp.nodes[0].clone();
    // 周期 1：第 1、2 次失败（pending），第 3 次失败（最终）
    for (att, last) in [(1, false), (2, false), (3, true)] {
        now += 30.0;
        inp = refresh(inp, now);
        inp.probes.get_mut(&s.uid).unwrap().apply(&egress(&s, false, now - 1.0, att, last, Some(reason::TIMEOUT)));
        let out = e.evaluate(&inp, now);
        let n = &out.evaluation.nodes[0];
        let c = n.check("egress").unwrap();
        if !last {
            assert_eq!((c.status.as_str(), c.pending), ("ok", true), "尝试 {att} 应是 pending");
        } else {
            // fail_after = 2：第一个最终失败只到 degraded
            assert_eq!((n.status.as_str(), n.reason.as_deref()), ("degraded", Some("timeout")));
        }
    }
    now += 30.0;
    inp = refresh(inp, now);
    inp.probes.get_mut(&s.uid).unwrap().apply(&egress(&s, false, now - 1.0, 3, true, Some(reason::TIMEOUT)));
    let out = e.evaluate(&inp, now);
    assert_eq!((out.evaluation.nodes[0].status.as_str(), out.evaluation.nodes[0].reason.as_deref()), ("fail", Some("timeout")));
    assert_eq!(out.evaluation.nodes[0].availability, Some(0));
    now += 30.0;
    inp = refresh(inp, now);
    inp.probes.get_mut(&s.uid).unwrap().apply(&egress(&s, true, now - 1.0, 1, true, None));
    let out = e.evaluate(&inp, now);
    assert_eq!(out.evaluation.nodes[0].status, "ok", "一次成功即恢复");
}

fn refresh(mut inp: Inputs, now: f64) -> Inputs {
    for r in inp.relays.values_mut() {
        r.received_at = now - 2.0;
        r.ts = now - 2.0;
    }
    for h in inp.hosts.values_mut() {
        h.received_at = now - 3.0;
    }
    for (uid, p) in inp.probes.iter_mut() {
        p.apply(&result(uid, CheckKind::Tcp, Status::Ok, now - 5.0));
    }
    for cs in inp.control.values_mut() {
        for c in cs.iter_mut() {
            c.measured_at = now - 10.0;
        }
    }
    inp
}

#[test]
fn probe_target_down_is_unknown_and_not_counted() {
    let mut e = Engine::new(EngineConfig::default());
    let mut inp = inputs(2, T0);
    let s = inp.nodes[0].clone();
    let mut r = egress(&s, false, T0 - 1.0, 1, true, Some(reason::PROBE_TARGET_DOWN));
    r.status = Status::Unknown;
    inp.probes.get_mut(&s.uid).unwrap().apply(&r);
    let out = e.evaluate(&inp, T0);
    let n = out.evaluation.nodes.iter().find(|n| n.uuid == s.uid).unwrap();
    assert_eq!((n.status.as_str(), n.reason.as_deref(), n.availability), ("unknown", Some("probe_target_down"), None));
    let w = e.windows(&s.uid, T0);
    assert_eq!(w["1"].failed_samples, 0);
    assert_eq!(w["1"].unknown_samples, 1);
}

#[test]
fn correlated_node_failure_needs_healthy_control_path() {
    for control_ok in [true, false] {
        let mut e = Engine::new(EngineConfig::default());
        let mut now = T0;
        let mut inp = inputs(5, now);
        if !control_ok {
            for cs in inp.control.values_mut() {
                cs[0].status = Status::Fail;
            }
        }
        let specs = inp.nodes.clone();
        let mut out = None;
        for _ in 0..2 {
            now += 60.0;
            inp = refresh(inp, now);
            for s in &specs[..4] {
                inp.probes.get_mut(&s.uid).unwrap().apply(&egress(s, false, now - 1.0, 3, true, Some(reason::TIMEOUT)));
            }
            out = Some(e.evaluate(&inp, now));
        }
        let out = out.unwrap();
        let failing: Vec<_> = out.evaluation.nodes.iter().filter(|n| n.status == "fail").collect();
        assert_eq!(failing.len(), 4);
        if control_ok {
            assert!(failing.iter().all(|n| n.reason.as_deref() == Some("correlated_node_failure")));
            assert_eq!(out.correlations.len(), 2, "线路机与供应商各聚合一条");
            assert_eq!(out.correlations[0].failing.len(), 4);
            assert_eq!(out.correlations[0].total, 5);
        } else {
            assert!(failing.iter().all(|n| n.reason.as_deref() == Some("timeout")));
            assert!(out.correlations.is_empty());
        }
    }
}

#[test]
fn relay_stale_makes_nodes_unknown_and_udp_is_informational() {
    let mut e = Engine::new(EngineConfig::default());
    let mut inp = inputs(2, T0);
    let s = inp.nodes[0].clone();
    let mut u = result(&s.uid, CheckKind::Udp, Status::Fail, T0 - 5.0);
    u.reason_code = Some(reason::UDP_EGRESS_MISMATCH.into());
    inp.probes.get_mut(&s.uid).unwrap().apply(&u);
    let out = e.evaluate(&inp, T0);
    let n = out.evaluation.nodes.iter().find(|n| n.uuid == s.uid).unwrap();
    assert_eq!(n.check("udp").unwrap().status, "degraded");
    assert_eq!(n.status, "ok", "UDP 不参与节点状态");
    inp.relays.get_mut("线路机3").unwrap().fresh = false;
    let out = e.evaluate(&inp, T0 + 30.0);
    assert!(out.evaluation.nodes.iter().all(|n| n.status == "unknown" && n.reason.as_deref() == Some("stale")));
}
