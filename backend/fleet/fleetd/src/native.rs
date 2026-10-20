//! 原生数据（fleet-edge：relayd / nodeagent 经 /v2/ingest/* 上报）接进告警：
//!
//! 1. relayd 的逐项探测（probe_land）→ fleet-health 的新判定引擎；**逐台线路机**切换：某线路机名下旧系统的
//!    全部节点都在 relayd 的在测列表里、tcp 与 egress 结果都新鲜、relayd 本身在上报，并且这种“覆盖完整”
//!    持续 `edge.native_after_s` 之后，这台线路机名下节点的综合状态才改用原生判定；覆盖一断立即退回旧 /health。
//!    两边的判定一直并行比较，结果在 /v2/api/v1/health/shadow 的 `native` 里。
//! 2. relayd 遥测 → relay_xray_down：Xray 进程不在、或 Stats API 连续不可达、或 relayd 自己不再上报而名下节点在失败。
//! 3. fleetd-ingest 的原生主机告警（native_host_offline 等），启动后 `edge.startup_hold_s` 内不开 native_host_offline。
use crate::alerts::{Labels, Observation, Severity};
use crate::config::EdgeCfg;
use crate::rules::{head_name, parse_time, short_id};
use fleet_health::engine::{EntryObs, HostObs, RelayObs};
use fleet_health::{Engine, EngineConfig, Inputs, NodeProbes, NodeSpec};
use fleet_probe::ProbeResult;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

pub const NATIVE_RULES: &[&str] = &["relay_egress_mismatch", "relay_tcp_down", "relay_probe_stalled", "egress_route_leak", "native_host_offline", "native_host_resource", "native_cpu_steal", "native_cpu_iowait"];

#[derive(Serialize, Clone, Default, Debug)]
pub struct RelayCoverage {
    pub relay: String,
    pub host_id: Option<String>,
    pub relayd_age_s: Option<f64>,
    pub nodes: usize,
    pub targeted: usize,
    pub fresh: usize,
    pub missing: Vec<String>,
    pub stale: Vec<String>,
    /// relayd 的探测不支持的节点（SOCKS5 代理产品等）：不计入覆盖要求，状态始终来自旧 /health
    pub legacy_only: Vec<String>,
    pub complete: bool,
    pub complete_since: Option<f64>,
    /// 现在这台线路机名下节点状态的来源：legacy | native
    pub source: String,
    pub compared: u64,
    pub agree: u64,
    pub last_disagree: Vec<Value>,
}

pub struct NativeHealth {
    engine: Engine,
    probes: BTreeMap<String, NodeProbes>,
    control: BTreeMap<String, Vec<ProbeResult>>,
    last_land: i64,
    loaded_once: bool,
    pub coverage: BTreeMap<String, RelayCoverage>,
    pub rows_loaded: u64,
    pub last_eval_ms: f64,
    /// 上次进程留下的覆盖窗口：线路机 → (complete_since, last_complete_at)；重启后若覆盖仍完整且间隔 ≤ 300 s 就沿用
    pub restore: BTreeMap<String, (f64, f64)>,
    pub last_complete_at: BTreeMap<String, f64>,
}

impl Default for NativeHealth {
    fn default() -> Self {
        NativeHealth {
            engine: Engine::new(EngineConfig::default()),
            probes: BTreeMap::new(),
            control: BTreeMap::new(),
            last_land: 0,
            loaded_once: false,
            coverage: BTreeMap::new(),
            rows_loaded: 0,
            last_eval_ms: 0.0,
            restore: BTreeMap::new(),
            last_complete_at: BTreeMap::new(),
        }
    }
}

/// 从 ingest.db 读的一次快照
pub struct EdgeSnapshot {
    pub land: Vec<(i64, String, String, String)>,
    pub targets: BTreeMap<String, Vec<String>>,
    /// host_id → (kind, received_at, sample, xray)
    pub hosts: BTreeMap<String, (String, f64, Value, Value)>,
}

pub fn read_snapshot(ing: &fleetd_ingest::Ingest, after: i64, first: bool, now: f64) -> EdgeSnapshot {
    ing.with_db(|c| {
        let mut land = vec![];
        let (sql, arg) = if first {
            ("SELECT land_id, relay, node, doc FROM probe_land WHERE measured_at > ?1 ORDER BY land_id LIMIT 50000",
             rusqlite::types::Value::Real(now - 900.0))
        } else {
            ("SELECT land_id, relay, node, doc FROM probe_land WHERE land_id > ?1 ORDER BY land_id LIMIT 50000",
             rusqlite::types::Value::Integer(after))
        };
        if let Ok(mut st) = c.prepare(sql) {
            if let Ok(rows) = st.query_map([arg], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))) {
                land = rows.filter_map(Result::ok).collect();
            }
        }
        let mut targets = BTreeMap::new();
        if let Ok(mut st) = c.prepare("SELECT relay, targets FROM probe_targets") {
            if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                for (relay, t) in rows.filter_map(Result::ok) {
                    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    let nodes = v.as_array().map(|a| a.iter().filter_map(|x| x["node"].as_str().map(String::from)).collect()).unwrap_or_default();
                    targets.insert(relay, nodes);
                }
            }
        }
        let mut hosts = BTreeMap::new();
        if let Ok(mut st) = c.prepare("SELECT host_id, kind, received_at, sample, xray FROM host_latest") {
            if let Ok(rows) = st.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?))
            }) {
                for (h, k, recv, s, x) in rows.filter_map(Result::ok) {
                    let s: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
                    let x: Value = x.and_then(|x| serde_json::from_str(&x).ok()).unwrap_or(Value::Null);
                    hosts.insert(h, (k, recv, s, x));
                }
            }
        }
        EdgeSnapshot { land, targets, hosts }
    })
}

fn opt_u16(v: &Value) -> Option<u16> {
    v.as_u64().and_then(|x| u16::try_from(x).ok()).or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn opt_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl NativeHealth {
    pub fn loaded_once(&self) -> bool {
        self.loaded_once
    }
    pub fn last_land(&self) -> i64 {
        self.last_land
    }
    /// 要持久化的覆盖窗口（fleetd meta: native_coverage）
    pub fn persist_state(&self) -> Value {
        let m: serde_json::Map<String, Value> = self
            .coverage
            .values()
            .filter_map(|c| {
                let since = c.complete_since?;
                let last = *self.last_complete_at.get(&c.relay)?;
                Some((c.relay.clone(), json!([since, last])))
            })
            .collect();
        Value::Object(m)
    }

    pub fn with_restore(v: &Value) -> NativeHealth {
        let mut n = NativeHealth::default();
        if let Some(m) = v.as_object() {
            for (k, x) in m {
                if let (Some(a), Some(b)) = (x[0].as_f64(), x[1].as_f64()) {
                    n.restore.insert(k.clone(), (a, b));
                }
            }
        }
        n
    }

    pub fn any_native(&self) -> bool {
        self.coverage.values().any(|c| c.source == "native")
    }

    fn ingest_land(&mut self, snap: &EdgeSnapshot, relay_name: &dyn Fn(&str) -> String) {
        for (id, relay_host, node, doc) in &snap.land {
            self.last_land = self.last_land.max(*id);
            self.rows_loaded += 1;
            let Ok(r) = serde_json::from_str::<ProbeResult>(doc) else { continue };
            if node == "_relay" {
                let v = self.control.entry(relay_name(relay_host)).or_default();
                if let Some(i) = v.iter().position(|x| x.source == r.source) {
                    if v[i].measured_at <= r.measured_at {
                        v[i] = r;
                    }
                } else {
                    v.push(r);
                }
            } else {
                self.probes.entry(node.clone()).or_default().apply(&r);
            }
        }
        self.loaded_once = true;
    }

    /// 一轮：更新原生判定、覆盖情况；返回“改用原生状态后的 health”（没有任何线路机切换时 None）。
    pub fn step(&mut self, cfg: &EdgeCfg, snap: &EdgeSnapshot, ov: &Value, health: &Value, now: f64) -> Option<Value> {
        let t0 = std::time::Instant::now();
        let host_to_relay: BTreeMap<String, String> = cfg.relay_hosts.clone();
        let relay_to_host: BTreeMap<String, String> = host_to_relay.iter().map(|(h, r)| (r.clone(), h.clone())).collect();
        let rn = |h: &str| host_to_relay.get(h).cloned().unwrap_or_else(|| h.to_string());
        self.ingest_land(snap, &rn);

        let empty = vec![];
        let assets = ov.get("assets").and_then(|x| x.as_array()).unwrap_or(&empty);
        let legacy_nodes = health.pointer("/monitor/nodes").cloned().unwrap_or(Value::Null);
        // nodeagent 的 host_id 是资产短名称（「团队1-24」）
        let mut inp = Inputs::default();
        let mut by_relay: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let mut legacy_only: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for a in assets {
            let name = a["name"].as_str().unwrap_or("");
            let uuid = a["uuid"].as_str().unwrap_or("").to_string();
            if a["team"] == "线路机" || uuid.is_empty() {
                continue;
            }
            let relay = a["relay"].as_str().unwrap_or("").to_string();
            // fleet-probe 只会探 Shadowsocks 出站；其他协议（NodeMach SOCKS5 代理产品）留在旧判定
            let probeable = a["protocol"].as_str().map(|p| p.eq_ignore_ascii_case("shadowsocks") || p.eq_ignore_ascii_case("ss")).unwrap_or(false);
            if !probeable {
                legacy_only.entry(relay.clone()).or_default().push(short_id(name));
                continue;
            }
            by_relay.entry(relay.clone()).or_default().push((uuid.clone(), short_id(name)));
            inp.nodes.push(NodeSpec {
                uid: uuid.clone(),
                name: name.to_string(),
                team: a["team"].as_str().unwrap_or("").into(),
                relay: if relay.is_empty() { None } else { Some(relay.clone()) },
                provider: a["provider"].as_str().unwrap_or("").into(),
                ip: a["ip"].as_str().unwrap_or("").into(),
                relay_port: opt_u16(&a["relay_port"]),
                outbound_tag: opt_str(&a["outbound_tag"]),
                target: opt_str(&a["target"]),
                target_port: opt_u16(&a["target_port"]),
                protocol: opt_str(&a["protocol"]),
                host_required: a["host_measured"].as_bool().unwrap_or(false) || a["provider"] == "ATTVPS",
                policy: a.get("policy").cloned().filter(|p| !p.is_null()).unwrap_or(json!({"mode": "observe", "domains": []})),
                profile: a.get("profile").cloned().unwrap_or(Value::Null),
                l3: false,
            });
            // 主机：nodeagent 优先，没有就用旧系统同步的 Komari 最新状态
            let na = snap.hosts.get(&head_name(name)).filter(|h| h.0 == "node");
            let host = if let Some((_, recv, s, _)) = na {
                HostObs { received_at: *recv, online: true, cpu: s["cpu"]["pct"].as_f64(), ram: s["mem"]["pct"].as_f64(), ram_total: Some(100.0),
                    disk: s["disk"]["pct"].as_f64(), disk_total: Some(100.0) }
            } else if a["host"].is_object() {
                let h = &a["host"];
                HostObs { received_at: parse_time(&h["time"]).unwrap_or(0.0), online: h["online"].as_bool().unwrap_or(false), cpu: h["cpu"].as_f64(),
                    ram: h["ram"].as_f64(), ram_total: h["ram_total"].as_f64(), disk: h["disk"].as_f64(), disk_total: h["disk_total"].as_f64() }
            } else {
                HostObs::default()
            };
            if na.is_some() || a["host"].is_object() {
                inp.hosts.insert(uuid.clone(), host);
            }
            let ch = &a["channel"];
            if ch.is_object() {
                inp.entries.insert(uuid.clone(), EntryObs {
                    enable: ch["enable"].as_i64().map(|x| x != 0).or(ch["enable"].as_bool()).unwrap_or(false),
                    outbound_tag: opt_str(&ch["outbound_tag"]),
                    target: opt_str(&ch["target"]),
                    target_port: opt_u16(&ch["target_port"]),
                    protocol: opt_str(&ch["protocol"]),
                });
            }
            if let Some(p) = self.probes.get(&uuid) {
                inp.probes.insert(uuid.clone(), p.clone());
            }
        }
        for (relay, host) in &relay_to_host {
            if let Some((k, recv, _, _)) = snap.hosts.get(host) {
                if k == "relay" {
                    inp.relays.insert(relay.clone(), RelayObs { fresh: now - recv <= 90.0, received_at: *recv, ts: *recv });
                }
            }
        }
        inp.control = self.control.clone();
        let out = self.engine.evaluate(&inp, now);
        let native: BTreeMap<String, Value> =
            out.evaluation.nodes.iter().map(|n| (n.uuid.clone(), serde_json::to_value(n).unwrap_or(Value::Null))).collect();

        let fresh_s = cfg.native_fresh_s as f64;
        let mut switched: HashSet<String> = HashSet::new();
        for (relay, members) in &by_relay {
            if relay.is_empty() {
                continue;
            }
            let host = relay_to_host.get(relay).cloned();
            let relayd_age = host.as_ref().and_then(|h| snap.hosts.get(h)).map(|x| now - x.1);
            let targets: HashSet<&String> = host.as_ref().and_then(|h| snap.targets.get(h)).map(|v| v.iter().collect()).unwrap_or_default();
            let mut cov = self.coverage.remove(relay).unwrap_or_default();
            cov.relay = relay.clone();
            cov.host_id = host.clone();
            cov.relayd_age_s = relayd_age.map(|x| (x * 10.0).round() / 10.0);
            cov.nodes = members.len();
            cov.legacy_only = legacy_only.get(relay).cloned().unwrap_or_default();
            cov.missing.clear();
            cov.stale.clear();
            cov.targeted = 0;
            cov.fresh = 0;
            for (uuid, short) in members {
                if !targets.contains(uuid) {
                    cov.missing.push(short.clone());
                    continue;
                }
                cov.targeted += 1;
                let p = self.probes.get(uuid);
                let ok = p
                    .map(|p| {
                        p.tcp.as_ref().map(|r| now - r.measured_at <= fresh_s).unwrap_or(false)
                            && p.egress.as_ref().map(|r| now - r.measured_at <= fresh_s).unwrap_or(false)
                    })
                    .unwrap_or(false);
                if ok {
                    cov.fresh += 1;
                } else {
                    cov.stale.push(short.clone());
                }
            }
            let complete = !members.is_empty() && cov.fresh == members.len() && relayd_age.map(|a| a <= 90.0).unwrap_or(false);
            cov.complete = complete;
            if complete {
                if cov.complete_since.is_none() {
                    let restored = self.restore.remove(relay).filter(|(_, last)| now - last <= 300.0).map(|(since, _)| since);
                    cov.complete_since = Some(restored.unwrap_or(now));
                }
                self.last_complete_at.insert(relay.clone(), now);
            } else {
                self.restore.remove(relay);
                cov.complete_since = None;
            }
            let use_native = cfg.native_health == "auto" && cov.complete_since.map(|t| now - t >= cfg.native_after_s as f64).unwrap_or(false);
            cov.source = if use_native { "native" } else { "legacy" }.into();
            if use_native {
                switched.insert(relay.clone());
            }
            // 原生与旧判定逐节点比较（只比较在测且有结果的节点）
            let mut dis = vec![];
            for (uuid, short) in members {
                let (Some(n), Some(l)) = (native.get(uuid), legacy_nodes.get(uuid)) else { continue };
                if !targets.contains(uuid) {
                    continue;
                }
                cov.compared += 1;
                if n["status"] == l["status"] && n["reason"] == l["reason"] {
                    cov.agree += 1;
                } else {
                    dis.push(json!({"node": short, "native": {"status": n["status"], "reason": n["reason"]},
                        "legacy": {"status": l["status"], "reason": l["reason"]}}));
                }
            }
            cov.last_disagree = dis;
            self.coverage.insert(relay.clone(), cov);
        }
        self.last_eval_ms = t0.elapsed().as_secs_f64() * 1000.0;
        if switched.is_empty() {
            return None;
        }
        let mut h = health.clone();
        if let Some(Value::Object(nodes)) = h.pointer_mut("/monitor/nodes") {
            for (uuid, v) in nodes.iter_mut() {
                let relay = v["relay"].as_str().unwrap_or("").to_string();
                if switched.contains(&relay) {
                    if let Some(n) = native.get(uuid) {
                        *v = n.clone();
                        v["source"] = json!("native");
                    }
                }
            }
        }
        Some(h)
    }
}

/// relayd 遥测 → relay_xray_down（取代依赖 x-ui 面板的判定）
pub fn relay_xray_observations(cfg: &EdgeCfg, snap: &EdgeSnapshot, health: &Value, now: f64) -> Vec<Observation> {
    let mut out = vec![];
    let nodes = health.pointer("/monitor/nodes").and_then(|x| x.as_object()).cloned().unwrap_or_default();
    for (host, relay) in &cfg.relay_hosts {
        let Some((kind, recv, _, x)) = snap.hosts.get(host) else { continue };
        if kind != "relay" {
            continue;
        }
        let mut l = Labels::new();
        l.insert("kind".into(), "relay".into());
        l.insert("relay".into(), relay.clone());
        l.insert("relay_id".into(), short_id(relay));
        l.insert("host".into(), host.clone());
        let age = now - recv;
        let mut push = |cause: &str, summary: String, since: Option<f64>, for_s: f64, ev: Value| {
            let mut l = l.clone();
            l.insert("cause".into(), cause.into());
            out.push(Observation {
                rule: "relay_xray_down".into(),
                labels: l,
                severity: Severity::Critical,
                summary,
                evidence: ev,
                active_since: since,
                for_s,
                notify_resolved: true,
            });
        };
        if age <= 120.0 {
            let running = x.pointer("/xray/running").and_then(|v| v.as_bool());
            let q_ok = x.pointer("/last_query/ok").and_then(|v| v.as_bool());
            let last_ok = x.get("last_ok").and_then(|v| v.as_f64()).filter(|t| *t > 0.0);
            let ev = json!({"relayd_age_s": age.round(), "xray_running": running, "stats_api_ok": q_ok, "last_ok": last_ok,
                "last_error": x.pointer("/last_query/err"), "xray_version": x.pointer("/xray/version")});
            if running == Some(false) {
                push("xray_process_down", format!("{relay} 上 Xray 进程不在运行（relayd 报告）"), None, 30.0, ev);
            } else if q_ok == Some(false) {
                push("xray_api_unreachable", format!("{relay} 的 Xray Stats API 连续不可达（relayd 报告）"), last_ok.or(Some(now)),
                    cfg.xray_api_down_s as f64, ev);
            }
        } else {
            // relayd 不上报了：名下节点也在失败 → 当作 Xray/线路机故障
            let mine: Vec<&Value> = nodes.values().filter(|n| n["relay"].as_str() == Some(relay.as_str())).collect();
            let failing = mine.iter().filter(|n| n["status"] == "fail").count();
            if !mine.is_empty() && failing >= 2 && failing * 2 >= mine.len() {
                push("relayd_silent_nodes_failing", format!("{relay} 的 relayd 已 {:.0} 秒没有上报，且名下 {failing}/{} 个节点失败", age, mine.len()),
                    Some(*recv), 60.0, json!({"relayd_age_s": age.round(), "failing": failing, "nodes": mine.len()}));
            }
        }
    }
    out
}

/// fleetd-ingest 的原生主机告警 → Observation
pub fn host_observations(ing: &fleetd_ingest::Ingest, cfg: &EdgeCfg, now: f64, hold_offline: bool) -> Vec<Observation> {
    ing.evaluate(now)
        .into_iter()
        .filter(|a| !(hold_offline && a.rule == "native_host_offline"))
        .map(|a| {
            let mut l = Labels::new();
            l.insert("host".into(), a.host_id.clone());
            l.insert("kind".into(), a.kind.clone());
            l.insert("source".into(), "agent".into());
            l.insert("cause".into(), a.rule.into());
            if a.kind == "node" {
                l.insert("node".into(), short_id(&a.host_id));
            } else if let Some(r) = cfg.relay_hosts.get(&a.host_id) {
                l.insert("relay".into(), r.clone());
                l.insert("relay_id".into(), short_id(r));
            }
            Observation {
                rule: a.rule.into(),
                labels: l,
                severity: Severity::parse(a.severity).unwrap_or(Severity::Warning),
                summary: a.summary,
                evidence: a.evidence,
                active_since: Some(a.active_since),
                for_s: a.for_s,
                notify_resolved: true,
            }
        })
        .collect()
}


/// 线路机自身的检查（relayd 探测，node = 线路机资产 uuid）：
/// - 出口验证：只看连通性（记录实际出口 IP，不比对）；
/// - TCP 心跳：对端线路机连主节点入口端口；每个对端最近两次都失败 → relay_tcp_down（严重）。
pub fn relay_self_observations(ing: &fleetd_ingest::Shared, cfg: &EdgeCfg, relay_uuids: &[(String, String)], now: f64) -> Vec<Observation> {
    let mut out = vec![];
    let ids: Vec<String> = relay_uuids.iter().map(|x| x.0.clone()).collect();
    // (node, relay host, kind) → 最近两条（新→旧）：(status, doc)
    let rows: Vec<(String, String, String, String, String)> = ing.with_db(|c| {
        let mut v = vec![];
        let q = format!(
            "SELECT node, relay, check_kind, status, doc FROM probe_land WHERE measured_at>=?1 AND check_kind IN ('tcp','egress') AND node IN ({}) ORDER BY measured_at DESC",
            vec!["?"; ids.len().max(1)].join(",")
        );
        if let Ok(mut st) = c.prepare(&q) {
            let mut params: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Real(now - 900.0)];
            params.extend(ids.iter().map(|s| rusqlite::types::Value::Text(s.clone())));
            if let Ok(it) = st.query_map(rusqlite::params_from_iter(params.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))) {
                v = it.flatten().collect();
            }
        }
        v
    });
    for (uuid, name) in relay_uuids {
        let host = cfg.relay_hosts.iter().find(|(_, n)| *n == name).map(|(h, _)| h.clone()).unwrap_or_default();
        let mut l = Labels::new();
        l.insert("kind".into(), "relay".into());
        l.insert("relay".into(), name.clone());
        l.insert("relay_id".into(), short_id(name));
        l.insert("host".into(), host.clone());
        // 出口验证只算连通性（线路机经主节点出网取到 IP 即通过），不比对 IP：relay_egress_mismatch 不再产生（留在 scope 里让旧告警恢复）
        // 自测停滞：线路机批次照常到（≤90 s），但它自己测的出口验证 / 对端 TCP 超过 5 分钟没有新行（relayd 探测线程退出之类）
        if !host.is_empty() {
            let (recv, last_own): (Option<f64>, Option<f64>) = ing.with_db(|c| {
                let recv = c.query_row("SELECT received_at FROM host_latest WHERE host_id=?1", [&host], |r| r.get(0)).ok();
                let last = c
                    .query_row("SELECT max(measured_at) FROM probe_land WHERE relay=?1 AND check_kind IN ('tcp','egress') AND measured_at>=?2", rusqlite::params![&host, now - 86400.0], |r| r.get::<_, Option<f64>>(0))
                    .ok()
                    .flatten();
                (recv, last)
            });
            // 从没测过（旧版 relayd）不算停滞
            if let (Some(recv), Some(last)) = (recv, last_own) {
                if now - recv <= 90.0 && now - last > 300.0 {
                    let mut l = l.clone();
                    l.insert("cause".into(), "probe_stalled".into());
                    out.push(Observation {
                        rule: "relay_probe_stalled".into(),
                        labels: l,
                        severity: Severity::Warning,
                        summary: format!("{name} 的批次正常上报，但它的出口验证 / 对端 TCP 已 {} 分钟没有新结果（relayd 探测可能已停止）", ((now - last) / 60.0).round()),
                        evidence: json!({"host": host, "last_probe_at": last, "last_batch_at": recv}),
                        active_since: Some(last + 300.0),
                        for_s: 0.0,
                        notify_resolved: true,
                    });
                }
            }
        }
        // TCP 心跳（对端测）：按对端分组，每个对端最近两次都失败才算
        let mut peers: std::collections::BTreeMap<String, Vec<(String, Value)>> = Default::default();
        for r in rows.iter().filter(|r| r.0 == *uuid && r.2 == "tcp") {
            let e = peers.entry(r.1.clone()).or_default();
            if e.len() < 2 {
                e.push((r.3.clone(), serde_json::from_str(&r.4).unwrap_or(Value::Null)));
            }
        }
        if !peers.is_empty() && peers.values().all(|v| v.len() == 2 && v.iter().all(|x| x.0 != "ok")) {
            let mut l = l.clone();
            l.insert("cause".into(), "upstream_down".into());
            let since = peers.values().filter_map(|v| v[1].1["measured_at"].as_f64()).fold(f64::MAX, f64::min);
            out.push(Observation {
                rule: "relay_tcp_down".into(),
                labels: l,
                severity: Severity::Critical,
                summary: format!("对端线路机都连不上 {name} 的主节点入口（{} 个对端，各连续两次失败）", peers.len()),
                evidence: json!({"peers": peers.iter().map(|(k, v)| json!({"from": k, "latest": v.iter().map(|x| x.1.clone()).collect::<Vec<_>>()})).collect::<Vec<_>>()}),
                active_since: (since < f64::MAX).then_some(since),
                for_s: 0.0,
                notify_resolved: true,
            });
        }
    }
    out
}


/// 唯一的出口安全规则：节点的实际出口 IP 等于某台线路机自己的公网 IP（流量没走落地、从线路机直接出去了）→ egress_route_leak（严重）。
/// 只看 15 分钟内的出口验证 / 实际入口到出口结果；线路机自己的主入口不适用。
pub fn egress_leak_observations(ov: &Value, now: f64) -> Vec<Observation> {
    let assets = ov["assets"].as_array().cloned().unwrap_or_default();
    let relay_ips: Vec<(String, String)> = assets
        .iter()
        .filter(|a| a["team"] == "线路机")
        .filter_map(|a| Some((a["ip"].as_str()?.to_string(), a["name"].as_str().unwrap_or("").split(" · ").next().unwrap_or("").to_string())))
        .filter(|(ip, _)| !ip.is_empty())
        .collect();
    let mut out = vec![];
    for a in assets.iter().filter(|a| a["team"] != "线路机") {
        let mut seen: Option<(String, String, f64)> = None;
        for (obj, at_key) in [(&a["probe"], "egress_at"), (&a["path"], "measured_at")] {
            let Some(ip) = obj["actual_ip"].as_str() else { continue };
            let at = obj[at_key].as_f64().unwrap_or(0.0);
            if now - at > 900.0 {
                continue;
            }
            if let Some((_, rn)) = relay_ips.iter().find(|(rip, _)| rip == ip) {
                seen = Some((ip.to_string(), rn.clone(), at));
            }
        }
        let Some((ip, relay, at)) = seen else { continue };
        let name = a["name"].as_str().unwrap_or("").split(" · ").next().unwrap_or("").to_string();
        let mut l = crate::rules::node_labels(a);
        l.insert("cause".into(), "egress_route_leak".into());
        out.push(Observation {
            rule: "egress_route_leak".into(),
            labels: l,
            severity: Severity::Critical,
            summary: format!("{name} 的出口是{relay}自己的 IP {ip}：流量没有经过落地节点"),
            evidence: json!({"actual_ip": ip, "relay": relay, "measured_at": at}),
            active_since: Some(at),
            for_s: 0.0,
            notify_resolved: true,
        });
    }
    out
}

#[cfg(test)]
mod leak_tests {
    use super::*;
    #[test]
    fn leak_only_when_egress_is_a_relay_ip() {
        let now = 1_000_000.0;
        let ov = json!({"assets": [
            {"uuid": "r", "team": "线路机", "name": "线路机3", "ip": "38.58.57.69"},
            {"uuid": "a", "team": "团队1", "name": "团队1-31 · MX · 201.113.108.194", "ip": "189.235.77.73", "relay": "线路机3", "relay_port": 20007,
             "probe": {"actual_ip": "201.113.108.194", "egress_at": now - 10.0}},
            {"uuid": "b", "team": "团队1", "name": "团队1-02 · X · 1.1.1.1", "ip": "1.1.1.1", "relay": "线路机3", "relay_port": 20008,
             "probe": {"actual_ip": "1.1.1.1", "egress_at": now - 10.0}, "path": {"actual_ip": "38.58.57.69", "measured_at": now - 20.0}},
            {"uuid": "c", "team": "团队1", "name": "团队1-03", "ip": "2.2.2.2", "path": {"actual_ip": "38.58.57.69", "measured_at": now - 5000.0}}]});
        let o = egress_leak_observations(&ov, now);
        assert_eq!(o.len(), 1, "IP 不同但不是线路机 IP 的不报；过期的不报");
        assert_eq!(o[0].rule, "egress_route_leak");
        assert_eq!(o[0].labels["node"], "t1-02");
    }
}
