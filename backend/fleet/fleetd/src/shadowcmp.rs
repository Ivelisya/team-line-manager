//! 下线闸门：原生旧库渲染的旧接口 vs 旧系统实时响应，逐路径、逐字段比较（API.md §12）。
//!
//! 每轮（默认 60 s）取一组旧接口路径：旧系统经只读适配器取最新响应；原生版在 legacy.db 上渲染，时间取旧响应里的
//! `now`/`time`（对齐以“现在”为基准的计算）。差异逐项归类：
//! - **已解释**：采样时刻（*_at、ts、now…）、计数/速率在不同时刻采样（up/down/rate/cpu…）、数据源换代
//!   （x-ui 停用后旧 panel、bridge 游标、core_id…）、列表在时间窗/截断边界上的成员差异；
//! - **未解释**：其余一切（名称、状态、归属、结构、配置值……）。
//! 闸门：最近 30 分钟每一轮都 0 个未解释差异，且轮数足够（≥ 25）。
use crate::app::App;
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::sync::Mutex;

pub const WINDOW_S: f64 = 1800.0;
pub const MIN_ROUNDS: usize = 25;

/// 固定路径（节点详情每轮换一个资产）
pub const PATHS: &[&str] = &[
    "/overview", "/health", "/history?hours=24", "/incidents?status=open", "/incidents", "/renewals", "/audit", "/jobs",
    "/behavior", "/activity?limit=100", "/panel/overview", "/fleet", "/public-status", "/policies", "/export", "/schema",
    "/schema.md", "/api-keys", "/api-keys/usage", "/provision/defaults",
];

const TIME_KEYS: &[&str] = &[
    "now", "time", "ts", "received_at", "sample_at", "checked_at", "measured_at", "updated_at", "computed_at", "exported_at",
    "last_seen", "last_active", "hosts_at", "collector_at", "monitor_at", "failed_at", "first_seen", "first_in_window",
    "verified_at", "captured_at", "tcp_at", "egress_at", "since", "last", "bucket", "opened_at", "resolved_at",
];
const VOLUME_KEYS: &[&str] = &[
    "up", "down", "total", "counter_up", "counter_down", "rate_up", "rate_down", "partial_rate_up", "partial_rate_down",
    "daily_bytes", "monthly_bytes", "tcp_connections", "connections", "connections_udp", "cpu", "ram", "swap", "disk", "net_in",
    "net_out", "net_total_up", "net_total_down", "load", "load5", "load15", "process", "uptime", "mem_used", "upload_percent",
    "download_percent", "utilization_percent", "events_today", "blocked", "sources", "domains", "targets", "total_recorded",
    "oldest_wait_seconds", "latest", "avg", "min", "max", "tail", "loss", "active", "rate_sampled", "value", "successful_samples",
    "failed_samples", "unknown_samples", "missing_samples", "maintenance_samples", "availability_percent", "coverage_percent",
    "tcp_ms", "egress_ms", "path_ms", "ingress_ms", "ms", "ip_only", "count", "coverage", "incidents", "size_bytes", "memory",
    "rate_complete", "ok", "unknown", "failed", "maintenance", "use_count", "disk_total", "ram_total", "swap_total",
];
/// 这些时刻两边必须接近（≤ 300 s）：差得更多说明某一边的喂数停了
const STRICT_TIME: &[&str] = &["now", "time", "checked_at", "received_at", "hosts_at", "collector_at", "monitor_at", "computed_at", "exported_at", "sample_at", "updated_at"];
/// 测量输入的时刻：一条记录（带 uuid/key 的对象）里这些值两边不同，说明两边判定用的是不同的测量
const STAMP_KEYS: &[&str] = &["measured_at", "tcp_at", "egress_at", "time", "sample_at", "checked_at", "last"];
/// 由测量推出的结论字段：所在记录被重新测量过时，这些字段的差异算“采样时刻不同”
const VERDICT_KEYS: &[&str] = &[
    "status", "reason", "since", "availability", "summary_counts", "healthy", "evidence", "attempts", "actual_ip", "rc", "phase", "pending",
    "online", "host_online", "link_healthy", "link_status", "host_fresh", "host_measured", "fresh", "egress_error", "tcp_error", "error",
    "egress_rc", "tcp_rc", "egress_phase", "tcp_phase", "egress_attempt", "attempt", "ingress_error", "egress_pending", "path_healthy",
    "egress_actual", "rate_up", "rate_down", "health",
];
fn stamps(v: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > 3 {
        return;
    }
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if STAMP_KEYS.contains(&k.as_str()) && !x.is_object() && !x.is_array() {
                    out.push(format!("{k}={x}"));
                } else {
                    stamps(x, depth + 1, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| stamps(x, depth + 1, out)),
        _ => {}
    }
}
fn resampled(a: &Value, b: &Value) -> bool {
    let (mut x, mut y) = (vec![], vec![]);
    stamps(a, 0, &mut x);
    stamps(b, 0, &mut y);
    x.sort();
    y.sort();
    x != y
}
/// 由逐节点结论汇总出来的列表/计数（节点本身逐个比较，这里的差异随节点差异一起解释）
const DERIVED_KEYS: &[&str] = &["unknown", "attention", "counts"];
/// 分页/截断游标：两边最新一条不同是时间窗边界
const WINDOW_KEYS: &[&str] = &["next_before", "next_cursor", "next_offset"];
/// 按路径追加的“数据源换代”字段（/panel/overview 把旧 x-ui 面板状态平铺在线路机一行里）
fn path_source_keys(path: &str) -> &'static [&'static str] {
    if path.starts_with("/panel/overview") {
        &["ok", "error", "version", "xray_version", "xray_running", "cpu", "mem_used", "mem_total", "checked_at", "onlines_count"]
    } else {
        &[]
    }
}
const SOURCE_KEYS: &[&str] = &[
    "core_id", "cursor", "log_size_bytes", "log_backlog_bytes", "panel", "source_ips", "onlines", "onlines_count", "duration_seconds",
    "backup", "xray_version", "xray_running", "mem_total", "version", "email", "online", "sample_gap", "stats_valid", "counter_reset",
    "file", "integrity", "offsite", "scope", "collector",
];

#[derive(Clone, Debug, serde::Serialize)]
pub struct Diff {
    pub at: String,
    pub legacy: Value,
    pub native: Value,
    pub class: &'static str,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct PathResult {
    pub path: String,
    pub legacy_status: u16,
    pub native_status: u16,
    pub identical_bytes: bool,
    pub explained: usize,
    pub unexplained: usize,
    pub by_class: std::collections::BTreeMap<&'static str, usize>,
    pub samples: Vec<Diff>,
    pub native_ms: f64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Round {
    pub at: f64,
    pub paths: Vec<PathResult>,
    pub unexplained: usize,
    pub explained: usize,
    pub duration_ms: f64,
}

#[derive(Default)]
pub struct ShadowCmp {
    pub rounds: Mutex<VecDeque<Round>>,
    pub node_cursor: Mutex<usize>,
}

fn trunc(v: &Value) -> Value {
    let s = v.to_string();
    if s.len() > 300 {
        Value::String(format!("{}…（{} 字节）", s.chars().take(200).collect::<String>(), s.len()))
    } else {
        v.clone()
    }
}

struct Ctx<'a> {
    out: &'a mut PathResult,
    extra_source: &'static [&'static str],
    /// 当前所在记录两边的测量输入不同
    remeasured: bool,
    /// 这一轮的“现在”（旧响应里的 now/time；没有时为 0，退回按对方时间范围判断边界）
    now: f64,
}

impl Ctx<'_> {
    fn push(&mut self, at: &str, l: &Value, n: &Value, class: &'static str) {
        *self.out.by_class.entry(class).or_default() += 1;
        if class == "unexplained" {
            self.out.unexplained += 1;
        } else {
            self.out.explained += 1;
        }
        // 未解释的全留（最多 60 条），已解释的只留前 10 条做样例
        let keep = if class == "unexplained" { self.out.samples.iter().filter(|d| d.class == "unexplained").count() < 60 } else { self.out.samples.len() < 10 };
        if keep {
            self.out.samples.push(Diff { at: at.to_string(), legacy: trunc(l), native: trunc(n), class });
        }
    }
}

fn num_eq(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y || (x - y).abs() <= 1e-9 * x.abs().max(y.abs()),
        _ => false,
    }
}

fn classify_key(key: &str, l: &Value, n: &Value) -> Option<&'static str> {
    let both_num_or_null = |a: &Value, b: &Value| (a.is_number() || a.is_null()) && (b.is_number() || b.is_null());
    if WINDOW_KEYS.contains(&key) {
        return Some("window_edge");
    }
    if DERIVED_KEYS.contains(&key) && (l.is_array() || l.is_object()) && (n.is_array() || n.is_object()) {
        return Some("sample_time");
    }
    if TIME_KEYS.contains(&key) || key.ends_with("_at") {
        if STRICT_TIME.contains(&key) {
            if let (Some(a), Some(b)) = (l.as_f64(), n.as_f64()) {
                return ((a - b).abs() <= 300.0).then_some("sample_time");
            }
        }
        if both_num_or_null(l, n) || (l.is_string() && n.is_string()) {
            return Some("sample_time");
        }
    }
    if VOLUME_KEYS.contains(&key) && (both_num_or_null(l, n) || (l.is_boolean() && n.is_boolean())) {
        return Some("sampled_volume");
    }
    if SOURCE_KEYS.contains(&key) {
        return Some("source_changed");
    }
    None
}

/// 用哪个字段把数组元素对齐
fn identity_key(arr: &[Value]) -> Option<&'static str> {
    let first = arr.first()?.as_object()?;
    ["id", "uuid", "key", "t", "bucket", "scope", "name", "relay", "host", "source_ip", "asset", "path", "action", "port", "tag"]
        .into_iter()
        .find(|k| {
            if !first.contains_key(*k) || !arr.iter().all(|x| x.get(*k).is_some()) {
                return false;
            }
            // 必须在数组里唯一，否则按下标比
            let mut seen = std::collections::HashSet::new();
            arr.iter().all(|x| seen.insert(ident(x, k)))
        })
}

fn ident(v: &Value, k: &str) -> String {
    let base = v[k].to_string();
    // 行为/活动聚合：同一 host 在不同资产下各一行
    match (k, v.get("asset"), v.get("kind")) {
        ("host", Some(a), _) | ("source_ip", Some(a), _) => format!("{a}|{base}"),
        ("host", None, Some(kd)) => format!("{kd}|{base}"),
        _ => base,
    }
}

fn walk(cx: &mut Ctx, at: &str, key: &str, l: &Value, n: &Value, list_limit_hit: bool) {
    match (l, n) {
        (Value::Object(a), Value::Object(b)) => {
            if SOURCE_KEYS.contains(&key) && a != b {
                cx.push(at, l, n, "source_changed");
                return;
            }
            let saved = cx.remeasured;
            if a.contains_key("uuid") || a.contains_key("key") {
                cx.remeasured = saved || resampled(l, n);
            }
            walk_obj(cx, at, a, b);
            cx.remeasured = saved;
        }
        (Value::Array(a), Value::Array(b)) => walk_arr(cx, at, key, l, n, a, b, list_limit_hit),
        _ => {
            let same = l == n || num_eq(l, n);
            // 只有旧系统出错（采集超时、快照过期）而原生正常：旧系统自身的故障，不是移植差异
            let legacy_fault = (key == "error" && !l.is_null() && n.is_null()) || (key == "fresh" && l == &Value::Bool(false) && n == &Value::Bool(true));
            if !same && legacy_fault {
                cx.push(at, l, n, "legacy_fault");
            } else if !same {
                let c = if cx.extra_source.contains(&key) {
                    "source_changed"
                } else {
                    match classify_key(key, l, n) {
                        Some(c) => c,
                        None if cx.remeasured && VERDICT_KEYS.contains(&key) => "sample_time",
                        None => "unexplained",
                    }
                };
                cx.push(at, l, n, c);
            }
        }
    }
}

fn walk_obj(cx: &mut Ctx, at: &str, a: &Map<String, Value>, b: &Map<String, Value>) {
    {
        {
            for (k, lv) in a {
                let p = format!("{at}/{k}");
                match b.get(k) {
                    Some(nv) => walk(cx, &p, k, lv, nv, false),
                    None => cx.push(&p, lv, &Value::Null, if classify_key(k, lv, &Value::Null).is_some() { "source_changed" } else { "unexplained" }),
                }
            }
            for (k, nv) in b {
                if !a.contains_key(k) {
                    let p = format!("{at}/{k}");
                    cx.push(&p, &Value::Null, nv, if classify_key(k, &Value::Null, nv).is_some() { "source_changed" } else { "unexplained" });
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_arr(cx: &mut Ctx, at: &str, key: &str, l: &Value, n: &Value, a: &[Value], b: &[Value], list_limit_hit: bool) {
    {
        {
            if SOURCE_KEYS.contains(&key) {
                if a != b {
                    cx.push(at, l, n, "source_changed");
                }
                return;
            }
            if DERIVED_KEYS.contains(&key) || (cx.remeasured && VERDICT_KEYS.contains(&key)) {
                // 汇总列表，或所在记录两边用的是不同测量时的结论列表（如 attempts）
                if a != b {
                    cx.push(at, l, n, "sample_time");
                }
                return;
            }
            match identity_key(if a.is_empty() { b } else { a }) {
                Some(k) => {
                    let limit_hit = list_limit_hit || [40usize, 30, 100, 150, 200, 250, 500, 60, 15, 10].contains(&a.len().max(b.len()));
                    // 时间窗：两边各自的最新/最旧时刻（行里有 ts / t / opened_at）
                    let tkey = ["ts", "t", "opened_at", "bucket", "last_seen"].into_iter().find(|t| a.first().or(b.first()).is_some_and(|x| x.get(*t).is_some_and(|v| v.is_number())));
                    let span = |xs: &[Value]| -> Option<(f64, f64)> {
                        let t = tkey?;
                        let v: Vec<f64> = xs.iter().filter_map(|x| x[t].as_f64()).collect();
                        (!v.is_empty()).then(|| (v.iter().cloned().fold(f64::MAX, f64::min), v.iter().cloned().fold(f64::MIN, f64::max)))
                    };
                    let (sa, sb) = (span(a), span(b));
                    let bi: std::collections::HashMap<String, &Value> = b.iter().map(|x| (ident(x, k), x)).collect();
                    let ai: std::collections::HashMap<String, &Value> = a.iter().map(|x| (ident(x, k), x)).collect();
                    let now = cx.now;
                    // 边界：比对方最旧的还旧（截断），或刚刚发生（两边采样时刻不同）；持续存在的差异不算边界
                    let edge = |x: &Value, other: Option<(f64, f64)>| -> bool {
                        match (tkey, other) {
                            (Some(t), Some((lo, hi))) => x[t].as_f64().is_some_and(|v| v <= lo + 120.0 || if now > 0.0 { v >= now - 180.0 } else { v >= hi - 120.0 }),
                            (Some(_), None) => true,
                            _ => false,
                        }
                    };
                    let mut ua: Vec<&Value> = vec![];
                    for x in a {
                        let id = ident(x, k);
                        let p = format!("{at}[{k}={}]", id.trim_matches('"'));
                        match bi.get(&id) {
                            Some(y) => walk(cx, &p, key, x, y, false),
                            None => ua.push(x),
                        }
                    }
                    let mut ub: Vec<&Value> = b.iter().filter(|y| !ai.contains_key(&ident(y, k))).collect();
                    // 两边各自产生的记录（快照之后的故障、审计、行为提示）自增 id 不同：按内容 + 时间（±300 s）配对
                    if k == "id" {
                        let sig = |x: &Value| -> Option<(String, f64)> {
                            let parts: Vec<String> = ["asset", "key", "kind", "subject", "action", "target", "actor", "window"]
                                .iter()
                                .filter_map(|f| x.get(*f).map(|v| format!("{f}={v}")))
                                .collect();
                            let t = x.get("opened_at").or_else(|| x.get("ts")).and_then(|v| v.as_f64())?;
                            // 只配对“事件”类记录（故障 key、审计 target、行为提示 subject）；访问记录按 id 严格对齐
                            let eventish = ["key", "subject", "target"].iter().any(|f| x.get(*f).is_some()) && x.get("source_ip").is_none();
                            (eventish && parts.len() >= 2).then(|| (parts.join("|"), t))
                        };
                        let mut i = 0;
                        while i < ua.len() {
                            let Some((sx, tx)) = sig(ua[i]) else {
                                i += 1;
                                continue;
                            };
                            let best = ub.iter().enumerate().filter_map(|(j, y)| sig(y).filter(|(sy, ty)| *sy == sx && (ty - tx).abs() <= 300.0).map(|(_, ty)| (j, (ty - tx).abs())))
                                .min_by(|p, q| p.1.partial_cmp(&q.1).unwrap_or(std::cmp::Ordering::Equal)).map(|(j, _)| j);
                            if let Some(j) = best {
                                let (mut x2, mut y2) = (ua[i].clone(), ub[j].clone());
                                let p = format!("{at}[{sx}]");
                                cx.push(&format!("{p}/id"), &x2["id"], &y2["id"], "renumbered");
                                x2.as_object_mut().map(|m| m.shift_remove("id"));
                                y2.as_object_mut().map(|m| m.shift_remove("id"));
                                let saved = cx.remeasured;
                                cx.remeasured = true; // 两边各自评估出来的同一事件：时刻、证据值是各自的测量
                                walk(cx, &p, key, &x2, &y2, false);
                                cx.remeasured = saved;
                                ua.remove(i);
                                ub.remove(j);
                            } else {
                                i += 1;
                            }
                        }
                    }
                    for x in ua {
                        let p = format!("{at}[{k}={}]", ident(x, k).trim_matches('"'));
                        cx.push(&p, x, &Value::Null, if limit_hit || edge(x, sb) { "window_edge" } else { "unexplained" });
                    }
                    for y in ub {
                        let p = format!("{at}[{k}={}]", ident(y, k).trim_matches('"'));
                        cx.push(&p, &Value::Null, y, if limit_hit || edge(y, sa) { "window_edge" } else { "unexplained" });
                    }
                }
                None => {
                    if a.len() != b.len() {
                        let c = classify_key(key, l, n).unwrap_or("unexplained");
                        cx.push(at, l, n, if a.iter().chain(b.iter()).all(|x| x.is_number()) && c == "unexplained" { "sampled_volume" } else { c });
                        return;
                    }
                    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
                        walk(cx, &format!("{at}[{i}]"), key, x, y, false);
                    }
                }
            }
        }
    }
}

pub fn compare(path: &str, legacy: &Value, native: &Value) -> PathResult {
    let mut r = PathResult {
        path: path.into(),
        legacy_status: 200,
        native_status: 200,
        identical_bytes: false,
        explained: 0,
        unexplained: 0,
        by_class: Default::default(),
        samples: vec![],
        native_ms: 0.0,
        error: None,
    };
    let now = legacy["data"]["now"].as_f64().or_else(|| legacy["data"]["time"].as_f64()).unwrap_or(0.0);
    let mut cx = Ctx { out: &mut r, extra_source: path_source_keys(path), remeasured: false, now };
    walk(&mut cx, "", "", legacy, native, false);
    r
}

fn now_of(v: &Value) -> Option<f64> {
    let d = &v["data"];
    d["now"].as_f64().or_else(|| d["time"].as_f64()).or_else(|| d["exported_at"].as_f64())
}

/// 一轮比较
pub async fn round(app: &App, ls: &std::sync::Arc<crate::legacystore::LegacyStore>, cmp: &ShadowCmp) -> Round {
    let t0 = std::time::Instant::now();
    let at = app.clock.now();
    let mut paths: Vec<String> = PATHS.iter().map(|s| s.to_string()).collect();
    // 节点详情：每轮换一个资产
    if let Some(ov) = app.overview_data().await {
        if let Some(arr) = ov["assets"].as_array() {
            if !arr.is_empty() {
                let mut i = cmp.node_cursor.lock().unwrap();
                *i = (*i + 1) % arr.len();
                if let Some(u) = arr[*i]["uuid"].as_str() {
                    paths.push(format!("/node?asset={u}"));
                }
            }
        }
    }
    let mut out = vec![];
    for p in paths {
        let (path, q) = p.split_once('?').map(|(a, b)| (a.to_string(), Some(b.to_string()))).unwrap_or((p.clone(), None));
        let Some(key) = crate::legacy::normalize(&path, q.as_deref()) else { continue };
        let (r, _) = app.legacy.get(&key, 5.0).await;
        let (lstatus, lbody) = match r {
            Ok(e) => (e.status, e.body.clone()),
            Err(e) => {
                out.push(PathResult { path: p.clone(), legacy_status: 0, native_status: 0, identical_bytes: false, explained: 0, unexplained: 0,
                    by_class: Default::default(), samples: vec![], native_ms: 0.0, error: Some(format!("旧系统：{e}")) });
                continue;
            }
        };
        let lv: Value = serde_json::from_slice(&lbody).unwrap_or(Value::Null);
        let now = now_of(&lv).unwrap_or_else(|| app.clock.now());
        let t1 = std::time::Instant::now();
        let p2 = p.clone();
        let ls2 = ls.clone();
        let native = tokio::task::spawn_blocking(move || ls2.render(&p2, now)).await.ok().flatten();
        let native_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let Some((nstatus, nbody)) = native else {
            out.push(PathResult { path: p.clone(), legacy_status: lstatus, native_status: 0, identical_bytes: false, explained: 0, unexplained: 1,
                by_class: Default::default(), samples: vec![], native_ms, error: Some("原生旧库不可用".into()) });
            continue;
        };
        let nv: Value = serde_json::from_str(&nbody).unwrap_or(Value::Null);
        let same = lbody.as_ref() == nbody.as_bytes();
        let mut r = if same { compare(&p, &Value::Null, &Value::Null) } else { compare(&p, &lv, &nv) };
        r.legacy_status = lstatus;
        r.native_status = nstatus;
        r.identical_bytes = lbody.as_ref() == nbody.as_bytes();
        r.native_ms = (native_ms * 10.0).round() / 10.0;
        if lstatus != nstatus {
            r.unexplained += 1;
            *r.by_class.entry("unexplained").or_default() += 1;
        }
        out.push(r);
    }
    let unexplained = out.iter().map(|r| r.unexplained).sum();
    let explained = out.iter().map(|r| r.explained).sum();
    Round { at, paths: out, unexplained, explained, duration_ms: t0.elapsed().as_secs_f64() * 1000.0 }
}

pub async fn run(app: std::sync::Arc<App>, ls: std::sync::Arc<crate::legacystore::LegacyStore>, cmp: std::sync::Arc<ShadowCmp>, interval_s: u64) {
    let mut iv = tokio::time::interval(std::time::Duration::from_secs(interval_s.max(20)));
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        iv.tick().await;
        if !ls.exists() || !ls.status.lock().unwrap().imported {
            continue;
        }
        let r = round(&app, &ls, &cmp).await;
        let mut q = cmp.rounds.lock().unwrap();
        q.push_back(r);
        while q.len() > 90 {
            q.pop_front();
        }
    }
}

/// 汇总：闸门状态 + 最近一轮（每路径）+ 窗口内出现过的未解释差异（去重，最多 100 条）
pub fn summary(cmp: &ShadowCmp, now: f64) -> Value {
    let q = cmp.rounds.lock().unwrap();
    let win: Vec<&Round> = q.iter().filter(|r| now - r.at <= WINDOW_S).collect();
    let bad: usize = win.iter().map(|r| r.unexplained).sum();
    let clean_since = q.iter().rev().take_while(|r| r.unexplained == 0).last().map(|r| r.at);
    // 连续干净的时长必须覆盖整个窗口（重启后比较器从零开始，需重新攒满 30 分钟）
    let passing = win.len() >= MIN_ROUNDS && bad == 0 && clean_since.is_some_and(|t| now - t >= WINDOW_S - 90.0);
    let mut seen = std::collections::BTreeMap::new();
    for r in &win {
        for p in &r.paths {
            for d in p.samples.iter().filter(|d| d.class == "unexplained") {
                let k = format!("{} {}", p.path.split('?').next().unwrap_or(""), d.at);
                seen.entry(k).or_insert_with(|| json!({"path": p.path, "at": d.at, "legacy": d.legacy, "native": d.native, "round_at": r.at}));
            }
        }
    }
    let last = q.back().map(|r| {
        json!({"at": r.at, "duration_ms": r.duration_ms.round(), "unexplained": r.unexplained, "explained": r.explained,
            "paths": r.paths.iter().map(|p| json!({"path": p.path, "status": [p.legacy_status, p.native_status], "identical_bytes": p.identical_bytes,
                "unexplained": p.unexplained, "explained": p.explained, "by_class": p.by_class, "native_ms": p.native_ms, "error": p.error,
                "examples": p.samples.iter().take(12).collect::<Vec<_>>()})).collect::<Vec<_>>()})
    });
    let mut m = Map::new();
    m.insert("gate".into(), json!({"window_s": WINDOW_S, "min_rounds": MIN_ROUNDS, "rounds_in_window": win.len(), "unexplained_in_window": bad,
        "passing": passing, "clean_since": clean_since,
        "clean_for_s": clean_since.map(|t| (now - t).round()),
        "rule": "连续 30 分钟每轮 0 个未解释差异（且至少 25 轮）才可下线旧系统"}));
    m.insert("classes".into(), json!({"sample_time": "采样时刻不同（两边在不同时刻取样）", "sampled_volume": "计数/速率/资源占用在不同时刻采样",
        "source_changed": "数据源换代（x-ui 停用后旧 panel 恒为错误、bridge 游标 → relayd、core_id 格式等）",
        "window_edge": "列表在时间窗或条数截断边界上的成员差异", "renumbered": "两边各自产生的同一事件（故障/审计/提示），自增 id 不同", "legacy_fault": "只有旧系统报错或数据过期（原生正常）", "unexplained": "其余一切：必须为 0"}));
    m.insert("last_round".into(), last.unwrap_or(Value::Null));
    m.insert("unexplained_in_window".into(), Value::Array(seen.into_values().take(100).collect()));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classes() {
        let l = json!({"data": {"now": 1.0, "assets": [{"uuid": "a", "name": "x", "channel": {"rate_up": 1.0, "up": 5}, "team": "t"}], "panel": {"ok": false}}});
        let n = json!({"data": {"now": 2.0, "assets": [{"uuid": "a", "name": "y", "channel": {"rate_up": 2.0, "up": 6}, "team": "t"}], "panel": {"ok": true}}});
        let r = compare("/x", &l, &n);
        assert_eq!(r.unexplained, 1, "{:?}", r.samples);
        assert_eq!(r.by_class.get("sample_time"), Some(&1));
        assert_eq!(r.by_class.get("sampled_volume"), Some(&2));
        assert_eq!(r.by_class.get("source_changed"), Some(&1));
        let rows_l = json!({"rows": [{"id": "a", "ts": 1000.0}, {"id": "b", "ts": 500.0}, {"id": "c", "ts": 10.0}]});
        let rows_n = json!({"rows": [{"id": "a", "ts": 1000.0}, {"id": "c", "ts": 10.0}]});
        let r = compare("/activity", &rows_l, &rows_n);
        assert_eq!(r.unexplained, 1);
        let old_row = json!({"id": "r0", "ts": 10.0, "asset": "u", "kind": "domain", "action": "routed", "host": "o.com", "source_ip": "1.1.1.1"});
        let act_l = json!({"data": {"now": 5000.0, "rows": [{"id": "r1", "ts": 1000.0, "asset": "u", "kind": "domain", "action": "routed", "host": "a.com", "source_ip": "1.1.1.1"}, old_row]}});
        let act_n = json!({"data": {"now": 5000.0, "rows": [{"id": "r2", "ts": 1001.0, "asset": "u", "kind": "domain", "action": "routed", "host": "b.com", "source_ip": "1.1.1.1"}, old_row]}});
        assert_eq!(compare("/activity", &act_l, &act_n).unexplained, 2, "访问记录不按内容配对");
        let rows_n2 = json!({"rows": [{"id": "z", "ts": 1001.0}, {"id": "a", "ts": 1000.0}, {"id": "b", "ts": 500.0}, {"id": "c", "ts": 10.0}]});
        let r = compare("/activity", &rows_l, &rows_n2);
        assert_eq!(r.unexplained, 0);
        assert_eq!(r.by_class.get("window_edge"), Some(&1));
        // 同一节点：两边判定用的测量不同 → 结论差异算采样时刻；测量相同而结论不同 → 未解释
        let l = json!({"uuid": "n", "status": "ok", "name": "x", "checks": [{"key": "egress", "status": "ok", "measured_at": 10.0}]});
        let n = json!({"uuid": "n", "status": "fail", "name": "x", "checks": [{"key": "egress", "status": "fail", "measured_at": 20.0}]});
        assert_eq!(compare("/health", &l, &n).unexplained, 0);
        // 两边各自开的同一故障：id 不同、时间差几秒 → 配对；只有一边有、已不是刚刚发生 → 未解释
        let li = json!({"data": {"now": 10000.0, "items": [{"id": 90, "asset": "u", "key": "egress", "opened_at": 9000.0, "status": "fail"}]}});
        let ni = json!({"data": {"now": 10000.0, "items": [{"id": 91, "asset": "u", "key": "egress", "opened_at": 9004.0, "status": "fail"}]}});
        let r = compare("/incidents", &li, &ni);
        assert_eq!(r.unexplained, 0, "{:?}", r.samples);
        assert_eq!(r.by_class.get("renumbered"), Some(&1));
        let ni2 = json!({"data": {"now": 10000.0, "items": [{"id": 91, "asset": "u", "key": "tcp", "opened_at": 9004.0, "status": "fail"}, {"id": 92, "asset": "v", "key": "x", "opened_at": 1.0}]}});
        let li2 = json!({"data": {"now": 10000.0, "items": [{"id": 92, "asset": "v", "key": "x", "opened_at": 1.0}]}});
        assert_eq!(compare("/incidents", &li2, &ni2).unexplained, 1);
        let la = json!({"uuid": "n", "path": {"measured_at": 1.0, "attempts": [{"rc": 28}, {"rc": 0}]}});
        let na = json!({"uuid": "n", "path": {"measured_at": 2.0, "attempts": [{"rc": 0}]}});
        assert_eq!(compare("/overview", &la, &na).unexplained, 0);
        let n2 = json!({"uuid": "n", "status": "fail", "name": "x", "checks": [{"key": "egress", "status": "fail", "measured_at": 10.0}]});
        assert_eq!(compare("/health", &l, &n2).unexplained, 2);
        let n3 = json!({"uuid": "n", "status": "ok", "name": "y", "checks": [{"key": "egress", "status": "ok", "measured_at": 20.0}]});
        assert_eq!(compare("/health", &l, &n3).unexplained, 1, "名称差异不因重新测量而被解释");
        let l = json!({"monitor": {"unknown": ["a", "b"], "attention": ["x"]}});
        let n = json!({"monitor": {"unknown": [], "attention": ["y"]}});
        assert_eq!(compare("/health", &l, &n).unexplained, 0);
    }
}
