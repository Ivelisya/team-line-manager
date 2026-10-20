//! 旧 `/komari/*` 读接口的原生等价（没有 Komari 管理员会话时用：API 密钥、fleetd 账户，或以后 Komari 退役）。
//! 形状按 Komari 1.5（见 staging/komari_mirror.py 的抓包说明），数据来自 fleetd 自己：
//! - `komari/hosts` → 原生旧库的 hosts（nodeagent / relayd，见 legacystore）；
//! - `komari/hosts/<uuid>/metrics?range=1h|4h|1d` → nodeagent 的 host_1m（≤4 h 按分钟，否则 5 分钟汇总）；
//! - `komari/pings`、`komari/pings/<id>/results` → 资产上的 tcp_task_id / egress_task_id 对应 relayd 的 TCP / 出口验证探测；
//! - `komari/clients` → 资产清单（没有 token）。
//! 与 Komari 原值的差异是测量来源不同（nodeagent 替代 komari-agent、relayd 探测替代 Komari ping 任务），字段与类型一致。
use crate::app::App;
use crate::compat::py;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

fn go_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default()
}

fn assets(app: &App) -> Vec<Value> {
    app.legacy_store.reader().map(|c| crate::compat::store::raw_assets(&crate::compat::Ctx { db: &c, now: 0.0, cfg: &Default::default() }).unwrap_or_default()).unwrap_or_default()
}

fn hosts_meta(app: &App) -> Value {
    app.legacy_store.reader().and_then(|c| py::meta(&c, "hosts", json!({})).ok()).unwrap_or(json!({}))
}

fn host_id_of(app: &App, a: &Value) -> Option<String> {
    let name = a["name"].as_str()?;
    if a["team"] == "线路机" {
        app.cfg.edge.relay_hosts.iter().find(|(_, n)| n.as_str() == name).map(|(h, _)| h.clone())
    } else {
        Some(name.split(" · ").next().unwrap_or("").to_string())
    }
}

fn num(part: usize, whole: usize) -> Value {
    if whole == 0 {
        return json!(0);
    }
    let v = crate::compat::pyjson::py_round(part as f64 * 100.0 / whole as f64, 2);
    if v == v.trunc() { json!(v as i64) } else { json!(v) }
}

const ORDER: [&str; 21] = ["client", "time", "cpu", "gpu", "ram", "ram_total", "swap", "swap_total", "load", "temp", "disk", "disk_total", "net_in", "net_out",
    "net_total_up", "net_total_down", "traffic_up", "traffic_down", "process", "connections", "connections_udp"];

fn records(app: &App, uuid: &str, hours: f64, now: f64) -> Result<Value, String> {
    let a = assets(app).into_iter().find(|a| a["uuid"] == uuid).ok_or("UUID is required")?;
    let Some(host) = host_id_of(app, &a) else { return Ok(json!({"count": 0, "has_gpu_data": false, "records": []})) };
    let step: i64 = if hours <= 4.0 { 60 } else { 300 };
    let since = ((now - hours * 3600.0) as i64).div_euclid(step) * step;
    let h2 = host.clone();
    let (totals, rows): (Value, Vec<Vec<Option<i64>>>) = app.ingest.with_db(move |c| {
        let latest: Value = c
            .query_row("SELECT sample FROM host_latest WHERE host_id=?1", [&h2], |r| r.get::<_, String>(0))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        let mut out = vec![];
        if let Ok(mut st) = c.prepare(
            "SELECT minute, cpu_n, cpu_s, mem_n, mem_s, swap_n, swap_s, disk_n, disk_s, load1_n, load1_s, rx_n, rx_s, tx_n, tx_s, procs_n, procs_s, tcp_n, tcp_s, udp_n, udp_s
             FROM host_1m WHERE host_id=?1 AND minute>=?2 ORDER BY minute",
        ) {
            if let Ok(it) = st.query_map(rusqlite::params![h2, since], |r| (0..21).map(|i| r.get::<_, Option<i64>>(i)).collect::<rusqlite::Result<Vec<_>>>()) {
                out = it.flatten().collect();
            }
        }
        (latest, out)
    });
    // 按桶汇总：各指标 (n, s)
    let mut buckets: BTreeMap<i64, [(i64, i64); 10]> = BTreeMap::new();
    for r in rows {
        let m = r[0].unwrap_or(0).div_euclid(step) * step;
        let b = buckets.entry(m).or_insert([(0, 0); 10]);
        for k in 0..10 {
            b[k].0 += r[1 + 2 * k].unwrap_or(0);
            b[k].1 += r[2 + 2 * k].unwrap_or(0);
        }
    }
    let mean = |x: (i64, i64)| if x.0 > 0 { x.1 as f64 / x.0 as f64 } else { 0.0 };
    let tot = |k: &str| totals[k]["total"].as_i64().unwrap_or(0);
    let mut recs = vec![];
    for (m, b) in buckets {
        let mut rec = Map::new();
        let pct_of = |i: usize, total: i64| ((mean(b[i]) / 100.0 / 100.0) * total as f64) as i64;
        let vals: Vec<(&str, Value)> = vec![
            ("client", json!(uuid)),
            ("time", json!(go_time(m * 1000))),
            ("cpu", py::f(mean(b[0]) / 100.0)),
            ("gpu", json!(0)),
            ("ram", json!(pct_of(1, tot("mem")))),
            ("ram_total", json!(tot("mem"))),
            ("swap", json!(pct_of(2, tot("swap")))),
            ("swap_total", json!(tot("swap"))),
            ("load", py::f(mean(b[4]) / 100.0)),
            ("temp", json!(0)),
            ("disk", json!(pct_of(3, tot("disk")))),
            ("disk_total", json!(tot("disk"))),
            ("net_in", json!(mean(b[5]) as i64)),
            ("net_out", json!(mean(b[6]) as i64)),
            ("net_total_up", json!(0)),
            ("net_total_down", json!(0)),
            ("traffic_up", json!((mean(b[6]) * step as f64) as i64)),
            ("traffic_down", json!((mean(b[5]) * step as f64) as i64)),
            ("process", json!(mean(b[7]) as i64)),
            ("connections", json!(mean(b[8]) as i64)),
            ("connections_udp", json!(mean(b[9]) as i64)),
        ];
        for (k, v) in vals {
            rec.insert(k.into(), v);
        }
        debug_assert!(ORDER.iter().all(|k| rec.contains_key(*k)));
        recs.push(Value::Object(rec));
    }
    Ok(json!({"count": recs.len(), "has_gpu_data": false, "records": recs}))
}

/// Komari 任务号 → (资产, 探测种类)
fn task_index(app: &App) -> Vec<(i64, Value, &'static str)> {
    let mut out = vec![];
    for a in assets(app) {
        for (f, k) in [("tcp_task_id", "tcp"), ("egress_task_id", "egress")] {
            if let Some(t) = a[f].as_i64() {
                out.push((t, a.clone(), k));
            }
        }
    }
    out.sort_by_key(|x| x.0);
    out
}

fn task_row(t: i64, a: &Value, kind: &str, hosts: &Value) -> Value {
    let uuid = a["uuid"].as_str().unwrap_or("");
    let short = a["name"].as_str().unwrap_or("").split(" · ").next().unwrap_or("").to_string();
    let relay = a["relay"].as_str().unwrap_or("");
    let name = hosts[uuid]["ping"][t.to_string()]["name"].as_str().map(String::from)
        .unwrap_or_else(|| format!("{short} {} · {relay}", if kind == "tcp" { "TCP" } else { "出口验证" }));
    let target = if kind == "tcp" { format!("{}:{}", a["target"].as_str().unwrap_or(""), a["target_port"].as_i64().unwrap_or(0)) } else { "https://www.cloudflare.com/cdn-cgi/trace".into() };
    json!({"id": t, "weight": t, "name": name, "clients": [uuid], "default_on": false, "type": if kind == "tcp" { "tcp" } else { "http" },
        "target": target, "interval": if kind == "tcp" { 30 } else { 60 }})
}

fn ping_records(app: &App, task: i64, hours: f64, uuid: Option<&str>, now: f64) -> Result<Value, String> {
    let idx = task_index(app);
    let Some((_, a, kind)) = idx.iter().find(|x| x.0 == task).cloned() else {
        return Ok(json!({"count": 0, "basic_info": [], "records": [], "tasks": []}));
    };
    let node = a["uuid"].as_str().unwrap_or("").to_string();
    if uuid.is_some_and(|u| u != node) {
        return Ok(json!({"count": 0, "basic_info": [], "records": [], "tasks": []}));
    }
    let step = if hours <= 4.0 { 60.0 } else { 300.0 };
    let since = now - hours * 3600.0;
    let (n2, k2) = (node.clone(), kind.to_string());
    let rows: Vec<(f64, String, Option<f64>, bool)> = app.ingest.with_db(move |c| {
        let mut out = vec![];
        if let Ok(mut st) = c.prepare("SELECT measured_at, status, doc FROM probe_land WHERE node=?1 AND check_kind=?2 AND measured_at>=?3 ORDER BY measured_at") {
            if let Ok(it) = st.query_map(rusqlite::params![n2, k2, since], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))) {
                for (t, s, d) in it.flatten() {
                    let d: Value = serde_json::from_str(&d).unwrap_or(Value::Null);
                    out.push((t, s, d["value_ms"].as_f64(), d["final"] != false));
                }
            }
        }
        out
    });
    // 每桶取最后一个样本（出口验证只看一个周期的最终结果），-1 = 失败
    let mut last: BTreeMap<i64, i64> = BTreeMap::new();
    for (t, s, v, fin) in rows {
        if !fin {
            continue;
        }
        let b = ((t / step).floor() * step * 1000.0) as i64;
        last.insert(b, if s == "ok" { v.map(|x| x.round() as i64).unwrap_or(-1) } else { -1 });
    }
    let records: Vec<Value> = last.iter().rev().map(|(b, v)| json!({"task_id": task, "time": go_time(*b), "value": v, "client": node})).collect();
    let vals: Vec<i64> = last.values().cloned().collect();
    let ok: Vec<i64> = vals.iter().cloned().filter(|v| *v >= 0).collect();
    let basic = if records.is_empty() { vec![] } else { vec![json!({"client": node, "loss": num(vals.len() - ok.len(), vals.len()), "min": ok.iter().min().cloned().unwrap_or(0), "max": ok.iter().max().cloned().unwrap_or(0)})] };
    let t = task_row(task, &a, kind, &hosts_meta(app));
    let mut row = Map::new();
    row.insert("avg".into(), json!(if ok.is_empty() { 0 } else { ok.iter().sum::<i64>() / ok.len() as i64 }));
    if uuid.is_none() {
        row.insert("clients".into(), t["clients"].clone());
    }
    for k in ["default_on", "id"] {
        row.insert(k.into(), t[k].clone());
    }
    row.insert("interval".into(), t["interval"].clone());
    row.insert("loss".into(), num(vals.len() - ok.len(), vals.len()));
    row.insert("max".into(), json!(ok.iter().max().cloned().unwrap_or(0)));
    row.insert("min".into(), json!(ok.iter().min().cloned().unwrap_or(0)));
    row.insert("name".into(), t["name"].clone());
    row.insert("total".into(), json!(records.len()));
    row.insert("type".into(), t["type"].clone());
    Ok(json!({"count": records.len(), "basic_info": basic, "records": records, "tasks": [row]}))
}

/// Komari 的时间（"2026-10-12 16:00:00+00:00"）→ Komari API 的 RFC 3339（"2026-10-12T16:00:00Z"）
fn go_db_time(v: &Value) -> Value {
    let Some(s) = v.as_str() else { return v.clone() };
    let t = s.replacen(' ', "T", 1);
    match chrono::DateTime::parse_from_rfc3339(&t) {
        Ok(d) => {
            let u = d.with_timezone(&chrono::Utc);
            let frac = t.split('.').nth(1).map(|f| f.chars().take_while(|c| c.is_ascii_digit()).collect::<String>()).unwrap_or_default();
            let base = u.format("%Y-%m-%dT%H:%M:%S").to_string();
            json!(if frac.is_empty() { format!("{base}Z") } else { format!("{base}.{frac}Z") })
        }
        Err(_) => v.clone(),
    }
}

/// komari/clients：Komari 退役前的静态资料快照（fleetd legacy komari-clients 导入）+ 资产现值 + nodeagent/relayd 的主机信息
/// 线路机自身检查的 ping 记录（与 /komari/pings/<id>/results 同形；value = 该桶最后一个样本毫秒，失败 -1）
fn relay_ping_records(app: &App, key: &str, uuid: Option<&str>, hours: f64, now: f64) -> Result<Value, String> {
    let uuid = uuid.ok_or("需要 uuid（线路机资产）")?.to_string();
    let a = assets(app).into_iter().find(|a| a["uuid"].as_str() == Some(uuid.as_str())).ok_or("没有这个资产")?;
    let host = host_id_of(app, &a).ok_or("不是线路机")?;
    let step = if hours <= 4.0 { 60.0 } else { 300.0 };
    let since = now - hours * 3600.0;
    let (node, kind, relay_filter) = match key {
        "control" => ("_relay".to_string(), "control", Some(host.clone())),
        "peer_tcp" => (uuid.clone(), "tcp", None),
        _ => (uuid.clone(), "egress", Some(host.clone())),
    };
    let rows: Vec<(f64, String, Option<f64>, bool)> = app.ingest.with_db(move |c| {
        let mut out = vec![];
        if let Ok(mut st) = c.prepare("SELECT measured_at, status, doc, relay FROM probe_land WHERE node=?1 AND check_kind=?2 AND measured_at>=?3 ORDER BY measured_at") {
            if let Ok(it) = st.query_map(rusqlite::params![node, kind, since], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))) {
                for (t, s, d, rl) in it.flatten() {
                    if relay_filter.as_ref().is_some_and(|f| *f != rl) {
                        continue;
                    }
                    let d: Value = serde_json::from_str(&d).unwrap_or(Value::Null);
                    out.push((t, s, d["value_ms"].as_f64(), d["final"] != false));
                }
            }
        }
        out
    });
    let mut last: BTreeMap<i64, i64> = BTreeMap::new();
    for (t, s, v, fin) in rows {
        if !fin {
            continue;
        }
        let b = ((t / step).floor() * step * 1000.0) as i64;
        last.insert(b, if s == "ok" { v.map(|x| x.round() as i64).unwrap_or(-1) } else { -1 });
    }
    let records: Vec<Value> = last.iter().rev().map(|(b, v)| json!({"task_id": key, "time": go_time(*b), "value": v, "client": uuid})).collect();
    let vals: Vec<i64> = last.values().cloned().collect();
    let ok: Vec<i64> = vals.iter().cloned().filter(|v| *v >= 0).collect();
    Ok(json!({"count": records.len(), "basic_info": if records.is_empty() { json!([]) } else { json!([{"client": uuid, "loss": num(vals.len() - ok.len(), vals.len()),
        "min": ok.iter().min().cloned().unwrap_or(0), "max": ok.iter().max().cloned().unwrap_or(0)}]) }, "records": records,
        "tasks": [{"avg": if ok.is_empty() { 0 } else { ok.iter().sum::<i64>() / ok.len() as i64 }, "id": key, "loss": num(vals.len() - ok.len(), vals.len()),
            "max": ok.iter().max().cloned().unwrap_or(0), "min": ok.iter().min().cloned().unwrap_or(0), "total": records.len(),
            "name": match key { "control" => "控制路径（直连）", "peer_tcp" => "心跳 TCP（对端线路机 → 主节点）", _ => "出口验证（经主节点）" }}]}))
}

fn clients(app: &App) -> Value {
    let hosts = hosts_meta(app);
    let snap: Value = app.legacy_store.reader().and_then(|c| py::meta(&c, "fleetd:komari_clients", json!([])).ok()).unwrap_or(json!([]));
    let metas: std::collections::HashMap<String, Value> = app.ingest.with_db(|c| {
        let mut m = std::collections::HashMap::new();
        if let Ok(mut st) = c.prepare("SELECT host_id, meta FROM host_latest") {
            if let Ok(it) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                for (h, meta) in it.flatten() {
                    m.insert(h, serde_json::from_str(&meta).unwrap_or(Value::Null));
                }
            }
        }
        m
    });
    let out: Vec<Value> = assets(app)
        .iter()
        .map(|a| {
            let u = a["uuid"].as_str().unwrap_or("");
            let h = &hosts[u];
            let base = snap.as_array().and_then(|v| v.iter().find(|x| x["uuid"].as_str() == Some(u))).cloned();
            let mut c = base.as_ref().and_then(|b| b.as_object().cloned()).unwrap_or_else(|| {
                json!({"uuid": u, "name": "", "cpu_name": "", "virtualization": "", "arch": "", "cpu_cores": 0, "os": "", "kernel_version": "",
                    "gpu_name": "", "ipv4": "", "ipv6": "", "region": "", "remark": "", "public_remark": "", "mem_total": 0, "swap_total": 0, "disk_total": 0,
                    "version": "", "weight": 0, "price": 0.0, "billing_cycle": 30, "auto_renewal": false, "currency": "USD", "expired_at": null, "group": "",
                    "tags": "", "hidden": false, "traffic_limit": 0, "traffic_limit_type": "", "created_at": null, "updated_at": null})
                .as_object()
                .cloned()
                .unwrap()
            });
            c.shift_remove("token");
            for k in ["created_at", "updated_at", "expired_at"] {
                if let Some(v) = c.get(k).cloned() {
                    c.insert(k.into(), go_db_time(&v));
                }
            }
            for k in ["auto_renewal", "hidden"] {
                if let Some(v) = c.get(k).and_then(|v| v.as_i64()) {
                    c.insert(k.into(), json!(v != 0));
                }
            }
            // 现值：资产（名称、分组、价格、到期、IP）与主机（总量、核数、系统）
            c.insert("name".into(), a["name"].clone());
            c.insert("group".into(), a["team"].clone());
            c.insert("ipv4".into(), a["ip"].clone());
            if let Some(p) = a["price_cents"].as_i64() {
                c.insert("price".into(), crate::compat::py::f(p as f64 / 100.0));
            }
            if a["expired_at"].is_string() {
                if let Ok(d) = chrono::DateTime::parse_from_rfc3339(a["expired_at"].as_str().unwrap_or("")) {
                    c.insert("expired_at".into(), json!(d.with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%SZ").to_string()));
                }
            }
            for (k, hk) in [("mem_total", "ram_total"), ("swap_total", "swap_total"), ("disk_total", "disk_total")] {
                if let Some(v) = h[hk].as_i64().filter(|v| *v > 0) {
                    c.insert(k.into(), json!(v));
                }
            }
            if let Some(m) = host_id_of(app, a).and_then(|hid| metas.get(&hid)) {
                if let Some(n) = m["ncpu"].as_i64() {
                    c.insert("cpu_cores".into(), json!(n));
                }
                if let Some(o) = m["os"].as_str() {
                    c.insert("os".into(), json!(o));
                }
                if let Some(k) = m["kernel"].as_str() {
                    c.insert("kernel_version".into(), json!(k));
                }
                for (k, mk) in [("arch", "arch"), ("cpu_name", "cpu_model"), ("virtualization", "virt")] {
                    if let Some(v) = m[mk].as_str() {
                        c.insert(k.into(), json!(v));
                    }
                }
            }
            Value::Object(c)
        })
        .collect();
    Value::Array(out)
}

/// `/komari/...` 的原生等价；返回 (状态, {"data":…} 或 {"error":…})。路径不认识时 None。
pub fn render(app: &App, path: &str, params: &BTreeMap<String, String>, now: f64) -> Option<(u16, Value)> {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let wrap = |r: Result<Value, String>| match r {
        Ok(v) => (200, json!({"data": v})),
        Err(e) => (500, json!({"error": e})),
    };
    Some(match segs.as_slice() {
        ["komari", "hosts"] => (200, json!({"data": hosts_meta(app)})),
        ["komari", "hosts", uuid, "metrics"] => match params.get("range").map(String::as_str) {
            Some("1h") => wrap(records(app, uuid, 1.0, now)),
            Some("4h") => wrap(records(app, uuid, 4.0, now)),
            Some("1d") => wrap(records(app, uuid, 24.0, now)),
            _ => (404, json!({"error": "接口不存在"})),
        },
        ["komari", "pings"] => {
            let hosts = hosts_meta(app);
            (200, json!({"data": task_index(app).iter().map(|(t, a, k)| task_row(*t, a, k, &hosts)).collect::<Vec<_>>()}))
        }
        // 线路机自身的检查：control（直连，relayd 控制路径）/ peer_tcp（对端心跳）/ self_egress（经主节点出口），?uuid=<线路机资产>
        ["komari", "pings", key @ ("control" | "peer_tcp" | "self_egress"), "results"] => {
            let hours = params.get("hours").and_then(|h| h.parse::<f64>().ok()).unwrap_or(4.0);
            wrap(relay_ping_records(app, key, params.get("uuid").map(String::as_str), hours, now))
        }
        ["komari", "pings", task, "results"] => match task.parse::<i64>() {
            Ok(id) => {
                let hours = params.get("hours").and_then(|h| h.parse::<f64>().ok()).unwrap_or(4.0);
                wrap(ping_records(app, id, hours, params.get("uuid").map(String::as_str), now))
            }
            Err(_) => (400, json!({"error": format!("invalid literal for int() with base 10: '{task}'")})),
        },
        ["komari", "clients"] => (200, json!({"data": clients(app)})),
        _ => return None,
    })
}
