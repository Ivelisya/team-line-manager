//! 回放：用 a395 抓的真实 /overview + /health（2026-09-23 15:22，全部正常）做底，
//! 按 status_replays 的 5 个真实时刻和若干注入故障改副本，假时钟推进，检查告警引擎的输出。
//! 夹具含真实 IP，只在本机 / a395 上跑：FLEETD_FIXTURES=<deploy/v2/live/fixtures>；没有就跳过。
use fleetd::alerts::{Engine, EngineCfg, Matcher, Outgoing, Severity, Silence, State};
use fleetd::catalog::{self, Catalog};
use fleetd::config::AlertCfg;
use fleetd::rules::{self, short_id, PlanIndex};
use fleetd::store::Store;
use serde_json::{json, Value};
use std::path::PathBuf;

fn fixtures() -> Option<PathBuf> {
    let p = std::env::var("FLEETD_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/v2/live/fixtures"));
    if p.join("api/overview.json").exists() {
        Some(p)
    } else {
        eprintln!("skip: fixtures not found at {}", p.display());
        None
    }
}

fn load(p: &PathBuf, name: &str) -> Value {
    let v: Value = serde_json::from_slice(&std::fs::read(p.join("api").join(name)).unwrap()).unwrap();
    v["body"]["data"].clone()
}

fn plans() -> PlanIndex {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../architecture/plan-catalog/catalog.json");
    let s = Store::open_memory();
    catalog::import(&s, &path, "test", 1.0).unwrap();
    catalog::plan_index(&Catalog::load(&s))
}

struct World {
    ov: Value,
    health: Value,
    plans: PlanIndex,
    cfg: AlertCfg,
    engine: Engine,
    sent: Vec<Outgoing>,
    as_of: f64,
}

impl World {
    fn new(p: &PathBuf) -> World {
        let ov = load(p, "overview.json");
        let health = load(p, "health.json");
        let as_of = health["monitor"]["checked_at"].as_f64().unwrap();
        World { ov, health, plans: plans(), cfg: AlertCfg::default(), engine: Engine::new(EngineCfg::default(), as_of), sent: vec![], as_of }
    }

    fn uuid(&self, short: &str) -> String {
        self.ov["assets"].as_array().unwrap().iter().find(|a| short_id(a["name"].as_str().unwrap()) == short).unwrap()["uuid"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn asset_mut(&mut self, short: &str) -> &mut Value {
        self.ov["assets"].as_array_mut().unwrap().iter_mut().find(|a| short_id(a["name"].as_str().unwrap()) == short).unwrap()
    }

    fn node_mut(&mut self, short: &str) -> &mut Value {
        let u = self.uuid(short);
        &mut self.health["monitor"]["nodes"][&u]
    }

    /// 让某节点的某项检查进入 status（旧 monitor 的判定结果）
    fn set_check(&mut self, short: &str, key: &str, status: &str, reason: Option<&str>, since: f64, evidence: Value) {
        let n = self.node_mut(short);
        n["status"] = json!(status);
        n["reason"] = json!(reason);
        n["since"] = json!(since);
        for c in n["checks"].as_array_mut().unwrap() {
            if c["key"] == key {
                c["status"] = json!(status);
                c["reason"] = json!(reason);
                c["since"] = json!(since);
                if let (Value::Object(ev), Value::Object(add)) = (&mut c["evidence"], &evidence) {
                    for (k, v) in add {
                        ev.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }

    fn heal(&mut self, short: &str, since: f64, p: &PathBuf) {
        let orig = load(p, "health.json");
        let u = self.uuid(short);
        self.health["monitor"]["nodes"][&u] = orig["monitor"]["nodes"][&u].clone();
        self.health["monitor"]["nodes"][&u]["since"] = json!(since);
    }

    /// 让时间戳类字段跟上假时钟（旧系统本身每 5–10 s 刷新这些）
    fn freshen(&mut self, now: f64) {
        // 每次评估用的是刚渲染的 overview / health（渲染时刻 = now；规则按渲染时刻算新鲜度）
        self.ov["now"] = json!(now);
        self.health["time"] = json!(now);
        self.health["collector_at"] = json!(now - 3.0);
        self.health["monitor_at"] = json!(now - 5.0);
        self.ov["hosts_at"] = json!(now - 2.0);
        let t = chrono::DateTime::from_timestamp(now as i64 - 3, 0).unwrap().to_rfc3339();
        for a in self.ov["assets"].as_array_mut().unwrap() {
            if a["host"].is_object() && a["host"]["online"] == json!(true) {
                a["host"]["time"] = json!(t);
            }
        }
    }

    fn step(&mut self, now: f64) -> Vec<Outgoing> {
        let d = rules::derive(&self.ov, &self.health, &self.plans, &self.cfg, now);
        self.engine.evaluate_held(now, d.obs, &d.scope, &d.hold);
        let out: Vec<Outgoing> = self.engine.tick(now).into_iter().filter(|o| o.kind != "digest").collect();
        self.engine.take_dirty();
        self.sent.extend(out.clone());
        out
    }

    fn at(&mut self, now: f64) -> Vec<Outgoing> {
        self.freshen(now);
        self.step(now)
    }

    /// 每 10 s 一次评估（有人查看时的节奏），返回这段时间的全部通知
    fn run(&mut self, from: f64, to: f64) -> Vec<Outgoing> {
        let mut out = vec![];
        let mut t = from;
        while t <= to {
            self.freshen(t);
            out.extend(self.step(t));
            t += 10.0;
        }
        out
    }

    fn firing(&self, rule: &str) -> Vec<fleetd::alerts::Alert> {
        self.engine.active().filter(|a| a.rule == rule && a.state == State::Firing).cloned().collect()
    }
}

#[test]
fn baseline_all_normal_no_notifications() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of;
    let out = w.run(t, t + 600.0);
    let crit: Vec<_> = out.iter().filter(|o| o.severity >= Severity::Warning).collect();
    assert!(crit.is_empty(), "09-23 的副本全部正常，不应有 warning/critical 通知：{:?}", crit.iter().map(|o| &o.title).collect::<Vec<_>>());
    // 只可能有 info 级的续费提醒（进摘要）
    assert!(w.engine.active().all(|a| a.rule == "renewal_due"), "{:?}", w.engine.active().map(|a| &a.rule).collect::<Vec<_>>());
}

#[test]
fn replay_0911_t1_31_ingress_auth_fail_then_resolve() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of + 60.0;
    w.run(w.as_of, t - 10.0);
    // 真实回放：团队1-31 实际入口到出口 auth（rc=35 phase=tls），持续 376 s，658 s 后恢复
    w.set_check("t1-31", "ingress", "fail", Some("auth"), t, json!({"rc": 35, "phase": "tls", "actual_ip": null}));
    let out = w.run(t, t + 30.0);
    assert_eq!(out.len(), 1, "critical：group_wait 10 s 后一条");
    let o = &out[0];
    assert_eq!(o.severity, Severity::Critical);
    assert_eq!(o.group_key.as_deref(), Some("relay=线路机3|cause=auth"), "团队1-31 挂在线路机3");
    assert!(o.body.contains("团队1-31") && o.body.contains("rc=35") && o.body.contains("#/nodes/t1-31"), "{}", o.body);
    assert!(!o.body.contains("客户") && !o.body.contains("用户"), "通知只写技术对象");
    assert!(o.ts - t <= 20.0 && o.ts - t >= 10.0);
    // 持续期间不重复（30 min 内）
    assert!(w.run(t + 40.0, t + 650.0).is_empty());
    w.heal("t1-31", t + 658.0, &p);
    let out = w.run(t + 660.0, t + 1000.0);
    assert_eq!(out.len(), 1, "恢复通知（按 group_interval 对齐）");
    assert!(out[0].title.starts_with("已恢复"), "{}", out[0].title);
    assert!(w.engine.active().all(|a| a.rule == "renewal_due"));
}

#[test]
fn replay_1845_t1_15_auth_fail_and_repeat_until_ack() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of + 30.0;
    w.set_check("t1-15", "ingress", "fail", Some("auth"), t, json!({"rc": 35, "phase": "tls"}));
    let out = w.run(t, t + 1900.0);
    let reps: Vec<_> = out.iter().map(|o| o.kind.as_str()).collect();
    assert_eq!(reps, vec!["group", "repeat"], "未确认的 critical 30 min 后重复");
    let id = w.firing("node_health_fail")[0].id;
    w.engine.ack(id, "admin", t + 1900.0).unwrap();
    assert!(w.run(t + 1910.0, t + 5000.0).is_empty(), "确认后不再重复");
}

#[test]
fn replay_1637_degraded_126s_and_1722_degraded_60s_are_quiet() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of + 30.0;
    // 16:37 团队2-01 / 2-02 实际入口到出口降级 126 s；17:22 团队1-21 认证出口降级 60 s
    w.set_check("t2-01", "ingress", "degraded", None, t, json!({}));
    w.set_check("t2-02", "ingress", "degraded", None, t, json!({}));
    w.set_check("t1-21", "egress", "degraded", None, t, json!({}));
    assert!(w.run(t, t + 50.0).is_empty());
    w.heal("t1-21", t + 60.0, &p);
    assert!(w.run(t + 60.0, t + 120.0).is_empty());
    w.heal("t2-01", t + 126.0, &p);
    w.heal("t2-02", t + 126.0, &p);
    assert!(w.run(t + 130.0, t + 900.0).is_empty(), "降级不足 5 min：不通知");
    assert!(w.engine.active().all(|a| a.rule == "renewal_due"));
    // 持续 ≥ 5 min 的降级：warning，30 s group_wait
    w.set_check("t2-01", "ingress", "degraded", Some("unstable"), t + 1000.0, json!({}));
    let out = w.run(t + 1000.0, t + 1400.0);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].severity, Severity::Warning);
    assert!(out[0].ts >= t + 1300.0 && out[0].ts <= t + 1340.0, "5 min + 30 s：{}", out[0].ts - t);
}

#[test]
fn replay_1121_host_probe_gap_70s_is_quiet_but_120s_fires() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of + 30.0;
    w.run(w.as_of, t);
    // 团队1-01 / 1-02 主机探针 70 s 没有上报
    let stale = chrono::DateTime::from_timestamp(t as i64, 0).unwrap().to_rfc3339();
    for s in ["t1-01", "t1-02"] {
        w.asset_mut(s)["host"]["time"] = json!(stale.clone());
        w.asset_mut(s)["host"]["online"] = json!(false);
    }
    assert!(w.run(t + 10.0, t + 70.0).is_empty());
    for s in ["t1-01", "t1-02"] {
        w.asset_mut(s)["host"]["online"] = json!(true);
    }
    assert!(w.run(t + 80.0, t + 400.0).is_empty(), "70 s 的空档不通知");
    // 持续离线：120 s 后 warning，同组（线路机2 / host_offline）一条
    let t2 = t + 500.0;
    let old = chrono::DateTime::from_timestamp(t2 as i64, 0).unwrap().to_rfc3339();
    for s in ["t1-01", "t1-02"] {
        w.asset_mut(s)["host"]["online"] = json!(false);
        w.asset_mut(s)["host"]["time"] = json!(old.clone());
    }
    let out = w.run(t2 + 10.0, t2 + 200.0);
    assert_eq!(out.len(), 1, "{:?}", out.iter().map(|o| &o.title).collect::<Vec<_>>());
    assert_eq!(out[0].group_key.as_deref(), Some("relay=线路机2|cause=host_offline"));
    assert_eq!(out[0].alert_ids.len(), 2);
}

#[test]
fn relay_down_inhibits_nodes_of_that_relay() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of + 30.0;
    w.run(w.as_of, t);
    // 线路机1：2 分钟前起管理通道中断 + 主机探针离线
    w.ov["relays"]["线路机1"]["fresh"] = json!(false);
    w.ov["relays"]["线路机1"]["received_at"] = json!(t - 120.0);
    w.asset_mut("r1")["host"]["online"] = json!(false);
    w.asset_mut("r1")["host"]["time"] = json!(chrono::DateTime::from_timestamp(t as i64 - 120, 0).unwrap().to_rfc3339());
    // 它名下两个节点被旧 monitor 判为 fail（stale）
    let nodes: Vec<String> = w.ov["assets"].as_array().unwrap().iter()
        .filter(|a| a["relay"] == "线路机1").map(|a| short_id(a["name"].as_str().unwrap())).take(2).collect();
    assert_eq!(nodes.len(), 2);
    for n in &nodes {
        w.set_check(n, "collector", "fail", Some("stale"), t, json!({"fresh": false}));
    }
    // 另一台线路机下的节点故障不受影响
    w.set_check("t1-31", "ingress", "fail", Some("auth"), t, json!({"rc": 35}));
    let out = w.run(t, t + 200.0);
    let keys: Vec<_> = out.iter().map(|o| o.group_key.clone().unwrap()).collect();
    assert!(keys.contains(&"relay=线路机1|cause=relay_down".to_string()), "{keys:?}");
    assert!(keys.contains(&"relay=线路机3|cause=auth".to_string()), "{keys:?}");
    assert!(!keys.iter().any(|k| k == "relay=线路机1|cause=stale"), "节点告警被 relay_down 抑制：{keys:?}");
    let rd = &w.firing("relay_down")[0];
    assert!(out.iter().find(|o| o.group_key.as_deref() == Some("relay=线路机1|cause=relay_down")).unwrap().ts - rd.fired_at.unwrap() <= 10.0 + 10.0);
    for a in w.firing("node_health_fail").iter().filter(|a| a.labels["relay"] == "线路机1") {
        assert_eq!(a.suppressed.as_deref(), Some(format!("inhibited:{}", rd.id).as_str()));
    }
}

#[test]
fn correlated_failure_one_notification() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of + 30.0;
    w.run(w.as_of, t);
    let nodes: Vec<String> = w.ov["assets"].as_array().unwrap().iter()
        .filter(|a| a["relay"] == "线路机1").map(|a| short_id(a["name"].as_str().unwrap())).take(3).collect();
    for (i, n) in nodes.iter().enumerate() {
        let (k, r) = [("ingress", "auth"), ("egress", "timeout"), ("ingress", "upstream_down")][i];
        w.set_check(n, k, "fail", Some(r), t + i as f64 * 20.0, json!({}));
    }
    let out = w.run(t + 40.0, t + 200.0);
    assert_eq!(out.len(), 1, "{:?}", out.iter().map(|o| &o.title).collect::<Vec<_>>());
    assert_eq!(out[0].group_key.as_deref(), Some("relay=线路机1|cause=correlated_node_failure"));
    for n in &nodes {
        assert!(out[0].body.contains(n.as_str()), "{}", out[0].body);
    }
}

/// 2026-09-24 起出口 IP 不再与记录比对（节点健康只看连通性，rules::EGRESS_IP_COMPARE = false）：不产生 egress_mismatch
#[test]
fn egress_ip_is_not_compared() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of + 30.0;
    let expected = w.node_mut("t1-05")["checks"].as_array().unwrap().iter().find(|c| c["key"] == "egress")
        .and_then(|c| c.pointer("/evidence/expected_ip").cloned()).unwrap_or(json!("198.51.100.1"));
    w.set_check("t1-05", "egress", "degraded", None, t, json!({"actual_ip": "203.0.113.9", "expected_ip": expected}));
    w.run(t, t + 60.0);
    assert!(w.firing("egress_mismatch").is_empty());
}

#[test]
fn maintenance_window_suppresses_then_releases() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of + 30.0;
    w.engine.set_silences(vec![Silence { id: 3, kind: "maintenance".into(),
        matchers: vec![Matcher { name: "node".into(), value: "t1-31".into(), negate: false }],
        starts_at: t - 60.0, ends_at: t + 600.0, tz: "Asia/Shanghai".into(), comment: "换机".into(), created_by: "admin".into() }]);
    w.set_check("t1-31", "ingress", "fail", Some("auth"), t, json!({}));
    assert!(w.run(t, t + 590.0).is_empty());
    let out = w.run(t + 600.0, t + 700.0);
    assert_eq!(out.len(), 1, "窗口结束仍在故障：发出");
}

#[test]
fn collector_stale_is_critical_and_inhibits_nodes() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of + 30.0;
    w.set_check("t1-31", "ingress", "fail", Some("auth"), t, json!({}));
    let mut out = vec![];
    let mut now = t;
    while now < t + 300.0 {
        w.freshen(now);
        w.health["collector_at"] = json!(t - 30.0);
        out.extend(w.step(now));
        now += 10.0;
    }
    let keys: Vec<_> = out.iter().map(|o| o.group_key.clone().unwrap()).collect();
    // 节点告警先发（采集器 120 s 后才判停滞），之后采集器停滞发一条
    assert!(keys.contains(&"relay=-|cause=collector_stale".to_string()), "{keys:?}");
    assert!(w.firing("node_health_fail").iter().all(|a| a.suppressed.as_deref().unwrap_or("").starts_with("inhibited:")));
}

#[test]
fn renewal_7_3_1_days_in_billing_tz() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    // 线路机1：到期 2026-10-01，账期时区 America/Los_Angeles；as_of = 09-23 LA 00:22 → 8 天
    let r1 = w.ov["assets"].as_array().unwrap().iter().find(|a| a["name"] == "线路机1").unwrap().clone();
    assert_eq!(rules::due_calendar_days(&r1, w.as_of), Some(8));
    let day = 86400.0;
    w.at(w.as_of);
    assert!(!w.engine.active().any(|a| a.rule == "renewal_due" && a.labels["relay"] == "线路机1"));
    w.at(w.as_of + day);
    let a = w.engine.active().find(|a| a.rule == "renewal_due" && a.labels.get("asset").map(String::as_str) == Some("r1")).cloned().unwrap();
    assert_eq!((a.severity, a.labels["threshold"].as_str()), (Severity::Info, "7"));
    let out = w.at(w.as_of + 5.0 * day);
    let a = w.engine.active().find(|a| a.rule == "renewal_due" && a.labels.get("asset").map(String::as_str) == Some("r1")).cloned().unwrap();
    assert_eq!((a.severity, a.labels["threshold"].as_str()), (Severity::Warning, "3"));
    let _ = out;
    let out = w.run(w.as_of + 5.0 * day + 10.0, w.as_of + 5.0 * day + 60.0);
    assert!(out.iter().any(|o| o.body.contains("线路机1 还有 3 天到期")), "{:?}", out.iter().map(|o| &o.body).collect::<Vec<_>>());
    w.at(w.as_of + 7.0 * day);
    let a = w.engine.active().find(|a| a.rule == "renewal_due" && a.labels.get("asset").map(String::as_str) == Some("r1")).cloned().unwrap();
    assert_eq!((a.severity, a.labels["threshold"].as_str()), (Severity::Critical, "1"));
    // 续费后（到期日后移）自动结束，不发恢复通知
    let n = w.sent.len();
    w.asset_mut("r1")["due_date"] = json!("2026-11-01");
    w.run(w.as_of + 7.0 * day + 10.0, w.as_of + 7.0 * day + 400.0);
    assert!(!w.engine.active().any(|a| a.rule == "renewal_due" && a.labels.get("asset").map(String::as_str) == Some("r1")));
    assert!(!w.sent[n..].iter().any(|o| o.title.starts_with("已恢复") && o.body.contains("线路机1 还有")));
}

#[test]
fn relay_quota_80_then_every_5() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of + 30.0;
    // 线路机1 套餐 2000 GB 出站（十进制）；夹具里 net_total_up ≈ 1195.9 GB（59.8%）
    w.run(w.as_of, t);
    assert!(w.firing("quota").is_empty());
    w.asset_mut("r1")["host"]["net_total_up"] = json!(1_620_000_000_000u64);
    let out = w.run(t, t + 60.0);
    assert_eq!(out.len(), 1);
    assert!(out[0].body.contains("81.0%") && out[0].body.contains("80% 档"), "{}", out[0].body);
    w.asset_mut("r1")["host"]["net_total_up"] = json!(1_730_000_000_000u64);
    let out = w.run(t + 70.0, t + 500.0);
    assert_eq!(out.len(), 1);
    assert!(out[0].body.contains("85% 档"));
    assert_eq!(w.firing("quota").len(), 1, "只保留当前档");
    w.asset_mut("r1")["host"]["net_total_up"] = json!(2_010_000_000_000u64);
    w.run(t + 510.0, t + 900.0);
    assert_eq!(w.firing("quota")[0].severity, Severity::Critical);
}

#[test]
fn node_unknown_under_dead_relay_is_held_not_resolved() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of + 30.0;
    w.set_check("t1-31", "ingress", "fail", Some("auth"), t, json!({"rc": 35}));
    let out = w.run(t, t + 30.0);
    assert_eq!(out.len(), 1);
    // 线路机3 失联：旧 monitor 把它名下节点判为 unknown（没有数据），relay_down 60 s 后触发
    w.ov["relays"]["线路机3"]["fresh"] = json!(false);
    w.ov["relays"]["线路机3"]["received_at"] = json!(t + 40.0);
    w.asset_mut("r3")["host"]["online"] = json!(false);
    w.asset_mut("r3")["host"]["time"] = json!(chrono::DateTime::from_timestamp(t as i64 + 40, 0).unwrap().to_rfc3339());
    let u = w.uuid("t1-31");
    w.health["monitor"]["nodes"][&u]["status"] = json!("unknown");
    w.health["monitor"]["nodes"][&u]["reason"] = json!("stale");
    let out = w.run(t + 40.0, t + 400.0);
    assert!(out.iter().all(|o| !o.title.starts_with("已恢复")), "没有数据不等于恢复：{:?}", out.iter().map(|o| &o.title).collect::<Vec<_>>());
    assert!(out.iter().any(|o| o.group_key.as_deref() == Some("relay=线路机3|cause=relay_down")));
    let a = w.firing("node_health_fail");
    assert_eq!(a.len(), 1, "t1-31 的告警保持");
    assert!(a[0].suppressed.as_deref().unwrap_or("").starts_with("inhibited:"), "{:?}", a[0].suppressed);
}

/// 重启（真实数据形状）：停机 100 s 后启动，库里的采集 / 评估 / 主机快照与主机上报时间都是停机前的，边缘上报约 40 s 后才到。
/// 宽限期里不发任何「还没数据」类通知；数据跟上之后宽限结束，也不会冒出“恢复 → 再触发”。
#[test]
fn restart_startup_grace_no_shadow_alerts() {
    let Some(p) = fixtures() else { return };
    let mut w = World::new(&p);
    let t = w.as_of;
    w.run(t, t + 120.0);
    let down = t + 130.0;
    let started = down + 100.0;
    let first_ingest = started + 40.0;
    let mut out = vec![];
    let mut now = started + 5.0;
    let mut held_steps = 0;
    while now < started + 400.0 {
        if now < first_ingest + 30.0 {
            // 旧时间戳：库里是停机前那一刻；overview / health 是刚渲染的
            w.ov["now"] = json!(now);
            w.health["time"] = json!(now);
            w.health["collector_at"] = json!(if now < started + 12.0 { down - 3.0 } else { now - 3.0 });
            w.health["monitor_at"] = json!(if now < started + 35.0 { down - 5.0 } else { now - 5.0 });
            w.ov["hosts_at"] = json!(if now < first_ingest + 25.0 { down - 2.0 } else { now - 2.0 });
            let ts = chrono::DateTime::from_timestamp(down as i64 - 3, 0).unwrap().to_rfc3339();
            for a in w.ov["assets"].as_array_mut().unwrap() {
                if a["host"].is_object() {
                    a["host"]["time"] = json!(ts);
                }
            }
            for r in w.ov["relays"].as_object_mut().unwrap().values_mut() {
                r["fresh"] = json!(false);
            }
        } else {
            w.freshen(now);
            for r in w.ov["relays"].as_object_mut().unwrap().values_mut() {
                r["fresh"] = json!(true);
                r["received_at"] = json!(now - 4.0);
            }
        }
        let mut d = rules::derive(&w.ov, &w.health, &w.plans, &w.cfg, now);
        let ingest = (now >= first_ingest).then_some(first_ingest);
        if rules::startup_hold(started, ingest, true, &w.ov, &w.health, now, w.cfg.startup_grace_s as f64, w.cfg.startup_grace_max_s as f64) {
            held_steps += 1;
            assert!(!d.obs.is_empty() || now >= first_ingest + 30.0, "空窗期里规则本来会报");
            d.hold_startup();
        }
        w.engine.evaluate_held(now, d.obs, &d.scope, &d.hold);
        out.extend(w.engine.tick(now).into_iter().filter(|o| o.kind != "digest"));
        w.engine.take_dirty();
        now += 10.0;
    }
    assert!(held_steps >= 12, "至少 120 s 宽限：{held_steps}");
    let bad: Vec<_> = out.iter().filter(|o| o.severity >= Severity::Warning).map(|o| o.title.clone()).collect();
    assert!(bad.is_empty(), "重启不应有影子告警：{bad:?}");
    assert!(w.engine.active().all(|a| a.rule == "renewal_due"), "{:?}", w.engine.active().map(|a| &a.rule).collect::<Vec<_>>());
}
