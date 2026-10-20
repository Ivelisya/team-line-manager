//! relayd 管理模式（fleet.relayspec.v1，API.md §10）的 fleetd 侧：
//! 观测上报 → 期望状态（按线路机的 generation）→ relayd 回执 → op 状态。
//!
//! - 在第一次写操作之前，fleetd 只保存 relayd 上报的受管集合（generation 0 = 跟随文件，relayd 什么都不做）。
//! - 写操作：以“最近一次已应用的期望”或（尚未接管时）最新观测为基准，改出新的完整受管集合，generation + 1。
//! - 外部写入者（fleet-cutover / onboard 在同一把 flock 下改配置）让观测 sha 变化而受管集合与期望不同：收编为新一代。
//! - relayd 拒绝某一代：生成一代“回到上次已应用内容”的期望，避免后续写操作叠在没生效的改动上。
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS relay_observed(relay TEXT PRIMARY KEY, config_sha256 TEXT NOT NULL, generation INTEGER NOT NULL,
  managed TEXT NOT NULL, observed_at REAL NOT NULL, received_at REAL NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS relay_specs(relay TEXT NOT NULL, generation INTEGER NOT NULL, managed TEXT NOT NULL, spec_sha256 TEXT NOT NULL,
  base_config_sha256 TEXT NOT NULL, reason TEXT NOT NULL, created_by TEXT NOT NULL, created_at REAL NOT NULL, PRIMARY KEY(relay, generation)) STRICT;
CREATE TABLE IF NOT EXISTS relay_acks(relay TEXT NOT NULL, generation INTEGER NOT NULL, status TEXT NOT NULL, config_sha256_after TEXT,
  doc TEXT NOT NULL, received_at REAL NOT NULL, PRIMARY KEY(relay, generation)) STRICT;
CREATE TABLE IF NOT EXISTS relay_commands(id TEXT PRIMARY KEY, relay TEXT NOT NULL, kind TEXT NOT NULL, args TEXT NOT NULL, op_id TEXT,
  created_at REAL NOT NULL, expires_at REAL NOT NULL, status TEXT NOT NULL, result TEXT) STRICT;
CREATE TABLE IF NOT EXISTS ops(id TEXT PRIMARY KEY, kind TEXT NOT NULL, relay TEXT NOT NULL, generation INTEGER, target TEXT NOT NULL,
  detail TEXT NOT NULL, status TEXT NOT NULL, created_by TEXT NOT NULL, created_at REAL NOT NULL, acked_at REAL, result TEXT, error TEXT,
  expires_at REAL NOT NULL) STRICT;
CREATE INDEX IF NOT EXISTS ops_relay ON ops(relay, created_at);
CREATE TABLE IF NOT EXISTS entry_meta(relay TEXT NOT NULL, port INTEGER NOT NULL, remark TEXT, disabled_inbound TEXT, ledger_note TEXT,
  updated_at REAL NOT NULL, PRIMARY KEY(relay, port)) STRICT;
"#;

/// relayd 多久没取走期望 / 回执就把 op 记为 expired
pub const OP_TTL_S: f64 = 180.0;

pub fn canonical(v: &Value) -> String {
    // serde_json（preserve_order）按插入顺序输出；受管对象来自 relayd，顺序稳定即可
    serde_json::to_string(v).unwrap_or_default()
}

pub fn sha(v: &Value) -> String {
    hex::encode(Sha256::digest(canonical(v).as_bytes()))
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", hex::encode(crate::accounts::random_bytes(8)))
}

#[derive(Clone, Debug)]
pub struct Spec {
    pub generation: i64,
    pub managed: Value,
    pub spec_sha256: String,
    pub base_config_sha256: String,
    pub created_at: f64,
}

pub fn current_spec(c: &Connection, relay: &str) -> Option<Spec> {
    c.query_row(
        "SELECT generation, managed, spec_sha256, base_config_sha256, created_at FROM relay_specs WHERE relay=? ORDER BY generation DESC LIMIT 1",
        [relay],
        |r| {
            let m: String = r.get(1)?;
            Ok(Spec { generation: r.get(0)?, managed: serde_json::from_str(&m).unwrap_or(Value::Null), spec_sha256: r.get(2)?,
                base_config_sha256: r.get(3)?, created_at: r.get(4)? })
        },
    )
    .optional()
    .ok()
    .flatten()
}

pub fn observed(c: &Connection, relay: &str) -> Option<(String, i64, Value, f64)> {
    c.query_row("SELECT config_sha256, generation, managed, observed_at FROM relay_observed WHERE relay=?", [relay], |r| {
        let m: String = r.get(2)?;
        Ok((r.get(0)?, r.get(1)?, serde_json::from_str(&m).unwrap_or(Value::Null), r.get(3)?))
    })
    .optional()
    .ok()
    .flatten()
}

/// 最近一次 applied 回执：(generation, config_sha256_after)
pub fn last_applied(c: &Connection, relay: &str) -> Option<(i64, String)> {
    c.query_row(
        "SELECT generation, coalesce(config_sha256_after,'') FROM relay_acks WHERE relay=? AND status='applied' ORDER BY generation DESC LIMIT 1",
        [relay],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .ok()
    .flatten()
}

pub fn insert_spec(c: &Connection, relay: &str, managed: &Value, base_sha: &str, reason: &str, actor: &str, now: f64) -> rusqlite::Result<i64> {
    let gen: i64 = c.query_row("SELECT coalesce(max(generation),0)+1 FROM relay_specs WHERE relay=?", [relay], |r| r.get(0))?;
    c.execute(
        "INSERT INTO relay_specs(relay, generation, managed, spec_sha256, base_config_sha256, reason, created_by, created_at) VALUES(?,?,?,?,?,?,?,?)",
        params![relay, gen, canonical(managed), sha(managed), base_sha, reason, actor, now],
    )?;
    Ok(gen)
}

/// 观测上报：保存；已接管时若有外部改动则收编为新一代。返回（fleetd 当前 generation，是否收编）
pub fn record_observed(c: &mut Connection, relay: &str, doc: &Value, now: f64) -> rusqlite::Result<(i64, bool)> {
    let sha_ = doc["config_sha256"].as_str().unwrap_or("").to_string();
    let gen = doc["generation"].as_i64().unwrap_or(0);
    let managed = doc["managed"].clone();
    let at = doc["observed_at"].as_f64().unwrap_or(now);
    let tx = c.transaction()?;
    tx.execute(
        "INSERT INTO relay_observed(relay, config_sha256, generation, managed, observed_at, received_at) VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(relay) DO UPDATE SET config_sha256=excluded.config_sha256, generation=excluded.generation, managed=excluded.managed,
           observed_at=excluded.observed_at, received_at=excluded.received_at",
        params![relay, sha_, gen, canonical(&managed), at, now],
    )?;
    let mut adopted = false;
    let cur = current_spec(&tx, relay);
    if let Some(s) = &cur {
        let applied_sha = last_applied(&tx, relay).map(|x| x.1).unwrap_or_default();
        let pending = gen < s.generation; // relayd 还没应用到最新一代：不收编，等它应用
        if !pending && sha_ != applied_sha && sha(&managed) != s.spec_sha256 && managed.is_object() {
            insert_spec(&tx, relay, &managed, &sha_, "adopted_external_change", "system", now)?;
            adopted = true;
        }
    }
    let g = current_spec(&tx, relay).map(|s| s.generation).unwrap_or(0);
    tx.commit()?;
    Ok((g, adopted))
}

/// 写操作的基准：当前期望（已接管）或最新观测（尚未接管）。返回 (managed, base_config_sha256)
pub fn base_for_write(c: &Connection, relay: &str) -> Result<(Value, String), String> {
    let obs = observed(c, relay);
    match current_spec(c, relay) {
        Some(s) => {
            // 基准 sha：relayd 应用最新一代后报的 sha（没有就用观测）
            let base = last_applied(c, relay).filter(|(g, _)| *g == s.generation).map(|x| x.1)
                .or_else(|| obs.as_ref().map(|o| o.0.clone())).unwrap_or_default();
            Ok((s.managed, base))
        }
        None => obs.map(|(sha_, _, m, _)| (m, sha_)).ok_or_else(|| "这台线路机的 relayd 还没上报过配置（观测），不能下发".to_string()),
    }
}

pub fn pending_commands(c: &Connection, relay: &str, now: f64) -> Vec<Value> {
    let mut st = c.prepare("SELECT id, kind, args, op_id, expires_at FROM relay_commands WHERE relay=? AND status='pending' AND expires_at>? ORDER BY created_at").unwrap();
    st.query_map(params![relay, now], |r| {
        let a: String = r.get(2)?;
        Ok(json!({"id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "args": serde_json::from_str::<Value>(&a).unwrap_or(Value::Null),
            "op_id": r.get::<_, Option<String>>(3)?, "expires_at": r.get::<_, f64>(4)?}))
    })
    .unwrap()
    .filter_map(Result::ok)
    .collect()
}

/// relayd 取期望：Some(文档) 或 None（304）
pub fn spec_doc(c: &Connection, relay: &str, have: i64, now: f64) -> Option<Value> {
    let cmds = pending_commands(c, relay, now);
    let Some(s) = current_spec(c, relay) else {
        // 尚未接管（generation 0，跟随文件）但有命令（例如 restart_xray）：只下发命令，managed 为 null
        if cmds.is_empty() {
            return None;
        }
        return Some(json!({"schema": "fleet.relayspec.v1", "relay": relay, "generation": 0, "spec_sha256": Value::Null,
            "issued_at": now, "base_config_sha256": Value::Null, "managed": Value::Null, "commands": cmds}));
    };
    if s.generation == have && cmds.is_empty() {
        return None;
    }
    Some(json!({"schema": "fleet.relayspec.v1", "relay": relay, "generation": s.generation, "spec_sha256": s.spec_sha256,
        "issued_at": s.created_at, "base_config_sha256": s.base_config_sha256, "managed": s.managed, "commands": cmds}))
}

pub fn create_op(c: &Connection, kind: &str, relay: &str, generation: Option<i64>, target: &str, detail: &Value, actor: &str, now: f64) -> rusqlite::Result<String> {
    let id = new_id("op");
    c.execute(
        "INSERT INTO ops(id, kind, relay, generation, target, detail, status, created_by, created_at, expires_at) VALUES(?,?,?,?,?,?,'pending',?,?,?)",
        params![id, kind, relay, generation, target, detail.to_string(), actor, now, now + OP_TTL_S],
    )?;
    Ok(id)
}

pub fn create_command(c: &Connection, relay: &str, kind: &str, args: &Value, op_id: &str, now: f64) -> rusqlite::Result<String> {
    let id = new_id("cmd");
    c.execute(
        "INSERT INTO relay_commands(id, relay, kind, args, op_id, created_at, expires_at, status) VALUES(?,?,?,?,?,?,?,'pending')",
        params![id, relay, kind, args.to_string(), op_id, now, now + OP_TTL_S],
    )?;
    Ok(id)
}

/// 回执：更新 op 与命令；被拒的一代生成“回到上次已应用内容”的新一代。返回受影响的 op id。
pub fn record_ack(c: &mut Connection, relay: &str, doc: &Value, now: f64) -> rusqlite::Result<Vec<String>> {
    let gen = doc["generation"].as_i64().unwrap_or(-1);
    let status = doc["status"].as_str().unwrap_or("rejected").to_string();
    let tx = c.transaction()?;
    tx.execute(
        "INSERT INTO relay_acks(relay, generation, status, config_sha256_after, doc, received_at) VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(relay, generation) DO UPDATE SET status=excluded.status, config_sha256_after=excluded.config_sha256_after,
           doc=excluded.doc, received_at=excluded.received_at",
        params![relay, gen, status, doc["config_sha256_after"].as_str(), doc.to_string(), now],
    )?;
    let mut touched = vec![];
    // 本代及更早仍 pending 的 spec 类 op
    let ids: Vec<String> = {
        let mut st = tx.prepare("SELECT id FROM ops WHERE relay=? AND generation IS NOT NULL AND generation<=? AND status='pending'")?;
        let v = st.query_map(params![relay, gen], |r| r.get(0))?.filter_map(Result::ok).collect();
        v
    };
    let result = json!({"status": status, "diff": doc.get("diff"), "runtime": doc.get("runtime"), "results": doc.get("results"),
        "config_sha256_before": doc.get("config_sha256_before"), "config_sha256_after": doc.get("config_sha256_after"), "applied_at": doc.get("applied_at")});
    for id in &ids {
        tx.execute("UPDATE ops SET status=?, acked_at=?, result=?, error=? WHERE id=?",
            params![status, now, result.to_string(), doc["error"].as_str(), id])?;
        touched.push(id.clone());
    }
    // 命令结果
    if let Some(cmds) = doc["commands"].as_array() {
        for cmd in cmds {
            let Some(cid) = cmd["id"].as_str() else { continue };
            let ok = cmd["ok"].as_bool().unwrap_or(false);
            tx.execute("UPDATE relay_commands SET status=?, result=? WHERE id=?", params![if ok { "done" } else { "failed" }, cmd.to_string(), cid])?;
            let op: Option<String> = tx.query_row("SELECT op_id FROM relay_commands WHERE id=?", [cid], |r| r.get(0)).optional()?.flatten();
            if let Some(op) = op {
                tx.execute("UPDATE ops SET status=?, acked_at=?, result=?, error=? WHERE id=? AND status='pending'",
                    params![if ok { "applied" } else { "rejected" }, now, json!({"command": cmd}).to_string(), cmd["error"].as_str(), op])?;
                touched.push(op);
            }
        }
    }
    if status == "rejected" {
        if let Some(cur) = current_spec(&tx, relay) {
            // 回退本身被拒就不再回退（避免循环）；停在这一代等人处理
            let reason: String = tx.query_row("SELECT reason FROM relay_specs WHERE relay=? AND generation=?", params![relay, gen], |r| r.get(0)).unwrap_or_default();
            if cur.generation == gen && !reason.starts_with("revert_") {
                // 回到上次已应用内容（没有就回到观测），后续写操作不叠在没生效的改动上
                let back = last_applied(&tx, relay)
                    .and_then(|(g, sha_)| {
                        tx.query_row("SELECT managed FROM relay_specs WHERE relay=? AND generation=?", params![relay, g], |r| r.get::<_, String>(0))
                            .ok()
                            .map(|m| (serde_json::from_str::<Value>(&m).unwrap_or(Value::Null), sha_))
                    })
                    .or_else(|| observed(&tx, relay).map(|o| (o.2, o.0)));
                if let Some((m, sha_)) = back {
                    insert_spec(&tx, relay, &m, &sha_, &format!("revert_rejected_{gen}"), "system", now)?;
                }
            }
        }
    }
    tx.commit()?;
    Ok(touched)
}

pub fn expire_ops(c: &Connection, now: f64) -> rusqlite::Result<usize> {
    // 过期的一代如果还是最新：回到上次已应用内容，免得 relayd 以后上线时把早已放弃的改动应用下去
    let stale: Vec<(String, i64)> = {
        let mut st = c.prepare("SELECT relay, generation FROM ops WHERE status='pending' AND expires_at<? AND generation IS NOT NULL")?;
        let v = st.query_map([now], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(Result::ok).collect();
        v
    };
    for (relay, g) in stale {
        if current_spec(c, &relay).map(|s| s.generation) == Some(g) {
            let back = last_applied(c, &relay)
                .and_then(|(ag, sha_)| {
                    c.query_row("SELECT managed FROM relay_specs WHERE relay=? AND generation=?", params![relay, ag], |r| r.get::<_, String>(0))
                        .ok()
                        .map(|m| (serde_json::from_str::<Value>(&m).unwrap_or(Value::Null), sha_))
                })
                .or_else(|| observed(c, &relay).map(|o| (o.2, o.0)));
            if let Some((m, sha_)) = back {
                insert_spec(c, &relay, &m, &sha_, &format!("revert_expired_{g}"), "system", now)?;
            }
        }
    }
    let n = c.execute("UPDATE ops SET status='expired', error='relayd 超时未回执' WHERE status='pending' AND expires_at<?", [now])?;
    c.execute("UPDATE relay_commands SET status='expired' WHERE status='pending' AND expires_at<?", [now])?;
    Ok(n)
}

pub fn op_json(c: &Connection, id: &str) -> Option<Value> {
    c.query_row("SELECT id, kind, relay, generation, target, detail, status, created_by, created_at, acked_at, result, error, expires_at FROM ops WHERE id=?", [id], op_row)
        .optional()
        .ok()
        .flatten()
}

pub fn op_row(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let d: String = r.get(5)?;
    let res: Option<String> = r.get(10)?;
    Ok(json!({"op_id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "relay": r.get::<_, String>(2)?,
        "generation": r.get::<_, Option<i64>>(3)?, "target": r.get::<_, String>(4)?,
        "detail": serde_json::from_str::<Value>(&d).unwrap_or(Value::Null), "status": r.get::<_, String>(6)?,
        "created_by": r.get::<_, String>(7)?, "created_at": r.get::<_, f64>(8)?, "acked_at": r.get::<_, Option<f64>>(9)?,
        "result": res.and_then(|x| serde_json::from_str::<Value>(&x).ok()), "error": r.get::<_, Option<String>>(11)?,
        "expires_at": r.get::<_, f64>(12)?}))
}

// ─── 对受管集合的修改 ──────────────────────────────────────────────────────────────────────────────

pub fn inbound_port(ib: &Value) -> Option<i64> {
    ib["port"].as_i64().or_else(|| ib["tag"].as_str().and_then(|t| t.strip_prefix("inbound-")).and_then(|p| p.parse().ok()))
}

pub fn find_inbound(managed: &Value, port: i64) -> Option<usize> {
    managed["inbounds"].as_array()?.iter().position(|ib| inbound_port(ib) == Some(port))
}

/// 轮换入站里所有客户端的 id（VLESS UUID）。返回 [(旧前 8 位, 新前 8 位)]
pub fn rotate_clients(ib: &mut Value) -> Vec<(String, String)> {
    let mut out = vec![];
    if let Some(cs) = ib.pointer_mut("/settings/clients").and_then(|c| c.as_array_mut()) {
        for c in cs.iter_mut() {
            if let Some(old) = c["id"].as_str().map(String::from) {
                let new = onboard::steps::uuid_v4();
                out.push((old.chars().take(8).collect(), new.chars().take(8).collect()));
                c["id"] = json!(new);
            }
        }
    }
    out
}

/// vless 分享链接（与上架流水线 P16 同格式）
pub fn share_link(ib: &Value, public_host: &str, name: &str) -> Option<String> {
    let id = ib.pointer("/settings/clients/0/id")?.as_str()?;
    let flow = ib.pointer("/settings/clients/0/flow").and_then(|f| f.as_str()).unwrap_or("xtls-rprx-vision");
    let port = inbound_port(ib)?;
    let rs = ib.pointer("/streamSettings/realitySettings")?;
    let pbk = rs.get("publicKey").and_then(|x| x.as_str()).map(String::from)
        .or_else(|| rs.get("privateKey").and_then(|x| x.as_str()).and_then(onboard::steps::reality_public))?;
    let sni = rs.pointer("/serverNames/0").and_then(|x| x.as_str()).unwrap_or("www.cloudflare.com");
    let sid = rs.pointer("/shortIds/0").and_then(|x| x.as_str()).unwrap_or("");
    let enc: String = name.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    Some(format!("vless://{id}@{public_host}:{port}?encryption=none&flow={flow}&security=reality&sni={sni}&fp=chrome&pbk={pbk}&sid={sid}&type=tcp#{enc}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c
    }

    fn managed(uuid: &str) -> Value {
        json!({"inbounds": [{"tag": "inbound-20007", "port": 20007, "protocol": "vless",
            "settings": {"clients": [{"id": uuid, "flow": "xtls-rprx-vision", "email": "node-07"}]},
            "streamSettings": {"security": "reality", "realitySettings": {"privateKey": "cH0QzGzV4b1cM0W3Sb0V6pYv5yD6z9z0bLzY2W1XxXg", "serverNames": ["www.cloudflare.com"], "shortIds": ["ab12"]}}}],
            "outbounds": [{"tag": "ss-1-2-3-4", "protocol": "shadowsocks"}], "routing_rules": []})
    }

    #[test]
    fn lifecycle_write_ack_reject_adopt() {
        let mut c = db();
        // 未接管：没有期望 → 304
        assert!(spec_doc(&c, "line3", 0, 1.0).is_none());
        assert!(base_for_write(&c, "line3").is_err(), "没观测不能写");
        record_observed(&mut c, "line3", &json!({"config_sha256": "s0", "generation": 0, "managed": managed("aaaaaaaa-0000-4000-8000-000000000000")}), 10.0).unwrap();
        // 第一次写：基于观测
        let (mut m, base) = base_for_write(&c, "line3").unwrap();
        assert_eq!(base, "s0");
        let i = find_inbound(&m, 20007).unwrap();
        let rot = rotate_clients(&mut m["inbounds"][i]);
        assert_eq!(rot[0].0, "aaaaaaaa");
        let g = insert_spec(&c, "line3", &m, &base, "rotate_uuid", "admin", 20.0).unwrap();
        assert_eq!(g, 1);
        let op = create_op(&c, "rotate_uuid", "line3", Some(g), "20007", &json!({}), "admin", 20.0).unwrap();
        let d = spec_doc(&c, "line3", 0, 21.0).unwrap();
        assert_eq!(d["generation"], 1);
        assert_eq!(d["base_config_sha256"], "s0");
        assert!(spec_doc(&c, "line3", 1, 21.0).is_none(), "已是最新一代 → 304");
        // 回执 applied
        let t = record_ack(&mut c, "line3", &json!({"generation": 1, "status": "applied", "config_sha256_after": "s1", "diff": {"inbounds_changed": 1}}), 30.0).unwrap();
        assert_eq!(t, vec![op.clone()]);
        assert_eq!(op_json(&c, &op).unwrap()["status"], "applied");
        assert_eq!(op_json(&c, &op).unwrap()["result"]["diff"]["inbounds_changed"], 1);
        // 观测回到 relayd 写后的 sha：不收编
        record_observed(&mut c, "line3", &json!({"config_sha256": "s1", "generation": 1, "managed": m.clone()}), 40.0).unwrap();
        assert_eq!(current_spec(&c, "line3").unwrap().generation, 1);
        // 外部写入者（onboard 加了入站）：sha 变、受管集合变 → 收编为第 2 代
        let mut ext = m.clone();
        ext["inbounds"].as_array_mut().unwrap().push(json!({"tag": "inbound-20008", "port": 20008}));
        let (g2, adopted) = record_observed(&mut c, "line3", &json!({"config_sha256": "s2", "generation": 1, "managed": ext}), 50.0).unwrap();
        assert!(adopted);
        assert_eq!(g2, 2);
        // 第 3 代被拒 → 生成第 4 代回到第 1 代（最近 applied）的内容
        let (m3, base3) = base_for_write(&c, "line3").unwrap();
        assert_eq!(m3["inbounds"].as_array().unwrap().len(), 2);
        let g3 = insert_spec(&c, "line3", &json!({"inbounds": []}), &base3, "x", "admin", 60.0).unwrap();
        let op3 = create_op(&c, "disable", "line3", Some(g3), "20007", &json!({}), "admin", 60.0).unwrap();
        record_ack(&mut c, "line3", &json!({"generation": g3, "status": "rejected", "error": "guard:no_plain_route"}), 70.0).unwrap();
        assert_eq!(op_json(&c, &op3).unwrap()["status"], "rejected");
        let cur = current_spec(&c, "line3").unwrap();
        assert_eq!(cur.generation, g3 + 1);
        assert_eq!(cur.managed["inbounds"].as_array().unwrap().len(), 1, "回到最近一次 applied 的第 1 代");
        // 过期
        let op4 = create_op(&c, "restart_xray", "line3", None, "", &json!({}), "admin", 100.0).unwrap();
        let cmd = create_command(&c, "line3", "restart_xray", &json!({}), &op4, 100.0).unwrap();
        assert_eq!(spec_doc(&c, "line3", cur.generation, 101.0).unwrap()["commands"][0]["id"], cmd, "有待执行命令时即使代数相同也返回");
        record_ack(&mut c, "line3", &json!({"generation": cur.generation, "status": "applied", "commands": [{"id": cmd, "ok": true}]}), 110.0).unwrap();
        assert_eq!(op_json(&c, &op4).unwrap()["status"], "applied");
        let op5 = create_op(&c, "reset_traffic", "line3", None, "", &json!({}), "admin", 200.0).unwrap();
        expire_ops(&c, 200.0 + OP_TTL_S + 1.0).unwrap();
        assert_eq!(op_json(&c, &op5).unwrap()["status"], "expired");
    }

    #[test]
    fn share_link_format() {
        let m = managed("11111111-2222-4333-8444-555555555555");
        let l = share_link(&m["inbounds"][0], "38.58.57.69", "团队1-31").unwrap();
        assert!(l.starts_with("vless://11111111-2222-4333-8444-555555555555@38.58.57.69:20007?encryption=none&flow=xtls-rprx-vision&security=reality&sni=www.cloudflare.com&fp=chrome&pbk="));
        assert!(l.contains("&sid=ab12&type=tcp#%E5%9B%A2"));
    }
}
