//! 旧 `teamops/monitor.py` 的逐行移植：`diagnose`（逐项检查）+ `Monitor.transition`（每项的状态机）
//! + `Monitor.evaluate`（综合状态、since、分钟样本）+ `availability_windows`。
//!
//! 输入保持旧系统的“资产 JSON”形状（`serde_json::Value`），这样同一份输入在 Python 与 Rust 上逐字可比
//! （tests/legacy_golden.rs 用真实 monitor.py 生成的 golden 对照）。新架构的输入由 [`crate::engine`] 转成这个形状。
//!
//! Python 语义细节都按原样保留：真值判断（`truthy`）、`dict.get(k, default)` 只在键缺失时取默认、
//! `1 == 1.0 == True`（`py_eq`）、`round(x, 2)`、ISO 时间截到微秒。
//! 不移植的部分：incidents 表与审计（属于 fleetd 告警引擎；这里只给出 `opens_incident` 标志）。
use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

pub const EGRESS_ATTEMPTS: i64 = 3;
pub const WINDOW: usize = 6;
pub const UNSTABLE_FLIPS: usize = 3;

pub fn fresh_after(interval: f64) -> f64 {
    f64::max(120.0, 3.0 * interval)
}

/// (fail_after, recover_after)
pub fn thresholds(key: &str) -> (i64, i64) {
    match key {
        "ingress" | "egress" => (2, 1),
        "host" => (2, 2),
        _ => (1, 1),
    }
}

fn windowed(key: &str) -> bool {
    key == "egress" || key == "ingress"
}

/// 旧 REASONS：curl 退出码 → 原因码
pub fn reason_for_rc(rc: &Value) -> &'static str {
    let Some(n) = num(rc) else { return "unknown" };
    match n as i64 {
        _ if n.fract() != 0.0 => "unknown",
        28 => "timeout",
        35 | 52 | 55 | 56 => "auth",
        7 | 97 => "upstream_down",
        _ => "unknown",
    }
}

// ---------- Python 语义辅助 ----------

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Python `==`：数字按值比较（含 bool），容器递归
pub fn py_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(_) | Value::Bool(_), Value::Number(_) | Value::Bool(_)) => num(a) == num(b),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| py_eq(v, w))),
        _ => a == b,
    }
}

/// `d.get(k)`：键缺失或值为 null 都是 None（Python 里 null 也是 None）
fn get<'a>(d: &'a Value, k: &str) -> &'a Value {
    d.get(k).unwrap_or(&Value::Null)
}

/// `d.get(k) or {}`
fn obj_or_empty(v: &Value) -> Value {
    if truthy(v) {
        v.clone()
    } else {
        json!({})
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()`，小数秒截到微秒；无时区按 UTC
pub fn parse_iso(s: &str) -> Option<f64> {
    let s = s.trim();
    let (date, rest) = s.split_at(s.find(['T', ' ']).unwrap_or(s.len()));
    let mut dp = date.split('-');
    let (y, m, d): (i64, i64, i64) = (dp.next()?.parse().ok()?, dp.next()?.parse().ok()?, dp.next()?.parse().ok()?);
    let mut secs = days_from_civil(y, m, d) as f64 * 86400.0;
    if rest.is_empty() {
        return Some(secs);
    }
    let rest = &rest[1..];
    let tz_pos = rest.find(['Z', '+', '-']);
    let (time, tz) = match tz_pos {
        Some(p) => (&rest[..p], &rest[p..]),
        None => (rest, ""),
    };
    let mut tp = time.split(':');
    let h: f64 = tp.next()?.parse().ok()?;
    let mi: f64 = tp.next().unwrap_or("0").parse().ok()?;
    let sec_s = tp.next().unwrap_or("0");
    let (whole, frac) = sec_s.split_once('.').unwrap_or((sec_s, ""));
    let sec: f64 = whole.parse().ok()?;
    let micros: f64 = if frac.is_empty() {
        0.0
    } else {
        let f: String = frac.chars().take(6).collect();
        format!("{f:0<6}").parse::<f64>().ok()? / 1e6
    };
    secs += h * 3600.0 + mi * 60.0 + sec;
    if !tz.is_empty() && tz != "Z" {
        let sign = if tz.starts_with('-') { -1.0 } else { 1.0 };
        let z = &tz[1..];
        let (zh, zm) = z.split_once(':').unwrap_or((&z[..z.len().min(2)], if z.len() > 2 { &z[2..] } else { "0" }));
        secs -= sign * (zh.parse::<f64>().ok()? * 3600.0 + zm.parse::<f64>().ok()? * 60.0);
    }
    // Python：timestamp() = 整秒 + 微秒/1e6（浮点加法顺序与此一致）
    Some(secs + micros)
}

pub fn timestamp(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::String(s) if s.is_empty() => 0.0,
        Value::String(s) => parse_iso(s).unwrap_or(0.0),
        _ => num(v).unwrap_or(0.0),
    }
}

pub fn recent(v: &Value, now: f64, age: f64) -> bool {
    let s = timestamp(v);
    s > 0.0 && -30.0 <= now - s && now - s <= age
}

/// Python `round(x, 2)`（对 .5 边界的银行家舍入不做精确模拟；只影响数值展示，不影响判定）
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn percent(used: &Value, total: &Value) -> Option<f64> {
    if used.is_null() || !truthy(total) {
        return None;
    }
    Some(round2(100.0 * num(used)? / num(total)?))
}

fn protocol_name(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.to_lowercase(),
        v if !truthy(v) => String::new(),
        v => v.to_string().to_lowercase(),
    };
    match s.as_str() {
        "socks5" => "socks".into(),
        "ss" => "shadowsocks".into(),
        _ => s,
    }
}

/// 旧 policy.decision(host, policy)
pub fn policy_decision(host: &str, policy: &Value) -> &'static str {
    let mode = policy["mode"].as_str().unwrap_or("observe");
    if mode == "blocked" {
        return "blocked";
    }
    let host = host.to_lowercase();
    let host = host.trim_end_matches('.');
    let hit = policy["domains"].as_array().into_iter().flatten().filter_map(|p| p.as_str()).any(|p| {
        host == p.trim_start_matches('=') || (!p.starts_with('=') && host.ends_with(&format!(".{p}")))
    });
    if (mode == "allowlist" && !hit) || (mode == "denylist" && hit) {
        "blocked"
    } else {
        "routed"
    }
}

// ---------- 数据结构 ----------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Check {
    pub key: String,
    pub title: String,
    pub critical: bool,
    pub status: String,
    pub reason: Option<String>,
    pub measured_at: f64,
    pub value: Value,
    pub evidence: Value,
    pub since: f64,
    pub pending: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NodeHealth {
    pub uuid: String,
    pub name: String,
    pub team: String,
    pub relay: Value,
    pub status: String,
    pub reason: Option<String>,
    pub since: f64,
    /// 1 / 0 / null（unknown）
    pub availability: Option<i64>,
    pub checks: Vec<Check>,
    pub metrics: Value,
    pub limits: Value,
    pub maintenance: bool,
    pub checked_at: f64,
}

impl NodeHealth {
    pub fn check(&self, key: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.key == key)
    }
    pub fn check_mut(&mut self, key: &str) -> Option<&mut Check> {
        self.checks.iter_mut().find(|c| c.key == key)
    }
}

/// 节点级“决定性检查”：按 fail → degraded → unknown 的顺序取第一个；fail/unknown 只看 critical，
/// degraded 看除 `informational` 以外的所有检查（旧系统：除 tcp、latency）。
pub fn deciding<'a>(checks: &'a [Check], informational: &[&str]) -> Option<&'a Check> {
    for status in ["fail", "degraded", "unknown"] {
        for c in checks {
            if c.status == status && (if status != "degraded" { c.critical } else { !informational.contains(&c.key.as_str()) }) {
                return Some(c);
            }
        }
    }
    None
}

pub const LEGACY_INFORMATIONAL: &[&str] = &["tcp", "latency"];

// ---------- diagnose ----------

/// 新鲜度所依据的探测周期（秒）。默认值 = 旧系统（TCP 30、认证出口 300、入口到出口 60）。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Params {
    pub tcp_interval: f64,
    pub egress_interval: f64,
    pub path_interval: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params { tcp_interval: 30.0, egress_interval: 300.0, path_interval: 60.0 }
    }
}

/// 旧 `diagnose(asset, relays, now)`
pub fn diagnose(asset: &Value, relays: &Value, now: f64) -> NodeHealth {
    diagnose_with(asset, relays, now, &Params::default())
}

pub fn diagnose_with(asset: &Value, relays: &Value, now: f64, params: &Params) -> NodeHealth {
    let mut checks: Vec<Check> = Vec::new();
    let profile = obj_or_empty(get(asset, "profile"));
    let mut limits = json!({"cpu_percent": 90, "memory_percent": 90, "disk_percent": 85, "latency_ms": 3000});
    if let Some(t) = profile.get("thresholds").and_then(|t| t.as_object()) {
        for (k, v) in t {
            limits[k] = v.clone();
        }
    }
    let maintenance = timestamp(get(&profile, "maintenance_until")) > now;

    #[allow(clippy::too_many_arguments)]
    fn add(
        checks: &mut Vec<Check>,
        now: f64,
        key: &str,
        title: &str,
        status: &str,
        reason: Option<&str>,
        at: &Value,
        critical: bool,
        value: Value,
        evidence: Value,
        pending: bool,
    ) {
        let reason = if status == "unknown" {
            Some("stale".to_string())
        } else if pending {
            Some("retrying".to_string())
        } else if status == "ok" {
            None
        } else {
            reason.map(str::to_string)
        };
        checks.push(Check {
            key: key.into(),
            title: title.into(),
            critical,
            status: status.into(),
            reason,
            measured_at: timestamp(at),
            value,
            evidence,
            since: now,
            pending,
        });
    }

    let name = asset["name"].as_str().unwrap_or("").to_string();
    let relay_key = if truthy(get(asset, "relay")) { asset["relay"].as_str().unwrap_or("").to_string() } else { name.clone() };
    let relay = obj_or_empty(get(relays, &relay_key));
    let relay_ok = truthy(get(&relay, "fresh"));
    let own_relay = !truthy(get(asset, "relay")) || py_eq(get(asset, "relay"), &asset["name"]);
    let team = asset["team"].as_str().unwrap_or("");
    add(
        &mut checks,
        now,
        "collector",
        "线路管理通道",
        if relay_ok {
            "ok"
        } else if own_relay {
            "fail"
        } else {
            "unknown"
        },
        Some("stale"),
        get(&relay, "received_at"),
        true,
        Value::Null,
        json!({"fresh": relay_ok}),
        false,
    );
    let host = obj_or_empty(get(asset, "host"));
    let host_required = truthy(get(asset, "host_measured")) || get(asset, "provider") == "ATTVPS" || team == "线路机";
    let host_fresh = truthy(get(asset, "host_fresh"));
    let host_ok = host_fresh && truthy(get(&host, "online"));
    let host_status = if !host_required {
        "na"
    } else if host_ok {
        "ok"
    } else if !host_fresh {
        "unknown"
    } else {
        "fail"
    };
    add(
        &mut checks,
        now,
        "host",
        "主机探针",
        host_status,
        if host_status == "fail" { Some("probe_offline") } else { None },
        get(&host, "time"),
        team == "线路机",
        Value::Null,
        json!({"online": get(&host, "online"), "time": get(&host, "time")}),
        false,
    );
    let mut metrics = Map::new();
    let cpu = if host_ok { num(get(&host, "cpu")) } else { None };
    let mem = if host_ok { percent(get(&host, "ram"), get(&host, "ram_total")) } else { None };
    let disk = if host_ok { percent(get(&host, "disk"), get(&host, "disk_total")) } else { None };
    for (key, title, v) in [("cpu", "CPU 容量", cpu), ("memory", "内存容量", mem), ("disk", "磁盘容量", disk)] {
        let value = v.filter(|x| *x >= 0.0);
        metrics.insert(key.into(), json!(value));
        let limit = num(&limits[format!("{key}_percent")]).unwrap_or(f64::INFINITY);
        let status = match value {
            None => "unknown",
            Some(x) if x >= limit => "degraded",
            Some(_) => "ok",
        };
        add(&mut checks, now, key, title, status, None, get(&host, "time"), false, json!(value), json!({"limit": limits[format!("{key}_percent")]}), false);
    }
    for k in ["tcp_ms", "egress_ms", "path_ms"] {
        metrics.insert(k.into(), Value::Null);
    }
    let probe = obj_or_empty(get(asset, "probe"));
    let path = obj_or_empty(get(asset, "path"));
    let channel = obj_or_empty(get(asset, "channel"));
    if truthy(get(asset, "relay_port")) {
        add(
            &mut checks,
            now,
            "entry",
            "业务入口",
            if !relay_ok {
                "unknown"
            } else if truthy(get(&channel, "enable")) {
                "ok"
            } else {
                "fail"
            },
            None,
            get(&relay, "ts"),
            true,
            Value::Null,
            json!({"enable": get(&channel, "enable")}),
            false,
        );
        let expected = json!([get(asset, "outbound_tag"), get(asset, "target"), get(asset, "target_port"), protocol_name(get(asset, "protocol"))]);
        let actual = json!([get(&channel, "outbound_tag"), get(&channel, "target"), get(&channel, "target_port"), protocol_name(get(&channel, "protocol"))]);
        add(
            &mut checks,
            now,
            "mapping",
            "固定出口绑定",
            if !relay_ok {
                "unknown"
            } else if py_eq(&expected, &actual) {
                "ok"
            } else {
                "fail"
            },
            None,
            get(&relay, "ts"),
            true,
            Value::Null,
            json!({"expected": expected, "actual": actual}),
            false,
        );
        let nn = |v: &Value| num(v).filter(|x| *x >= 0.0).filter(|_| !v.is_null());
        let (tcp_ms, egress_ms, path_ms) = (nn(get(&probe, "tcp_ms")), nn(get(&probe, "egress_ms")), nn(get(&path, "egress_ms")));
        let tcp_fresh = recent(get(&probe, "tcp_at"), now, fresh_after(params.tcp_interval));
        let tcp_failed = truthy(get(&probe, "tcp_error")) || num(get(&probe, "tcp_ms")).is_some_and(|x| x < 0.0);
        let tcp_status = if probe.get("tcp_ms").is_none() {
            "na"
        } else if !tcp_fresh {
            "unknown"
        } else if tcp_ms.is_some() && !tcp_failed {
            "ok"
        } else {
            "fail"
        };
        add(
            &mut checks,
            now,
            "tcp",
            "线路到上游 TCP",
            tcp_status,
            if tcp_status == "fail" { Some("upstream_down") } else { None },
            get(&probe, "tcp_at"),
            false,
            json!(tcp_ms),
            json!({"rc": get(&probe, "tcp_rc"), "phase": get(&probe, "tcp_phase"), "error": get(&probe, "tcp_error")}),
            false,
        );
        let egress_fresh = recent(get(&probe, "egress_at"), now, fresh_after(params.egress_interval));
        let ip_ok = py_eq(get(&probe, "actual_ip"), &asset["ip"]);
        let mismatch = truthy(get(&probe, "actual_ip")) && !ip_ok;
        let egress_failed = !ip_ok || truthy(get(&probe, "egress_error"));
        let attempt = get(&probe, "egress_attempt");
        let attempt_int = attempt.as_i64().filter(|_| attempt.is_i64() || attempt.is_u64());
        let egress_pending = egress_fresh && egress_failed && !mismatch && attempt_int.is_some_and(|a| a < EGRESS_ATTEMPTS);
        let probe_missing = get(asset, "probe").is_null();
        let egress_status = if probe_missing {
            "na"
        } else if !egress_fresh {
            "unknown"
        } else if !egress_failed || egress_pending {
            "ok"
        } else {
            "fail"
        };
        let egress_reason = if mismatch { "egress_mismatch" } else { reason_for_rc(get(&probe, "egress_rc")) };
        add(
            &mut checks,
            now,
            "egress",
            "认证出口验证",
            egress_status,
            if egress_status == "fail" { Some(egress_reason) } else { None },
            get(&probe, "egress_at"),
            true,
            json!(egress_ms),
            json!({"rc": get(&probe, "egress_rc"), "phase": get(&probe, "egress_phase"), "ms": egress_ms, "attempt": attempt,
                   "actual_ip": get(&probe, "actual_ip"), "expected_ip": asset["ip"], "error": get(&probe, "egress_error")}),
            egress_pending,
        );
        // path.get("measured_at", path.get("checked_at", path.get("egress_at")))：只在键缺失时取下一个
        let path_at = path
            .get("measured_at")
            .or_else(|| path.get("checked_at"))
            .or_else(|| path.get("egress_at"))
            .cloned()
            .unwrap_or(Value::Null);
        let path_fresh = recent(&path_at, now, fresh_after(params.path_interval));
        let path_ok = truthy(get(&path, "healthy")) && py_eq(get(&path, "actual_ip"), &asset["ip"]);
        let path_status = if !path_fresh {
            "unknown"
        } else if path_ok {
            "ok"
        } else if policy_decision("www.cloudflare.com", get(asset, "policy")) == "blocked" {
            "degraded"
        } else {
            "fail"
        };
        let mut path_reason = if path_status == "degraded" { "policy_blocked" } else { reason_for_rc(get(&path, "rc")) };
        if tcp_failed && (path_status == "degraded" || path_status == "fail") {
            path_reason = "upstream_down";
        }
        add(
            &mut checks,
            now,
            "ingress",
            "实际入口到出口",
            path_status,
            Some(path_reason),
            &path_at,
            true,
            json!(path_ms),
            json!({"rc": get(&path, "rc"), "phase": get(&path, "phase"), "ms": path_ms, "actual_ip": get(&path, "actual_ip"),
                   "expected_ip": asset["ip"], "attempts": get(&path, "attempts")}),
            false,
        );
        let latency = if egress_fresh { egress_ms } else { None };
        let lat_limit = num(&limits["latency_ms"]).unwrap_or(f64::INFINITY);
        let lat_status = if probe_missing {
            "na"
        } else {
            match latency {
                None => "unknown",
                Some(x) if x >= lat_limit => "degraded",
                Some(_) => "ok",
            }
        };
        add(&mut checks, now, "latency", "出口请求耗时", lat_status, None, get(&probe, "egress_at"), false, json!(latency), json!({"limit": limits["latency_ms"]}), false);
        metrics.insert("tcp_ms".into(), json!(if tcp_fresh { tcp_ms } else { None }));
        metrics.insert("egress_ms".into(), json!(latency));
        metrics.insert("path_ms".into(), json!(if path_fresh { path_ms } else { None }));
        metrics.insert("egress_pending".into(), json!(egress_pending));
    }
    let (status, reason) = match deciding(&checks, LEGACY_INFORMATIONAL) {
        Some(c) => (c.status.clone(), c.reason.clone()),
        None => ("ok".to_string(), None),
    };
    NodeHealth {
        uuid: asset["uuid"].as_str().unwrap_or("").into(),
        name,
        team: team.into(),
        relay: get(asset, "relay").clone(),
        availability: availability_of(&status),
        status: if maintenance { "maintenance".into() } else { status },
        reason,
        since: now,
        checks,
        metrics: Value::Object(metrics),
        limits,
        maintenance,
        checked_at: now,
    }
}

pub fn availability_of(status: &str) -> Option<i64> {
    match status {
        "fail" => Some(0),
        "unknown" => None,
        _ => Some(1),
    }
}

// ---------- 状态机 ----------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CheckState {
    pub status: String,
    pub fails: i64,
    pub oks: i64,
    pub last: Value,
    pub since: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unstable: Option<bool>,
}

impl CheckState {
    fn initial(now: f64) -> CheckState {
        CheckState { status: "ok".into(), fails: 0, oks: 0, last: Value::Null, since: now, reason: None, history: None, unstable: None }
    }
}

/// 节点综合状态的持久部分（旧 health_state 的 key='status' 行）
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StatusState {
    pub status: String,
    pub since: f64,
}

/// 一分钟一行的可用性样本（旧 health_samples 的计数部分）
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub ok: i64,
    pub unknown: i64,
    pub failed: i64,
    pub maintenance: i64,
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, o: Counts) {
        self.ok += o.ok;
        self.unknown += o.unknown;
        self.failed += o.failed;
        self.maintenance += o.maintenance;
    }
}

/// 状态机发生的事（供 fleetd 写审计、开关故障）
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Flip {
    pub node: String,
    pub check: String,
    pub from: String,
    pub to: String,
    pub reason: Option<String>,
    pub at: f64,
}

/// 旧 Monitor 的内存版（持久化由调用方把 `states`/`status`/`hours` 存进自己的库，结构都可序列化）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Monitor {
    /// (节点, 检查) → 状态机
    pub states: BTreeMap<String, BTreeMap<String, CheckState>>,
    pub status: BTreeMap<String, StatusState>,
    /// 上一次的报告（维护期里用）
    pub last_report: BTreeMap<String, NodeHealth>,
    /// 节点 → 分钟桶（已写入的分钟，INSERT OR IGNORE 语义）
    pub minutes: BTreeMap<String, BTreeMap<i64, Counts>>,
    /// 节点 → 小时桶
    pub hours: BTreeMap<String, BTreeMap<i64, Counts>>,
    pub started: BTreeMap<String, f64>,
    pub started_at: Option<f64>,
}

/// 一次 evaluate 的输出
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Evaluation {
    pub nodes: Vec<NodeHealth>,
    pub flips: Vec<Flip>,
    /// 需要开启/保持故障的 (节点, 检查)：critical 且 fail，或 cpu/memory/disk degraded ≥ 120 s
    pub incident_open: Vec<(String, String)>,
    pub attention_degraded: Vec<String>,
}

impl Monitor {
    pub fn new() -> Monitor {
        Monitor::default()
    }

    /// 旧 `Monitor.transition`，就地改写 `check`
    pub fn transition(&mut self, node: &NodeHealth, check: &mut Check, now: f64, flips: &mut Vec<Flip>) -> bool {
        let row = self.states.get(&node.uuid).and_then(|m| m.get(&check.key)).cloned();
        let mut state = row.clone().unwrap_or_else(|| CheckState::initial(now));
        check.since = state.since;
        if node.maintenance {
            let last = self.last_report.get(&node.uuid).and_then(|n| n.check(&check.key)).map(|c| (c.status.clone(), c.since));
            let effective = if row.is_some() { Some((state.status.clone(), state.since)) } else { last };
            check.status = if check.status == "unknown" || effective.is_none() { "unknown".into() } else { effective.clone().unwrap().0 };
            check.since = effective.map(|e| e.1).unwrap_or(now);
            if check.status == "unknown" {
                check.reason = Some("stale".into());
            } else if check.status == "ok" {
                check.reason = None;
            }
            return false;
        }
        if check.key == "tcp" || check.key == "latency" || check.status == "na" {
            return false;
        }
        let previous = state.status.clone();
        let measurement = if ["collector", "entry", "mapping"].contains(&check.key.as_str()) {
            json!([check.status, check.evidence])
        } else {
            json!(check.measured_at)
        };
        if check.status != "unknown" && !check.pending && !py_eq(&measurement, &state.last) {
            let (fail_after, recover_after) = thresholds(&check.key);
            let mut history = state.history.clone().unwrap_or_default();
            if history.len() > WINDOW - 1 {
                history = history[history.len() - (WINDOW - 1)..].to_vec();
            }
            if check.status == "ok" {
                state.oks += 1;
                state.fails = 0;
                if state.oks >= recover_after {
                    state.status = "ok".into();
                } else if state.status == "fail" {
                    state.status = "degraded".into();
                }
                history.push(1);
            } else {
                state.fails += 1;
                state.oks = 0;
                state.status = if check.status == "fail" && state.fails >= fail_after { "fail".into() } else { "degraded".into() };
                state.reason = check.reason.clone();
                history.push(0);
            }
            if windowed(&check.key) {
                let flips_n = history.windows(2).filter(|w| w[0] != w[1]).count();
                state.history = Some(history);
                let unstable = state.status == "ok" && flips_n >= UNSTABLE_FLIPS;
                state.unstable = Some(unstable);
                if unstable {
                    state.status = "degraded".into();
                    state.reason = Some("unstable".into());
                }
            }
            if state.status != previous {
                state.since = now;
            }
            state.last = measurement;
            self.states.entry(node.uuid.clone()).or_default().insert(check.key.clone(), state.clone());
        }
        if check.status != "unknown" {
            check.status = state.status.clone();
            check.since = state.since;
            if check.status != "ok" && matches!(check.reason.as_deref(), None | Some("retrying")) && state.reason.as_deref().is_some_and(|r| !r.is_empty()) {
                check.reason = state.reason.clone();
            }
        }
        if check.status == "ok" && !check.pending {
            check.reason = None;
        }
        if previous != check.status && check.status != "unknown" && check.critical {
            flips.push(Flip { node: node.uuid.clone(), check: check.key.clone(), from: previous, to: check.status.clone(), reason: check.reason.clone(), at: now });
        }
        (check.critical && check.status == "fail") || (["cpu", "memory", "disk"].contains(&check.key.as_str()) && check.status == "degraded" && now - check.since >= 120.0)
    }

    /// 旧 `Monitor.evaluate`：对已经 diagnose 过的节点跑状态机、合成状态与 since、写分钟样本。
    /// `informational`：不参与 degraded 判定的检查（旧系统为 tcp、latency）。
    pub fn evaluate_nodes(&mut self, mut nodes: Vec<NodeHealth>, now: f64, informational: &[&str]) -> Evaluation {
        let mut flips = Vec::new();
        let mut incident_open = Vec::new();
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }
        for node in nodes.iter_mut() {
            let uuid = node.uuid.clone();
            self.started.entry(uuid.clone()).or_insert(now);
            let snapshot = node.clone();
            let mut checks = std::mem::take(&mut node.checks);
            for c in checks.iter_mut() {
                if self.transition(&snapshot, c, now, &mut flips) {
                    incident_open.push((uuid.clone(), c.key.clone()));
                }
            }
            node.checks = checks;
            let (status, reason) = match deciding(&node.checks, informational) {
                Some(c) => (c.status.clone(), c.reason.clone()),
                None => ("ok".to_string(), None),
            };
            node.reason = reason;
            node.availability = availability_of(&status);
            node.status = if node.maintenance { "maintenance".into() } else { status.clone() };
            let previous = self.status.get(&uuid).cloned();
            node.since = match &previous {
                Some(p) if node.maintenance || p.status == status => p.since,
                _ => now,
            };
            let last_report = self.last_report.get(&uuid);
            if node.maintenance || last_report.is_some_and(|l| l.maintenance) {
                node.since = match last_report {
                    Some(l) if l.status == node.status => l.since,
                    _ => now,
                };
            }
            if !node.maintenance {
                if let Some(p) = &previous {
                    let known = ["ok", "degraded", "fail"];
                    if p.status != status && known.contains(&p.status.as_str()) && known.contains(&status.as_str()) {
                        flips.push(Flip { node: uuid.clone(), check: "status".into(), from: p.status.clone(), to: status.clone(), reason: node.reason.clone(), at: now });
                    }
                }
                self.status.insert(uuid.clone(), StatusState { status: status.clone(), since: node.since });
            }
            let bucket = (now / 60.0).floor() as i64 * 60;
            let counts = if node.maintenance {
                Counts { maintenance: 1, ..Default::default() }
            } else {
                Counts {
                    ok: (node.availability == Some(1)) as i64,
                    unknown: node.availability.is_none() as i64,
                    failed: (node.availability == Some(0)) as i64,
                    maintenance: 0,
                }
            };
            let mins = self.minutes.entry(uuid.clone()).or_default();
            if let std::collections::btree_map::Entry::Vacant(e) = mins.entry(bucket) {
                e.insert(counts);
                *self.hours.entry(uuid.clone()).or_default().entry((now / 3600.0).floor() as i64 * 3600).or_default() += counts;
            }
        }
        let attention_degraded = nodes.iter().filter(|n| n.status == "degraded" && n.since <= now - 300.0 && !n.maintenance).map(|n| n.uuid.clone()).collect();
        for n in &nodes {
            self.last_report.insert(n.uuid.clone(), n.clone());
        }
        Evaluation { nodes, flips, incident_open, attention_degraded }
    }

    /// 旧 `Monitor.evaluate`：diagnose + evaluate_nodes
    pub fn evaluate(&mut self, assets: &[Value], relays: &Value, now: f64) -> Evaluation {
        let nodes = assets.iter().map(|a| diagnose(a, relays, now)).collect();
        self.evaluate_nodes(nodes, now, LEGACY_INFORMATIONAL)
    }

    /// 旧 `availability_windows`（小时桶，含当前不完整的小时）
    pub fn availability_windows(&self, asset: &str, now: f64, periods: &[i64]) -> BTreeMap<String, Window> {
        let start = self.started.get(asset).copied().or(self.started_at).unwrap_or(now);
        let hours = self.hours.get(asset);
        periods.iter().map(|d| (d.to_string(), window(hours, start, now, *d))).collect()
    }

    /// 删掉 14 d 以前的分钟桶、400 d 以前的小时桶（旧 maintenance()）
    pub fn prune(&mut self, now: f64) {
        for m in self.minutes.values_mut() {
            m.retain(|b, _| *b as f64 >= now - 14.0 * 86400.0);
        }
        for h in self.hours.values_mut() {
            h.retain(|b, _| *b as f64 >= now - 400.0 * 86400.0);
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Window {
    pub availability_percent: Option<f64>,
    pub coverage_percent: Option<f64>,
    pub successful_samples: i64,
    pub failed_samples: i64,
    pub unknown_samples: i64,
    pub missing_samples: i64,
    pub maintenance_samples: i64,
    pub since: f64,
}

fn round_to(x: f64, d: i32) -> f64 {
    let p = 10f64.powi(d);
    (x * p).round() / p
}

pub fn window(hours: Option<&BTreeMap<i64, Counts>>, start: f64, now: f64, days: i64) -> Window {
    let since = f64::max(start, now - days as f64 * 86400.0);
    let from = (since / 3600.0).floor() as i64 * 3600;
    let mut c = Counts::default();
    if let Some(h) = hours {
        for (_, v) in h.range(from..) {
            c += *v;
        }
    }
    let observed = c.ok + c.unknown + c.failed + c.maintenance;
    let expected = observed.max(((now - since) / 60.0).ceil() as i64);
    let eligible = expected - c.maintenance;
    Window {
        availability_percent: (c.ok + c.failed > 0).then(|| round_to(c.ok as f64 * 100.0 / (c.ok + c.failed) as f64, 3)),
        coverage_percent: (eligible != 0).then(|| round_to((c.ok + c.failed) as f64 * 100.0 / eligible as f64, 2)),
        successful_samples: c.ok,
        failed_samples: c.failed,
        unknown_samples: c.unknown,
        missing_samples: (expected - observed).max(0),
        maintenance_samples: c.maintenance,
        since,
    }
}

/// 便于对比：节点 → (状态, 原因, {检查 → (状态, 原因)})
pub fn summary(nodes: &[NodeHealth]) -> HashMap<String, (String, Option<String>, BTreeMap<String, (String, Option<String>)>)> {
    nodes
        .iter()
        .map(|n| {
            (n.uuid.clone(), (n.status.clone(), n.reason.clone(), n.checks.iter().map(|c| (c.key.clone(), (c.status.clone(), c.reason.clone()))).collect()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_parse_truncates_to_micros() {
        let t = parse_iso("2026-09-23T07:22:14.881384622Z").unwrap();
        assert!((t - 1790148134.881384).abs() < 1e-6, "{t}");
        assert_eq!(parse_iso("1970-01-01T00:00:00+08:00"), Some(-28800.0));
    }

    #[test]
    fn py_equality() {
        assert!(py_eq(&json!([8388, "a"]), &json!([8388.0, "a"])));
        assert!(py_eq(&json!(1), &json!(true)));
        assert!(!py_eq(&json!(null), &json!("1.2.3.4")));
    }

    #[test]
    fn policy() {
        assert_eq!(policy_decision("www.cloudflare.com", &json!({"mode":"allowlist","domains":["cloudflare.com"]})), "routed");
        assert_eq!(policy_decision("www.cloudflare.com", &json!({"mode":"allowlist","domains":["=cloudflare.com"]})), "blocked");
        assert_eq!(policy_decision("www.cloudflare.com", &json!({"mode":"observe","domains":[]})), "routed");
    }
}
