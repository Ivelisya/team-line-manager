//! 旧 `teamops.monitor` 的写路径：diagnose()、Monitor.transition()/evaluate()/maintenance() 与
//! Monitor.__init__ 的建表与 meta 初始化（[`ensure`]）。
//!
//! 逐行移植自 release 20260922T081813Z-providers 的 monitor.py。写入的每一列（health_state、incidents、
//! health_samples、health_hours、audit、meta monitor / monitor_started:* / monitor_started_at /
//! availability_statistic_v2）的 JSON 文本与 int/float 类型都和 Python 相同；由本文件末尾的差分测试核对。
//!
//! ## 差分测试（旧 Python vs 本移植）怎么重跑
//!
//! 在 a395 上（真实库只在 a395，不要拷走）：
//!
//! ```sh
//! # Mac：同步到独立的构建目录（和 sync.sh 相同的布局，只是目标目录不同）
//! cd ~/Desktop/team-ops-v2/fleet && export COPYFILE_DISABLE=1 A=kismet@10.172.17.213 K=~/.ssh/a395_kismet
//! ssh -i $K $A 'mkdir -p ~/team-ops-eval/fleet-build-port/{src,fleet-edge,architecture/plan-catalog}'
//! tar --no-xattrs --exclude=target --exclude='*.local' --exclude=fixtures --exclude=dist -czf - . | ssh -i $K $A 'cd ~/team-ops-eval/fleet-build-port/src && tar -xzf -'
//! tar --no-xattrs -C ../fleet-edge --exclude=target -czf - fleetd-ingest | ssh -i $K $A 'cd ~/team-ops-eval/fleet-build-port/fleet-edge && tar -xzf -'
//! tar --no-xattrs -C ../architecture/plan-catalog -czf - catalog.json | ssh -i $K $A 'tar -xzf - -C ~/team-ops-eval/fleet-build-port/architecture/plan-catalog'
//! # a395：
//! source ~/team-ops-eval/tools/env.sh && cd ~/team-ops-eval/fleet-build-port/src
//! mkdir -p ../work && chmod 700 ../work
//! LEGACY_DIFF_WORK=$HOME/team-ops-eval/fleet-build-port/work \
//! LEGACY_DIFF_GOLDEN=$HOME/team-ops-eval/fleet-stage/golden \
//! LEGACY_DIFF_PY=$HOME/team-ops-eval/teamops/runtime/python/bin/python3.12 \
//! LEGACY_DIFF_RELEASE=$HOME/team-ops-eval/teamops-release \
//!   cargo test --release -p fleetd --lib compat::monitor_eval::diff -- --ignored --nocapture
//! ```
//!
//! 每个场景都从 golden 库 reflink 出两份副本（A 给 Python、B 给 Rust，放在 work/，700 权限），
//! 由测试生成同一串步骤：输入变更（relays/assets/meta 的 JSON 字段、SQL）、evaluate、maintenance、
//! 行为事件写入、detect、refresh……Python 端跑真实的旧代码（time.time() 钉在步骤给定的时刻，TZ=UTC
//! 与中控一致，start_times 按虚拟时钟打补丁，和 fleet-stage/vendor/stage_server.py 的做法相同），
//! Rust 端跑本移植；每步之后逐行比对受影响的表（值 + typeof，JSON 文本逐字节），最后全表比对。
//! 通过条件：`total diffs: 0`。不带 `--ignored` 时只跑不依赖私有库的单元测试。
//! 注意：tar 会带上 Mac 端的 mtime；若在 a395 上改过源文件再用 tar 覆盖回去，先 `touch` 一下，否则 cargo 不重编。
//! `LEGACY_DIFF_ONLY=monitor,behavior,init` 可只跑部分场景。
use super::py::*;
use super::pyjson::{dumps, py_round};
use super::{store, Ctx};
use rusqlite::types::Value as SqlVal;
use rusqlite::Connection;
use serde_json::{json, Map, Value};

const DEFAULTS: [(&str, i64); 4] = [("cpu_percent", 90), ("memory_percent", 90), ("disk_percent", 85), ("latency_ms", 3000)];
const EGRESS_ATTEMPTS: i64 = 3;
const WINDOW: usize = 6;
const UNSTABLE_FLIPS: usize = 3;

fn reason_of(rc: &Value) -> Option<&'static str> {
    // REASONS 的键是 int；dict 查找时 28.0 == 28，True == 1
    let x = match rc {
        Value::Number(_) => num(rc)?,
        Value::Bool(b) => *b as i64 as f64,
        _ => return None,
    };
    match x {
        x if x == 28.0 => Some("timeout"),
        x if x == 35.0 || x == 52.0 || x == 55.0 || x == 56.0 => Some("auth"),
        x if x == 7.0 || x == 97.0 => Some("upstream_down"),
        _ => None,
    }
}

fn thresholds(key: &str) -> (i64, i64) {
    match key {
        "ingress" | "egress" => (2, 1),
        "host" => (2, 2),
        _ => (1, 1),
    }
}

// ─── Python 语义的小工具 ─────────────────────────────────────────────────────────────────────────

/// Python `==`，递归：数字按数值（True == 1 == 1.0），dict 与键顺序无关
pub(crate) fn py_eq(a: &Value, b: &Value) -> bool {
    fn numeric(v: &Value) -> Option<(Option<i64>, f64)> {
        match v {
            Value::Bool(x) => Some((Some(*x as i64), *x as i64 as f64)),
            Value::Number(n) => Some((if n.is_f64() { None } else { n.as_i64() }, n.as_f64()?)),
            _ => None,
        }
    }
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(_) | Value::Bool(_), Value::Number(_) | Value::Bool(_)) => match (numeric(a), numeric(b)) {
            (Some((Some(x), _)), Some((Some(y), _))) => x == y,
            (Some((_, x)), Some((_, y))) => x == y,
            _ => false,
        },
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| py_eq(v, w))),
        _ => a == b,
    }
}

fn type_error(what: &str) -> Err {
    Err::Internal(format!("TypeError: {what}"))
}

/// 数值（含 bool）→ f64；None 或其它类型按 Python 比较时的 TypeError
fn number(v: &Value) -> R<f64> {
    match v {
        Value::Number(n) => Ok(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => Ok(*b as i64 as f64),
        _ => Err(type_error("'<' not supported between instances")),
    }
}

/// Python `a >= b`（数字）
fn ge(a: &Value, b: &Value) -> R<bool> {
    if let (Some(x), Some(y)) = (int_of(a), int_of(b)) {
        return Ok(x >= y);
    }
    Ok(number(a)? >= number(b)?)
}

/// `v if v is not None and v >= 0 else None`
fn nonneg(v: &Value) -> R<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    Ok(if ge(v, &i(0))? { v.clone() } else { Value::Null })
}

/// Python `float(x)`
fn py_float_of(v: &Value) -> R<f64> {
    match v {
        Value::String(s) => py_float(s).map_err(|e| match e {
            Err::Bad(m) => Err::Internal(format!("ValueError: {m}")),
            e => e,
        }),
        _ => number(v).map_err(|_| type_error("float() argument must be a string or a real number")),
    }
}

/// `datetime.fromisoformat(s)` 的解析结果：本地（墙钟）秒、微秒、UTC 偏移秒（无时区为 None）
fn fromisoformat(s: &str) -> Option<(i64, i64, Option<i64>)> {
    let b = s.as_bytes();
    let digits = |from: usize, n: usize| -> Option<i64> {
        let part = b.get(from..from + n)?;
        if !part.iter().all(|c| c.is_ascii_digit()) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    // 日期：YYYY-MM-DD 或 YYYYMMDD
    let (y, mo, d, mut pos) = if b.get(4) == Some(&b'-') {
        if b.get(7) != Some(&b'-') {
            return None;
        }
        (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?, 10)
    } else {
        (digits(0, 4)?, digits(4, 2)?, digits(6, 2)?, 8)
    };
    let date = chrono::NaiveDate::from_ymd_opt(y as i32, mo as u32, d as u32)?;
    let days = date.signed_duration_since(chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?).num_days();
    let (mut h, mut mi, mut sec, mut us) = (0i64, 0i64, 0i64, 0i64);
    let mut offset = None;
    if pos < b.len() {
        // 日期与时间之间的任意单个分隔符
        pos += 1;
        let t = &s[pos..];
        let tz_at = t.find(['+', '-']).unwrap_or(t.len());
        let (time, tz) = t.split_at(tz_at);
        let tb = time.as_bytes();
        let two = |bytes: &[u8], at: usize| -> Option<i64> {
            let part = bytes.get(at..at + 2)?;
            if !part.iter().all(|c| c.is_ascii_digit()) {
                return None;
            }
            Some(((part[0] - b'0') * 10 + (part[1] - b'0')) as i64)
        };
        h = two(tb, 0)?;
        let extended = tb.get(2) == Some(&b':');
        let mut p = 2;
        if p < tb.len() && tb[p] != b'.' && tb[p] != b',' {
            if extended {
                p += 1;
            }
            mi = two(tb, p)?;
            p += 2;
            if p < tb.len() && tb[p] != b'.' && tb[p] != b',' {
                if extended {
                    if tb[p] != b':' {
                        return None;
                    }
                    p += 1;
                }
                sec = two(tb, p)?;
                p += 2;
            }
        }
        if p < tb.len() {
            if tb[p] != b'.' && tb[p] != b',' {
                return None;
            }
            let frac = &tb[p + 1..];
            if frac.is_empty() || !frac.iter().all(|c| c.is_ascii_digit()) {
                return None;
            }
            // 取前 6 位，其余截断
            let mut v = 0i64;
            for k in 0..6 {
                v = v * 10 + frac.get(k).map(|c| (c - b'0') as i64).unwrap_or(0);
            }
            us = v;
        }
        if h > 23 || mi > 59 || sec > 59 {
            return None;
        }
        if !tz.is_empty() {
            let sign = if tz.starts_with('-') { -1 } else { 1 };
            let zb = &tz.as_bytes()[1..];
            let oh = two(zb, 0)?;
            let mut om = 0;
            let mut os = 0;
            let mut q = 2;
            if q < zb.len() {
                if zb[q] == b':' {
                    q += 1;
                }
                om = two(zb, q)?;
                q += 2;
                if q < zb.len() {
                    if zb[q] == b':' {
                        q += 1;
                    }
                    os = two(zb, q)?;
                    q += 2;
                }
                if q != zb.len() {
                    return None;
                }
            }
            if oh > 23 || om > 59 || os > 59 {
                return None;
            }
            offset = Some(sign * (oh * 3600 + om * 60 + os));
        }
    }
    Some((days * 86400 + h * 3600 + mi * 60 + sec, us, offset))
}

/// Python `timestamp(value)`：None/"" → int 0，数字 → float，ISO 文本 → float
/// （无时区按本地时间，即中控的 UTC；有时区时是整微秒数 / 10**6 的精确除法）
pub(crate) fn timestamp(v: &Value) -> R<Value> {
    match v {
        Value::Null => Ok(i(0)),
        Value::String(s) if s.is_empty() => Ok(i(0)),
        Value::Number(_) | Value::Bool(_) => Ok(f(number(v)?)),
        Value::String(s) => {
            let t = s.replace('Z', "+00:00");
            let (local, us, off) = fromisoformat(&t).ok_or_else(|| Err::Internal(format!("ValueError: Invalid isoformat string: '{t}'")))?;
            Ok(f(match off {
                Some(o) => ((local - o) * 1_000_000 + us) as f64 / 1e6,
                None => local as f64 + us as f64 / 1e6,
            }))
        }
        _ => Err(type_error("timestamp() of a container")),
    }
}

fn recent(v: &Value, now: f64, age: f64) -> R<bool> {
    let stamp = num(&timestamp(v)?).unwrap_or(0.0);
    Ok(stamp > 0.0 && -30.0 <= now - stamp && now - stamp <= age)
}

/// `round(100 * float(used) / float(total), 2) if used is not None and total else None`
fn percent(used: &Value, total: &Value) -> R<Value> {
    if used.is_null() || !truthy(total) {
        return Ok(Value::Null);
    }
    let (u, t) = (py_float_of(used)?, py_float_of(total)?);
    Ok(f(py_round(100.0 * u / t, 2)))
}

fn protocol_name(v: &Value) -> String {
    let s = match v {
        x if !truthy(x) => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(_) => "True".into(),
        Value::Number(n) if n.is_f64() => super::pyjson::float_repr(n.as_f64().unwrap_or(0.0)),
        Value::Number(n) => n.to_string(),
        other => dumps(other),
    };
    let s = s.to_lowercase();
    match s.as_str() {
        "socks5" => "socks".into(),
        "ss" => "shadowsocks".into(),
        _ => s,
    }
}

fn s_of<'a>(m: &'a Map<String, Value>, k: &str) -> &'a str {
    m.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

// ─── diagnose ────────────────────────────────────────────────────────────────────────────────────

struct Checks {
    now: f64,
    list: Vec<Value>,
}

impl Checks {
    #[allow(clippy::too_many_arguments)]
    fn add(&mut self, key: &str, title: &str, status: &str, reason: Value, at: &Value, critical: bool, value: Value, evidence: Value, pending: bool) -> R<()> {
        let reason = if status == "unknown" {
            json!("stale")
        } else if pending {
            json!("retrying")
        } else if status == "ok" {
            Value::Null
        } else {
            reason
        };
        let mut c = Map::new();
        c.insert("key".into(), json!(key));
        c.insert("title".into(), json!(title));
        c.insert("critical".into(), Value::Bool(critical));
        c.insert("status".into(), json!(status));
        c.insert("reason".into(), reason);
        c.insert("measured_at".into(), timestamp(at)?);
        c.insert("value".into(), value);
        c.insert("evidence".into(), evidence);
        c.insert("since".into(), f(self.now));
        c.insert("pending".into(), Value::Bool(pending));
        self.list.push(Value::Object(c));
        Ok(())
    }
}

fn deciding(checks: &[Value]) -> Option<usize> {
    for status in ["fail", "degraded", "unknown"] {
        for (k, c) in checks.iter().enumerate() {
            let key = get(c, "key").as_str().unwrap_or("");
            if get(c, "status").as_str() == Some(status) && if status != "degraded" { truthy(get(c, "critical")) } else { key != "tcp" && key != "latency" } {
                return Some(k);
            }
        }
    }
    None
}

fn obj_or_empty(v: &Value) -> Value {
    if truthy(v) {
        v.clone()
    } else {
        json!({})
    }
}

/// monitor.diagnose(asset, relays, now)
pub fn diagnose(asset: &Value, relays: &Value, now: f64) -> R<Value> {
    diagnose_with(asset, relays, now, None, true)
}

/// diagnose，可覆盖“实际入口到出口”的新鲜期（生产 240 s，见 CompatCfg.path_fresh_s）
pub fn diagnose_with(asset: &Value, relays: &Value, now: f64, path_fresh_s: Option<f64>, ip_check: bool) -> R<Value> {
    let mut checks = Checks { now, list: vec![] };
    let profile = get_or(asset, "profile", json!({}));
    let mut limits = Map::new();
    for (k, v) in DEFAULTS {
        limits.insert(k.into(), i(v));
    }
    match get_or(&profile, "thresholds", json!({})) {
        Value::Object(m) => {
            for (k, v) in m {
                limits.insert(k, v);
            }
        }
        _ => return Err(type_error("'thresholds' must be a mapping")),
    }
    let maintenance = num(&timestamp(get(&profile, "maintenance_until"))?).unwrap_or(0.0) > now;
    let name = get(asset, "name");
    let team = get(asset, "team").as_str().unwrap_or("");
    let relay_key = if truthy(get(asset, "relay")) { get(asset, "relay") } else { name };
    let relay = relay_key.as_str().and_then(|k| relays.get(k)).cloned().unwrap_or_else(|| json!({}));
    let relay_ok = truthy(get(&relay, "fresh"));
    let own_relay = !truthy(get(asset, "relay")) || py_eq(get(asset, "relay"), name);
    checks.add("collector", "线路管理通道", if relay_ok { "ok" } else if own_relay { "fail" } else { "unknown" }, json!("stale"),
        get(&relay, "received_at"), true, Value::Null, json!({"fresh": relay_ok}), false)?;
    let host = obj_or_empty(get(asset, "host"));
    let host_required = truthy(get(asset, "host_measured")) || get(asset, "provider") == "ATTVPS" || team == store::RELAY_TEAM;
    let host_ok = truthy(get(asset, "host_fresh")) && truthy(get(&host, "online"));
    let host_status = if !host_required {
        "na"
    } else if host_ok {
        "ok"
    } else if !truthy(get(asset, "host_fresh")) {
        "unknown"
    } else {
        "fail"
    };
    checks.add("host", "主机探针", host_status, if host_status == "fail" { json!("probe_offline") } else { Value::Null }, get(&host, "time"),
        team == store::RELAY_TEAM, Value::Null, json!({"online": get(&host, "online"), "time": get(&host, "time")}), false)?;
    let mut metrics = Map::new();
    metrics.insert("cpu".into(), if host_ok { get(&host, "cpu").clone() } else { Value::Null });
    metrics.insert("memory".into(), if host_ok { percent(get(&host, "ram"), get(&host, "ram_total"))? } else { Value::Null });
    metrics.insert("disk".into(), if host_ok { percent(get(&host, "disk"), get(&host, "disk_total"))? } else { Value::Null });
    for k in ["tcp_ms", "egress_ms", "path_ms"] {
        metrics.insert(k.into(), Value::Null);
    }
    for (key, title) in [("cpu", "CPU 容量"), ("memory", "内存容量"), ("disk", "磁盘容量")] {
        let value = nonneg(&metrics[key])?;
        metrics.insert(key.into(), value.clone());
        let limit = limits[&format!("{key}_percent")].clone();
        let status = if value.is_null() {
            "unknown"
        } else if ge(&value, &limit)? {
            "degraded"
        } else {
            "ok"
        };
        checks.add(key, title, status, Value::Null, get(&host, "time"), false, value, json!({"limit": limit}), false)?;
    }
    let probe = obj_or_empty(get(asset, "probe"));
    let path = obj_or_empty(get(asset, "path"));
    let channel = obj_or_empty(get(asset, "channel"));
    if truthy(get(asset, "relay_port")) {
        let at = get(&relay, "ts");
        checks.add("entry", "业务入口", if !relay_ok { "unknown" } else if truthy(get(&channel, "enable")) { "ok" } else { "fail" }, Value::Null, at, true,
            Value::Null, json!({"enable": get(&channel, "enable")}), false)?;
        let expected = json!([get(asset, "outbound_tag"), get(asset, "target"), get(asset, "target_port"), protocol_name(get(asset, "protocol"))]);
        let actual = json!([get(&channel, "outbound_tag"), get(&channel, "target"), get(&channel, "target_port"), protocol_name(get(&channel, "protocol"))]);
        let same = py_eq(&expected, &actual);
        checks.add("mapping", "固定出口绑定", if !relay_ok { "unknown" } else if same { "ok" } else { "fail" }, Value::Null, at, true, Value::Null,
            json!({"expected": expected, "actual": actual}), false)?;
        let tcp_ms = nonneg(get(&probe, "tcp_ms"))?;
        let egress_ms = nonneg(get(&probe, "egress_ms"))?;
        let path_ms = nonneg(get(&path, "egress_ms"))?;
        let tcp_fresh = recent(get(&probe, "tcp_at"), now, store::fresh_after(30.0))?;
        let tcp_failed = truthy(get(&probe, "tcp_error")) || (!get(&probe, "tcp_ms").is_null() && !ge(get(&probe, "tcp_ms"), &i(0))?);
        let tcp_status = if probe.get("tcp_ms").is_none() {
            "na"
        } else if !tcp_fresh {
            "unknown"
        } else if !tcp_ms.is_null() && !tcp_failed {
            "ok"
        } else {
            "fail"
        };
        checks.add("tcp", "线路到上游 TCP", tcp_status, if tcp_status == "fail" { json!("upstream_down") } else { Value::Null }, get(&probe, "tcp_at"), false,
            tcp_ms.clone(), json!({"rc": get(&probe, "tcp_rc"), "phase": get(&probe, "tcp_phase"), "error": get(&probe, "tcp_error")}), false)?;
        let egress_fresh = recent(get(&probe, "egress_at"), now, store::fresh_after(300.0))?;
        let ip = get(asset, "ip");
        // ip_check = false：取到出口 IP 就算通过（只看连通性），不与资产 IP 比对
        let ip_ok = if ip_check { py_eq(get(&probe, "actual_ip"), ip) } else { truthy(get(&probe, "actual_ip")) };
        let mismatch = ip_check && truthy(get(&probe, "actual_ip")) && !ip_ok;
        let egress_failed = !ip_ok || truthy(get(&probe, "egress_error"));
        let attempt = get(&probe, "egress_attempt");
        let is_int = matches!(attempt, Value::Bool(_)) || matches!(attempt, Value::Number(n) if !n.is_f64());
        let egress_pending = egress_fresh && egress_failed && !mismatch && is_int && !ge(attempt, &i(EGRESS_ATTEMPTS))?;
        let no_probe = get(asset, "probe").is_null();
        let egress_status = if no_probe {
            "na"
        } else if !egress_fresh {
            "unknown"
        } else if !egress_failed || egress_pending {
            "ok"
        } else {
            "fail"
        };
        let egress_reason = if mismatch { "egress_mismatch" } else { reason_of(get(&probe, "egress_rc")).unwrap_or("unknown") };
        checks.add("egress", "认证出口验证", egress_status, if egress_status == "fail" { json!(egress_reason) } else { Value::Null }, get(&probe, "egress_at"), true,
            egress_ms.clone(),
            json!({"rc": get(&probe, "egress_rc"), "phase": get(&probe, "egress_phase"), "ms": egress_ms, "attempt": attempt,
                "actual_ip": get(&probe, "actual_ip"), "expected_ip": ip, "error": get(&probe, "egress_error")}),
            egress_pending)?;
        let path_at = get_or(&path, "measured_at", get_or(&path, "checked_at", get(&path, "egress_at").clone()));
        let path_fresh = recent(&path_at, now, path_fresh_s.unwrap_or_else(|| store::fresh_after(60.0)))?;
        let path_ok = truthy(get(&path, "healthy")) && (!ip_check || py_eq(get(&path, "actual_ip"), ip));
        let path_status = if !path_fresh {
            "unknown"
        } else if path_ok {
            "ok"
        } else if super::policy::decision("www.cloudflare.com", get(asset, "policy")) == "blocked" {
            "degraded"
        } else {
            "fail"
        };
        let mut path_reason = if path_status == "degraded" { "policy_blocked" } else { reason_of(get(&path, "rc")).unwrap_or("unknown") };
        if tcp_failed && (path_status == "degraded" || path_status == "fail") {
            path_reason = "upstream_down";
        }
        checks.add("ingress", "实际入口到出口", path_status, json!(path_reason), &path_at, true, path_ms.clone(),
            json!({"rc": get(&path, "rc"), "phase": get(&path, "phase"), "ms": path_ms, "actual_ip": get(&path, "actual_ip"), "expected_ip": ip,
                "attempts": get(&path, "attempts")}),
            false)?;
        let latency = if egress_fresh { egress_ms } else { Value::Null };
        let limit = limits["latency_ms"].clone();
        let latency_status = if no_probe {
            "na"
        } else if latency.is_null() {
            "unknown"
        } else if ge(&latency, &limit)? {
            "degraded"
        } else {
            "ok"
        };
        checks.add("latency", "出口请求耗时", latency_status, Value::Null, get(&probe, "egress_at"), false, latency.clone(), json!({"limit": limit}), false)?;
        metrics.insert("tcp_ms".into(), if tcp_fresh { tcp_ms } else { Value::Null });
        metrics.insert("egress_ms".into(), latency);
        metrics.insert("path_ms".into(), if path_fresh { path_ms } else { Value::Null });
        metrics.insert("egress_pending".into(), Value::Bool(egress_pending));
    }
    let d = deciding(&checks.list);
    let status = d.map(|k| checks.list[k]["status"].as_str().unwrap_or("").to_string()).unwrap_or_else(|| "ok".into());
    let reason = d.map(|k| checks.list[k]["reason"].clone()).unwrap_or(Value::Null);
    let fresh = truthy(get(&channel, "fresh"));
    let pick = |k: &str| if fresh { get(&channel, k).clone() } else { Value::Null };
    let mut out = Map::new();
    out.insert("uuid".into(), get(asset, "uuid").clone());
    out.insert("name".into(), name.clone());
    out.insert("team".into(), get(asset, "team").clone());
    out.insert("relay".into(), get(asset, "relay").clone());
    out.insert("status".into(), json!(if maintenance { "maintenance" } else { status.as_str() }));
    out.insert("reason".into(), reason);
    out.insert("since".into(), f(now));
    out.insert("availability".into(), availability(&status));
    out.insert("checks".into(), Value::Array(checks.list));
    out.insert("metrics".into(), Value::Object(metrics));
    out.insert("limits".into(), Value::Object(limits));
    out.insert("maintenance".into(), Value::Bool(maintenance));
    out.insert("checked_at".into(), f(now));
    out.insert("rate_up".into(), pick("rate_up"));
    out.insert("rate_down".into(), pick("rate_down"));
    out.insert("connections".into(), pick("tcp_connections"));
    Ok(Value::Object(out))
}

fn availability(status: &str) -> Value {
    match status {
        "fail" => i(0),
        "unknown" => Value::Null,
        _ => i(1),
    }
}

// ─── 写库小工具（behavior_write 也用） ────────────────────────────────────────────────────────────

/// JSON 值 → SQLite 绑定值（与 sqlite3 模块对 None/bool/int/float/str 的适配相同）
pub(crate) fn to_sql(v: &Value) -> R<SqlVal> {
    Ok(match v {
        Value::Null => SqlVal::Null,
        Value::Bool(b) => SqlVal::Integer(*b as i64),
        Value::Number(n) if n.is_f64() => SqlVal::Real(n.as_f64().unwrap_or(0.0)),
        Value::Number(n) => SqlVal::Integer(n.as_i64().ok_or_else(|| Err::Internal("OverflowError: Python int too large to convert to SQLite INTEGER".into()))?),
        Value::String(s) => SqlVal::Text(s.clone()),
        _ => return Err(Err::Internal("ProgrammingError: Error binding parameter - type 'dict'/'list' is not supported".into())),
    })
}

pub(crate) fn exec(db: &Connection, sql: &str, args: &[SqlVal]) -> R<usize> {
    Ok(db.prepare_cached(sql)?.execute(rusqlite::params_from_iter(args.iter()))?)
}

/// store.meta(key, value)
pub(crate) fn set_meta(db: &Connection, key: &str, value: &Value) -> R<()> {
    exec(db, "INSERT INTO meta VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", &[st(key), st(&dumps(value))])?;
    Ok(())
}

/// store.audit(actor, action, target, detail)；ts = time.time() = cx.now
pub(crate) fn audit(cx: &Ctx, actor: &str, action: &str, target: &str, detail: &Value) -> R<()> {
    exec(cx.db, "INSERT INTO audit(ts,actor,action,target,detail) VALUES(?,?,?,?,?)",
        &[sf(cx.now), st(&head(actor, 100)), st(action), st(&head(target, 150)), st(&dumps(detail))])?;
    Ok(())
}

/// `with store.db:` —— 已在事务里（调用方自己开的）就并入该事务，否则开一个，出错回滚
pub(crate) fn in_tx<T>(cx: &Ctx, body: impl FnOnce() -> R<T>) -> R<T> {
    if !cx.db.is_autocommit() {
        return body();
    }
    let tx = cx.db.unchecked_transaction()?;
    let out = body()?;
    tx.commit()?;
    Ok(out)
}

// ─── Monitor ─────────────────────────────────────────────────────────────────────────────────────

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS health_state(asset TEXT,key TEXT,data TEXT,PRIMARY KEY(asset,key));
CREATE TABLE IF NOT EXISTS incidents(id INTEGER PRIMARY KEY,asset TEXT,key TEXT,title TEXT,severity TEXT,opened_at REAL,last_seen REAL,resolved_at REAL,ack_at REAL,ack_actor TEXT,note TEXT,evidence TEXT);
CREATE INDEX IF NOT EXISTS incidents_asset ON incidents(asset,opened_at);
CREATE UNIQUE INDEX IF NOT EXISTS incident_active ON incidents(asset,key) WHERE resolved_at IS NULL;
CREATE TABLE IF NOT EXISTS health_samples(asset TEXT,bucket INTEGER,ok INTEGER,unknown INTEGER,failed INTEGER,maintenance INTEGER,data TEXT,PRIMARY KEY(asset,bucket));
CREATE TABLE IF NOT EXISTS health_hours(asset TEXT,bucket INTEGER,ok INTEGER,unknown INTEGER,failed INTEGER,maintenance INTEGER,PRIMARY KEY(asset,bucket));
";

/// Monitor.__init__：建表；monitor_started_at / availability_statistic_v2 缺失（或为假值）时写入 cx.now
pub fn ensure(cx: &Ctx) -> R<()> {
    cx.db.execute_batch(SCHEMA)?;
    in_tx(cx, || {
        if !truthy(&meta(cx.db, "monitor_started_at", Value::Null)?) {
            set_meta(cx.db, "monitor_started_at", &f(cx.now))?;
        }
        if !truthy(&meta(cx.db, "availability_statistic_v2", Value::Null)?) {
            set_meta(cx.db, "availability_statistic_v2", &json!({"effective_at": f(cx.now), "version": 2}))?;
        }
        Ok(())
    })
}

/// Monitor.evaluate(overview, now=cx.now)。overview 为 None 时现算 `store.overview()`。
pub fn evaluate(cx: &Ctx, overview: Option<Value>) -> R<()> {
    let now = cx.now;
    let ov = match overview {
        Some(v) => v,
        None => store::overview(cx)?,
    };
    let relays = get(&ov, "relays");
    let mut nodes = vec![];
    for a in get(&ov, "assets").as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
        nodes.push(diagnose_with(a, relays, now, cx.cfg.path_fresh_s, cx.cfg.egress_ip_check)?);
    }
    in_tx(cx, || evaluate_tx(cx, nodes, now))
}

fn evaluate_tx(cx: &Ctx, mut nodes: Vec<Value>, now: f64) -> R<()> {
    let db = cx.db;
    // 上一轮的报告；本轮只在最后才写 meta monitor，所以循环内读到的都是同一份
    let report = meta(db, "monitor", json!({}))?;
    let last_nodes = get_or(&report, "nodes", json!({}));
    for node in nodes.iter_mut() {
        let uuid = get(node, "uuid").as_str().unwrap_or("").to_string();
        let started_key = format!("monitor_started:{uuid}");
        if !truthy(&meta(db, &started_key, Value::Null)?) {
            set_meta(db, &started_key, &f(now))?;
        }
        let in_maintenance = truthy(get(node, "maintenance"));
        let mut checks: Vec<Value> = node["checks"].as_array().cloned().unwrap_or_default();
        for c in checks.iter_mut() {
            transition(cx, &uuid, in_maintenance, c.as_object_mut().unwrap(), now, &last_nodes)?;
        }
        let d = deciding(&checks);
        let status = d.map(|k| checks[k]["status"].as_str().unwrap_or("").to_string()).unwrap_or_else(|| "ok".into());
        let n = node.as_object_mut().unwrap();
        n.insert("reason".into(), d.map(|k| checks[k]["reason"].clone()).unwrap_or(Value::Null));
        n.insert("availability".into(), availability(&status));
        n.insert("status".into(), json!(if in_maintenance { "maintenance" } else { status.as_str() }));
        let previous = match scalar(db, "SELECT data FROM health_state WHERE asset=? AND key='status'", &[st(&uuid)])? {
            Value::Null => None,
            v => Some(loads(&v)?),
        };
        let prev_truthy = previous.as_ref().is_some_and(truthy);
        let since = match &previous {
            Some(p) if prev_truthy && (in_maintenance || get(p, "status") == status.as_str()) => get(p, "since").clone(),
            _ => f(now),
        };
        n.insert("since".into(), since);
        let last_report = get(&last_nodes, &uuid);
        if in_maintenance || (truthy(last_report) && truthy(get(last_report, "maintenance"))) {
            let s = if truthy(last_report) && py_eq(get(last_report, "status"), &n["status"]) { get(last_report, "since").clone() } else { f(now) };
            n.insert("since".into(), s);
        }
        if !in_maintenance {
            if let Some(p) = previous.as_ref().filter(|_| prev_truthy) {
                let ps = get(p, "status").as_str().unwrap_or("");
                let both = |x: &str| x == "ok" || x == "degraded" || x == "fail";
                if !py_eq(get(p, "status"), &json!(status)) && get(p, "status").is_string() && both(ps) && both(&status) {
                    let evidence = d.map(|k| checks[k]["evidence"].clone()).unwrap_or_else(|| json!({}));
                    audit(cx, "system", "health_flip", &uuid,
                        &json!({"check": "status", "from": ps, "to": status, "reason": n["reason"], "evidence": evidence}))?;
                }
            }
            let state = json!({"status": status, "fails": 0, "oks": 0, "last": f(now), "since": n["since"]});
            exec(db, "INSERT INTO health_state VALUES(?,'status',?) ON CONFLICT(asset,key) DO UPDATE SET data=excluded.data", &[st(&uuid), st(&dumps(&state))])?;
            let keys: Vec<&str> = checks.iter().map(|c| get(c, "key").as_str().unwrap_or("")).collect();
            for r in rows(db, "SELECT id,key FROM incidents WHERE asset=? AND resolved_at IS NULL", &[st(&uuid)])? {
                if !keys.iter().any(|k| r["key"].as_str() == Some(*k)) {
                    exec(db, "UPDATE incidents SET resolved_at=?,last_seen=? WHERE id=?", &[sf(now), sf(now), to_sql(&r["id"])?])?;
                    audit(cx, "system", "incident_resolved", &uuid, &json!({"check": r["key"], "incident": r["id"]}))?;
                }
            }
        }
        n.insert("checks".into(), Value::Array(checks));
        let bucket = (now / 60.0).floor() as i64 * 60;
        let avail = n["availability"].clone();
        let counts: [i64; 4] = if in_maintenance {
            [0, 0, 0, 1]
        } else {
            [(avail == i(1)) as i64, avail.is_null() as i64, (avail == i(0)) as i64, 0]
        };
        let inserted = exec(db, "INSERT OR IGNORE INTO health_samples VALUES(?,?,?,?,?,?,?)",
            &[st(&uuid), si(bucket), si(counts[0]), si(counts[1]), si(counts[2]), si(counts[3]), st(&dumps(&n["metrics"]))])?;
        if inserted > 0 {
            exec(db, "INSERT INTO health_hours VALUES(?,?,?,?,?,?) ON CONFLICT(asset,bucket) DO UPDATE SET ok=ok+excluded.ok,unknown=unknown+excluded.unknown,failed=failed+excluded.failed,maintenance=maintenance+excluded.maintenance",
                &[st(&uuid), si((now / 3600.0).floor() as i64 * 3600), si(counts[0]), si(counts[1]), si(counts[2]), si(counts[3])])?;
        }
    }
    let mut attention: std::collections::BTreeSet<String> = rows(db, "SELECT DISTINCT asset FROM incidents WHERE resolved_at IS NULL", &[])?
        .into_iter()
        .filter_map(|r| r["asset"].as_str().map(String::from))
        .collect();
    for n in &nodes {
        if get(n, "status") == "degraded" && num(get(n, "since")).unwrap_or(0.0) <= now - 300.0 {
            attention.insert(get(n, "uuid").as_str().unwrap_or("").to_string());
        }
    }
    for n in &nodes {
        if truthy(get(n, "maintenance")) {
            attention.remove(get(n, "uuid").as_str().unwrap_or(""));
        }
    }
    let day = (now / 86400.0).floor() as i64;
    for node in nodes.iter_mut() {
        let uuid = get(node, "uuid").as_str().unwrap_or("").to_string();
        let args = [st(&uuid), si((day - 29) * 86400), si((day + 1) * 86400)];
        let mut coverage: Vec<(f64, Value)> = vec![];
        for r in rows(db, "SELECT bucket/86400,SUM(ok),SUM(failed) FROM health_hours WHERE asset=? AND bucket>=? AND bucket<? GROUP BY bucket/86400", &args)? {
            let vals: Vec<Value> = r.into_iter().map(|(_, v)| v).collect();
            let (ok, failed) = (&vals[1], &vals[2]);
            let total = add(ok, failed);
            let pct = if truthy(&total) {
                let numer = match int_of(ok) {
                    Some(x) => (x * 100) as f64,
                    None => number(ok)? * 100.0,
                };
                f(py_round(numer / number(&total)?, 2))
            } else {
                Value::Null
            };
            let key = num(&vals[0]).unwrap_or(f64::NAN);
            match coverage.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = pct,
                None => coverage.push((key, pct)),
            }
        }
        let mut incidents: Vec<(f64, Value)> = vec![];
        for r in rows(db, "SELECT CAST(opened_at/86400 AS INTEGER),COUNT(*) FROM incidents WHERE asset=? AND opened_at>=? AND opened_at<? GROUP BY CAST(opened_at/86400 AS INTEGER)", &args)? {
            let vals: Vec<Value> = r.into_iter().map(|(_, v)| v).collect();
            incidents.push((num(&vals[0]).unwrap_or(f64::NAN), vals[1].clone()));
        }
        let windows = super::monitor::availability_windows(cx, &uuid, &[30])?;
        let days: Vec<Value> = ((day - 29)..=day)
            .map(|d| {
                let date = chrono::DateTime::from_timestamp(d * 86400, 0).map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default();
                let cov = coverage.iter().find(|(k, _)| *k == d as f64).map(|(_, v)| v.clone()).unwrap_or(Value::Null);
                let inc = incidents.iter().find(|(k, _)| *k == d as f64).map(|(_, v)| v.clone()).unwrap_or(i(0));
                json!({"date": date, "coverage": cov, "incidents": inc})
            })
            .collect();
        let n = node.as_object_mut().unwrap();
        n.insert("windows".into(), windows);
        n.insert("days".into(), Value::Array(days));
    }
    let mut by_uuid = Map::new();
    for n in &nodes {
        by_uuid.insert(get(n, "uuid").as_str().unwrap_or("").to_string(), n.clone());
    }
    let mut counts = Map::new();
    for s in ["ok", "degraded", "fail", "unknown", "maintenance"] {
        counts.insert(s.into(), i(nodes.iter().filter(|n| get(n, "status") == s).count() as i64));
    }
    let mut unknown: Vec<String> = nodes.iter().filter(|n| get(n, "status") == "unknown").map(|n| get(n, "uuid").as_str().unwrap_or("").to_string()).collect();
    unknown.sort();
    set_meta(db, "monitor", &json!({"checked_at": f(now), "nodes": by_uuid, "counts": counts,
        "attention": attention.into_iter().collect::<Vec<_>>(), "unknown": unknown}))
}

/// Monitor.transition(node, check, now)
fn transition(cx: &Ctx, uuid: &str, in_maintenance: bool, check: &mut Map<String, Value>, now: f64, last_nodes: &Value) -> R<()> {
    let db = cx.db;
    let key = s_of(check, "key").to_string();
    let row = scalar(db, "SELECT data FROM health_state WHERE asset=? AND key=?", &[st(uuid), st(&key)])?;
    let has_row = !row.is_null();
    let mut state = if has_row {
        match loads(&row)? {
            Value::Object(m) => m,
            _ => return Err(type_error("health_state data is not a dict")),
        }
    } else {
        json!({"status": "ok", "fails": 0, "oks": 0, "last": null, "since": f(now)}).as_object().cloned().unwrap()
    };
    check.insert("since".into(), state.get("since").cloned().unwrap_or(Value::Null));
    if in_maintenance {
        // get_meta("monitor", {}).get("nodes", {}).get(uuid, {}).get("checks", [])
        let report = get_or(last_nodes, uuid, json!({}));
        let last = get_or(&report, "checks", json!([])).as_array().and_then(|cs| cs.iter().find(|c| get(c, "key").as_str() == Some(&key)).cloned());
        let effective = if has_row { Some(Value::Object(state.clone())) } else { last };
        let status = if s_of(check, "status") == "unknown" || effective.is_none() {
            json!("unknown")
        } else {
            get(effective.as_ref().unwrap(), "status").clone()
        };
        check.insert("status".into(), status);
        check.insert("since".into(), effective.as_ref().map(|e| get(e, "since").clone()).unwrap_or_else(|| f(now)));
        if check["status"] == "unknown" {
            check.insert("reason".into(), json!("stale"));
        } else if check["status"] == "ok" {
            check.insert("reason".into(), Value::Null);
        }
        return Ok(());
    }
    if key == "tcp" || key == "latency" || check["status"] == "na" {
        return Ok(());
    }
    let previous = state.get("status").cloned().unwrap_or(Value::Null);
    let measurement = if matches!(key.as_str(), "collector" | "entry" | "mapping") {
        json!([check["status"], check["evidence"]])
    } else {
        check["measured_at"].clone()
    };
    let pending = truthy(check.get("pending").unwrap_or(&Value::Null));
    let last = state.get("last").cloned().unwrap_or(Value::Null);
    if check["status"] != "unknown" && !pending && !py_eq(&measurement, &last) {
        let (fail_after, recover_after) = thresholds(&key);
        let hist_src = state.get("history").filter(|h| truthy(h)).cloned().unwrap_or_else(|| json!([]));
        let hist_all = hist_src.as_array().cloned().unwrap_or_default();
        let mut history: Vec<Value> = hist_all[hist_all.len().saturating_sub(WINDOW - 1)..].to_vec();
        if check["status"] == "ok" {
            let oks = add(state.get("oks").unwrap_or(&Value::Null), &i(1));
            state.insert("oks".into(), oks.clone());
            state.insert("fails".into(), i(0));
            if ge(&oks, &i(recover_after))? {
                state.insert("status".into(), json!("ok"));
            } else if state.get("status").is_some_and(|s| s == "fail") {
                state.insert("status".into(), json!("degraded"));
            }
            history.push(i(1));
        } else {
            let fails = add(state.get("fails").unwrap_or(&Value::Null), &i(1));
            state.insert("fails".into(), fails.clone());
            state.insert("oks".into(), i(0));
            let s = if check["status"] == "fail" && ge(&fails, &i(fail_after))? { "fail" } else { "degraded" };
            state.insert("status".into(), json!(s));
            state.insert("reason".into(), check["reason"].clone());
            history.push(i(0));
        }
        if key == "egress" || key == "ingress" {
            let flips = history.windows(2).filter(|w| !py_eq(&w[0], &w[1])).count();
            state.insert("history".into(), Value::Array(history));
            let unstable = state.get("status").is_some_and(|s| s == "ok") && flips >= UNSTABLE_FLIPS;
            state.insert("unstable".into(), Value::Bool(unstable));
            if unstable {
                state.insert("status".into(), json!("degraded"));
                state.insert("reason".into(), json!("unstable"));
            }
        }
        if !py_eq(state.get("status").unwrap_or(&Value::Null), &previous) {
            state.insert("since".into(), f(now));
        }
        state.insert("last".into(), measurement);
        exec(db, "INSERT INTO health_state VALUES(?,?,?) ON CONFLICT(asset,key) DO UPDATE SET data=excluded.data",
            &[st(uuid), st(&key), st(&dumps(&Value::Object(state.clone())))])?;
    }
    if check["status"] != "unknown" {
        check.insert("status".into(), state.get("status").cloned().unwrap_or(Value::Null));
        check.insert("since".into(), state.get("since").cloned().unwrap_or(Value::Null));
        let r = &check["reason"];
        if check["status"] != "ok" && (r.is_null() || r == "retrying") && truthy(state.get("reason").unwrap_or(&Value::Null)) {
            check.insert("reason".into(), state["reason"].clone());
        }
    }
    if check["status"] == "ok" && !pending {
        check.insert("reason".into(), Value::Null);
    }
    let critical = truthy(&check["critical"]);
    if !py_eq(&previous, &check["status"]) && check["status"] != "unknown" && critical {
        audit(cx, "system", "health_flip", uuid,
            &json!({"check": key, "from": previous, "to": check["status"], "reason": check["reason"], "evidence": check["evidence"]}))?;
    }
    let incident = scalar(db, "SELECT id FROM incidents WHERE asset=? AND key=? AND resolved_at IS NULL", &[st(uuid), st(&key)])?;
    let severity = if critical { "critical" } else { "warning" };
    let mut evidence = Map::new();
    evidence.insert("check".into(), json!(key));
    for k in ["status", "reason", "measured_at", "value", "evidence"] {
        evidence.insert(k.into(), check[k].clone());
    }
    let evidence = Value::Object(evidence);
    let status = check["status"].clone();
    let opens = (critical && status == "fail")
        || (matches!(key.as_str(), "cpu" | "memory" | "disk") && status == "degraded" && now - number(&check["since"])? >= 120.0);
    if !incident.is_null() && status == "ok" {
        exec(db, "UPDATE incidents SET resolved_at=?,last_seen=? WHERE id=?", &[sf(now), sf(now), to_sql(&incident)?])?;
        audit(cx, "system", "incident_resolved", uuid, &json!({"check": key, "incident": incident}))?;
    } else if !incident.is_null() {
        exec(db, "UPDATE incidents SET last_seen=?,severity=?,evidence=? WHERE id=?", &[sf(now), st(severity), st(&dumps(&evidence)), to_sql(&incident)?])?;
    } else if opens {
        exec(db, "INSERT INTO incidents(asset,key,title,severity,opened_at,last_seen,note,evidence) VALUES(?,?,?,?,?,?,?,?)",
            &[st(uuid), st(&key), to_sql(&check["title"])?, st(severity), sf(now), sf(now), st(""), st(&dumps(&evidence))])?;
        audit(cx, "system", "incident_open", uuid, &json!({"check": key, "severity": severity, "evidence": evidence}))?;
    }
    Ok(())
}

/// Monitor.maintenance()：按 cx.now 清理过期样本与已解决故障
pub fn maintenance(cx: &Ctx) -> R<()> {
    let now = cx.now;
    in_tx(cx, || {
        exec(cx.db, "DELETE FROM health_samples WHERE bucket<?", &[sf(now - 14.0 * 86400.0)])?;
        exec(cx.db, "DELETE FROM health_hours WHERE bucket<?", &[sf(now - 400.0 * 86400.0)])?;
        exec(cx.db, "DELETE FROM incidents WHERE resolved_at<?", &[sf(now - 365.0 * 86400.0)])?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::CompatCfg;

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(&Value::Null).unwrap(), i(0));
        assert_eq!(timestamp(&json!("")).unwrap(), i(0));
        assert!(timestamp(&json!(5)).unwrap().is_f64());
        // 9 位小数截断到微秒；有时区时按整微秒精确除法
        assert_eq!(timestamp(&json!("2026-09-23T07:22:19.324360935Z")).unwrap(), f(1790148139.32436));
        assert_eq!(timestamp(&json!("2026-09-23 07:22:19+08:00")).unwrap(), f(1790119339.0));
        assert_eq!(timestamp(&json!("20260923T072219Z")).unwrap(), f(1790148139.0));
        assert_eq!(timestamp(&json!("2026-09-23")).unwrap(), f(1790121600.0));
        assert!(timestamp(&json!("yesterday")).is_err());
    }

    #[test]
    fn python_equality() {
        assert!(py_eq(&json!(["ok", {"fresh": true, "n": 1}]), &json!(["ok", {"n": 1.0, "fresh": 1}])));
        assert!(!py_eq(&json!(["ok", {"fresh": true}]), &json!(1790148134.881384)));
        assert!(!py_eq(&json!({"a": 1}), &json!({"a": 1, "b": null})));
        assert_eq!(percent(&json!(173133824), &json!(2756128768u64)).unwrap(), f(6.28));
        assert_eq!(percent(&json!(1), &json!(0)).unwrap(), Value::Null);
        assert_eq!(protocol_name(&json!("SS")), "shadowsocks");
    }

    fn asset(now: f64, probe: Value) -> Value {
        json!({"uuid": "u1", "name": "n1", "team": "团队1", "relay": "r1", "relay_port": 20002, "ip": "1.2.3.4", "outbound_tag": "t", "target": "x",
            "target_port": 1, "protocol": "Shadowsocks", "provider": "X", "policy": {"mode": "observe", "domains": []}, "host": null, "host_fresh": false,
            "channel": {"enable": 1, "outbound_tag": "t", "target": "x", "target_port": 1, "protocol": "ss", "fresh": true, "rate_up": 1.5, "rate_down": 2.0, "tcp_connections": 3},
            "probe": probe, "path": {"healthy": true, "measured_at": now - 5.0, "actual_ip": "1.2.3.4", "egress_ms": 50.5, "rc": 0}})
    }

    #[test]
    fn evaluate_flow() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE audit(id INTEGER PRIMARY KEY,ts REAL,actor TEXT,action TEXT,target TEXT,detail TEXT);").unwrap();
        let cfg = CompatCfg::default();
        let t0 = 1790148138.5;
        ensure(&Ctx { db: &db, now: t0, cfg: &cfg }).unwrap();
        let run = |k: i64, err: Option<(&str, i64)>| {
            let now = t0 + 10.0 * k as f64;
            let mut probe = json!({"tcp_ms": 5, "tcp_at": now - 5.0, "egress_ms": 100, "egress_at": now - 5.0, "actual_ip": "1.2.3.4", "egress_rc": 0, "egress_attempt": 1});
            if let Some((e, attempt)) = err {
                probe["egress_error"] = json!(e);
                probe["egress_rc"] = json!(28);
                probe["egress_attempt"] = json!(attempt);
            }
            let ov = json!({"assets": [asset(now, probe)], "relays": {"r1": {"fresh": true, "received_at": now - 1.0, "ts": now - 2.0}}});
            evaluate(&Ctx { db: &db, now, cfg: &cfg }, Some(ov)).unwrap();
            meta(&db, "monitor", json!({})).unwrap()
        };
        let m = run(0, None);
        assert_eq!(m["counts"]["ok"], json!(1));
        assert_eq!(m["nodes"]["u1"]["days"].as_array().unwrap().len(), 30);
        // 重试中（非最后一次尝试）仍是 ok，原因写 retrying
        let m = run(1, Some(("timeout", 1)));
        let egress = m["nodes"]["u1"]["checks"].as_array().unwrap().iter().find(|c| c["key"] == "egress").unwrap().clone();
        assert_eq!((egress["status"].as_str(), egress["reason"].as_str()), (Some("ok"), Some("retrying")));
        let m = run(2, Some(("timeout", 3)));
        assert_eq!(m["nodes"]["u1"]["status"], "degraded");
        let m = run(3, Some(("timeout", 3)));
        assert_eq!(m["nodes"]["u1"]["status"], "fail");
        assert_eq!(m["nodes"]["u1"]["reason"], "timeout");
        assert_eq!(scalar(&db, "SELECT count(*) FROM incidents WHERE resolved_at IS NULL", &[]).unwrap(), json!(1));
        run(4, None);
        assert_eq!(scalar(&db, "SELECT count(*) FROM incidents WHERE resolved_at IS NULL", &[]).unwrap(), json!(0));
        let actions: Vec<String> = db.prepare("SELECT action FROM audit ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().flatten().collect();
        // 检查级翻转 → 节点级翻转；第二次失败时先检查级翻转再开故障，最后节点级
        assert_eq!(actions, ["health_flip", "health_flip", "health_flip", "incident_open", "health_flip", "health_flip", "incident_resolved", "health_flip"]);
        let st: String = db.query_row("SELECT data FROM health_state WHERE asset='u1' AND key='egress'", [], |r| r.get(0)).unwrap();
        assert!(st.starts_with("{\"status\":\"ok\",\"fails\":0,\"oks\":1,\"last\":1790148173.5,"), "{st}");
        maintenance(&Ctx { db: &db, now: t0 + 15.0 * 86400.0, cfg: &cfg }).unwrap();
        assert_eq!(scalar(&db, "SELECT count(*) FROM health_samples", &[]).unwrap(), json!(0));
    }
}

/// 旧 Python（真实代码、真实库副本）与本移植的差分测试；用法见文件头。
#[cfg(test)]
mod diff {
    use super::*;
    use crate::compat::{behavior_write, CompatCfg};
    use rusqlite::types::ValueRef;
    use rusqlite::OpenFlags;
    use std::io::{BufRead, BufReader, Write};
    use std::path::{Path, PathBuf};
    use std::process::{ChildStdin, ChildStdout, Command, Stdio};

    /// Python 端：加载真实 teamops（release），time.time() 钉在每步给定的时刻，逐行读步骤执行。
    const DRIVER: &str = r##"
import sys, json, types, time as _time, datetime as dt, traceback
from zoneinfo import ZoneInfo
release, db, as_of = sys.argv[1], sys.argv[2], float(sys.argv[3])
sys.path.insert(0, release)
import teamops.store as tstore, teamops.monitor as tmon, teamops.behavior as tbeh

class Clock:
    t = as_of

class Shim(types.ModuleType):
    def __init__(self):
        super().__init__("time")
    def time(self):
        return Clock.t
    def __getattr__(self, name):
        return getattr(_time, name)

shim = Shim()
for m in (tstore, tmon, tbeh):
    m.time = shim
BJ = ZoneInfo("Asia/Shanghai")
def start_times():
    now = dt.datetime.fromtimestamp(Clock.t, BJ)
    day = now.replace(hour=0, minute=0, second=0, microsecond=0)
    return day.timestamp(), day.replace(day=1).timestamp()
tstore.start_times = start_times

store = tstore.Store(db)
monitor = tmon.Monitor(store)

def locate(cur, p):
    if isinstance(p, dict):
        return next(i for i, x in enumerate(cur) if x.get("uuid") == p["uuid"])
    return p

def set_path(obj, path, value, delete):
    if not path:
        return value
    cur = obj
    for p in path[:-1]:
        k = locate(cur, p)
        if isinstance(cur, dict) and k not in cur:
            cur[k] = {}
        cur = cur[k]
    k = locate(cur, path[-1])
    if delete:
        cur.pop(k)
    else:
        cur[k] = value
    return obj

def run(s):
    op = s["op"]
    Clock.t = s.get("now", Clock.t)
    if op == "set":
        with store.lock, store.db:
            if s["table"] == "meta":
                v = set_path(store.get_meta(s["id"], {}), s["path"], s.get("value"), s.get("delete"))
                store.meta(s["id"], v)
            else:
                col = "relay" if s["table"] == "relays" else "uuid"
                row = store.db.execute(f"SELECT data FROM {s['table']} WHERE {col}=?", (s["id"],)).fetchone()
                v = set_path(json.loads(row[0]), s["path"], s.get("value"), s.get("delete"))
                store.db.execute(f"UPDATE {s['table']} SET data=? WHERE {col}=?", (tstore.dumps(v), s["id"]))
    elif op == "restamp":
        with store.lock, store.db:
            for relay, st in s["relays"].items():
                d = json.loads(store.db.execute("SELECT data FROM relays WHERE relay=?", (relay,)).fetchone()[0])
                d["received_at"], d["ts"] = st["received_at"], st["ts"]
                for p in d.get("probe", {}).get("assets", []):
                    for k in ("tcp_at", "egress_at"):
                        if k in p:
                            p[k] = s[k]
                for p in d.get("paths", {}).get("assets", []):
                    for k in ("measured_at", "tcp_at", "egress_at"):
                        if k in p:
                            p[k] = s["path_at"]
                store.db.execute("UPDATE relays SET data=? WHERE relay=?", (tstore.dumps(d), relay))
            hosts = store.get_meta("hosts", {})
            for u, h in hosts.items():
                if u not in s["skip_hosts"] and isinstance(h, dict) and h.get("time"):
                    h["time"] = s["host_time"]
            store.meta("hosts", hosts)
            store.meta("hosts_at", s["hosts_at"])
    elif op == "sql":
        with store.lock, store.db:
            store.db.execute(s["sql"], s.get("args", []))
    elif op == "evaluate":
        monitor.evaluate(now=s["now"])
    elif op == "monitor_maintenance":
        monitor.maintenance()
    elif op == "monitor_init":
        tmon.Monitor(store)
    elif op == "behavior_init":
        store.behavior = tbeh.Behavior(store)
    elif op == "ingest":
        relay = s["relay"]
        # store.ingest 的事件写入段（逐字照抄）
        with store.lock, store.db:
            by_port = {a["relay_port"]: a for a in store.assets() if a.get("relay") == relay}
            master_ports = set(s.get("master_ports", []))
            if master_ports:
                master = next((a for a in store.assets() if a["team"] == "线路机" and a["name"] == relay), None)
                if master:
                    by_port.update({p: master for p in master_ports})
            for e in s["events"]:
                a = by_port.get(int(e["inbound"].split("-")[-1]))
                uuid = a["uuid"] if a else "direct:" + relay + ":" + e["inbound"].split("-")[-1]
                inserted = store.db.execute("INSERT OR IGNORE INTO activity VALUES(?,?,?,?,?,?,?,?,?,?,?,?)", (
                    relay + ":" + e["event_id"], e["ts"], uuid, a["team"] if a else ("未分配直连" if e["outbound"] == "direct" else "未归属入口"), relay,
                    e["source_ip"], e["host"], e["port"], e["protocol"], e["action"], e["client"], e["kind"]))
                if inserted.rowcount:
                    store.behavior.ingest(uuid, e)
    elif op == "detect":
        store.behavior.detect()
    elif op == "refresh":
        store.behavior._refresh()
    elif op == "behavior_maintenance":
        store.behavior.maintenance()
    else:
        raise ValueError("unknown op " + op)

print("@@ready", flush=True)
for line in sys.stdin:
    try:
        run(json.loads(line))
        print("@@" + json.dumps({"ok": True}), flush=True)
    except Exception as error:
        print("@@" + json.dumps({"ok": False, "error": "".join(traceback.format_exception(error))[-1500:]}), flush=True)
"##;

    struct Env {
        work: PathBuf,
        golden_db: PathBuf,
        as_of: f64,
        py: String,
        release: String,
    }

    fn env() -> Option<Env> {
        let var = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        let golden = PathBuf::from(var("LEGACY_DIFF_GOLDEN")?);
        let index: Value = serde_json::from_str(&std::fs::read_to_string(golden.join("index.json")).ok()?).ok()?;
        Some(Env {
            work: PathBuf::from(var("LEGACY_DIFF_WORK")?),
            golden_db: golden.join("operations.golden.db"),
            as_of: index["as_of"].as_f64()?,
            py: var("LEGACY_DIFF_PY")?,
            release: var("LEGACY_DIFF_RELEASE")?,
        })
    }

    struct Py {
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,
        child: std::process::Child,
    }

    impl Py {
        fn start(e: &Env, db: &Path) -> Py {
            let driver = e.work.join("legacy_driver.py");
            std::fs::write(&driver, DRIVER).unwrap();
            let mut child = Command::new(&e.py)
                .arg(&driver)
                .arg(&e.release)
                .arg(db)
                .arg(super::super::pyjson::float_repr(e.as_of))
                .env("TZ", "UTC")
                .env("PYTHONDONTWRITEBYTECODE", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("启动 Python 失败");
            let stdin = child.stdin.take().unwrap();
            let stdout = BufReader::new(child.stdout.take().unwrap());
            let mut py = Py { stdin, stdout, child };
            assert_eq!(py.line(), "ready");
            py
        }

        fn line(&mut self) -> String {
            loop {
                let mut s = String::new();
                if self.stdout.read_line(&mut s).unwrap() == 0 {
                    panic!("Python 进程提前退出");
                }
                if let Some(rest) = s.trim_end().strip_prefix("@@") {
                    return rest.to_string();
                }
            }
        }

        fn step(&mut self, op: &Value) -> Result<(), String> {
            writeln!(self.stdin, "{}", dumps(op)).unwrap();
            self.stdin.flush().unwrap();
            let r: Value = serde_json::from_str(&self.line()).unwrap();
            if r["ok"] == true {
                Ok(())
            } else {
                Err(r["error"].as_str().unwrap_or("").to_string())
            }
        }
    }

    impl Drop for Py {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    // ─── 步骤在 Rust 端的执行 ────────────────────────────────────────────────────────────────────

    fn set_path(cur: &mut Value, path: &[Value], value: &Option<Value>) -> Result<(), String> {
        if path.is_empty() {
            *cur = value.clone().unwrap_or(Value::Null);
            return Ok(());
        }
        let last = path.len() == 1;
        match cur {
            Value::Object(m) => {
                let k = path[0].as_str().ok_or("dict 路径必须是字符串")?.to_string();
                if last {
                    match value {
                        Some(v) => {
                            m.insert(k, v.clone());
                        }
                        None => {
                            m.shift_remove(&k).ok_or("KeyError")?;
                        }
                    }
                    Ok(())
                } else {
                    set_path(m.entry(k).or_insert_with(|| json!({})), &path[1..], value)
                }
            }
            Value::Array(a) => {
                let idx = match &path[0] {
                    Value::Number(n) => n.as_u64().ok_or("index")? as usize,
                    sel => a.iter().position(|x| get(x, "uuid") == &sel["uuid"]).ok_or("StopIteration")?,
                };
                if idx >= a.len() {
                    return Err("IndexError".into());
                }
                if last {
                    match value {
                        Some(v) => a[idx] = v.clone(),
                        None => {
                            a.remove(idx);
                        }
                    }
                    Ok(())
                } else {
                    set_path(&mut a[idx], &path[1..], value)
                }
            }
            _ => Err("TypeError: not subscriptable".into()),
        }
    }

    fn rs<T>(r: R<T>) -> Result<T, String> {
        r.map_err(|e| format!("{e:?}"))
    }

    fn run_rust(db: &Connection, cfg: &CompatCfg, clock: &mut f64, s: &Value) -> Result<(), String> {
        if let Some(t) = s.get("now").and_then(|v| v.as_f64()) {
            *clock = t;
        }
        let cx = Ctx { db, now: *clock, cfg };
        match s["op"].as_str().unwrap_or("") {
            "set" => rs(in_tx(&cx, || {
                let table = s["table"].as_str().unwrap();
                let id = s["id"].as_str().unwrap();
                let path = s["path"].as_array().cloned().unwrap_or_default();
                let value = if s.get("delete").is_some_and(truthy) { None } else { Some(s.get("value").cloned().unwrap_or(Value::Null)) };
                if table == "meta" {
                    let mut v = meta(db, id, json!({}))?;
                    set_path(&mut v, &path, &value).map_err(Err::Internal)?;
                    set_meta(db, id, &v)
                } else {
                    let col = if table == "relays" { "relay" } else { "uuid" };
                    let mut v = loads(&scalar(db, &format!("SELECT data FROM {table} WHERE {col}=?"), &[st(id)])?)?;
                    set_path(&mut v, &path, &value).map_err(Err::Internal)?;
                    exec(db, &format!("UPDATE {table} SET data=? WHERE {col}=?"), &[st(&dumps(&v)), st(id)])?;
                    Ok(())
                }
            })),
            "restamp" => rs(in_tx(&cx, || {
                for (relay, stamp) in s["relays"].as_object().unwrap() {
                    let mut d = loads(&scalar(db, "SELECT data FROM relays WHERE relay=?", &[st(relay)])?)?;
                    d["received_at"] = stamp["received_at"].clone();
                    d["ts"] = stamp["ts"].clone();
                    if let Some(ps) = d.get_mut("probe").and_then(|p| p.get_mut("assets")).and_then(|a| a.as_array_mut()) {
                        for p in ps.iter_mut().filter_map(|p| p.as_object_mut()) {
                            for k in ["tcp_at", "egress_at"] {
                                if p.contains_key(k) {
                                    p.insert(k.into(), s[k].clone());
                                }
                            }
                        }
                    }
                    if let Some(ps) = d.get_mut("paths").and_then(|p| p.get_mut("assets")).and_then(|a| a.as_array_mut()) {
                        for p in ps.iter_mut().filter_map(|p| p.as_object_mut()) {
                            for k in ["measured_at", "tcp_at", "egress_at"] {
                                if p.contains_key(k) {
                                    p.insert(k.into(), s["path_at"].clone());
                                }
                            }
                        }
                    }
                    exec(db, "UPDATE relays SET data=? WHERE relay=?", &[st(&dumps(&d)), st(relay)])?;
                }
                let mut hosts = meta(db, "hosts", json!({}))?;
                let skip = s["skip_hosts"].as_array().cloned().unwrap_or_default();
                for (u, h) in hosts.as_object_mut().unwrap().iter_mut() {
                    if !skip.iter().any(|x| x == u.as_str()) && h.is_object() && truthy(get(h, "time")) {
                        h["time"] = s["host_time"].clone();
                    }
                }
                set_meta(db, "hosts", &hosts)?;
                set_meta(db, "hosts_at", &s["hosts_at"])
            })),
            "sql" => rs(in_tx(&cx, || {
                let args: Vec<SqlVal> = s.get("args").and_then(|a| a.as_array()).map(|a| a.iter().map(|v| to_sql(v).unwrap()).collect()).unwrap_or_default();
                exec(db, s["sql"].as_str().unwrap(), &args).map(|_| ())
            })),
            "evaluate" => rs(evaluate(&cx, None)),
            "monitor_maintenance" => rs(maintenance(&cx)),
            "monitor_init" => rs(ensure(&cx)),
            "behavior_init" => rs(behavior_write::ensure(&cx)),
            "ingest" => rs(in_tx(&cx, || {
                let relay = s["relay"].as_str().unwrap();
                let masters: Vec<i64> = s.get("master_ports").and_then(|m| m.as_array()).map(|m| m.iter().filter_map(|p| p.as_i64()).collect()).unwrap_or_default();
                for e in s["events"].as_array().unwrap() {
                    let (asset, team) = behavior_write::event_owner(&cx, relay, &masters, e)?;
                    behavior_write::insert_activity(&cx, relay, &asset, &team, e)?;
                }
                Ok(())
            })),
            "detect" => rs(behavior_write::detect(&cx)),
            "refresh" => rs(behavior_write::refresh(&cx)),
            "behavior_maintenance" => rs(behavior_write::maintenance(&cx)),
            other => Err(format!("unknown op {other}")),
        }
    }

    // ─── 逐行比对 ────────────────────────────────────────────────────────────────────────────────

    fn same(a: ValueRef, b: ValueRef) -> bool {
        match (a, b) {
            (ValueRef::Null, ValueRef::Null) => true,
            (ValueRef::Integer(x), ValueRef::Integer(y)) => x == y,
            (ValueRef::Real(x), ValueRef::Real(y)) => x.to_bits() == y.to_bits(),
            (ValueRef::Text(x), ValueRef::Text(y)) => x == y,
            (ValueRef::Blob(x), ValueRef::Blob(y)) => x == y,
            _ => false,
        }
    }

    fn show(v: ValueRef) -> String {
        let clip = |s: String| if s.chars().count() > 160 { format!("{}…", head(&s, 160)) } else { s };
        match v {
            ValueRef::Null => "null".into(),
            ValueRef::Integer(x) => format!("integer:{x}"),
            ValueRef::Real(x) => format!("real:{}", super::super::pyjson::float_repr(x)),
            ValueRef::Text(t) => format!("text:{}", clip(String::from_utf8_lossy(t).into_owned())),
            ValueRef::Blob(b) => format!("blob:{}B", b.len()),
        }
    }

    fn first_text_diff(a: ValueRef, b: ValueRef) -> String {
        if let (ValueRef::Text(x), ValueRef::Text(y)) = (a, b) {
            let at = x.iter().zip(y.iter()).position(|(p, q)| p != q).unwrap_or(x.len().min(y.len()));
            let from = at.saturating_sub(120);
            return format!("\n      first diff @byte {at}\n      A …{}\n      B …{}", String::from_utf8_lossy(&x[from..(at + 120).min(x.len())]),
                String::from_utf8_lossy(&y[from..(at + 120).min(y.len())]));
        }
        String::new()
    }

    /// 两边同一 SQL（按 rowid 排序）逐行、逐列比对值与存储类型
    fn diff_table(a: &Connection, b: &Connection, table: &str, filter: &str, out: &mut Vec<String>) -> usize {
        let sql = format!("SELECT rowid,* FROM \"{table}\" {filter} ORDER BY rowid");
        let mut sa = a.prepare(&sql).unwrap();
        let mut sb = b.prepare(&sql).unwrap();
        let names: Vec<String> = sa.column_names().iter().map(|s| s.to_string()).collect();
        let mut ra = sa.query([]).unwrap();
        let mut rb = sb.query([]).unwrap();
        let mut n = 0;
        let mut diffs = 0;
        loop {
            let (x, y) = (ra.next().unwrap(), rb.next().unwrap());
            match (x, y) {
                (None, None) => break,
                (Some(x), None) => {
                    diffs += 1;
                    out.push(format!("{table}: 仅 Python 有 rowid={}", show(x.get_ref(0).unwrap())));
                    while ra.next().unwrap().is_some() {
                        diffs += 1;
                    }
                    break;
                }
                (None, Some(y)) => {
                    diffs += 1;
                    out.push(format!("{table}: 仅 Rust 有 rowid={}", show(y.get_ref(0).unwrap())));
                    while rb.next().unwrap().is_some() {
                        diffs += 1;
                    }
                    break;
                }
                (Some(x), Some(y)) => {
                    n += 1;
                    for (k, name) in names.iter().enumerate() {
                        let (p, q) = (x.get_ref(k).unwrap(), y.get_ref(k).unwrap());
                        if !same(p, q) {
                            diffs += 1;
                            if out.len() < 400 {
                                out.push(format!("{table} rowid={} {name}:\n      A(Python)={}\n      B(Rust)  ={}{}", show(x.get_ref(0).unwrap()), show(p), show(q), first_text_diff(p, q)));
                            }
                        }
                    }
                }
            }
        }
        let _ = n;
        diffs
    }

    const SMALL: [&str; 10] = ["health_state", "incidents", "health_hours", "meta", "audit", "behavior_events", "behavior_cache", "sources_seen", "destinations", "behavior_buckets"];

    fn max_rowid(db: &Connection, table: &str) -> i64 {
        db.query_row(&format!("SELECT coalesce(max(rowid),0) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    fn tables(db: &Connection) -> Vec<String> {
        db.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").unwrap().query_map([], |r| r.get(0)).unwrap().flatten().collect()
    }

    // ─── 场景 ────────────────────────────────────────────────────────────────────────────────────

    fn iso(t: f64, frac: &str) -> String {
        format!("{}{frac}", chrono::DateTime::from_timestamp(t.floor() as i64, 0).unwrap().format("%Y-%m-%dT%H:%M:%S"))
    }

    fn set(table: &str, id: &str, path: Value, value: Value) -> Value {
        json!({"op": "set", "table": table, "id": id, "path": path, "value": value})
    }

    struct World {
        assets: Vec<Value>,
        relays: Vec<String>,
    }

    fn world(db: &Connection) -> World {
        let assets = rows(db, "SELECT data FROM assets", &[]).unwrap().iter().map(|r| loads(&r["data"]).unwrap()).collect();
        let relays = rows(db, "SELECT relay FROM relays ORDER BY relay", &[]).unwrap().iter().map(|r| r["relay"].as_str().unwrap().to_string()).collect();
        World { assets, relays }
    }

    impl World {
        fn on_relay(&self, relay: &str) -> Vec<Value> {
            let mut v: Vec<Value> = self.assets.iter().filter(|a| get(a, "relay") == relay && truthy(get(a, "relay_port"))).cloned().collect();
            v.sort_by_key(|a| get(a, "relay_port").as_i64().unwrap_or(0));
            v
        }
        fn master(&self, relay: &str) -> Value {
            self.assets.iter().find(|a| get(a, "team") == store::RELAY_TEAM && get(a, "name") == relay).cloned().unwrap()
        }
    }

    fn uuid(a: &Value) -> String {
        get(a, "uuid").as_str().unwrap().to_string()
    }

    fn restamp(w: &World, t: f64, skip_relays: &[&str]) -> Value {
        let mut relays = Map::new();
        for r in &w.relays {
            if !skip_relays.contains(&r.as_str()) {
                relays.insert(r.clone(), json!({"received_at": t - 2.5, "ts": t - 3.25}));
            }
        }
        json!({"op": "restamp", "relays": relays, "tcp_at": t - 7.125, "egress_at": t - 20.5, "path_at": t - 11.0,
            "host_time": iso(t - 4.0, ".324360935Z"), "hosts_at": t - 1.0, "skip_hosts": []})
    }

    /// 监控：30 个 10 秒节拍 + 跨 UTC 零点 + 20 天后 maintenance；节拍之间对两份副本做相同的输入变更
    fn monitor_scenario(db: &Connection, as_of: f64) -> Vec<Value> {
        let w = world(db);
        let l2 = w.on_relay("线路机2");
        let [e, m, t, p, c, wm, mem, neg] = [0, 1, 2, 3, 4, 5, 6, 7].map(|k| uuid(&l2[k]));
        let h = uuid(&w.master("线路机1"));
        let relay_of = |u: &str| get(w.assets.iter().find(|a| uuid(a) == u).unwrap(), "relay").as_str().unwrap_or("").to_string();
        let r2 = relay_of(&e);
        let hosts = meta(db, "hosts", json!({})).unwrap();
        let ram_total = get(get(&hosts, &mem), "ram_total").as_i64().unwrap();
        let ram = get(get(&hosts, &mem), "ram").clone();
        let c_port = get(&l2[4], "target_port").clone();
        let m_port = get(&l2[1], "relay_port").clone();
        let probe = |u: &str, k: &str, v: Value| set("relays", &r2, json!(["probe", "assets", {"uuid": u}, k]), v);
        let path = |u: &str, k: &str, v: Value| set("relays", &r2, json!(["paths", "assets", {"uuid": u}, k]), v);
        let egress = |u: &str, fail: bool, attempt: Value| {
            vec![probe(u, "egress_error", if fail { json!("curl: (28) Operation timed out") } else { Value::Null }), probe(u, "egress_rc", json!(if fail { 28 } else { 0 })),
                probe(u, "egress_attempt", attempt)]
        };
        let mut steps = vec![];
        for k in 0..30i64 {
            let now = as_of + 10.0 * k as f64;
            if k > 0 {
                let stale: &[&str] = if (8..26).contains(&k) { &["线路机3"] } else { &[] };
                steps.push(restamp(&w, now, stale));
            }
            match k {
                2 => {
                    steps.extend(egress(&e, true, json!(1)));
                    steps.push(set("meta", "hosts", json!([mem, "ram"]), json!(ram_total * 95 / 100)));
                }
                3 => {
                    steps.extend(egress(&e, true, json!(3)));
                    steps.push(probe(&m, "actual_ip", json!("203.0.113.9")));
                }
                4 => steps.push(set("meta", "hosts", json!([h, "online"]), json!(false))),
                5 => {
                    steps.extend([probe(&t, "tcp_error", json!("connect: connection refused")), probe(&t, "tcp_ms", json!(-1)), probe(&t, "tcp_rc", json!(7)),
                        path(&t, "healthy", json!(false)), path(&t, "rc", json!(7)), path(&t, "actual_ip", Value::Null)]);
                    steps.push(set("meta", "applied_policies", json!([format!("node:{p}")]), json!({"mode": "blocked", "domains": [], "note": ""})));
                    steps.push(path(&p, "healthy", json!(false)));
                }
                6 => {
                    steps.extend(egress(&e, false, json!(1)));
                    steps.push(set("relays", &r2, json!(["inbounds", {"uuid": c}, "enable"]), json!(0)));
                }
                7 | 9 => steps.extend(egress(&e, true, json!(3))),
                8 | 10 => steps.extend(egress(&e, false, json!(2))),
                _ => {}
            }
            match k {
                9 => steps.push(set("relays", &r2, json!(["inbounds", {"uuid": c}, "target_port"]), json!(1))),
                10 => steps.push(set("assets", &m, json!(["relay_port"]), Value::Null)),
                11 => steps.push(set("meta", "hosts", json!([h, "online"]), json!(true))),
                13 => steps.extend([set("relays", &r2, json!(["inbounds", {"uuid": c}, "enable"]), json!(1)),
                    set("relays", &r2, json!(["inbounds", {"uuid": c}, "target_port"]), c_port.clone()), set("assets", &m, json!(["relay_port"]), m_port.clone()),
                    probe(&m, "actual_ip", get(&l2[1], "ip").clone())]),
                14 => {
                    steps.extend([probe(&t, "tcp_error", Value::Null), probe(&t, "tcp_ms", json!(9)), probe(&t, "tcp_rc", json!(0)), path(&t, "healthy", json!(true)),
                        path(&t, "rc", json!(0)), path(&t, "actual_ip", get(&l2[2], "ip").clone())]);
                    steps.extend(egress(&wm, true, json!(3)));
                }
                15 => steps.push(path(&p, "healthy", json!(true))),
                16 => steps.push(set("assets", &wm, json!(["profile"]),
                    json!({"maintenance_until": now + 95.0, "thresholds": {"cpu_percent": 80.0, "memory_percent": 90.0, "disk_percent": 85.0, "latency_ms": 3000.0}}))),
                18 => {
                    steps.extend(egress(&wm, false, json!(1)));
                    steps.push(set("meta", "hosts", json!([mem, "ram"]), ram.clone()));
                }
                19 => {
                    let mut n = w.assets.iter().find(|a| uuid(a) == e).unwrap().clone();
                    n["uuid"] = json!("00000000-0000-4000-8000-00000000d1ff");
                    n["name"] = json!("差分测试节点");
                    n["relay_port"] = Value::Null;
                    n["profile"] = json!({"maintenance_until": now + 30.0, "thresholds": {}});
                    steps.push(json!({"op": "sql", "sql": "INSERT INTO assets VALUES(?,?)", "args": ["00000000-0000-4000-8000-00000000d1ff", dumps(&n)]}));
                }
                20 => steps.push(set("assets", &wm, json!(["profile", "maintenance_until"]), json!(iso(now + 50.0, "Z")))),
                21 => steps.extend([probe(&neg, "tcp_ms", json!(-1)), probe(&neg, "egress_ms", json!(-5)), probe(&neg, "egress_error", json!("x")),
                    probe(&neg, "egress_rc", json!(28.0)), probe(&neg, "egress_attempt", json!(3.0))]),
                24 => steps.extend([probe(&neg, "tcp_ms", json!(12)), probe(&neg, "egress_ms", json!(640.5)), probe(&neg, "egress_error", Value::Null),
                    probe(&neg, "egress_rc", json!(0)), probe(&neg, "egress_attempt", json!(1))]),
                27 => steps.push(set("assets", &wm, json!(["profile", "maintenance_until"]), json!(0.0))),
                _ => {}
            }
            steps.push(json!({"op": "evaluate", "now": now}));
        }
        // 跨 UTC 零点（days 列表滚动、按日覆盖率），再到 20 天后做清理
        let midnight = ((as_of / 86400.0).floor() + 1.0) * 86400.0;
        for t in [midnight + 30.0, midnight + 95.0] {
            steps.push(restamp(&w, t, &[]));
            steps.push(json!({"op": "evaluate", "now": t}));
        }
        let later = as_of + 20.0 * 86400.0 + 7.0;
        steps.push(json!({"op": "monitor_maintenance", "now": later}));
        steps.push(restamp(&w, later, &[]));
        steps.push(json!({"op": "evaluate", "now": later}));
        steps
    }

    fn event(id: String, ts: Value, port: i64, host: &str, ip: &str, action: &str, kind: &str) -> Value {
        json!({"event_id": id, "ts": ts, "inbound": format!("inbound-{port}"), "outbound": "ss-x", "source_ip": ip, "host": host, "port": 443,
            "protocol": "tcp", "action": action, "client": "node-01", "kind": kind})
    }

    /// 行为：两台新节点（13/12 个历史桶 → int/float 中位数）+ 突增桶、已有节点的新目标/新来源、
    /// 重复事件、未归属/直连/主入口端口、detect、refresh、maintenance
    fn behavior_scenario(db: &Connection, as_of: f64) -> Vec<Value> {
        let w = world(db);
        let l2 = w.on_relay("线路机2");
        let base = l2[0].clone();
        let mut steps = vec![json!({"op": "refresh", "now": as_of})];
        let vs = [("00000000-0000-4000-8000-0000000bea01", 29001i64, 13usize, 330usize, 25usize), ("00000000-0000-4000-8000-0000000bea02", 29002, 12, 310, 3)];
        for (u, port, _, _, _) in vs {
            let mut a = base.clone();
            a["uuid"] = json!(u);
            a["relay_port"] = json!(port);
            a["name"] = json!(format!("差分行为节点 {port}"));
            steps.push(json!({"op": "sql", "sql": "INSERT INTO assets VALUES(?,?)", "args": [u, dumps(&a)]}));
        }
        let tdet = as_of + 900.0;
        let cur = (tdet / 300.0).floor() as i64 * 300 - 300;
        for (_, port, hist, spike, blocked) in vs {
            for j in (1..=hist).rev() {
                let b = cur - 300 * j as i64;
                let n = 5 + (j * 7) % 4;
                let evs: Vec<Value> = (0..n)
                    .map(|q| {
                        let ts = if q % 2 == 0 { json!(b + 3 + q as i64) } else { json!(b as f64 + 1.5 + q as f64 * 0.25) };
                        let host = if j <= 3 { format!("late-{j}.example.net") } else { format!("h{}.example.com", q % 3) };
                        event(format!("d{port}-{j}-{q}"), ts, port, &host, &format!("198.51.100.{}", if j <= 2 { 50 + j } else { q % 2 }), "routed", "domain")
                    })
                    .collect();
                steps.push(json!({"op": "ingest", "now": as_of, "relay": "线路机2", "events": evs}));
            }
            let evs: Vec<Value> = (0..spike)
                .map(|q| {
                    let action = if q < blocked { "blocked" } else { "routed" };
                    let (host, kind) = if q % 5 == 0 { (format!("203.0.113.{}", q % 7), "ip") } else { (format!("s{}.example.org", q % 4), "domain") };
                    event(format!("s{port}-{q}"), json!(cur as f64 + (q as f64) * 0.75), port, &host, "198.51.100.1", action, kind)
                })
                .collect();
            steps.push(json!({"op": "ingest", "now": as_of, "relay": "线路机2", "events": evs}));
        }
        // 已有节点：新目标、新来源、重复事件、仅 IP、被阻止；未归属与直连端口；线路机主入口
        let port = get(&base, "relay_port").as_i64().unwrap();
        let mut evs = vec![
            event("x-1".into(), json!(as_of + 5.0), port, "brand-new.example.io", "192.0.2.77", "routed", "domain"),
            event("x-2".into(), json!((as_of as i64) + 6), port, "brand-new.example.io", "192.0.2.77", "blocked", "domain"),
            event("x-2".into(), json!((as_of as i64) + 6), port, "brand-new.example.io", "192.0.2.77", "blocked", "domain"),
            event("x-3".into(), json!(as_of + 7.25), port, "192.0.2.200", "192.0.2.78", "routed", "ip"),
            event("x-4".into(), json!(as_of + 8.0), 9, "direct.example.com", "192.0.2.79", "routed", "domain"),
            event("x-5".into(), json!(as_of + 8.5), 7777, "orphan.example.com", "192.0.2.80", "routed", "domain"),
        ];
        evs[4]["outbound"] = json!("direct");
        steps.push(json!({"op": "ingest", "now": as_of + 9.0, "relay": "线路机2", "events": evs}));
        steps.push(json!({"op": "ingest", "now": as_of + 9.0, "relay": "线路机2", "master_ports": [20001],
            "events": [event("x-6".into(), json!(as_of + 9.0), 20001, "master.example.com", "192.0.2.81", "routed", "domain")]}));
        steps.push(json!({"op": "detect", "now": tdet}));
        steps.push(json!({"op": "detect", "now": tdet + 1.0}));
        steps.push(json!({"op": "refresh", "now": tdet}));
        let later = as_of + 15.0 * 86400.0 + 3.0;
        steps.push(json!({"op": "behavior_maintenance", "now": later}));
        steps.push(json!({"op": "detect", "now": later}));
        steps.push(json!({"op": "refresh", "now": later}));
        steps
    }

    /// 初始化：meta 缺失时的 Monitor/Behavior 初始化（含从 activity 重新播种一台节点）
    fn init_scenario(db: &Connection, as_of: f64) -> Vec<Value> {
        let w = world(db);
        let e = uuid(&w.on_relay("线路机2")[0]);
        let mut steps: Vec<Value> = ["DELETE FROM meta WHERE key IN ('monitor_started_at','availability_statistic_v2','behavior_started_at')",
            "DELETE FROM destinations WHERE asset=?1", "DELETE FROM sources_seen WHERE asset=?1", "DELETE FROM behavior_buckets WHERE asset=?1",
            "DELETE FROM meta WHERE key='monitor_started:'||?1", "DELETE FROM health_state WHERE asset=?1"]
            .iter()
            .map(|q| if q.contains("?1") { json!({"op": "sql", "sql": q, "args": [e]}) } else { json!({"op": "sql", "sql": q}) })
            .collect();
        steps.push(json!({"op": "monitor_init", "now": as_of + 1.0}));
        steps.push(json!({"op": "behavior_init", "now": as_of + 1.0}));
        steps.push(json!({"op": "evaluate", "now": as_of + 1.0}));
        steps.push(json!({"op": "refresh", "now": as_of + 1.0}));
        steps
    }

    fn copy_db(src: &Path, dst: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", dst.display()));
        }
        let ok = Command::new("cp").arg("--reflink=auto").arg(src).arg(dst).status().unwrap().success();
        assert!(ok, "复制 golden 库失败");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn run_scenario(e: &Env, name: &str, build: fn(&Connection, f64) -> Vec<Value>) -> usize {
        let a_path = e.work.join(format!("{name}-A.db"));
        let b_path = e.work.join(format!("{name}-B.db"));
        copy_db(&e.golden_db, &a_path);
        copy_db(&e.golden_db, &b_path);
        let b = Connection::open(&b_path).unwrap();
        b.busy_timeout(std::time::Duration::from_secs(30)).unwrap();
        b.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        let steps = build(&b, e.as_of);
        let cfg = CompatCfg::stage_defaults(&b);
        let mut py = Py::start(e, &a_path);
        let a = Connection::open_with_flags(&a_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        a.busy_timeout(std::time::Duration::from_secs(30)).unwrap();
        // Store()/Monitor() 已在 Python 端构造；Rust 端对应的初始化
        let mut clock = e.as_of;
        {
            let cx = Ctx { db: &b, now: clock, cfg: &cfg };
            ensure(&cx).unwrap();
            behavior_write::ensure(&cx).unwrap();
        }
        let base_samples = max_rowid(&a, "health_samples");
        let base_activity = max_rowid(&a, "activity");
        let mut total = 0;
        let started = std::time::Instant::now();
        for (k, s) in steps.iter().enumerate() {
            let t0 = std::time::Instant::now();
            let pr = py.step(s);
            let t1 = t0.elapsed();
            let rr = run_rust(&b, &cfg, &mut clock, s);
            let t2 = t0.elapsed() - t1;
            let op = s["op"].as_str().unwrap_or("");
            match (&pr, &rr) {
                (Ok(()), Ok(())) => {}
                (Err(x), Err(y)) if !matches!(op, "set" | "restamp" | "sql") => println!("  [{name} #{k} {op}] 两边都报错（一致）：py={} rust={y}", x.lines().last().unwrap_or("")),
                _ => {
                    total += 1;
                    println!("  [{name} #{k} {op}] 结果不一致：python={pr:?} rust={rr:?}");
                    assert!(!matches!(op, "set" | "restamp" | "sql"), "输入变更必须两边都成功");
                }
            }
            let mut out = vec![];
            let mut n = 0;
            for t in SMALL {
                n += diff_table(&a, &b, t, "", &mut out);
            }
            n += diff_table(&a, &b, "health_samples", &format!("WHERE rowid>{base_samples}"), &mut out);
            n += diff_table(&a, &b, "activity", &format!("WHERE rowid>{base_activity}"), &mut out);
            if n > 0 || matches!(op, "evaluate" | "detect" | "refresh" | "monitor_maintenance" | "behavior_maintenance" | "behavior_init" | "monitor_init") {
                println!("  [{name} #{k:>3} {op:<20}] now={} diffs={n}  (py {:.0?}, rust {:.0?})", super::super::pyjson::float_repr(clock), t1, t2);
            }
            for d in out.iter().take(30) {
                println!("    {d}");
            }
            total += n;
            if n > 0 {
                // 后续步骤会级联出同样的差异；第一处就停下
                break;
            }
        }
        let mut out = vec![];
        let (ta, tb) = (tables(&a), tables(&b));
        if ta != tb {
            total += 1;
            println!("  [{name}] 表集合不同：{ta:?} vs {tb:?}");
        }
        let mut full = 0;
        for t in &ta {
            full += diff_table(&a, &b, t, "", &mut out);
        }
        for d in out.iter().take(30) {
            println!("    {d}");
        }
        println!("  [{name}] 全表比对（{} 张表）diffs={full}；{} 步，用时 {:.1?}", ta.len(), steps.len(), started.elapsed());
        total + full
    }

    #[test]
    #[ignore = "需要 a395 上的私有 golden 库与 Python 运行时，见文件头"]
    fn diff() {
        let Some(e) = env() else {
            println!("未设置 LEGACY_DIFF_* 环境变量，跳过");
            return;
        };
        std::fs::create_dir_all(&e.work).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&e.work, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut total = 0;
        let scenarios: [(&str, fn(&Connection, f64) -> Vec<Value>); 3] = [("monitor", monitor_scenario), ("behavior", behavior_scenario), ("init", init_scenario)];
        let only = std::env::var("LEGACY_DIFF_ONLY").unwrap_or_default();
        for (name, build) in scenarios {
            if !only.is_empty() && !only.split(',').any(|s| s == name) {
                continue;
            }
            println!("== 场景 {name}");
            total += run_scenario(&e, name, build);
        }
        println!("total diffs: {total}");
        assert_eq!(total, 0);
    }
}
