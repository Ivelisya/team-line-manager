//! 旧系统写接口的原生实现（落在原生旧库上）：资产编辑/续费、监控档案、故障确认、行为确认、API 密钥、策略草稿。
//! 逐行对照 store.edit_asset / monitor.profile / monitor.ack / behavior.ack / apikeys.* / server.policy_save 与 discard；
//! 错误文案、审计记录（actor[:100]、target[:150]、detail = Python dumps）与返回值同形。
//! 策略发布（apply）在 policy_apply.rs：编译成 relayspec 的 policy-* 规则，由 relayd 下发。
use super::py::*;
use super::pyjson::dumps;
use super::Ctx;
use chrono::TimeZone;
use rusqlite::params;
use serde_json::{json, Map, Value};

pub fn audit(cx: &Ctx, actor: &str, action: &str, target: &str, detail: &Value) -> R<()> {
    cx.db.execute(
        "INSERT INTO audit(ts,actor,action,target,detail) VALUES(?,?,?,?,?)",
        params![cx.now, head(actor, 100), action, head(target, 150), dumps(detail)],
    )?;
    Ok(())
}

/// Python `str(x)`（仅覆盖 JSON 标量；对象/数组用 JSON 文本近似）
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) if n.is_f64() => super::pyjson::float_repr(n.as_f64().unwrap_or(0.0)),
        Value::Number(n) => n.to_string(),
        v => v.to_string(),
    }
}

/// 十进制金额（Python Decimal(str(x)) 的子集）：返回 (以 1/10^scale 为单位的整数, scale)
fn parse_decimal(s: &str) -> Option<(i128, u32)> {
    let t = s.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i32>().ok()?),
        None => (body, 0),
    };
    let (ip, fp) = mant.split_once('.').unwrap_or((mant, ""));
    if (ip.is_empty() && fp.is_empty()) || !ip.chars().all(|c| c.is_ascii_digit()) || !fp.chars().all(|c| c.is_ascii_digit()) || ip.len() + fp.len() > 30 {
        return None;
    }
    let digits = format!("{ip}{fp}");
    let mut v: i128 = digits.parse().ok()?;
    let mut scale = fp.len() as i32 - exp;
    while scale < 0 {
        v = v.checked_mul(10)?;
        scale += 1;
    }
    Some((if neg { -v } else { v }, scale as u32))
}

fn to_scale(v: i128, from: u32, to: u32) -> Option<i128> {
    if from <= to {
        v.checked_mul(10i128.checked_pow(to - from)?)
    } else {
        let d = 10i128.pow(from - to);
        (v % d == 0).then_some(v / d)
    }
}

/// store.edit_asset
pub fn edit_asset(cx: &Ctx, data: &Value, actor: &str) -> R<Value> {
    let uuid = get(data, "uuid").clone();
    let row = rows(cx.db, "SELECT data FROM assets WHERE uuid=?", &[st(uuid.as_str().unwrap_or(""))])?;
    let Some(row) = row.into_iter().next() else { return Err(Err::Bad("节点不存在".into())) };
    let old = loads(&row["data"])?;
    let old_rev = get_or(&old, "revision", i(1));
    if !super::store::py_eq(&get_or(data, "expected_revision", Value::Null), &old_rev) {
        return Err(Err::Bad("资产信息已变化，请刷新后重试".into()));
    }
    let mut new = old.as_object().cloned().unwrap_or_default();
    if let Some(v) = data.get("capacity_bps") {
        if !v.is_null() {
            let ok = matches!(v, Value::Number(_)) && num(v).is_some_and(|x| x.is_finite() && (1.0..=1e13).contains(&x));
            if !ok {
                return Err(Err::Bad("带宽须为有效的 bit/s 数值".into()));
            }
        }
        new.insert("capacity_bps".into(), v.clone());
    }
    for key in ["name", "note"] {
        if let Some(v) = data.get(key) {
            let limit = if key == "note" { 500 } else { 100 };
            new.insert(key.into(), json!(head(py_str(v).trim(), limit)));
        }
    }
    if new.get("name").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
        return Err(Err::Bad("名称不能为空".into()));
    }
    if new["name"] != old["name"] {
        for a in super::store::raw_assets(cx)? {
            if a["uuid"] != old["uuid"] && a["name"] == new["name"] {
                return Err(Err::Bad("名称已被其他资产使用".into()));
            }
        }
    }
    if get(&old, "team") == "线路机" && old["name"] != new["name"] {
        return Err(Err::Bad("线路机名称是管理通道标识，请通过备注补充别名，避免改变现有绑定".into()));
    }
    if let Some(p) = data.get("price") {
        let bad = || Err::Bad("请输入有效的 USD 月付金额，最多两位小数".into());
        let (v, sc) = parse_decimal(&py_str(p)).ok_or_else(bad)?;
        let cents = to_scale(v, sc, 2).ok_or_else(bad)?; // 多于两位且非零 → 无效
        if cents < 0 || cents > 10_000_000 {
            return Err(bad());
        }
        new.insert("price_cents".into(), json!(cents as i64));
        new.insert("price".into(), json!(format!("{}.{:02}", cents / 100, cents % 100)));
    }
    if let Some(d) = data.get("due_date") {
        let s = d.as_str().ok_or_else(|| Err::Bad(format!("fromisoformat: argument must be str")))?;
        let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").or_else(|_| chrono::NaiveDate::parse_from_str(s, "%Y%m%d")).map_err(|_| Err::Bad(format!("Invalid isoformat string: '{s}'")))?;
        let zone_name = new.get("billing_timezone").and_then(|v| v.as_str()).unwrap_or("Asia/Shanghai").to_string();
        let zone: chrono_tz::Tz = zone_name.parse().map_err(|_| Err::Bad(format!("'No time zone found with key {zone_name}'")))?;
        let dtz = zone.from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap()).earliest().ok_or_else(|| Err::Bad("到期日无效".into()))?;
        new.insert("due_date".into(), json!(date.format("%Y-%m-%d").to_string()));
        new.insert("expired_at".into(), json!(dtz.format("%Y-%m-%dT%H:%M:%S%:z").to_string()));
    }
    let renewal = truthy(get(data, "renewal"));
    if renewal {
        let nd = new.get("due_date").and_then(|v| v.as_str()).unwrap_or("");
        let od = get(&old, "due_date").as_str().unwrap_or("");
        if nd <= od {
            return Err(Err::Bad("登记续费时，新到期日必须晚于原到期日".into()));
        }
        cx.db.execute(
            "INSERT INTO renewals(ts,asset,actor,amount_cents,old_due,new_due,note) VALUES(?,?,?,?,?,?,?)",
            params![cx.now, old["uuid"].as_str(), actor, new.get("price_cents").and_then(|v| v.as_i64()), od, nd, head(&py_str(&get_or(data, "renewal_note", json!(""))), 500)],
        )?;
    }
    new.insert("revision".into(), add(&old_rev, &i(1)));
    new.insert("updated_at".into(), f(cx.now));
    new.insert("sync_pending".into(), json!(true));
    let newv = Value::Object(new);
    cx.db.execute("UPDATE assets SET data=? WHERE uuid=?", params![dumps(&newv), newv["uuid"].as_str()])?;
    audit(cx, actor, if renewal { "renewal" } else { "asset_edit" }, newv["uuid"].as_str().unwrap_or(""), &json!({"before": old, "after": newv}))?;
    Ok(newv)
}

/// monitor.Monitor.profile
pub fn node_profile(cx: &Ctx, data: &Value, actor: &str) -> R<Value> {
    let uuid = get(data, "uuid").clone();
    let mut asset = super::store::raw_assets(cx)?.into_iter().find(|a| a["uuid"] == uuid).ok_or_else(|| Err::Bad("节点信息已变化，请刷新详情后重试".into()))?;
    let rev = get_or(&asset, "revision", i(1));
    if !super::store::py_eq(&rev, &get_or(data, "expected_revision", Value::Null)) {
        return Err(Err::Bad("节点信息已变化，请刷新详情后重试".into()));
    }
    let raw = get_or(data, "profile", json!({}));
    if !raw.is_object() {
        return Err(Err::Bad("节点档案格式无效".into()));
    }
    let mu = match get(&raw, "maintenance_until") {
        Value::Null => 0.0,
        Value::String(s) if s.is_empty() => 0.0,
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => super::store::iso_ts(s).ok_or_else(|| Err::Bad(format!("Invalid isoformat string: '{s}'")))?,
        _ => return Err(Err::Bad("节点档案格式无效".into())),
    };
    let mut clean = Map::new();
    let mu_v = match get(&raw, "maintenance_until") {
        Value::Number(n) if !n.is_f64() => i(n.as_i64().unwrap_or(0)),
        Value::Null => i(0),
        Value::String(s) if s.is_empty() => i(0),
        _ => f(mu),
    };
    clean.insert("maintenance_until".into(), mu_v);
    if mu != 0.0 && !(cx.now < mu && mu <= cx.now + 7.0 * 86400.0) {
        return Err(Err::Bad("维护窗口须为未来 7 天内；留空表示结束维护".into()));
    }
    let mut th = Map::new();
    for (key, default) in [("cpu_percent", 90.0), ("memory_percent", 90.0), ("disk_percent", 85.0), ("latency_ms", 3000.0)] {
        let v = get(get(&raw, "thresholds"), key);
        let x = match v {
            Value::Null => default,
            Value::String(s) => py_float(s)?,
            Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
            Value::Bool(b) => *b as i64 as f64,
            _ => return Err(Err::Bad("float() argument must be a string or a real number".into())),
        };
        let (lo, hi) = if key == "latency_ms" { (100.0, 60000.0) } else { (1.0, 100.0) };
        if !x.is_finite() || !(lo..=hi).contains(&x) {
            return Err(Err::Bad("监控阈值超出允许范围".into()));
        }
        th.insert(key.into(), f(x));
    }
    clean.insert("thresholds".into(), Value::Object(th));
    let previous = get_or(&asset, "profile", json!({}));
    let m = asset.as_object_mut().unwrap();
    m.insert("profile".into(), Value::Object(clean.clone()));
    m.insert("revision".into(), add(&rev, &i(1)));
    m.insert("updated_at".into(), f(cx.now));
    cx.db.execute("UPDATE assets SET data=? WHERE uuid=?", params![dumps(&asset), asset["uuid"].as_str()])?;
    audit(cx, actor, "node_profile", asset["uuid"].as_str().unwrap_or(""), &json!({"before": previous, "after": clean}))?;
    Ok(asset)
}

fn int_param(v: &Value) -> R<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|x| x.trunc() as i64)).ok_or(Err::Bad("id 无效".into())),
        Value::String(s) => py_int(s),
        Value::Null => Err(Err::NotFound),
        _ => Err(Err::Bad("int() argument must be a string, a bytes-like object or a real number".into())),
    }
}

/// monitor.Monitor.ack
pub fn incident_ack(cx: &Ctx, data: &Value, actor: &str) -> R<Value> {
    let note = head(&py_str(&get_or(data, "note", json!(""))), 1000);
    let id = int_param(get(data, "id"))?;
    let n = cx.db.execute("UPDATE incidents SET ack_at=?,ack_actor=?,note=? WHERE id=?", params![cx.now, actor, note, id])?;
    if n == 0 {
        return Err(Err::Bad("故障记录不存在".into()));
    }
    audit(cx, actor, "incident_ack", &py_str(get(data, "id")), &json!({"note": note}))?;
    Ok(json!({"ok": true}))
}

/// behavior.Behavior.ack
pub fn behavior_ack(cx: &Ctx, data: &Value, actor: &str) -> R<Value> {
    let id = int_param(get(data, "id"))?;
    let n = cx.db.execute("UPDATE behavior_events SET ack_at=? WHERE id=?", params![cx.now, id])?;
    if n == 0 {
        return Err(Err::Bad("行为提示不存在".into()));
    }
    audit(cx, actor, "behavior_ack", &py_str(get(data, "id")), &json!({}))?;
    Ok(json!({"ok": true}))
}

fn b64url_nopad(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut s = String::new();
    for ch in b.chunks(3) {
        let n = ((ch[0] as u32) << 16) | ((*ch.get(1).unwrap_or(&0) as u32) << 8) | *ch.get(2).unwrap_or(&0) as u32;
        for k in 0..=ch.len() {
            s.push(T[((n >> (18 - 6 * k)) & 63) as usize] as char);
        }
    }
    s
}

/// apikeys.create（+ server 里的审计）
pub fn api_key_create(cx: &Ctx, data: &Value, actor: &str) -> R<Value> {
    let scope = data.get("scope").ok_or(Err::NotFound)?;
    if !matches!(scope.as_str(), Some("read" | "write" | "full")) {
        return Err(Err::Bad("权限无效".into()));
    }
    let expires = data.get("expires_days").ok_or(Err::NotFound)?.clone();
    let name_v = data.get("name").ok_or(Err::NotFound)?;
    let name = name_v.as_str().map(|s| s.trim().to_string()).filter(|s| (1..=100).contains(&s.chars().count())).ok_or_else(|| Err::Bad("密钥名称需为 1–100 个字符".into()))?;
    let days = match &expires {
        Value::Number(n) if !n.is_f64() => n.as_i64().filter(|d| (0..=3650).contains(d)),
        _ => None,
    }
    .ok_or_else(|| Err::Bad("有效期需为 0–3650 的整数天，0 表示长期有效".into()))?;
    let key = format!("tlm_{}", b64url_nopad(&crate::accounts::random_bytes(32)));
    let id = hex::encode(crate::accounts::random_bytes(8));
    let created = cx.now;
    let exp = if days == 0 { Value::Null } else { f(created + (days * 86400) as f64) };
    let prefix: String = key.chars().take(12).collect();
    cx.db.execute(
        "INSERT INTO api_keys(id,name,scope,prefix,created_at,expires_at,key_hash) VALUES(?,?,?,?,?,?,?)",
        params![id, name, scope.as_str(), prefix, created, exp.as_f64(), crate::accounts::sha256_hex(&key)],
    )?;
    audit(cx, actor, "apikey.create", &format!("apikey:{id}"), &json!({"name": name_v, "scope": scope, "expires_days": expires}))?;
    Ok(json!({"id": id, "name": name, "scope": scope, "prefix": prefix, "created_at": f(created), "expires_at": exp, "key": key}))
}

pub fn api_key_revoke(cx: &Ctx, data: &Value, actor: &str) -> R<Value> {
    let id = data.get("id").ok_or(Err::NotFound)?;
    let ids = py_str(id);
    cx.db.execute("UPDATE api_keys SET revoked_at=? WHERE id=?", params![cx.now, ids])?;
    audit(cx, actor, "apikey.revoke", &format!("apikey:{ids}"), &json!({}))?;
    Ok(json!({"ok": true}))
}

/// apikeys.authenticate：有效密钥返回去掉 key_hash 的一行
pub fn api_key_auth(cx: &Ctx, key: &str) -> R<Option<Map<String, Value>>> {
    let h = crate::accounts::sha256_hex(key);
    let r = rows(cx.db, "SELECT * FROM api_keys WHERE key_hash=?", &[st(&h)])?;
    let Some(mut row) = r.into_iter().next() else { return Ok(None) };
    if !row["revoked_at"].is_null() || row["expires_at"].as_f64().is_some_and(|e| e <= cx.now) {
        return Ok(None);
    }
    row.shift_remove("key_hash");
    Ok(Some(row))
}

pub fn api_key_usage(cx: &Ctx, key_id: &str, method: &str, path: &str, status: u16, ip: &str) -> R<()> {
    cx.db.execute("INSERT INTO api_key_usage VALUES(?,?,?,?,?,?)", params![cx.now, key_id, method, path, status, ip])?;
    cx.db.execute("UPDATE api_keys SET last_used_at=?,use_count=use_count+1 WHERE id=?", params![cx.now, key_id])?;
    Ok(())
}

/// policy.normalize_domains（域名只接受 ASCII；国际化域名请填 xn-- 形式）
pub fn normalize_domains(v: &Value) -> R<Vec<String>> {
    let items: Vec<Value> = match v {
        Value::String(s) => s.trim().split(|c: char| c.is_whitespace() || c == ',' || c == '，').filter(|x| !x.is_empty()).map(|x| json!(x)).collect(),
        Value::Array(a) => a.clone(),
        _ => return Err(Err::Bad("每份规则最多 300 个域名".into())),
    };
    if items.len() > 300 {
        return Err(Err::Bad("每份规则最多 300 个域名".into()));
    }
    let mut out: Vec<String> = vec![];
    for it in items {
        let s = it.as_str().ok_or_else(|| Err::Bad("域名必须是文本".into()))?;
        let item = s.trim().to_lowercase();
        let item = item.trim_end_matches('.');
        if item.is_empty() {
            continue;
        }
        let exact = item.starts_with('=');
        let host = if exact { &item[1..] } else { item.strip_prefix("*.").unwrap_or(item) };
        if !host.is_ascii() {
            return Err(Err::Bad("域名格式无效".into()));
        }
        let label_ok = |l: &str| {
            let b = l.as_bytes();
            !b.is_empty() && b.len() <= 63 && b[0].is_ascii_alphanumeric() && b[b.len() - 1].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        };
        if host.len() > 253 || !host.contains('.') || !host.split('.').all(label_ok) {
            return Err(Err::Bad("只支持域名，例如 example.com 或 =example.com；请勿填写网址、端口或正则".into()));
        }
        if host.parse::<std::net::IpAddr>().is_ok() {
            return Err(Err::Bad("请填写域名，不支持 IP 地址白名单".into()));
        }
        let norm = format!("{}{host}", if exact { "=" } else { "" });
        if !out.contains(&norm) {
            out.push(norm);
        }
    }
    Ok(out)
}

pub fn validate_policy(p: &Value) -> R<Value> {
    let mode = get(p, "mode").as_str().unwrap_or("");
    if !p.is_object() || !["observe", "allowlist", "denylist", "blocked"].contains(&mode) {
        return Err(Err::Bad("请选择有效的访问模式".into()));
    }
    let domains = normalize_domains(&get_or(p, "domains", json!([])))?;
    if (mode == "allowlist" || mode == "denylist") && domains.is_empty() {
        return Err(Err::Bad("启用白名单或黑名单前必须填写域名".into()));
    }
    Ok(json!({"mode": mode, "domains": domains, "note": head(&py_str(&get_or(p, "note", json!(""))), 500)}))
}

/// server.policy_save
pub fn policy_draft(cx: &Ctx, req: &Value, actor: &str) -> R<Value> {
    let scope = get_or(req, "scope", json!("")).as_str().unwrap_or("").to_string();
    let assets: Vec<Value> = super::store::raw_assets(cx)?.into_iter().filter(|a| truthy(get(a, "relay_port"))).collect();
    let ok_scope = assets.iter().any(|a| format!("node:{}", a["uuid"].as_str().unwrap_or("")) == scope || format!("team:{}", a["team"].as_str().unwrap_or("")) == scope);
    if !ok_scope {
        return Err(Err::Bad("请选择团队或家宽节点".into()));
    }
    let rev = rows(cx.db, "SELECT rev FROM drafts WHERE scope=?", &[st(&scope)])?.into_iter().next().map(|r| r["rev"].as_i64().unwrap_or(0)).unwrap_or(0);
    if !super::store::py_eq(&get_or(req, "expected_revision", i(0)), &i(rev)) {
        return Err(Err::Bad("草稿已被修改，请重新加载".into()));
    }
    let raw = get_or(req, "policy", json!({}));
    let policy = if get(&raw, "mode") == "inherit" && scope.starts_with("node:") { json!({"mode": "inherit", "domains": [], "note": ""}) } else { validate_policy(&raw)? };
    cx.db.execute(
        "INSERT INTO drafts VALUES(?,?,?,?) ON CONFLICT(scope) DO UPDATE SET data=excluded.data,rev=excluded.rev,ts=excluded.ts",
        params![scope, dumps(&policy), rev + 1, cx.now],
    )?;
    audit(cx, actor, "policy_draft", &scope, &policy)?;
    Ok(json!({"ok": true, "revision": rev + 1}))
}

pub fn policy_discard(cx: &Ctx, req: &Value, actor: &str) -> R<Value> {
    let st_ = super::policy::policy_state(cx)?;
    if Some(st_["draft_hash"].clone()) != req.get("draft_hash").cloned() {
        return Err(Err::Bad("草稿已变化，请重新加载".into()));
    }
    cx.db.execute("DELETE FROM drafts", [])?;
    audit(cx, actor, "policy_discard", "all", &json!({}))?;
    Ok(json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decimals() {
        assert_eq!(parse_decimal("28.88"), Some((2888, 2)));
        assert_eq!(to_scale(2888, 2, 2), Some(2888));
        assert_eq!(parse_decimal("28").and_then(|(v, s)| to_scale(v, s, 2)), Some(2800));
        assert_eq!(parse_decimal("28.880").and_then(|(v, s)| to_scale(v, s, 2)), Some(2888));
        assert_eq!(parse_decimal("28.885").and_then(|(v, s)| to_scale(v, s, 2)), None);
        assert_eq!(parse_decimal("1e2").and_then(|(v, s)| to_scale(v, s, 2)), Some(10000));
        assert!(parse_decimal("abc").is_none());
    }
    #[test]
    fn domains() {
        assert_eq!(normalize_domains(&json!("Example.com, =A.org ，*.b.net")).unwrap(), vec!["example.com", "=a.org", "b.net"]);
        assert!(normalize_domains(&json!(["https://x.com"])).is_err());
        assert!(normalize_domains(&json!(["1.2.3.4"])).is_err());
    }
    #[test]
    fn key_format() {
        assert_eq!(b64url_nopad(&[0u8; 32]).len(), 43);
    }
}


/// 改资产的 IP（出口 IP 变化）：ip、egress_ip、名称里的旧 IP；`with_target` 时连接地址 target（与旧 IP 相同时）一起改并在备注里记一笔。
/// 审计 action = asset_ip，detail = {before, after, reason}。IP 没变 → Ok(None)。
pub fn set_asset_ip(cx: &Ctx, uuid: &str, new_ip: &str, actor: &str, reason: &str, with_target: bool) -> R<Option<Value>> {
    if new_ip.parse::<std::net::IpAddr>().is_err() {
        return Err(Err::Bad("IP 无效".into()));
    }
    let row = rows(cx.db, "SELECT data FROM assets WHERE uuid=?", &[st(uuid)])?;
    let Some(row) = row.into_iter().next() else { return Err(Err::Bad("节点不存在".into())) };
    let mut a = loads(&row["data"])?;
    let old_ip = get(&a, "ip").as_str().unwrap_or("").to_string();
    if old_ip == new_ip {
        return Ok(None);
    }
    let rev = get_or(&a, "revision", i(1));
    let m = a.as_object_mut().unwrap();
    m.insert("ip".into(), json!(new_ip));
    m.insert("egress_ip".into(), json!(new_ip));
    if !old_ip.is_empty() {
        if let Some(n) = m.get("name").and_then(|v| v.as_str()).map(|n| n.replace(&old_ip, new_ip)) {
            m.insert("name".into(), json!(n));
        }
        if with_target && m.get("target").and_then(|v| v.as_str()) == Some(old_ip.as_str()) {
            m.insert("target".into(), json!(new_ip));
        }
    }
    if with_target {
        let note = m.get("note").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let day = chrono::DateTime::from_timestamp(cx.now as i64 + 8 * 3600, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
        m.insert("note".into(), json!(head(&format!("{note}{}{day} 公网 IP 变更：{old_ip} → {new_ip}（{reason}）", if note.is_empty() { "" } else { "；" }), 500)));
    }
    m.insert("revision".into(), add(&rev, &i(1)));
    m.insert("updated_at".into(), f(cx.now));
    cx.db.execute("UPDATE assets SET data=? WHERE uuid=?", params![dumps(&a), uuid])?;
    audit(cx, actor, "asset_ip", uuid, &json!({"before": old_ip, "after": new_ip, "reason": reason}))?;
    Ok(Some(a))
}


/// 线路机上这个入口的出站地址变了（切换 / relayd 采纳外部改动）：把资产的 target / target_port 同步成线路机实际配置（审计 asset_target）
pub fn set_asset_target(cx: &Ctx, uuid: &str, target: &Value, port: &Value, actor: &str, reason: &str) -> R<bool> {
    let row = rows(cx.db, "SELECT data FROM assets WHERE uuid=?", &[st(uuid)])?;
    let Some(row) = row.into_iter().next() else { return Ok(false) };
    let mut a = loads(&row["data"])?;
    let (ot, op) = (get(&a, "target").clone(), get(&a, "target_port").clone());
    if &ot == target && super::store::py_eq(&op, port) {
        return Ok(false);
    }
    let rev = get_or(&a, "revision", i(1));
    let m = a.as_object_mut().unwrap();
    m.insert("target".into(), target.clone());
    m.insert("target_port".into(), port.clone());
    m.insert("revision".into(), add(&rev, &i(1)));
    m.insert("updated_at".into(), f(cx.now));
    cx.db.execute("UPDATE assets SET data=? WHERE uuid=?", params![dumps(&a), uuid])?;
    audit(cx, actor, "asset_target", uuid, &json!({"before": [ot, op], "after": [target, port], "reason": reason}))?;
    Ok(true)
}
