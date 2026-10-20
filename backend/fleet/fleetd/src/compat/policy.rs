//! 旧 `teamops.policy` 与 server.policy_state / preview 的读路径。
use super::py::*;
use super::Ctx;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub fn digest(v: &Value) -> String {
    hex::encode(Sha256::digest(super::pyjson::dumps_sorted_ascii(v).as_bytes()))
}

pub fn effective_policy(a: &Value, policies: &Value) -> Value {
    let node = format!("node:{}", get(a, "uuid").as_str().unwrap_or(""));
    let team = format!("team:{}", get(a, "team").as_str().unwrap_or(""));
    if let Some(p) = policies.get(&node) {
        return p.clone();
    }
    if let Some(p) = policies.get(&team) {
        return p.clone();
    }
    json!({"mode": "observe", "domains": []})
}

fn matches(host: &str, patterns: &[Value]) -> bool {
    let h = host.to_lowercase();
    let h = h.trim_end_matches('.');
    patterns.iter().filter_map(|p| p.as_str()).any(|p| h == p.trim_start_matches('=') || (!p.starts_with('=') && h.ends_with(&format!(".{p}"))))
}

pub fn decision(host: &str, policy: &Value) -> &'static str {
    let mode = get(policy, "mode").as_str().unwrap_or("");
    if mode == "blocked" {
        return "blocked";
    }
    let hit = matches(host, get_or(policy, "domains", json!([])).as_array().map(|v| v.as_slice()).unwrap_or(&[]));
    if (mode == "allowlist" && !hit) || (mode == "denylist" && hit) {
        "blocked"
    } else {
        "routed"
    }
}

pub fn policy_state(cx: &Ctx) -> R<Value> {
    let active = meta(cx.db, "applied_policies", json!({}))?;
    let mut drafts = Map::new();
    for r in rows(cx.db, "SELECT * FROM drafts", &[])? {
        let scope = r["scope"].as_str().unwrap_or("").to_string();
        drafts.insert(scope, json!({"policy": loads(&r["data"])?, "revision": r["rev"], "updated_at": r["ts"]}));
    }
    let mut versions = vec![];
    for mut r in rows(cx.db, "SELECT id,ts,actor,status,result FROM versions ORDER BY ts DESC LIMIT 30", &[])? {
        let res = if truthy(&r["result"]) { loads(&r["result"])? } else { json!({}) };
        r.insert("result".into(), res);
        versions.push(Value::Object(r));
    }
    let mut hash_in = Map::new();
    for (k, v) in &drafts {
        hash_in.insert(k.clone(), v["policy"].clone());
    }
    Ok(json!({"active": active, "drafts": drafts, "versions": versions, "draft_hash": digest(&Value::Object(hash_in))}))
}

pub fn candidate(cx: &Ctx) -> R<(Value, String)> {
    let st = policy_state(cx)?;
    let mut pol = st["active"].as_object().cloned().unwrap_or_default();
    for (scope, row) in st["drafts"].as_object().cloned().unwrap_or_default() {
        if get(&row["policy"], "mode") == "inherit" {
            pol.shift_remove(&scope);
        } else {
            pol.insert(scope, row["policy"].clone());
        }
    }
    Ok((Value::Object(pol), st["draft_hash"].as_str().unwrap_or("").to_string()))
}

pub fn preview(cx: &Ctx) -> R<Value> {
    let (policies, draft_hash) = candidate(cx)?;
    let assets: Vec<Value> = super::store::raw_assets(cx)?.into_iter().filter(|a| truthy(get(a, "relay_port"))).collect();
    let recent = rows(cx.db, "SELECT asset,host,count(*) count FROM activity WHERE ts>? GROUP BY asset,host", &[sf(cx.now - 86400.0)])?;
    let mut matched: Map<String, Value> = Map::new();
    for r in recent {
        let u = r["asset"].as_str().unwrap_or("");
        let Some(a) = assets.iter().rev().find(|a| get(a, "uuid").as_str() == Some(u)) else { continue };
        if decision(r["host"].as_str().unwrap_or(""), &effective_policy(a, &policies)) == "blocked" {
            let e = matched.entry(u.to_string()).or_insert_with(|| json!({"connections": 0, "domains": []}));
            let c = add(&e["connections"], &r["count"]);
            e["connections"] = c;
            if e["domains"].as_array().map(|d| d.len()).unwrap_or(0) < 20 {
                e["domains"].as_array_mut().unwrap().push(r["host"].clone());
            }
        }
    }
    let ov = super::store::overview(cx)?;
    let nodes: Vec<Value> = assets
        .iter()
        .map(|a| {
            let u = get(a, "uuid").as_str().unwrap_or("");
            json!({"uuid": a["uuid"], "name": a["name"], "relay": a["relay"], "policy": effective_policy(a, &policies),
                "would_block": matched.get(u).cloned().unwrap_or(json!({"connections": 0, "domains": []}))})
        })
        .collect();
    let mut hashes = Map::new();
    for (k, v) in ov["relays"].as_object().cloned().unwrap_or_default() {
        hashes.insert(k, get(&v, "routing_hash").clone());
    }
    Ok(json!({"draft_hash": draft_hash, "policies": policies, "nodes": nodes, "routing_hashes": hashes,
        "notice": "基于最近 24 小时已记录目标的模拟；原有全局规则仍生效。域名隐藏或只有 IP 的连接无法匹配白名单。"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_digest() {
        // Python: hashlib.sha256(json.dumps({}, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        assert_eq!(digest(&json!({})), "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a");
    }
    #[test]
    fn decisions() {
        let p = json!({"mode": "allowlist", "domains": ["example.com", "=a.org"]});
        assert_eq!(decision("www.example.com", &p), "routed");
        assert_eq!(decision("x.a.org", &p), "blocked");
        assert_eq!(decision("a.org.", &p), "routed");
    }
}
