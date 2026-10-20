//! 套餐目录与资产：导入 architecture/plan-catalog/catalog.json（6 个规范套餐、41 项资产，含 3 台线路机）。
use crate::rules::{due_calendar_days, head_name, short_id, PlanIndex, PlanInfo};
use crate::store::{audit, meta_get, meta_set, Store};
use rusqlite::params;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub struct ImportResult {
    pub plans: usize,
    pub assets: usize,
    pub changed: bool,
}

pub fn import(store: &Store, path: &std::path::Path, actor: &str, now: f64) -> Result<ImportResult, String> {
    let text = std::fs::read(path).map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    let hash = hex::encode(Sha256::digest(&text));
    let v: Value = serde_json::from_slice(&text).map_err(|e| format!("catalog.json 格式错误：{e}"))?;
    let plans = v.get("plans").and_then(|x| x.as_array()).ok_or("catalog.json 缺少 plans")?.clone();
    let assets = v.get("assets").and_then(|x| x.as_array()).ok_or("catalog.json 缺少 assets")?.clone();
    let plan_ids: std::collections::HashSet<String> = plans.iter().filter_map(|p| p.get("id").and_then(|x| x.as_str()).map(String::from)).collect();
    for a in &assets {
        let pid = a.get("plan_id").and_then(|x| x.as_str()).unwrap_or("");
        if !plan_ids.contains(pid) {
            return Err(format!("资产 {} 引用了不存在的套餐 {pid}", a.get("asset_name").and_then(|x| x.as_str()).unwrap_or("?")));
        }
    }
    let (np, na) = (plans.len(), assets.len());
    let src = path.display().to_string();
    let actor = actor.to_string();
    store.call_blocking(move |c| {
        if meta_get(c, "catalog_sha256").as_deref() == Some(hash.as_str()) {
            return Ok(ImportResult { plans: np, assets: na, changed: false });
        }
        let tx = c.transaction().map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM plans", []).map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM assets", []).map_err(|e| e.to_string())?;
        for p in &plans {
            tx.execute(
                "INSERT INTO plans(id,canonical_name,provider,category,data,imported_at) VALUES(?,?,?,?,?,?)",
                params![p["id"].as_str(), p["canonical_name"].as_str(), p["provider"].as_str(), p["category"].as_str(), p.to_string(), now],
            ).map_err(|e| e.to_string())?;
        }
        for a in &assets {
            tx.execute(
                "INSERT INTO assets(name,short_name,team,provider,plan_id,service_id,price,data,imported_at) VALUES(?,?,?,?,?,?,?,?,?)",
                params![a["asset_name"].as_str(), a["short_name"].as_str(), a["team"].as_str(), a["provider"].as_str(),
                    a["plan_id"].as_str(), a["service_id"].as_str(), a["price"].as_str(), a.to_string(), now],
            ).map_err(|e| e.to_string())?;
        }
        meta_set(&tx, "catalog_sha256", &hash).map_err(|e| e.to_string())?;
        audit(&tx, now, &actor, "catalog.import", "catalog", &json!({"source": src, "sha256": hash, "plans": np, "assets": na}))
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(ImportResult { plans: np, assets: na, changed: true })
    })
}

pub fn load_plans(store: &Store) -> Vec<Value> {
    store.call_blocking(|c| {
        let mut st = c.prepare("SELECT p.data, (SELECT count(*) FROM assets a WHERE a.plan_id=p.id) FROM plans p ORDER BY p.category, p.id").unwrap();
        st.query_map([], |r| {
            let d: String = r.get(0)?;
            let n: i64 = r.get(1)?;
            let mut v: Value = serde_json::from_str(&d).unwrap_or(Value::Null);
            if let Value::Object(m) = &mut v {
                m.remove("alias_details");
                m.insert("asset_count".into(), json!(n));
            }
            strip_internal(&mut v);
            Ok(v)
        }).unwrap().filter_map(Result::ok).collect()
    })
}

/// 目录里的内部核对信息（待确认问题、核对记录）只留在文件里，不进任何接口
pub const INTERNAL_KEYS: &[&str] = &["open_questions", "questions", "question", "verification", "supplier_verification", "verification_note", "todo"];

pub fn strip_internal(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for k in INTERNAL_KEYS {
                m.shift_remove(*k);
            }
            for x in m.values_mut() {
                strip_internal(x);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(strip_internal),
        _ => {}
    }
}

pub fn load_assets(store: &Store) -> Vec<Value> {
    store.call_blocking(|c| {
        let mut st = c.prepare("SELECT data FROM assets ORDER BY team, name").unwrap();
        st.query_map([], |r| {
            let d: String = r.get(0)?;
            let mut v = serde_json::from_str::<Value>(&d).unwrap_or(Value::Null);
            strip_internal(&mut v);
            Ok(v)
        }).unwrap().filter_map(Result::ok).collect()
    })
}

/// 目录在内存里的只读副本（只在导入时变化）。
#[derive(Clone, Default)]
pub struct Catalog {
    pub plans: Vec<Value>,
    pub assets: Vec<Value>,
}

impl Catalog {
    pub fn load(store: &Store) -> Catalog {
        Catalog { plans: load_plans(store), assets: load_assets(store) }
    }
}

pub fn plan_index(cat: &Catalog) -> PlanIndex {
    let plans = plans_by_id(cat);
    let mut idx = PlanIndex::new();
    for a in cat.assets.iter().cloned() {
        let Some(p) = a.get("plan_id").and_then(|x| x.as_str()).and_then(|id| plans.get(id)) else { continue };
        let spec = &p["specs"];
        let info = PlanInfo {
            plan_id: p["id"].as_str().unwrap_or("").into(),
            canonical_name: p["canonical_name"].as_str().unwrap_or("").into(),
            quota_gb: spec["traffic_quota_gb"].as_f64(),
            quota_direction: spec["traffic_direction"].as_str().map(String::from),
            quota_period: spec["traffic_period"].as_str().map(String::from),
            traffic_unlimited: spec["traffic_unlimited"].as_bool(),
        };
        if let Some(n) = a.get("asset_name").and_then(|x| x.as_str()) {
            idx.insert(n.to_string(), info);
        }
    }
    idx
}

fn plans_by_id(cat: &Catalog) -> std::collections::HashMap<String, Value> {
    cat.plans.iter().cloned().filter_map(|p| p.get("id").and_then(|x| x.as_str()).map(|id| (id.to_string(), p.clone()))).collect()
}

/// /v1/assets：资产 → 规范套餐名、规格、价格、到期（旧系统实时）、配额。
pub fn assets_view(cat: &Catalog, ov: Option<&Value>, now: f64) -> Value {
    let plans = plans_by_id(cat);
    let live: std::collections::HashMap<String, Value> = ov
        .and_then(|o| o.get("assets"))
        .and_then(|x| x.as_array())
        .map(|xs| xs.iter().filter_map(|a| a.get("name").and_then(|n| n.as_str()).map(|n| (n.to_string(), a.clone()))).collect())
        .unwrap_or_default();
    let mut out = vec![];
    for a in cat.assets.iter() {
        let name = a["asset_name"].as_str().unwrap_or("").to_string();
        let p = plans.get(a["plan_id"].as_str().unwrap_or("")).cloned().unwrap_or(Value::Null);
        let l = live.get(&name);
        let due = l.map(|x| json!({
            "due_date": x.get("due_date"), "expired_at": x.get("expired_at"), "billing_timezone": x.get("billing_timezone"),
            "days": due_calendar_days(x, now), "billing_cycle": x.get("billing_cycle"),
        })).unwrap_or(Value::Null);
        let specs = &p["specs"];
        let is_relay = p["category"] == "relay";
        let quota = json!({
            "unlimited": specs["traffic_unlimited"], "quota_gb": specs["traffic_quota_gb"], "direction": specs.get("traffic_direction"),
            "period": specs.get("traffic_period"), "text": specs["traffic_text"],
            "used_bytes": if is_relay { l.and_then(|x| x.pointer("/host/net_total_up")).cloned().unwrap_or(Value::Null) } else { Value::Null },
        });
        out.push(json!({
            "asset_name": name, "short_name": a["short_name"], "id": short_id(&name), "team": a["team"], "provider": a["provider"],
            "service_id": a["service_id"], "category": p["category"], "plan_id": a["plan_id"], "canonical_name": p["canonical_name"],
            "spec_summary": p["spec_summary"], "specs": specs, "price": {"amount": a["price"], "currency": p["price"]["currency"], "cycle": p["price"]["cycle"],
            "override": a["price_override"]}, "due": due, "quota": quota, "legacy_uuid": l.and_then(|x| x.get("uuid")),
            "in_legacy": l.is_some(),
        }));
    }
    json!({"assets": out, "count": out.len(), "live_source": if ov.is_some() { "legacy /overview" } else { "不可用（旧系统未取到）" }})
}

/// /v1/quota：线路机出站用量 vs 套餐配额（附来源与口径说明）。
pub fn quota_view(cat: &Catalog, ov: Option<&Value>, komari_clients: Option<&Value>, now: f64) -> Value {
    let plans = plans_by_id(cat);
    let live: Vec<Value> = ov.and_then(|o| o.get("assets")).and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let mut rows = vec![];
    for a in cat.assets.iter() {
        let p = plans.get(a["plan_id"].as_str().unwrap_or("")).cloned().unwrap_or(Value::Null);
        if p["category"] != "relay" {
            continue;
        }
        let name = a["asset_name"].as_str().unwrap_or("").to_string();
        let l = live.iter().find(|x| x.get("name").and_then(|n| n.as_str()) == Some(name.as_str()));
        let quota_gb = p["specs"]["traffic_quota_gb"].as_f64();
        let used = l.and_then(|x| x.pointer("/host/net_total_up")).and_then(|x| x.as_f64());
        let pct = match (used, quota_gb) {
            (Some(u), Some(q)) if q > 0.0 => Some((u * 1000.0 / (q * 1e9)).round() / 10.0),
            _ => None,
        };
        let kc = komari_clients.and_then(|v| v.as_array()).and_then(|xs| {
            xs.iter().find(|c| c.get("name").and_then(|n| n.as_str()) == Some(head_name(&name).as_str())).cloned()
        });
        rows.push(json!({
            "relay": head_name(&name), "id": short_id(&name), "plan_id": a["plan_id"], "canonical_name": p["canonical_name"],
            "quota_gb": quota_gb, "quota_bytes": quota_gb.map(|q| q * 1e9), "direction": p["specs"]["traffic_direction"],
            "period": p["specs"]["traffic_period"], "reset_day": l.and_then(|x| x.get("traffic_reset_day")),
            "timezone": l.and_then(|x| x.get("traffic_timezone").or(x.get("billing_timezone"))),
            "used_bytes": used, "used_pct": pct, "sampled_at": l.and_then(|x| x.pointer("/host/time")),
            "legacy_monthly": l.and_then(|x| x.get("monthly")),
            "komari_traffic_limit": kc.as_ref().and_then(|c| c.get("traffic_limit")),
            "komari_traffic_limit_type": kc.as_ref().and_then(|c| c.get("traffic_limit_type")),
            "due_days": l.and_then(|x| due_calendar_days(x, now)),
        }));
    }
    json!({
        "relays": rows,
        "source": "用量：本期累计出站（供应商读数基线 + 网卡出站计数，fleetd 流量计）；配额：套餐目录 catalog.json",
        "caveats": [
            "用量 = 供应商面板读数（fleetd vendor import 时的基线）+ 之后线路机网卡出站计数；每个账单周期（从到期日的“日”开始）清零。",
            "配额按十进制 GB（1 GB = 10^9 B）、出站：Pulse Mini 2000 GB，Pulse Nano 1000 GB。",
            "线路机2 与中控同机，出站含管理面流量。",
            "legacy_monthly 是按北京时间自然月从小时表求和的 Xray 入站计数，仅供对照。"
        ],
        "alert": "80% 起每 5% 一档（warning），≥ 100% 为 critical"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_real_catalog() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../architecture/plan-catalog/catalog.json");
        if !path.exists() {
            eprintln!("skip: {} missing", path.display());
            return;
        }
        let s = Store::open_memory();
        let r = import(&s, &path, "test", 1.0).unwrap();
        assert_eq!((r.plans, r.assets, r.changed), (6, 41, true));
        let r2 = import(&s, &path, "test", 2.0).unwrap();
        assert!(!r2.changed, "同一文件幂等");
        let cat = Catalog::load(&s);
        let idx = plan_index(&cat);
        assert_eq!(idx.len(), 41);
        assert_eq!(idx["线路机1"].quota_gb, Some(2000.0));
        assert_eq!(idx["线路机3"].quota_gb, Some(1000.0));
        assert_eq!(idx["线路机3"].canonical_name, "GoMami Pulse Nano");
        let relays = load_assets(&s).iter().filter(|a| a["team"] == "线路机").count();
        assert_eq!(relays, 3);
        let v = assets_view(&cat, None, 1.0);
        assert_eq!(v["count"], 41);
        let q = quota_view(&cat, None, None, 1.0);
        assert_eq!(q["relays"].as_array().unwrap().len(), 3);
    }
}
