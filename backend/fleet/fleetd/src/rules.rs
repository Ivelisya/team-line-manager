//! 告警规则 v1：全部从旧形状的 /overview + /health 推导（原生模式下由原生旧库渲染；与旧健康转移逐条对应）。
use crate::alerts::{Labels, Observation, Severity};
use crate::config::AlertCfg;
use chrono::TimeZone;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

/// 套餐目录里规则需要的字段（按资产全名索引）。
#[derive(Clone, Debug, Default)]
pub struct PlanInfo {
    pub plan_id: String,
    pub canonical_name: String,
    pub quota_gb: Option<f64>,
    pub quota_direction: Option<String>,
    pub quota_period: Option<String>,
    pub traffic_unlimited: Option<bool>,
}

pub type PlanIndex = HashMap<String, PlanInfo>;

/// 出口 IP 是否参与告警判定（2026-09-24 起关闭：节点健康只看连通性）
const EGRESS_IP_COMPARE: bool = false;

pub const LEGACY_RULES: &[&str] = &[
    "node_health_fail",
    "node_health_degraded",
    "egress_mismatch",
    "host_offline",
    "host_resource",
    "host_sync_stale",
    "relay_down",
    "relay_mgmt_down",
    "relay_xray_down",
    "collector_stale",
    "monitor_stale",
    "correlated_node_failure",
    "renewal_due",
    "quota",
];

pub fn legacy_scope() -> HashSet<String> {
    LEGACY_RULES.iter().map(|s| s.to_string()).collect()
}

/// 「还没数据」类规则：fleetd 刚启动时（边缘上报有几十秒空窗、喂数与健康评估还没跑第一轮）不开新告警，也不恢复已有告警。
pub const STARTUP_RULES: &[&str] = &["host_offline", "host_sync_stale", "relay_down", "relay_mgmt_down", "collector_stale", "monitor_stale"];

/// 启动宽限是否仍在：启动后 `grace_s` 内一律是；之后直到这次评估用的数据确实是启动后产生的——
/// 收到了第一批边缘上报（`first_ingest_at`，`need_ingest` 时）且主机快照在那之后刷新过（overview.hosts_at），
/// 采集与健康评估的时间戳都在启动之后（health.collector_at / monitor_at）；最长到 `grace_max_s`。
pub fn startup_hold(started_at: f64, first_ingest_at: Option<f64>, need_ingest: bool, ov: &Value, health: &Value, now: f64, grace_s: f64, grace_max_s: f64) -> bool {
    let elapsed = now - started_at;
    if elapsed < grace_s {
        return true;
    }
    if elapsed >= grace_max_s {
        return false;
    }
    let after = |t: Option<f64>, t0: f64| t.is_some_and(|t| t >= t0);
    let hosts_at = f(ov, "hosts_at");
    let hosts_ok = if need_ingest { first_ingest_at.is_some_and(|t| after(hosts_at, t)) } else { after(hosts_at, started_at) };
    !(hosts_ok && after(f(health, "collector_at"), started_at) && after(f(health, "monitor_at"), started_at))
}

impl Derived {
    /// 启动宽限：去掉 STARTUP_RULES 的观测，并移出 scope（已有告警保持原状，不当作恢复）
    pub fn hold_startup(&mut self) {
        self.obs.retain(|o| !STARTUP_RULES.contains(&o.rule.as_str()));
        for r in STARTUP_RULES {
            self.scope.remove(*r);
        }
    }
}

/// 「团队1-01 · ATT · 75.37.237.38」→ t1-01；「线路机2」→ r2（与 build.py 的短名规则一致）。
pub fn short_id(name: &str) -> String {
    let head = name.split(" · ").next().unwrap_or(name).trim();
    if let Some(rest) = head.strip_prefix("团队") {
        if let Some((t, n)) = rest.split_once('-') {
            if t.chars().all(|c| c.is_ascii_digit()) && n.chars().all(|c| c.is_ascii_digit()) {
                return format!("t{t}-{n}");
            }
        }
    }
    if let Some(n) = head.strip_prefix("线路机") {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return format!("r{n}");
        }
    }
    head.to_string()
}

pub fn head_name(name: &str) -> String {
    name.split(" · ").next().unwrap_or(name).trim().to_string()
}

pub fn parse_time(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if !s.is_empty() => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.timestamp() as f64 + d.timestamp_subsec_nanos() as f64 / 1e9),
        _ => None,
    }
}

fn s(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

/// 账期时区的日历天数（与旧系统 due_calendar_days 相同：日期标签，不按 24 h 取整）。
pub fn due_calendar_days(asset: &Value, now: f64) -> Option<i64> {
    let tzname = asset.get("billing_timezone").and_then(|x| x.as_str()).filter(|x| !x.is_empty()).unwrap_or("Asia/Shanghai");
    let tz: chrono_tz::Tz = tzname.parse().ok()?;
    let today = tz.timestamp_opt(now as i64, 0).single()?.date_naive();
    let due = if let Some(d) = asset.get("due_date").and_then(|x| x.as_str()).filter(|x| !x.is_empty()) {
        chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?
    } else {
        let e = asset.get("expired_at").and_then(|x| x.as_str())?;
        chrono::DateTime::parse_from_rfc3339(e).ok()?.with_timezone(&tz).date_naive()
    };
    Some((due - today).num_days())
}

/// 续费阈值：天数落在哪一档（最小的 t ≥ days）。
pub fn renewal_threshold(days: i64, thresholds: &[i64]) -> Option<i64> {
    let mut t: Vec<i64> = thresholds.to_vec();
    t.sort();
    t.into_iter().find(|&x| days <= x)
}

/// 配额档位：≥ start 后每 step 一档。
pub fn quota_step(pct: f64, start: u32, step: u32) -> Option<u32> {
    if !pct.is_finite() || pct < start as f64 {
        return None;
    }
    let s = step.max(1) as f64;
    Some(((pct / s).floor() * s) as u32)
}

fn base_labels(kind: &str, a: &Value) -> Labels {
    let mut l = Labels::new();
    l.insert("kind".into(), kind.into());
    let relay = if s(a, "team") == "线路机" { head_name(&s(a, "name")) } else { s(a, "relay") };
    if !relay.is_empty() {
        l.insert("relay_id".into(), short_id(&relay));
        l.insert("relay".into(), relay);
    }
    let p = s(a, "provider");
    if !p.is_empty() {
        l.insert("provider".into(), p);
    }
    l
}

pub fn node_labels(a: &Value) -> Labels {
    let mut l = base_labels("node", a);
    let name = s(a, "name");
    l.insert("node".into(), short_id(&name));
    l.insert("node_name".into(), head_name(&name));
    l.insert("team".into(), s(a, "team"));
    let port = s(a, "relay_port");
    if !port.is_empty() {
        l.insert("entry".into(), format!("{}:{}", s(a, "relay"), port));
    }
    l
}

fn check_evidence(c: &Value) -> Value {
    let mut ev = serde_json::Map::new();
    ev.insert("check".into(), c.get("key").cloned().unwrap_or(Value::Null));
    for k in ["status", "reason", "value", "measured_at"] {
        if let Some(v) = c.get(k) {
            if !v.is_null() {
                ev.insert(k.into(), v.clone());
            }
        }
    }
    if let Some(Value::Object(m)) = c.get("evidence") {
        for (k, v) in m {
            if k != "attempts" && !v.is_null() {
                ev.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(ev)
}

pub struct Derived {
    pub obs: Vec<Observation>,
    pub scope: HashSet<String>,
    /// 状态未知的节点（短名）：已有告警保持，不当作恢复
    pub hold: HashSet<String>,
}

pub fn derive(ov: &Value, health: &Value, plans: &PlanIndex, cfg: &AlertCfg, now: f64) -> Derived {
    let mut obs: Vec<Observation> = vec![];
    let empty = vec![];
    let assets = ov.get("assets").and_then(|x| x.as_array()).unwrap_or(&empty);
    let relays = ov.get("relays").cloned().unwrap_or(Value::Null);
    let nodes = health.pointer("/monitor/nodes").cloned().unwrap_or(Value::Null);
    // 新鲜度按数据自己的渲染时刻算（overview.now / health.time）：评估用的是轮询缓存里的那一份（空闲时最多 60 s 前取的），
    // 拿评估时刻去比会把缓存的年龄算成“主机数据同步停滞 / 主机离线”。数据本身停更由 legacy_source_down（缓存过旧）负责。
    let at = |v: &Value, k: &str| f(v, k).filter(|t| *t > 0.0 && *t <= now + 5.0).unwrap_or(now);
    let (t_ov, t_h) = (at(ov, "now"), at(health, "time"));
    let hosts_at = f(ov, "hosts_at").unwrap_or(0.0);
    let host_sync_ok = t_ov - hosts_at <= 60.0;

    // ── 全局：采集器、健康评估、主机同步 ──
    let mut global = |rule: &str, since: Option<f64>, ref_t: f64, max_age: f64, for_s: f64, summary: String| {
        let age = since.map(|t| ref_t - t).unwrap_or(f64::INFINITY);
        if age > max_age {
            let mut l = Labels::new();
            l.insert("kind".into(), "system".into());
            l.insert("cause".into(), rule.into());
            obs.push(Observation {
                rule: rule.into(),
                labels: l,
                severity: if rule == "host_sync_stale" { Severity::Warning } else { Severity::Critical },
                summary,
                evidence: json!({"last_at": since, "age_s": if age.is_finite() { json!(age.round()) } else { Value::Null }}),
                active_since: since,
                for_s,
            notify_resolved: true,
            });
        }
    };
    let collector_at = f(health, "collector_at");
    let monitor_at = f(health, "monitor_at");
    global("collector_stale", collector_at, t_h, 60.0, 120.0, "采集循环停止上报（collector.checked_at 过旧）".into());
    global("monitor_stale", monitor_at, t_h, 60.0, 180.0, "健康评估停止（monitor.checked_at 过旧），节点状态不可信".into());
    global("host_sync_stale", if hosts_at > 0.0 { Some(hosts_at) } else { None }, t_ov, 60.0, 60.0,
        "主机快照同步停止（hosts_at 过旧），主机离线判断暂停".into());

    let mut critical_nodes: Vec<(Labels, f64, String)> = vec![];
    let mut hold: HashSet<String> = HashSet::new();

    for a in assets {
        let name = s(a, "name");
        let uuid = s(a, "uuid");
        let is_relay = s(a, "team") == "线路机";
        let h = nodes.get(&uuid).cloned().unwrap_or(Value::Null);
        let checks = h.get("checks").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        let maintenance = h.get("maintenance").and_then(|x| x.as_bool()).unwrap_or(false);
        let short = short_id(&name);

        // ── 续费（节点与线路机） ──
        let days = due_calendar_days(a, now).or_else(|| a.get("due_days").and_then(|x| x.as_i64()));
        if let Some(days) = days {
            if let Some(t) = renewal_threshold(days, &cfg.renewal_days) {
                let mut l = base_labels("renewal", a);
                l.insert("asset".into(), short.clone());
                l.insert("cause".into(), "renewal_due".into());
                l.insert("threshold".into(), t.to_string());
                let sorted = { let mut x = cfg.renewal_days.clone(); x.sort(); x };
                let sev = if t <= sorted[0] { Severity::Critical } else if sorted.len() > 1 && t <= sorted[1] { Severity::Warning } else { Severity::Info };
                let plan = plans.get(&name).map(|p| p.canonical_name.clone()).unwrap_or_else(|| s(a, "plan"));
                let due = if s(a, "due_date").is_empty() { s(a, "expired_at") } else { s(a, "due_date") };
                let summary = if days < 0 {
                    format!("{} 已过期 {} 天（到期 {}，{}）· {} · {} {}", head_name(&name), -days, due, s(a, "billing_timezone"), plan, s(a, "price"), s(a, "currency"))
                } else {
                    format!("{} 还有 {} 天到期（{}，{}）· {} · {} {}", head_name(&name), days, due, s(a, "billing_timezone"), plan, s(a, "price"), s(a, "currency"))
                };
                obs.push(Observation {
                    rule: "renewal_due".into(),
                    labels: l,
                    severity: sev,
                    summary,
                    evidence: json!({"days": days, "due_date": due, "billing_timezone": a.get("billing_timezone"),
                        "provider": a.get("provider"), "service_id": a.get("service_id"), "price": a.get("price"), "plan": plan}),
                    active_since: None,
                    for_s: 0.0,
                    notify_resolved: false,
                });
            }
        }

        // ── 主机资源（cpu/内存/磁盘 degraded 持续 ≥ 120 s） ──
        if !maintenance {
            for c in &checks {
                let key = s(c, "key");
                if ["cpu", "memory", "disk"].contains(&key.as_str()) && s(c, "status") == "degraded" {
                    let mut l = if is_relay { base_labels("relay", a) } else { node_labels(a) };
                    if is_relay {
                        l.insert("asset".into(), short.clone());
                    }
                    l.insert("cause".into(), format!("{key}_high"));
                    l.insert("check".into(), key.clone());
                    obs.push(Observation {
                        rule: "host_resource".into(),
                        labels: l,
                        severity: Severity::Warning,
                        summary: format!("{} {} {}%（阈值 {}%）", head_name(&name), s(c, "title"), s(c, "value"),
                            c.pointer("/evidence/limit").map(|x| x.to_string()).unwrap_or_default()),
                        evidence: check_evidence(c),
                        active_since: f(c, "since"),
                        for_s: 120.0,
                        notify_resolved: true,
                    });
                }
            }
        }

        // ── 主机探针在线 ──
        let host = a.get("host").cloned().unwrap_or(Value::Null);
        let last_seen = parse_time(host.get("time").unwrap_or(&Value::Null));
        let online = host.get("online").and_then(|x| x.as_bool()).unwrap_or(false);
        let host_required = checks.iter().any(|c| s(c, "key") == "host" && s(c, "status") != "na") || is_relay;
        let host_age = last_seen.map(|t| t_ov - t).unwrap_or(f64::INFINITY);
        let host_ok = !host.is_null() && online && host_age <= 60.0;

        if is_relay {
            let rname = head_name(&name);
            let r = relays.get(&rname).cloned().unwrap_or(Value::Null);
            let mgmt_ok = r.get("fresh").and_then(|x| x.as_bool()).unwrap_or(false);
            let received_at = f(&r, "received_at");
            let panel = r.get("panel").cloned().unwrap_or(Value::Null);
            let l = base_labels("relay", a);
            let short_c = short.clone();
            let push = |obs: &mut Vec<Observation>, rule: &str, cause: &str, summary: String, since: Option<f64>, ev: Value, sev: Severity, for_s: f64| {
                let mut l = l.clone();
                l.insert("cause".into(), cause.into());
                if rule == "host_offline" {
                    l.insert("asset".into(), short_c.clone());
                }
                obs.push(Observation { rule: rule.into(), labels: l, severity: sev, summary, evidence: ev, active_since: since, for_s, notify_resolved: true });
            };
            let ev = json!({"host_online": online, "host_last_seen": last_seen, "mgmt_received_at": received_at,
                "mgmt_error": r.get("error"), "xray_running": panel.get("xray_running"), "panel_ok": panel.get("ok")});
            if !host_sync_ok {
                // 主机数据不可信时只看管理通道
                if !mgmt_ok {
                    push(&mut obs, "relay_mgmt_down", "relay_mgmt_down", format!("{rname} 管理通道中断（采集快照过旧）"), received_at, ev.clone(), Severity::Critical, cfg.relay_for_s as f64);
                }
            } else if !host_ok && !mgmt_ok {
                let since = match (last_seen, received_at) { (Some(x), Some(y)) => Some(x.max(y)), (x, y) => x.or(y) };
                push(&mut obs, "relay_down", "relay_down", format!("{rname} 主机探针离线且管理通道中断"), since, ev.clone(), Severity::Critical, cfg.relay_for_s as f64);
            } else if !mgmt_ok {
                push(&mut obs, "relay_mgmt_down", "relay_mgmt_down", format!("{rname} 管理通道中断（主机在线）"), received_at, ev.clone(), Severity::Critical, cfg.relay_for_s as f64);
            } else if !host_ok {
                push(&mut obs, "host_offline", "host_offline", format!("{rname} 主机探针离线（管理通道正常）"), last_seen, ev.clone(), Severity::Warning, cfg.host_offline_s as f64);
            }
            if mgmt_ok && panel.get("ok").and_then(|x| x.as_bool()) == Some(true) && panel.get("xray_running").and_then(|x| x.as_bool()) == Some(false) {
                push(&mut obs, "relay_xray_down", "relay_xray_down", format!("{rname} Xray 未运行（面板 {}）", s(&panel, "version")), None, ev.clone(), Severity::Critical, cfg.relay_for_s as f64);
            }
            // ── 线路机出站配额 ──
            if let Some(p) = plans.get(&name).or_else(|| plans.get(&rname)) {
                if let (Some(q), Some(used)) = (p.quota_gb, f(&host, "net_total_up")) {
                    let quota = q * 1e9;
                    let pct = used * 100.0 / quota;
                    if let Some(step) = quota_step(pct, cfg.quota_start_pct, cfg.quota_step_pct) {
                        let mut l = base_labels("quota", a);
                        l.insert("cause".into(), "quota".into());
                        l.insert("threshold".into(), step.to_string());
                        obs.push(Observation {
                            rule: "quota".into(),
                            labels: l,
                            severity: if pct >= 100.0 { Severity::Critical } else { Severity::Warning },
                            summary: format!("{rname} 本期出站 {:.1} GB / {} GB（{:.1}%，已过 {}% 档）", used / 1e9, q, pct, step),
                            evidence: json!({"used_bytes": used, "quota_bytes": quota, "pct": (pct * 10.0).round() / 10.0,
                                "source": "主机累计出站 net_total_up（nodeagent / relayd 网卡计数）", "plan": p.canonical_name}),
                            active_since: None,
                            for_s: 0.0,
                            notify_resolved: false,
                        });
                    }
                }
            }
            continue;
        }

        // ── 以下为节点 ──
        if maintenance {
            continue;
        }
        if s(&h, "status") == "unknown" || h.is_null() {
            hold.insert(short.clone());
        }
        if host_required && host_sync_ok && !host_ok {
            let mut l = node_labels(a);
            l.insert("cause".into(), "host_offline".into());
            obs.push(Observation {
                rule: "host_offline".into(),
                labels: l,
                severity: Severity::Warning,
                summary: format!("{} 主机探针离线（最后上报 {}）", head_name(&name),
                    last_seen.map(|t| crate::alerts::fmt_time(t, chrono_tz::Asia::Shanghai)).unwrap_or_else(|| "无".into())),
                evidence: json!({"online": online, "last_seen": last_seen, "age_s": if host_age.is_finite() { json!(host_age.round()) } else { Value::Null }}),
                active_since: last_seen,
                for_s: cfg.host_offline_s as f64,
                notify_resolved: true,
            });
        }
        // 出口 IP 不一致（认证出口或实际入口到出口）
        let mut mismatch = false;
        for c in &checks {
            let key = s(c, "key");
            if key != "egress" && key != "ingress" {
                continue;
            }
            let actual = c.pointer("/evidence/actual_ip").and_then(|x| x.as_str()).unwrap_or("");
            let expected = c.pointer("/evidence/expected_ip").and_then(|x| x.as_str()).unwrap_or("");
            let st = s(c, "status");
            // 出口 IP 不再与记录比对（只看连通性；IP 变化由喂数自动更新资产，线路机 IP 泄漏见 native::egress_leak_observations）：
            // egress_mismatch 留在 scope 里只为让旧告警恢复
            if EGRESS_IP_COMPARE && !actual.is_empty() && !expected.is_empty() && actual != expected && (st == "fail" || st == "degraded") {
                mismatch = true;
                let mut l = node_labels(a);
                l.insert("cause".into(), "egress_mismatch".into());
                l.insert("check".into(), key.clone());
                obs.push(Observation {
                    rule: "egress_mismatch".into(),
                    labels: l,
                    severity: Severity::Critical,
                    summary: format!("{} {}：出口 {actual}，期望 {expected}", head_name(&name), s(c, "title")),
                    evidence: check_evidence(c),
                    active_since: f(c, "since"),
                    for_s: 0.0,
                    notify_resolved: true,
                });
                critical_nodes.push((node_labels(a), f(c, "since").unwrap_or(now), "egress_mismatch".into()));
            }
        }
        let status = s(&h, "status");
        if status == "fail" {
            if let Some(c) = checks.iter().find(|c| s(c, "status") == "fail" && c.get("critical").and_then(|x| x.as_bool()) == Some(true)) {
                let cause = [s(&h, "reason"), s(c, "reason"), s(c, "key")].into_iter().find(|x| !x.is_empty()).unwrap();
                if !(mismatch && cause == "egress_mismatch") {
                    let mut l = node_labels(a);
                    l.insert("cause".into(), cause.clone());
                    l.insert("check".into(), s(c, "key"));
                    let since = f(c, "since").or_else(|| f(&h, "since"));
                    obs.push(Observation {
                        rule: "node_health_fail".into(),
                        labels: l.clone(),
                        severity: Severity::Critical,
                        summary: format!("{} {}失败：{}（入口 {}）", head_name(&name), s(c, "title"), cause, l.get("entry").cloned().unwrap_or_default()),
                        evidence: check_evidence(c),
                        active_since: since,
                        for_s: 0.0,
                        notify_resolved: true,
                    });
                    critical_nodes.push((node_labels(a), since.unwrap_or(now), cause));
                }
            }
        } else if status == "degraded" {
            if let Some(c) = checks.iter().find(|c| s(c, "status") == "degraded" && !["tcp", "latency"].contains(&s(c, "key").as_str())) {
                let key = s(c, "key");
                if !["cpu", "memory", "disk"].contains(&key.as_str()) {
                    let cause = [s(&h, "reason"), s(c, "reason")].into_iter().find(|x| !x.is_empty()).unwrap_or_else(|| format!("{key}_degraded"));
                    let mut l = node_labels(a);
                    l.insert("cause".into(), cause.clone());
                    l.insert("check".into(), key);
                    obs.push(Observation {
                        rule: "node_health_degraded".into(),
                        labels: l,
                        severity: Severity::Warning,
                        summary: format!("{} {}降级：{}", head_name(&name), s(c, "title"), cause),
                        evidence: check_evidence(c),
                        active_since: f(&h, "since").or_else(|| f(c, "since")),
                        for_s: cfg.degraded_for_s as f64,
                        notify_resolved: true,
                    });
                }
            }
        }
    }

    // ── 关联故障：同一线路机或同一供应商 ≥ N 个节点在窗口内相继故障 ──
    let window = cfg.correlate_window_s as f64;
    let min_n = cfg.correlate_min_nodes.max(2);
    let mut by_relay: BTreeMap<String, Vec<&(Labels, f64, String)>> = BTreeMap::new();
    let mut by_provider: BTreeMap<String, Vec<&(Labels, f64, String)>> = BTreeMap::new();
    let mut dedup = HashSet::new();
    for item in &critical_nodes {
        if !dedup.insert(item.0.get("node").cloned().unwrap_or_default()) {
            continue;
        }
        if let Some(r) = item.0.get("relay") {
            by_relay.entry(r.clone()).or_default().push(item);
        }
        if let Some(p) = item.0.get("provider") {
            by_provider.entry(p.clone()).or_default().push(item);
        }
    }
    let cluster = |v: &Vec<&(Labels, f64, String)>| -> Vec<(Labels, f64, String)> {
        let mut xs: Vec<&(Labels, f64, String)> = v.clone();
        xs.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let (mut best, mut bi, mut j) = (0usize, 0usize, 0usize);
        for i in 0..xs.len() {
            while xs[i].1 - xs[j].1 > window {
                j += 1;
            }
            if i + 1 - j > best {
                best = i + 1 - j;
                bi = j;
            }
        }
        xs[bi..bi + best].iter().map(|x| (*x).clone()).collect()
    };
    let push_corr = |obs: &mut Vec<Observation>, scope: &str, key: &str, members: Vec<(Labels, f64, String)>| {
        let mut l = Labels::new();
        l.insert("kind".into(), "correlated".into());
        l.insert("cause".into(), "correlated_node_failure".into());
        l.insert("scope".into(), scope.into());
        if scope == "relay" {
            l.insert("relay".into(), key.into());
            l.insert("relay_id".into(), short_id(key));
        } else {
            l.insert("provider".into(), key.into());
        }
        let first = members.iter().map(|m| m.1).fold(f64::INFINITY, f64::min);
        let names: Vec<String> = members.iter().map(|m| m.0.get("node").cloned().unwrap_or_default()).collect();
        obs.push(Observation {
            rule: "correlated_node_failure".into(),
            labels: l,
            severity: Severity::Critical,
            summary: format!("{}「{}」名下 {} 个节点在 {} 秒内相继故障：{}", if scope == "relay" { "线路机" } else { "供应商" }, key,
                members.len(), window as i64, names.join("、")),
            evidence: json!({"members": members.iter().map(|m| json!({"node": m.0.get("node"), "cause": m.2, "since": m.1,
                "relay": m.0.get("relay")})).collect::<Vec<_>>(), "window_s": window}),
            active_since: Some(first),
            for_s: 0.0,
            notify_resolved: true,
        });
    };
    let mut relay_clustered: HashSet<String> = HashSet::new();
    for (r, v) in &by_relay {
        let c = cluster(v);
        if c.len() >= min_n {
            for m in &c {
                relay_clustered.insert(m.0.get("node").cloned().unwrap_or_default());
            }
            push_corr(&mut obs, "relay", r, c);
        }
    }
    for (p, v) in &by_provider {
        let c = cluster(v);
        let relays: HashSet<String> = c.iter().filter_map(|m| m.0.get("relay").cloned()).collect();
        let fresh = c.iter().any(|m| !relay_clustered.contains(m.0.get("node").map(String::as_str).unwrap_or("")));
        if c.len() >= min_n && relays.len() >= 2 && fresh {
            push_corr(&mut obs, "provider", p, c);
        }
    }
    Derived { obs, scope: legacy_scope(), hold }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ids() {
        assert_eq!(short_id("团队1-01 · ATT · 75.37.237.38"), "t1-01");
        assert_eq!(short_id("线路机2"), "r2");
        assert_eq!(short_id("团队2-12"), "t2-12");
        assert_eq!(short_id("其他"), "其他");
    }

    #[test]
    fn calendar_days_in_billing_tz() {
        // 2026-09-23 07:22 UTC：北京 15:22（09-23），洛杉矶 00:22（09-23）
        let now = 1790148138.0;
        let a = json!({"due_date": "2026-10-01", "billing_timezone": "America/Los_Angeles"});
        assert_eq!(due_calendar_days(&a, now), Some(8));
        let b = json!({"due_date": "2026-10-13", "billing_timezone": "Asia/Shanghai"});
        assert_eq!(due_calendar_days(&b, now), Some(20));
        // 2026-09-23 16:30 UTC：北京已是 09-24，洛杉矶仍是 09-23
        let late = 1790181000.0;
        assert_eq!(due_calendar_days(&b, late), Some(19));
        assert_eq!(due_calendar_days(&a, late), Some(8));
        // 只有 expired_at
        let c = json!({"expired_at": "2026-10-13T00:00:00+08:00", "billing_timezone": "Asia/Shanghai"});
        assert_eq!(due_calendar_days(&c, now), Some(20));
        // 过期
        let d = json!({"due_date": "2026-09-20", "billing_timezone": "Asia/Shanghai"});
        assert_eq!(due_calendar_days(&d, now), Some(-3));
    }

    fn relay_world(t: f64, host_t: f64, hosts_at: f64, ov_now: f64) -> (Value, Value) {
        let ts = chrono::DateTime::from_timestamp(host_t as i64, 0).unwrap().to_rfc3339();
        let ov = json!({"now": ov_now, "hosts_at": hosts_at,
            "assets": [{"uuid": "r1u", "name": "线路机1", "team": "线路机", "host": {"online": true, "time": ts}}],
            "relays": {"线路机1": {"fresh": true, "received_at": ov_now - 5.0, "panel": {"ok": true, "xray_running": true}}}});
        let h = json!({"time": t, "collector_at": t - 5.0, "monitor_at": t - 10.0, "monitor": {"nodes": {}}});
        (ov, h)
    }

    fn rules_of(d: &Derived) -> Vec<String> {
        let mut v: Vec<String> = d.obs.iter().map(|o| o.rule.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn freshness_is_measured_at_render_time() {
        let cfg = AlertCfg::default();
        let t = 1_790_000_000.0;
        // 空闲时 overview 缓存 55 s 前取的：那一刻主机快照 25 s、主机上报 20 s——都新鲜，不能算停滞 / 离线
        let (ov, h) = relay_world(t, t - 75.0, t - 80.0, t - 55.0);
        let d = derive(&ov, &h, &PlanIndex::new(), &cfg, t);
        assert!(rules_of(&d).is_empty(), "{:?}", rules_of(&d));
        // 真停了：渲染时刻主机快照已 80 s 没刷新
        let (ov, h) = relay_world(t, t - 75.0, t - 80.0, t);
        let d = derive(&ov, &h, &PlanIndex::new(), &cfg, t);
        assert!(rules_of(&d).contains(&"host_sync_stale".to_string()), "{:?}", rules_of(&d));
        // 主机探针 90 s 没报（快照本身新鲜）→ 线路机主机离线
        let (ov, h) = relay_world(t, t - 90.0, t - 5.0, t);
        assert_eq!(rules_of(&derive(&ov, &h, &PlanIndex::new(), &cfg, t)), vec!["host_offline".to_string()]);
        // 健康评估停了：health 渲染时刻 monitor_at 已 70 s
        let (ov, mut h) = relay_world(t, t - 5.0, t - 5.0, t);
        h["monitor_at"] = json!(t - 70.0);
        assert_eq!(rules_of(&derive(&ov, &h, &PlanIndex::new(), &cfg, t)), vec!["monitor_stale".to_string()]);
    }

    #[test]
    fn startup_hold_window() {
        let t0 = 1_790_000_000.0;
        let (ov, h) = relay_world(t0 + 200.0, t0 + 190.0, t0 + 195.0, t0 + 200.0);
        let hold = |now: f64, ingest: Option<f64>, ov: &Value, h: &Value| startup_hold(t0, ingest, true, ov, h, now, 120.0, 600.0);
        assert!(hold(t0 + 60.0, Some(t0 + 30.0), &ov, &h), "宽限期内");
        assert!(hold(t0 + 200.0, None, &ov, &h), "还没收到边缘上报");
        assert!(!hold(t0 + 200.0, Some(t0 + 40.0), &ov, &h), "数据都是启动后的");
        let (old_ov, _) = relay_world(t0 + 200.0, t0 - 30.0, t0 - 20.0, t0 + 200.0);
        assert!(hold(t0 + 200.0, Some(t0 + 40.0), &old_ov, &h), "主机快照还是启动前的");
        let mut old_h = h.clone();
        old_h["collector_at"] = json!(t0 - 3.0);
        assert!(hold(t0 + 200.0, Some(t0 + 40.0), &ov, &old_h), "采集还没跑过");
        assert!(!hold(t0 + 601.0, None, &old_ov, &old_h), "最长 grace_max_s");
        assert!(!startup_hold(t0, None, false, &ov, &h, t0 + 200.0, 120.0, 600.0), "旧系统模式不等边缘上报");
    }

    /// 重启：已有的主机离线告警在宽限期里保持 firing（不恢复、不重发）；停滞类不新开；宽限结束后按真实数据恢复
    #[test]
    fn restart_keeps_alerts_and_opens_nothing_during_grace() {
        use crate::alerts::{Engine, EngineCfg, State};
        let cfg = AlertCfg::default();
        let t = 1_790_000_000.0;
        let mut e = Engine::new(EngineCfg::default(), t);
        // 重启前：线路机1 主机真离线，已 firing
        let (ov, h) = relay_world(t, t - 300.0, t - 5.0, t);
        let d = derive(&ov, &h, &PlanIndex::new(), &cfg, t);
        e.evaluate_held(t, d.obs, &d.scope, &d.hold);
        e.tick(t + 60.0);
        let firing = |e: &Engine| e.active().filter(|a| a.state == State::Firing).map(|a| a.rule.clone()).collect::<Vec<_>>();
        assert_eq!(firing(&e), vec!["host_offline".to_string()]);
        // 重启后 30 s：库里的时间戳全是重启前的（采集、评估、主机快照都旧），也还没有边缘上报
        let started = t + 100.0;
        let now = started + 30.0;
        let (mut ov, mut h) = relay_world(now, t - 300.0, t + 90.0, now);
        h["collector_at"] = json!(t + 95.0);
        h["monitor_at"] = json!(t + 70.0);
        ov["relays"]["线路机1"]["fresh"] = json!(false);
        let mut d = derive(&ov, &h, &PlanIndex::new(), &cfg, now);
        assert!(!d.obs.is_empty());
        assert!(startup_hold(started, None, true, &ov, &h, now, 120.0, 600.0));
        d.hold_startup();
        e.evaluate_held(now, d.obs, &d.scope, &d.hold);
        let out = e.tick(now + 60.0);
        assert!(out.is_empty(), "宽限期里不发任何通知：{:?}", out.iter().map(|o| &o.title).collect::<Vec<_>>());
        assert_eq!(firing(&e), vec!["host_offline".to_string()], "已有告警保持 firing，不因空窗恢复");
        assert_eq!(e.active().count(), 1, "停滞类不新开");
        // 宽限结束、数据都是启动后的、主机恢复上报 → 按真实数据恢复
        let now = started + 150.0;
        let (ov, h) = relay_world(now, now - 5.0, now - 3.0, now);
        assert!(!startup_hold(started, Some(started + 40.0), true, &ov, &h, now, 120.0, 600.0));
        let d = derive(&ov, &h, &PlanIndex::new(), &cfg, now);
        e.evaluate_held(now, d.obs, &d.scope, &d.hold);
        assert!(firing(&e).is_empty());
    }

    #[test]
    fn thresholds() {
        let t = [7, 3, 1];
        assert_eq!(renewal_threshold(8, &t), None);
        assert_eq!(renewal_threshold(7, &t), Some(7));
        assert_eq!(renewal_threshold(4, &t), Some(7));
        assert_eq!(renewal_threshold(3, &t), Some(3));
        assert_eq!(renewal_threshold(1, &t), Some(1));
        assert_eq!(renewal_threshold(-2, &t), Some(1));
        assert_eq!(quota_step(79.9, 80, 5), None);
        assert_eq!(quota_step(80.0, 80, 5), Some(80));
        assert_eq!(quota_step(84.99, 80, 5), Some(80));
        assert_eq!(quota_step(85.0, 80, 5), Some(85));
        assert_eq!(quota_step(103.0, 80, 5), Some(100));
    }
}
