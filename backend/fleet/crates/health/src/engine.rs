//! 新架构的综合判定（fleetd 用）：把边缘探测结果（fleet-probe 的 `ProbeResult`）、nodeagent 主机遥测、
//! 线路机遥测新鲜度、入口配置观测，拼成旧 `monitor.py` 的资产形状，交给逐行移植的 [`crate::legacy`] 判定，
//! 再叠加 v1.2 新增的三条规则。
//!
//! 与旧系统逐项一致的部分见 legacy.rs；**有意的差异**（都只在新输入出现时才触发，旧输入下结果不变）：
//! 1. `probe_target_down`：边缘报告两个公共来源的控制路径都失败时，egress 记 `unknown(probe_target_down)`，
//!    不进状态机、不计入可用率（旧系统会判 fail(timeout)）。
//! 2. `correlated_node_failure`：同一线路机（或同一供应商）名下 ≥ 3 个且 ≥ 80% 的节点 egress 同时 fail，
//!    而该线路机控制路径正常时，这些节点的原因码改为 `correlated_node_failure`（状态仍是 fail，计入可用率），
//!    并输出一条聚合 [`Correlation`]。旧系统每个节点各报各的。
//! 3. 没有部署 L3（`NodeSpec.l3 == false`）的节点，`ingress` 记 `na`；旧系统在没有 path 数据时记 unknown，
//!    会让整个节点变成 unknown。
//! 4. 新鲜度按新探测周期：egress 60 s → `fresh_after(60)=180 s`（旧 300 s → 900 s）。
//! 5. UDP / IPv6 结果作为信息性检查（`udp`、`ipv6`）附加，只出告警，不参与节点状态（与 tcp、latency 同类）。
use std::collections::BTreeMap;

use fleet_probe::{reason, CheckKind, ProbeResult, Status};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::legacy::{self, Check, Evaluation, Monitor, NodeHealth, Params, Window};

/// 不参与节点状态的检查
pub const INFORMATIONAL: &[&str] = &["tcp", "latency", "udp", "ipv6"];

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EngineConfig {
    pub params: Params,
    /// nodeagent 遥测多久内算新鲜（秒）
    pub host_fresh_s: f64,
    /// 控制路径结果多久内算数（秒）
    pub control_max_age_s: f64,
    pub correlated_min_nodes: usize,
    pub correlated_ratio: f64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            params: Params { tcp_interval: 30.0, egress_interval: 60.0, path_interval: 60.0 },
            host_fresh_s: 120.0,
            control_max_age_s: 150.0,
            correlated_min_nodes: 3,
            correlated_ratio: 0.8,
        }
    }
}

/// 节点意图（来自中控的资源模型）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct NodeSpec {
    pub uid: String,
    pub name: String,
    pub team: String,
    /// 所属线路机（名字或 uid，与 `relays` 的键一致）；线路机自身为 None
    pub relay: Option<String>,
    pub provider: String,
    /// 期望出口 IP
    pub ip: String,
    /// 入口端口；None = 没有入口（线路机自身）
    pub relay_port: Option<u16>,
    pub outbound_tag: Option<String>,
    pub target: Option<String>,
    pub target_port: Option<u16>,
    pub protocol: Option<String>,
    /// 有 nodeagent（旧 host_measured）
    pub host_required: bool,
    /// 访问规则（旧 policy 形状：{"mode", "domains"}）
    #[serde(default = "observe")]
    pub policy: Value,
    /// 维护窗口与阈值（旧 profile 形状：{"maintenance_until", "thresholds"}）
    #[serde(default)]
    pub profile: Value,
    /// 是否部署了 L3 全链路探测
    #[serde(default)]
    pub l3: bool,
}

fn observe() -> Value {
    json!({"mode": "observe", "domains": []})
}

/// nodeagent 最近一次主机遥测
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct HostObs {
    pub received_at: f64,
    pub online: bool,
    pub cpu: Option<f64>,
    pub ram: Option<f64>,
    pub ram_total: Option<f64>,
    pub disk: Option<f64>,
    pub disk_total: Option<f64>,
}

/// 线路机：中控看到的遥测新鲜度（collector）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RelayObs {
    pub fresh: bool,
    pub received_at: f64,
    /// 最近一次配置/入口观测时间
    pub ts: f64,
}

/// 线路机上观测到的入口（对应旧 channel）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct EntryObs {
    pub enable: bool,
    pub outbound_tag: Option<String>,
    pub target: Option<String>,
    pub target_port: Option<u16>,
    pub protocol: Option<String>,
}

/// L3 全链路结果（旧 path 形状）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PathObs {
    pub healthy: bool,
    pub measured_at: f64,
    pub actual_ip: Option<String>,
    pub egress_ms: Option<f64>,
    pub rc: Option<i64>,
    pub phase: Option<String>,
}

/// 每个节点、每类检查最近一条结果（egress 取最近一次尝试，含非最终尝试）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct NodeProbes {
    pub tcp: Option<ProbeResult>,
    pub egress: Option<ProbeResult>,
    pub udp: Option<ProbeResult>,
    pub ipv6: Option<ProbeResult>,
    pub path: Option<PathObs>,
}

impl NodeProbes {
    /// 用一条新结果更新（只接受更新的 measured_at）
    pub fn apply(&mut self, r: &ProbeResult) {
        let slot = match r.check {
            CheckKind::Tcp => &mut self.tcp,
            CheckKind::Egress => &mut self.egress,
            CheckKind::Udp => &mut self.udp,
            CheckKind::Ipv6 => &mut self.ipv6,
            // Control 按线路机归组；Path（relayd 原生全链路，fleet-edge 2026-09-24 新增）暂不进节点判定，由中控单独接入
            CheckKind::Control | CheckKind::Path => return,
        };
        if slot.as_ref().is_none_or(|o| o.measured_at <= r.measured_at) {
            *slot = Some(r.clone());
        }
    }
}

/// 一轮输入
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    pub nodes: Vec<NodeSpec>,
    pub probes: BTreeMap<String, NodeProbes>,
    pub hosts: BTreeMap<String, HostObs>,
    pub relays: BTreeMap<String, RelayObs>,
    pub entries: BTreeMap<String, EntryObs>,
    /// 线路机 → 该线路机最近的控制路径结果（每个来源一条）
    pub control: BTreeMap<String, Vec<ProbeResult>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Correlation {
    /// "relay" | "provider"
    pub scope: String,
    pub key: String,
    pub failing: Vec<String>,
    pub total: usize,
    pub at: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Output {
    pub evaluation: Evaluation,
    pub correlations: Vec<Correlation>,
}

/// 把新输入转成旧资产 JSON
pub fn legacy_asset(spec: &NodeSpec, inp: &Inputs, now: f64, cfg: &EngineConfig) -> Value {
    let mut a = json!({
        "uuid": spec.uid, "name": spec.name, "team": spec.team, "relay": spec.relay, "ip": spec.ip,
        "relay_port": spec.relay_port, "outbound_tag": spec.outbound_tag, "target": spec.target,
        "target_port": spec.target_port, "protocol": spec.protocol, "provider": spec.provider,
        "host_measured": spec.host_required, "policy": spec.policy,
    });
    if !spec.profile.is_null() {
        a["profile"] = spec.profile.clone();
    }
    if let Some(h) = inp.hosts.get(&spec.uid) {
        a["host_fresh"] = json!(now - h.received_at <= cfg.host_fresh_s && now - h.received_at >= -30.0);
        a["host"] = json!({"online": h.online, "cpu": h.cpu, "ram": h.ram, "ram_total": h.ram_total,
                           "disk": h.disk, "disk_total": h.disk_total, "time": h.received_at});
    } else {
        a["host_fresh"] = json!(false);
    }
    if let Some(e) = inp.entries.get(&spec.uid) {
        a["channel"] = json!({"enable": e.enable, "outbound_tag": e.outbound_tag, "target": e.target,
                              "target_port": e.target_port, "protocol": e.protocol});
    }
    let p = inp.probes.get(&spec.uid).cloned().unwrap_or_default();
    if p.tcp.is_some() || p.egress.is_some() {
        let mut probe = serde_json::Map::new();
        if let Some(t) = &p.tcp {
            let ok = t.status == Status::Ok;
            probe.insert("tcp_ms".into(), json!(if ok { t.value_ms.unwrap_or(1.0) } else { -1.0 }));
            probe.insert("tcp_at".into(), json!(t.measured_at));
            probe.insert("tcp_rc".into(), json!(if ok { 0 } else { -1 }));
            probe.insert("tcp_phase".into(), json!("connect"));
            probe.insert("tcp_error".into(), if ok { Value::Null } else { json!(t.detail.clone().unwrap_or_else(|| "error".into())) });
        }
        if let Some(e) = &p.egress {
            let ok = e.status == Status::Ok;
            probe.insert("egress_ms".into(), json!(if ok { e.value_ms.unwrap_or(1.0) } else { -1.0 }));
            probe.insert("egress_at".into(), json!(e.measured_at));
            probe.insert("actual_ip".into(), json!(e.actual_ip));
            probe.insert("egress_rc".into(), json!(e.legacy_rc.unwrap_or_else(|| fleet_probe::legacy_rc(e.status, e.reason_code.as_deref()))));
            probe.insert("egress_error".into(), if ok { Value::Null } else { json!(e.reason_code.clone().unwrap_or_else(|| "error".into())) });
            // HL-04：非最终尝试 → attempt < 3（旧系统据此判 pending）；最终尝试 → 3
            let attempt = if e.final_attempt.unwrap_or(true) || ok { if ok { e.attempt.unwrap_or(1) as i64 } else { legacy::EGRESS_ATTEMPTS } } else { e.attempt.unwrap_or(1).min(2) as i64 };
            probe.insert("egress_attempt".into(), json!(attempt));
        }
        a["probe"] = Value::Object(probe);
    }
    if let Some(path) = &p.path {
        a["path"] = json!({"healthy": path.healthy, "measured_at": path.measured_at, "actual_ip": path.actual_ip,
                           "egress_ms": path.egress_ms, "rc": path.rc, "phase": path.phase});
    }
    a
}

fn relays_value(inp: &Inputs) -> Value {
    let mut m = serde_json::Map::new();
    for (k, r) in &inp.relays {
        m.insert(k.clone(), json!({"fresh": r.fresh, "received_at": r.received_at, "ts": r.ts}));
    }
    Value::Object(m)
}

fn info_check(key: &str, title: &str, r: &ProbeResult, now: f64, max_age: f64) -> Check {
    let fresh = now - r.measured_at <= max_age;
    let (status, reason) = if !fresh {
        ("unknown", Some("stale".to_string()))
    } else {
        match r.status {
            Status::Ok => ("ok", None),
            Status::Unknown => ("unknown", r.reason_code.clone()),
            _ => ("degraded", r.reason_code.clone()),
        }
    };
    Check {
        key: key.into(),
        title: title.into(),
        critical: false,
        status: status.into(),
        reason,
        measured_at: r.measured_at,
        value: json!(r.value_ms),
        evidence: json!({"actual_ip": r.actual_ip, "reason_code": r.reason_code, "source": r.source}),
        since: now,
        pending: false,
    }
}

/// 综合判定引擎（持有状态机，可序列化后落库）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Engine {
    pub cfg: EngineConfig,
    pub monitor: Monitor,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Engine {
        Engine { cfg, monitor: Monitor::new() }
    }

    fn control_ok(&self, inp: &Inputs, relay: &str, now: f64) -> Option<bool> {
        let rs = inp.control.get(relay)?;
        let fresh: Vec<&ProbeResult> = rs.iter().filter(|r| now - r.measured_at <= self.cfg.control_max_age_s).collect();
        if fresh.is_empty() {
            return None;
        }
        Some(fresh.iter().any(|r| r.status == Status::Ok))
    }

    pub fn evaluate(&mut self, inp: &Inputs, now: f64) -> Output {
        let relays = relays_value(inp);
        let mut nodes: Vec<NodeHealth> = Vec::with_capacity(inp.nodes.len());
        for spec in &inp.nodes {
            let asset = legacy_asset(spec, inp, now, &self.cfg);
            let mut n = legacy::diagnose_with(&asset, &relays, now, &self.cfg.params);
            let p = inp.probes.get(&spec.uid);
            // 差异 1：probe_target_down
            if let Some(e) = p.and_then(|p| p.egress.as_ref()) {
                if e.status == Status::Unknown && e.reason_code.as_deref() == Some(reason::PROBE_TARGET_DOWN) {
                    if let Some(c) = n.check_mut("egress") {
                        c.status = "unknown".into();
                        c.reason = Some(reason::PROBE_TARGET_DOWN.into());
                        c.pending = false;
                    }
                }
            }
            // 差异 3：没有 L3 的节点 ingress = na
            if !spec.l3 {
                if let Some(c) = n.check_mut("ingress") {
                    c.status = "na".into();
                    c.reason = None;
                }
            }
            // 差异 5：UDP / IPv6 信息性检查
            if spec.relay_port.is_some() {
                if let Some(r) = p.and_then(|p| p.udp.as_ref()) {
                    n.checks.push(info_check("udp", "UDP 出口一致性", r, now, 3.0 * 900.0));
                }
                if let Some(r) = p.and_then(|p| p.ipv6.as_ref()) {
                    n.checks.push(info_check("ipv6", "IPv6 出口", r, now, 3.0 * 900.0));
                }
            }
            nodes.push(n);
        }
        // 重新按新的检查集合算一次 diagnose 级别的结论，再跑状态机
        let mut ev = self.monitor.evaluate_nodes(nodes, now, INFORMATIONAL);
        // 差异 2：correlated_node_failure
        let mut correlations = Vec::new();
        let specs: BTreeMap<&str, &NodeSpec> = inp.nodes.iter().map(|s| (s.uid.as_str(), s)).collect();
        for scope in ["relay", "provider"] {
            let mut groups: BTreeMap<String, (Vec<String>, usize, Option<String>)> = BTreeMap::new();
            for n in &ev.nodes {
                let Some(spec) = specs.get(n.uuid.as_str()) else { continue };
                let Some(relay) = &spec.relay else { continue };
                let Some(eg) = n.check("egress") else { continue };
                if eg.status == "na" || n.maintenance {
                    continue;
                }
                let key = if scope == "relay" { relay.clone() } else { spec.provider.clone() };
                let g = groups.entry(key).or_insert_with(|| (Vec::new(), 0, Some(relay.clone())));
                g.1 += 1;
                if eg.status == "fail" {
                    g.0.push(n.uuid.clone());
                }
                if g.2.as_deref() != Some(relay.as_str()) {
                    g.2 = None; // 跨线路机的供应商组：看每个节点自己的线路机控制路径
                }
            }
            for (key, (failing, total, relay)) in groups {
                if failing.len() < self.cfg.correlated_min_nodes || (failing.len() as f64) < self.cfg.correlated_ratio * total as f64 {
                    continue;
                }
                let control_ok = |uid: &str| {
                    let r = relay.clone().or_else(|| specs.get(uid).and_then(|s| s.relay.clone())).unwrap_or_default();
                    self.control_ok(inp, &r, now) == Some(true)
                };
                if !failing.iter().all(|u| control_ok(u)) {
                    continue;
                }
                for n in ev.nodes.iter_mut().filter(|n| failing.contains(&n.uuid)) {
                    if n.status == "fail" {
                        n.reason = Some("correlated_node_failure".into());
                    }
                }
                correlations.push(Correlation { scope: scope.into(), key, failing, total, at: now });
            }
        }
        Output { evaluation: ev, correlations }
    }

    pub fn windows(&self, uid: &str, now: f64) -> BTreeMap<String, Window> {
        self.monitor.availability_windows(uid, now, &[1, 7, 30])
    }
}
