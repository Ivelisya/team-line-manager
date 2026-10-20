//! `fleetd vendor import <json>`：把供应商面板的读数（到期日、价格、套餐、service_id、线路机本期出站用量）同步到资产与流量计。
//!
//! JSON 形状（~/Desktop/team-ops-v2/vendor/vendor-YYYY-MM-DD.json）：
//! `{"fetched_at": ISO, "gomami": [{ip, relay, product, traffic_used_gb, traffic_quota_gb, price_usd, next_due, …}],
//!   "attvps": [[service_id, plan, price_usd_monthly, next_due, ip], …], "attvps_columns": […]}`
//! - 资产按 ip / 名称（GoMami）或 service_id / ip（ATTVPS）匹配；有差异才改，revision+1，审计 actor = "vendor-sync"、action = "vendor_sync"；
//! - 线路机流量计基线 = 面板读数 + 读数时刻之后的网卡出站积分；入站按本周期积分；
//! - `--komari-db`：落地节点还没有流量计的，从旧库里 Komari 最后同步的 net_total_up/down 接上；
//! - `--dry-run`：只报告差异。
use crate::compat::{py, pyjson, writes, CompatCfg, Ctx};
use crate::meter;
use chrono::{DateTime, NaiveDate, TimeZone};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Map, Value};
use std::path::Path;

fn norm_plan(s: &str) -> String {
    s.replace("VPS", "").replace(char::is_whitespace, "").to_lowercase()
}

fn cents_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_f64().map(|x| (x * 100.0).round() as i64),
        Value::String(s) => s.trim().parse::<f64>().ok().map(|x| (x * 100.0).round() as i64),
        _ => None,
    }
}

fn expired_at(date: &str, tz: &str) -> Option<String> {
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let z: chrono_tz::Tz = tz.parse().ok()?;
    z.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest().map(|t| t.format("%Y-%m-%dT%H:%M:%S%:z").to_string())
}

struct Change {
    uuid: String,
    name: String,
    fields: Map<String, Value>,
    new: Value,
}

/// 与供应商读数比较，返回要改的字段
fn diff(a: &Value, price: Option<i64>, due: Option<&str>, plan: Option<&str>, service_id: Option<&Value>) -> Map<String, Value> {
    let mut ch = Map::new();
    if let Some(p) = price {
        if a["price_cents"].as_i64() != Some(p) {
            ch.insert("price_cents".into(), json!({"before": a["price_cents"], "after": p}));
            ch.insert("price".into(), json!({"before": a["price"], "after": format!("{}.{:02}", p / 100, p % 100)}));
        }
    }
    if let Some(d) = due {
        if a["due_date"].as_str() != Some(d) {
            ch.insert("due_date".into(), json!({"before": a["due_date"], "after": d}));
            let tz = a["billing_timezone"].as_str().unwrap_or("Asia/Shanghai");
            ch.insert("expired_at".into(), json!({"before": a["expired_at"], "after": expired_at(d, tz)}));
        }
    }
    if let Some(p) = plan {
        // 资产里的套餐说明常带规格（「LAX.Pulse.Mini · 2 vCPU / …」）：以供应商套餐名开头就算一致
        let (cur, want) = (norm_plan(a["plan"].as_str().unwrap_or("")), norm_plan(p));
        if !(cur == want || cur.starts_with(&want)) {
            ch.insert("plan".into(), json!({"before": a["plan"], "after": p}));
        }
    }
    if let Some(sid) = service_id {
        let cur = a["service_id"].as_i64().map(|x| x.to_string()).or_else(|| a["service_id"].as_str().map(String::from));
        let want = sid.as_i64().map(|x| x.to_string()).or_else(|| sid.as_str().map(String::from));
        if cur != want {
            let v = want.as_deref().and_then(|w| w.parse::<i64>().ok()).map(|x| json!(x)).unwrap_or(sid.clone());
            ch.insert("service_id".into(), json!({"before": a["service_id"], "after": v}));
        }
    }
    ch
}

fn apply(a: &Value, ch: &Map<String, Value>, now: f64) -> Value {
    let mut n = a.as_object().cloned().unwrap_or_default();
    for (k, v) in ch {
        n.insert(k.clone(), v["after"].clone());
    }
    let rev = a["revision"].as_i64().unwrap_or(1) + 1;
    n.insert("revision".into(), json!(rev));
    n.insert("updated_at".into(), py::f(now));
    Value::Object(n)
}

pub fn import(legacy_db: &Path, ingest_db: &Path, vendor_json: &Path, komari_db: Option<&Path>, dry_run: bool, relay_hosts: &std::collections::BTreeMap<String, String>, now: f64) -> Result<Value, String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(vendor_json).map_err(|e| format!("读 {}：{e}", vendor_json.display()))?).map_err(|e| e.to_string())?;
    let fetched_at = v["fetched_at"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.timestamp() as f64).ok_or("fetched_at 缺失或格式不对")?;
    let c = Connection::open(legacy_db).map_err(|e| e.to_string())?;
    c.busy_timeout(std::time::Duration::from_secs(30)).ok();
    c.execute_batch(meter::SCHEMA).map_err(|e| e.to_string())?;
    let ic = Connection::open_with_flags(ingest_db, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
    let assets: Vec<Value> = {
        let mut st = c.prepare("SELECT data FROM assets").map_err(|e| e.to_string())?;
        let v = st.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?.flatten().filter_map(|s| serde_json::from_str(&s).ok()).collect();
        v
    };
    let mut changes: Vec<Change> = vec![];
    let mut unmatched_vendor = vec![];
    let mut matched = std::collections::HashSet::new();
    let mut meters = vec![];
    // GoMami：线路机
    for g in v["gomami"].as_array().into_iter().flatten() {
        let a = assets.iter().find(|a| a["ip"] == g["ip"] || a["name"] == g["relay"]);
        let Some(a) = a else {
            unmatched_vendor.push(json!({"vendor": "gomami", "row": g}));
            continue;
        };
        let uuid = a["uuid"].as_str().unwrap_or("").to_string();
        matched.insert(uuid.clone());
        let price = g["price_usd"].as_f64().map(|x| (x * 100.0).round() as i64);
        let ch = diff(a, price, g["next_due"].as_str(), g["product"].as_str(), None);
        if !ch.is_empty() {
            changes.push(Change { uuid: uuid.clone(), name: a["name"].as_str().unwrap_or("").into(), new: apply(a, &ch, now), fields: ch });
        }
        // 流量计基线（用到期日更新后的资产算周期）
        let a2 = changes.iter().find(|c| c.uuid == uuid).map(|c| c.new.clone()).unwrap_or_else(|| a.clone());
        let host = relay_hosts.iter().find(|(_, n)| a["name"].as_str() == Some(n.as_str())).map(|(h, _)| h.clone());
        let used = g["traffic_used_gb"].as_f64().map(|x| x * 1e9);
        if let (Some(host), Some(used)) = (host, used) {
            let ps = meter::period_start(&a2, now);
            let ps_ts = ps.map(|d| meter::period_start_ts(&a2, d)).unwrap_or(fetched_at);
            let (tx_after, _) = meter::integrate(&ic, &host, fetched_at, now);
            let (_, rx_period) = meter::integrate(&ic, &host, ps_ts, now);
            let latest: Option<(String, String)> = ic.query_row("SELECT sample, boot_id FROM host_latest WHERE host_id=?1", [&host], |r| Ok((r.get(0)?, r.get(1)?))).ok();
            let (tx, rx, boot) = match &latest {
                Some((s, b)) => {
                    let s: Value = serde_json::from_str(s).unwrap_or(Value::Null);
                    (s["net"]["tx_bytes"].as_i64(), s["net"]["rx_bytes"].as_i64(), Some(b.clone()))
                }
                None => (None, None, None),
            };
            meters.push((uuid.clone(), meter::Meter { period_start: ps.map(|d| d.to_string()), up: used + tx_after, down: rx_period, last_tx: tx, last_rx: rx, last_boot: boot },
                json!({"source": "vendor", "vendor_used_gb": g["traffic_used_gb"], "fetched_at": fetched_at, "added_tx_after_fetch": tx_after.round(),
                    "quota_gb": g["traffic_quota_gb"], "period_start": ps.map(|d| d.to_string())}), a["name"].clone()));
        }
    }
    // ATTVPS
    let cols: Vec<String> = v["attvps_columns"].as_array().map(|c| c.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let idx = |k: &str| cols.iter().position(|c| c == k);
    for row in v["attvps"].as_array().into_iter().flatten() {
        let get = |k: &str| idx(k).and_then(|i| row.get(i)).cloned().unwrap_or(Value::Null);
        let (sid, ip) = (get("service_id"), get("ip"));
        let sid_s = sid.as_str().map(String::from).or_else(|| sid.as_i64().map(|x| x.to_string()));
        let a = assets.iter().find(|a| sid_s.is_some() && (a["service_id"].as_i64().map(|x| x.to_string()) == sid_s || a["service_id"].as_str().map(String::from) == sid_s))
            .or_else(|| assets.iter().find(|a| a["ip"] == ip));
        let Some(a) = a else {
            unmatched_vendor.push(json!({"vendor": "attvps", "row": row}));
            continue;
        };
        matched.insert(a["uuid"].as_str().unwrap_or("").to_string());
        let price = cents_of(&get("price_usd_monthly"));
        let mut ch = diff(a, price, get("next_due").as_str(), get("plan").as_str(), Some(&sid));
        if a["ip"] != ip {
            ch.insert("ip_mismatch".into(), json!({"asset_ip": a["ip"], "vendor_ip": ip, "note": "只报告，不改 IP"}));
        }
        if !ch.is_empty() {
            let mut applied = ch.clone();
            applied.remove("ip_mismatch");
            changes.push(Change { uuid: a["uuid"].as_str().unwrap_or("").into(), name: a["name"].as_str().unwrap_or("").into(), new: apply(a, &applied, now), fields: ch });
        }
    }
    let unmatched_assets: Vec<Value> = assets
        .iter()
        .filter(|a| ["ATTVPS", "GoMami"].contains(&a["provider"].as_str().unwrap_or("")) || a["team"] == "线路机")
        .filter(|a| !matched.contains(a["uuid"].as_str().unwrap_or("")))
        .map(|a| json!({"uuid": a["uuid"], "name": a["name"], "provider": a["provider"]}))
        .collect();
    // 落地节点：从 Komari 最后同步值接上
    let mut seeded = vec![];
    if let Some(kdb) = komari_db.filter(|p| p.exists()) {
        let k = Connection::open_with_flags(kdb, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| format!("{}：{e}", kdb.display()))?;
        let hosts = py::meta(&k, "hosts", json!({})).unwrap_or(json!({}));
        let t0 = py::meta(&k, "hosts_at", json!(0)).ok().and_then(|x| x.as_f64()).unwrap_or(0.0);
        for a in assets.iter().filter(|a| a["team"] != "线路机") {
            let uuid = a["uuid"].as_str().unwrap_or("");
            if meter::get(&c, uuid).is_some_and(|m| m.up > 0.0 || m.down > 0.0) && c.query_row("SELECT baseline IS NOT NULL FROM fleetd_meter WHERE asset=?1", [uuid], |r| r.get::<_, bool>(0)).unwrap_or(false) {
                continue;
            }
            let h = &hosts[uuid];
            let (Some(up0), Some(down0)) = (h["net_total_up"].as_f64(), h["net_total_down"].as_f64()) else { continue };
            let host = a["name"].as_str().unwrap_or("").split(" · ").next().unwrap_or("").to_string();
            let (tx, rx) = meter::integrate(&ic, &host, t0, now);
            let latest: Option<(String, String)> = ic.query_row("SELECT sample, boot_id FROM host_latest WHERE host_id=?1", [&host], |r| Ok((r.get(0)?, r.get(1)?))).ok();
            let Some((s, b)) = latest else { continue };
            let s: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
            meters.push((uuid.to_string(), meter::Meter { period_start: None, up: up0 + tx, down: down0 + rx, last_tx: s["net"]["tx_bytes"].as_i64(), last_rx: s["net"]["rx_bytes"].as_i64(), last_boot: Some(b) },
                json!({"source": "komari", "komari_up": up0, "komari_down": down0, "komari_at": t0, "added_after": [tx.round(), rx.round()]}), a["name"].clone()));
            seeded.push(a["name"].clone());
        }
    }
    let report_changes: Vec<Value> = changes.iter().map(|c| json!({"uuid": c.uuid, "name": c.name, "changes": c.fields})).collect();
    let report_meters: Vec<Value> = meters.iter().map(|(u, m, b, n)| json!({"uuid": u, "name": n, "up_gb": (m.up / 1e7).round() / 100.0, "down_gb": (m.down / 1e7).round() / 100.0, "period_start": m.period_start, "baseline": b})).collect();
    if !dry_run {
        let tx = c.unchecked_transaction().map_err(|e| e.to_string())?;
        let cfg = CompatCfg::default();
        let cx = Ctx { db: &c, now, cfg: &cfg };
        for ch in &changes {
            if ch.fields.keys().all(|k| k == "ip_mismatch") {
                continue;
            }
            c.execute("UPDATE assets SET data=?1 WHERE uuid=?2", params![pyjson::dumps(&ch.new), ch.uuid]).map_err(|e| e.to_string())?;
            writes::audit(&cx, "vendor-sync", "vendor_sync", &ch.uuid, &json!({"changes": ch.fields, "source": vendor_json.file_name().and_then(|n| n.to_str()), "fetched_at": fetched_at}))
                .map_err(|e| format!("{e:?}"))?;
        }
        for (u, m, b, _) in &meters {
            meter::put(&c, u, m, now, Some(b)).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
    }
    Ok(json!({"dry_run": dry_run, "fetched_at": fetched_at, "asset_changes": report_changes, "meters": report_meters, "komari_seeded": seeded,
        "unmatched_vendor_rows": unmatched_vendor, "assets_without_vendor_row": unmatched_assets}))
}
