//! 旧 `teamops.store` 的读路径：overview（资产汇总）、activity、history，以及直接由它派生的
//! export / public-status / panel/overview / fleet / provision/defaults / health。
//! 逐行对照 release 20260922T081813Z-providers 的 store.py / panel.py / fleet.py / server.py 移植；
//! 数字类型（int / float）、键顺序、None 的位置都与 Python 相同，输出经 pyjson 序列化后逐字节一致。
use super::py::*;
use super::Ctx;
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike};
use chrono_tz::Tz;
use serde_json::{json, Map, Value};

pub const RELAY_TEAM: &str = "线路机";

pub fn fresh_after(interval: f64) -> f64 {
    (3.0 * interval).max(120.0)
}

/// 北京时间当天 0 点、当月 1 日 0 点（float 时间戳）
pub fn start_times(now: f64) -> (f64, f64) {
    let tz: Tz = "Asia/Shanghai".parse().unwrap();
    let t = tz.timestamp_opt(now.floor() as i64, 0).single().unwrap();
    let day = t.with_hour(0).and_then(|x| x.with_minute(0)).and_then(|x| x.with_second(0)).unwrap();
    let month = day.with_day(1).unwrap();
    (day.timestamp() as f64, month.timestamp() as f64)
}

/// Python `datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()`；无时区按 UTC（中控时区即 UTC）
pub fn iso_ts(s: &str) -> Option<f64> {
    let s = s.replace('Z', "+00:00");
    if let Ok(d) = DateTime::parse_from_rfc3339(&s) {
        return Some(d.timestamp() as f64 + d.timestamp_subsec_micros() as f64 / 1e6);
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M%:z"] {
        if let Ok(d) = DateTime::parse_from_str(&s, fmt) {
            return Some(d.timestamp() as f64 + d.timestamp_subsec_micros() as f64 / 1e6);
        }
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(&s, fmt) {
            let u = d.and_utc();
            return Some(u.timestamp() as f64 + u.timestamp_subsec_micros() as f64 / 1e6);
        }
    }
    None
}

fn parse_aware(s: &str) -> Option<DateTime<chrono::FixedOffset>> {
    let s = s.replace('Z', "+00:00");
    DateTime::parse_from_rfc3339(&s).ok().or_else(|| DateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f%:z").ok())
}

/// store.due_calendar_days
pub fn due_calendar_days(a: &Value, now: f64) -> Value {
    let zone_name = match get(a, "billing_timezone") {
        v if truthy(v) => v.as_str().unwrap_or("").to_string(),
        _ => "Asia/Shanghai".into(),
    };
    let Ok(zone) = zone_name.parse::<Tz>() else { return Value::Null };
    let due = if truthy(get(a, "due_date")) {
        match get(a, "due_date").as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()) {
            Some(d) => d,
            None => return Value::Null,
        }
    } else {
        let Some(s) = a.get("expired_at").and_then(|v| v.as_str()) else { return Value::Null };
        match parse_aware(s) {
            Some(d) => d.with_timezone(&zone).date_naive(),
            None => return Value::Null,
        }
    };
    let Some(today) = zone.timestamp_opt(now.floor() as i64, 0).single().map(|t| t.date_naive()) else { return Value::Null };
    i((due - today).num_days())
}

/// store.capacity
pub fn capacity(a: &Value) -> Value {
    let mut value = get(a, "capacity_bps").clone();
    let mut source = "configured";
    if value.is_null() && get(a, "team") == RELAY_TEAM {
        value = i(1_000_000_000);
        source = "relay_default";
    }
    if value.is_null() {
        let bw = match a.get("bandwidth") {
            None => String::new(),
            Some(Value::Null) => "None".into(),
            Some(Value::String(s)) => s.clone(),
            Some(v) => super::pyjson::dumps(v),
        };
        if let Some((n, unit)) = parse_bandwidth(&bw) {
            value = f(n * if unit { 1_000_000_000.0 } else { 1_000_000.0 });
            source = "asset_bandwidth";
        }
    }
    let Some(v) = num(&value) else { return Value::Null };
    if !v.is_finite() || v <= 0.0 {
        return Value::Null;
    }
    let channel = get(a, "channel");
    let fresh = truthy(get(channel, "fresh"));
    let mut rates = vec![
        if fresh { get(channel, "rate_up").clone() } else { Value::Null },
        if fresh { get(channel, "rate_down").clone() } else { Value::Null },
    ];
    let mut measurement = "xray_inbound_payload";
    if get(a, "team") == RELAY_TEAM {
        let host = get(a, "host");
        let hf = truthy(get(a, "host_fresh"));
        rates = vec![
            if hf { get(host, "net_out").clone() } else { Value::Null },
            if hf { get(host, "net_in").clone() } else { Value::Null },
        ];
        measurement = "host_interface";
    }
    let pct: Vec<Value> = rates
        .iter()
        .map(|r| match r {
            Value::Number(n) => {
                let x = n.as_f64().unwrap_or(f64::NAN);
                if !x.is_finite() || x < 0.0 {
                    return Value::Null;
                }
                // rate*8*100/value，自左向右；int*8*100 仍是精确整数
                let top = match int_of(r) {
                    Some(k) => (k * 800) as f64,
                    None => x * 8.0 * 100.0,
                };
                f(super::pyjson::py_round(top / v, 3))
            }
            _ => Value::Null,
        })
        .collect();
    let util = if pct.iter().all(|p| !p.is_null()) { max2(pct[0].clone(), pct[1].clone()) } else { Value::Null };
    json!({"bps": value, "source": source, "measurement": measurement, "upload_percent": pct[0], "download_percent": pct[1], "utilization_percent": util})
}

/// `re.fullmatch(r"\s*(\d+(?:\.\d+)?)\s*(Mbps|Gbps)\s*", s, re.I)` → (数, 是否 Gbps)
fn parse_bandwidth(s: &str) -> Option<(f64, bool)> {
    let t = s.trim_matches(|c: char| c.is_whitespace());
    let low = t.to_ascii_lowercase();
    let (numpart, g) = if let Some(x) = low.strip_suffix("gbps") {
        (x, true)
    } else if let Some(x) = low.strip_suffix("mbps") {
        (x, false)
    } else {
        return None;
    };
    let numpart = numpart.trim_end_matches(|c: char| c.is_whitespace());
    let mut parts = numpart.splitn(2, '.');
    let a = parts.next()?;
    let b = parts.next();
    if a.is_empty() || !a.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if let Some(b) = b {
        if b.is_empty() || !b.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    numpart.parse::<f64>().ok().map(|n| (n, g))
}

/// store.aggregate_channels
fn aggregate_channels(chs: &[Map<String, Value>]) -> Map<String, Value> {
    let mut r = chs[0].clone();
    if chs.len() == 1 {
        return r;
    }
    r.insert("ports".into(), Value::Array(chs.iter().map(|c| get_m(c, "port").clone()).collect()));
    r.insert("channel_count".into(), i(chs.len() as i64));
    for field in ["rate_up", "rate_down", "counter_up", "counter_down", "up", "down", "total", "tcp_connections"] {
        let vals: Vec<&Value> = chs.iter().map(|c| get_m(c, field)).collect();
        let v = if vals.iter().all(|v| !v.is_null()) { sum(vals.into_iter()) } else { Value::Null };
        r.insert(field.into(), v);
    }
    r.insert("fresh".into(), Value::Bool(chs.iter().all(|c| truthy(get_m(c, "fresh")))));
    r.insert("stats_valid".into(), Value::Bool(chs.iter().all(|c| c.get("stats_valid").map(truthy).unwrap_or(true))));
    let mut sa = get_m(&chs[0], "sample_at").clone();
    for c in &chs[1..] {
        if cmp(get_m(c, "sample_at"), &sa) == std::cmp::Ordering::Less {
            sa = get_m(c, "sample_at").clone();
        }
    }
    r.insert("sample_at".into(), sa);
    let mut la = i(0);
    for c in chs {
        let v = get_m(c, "last_active");
        let v = if truthy(v) { v.clone() } else { i(0) };
        la = max2(la, v);
    }
    r.insert("last_active".into(), if truthy(&la) { la } else { Value::Null });
    let mut ips: Vec<String> = chs
        .iter()
        .flat_map(|c| get_m(c, "source_ips").as_array().cloned().unwrap_or_default())
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    ips.sort();
    ips.dedup();
    ips.truncate(100);
    r.insert("source_ips".into(), json!(ips));
    r.insert("sample_gap".into(), Value::Bool(chs.iter().any(|c| truthy(get_m(c, "sample_gap")))));
    r.insert("counter_reset".into(), Value::Bool(chs.iter().any(|c| truthy(get_m(c, "counter_reset")))));
    r
}

pub fn get_m<'a>(m: &'a Map<String, Value>, k: &str) -> &'a Value {
    static NULL: Value = Value::Null;
    m.get(k).unwrap_or(&NULL)
}

fn totals(cx: &Ctx, since: f64) -> R<Map<String, Value>> {
    let b = floor_div_int(&f(since), 3600) * 3600;
    if let Some(m) = &cx.cfg.memo {
        if let Some(v) = m.totals.lock().unwrap().get(&(b, m.gen())) {
            return Ok(v.clone());
        }
    }
    let out = totals_raw(cx, b)?;
    if let Some(m) = &cx.cfg.memo {
        let mut g = m.totals.lock().unwrap();
        if g.len() > 16 {
            g.clear();
        }
        g.insert((b, m.gen()), out.clone());
    }
    Ok(out)
}

fn totals_raw(cx: &Ctx, b: i64) -> R<Map<String, Value>> {
    let mut out = Map::new();
    for r in rows(cx.db, "SELECT asset,sum(up) up,sum(down) down FROM hours WHERE bucket>=? GROUP BY asset", &[si(b)])? {
        let a = get_m(&r, "asset").as_str().unwrap_or("").to_string();
        out.insert(a, json!({"up": r["up"], "down": r["down"]}));
    }
    Ok(out)
}

pub fn relay_fresh(r: &Value, now: f64) -> bool {
    let rec = num(&get_or(r, "received_at", i(0))).unwrap_or(0.0);
    let d = now - rec;
    -30.0 <= d && d < fresh_after(5.0) && !truthy(get(r, "error"))
}

pub fn raw_assets(cx: &Ctx) -> R<Vec<Value>> {
    let mut out = vec![];
    for r in rows(cx.db, "SELECT data FROM assets", &[])? {
        out.push(loads(&r["data"])?);
    }
    Ok(out)
}

/// store.overview()
pub fn overview(cx: &Ctx) -> R<Value> {
    if let Some(m) = &cx.cfg.memo {
        let dg = m.data_gen.load(std::sync::atomic::Ordering::Relaxed);
        if let Some((t, v)) = m.overview.lock().unwrap().as_ref() {
            // 2 s 内，或同一数据代内 15 s 内（推送轮询算过的一份，喂数里的健康评估接着用）
            if (cx.now - t).abs() < 2.0 || (dg > 0 && m.overview_gen() == dg && (cx.now - t).abs() < 15.0) {
                return Ok(v.clone());
            }
        }
        let v = overview_compute(cx)?;
        *m.overview.lock().unwrap() = Some((cx.now, v.clone()));
        m.set_overview_gen(dg);
        return Ok(v);
    }
    overview_compute(cx)
}

fn overview_compute(cx: &Ctx) -> R<Value> {
    let prof = std::env::var("FLEETD_PROF").is_ok();
    let t0 = std::time::Instant::now();
    let lap = |name: &str| if prof { eprintln!("  overview {name}: {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0) };
    let now = cx.now;
    let (day, month) = start_times(now);
    let daily = totals(cx, day)?;
    let monthly = totals(cx, month)?;
    let mut relays = Map::new();
    for r in rows(cx.db, "SELECT * FROM relays", &[])? {
        relays.insert(get_m(&r, "relay").as_str().unwrap_or("").to_string(), loads(&r["data"])?);
    }
    // uuid → 入口列表（保持首次出现顺序）
    let mut channels: Vec<(String, Vec<Map<String, Value>>)> = vec![];
    for (relay, value) in &relays {
        let fresh = relay_fresh(value, now);
        for inbound in get(value, "inbounds").as_array().cloned().unwrap_or_default() {
            let mut m = inbound.as_object().cloned().unwrap_or_default();
            m.insert("relay".into(), json!(relay));
            m.insert("fresh".into(), Value::Bool(fresh));
            m.insert("sample_at".into(), get(value, "ts").clone());
            let u = get(&inbound, "uuid").as_str().unwrap_or("").to_string();
            match channels.iter_mut().find(|(k, _)| *k == u) {
                Some((_, v)) => v.push(m),
                None => channels.push((u, vec![m])),
            }
        }
    }
    lap("relays+channels");
    let inbounds: Vec<(String, Map<String, Value>)> = channels.iter().map(|(u, rs)| (u.clone(), aggregate_channels(rs))).collect();
    let hosts = meta(cx.db, "hosts", json!({}))?;
    let host_at = meta(cx.db, "hosts_at", i(0))?;
    let htq = meta(cx.db, "host_traffic_quality", json!({}))?;
    let policies = meta(cx.db, "applied_policies", json!({}))?;
    lap("meta hosts etc");
    let mut assets = raw_assets(cx)?;
    lap("assets");
    let mut probes = Map::new();
    let mut paths = Map::new();
    for r in relays.values() {
        for p in get(get(r, "probe"), "assets").as_array().cloned().unwrap_or_default() {
            probes.insert(get(&p, "uuid").as_str().unwrap_or("").to_string(), p);
        }
        for p in get(get(r, "paths"), "assets").as_array().cloned().unwrap_or_default() {
            paths.insert(get(&p, "uuid").as_str().unwrap_or("").to_string(), p);
        }
    }
    for r in relays.values_mut() {
        let fr = relay_fresh(r, now);
        if let Some(m) = r.as_object_mut() {
            m.insert("fresh".into(), Value::Bool(fr));
        }
    }
    let monitor = meta(cx.db, "monitor", json!({}))?;
    let zero = || json!({"up": 0, "down": 0});
    let mut groups = Map::new();
    let host_at_f = num(&host_at).unwrap_or(0.0);
    for a in assets.iter_mut() {
        let uuid = get(a, "uuid").as_str().unwrap_or("").to_string();
        let m = a.as_object_mut().unwrap();
        m.insert("daily".into(), daily.get(&uuid).cloned().unwrap_or_else(zero));
        m.insert("monthly".into(), monthly.get(&uuid).cloned().unwrap_or_else(zero));
        m.insert("channel".into(), inbounds.iter().find(|(k, _)| *k == uuid).map(|(_, v)| Value::Object(v.clone())).unwrap_or(Value::Null));
        m.insert("probe".into(), probes.get(&uuid).cloned().unwrap_or(Value::Null));
        m.insert("path".into(), paths.get(&uuid).cloned().unwrap_or(Value::Null));
        m.insert("host".into(), get(&hosts, &uuid).clone());
        if let Some(q) = htq.get(&uuid) {
            m.insert("host_traffic_quality".into(), q.clone());
        }
        let host = m["host"].clone();
        let measured = truthy(&host) && cmp(&get_or(&host, "ram_total", i(0)), &i(0)) == std::cmp::Ordering::Greater;
        m.insert("host_measured".into(), Value::Bool(measured));
        let stamp = match get(&host, "time") {
            Value::String(s) => iso_ts(s).unwrap_or(0.0),
            _ => 0.0,
        };
        let fa = fresh_after(5.0);
        let hf = (-30.0 <= now - host_at_f && now - host_at_f < fa) && (-30.0 <= now - stamp && now - stamp < fa);
        m.insert("host_fresh".into(), Value::Bool(hf));
        let cap = capacity(a);
        let m = a.as_object_mut().unwrap();
        m.insert("capacity".into(), cap);
        let node = get(get(&monitor, "nodes"), &uuid);
        let health = if node.is_null() {
            Value::Null
        } else {
            let checks = get_or(node, "checks", json!([]));
            let mut counts = Map::new();
            for s in ["ok", "degraded", "fail", "unknown", "na"] {
                let n = checks.as_array().map(|v| v.iter().filter(|c| get(c, "status") == s).count()).unwrap_or(0);
                counts.insert(s.into(), i(n as i64));
            }
            json!({"status": get(node, "status"), "reason": get(node, "reason"), "since": get(node, "since"), "summary_counts": counts})
        };
        m.insert("health".into(), health);
        let pol = super::policy::effective_policy(a, &policies);
        let dd = due_calendar_days(a, now);
        let m = a.as_object_mut().unwrap();
        m.insert("policy".into(), pol);
        m.insert("due_days".into(), dd);
        let team = get(a, "team").as_str().unwrap_or("").to_string();
        let g = groups.entry(team).or_insert_with(|| {
            json!({"count": 0, "monthly_cents": 0, "rate_up": null, "rate_down": null, "daily_bytes": 0, "monthly_bytes": 0, "active": 0,
                   "rate_expected": 0, "rate_sampled": 0, "rate_complete": false, "partial_rate_up": 0, "partial_rate_down": 0})
        });
        let gm = g.as_object_mut().unwrap();
        let inc = |gm: &mut Map<String, Value>, k: &str, v: &Value| {
            let n = add(&gm[k], v);
            gm.insert(k.into(), n);
        };
        inc(gm, "count", &i(1));
        inc(gm, "rate_expected", &i(1));
        inc(gm, "monthly_cents", get(a, "price_cents"));
        inc(gm, "daily_bytes", &sum(get(a, "daily").as_object().map(|o| o.values().collect::<Vec<_>>()).unwrap_or_default()));
        inc(gm, "monthly_bytes", &sum(get(a, "monthly").as_object().map(|o| o.values().collect::<Vec<_>>()).unwrap_or_default()));
        let ch = get(a, "channel");
        if truthy(get(ch, "fresh")) && !get(ch, "rate_up").is_null() && !get(ch, "rate_down").is_null() {
            inc(gm, "rate_sampled", &i(1));
            inc(gm, "partial_rate_up", get(ch, "rate_up"));
            inc(gm, "partial_rate_down", get(ch, "rate_down"));
            let tot = num(&add(get(ch, "rate_up"), get(ch, "rate_down"))).unwrap_or(0.0);
            inc(gm, "active", &i((tot >= 1024.0) as i64));
        }
    }
    for g in groups.values_mut() {
        let gm = g.as_object_mut().unwrap();
        let complete = gm["rate_sampled"] == gm["rate_expected"];
        gm.insert("rate_complete".into(), Value::Bool(complete));
        if complete {
            let (u, d) = (gm["partial_rate_up"].clone(), gm["partial_rate_down"].clone());
            gm.insert("rate_up".into(), u);
            gm.insert("rate_down".into(), d);
        }
    }
    lap("per-asset loop");
    let mut direct = vec![];
    for (u, ib) in &inbounds {
        if u.starts_with("direct:") {
            let mut m = ib.clone();
            m.insert("daily".into(), daily.get(u).cloned().unwrap_or_else(zero));
            m.insert("monthly".into(), monthly.get(u).cloned().unwrap_or_else(zero));
            direct.push(Value::Object(m));
        }
    }
    let events_today = scalar(cx.db, "SELECT count(*) FROM activity WHERE ts>=?", &[sf(day)])?;
    lap("events_today");
    let monthly_cents = sum(assets.iter().map(|a| get(a, "price_cents")).collect::<Vec<_>>());
    let mut out = Map::new();
    out.insert("now".into(), f(now));
    out.insert("started_at".into(), meta(cx.db, "started_at", Value::Null)?);
    out.insert("assets".into(), Value::Array(assets));
    out.insert("groups".into(), Value::Object(groups));
    out.insert("relays".into(), Value::Object(relays));
    out.insert("direct".into(), Value::Array(direct));
    out.insert("monthly_cents".into(), monthly_cents);
    out.insert("events_today".into(), events_today);
    out.insert("retention".into(), json!({"activity_days": 14, "minute_days": 35, "hour_days": 400}));
    out.insert("collection_interval".into(), i(10));
    out.insert("applied_policies".into(), policies);
    out.insert("hosts_at".into(), host_at);
    out.insert("collector".into(), meta(cx.db, "collector", json!({}))?);
    out.insert("backup".into(), meta(cx.db, "backup", json!({}))?);
    out.insert("monitor_started_at".into(), meta(cx.db, "monitor_started_at", Value::Null)?);
    Ok(Value::Object(out))
}

fn b64url(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut s = String::new();
    for ch in data.chunks(3) {
        let b = [ch[0], *ch.get(1).unwrap_or(&0), *ch.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for k in 0..4 {
            if k <= ch.len() {
                s.push(T[((n >> (18 - 6 * k)) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// Python `base64.urlsafe_b64decode`（非严格：丢弃字母表外字符，要求补齐）
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut vals = vec![];
    let mut pad = 0;
    for c in s.chars() {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '-' | '+' => 62,
            '_' | '/' => 63,
            '=' => {
                pad += 1;
                continue;
            }
            _ => continue,
        };
        if pad > 0 {
            return None;
        }
        vals.push(v);
    }
    if (vals.len() + pad) % 4 != 0 && vals.len() % 4 != 0 {
        return None;
    }
    if vals.len() % 4 == 1 {
        return None;
    }
    let mut out = vec![];
    for ch in vals.chunks(4) {
        let mut n = 0u32;
        for (k, v) in ch.iter().enumerate() {
            n |= v << (18 - 6 * k);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..ch.len() - 1]);
    }
    Some(out)
}

const ACT_FIELDS: [&str; 7] = ["asset", "team", "action", "source_ip", "host", "kind", "protocol"];

/// store.activity(filters)；`limit_override` 供 /node 内部调用（Python 传入 int 15）
pub fn activity(cx: &Ctx, p: &Params) -> R<Value> {
    let seconds = p_int(p, "seconds", 86400)?;
    let mut conds: Vec<String> = vec!["ts>=?".into()];
    let mut args = vec![sf(cx.now - (14 * 86400).min(60.max(seconds)) as f64)];
    for field in ACT_FIELDS {
        if let Some(v) = p_truthy(p, field) {
            conds.push(format!("{field}=?"));
            args.push(st(v));
        }
    }
    if let Some(s) = p_truthy(p, "search") {
        conds.push("(host LIKE ? OR client LIKE ? OR source_ip LIKE ?)".into());
        let pat = format!("%{}%", head(s, 200));
        for _ in 0..3 {
            args.push(st(&pat));
        }
    }
    let wh = conds.join(" AND ");
    let limit = 500.min(1.max(p_int(p, "limit", 100)?));
    let mut paging = String::new();
    let mut pargs = vec![];
    if let Some(b) = p_truthy(p, "before") {
        paging = " AND ts<?".into();
        pargs = vec![sf(py_float(b)?)];
    }
    if let Some(c) = p_truthy(p, "cursor") {
        let bad = || Err::Bad("分页位置无效，请返回最新记录".into());
        let raw = b64url_decode(c).ok_or_else(bad)?;
        let v: Value = serde_json::from_slice(&raw).map_err(|_| bad())?;
        let arr = v.as_array().filter(|a| a.len() == 2).ok_or_else(bad)?;
        let stamp = match &arr[0] {
            Value::Number(n) => n.as_f64().ok_or_else(bad)?,
            Value::String(s) => py_float(s).map_err(|_| bad())?,
            Value::Bool(b) => *b as i64 as f64,
            _ => return Err(bad()),
        };
        let ident = arr[1].as_str().filter(|s| s.chars().count() <= 300).ok_or_else(bad)?;
        paging = " AND (ts<? OR (ts=? AND id<?))".into();
        pargs = vec![sf(stamp), sf(stamp), st(ident)];
    }
    let mut a2 = args.clone();
    a2.extend(pargs);
    a2.push(si(limit));
    let rs = rows(cx.db, &format!("SELECT * FROM activity WHERE {wh}{paging} ORDER BY ts DESC,id DESC LIMIT ?"), &a2)?;
    let filtered = ACT_FIELDS.iter().any(|k| p.contains_key(*k)) || p.contains_key("search");
    let mut out = Map::new();
    let last = rs.last().cloned();
    let full = rs.len() as i64 == limit;
    out.insert("rows".into(), Value::Array(rs.into_iter().map(Value::Object).collect()));
    if !filtered && seconds == 86400 {
        let agg = match super::behavior::activity_window(cx) {
            Some(r) => r?,
            None => match super::behavior::cached(cx, "activity")? {
                Some(v) => v,
                None => super::behavior::activity_aggregates(cx)?,
            },
        };
        for (k, v) in agg.as_object().cloned().unwrap_or_default() {
            out.insert(k, v);
        }
    } else if let Some(r) = activity_rollup(cx, p, seconds) {
        // 长窗口（> 24 h）：小时汇总
        let (domains, sources, total) = r?;
        out.insert("domains".into(), Value::Array(domains));
        out.insert("sources".into(), Value::Array(sources));
        out.insert("total".into(), i(total));
    } else if let Some(r) = activity_filtered_window(cx, p, seconds) {
        // 默认 24 h、过滤条件是等值（asset/team/action/kind/source_ip/host）：内存窗口里算，不扫表
        let (domains, sources, total) = r?;
        out.insert("domains".into(), Value::Array(domains));
        out.insert("sources".into(), Value::Array(sources));
        out.insert("total".into(), i(total));
    } else {
        let domains = rows(cx.db, &format!("SELECT host,kind,count(*) connections,count(DISTINCT source_ip) sources,max(ts) last_seen,sum(action='blocked') blocked FROM activity INDEXED BY activity_time WHERE {wh} GROUP BY host,kind ORDER BY connections DESC LIMIT 40"), &args)?;
        let sources = rows(cx.db, &format!("SELECT source_ip,count(*) connections,count(DISTINCT host) domains,max(ts) last_seen FROM activity WHERE {wh} GROUP BY source_ip ORDER BY connections DESC LIMIT 30"), &args)?;
        let total = scalar(cx.db, &format!("SELECT count(*) FROM activity WHERE {wh}"), &args)?;
        out.insert("domains".into(), Value::Array(domains.into_iter().map(Value::Object).collect()));
        out.insert("sources".into(), Value::Array(sources.into_iter().map(Value::Object).collect()));
        out.insert("total".into(), total);
    }
    let (nb, nc) = match (&last, full) {
        (Some(l), true) => {
            let cur = super::pyjson::dumps(&json!([l["ts"], l["id"]]));
            (l["ts"].clone(), Value::String(b64url(cur.as_bytes())))
        }
        _ => (Value::Null, Value::Null),
    };
    out.insert("next_before".into(), nb);
    out.insert("next_cursor".into(), nc);
    Ok(Value::Object(out))
}

/// 带过滤的 /activity 聚合走内存窗口（只在 seconds=86400、没有 search / protocol、action 为 blocked|routed 时）
/// /activity 长窗口（> 24 h，最多 14 天）且只有等值过滤（不含 action / protocol / search）：compat::rollup
pub fn activity_rollup(cx: &Ctx, p: &Params, seconds: i64) -> Option<R<(Vec<Value>, Vec<Value>, i64)>> {
    let memo = cx.cfg.memo.as_ref()?;
    let seconds = (14 * 86400).min(60.max(seconds));
    if seconds <= 86400 || p_truthy(p, "search").is_some() || p_truthy(p, "protocol").is_some() || p_truthy(p, "action").is_some() {
        return None;
    }
    let f = super::window::ActFilter {
        asset: p_truthy(p, "asset").map(String::from),
        team: p_truthy(p, "team").map(String::from),
        kind: p_truthy(p, "kind").map(String::from),
        source_ip: p_truthy(p, "source_ip").map(String::from),
        host: p_truthy(p, "host").map(String::from),
        blocked: None,
    };
    memo.with_rollup(cx.db, cx.now, |r| r.activity_parts(cx.db, cx.now, seconds, &f))
}

pub fn activity_filtered_window(cx: &Ctx, p: &Params, seconds: i64) -> Option<R<(Vec<Value>, Vec<Value>, i64)>> {
    let memo = cx.cfg.memo.as_ref()?;
    if seconds != 86400 || p_truthy(p, "search").is_some() || p_truthy(p, "protocol").is_some() {
        return None;
    }
    let blocked = match p_truthy(p, "action") {
        None => None,
        Some("blocked") => Some(true),
        Some("routed") => Some(false),
        Some(_) => return None,
    };
    let f = super::window::ActFilter {
        asset: p_truthy(p, "asset").map(String::from),
        team: p_truthy(p, "team").map(String::from),
        kind: p_truthy(p, "kind").map(String::from),
        source_ip: p_truthy(p, "source_ip").map(String::from),
        host: p_truthy(p, "host").map(String::from),
        blocked,
    };
    memo.with_window(cx.db, cx.now, |w| Ok(w.activity_filtered(&f)))
}

/// store.history(asset, hours)
pub fn history(cx: &Ctx, asset: Option<&str>, hours: i64) -> R<Value> {
    let hours = (24 * 35).min(1.max(hours));
    let start = cx.now - (hours * 3600) as f64;
    let a = asset.filter(|s| !s.is_empty());
    let g: i64 = if hours <= 24 { 300 } else { 3600 };
    let q = |lo: Option<f64>, from: Option<i64>, below: Option<i64>| -> R<Vec<Value>> {
        let mut wh = vec![];
        let mut args = vec![];
        if let Some(x) = lo {
            wh.push("bucket>=?");
            args.push(sf(x));
        }
        if let Some(x) = from {
            wh.push("bucket>=?");
            args.push(si(x));
        }
        if let Some(x) = below {
            wh.push("bucket<?");
            args.push(si(x));
        }
        if let Some(x) = a {
            wh.push("asset=?");
            args.push(st(x));
        }
        Ok(rows(cx.db, &format!("SELECT cast(bucket/{g} as integer)*{g} t,sum(up) up,sum(down) down FROM minutes WHERE {} GROUP BY t ORDER BY t", wh.join(" AND ")), &args)?
            .into_iter()
            .map(Value::Object)
            .collect())
    };
    let Some(m) = &cx.cfg.memo else { return Ok(Value::Array(q(Some(start), None, None)?)) };
    // 同一分钟起点、账本没变 → 原样返回
    let exact = (a.unwrap_or("").to_string(), hours, (start / 60.0).ceil() as i64, m.gen());
    if let Some(v) = m.history.lock().unwrap().get(&exact) {
        return Ok(v.clone());
    }
    // 增量：账本每 5 分钟重写最近 30 分钟，更早的分钟不再变。缓存 40 分钟以前的整桶，只重算“起点所在的桶”和“上次算时还没定稿的桶之后”
    let t_head = ((start.ceil() as i64) / g) * g;
    let new_tail = ((cx.now - 2400.0) / g as f64).floor() as i64 * g;
    let key = (a.unwrap_or("").to_string(), hours);
    let cached = m.history_inc.lock().unwrap().get(&key).cloned();
    let out: Vec<Value> = match cached {
        Some((old_tail, at, rows_c)) if cx.now - at < 1800.0 && old_tail.min(new_tail) > t_head => {
            let from = old_tail.min(new_tail);
            let mut v = q(Some(start), None, Some(t_head + g))?;
            v.extend(rows_c.iter().filter(|r| r["t"].as_i64().is_some_and(|t| t > t_head && t < from)).cloned());
            v.extend(q(None, Some(from), None)?);
            v
        }
        _ => q(Some(start), None, None)?,
    };
    {
        let mut h = m.history_inc.lock().unwrap();
        if h.len() > 256 {
            h.clear();
        }
        h.insert(key, (new_tail, cx.now, out.clone()));
    }
    let v = Value::Array(out);
    let mut hm = m.history.lock().unwrap();
    if hm.len() > 64 {
        hm.clear();
    }
    hm.insert(exact, v.clone());
    Ok(v)
}

pub fn health(cx: &Ctx) -> R<Value> {
    let collector = meta(cx.db, "collector", json!({}))?;
    let monitor = meta(cx.db, "monitor", json!({}))?;
    let c_at = num(&get_or(&collector, "checked_at", i(0))).unwrap_or(0.0);
    let m_at = num(&get_or(&monitor, "checked_at", i(0))).unwrap_or(0.0);
    let ok = cx.now - c_at < 30.0 && cx.now - m_at < 40.0;
    Ok(json!({"version": "1.2.0", "ok": ok, "collector_at": get(&collector, "checked_at"), "monitor_at": get(&monitor, "checked_at"),
        "monitor": monitor, "time": f(cx.now)}))
}

pub fn public_status(cx: &Ctx) -> R<Value> {
    let ov = overview(cx)?;
    let mut out = vec![];
    for a in get(&ov, "assets").as_array().cloned().unwrap_or_default() {
        let h = get(&a, "health");
        let relay_node = get(&a, "team") == RELAY_TEAM;
        let hm = truthy(get(&a, "host_measured"));
        let online = hm && truthy(get(&a, "host_fresh")) && truthy(get(get(&a, "host"), "online"));
        out.push(json!({"uuid": a["uuid"], "host_measured": a["host_measured"], "host_online": online, "relay_node": relay_node,
            "link_healthy": if truthy(h) && !relay_node { Value::Bool(get(h, "status") == "ok") } else { Value::Null },
            "link_status": if truthy(h) { get(h, "status").clone() } else { Value::Null },
            "reason": if truthy(h) { get(h, "reason").clone() } else { Value::Null },
            "checked_at": get(get(&a, "probe"), "tcp_at")}));
    }
    Ok(Value::Array(out))
}

pub fn export(cx: &Ctx) -> R<Value> {
    let assets = raw_assets(cx)?;
    let ov = overview(cx)?;
    Ok(json!({"exported_at": f(cx.now), "assets": assets, "summary": ov["groups"]}))
}

/// panel.panel_overview
pub fn panel_overview(cx: &Ctx, relay: Option<&str>) -> R<Value> {
    let ov = overview(cx)?;
    let assets = get(&ov, "assets").as_array().cloned().unwrap_or_default();
    // (relay, relay_port) → 资产；后出现的覆盖先出现的（dict 推导式语义）
    let by_key = |name: &str, port: &Value| -> Option<Value> {
        assets.iter().rev().find(|a| get(a, "relay").as_str() == Some(name) && py_eq(get(a, "relay_port"), port)).cloned()
    };
    let mut relays = Map::new();
    for (name, r) in get(&ov, "relays").as_object().cloned().unwrap_or_default() {
        if relay.is_some_and(|x| x != name) {
            continue;
        }
        let panel = match get(&r, "panel") {
            v if truthy(v) => v.clone(),
            _ => json!({"ok": false, "error": "面板数据未采集"}),
        };
        let mut row = Map::new();
        for k in ["ok", "error", "version", "xray_version", "xray_running", "cpu", "mem_used", "mem_total", "checked_at"] {
            row.insert(k.into(), get(&panel, k).clone());
        }
        let onlines = match get(&panel, "onlines") {
            v if truthy(v) => v.as_array().cloned().unwrap_or_default(),
            _ => vec![],
        };
        row.insert("onlines_count".into(), i(onlines.len() as i64));
        row.insert("fresh".into(), r["fresh"].clone());
        let mut ibs = vec![];
        for ib in get(&r, "inbounds").as_array().cloned().unwrap_or_default() {
            let a = if get(&ib, "kind") == "master" {
                assets.iter().find(|a| get(a, "team") == RELAY_TEAM && get(a, "name").as_str() == Some(name.as_str())).cloned()
            } else {
                by_key(&name, get(&ib, "port"))
            };
            let clients = get(&ib, "clients");
            let email = if truthy(clients) { get(&clients[0], "label").clone() } else { Value::Null };
            let online = onlines.iter().any(|o| *o == email);
            let master = get(&ib, "kind") == "master";
            let (au, an, at) = match &a {
                Some(a) => (a["uuid"].clone(), a["name"].clone(), a["team"].clone()),
                None => (Value::Null, Value::Null, Value::Null),
            };
            let ee = match &a {
                Some(a) if master => a["ip"].clone(),
                Some(a) => get(get(a, "probe"), "expected_ip").clone(),
                None => Value::Null,
            };
            let ea = match &a {
                Some(a) if !master => get(get(a, "probe"), "actual_ip").clone(),
                _ => Value::Null,
            };
            let ph = match &a {
                Some(a) if !master => get(get(a, "path"), "healthy").clone(),
                _ => Value::Null,
            };
            let mut m = Map::new();
            for (k, src) in [("kind", "kind"), ("id", "id"), ("port", "port"), ("protocol", "inbound_protocol"), ("security", "security"),
                ("client_uuid", "client_uuid"), ("reality", "reality"), ("source_ips", "source_ips"), ("outbound_protocol", "protocol"),
                ("remark", "remark"), ("tag", "tag"), ("enable", "enable")] {
                m.insert(k.into(), ib.get(src).cloned().ok_or_else(|| Err::NotFound)?);
            }
            m.insert("email".into(), email);
            m.insert("online".into(), Value::Bool(online));
            for k in ["last_active", "tcp_connections", "up", "down", "total"] {
                m.insert(k.into(), ib.get(k).cloned().ok_or(Err::NotFound)?);
            }
            m.insert("asset_uuid".into(), au);
            m.insert("asset_name".into(), an);
            m.insert("team".into(), at);
            for k in ["outbound_tag", "target", "target_port"] {
                m.insert(k.into(), ib.get(k).cloned().ok_or(Err::NotFound)?);
            }
            m.insert("egress_expected".into(), ee);
            m.insert("egress_actual".into(), ea);
            m.insert("path_healthy".into(), ph);
            ibs.push(Value::Object(m));
        }
        row.insert("inbounds".into(), Value::Array(ibs));
        relays.insert(name, Value::Object(row));
    }
    Ok(json!({"relays": relays}))
}

/// Python `==`（数字按数值、其余按值）
pub fn py_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => cmp(a, b) == std::cmp::Ordering::Equal,
        (Value::Bool(x), Value::Number(_)) | (Value::Number(_), Value::Bool(x)) => {
            let n = if let Value::Number(_) = a { a } else { b };
            num(n) == Some(*x as i64 as f64)
        }
        _ => a == b,
    }
}

fn fleet_configs(cx: &Ctx) -> R<Map<String, Value>> {
    let has = scalar(cx.db, "SELECT count(*) FROM sqlite_master WHERE name='fleet'", &[])?;
    let mut out = Map::new();
    if int_of(&has).unwrap_or(0) > 0 {
        for r in rows(cx.db, "SELECT * FROM fleet", &[])? {
            out.insert(get_m(&r, "relay").as_str().unwrap_or("").to_string(), loads(&r["data"])?);
        }
    }
    Ok(out)
}

/// fleet.relays()：配置里的线路机 + fleet 表里已启用的
pub fn fleet_relays(cx: &Ctx) -> R<Vec<String>> {
    let mut names: Vec<String> = cx.cfg.config_relays.clone();
    for (k, v) in fleet_configs(cx)? {
        if truthy(get(&v, "active")) && !names.contains(&k) {
            names.push(k);
        }
    }
    Ok(names)
}

/// fleet.state()
pub fn fleet_state(cx: &Ctx) -> R<Value> {
    let ov = overview(cx)?;
    let additions = fleet_configs(cx)?;
    let mut names: Vec<String> = cx.cfg.config_relays.clone();
    for k in additions.keys() {
        if !names.contains(k) {
            names.push(k.clone());
        }
    }
    let ovr = get(&ov, "relays");
    let mut result = vec![];
    for name in &names {
        let r = additions.get(name).cloned().unwrap_or(json!({}));
        let snap = get(ovr, name).clone();
        result.push(json!({"name": name, "ip": get(&r, "ip"), "active": get_or(&r, "active", Value::Bool(true)),
            "fingerprint": get(&r, "fingerprint"), "fresh": get_or(&snap, "fresh", Value::Bool(false)),
            "uuid": get(&r, "uuid"), "error": get(&snap, "error")}));
    }
    let assets = raw_assets(cx)?;
    let mut entries = vec![];
    for (name, r) in ovr.as_object().cloned().unwrap_or_default() {
        for ib in get_or(&r, "inbounds", json!([])).as_array().cloned().unwrap_or_default() {
            let mut m = Map::new();
            m.insert("relay".into(), json!(name));
            for k in ["port", "remark", "tag", "outbound_tag", "target", "target_port", "protocol", "supported"] {
                m.insert(k.into(), get(&ib, k).clone());
            }
            let port = &ib["port"];
            let assigned = assets.iter().any(|a| get(a, "relay").as_str() == Some(name.as_str()) && py_eq(get(a, "relay_port"), port));
            m.insert("assigned".into(), Value::Bool(assigned));
            m.insert("fresh".into(), get_or(&r, "fresh", Value::Bool(false)));
            entries.push(Value::Object(m));
        }
    }
    Ok(json!({"relays": result, "entries": entries}))
}

/// provision.defaults()
pub fn provision_defaults(cx: &Ctx) -> R<Value> {
    let relays = fleet_relays(cx)?;
    let ov = overview(cx)?;
    let mut next = Map::new();
    for relay in &relays {
        let mut mx = i(20000);
        for ib in get(get(get(&ov, "relays"), relay), "inbounds").as_array().cloned().unwrap_or_default() {
            let p = get(&ib, "port");
            if let Some(x) = num(p) {
                if (20001.0..21000.0).contains(&x) {
                    mx = max2(mx, p.clone());
                }
            }
        }
        next.insert(relay.clone(), add(&mx, &i(1)));
    }
    let mut teams: Vec<String> = raw_assets(cx)?
        .iter()
        .filter_map(|a| a["team"].as_str().map(String::from))
        .filter(|t| t != RELAY_TEAM)
        .collect();
    teams.sort();
    teams.dedup();
    Ok(json!({"relay_names": relays, "public_key": cx.cfg.public_key.trim(), "next_ports": next, "teams": teams,
        "providers": ["ATTVPS", "NodeMach", "OhnoICU"], "panel_version": "3.7.0", "packages": cx.cfg.packages}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn b64() {
        assert_eq!(b64url(b"[1.5,\"a\"]"), "WzEuNSwiYSJd");
        assert_eq!(b64url(b"ab"), "YWI=");
        assert_eq!(b64url_decode("YWI=").unwrap(), b"ab");
        assert_eq!(b64url_decode("WzEuNSwiYSJd").unwrap(), b"[1.5,\"a\"]");
        assert!(b64url_decode("YWI").is_none());
    }
    #[test]
    fn bandwidth() {
        assert_eq!(parse_bandwidth(" 50 Mbps "), Some((50.0, false)));
        assert_eq!(parse_bandwidth("1.5gbps"), Some((1.5, true)));
        assert_eq!(parse_bandwidth("50Mbit"), None);
        assert_eq!(parse_bandwidth("None"), None);
    }
    #[test]
    fn days() {
        let a = json!({"due_date": "2026-10-13", "billing_timezone": "Asia/Shanghai"});
        // 2026-09-24 00:30 北京时间
        let now = 1790181000.0;
        assert_eq!(due_calendar_days(&a, now), json!(19));
    }
}

#[cfg(test)]
mod history_inc_tests {
    use super::*;
    #[test]
    fn splice_equals_full() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE minutes(asset TEXT,bucket INTEGER,up INTEGER,down INTEGER,PRIMARY KEY(asset,bucket)); CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);").unwrap();
        let t0: i64 = 1_790_000_000 / 60 * 60;
        let put = |b: i64, up: i64| {
            c.execute("INSERT OR REPLACE INTO minutes VALUES('a',?1,?2,1)", rusqlite::params![b, up]).unwrap();
        };
        for k in 0..(26 * 60) {
            put(t0 - k * 60, k);
        }
        let memo = std::sync::Arc::new(super::super::Memo::default());
        let mut cfg = super::super::CompatCfg::default();
        cfg.memo = Some(memo.clone());
        let plain = super::super::CompatCfg::default();
        for (step, hours) in [(0.0, 24), (0.0, 30), (700.0, 24), (700.0, 30), (1500.0, 24), (4000.0, 30), (4000.0, 1)] {
            let now = t0 as f64 + 30.5 + step;
            // 最近 30 分钟有改写
            put(t0 - 120, 999 + step as i64);
            put((now as i64) / 60 * 60, 7);
            memo.bump();
            let a = history(&Ctx { db: &c, now, cfg: &cfg }, Some("a"), hours).unwrap();
            let b = history(&Ctx { db: &c, now, cfg: &plain }, Some("a"), hours).unwrap();
            assert_eq!(a, b, "step {step} hours {hours}");
        }
    }
}
