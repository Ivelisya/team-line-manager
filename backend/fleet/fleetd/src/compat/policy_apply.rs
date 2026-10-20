//! 策略发布（旧 server.apply_policies 的原生版）：草稿 + 已生效策略 → 每台线路机的 `policy-*` 路由规则，
//! 作为 relayspec 的新一代下发给 relayd（API.md §10.1：policy 块插在第一条入口路由之前；relayd 守卫 + xray -test）。
//! 版本记录仍写旧库的 versions 表（applying → applied / failed_rolled_back），审计同名。
//! 编译规则与旧 policy.compile_routing 相同：
//!   allowlist/denylist → `policy-<uuid>-match`（domain 列表：`=x` → `full:x`，否则 `domain:x`；allowlist 走节点出站，denylist 走 blocked）；
//!   allowlist/blocked → `policy-<uuid>-default`（其余流量走 blocked）；observe 不产生规则。
use super::py::*;
use super::pyjson::dumps;
use super::writes::{audit, validate_policy};
use super::Ctx;
use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

pub const PREFIX: &str = "policy-";

/// 一台线路机上各入口的策略规则（按资产表顺序）
pub fn compile(assets: &[Value], policies: &Value) -> R<Vec<Value>> {
    let mut out = vec![];
    for a in assets {
        let Some(port) = a["relay_port"].as_i64() else { continue };
        let p = validate_policy(&super::policy::effective_policy(a, policies))?;
        let mode = p["mode"].as_str().unwrap_or("observe");
        if mode == "observe" {
            continue;
        }
        let tag = format!("inbound-{port}");
        let uuid = a["uuid"].as_str().unwrap_or("");
        if mode == "allowlist" || mode == "denylist" {
            let domains: Vec<Value> = p["domains"]
                .as_array()
                .map(|d| d.iter().filter_map(|x| x.as_str()).map(|d| json!(if let Some(e) = d.strip_prefix('=') { format!("full:{e}") } else { format!("domain:{d}") })).collect())
                .unwrap_or_default();
            let ob = if mode == "allowlist" { a["outbound_tag"].clone() } else { json!("blocked") };
            out.push(json!({"type": "field", "inboundTag": [tag], "ruleTag": format!("{PREFIX}{uuid}-match"), "domain": domains, "outboundTag": ob}));
        }
        if mode == "allowlist" || mode == "blocked" {
            out.push(json!({"type": "field", "inboundTag": [tag], "ruleTag": format!("{PREFIX}{uuid}-default"), "outboundTag": "blocked"}));
        }
    }
    Ok(out)
}

fn with_policy_rules(managed: &Value, rules: &[Value]) -> Value {
    let mut m = managed.clone();
    let mut rs: Vec<Value> = m["routing_rules"].as_array().cloned().unwrap_or_default().into_iter().filter(|r| !r["ruleTag"].as_str().unwrap_or("").starts_with(PREFIX)).collect();
    rs.extend(rules.iter().cloned());
    m["routing_rules"] = Value::Array(rs);
    m
}

fn version_id(now: f64) -> String {
    let t = chrono::DateTime::from_timestamp(now as i64, 0).map(|d| d.format("%Y%m%dT%H%M%SZ").to_string()).unwrap_or_default();
    format!("{t}-{}", hex::encode(crate::accounts::random_bytes(4)))
}

/// POST /policies/apply：`{draft_hash}`（旧参数 routing_hashes 不再需要：relayd 以 base sha 做乐观并发）
/// relays = [(host_id, 线路机名)]；fc = fleetd.db（relayspec）；返回 {ok, version, status:"applying", relays:{名:op}}
pub fn apply(cx: &Ctx, fc: &Connection, req: &Value, actor: &str, relays: &[(String, String)], managed_live: &[String]) -> R<Value> {
    if !rows(cx.db, "SELECT 1 FROM versions WHERE status IN ('interrupted','rollback_failed','applying') LIMIT 1", &[])?.is_empty() {
        return Err(Err::Bad("存在未完成或未核实的发布，需先核对各线路状态，避免覆盖未知配置".into()));
    }
    let (policies, draft_hash) = super::policy::candidate(cx)?;
    if req.get("draft_hash").and_then(|v| v.as_str()) != Some(draft_hash.as_str()) {
        return Err(Err::Bad("草稿已变化，请重新预览".into()));
    }
    let assets: Vec<Value> = super::store::raw_assets(cx)?;
    let version = version_id(cx.now);
    let mut plan: Vec<(String, String, Value, String, Value)> = vec![]; // (host, 名, 新 managed, base, 旧 managed)
    for (host, name) in relays {
        let subset: Vec<Value> = assets.iter().filter(|a| a["relay"].as_str() == Some(name.as_str()) && a["relay_port"].is_i64()).cloned().collect();
        let rules = compile(&subset, &policies)?;
        let (m, base) = match crate::relayspec::base_for_write(fc, host) {
            Ok(x) => x,
            // 这台线路机没有策略规则要下发，也还没接管（没有观测）：跳过
            Err(_) if rules.is_empty() => continue,
            Err(e) => return Err(Err::Bad(format!("{name}：{e}"))),
        };
        let next = with_policy_rules(&m, &rules);
        if next == m {
            continue;
        }
        if !managed_live.contains(host) {
            return Err(Err::Bad(format!("{name} 的 relayd 还没开启管理模式，暂不发布")));
        }
        plan.push((host.clone(), name.clone(), next, base, m));
    }
    let mut ops = Map::new();
    let mut prev = Map::new();
    for (host, name, next, base, old) in &plan {
        let g = crate::relayspec::insert_spec(fc, host, next, base, "policy_apply", actor, cx.now).map_err(|e| Err::Internal(e.to_string()))?;
        let op = crate::relayspec::create_op(fc, "policy_apply", host, Some(g), &version, &json!({"version": version, "rules": next["routing_rules"].as_array().map(|r| r.len())}), actor, cx.now)
            .map_err(|e| Err::Internal(e.to_string()))?;
        ops.insert(name.clone(), json!({"host": host, "op": op, "generation": g}));
        prev.insert(host.clone(), old.clone());
    }
    let status = if plan.is_empty() { "applied" } else { "applying" };
    let result = json!({"attempted_relays": plan.iter().map(|p| p.1.clone()).collect::<Vec<_>>(), "ops": ops, "draft_hash": draft_hash});
    cx.db.execute("INSERT INTO versions VALUES(?,?,?,?,?,?)", params![version, cx.now, actor, dumps(&policies), status, dumps(&result)])?;
    audit(cx, actor, "policy_apply_started", &version, &json!({"draft_hash": draft_hash}))?;
    // 回滚要用的“发布前”受管集合放在 meta（不进 versions.result，免得旧界面展示）
    cx.db.execute("INSERT INTO meta VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![format!("fleetd:policy_prev:{version}"), dumps(&Value::Object(prev))])?;
    if plan.is_empty() {
        finish(cx, &version, &policies, &draft_hash, "applied", &result, actor)?;
    }
    Ok(json!({"ok": true, "version": version, "status": status, "relays": ops}))
}

fn finish(cx: &Ctx, version: &str, policies: &Value, draft_hash: &str, status: &str, result: &Value, actor: &str) -> R<()> {
    if status == "applied" {
        cx.db.execute("INSERT INTO meta VALUES('applied_policies',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![dumps(policies)])?;
        if super::policy::policy_state(cx)?["draft_hash"].as_str() == Some(draft_hash) {
            cx.db.execute("DELETE FROM drafts", [])?;
        }
    }
    cx.db.execute("UPDATE versions SET status=?,result=? WHERE id=?", params![status, dumps(result), version])?;
    audit(cx, actor, &format!("policy_apply_{status}"), version, result)?;
    Ok(())
}

/// 喂数节拍里调用：applying 的版本看各线路机回执；全部 applied → 生效；有被拒/过期 → 已应用的线路机回到发布前
pub fn reconcile(cx: &Ctx, fc: &Connection) -> R<usize> {
    let mut n = 0;
    for v in rows(cx.db, "SELECT id,actor,policies,result FROM versions WHERE status='applying'", &[])? {
        let id = v["id"].as_str().unwrap_or("").to_string();
        let mut result = loads(&v["result"])?;
        let actor = v["actor"].as_str().unwrap_or("system").to_string();
        let ops = result["ops"].as_object().cloned().unwrap_or_default();
        let mut states = Map::new();
        for (name, o) in &ops {
            let st: Option<(String, Option<String>)> = fc
                .query_row("SELECT status, error FROM ops WHERE id=?", [o["op"].as_str().unwrap_or("")], |r| Ok((r.get(0)?, r.get(1)?)))
                .ok();
            states.insert(name.clone(), json!({"status": st.as_ref().map(|s| s.0.clone()), "error": st.and_then(|s| s.1)}));
        }
        if states.values().any(|s| s["status"] == "pending") {
            continue;
        }
        let ok = states.values().all(|s| s["status"] == "applied");
        result["relays"] = Value::Object(states.clone());
        if ok {
            let policies = loads(&v["policies"])?;
            finish(cx, &id, &policies, result["draft_hash"].as_str().unwrap_or(""), "applied", &result, &actor)?;
        } else {
            // 已应用的线路机回到发布前的受管集合（被拒的那台 relayspec 已自动回退）
            let prev = super::py::meta(cx.db, &format!("fleetd:policy_prev:{id}"), json!({}))?;
            let mut rolled = Map::new();
            for (name, o) in &ops {
                if states[name]["status"] != "applied" {
                    continue;
                }
                let host = o["host"].as_str().unwrap_or("");
                if let Some(old) = prev.get(host) {
                    let base = crate::relayspec::base_for_write(fc, host).map(|x| x.1).unwrap_or_default();
                    match crate::relayspec::insert_spec(fc, host, old, &base, &format!("policy_rollback_{id}"), "system", cx.now) {
                        Ok(g) => rolled.insert(name.clone(), json!({"generation": g})),
                        Err(e) => rolled.insert(name.clone(), json!({"rollback_error": e.to_string()})),
                    };
                }
            }
            result["rollback"] = Value::Object(rolled.clone());
            let status = if rolled.values().any(|x| x.get("rollback_error").is_some()) { "rollback_failed" } else { "failed_rolled_back" };
            let policies = loads(&v["policies"])?;
            finish(cx, &id, &policies, "", status, &result, &actor)?;
        }
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compiles_like_legacy() {
        let assets = vec![
            json!({"uuid": "a", "team": "t", "relay_port": 20002, "outbound_tag": "ss-a"}),
            json!({"uuid": "b", "team": "t", "relay_port": 20003, "outbound_tag": "ss-b"}),
            json!({"uuid": "c", "team": "u", "relay_port": 20004, "outbound_tag": "ss-c"}),
        ];
        let pol = json!({"team:t": {"mode": "allowlist", "domains": ["x.com", "=y.org"], "note": ""}, "node:b": {"mode": "denylist", "domains": ["ads.com"]}});
        let r = compile(&assets, &pol).unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0]["ruleTag"], "policy-a-match");
        assert_eq!(r[0]["domain"], json!(["domain:x.com", "full:y.org"]));
        assert_eq!(r[0]["outboundTag"], "ss-a");
        assert_eq!(r[1]["ruleTag"], "policy-a-default");
        assert_eq!(r[2]["ruleTag"], "policy-b-match");
        assert_eq!(r[2]["outboundTag"], "blocked");
        let m = json!({"inbounds": [], "outbounds": [], "routing_rules": [{"ruleTag": "fleet-explicit-inbound-20002"}, {"ruleTag": "policy-old-default"}]});
        let n = with_policy_rules(&m, &r);
        assert_eq!(n["routing_rules"].as_array().unwrap().len(), 4);
        assert_eq!(n["routing_rules"][0]["ruleTag"], "fleet-explicit-inbound-20002");
    }
}
