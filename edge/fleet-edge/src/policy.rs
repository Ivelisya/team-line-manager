//! 访问规则编译：逐字移植旧 `teamops/policy.py`（validate_policy / effective_policy / compile_routing / digest）。
//! 差别：域名只接受 ASCII（含 punycode `xn--`）；旧系统用 Python 的 idna 编码中文域名，这一步由 fleetd 在下发前完成。
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub const PREFIX: &str = "teamops-v1-";

/// json.dumps(value, sort_keys=True, separators=(",", ":"))，ensure_ascii=True
pub fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            out.push('{');
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                esc(k, out);
                out.push(':');
                canonical(&m[*k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(x, out);
            }
            out.push(']');
        }
        Value::String(s) => esc(s, out),
        other => out.push_str(&other.to_string()),
    }
}

fn esc(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7f => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn digest(v: &Value) -> String {
    let mut s = String::new();
    canonical(v, &mut s);
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn label_ok(l: &str) -> bool {
    let b = l.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_lowercase() | b[0].is_ascii_digit()
        && b[b.len() - 1].is_ascii_lowercase() | b[b.len() - 1].is_ascii_digit()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub fn normalize_domains(v: &Value) -> Result<Vec<String>, String> {
    let items: Vec<String> = match v {
        Value::String(s) => s.split(|c: char| c.is_whitespace() || c == ',' || c == '，').map(|x| x.to_string()).collect(),
        Value::Array(a) => {
            if a.len() > 300 {
                return Err("每份规则最多 300 个域名".into());
            }
            a.iter().map(|x| x.as_str().map(|s| s.to_string()).ok_or_else(|| "域名必须是文本".to_string())).collect::<Result<_, _>>()?
        }
        Value::Null => vec![],
        _ => return Err("每份规则最多 300 个域名".into()),
    };
    if items.len() > 300 {
        return Err("每份规则最多 300 个域名".into());
    }
    let mut out: Vec<String> = vec![];
    for item in items {
        let item = item.trim().to_lowercase();
        let item = item.trim_end_matches('.').to_string();
        if item.is_empty() {
            continue;
        }
        let exact = item.starts_with('=');
        let host = if exact { &item[1..] } else if let Some(h) = item.strip_prefix("*.") { h } else { &item[..] };
        if !host.is_ascii() {
            return Err("域名请使用 punycode（xn--）形式".into());
        }
        if host.len() > 253 || !host.contains('.') || !host.split('.').all(label_ok) {
            return Err("只支持域名，例如 example.com 或 =example.com；请勿填写网址、端口或正则".into());
        }
        if host.parse::<std::net::IpAddr>().is_ok() {
            return Err("请填写域名，不支持 IP 地址白名单".into());
        }
        let norm = format!("{}{}", if exact { "=" } else { "" }, host);
        if !out.contains(&norm) {
            out.push(norm);
        }
    }
    Ok(out)
}

pub fn validate_policy(p: &Value) -> Result<Value, String> {
    let mode = p["mode"].as_str().filter(|m| ["observe", "allowlist", "denylist", "blocked"].contains(m)).ok_or("请选择有效的访问模式")?;
    let domains = normalize_domains(&p["domains"])?;
    if (mode == "allowlist" || mode == "denylist") && domains.is_empty() {
        return Err("启用白名单或黑名单前必须填写域名".into());
    }
    let note: String = match &p["note"] {
        Value::Null => String::new(),
        Value::String(s) => s.chars().take(500).collect(),
        other => other.to_string().chars().take(500).collect(),
    };
    Ok(json!({"mode": mode, "domains": domains, "note": note}))
}

fn effective_policy<'a>(asset: &Value, policies: &'a Map<String, Value>, observe: &'a Value) -> &'a Value {
    let node = format!("node:{}", asset["uuid"].as_str().unwrap_or(""));
    let team = format!("team:{}", asset["team"].as_str().unwrap_or(""));
    policies.get(&node).or_else(|| policies.get(&team)).unwrap_or(observe)
}

/// compile_routing(routing, assets, policies)
pub fn compile_routing(routing: &Value, assets: &[Value], policies: &Map<String, Value>) -> Result<Value, String> {
    let mut result = routing.clone();
    let rules = routing["rules"].as_array().cloned().unwrap_or_default();
    let base: Vec<Value> = rules.into_iter().filter(|r| !r["ruleTag"].as_str().unwrap_or("").starts_with(PREFIX)).collect();
    let observe = json!({"mode": "observe", "domains": []});
    let mut custom = vec![];
    let tags: Vec<String> = assets.iter().map(|a| format!("inbound-{}", port_str(&a["relay_port"]))).collect();
    for a in assets {
        let p = validate_policy(effective_policy(a, policies, &observe))?;
        let mode = p["mode"].as_str().unwrap();
        if mode == "observe" {
            continue;
        }
        let tag = format!("inbound-{}", port_str(&a["relay_port"]));
        let uuid = a["uuid"].as_str().unwrap_or("");
        if mode == "allowlist" || mode == "denylist" {
            let domains: Vec<Value> = p["domains"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| {
                    let d = d.as_str().unwrap();
                    Value::String(match d.strip_prefix('=') {
                        Some(x) => format!("full:{x}"),
                        None => format!("domain:{d}"),
                    })
                })
                .collect();
            let out = if mode == "allowlist" { a["outbound_tag"].clone() } else { json!("blocked") };
            custom.push(json!({"type": "field", "inboundTag": [tag], "ruleTag": format!("{PREFIX}{uuid}-match"), "domain": domains, "outboundTag": out}));
        }
        if mode == "allowlist" || mode == "blocked" {
            custom.push(json!({"type": "field", "inboundTag": [tag], "ruleTag": format!("{PREFIX}{uuid}-default"), "outboundTag": "blocked"}));
        }
    }
    let index = base
        .iter()
        .position(|r| r["inboundTag"].as_array().is_some_and(|a| a.iter().any(|t| t.as_str().is_some_and(|t| tags.iter().any(|x| x == t)))))
        .unwrap_or(base.len());
    let mut out = base[..index].to_vec();
    out.extend(custom);
    out.extend_from_slice(&base[index..]);
    result["rules"] = Value::Array(out);
    Ok(result)
}

fn port_str(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_matches_python() {
        // python3 -c 'import json,hashlib;print(hashlib.sha256(json.dumps({"b":[1,"é"],"a":{"z":None,"y":True}},sort_keys=True,separators=(",",":")).encode()).hexdigest())'
        let v = json!({"b": [1, "é"], "a": {"z": null, "y": true}});
        let mut s = String::new();
        canonical(&v, &mut s);
        assert_eq!(s, "{\"a\":{\"y\":true,\"z\":null},\"b\":[1,\"\\u00e9\"]}");
    }

    #[test]
    fn compile_like_legacy() {
        let routing = json!({"domainStrategy": "AsIs", "rules": [
            {"type": "field", "inboundTag": ["api"], "outboundTag": "api"},
            {"type": "field", "ip": ["geoip:private"], "outboundTag": "blocked"},
            {"type": "field", "inboundTag": ["inbound-20002"], "outboundTag": "ss-a"},
            {"type": "field", "inboundTag": ["inbound-20001"], "outboundTag": "direct", "ruleTag": "fleet-explicit-inbound-20001"},
            {"type": "field", "inboundTag": ["inbound-20002"], "ruleTag": "teamops-v1-old-default", "outboundTag": "blocked"}]});
        let assets = vec![
            json!({"uuid": "u1", "team": "t", "relay_port": 20001, "outbound_tag": "direct"}),
            json!({"uuid": "u2", "team": "t", "relay_port": 20002, "outbound_tag": "ss-a"}),
        ];
        let pol: Map<String, Value> = serde_json::from_value(json!({
            "node:u1": {"mode": "allowlist", "domains": "Example.com, =x.org."},
            "team:t": {"mode": "blocked"}})).unwrap();
        let r = compile_routing(&routing, &assets, &pol).unwrap();
        let tags: Vec<&str> = r["rules"].as_array().unwrap().iter().map(|x| x["ruleTag"].as_str().unwrap_or(x["outboundTag"].as_str().unwrap())).collect();
        assert_eq!(tags, vec!["api", "blocked", "teamops-v1-u1-match", "teamops-v1-u1-default", "teamops-v1-u2-default", "ss-a", "fleet-explicit-inbound-20001"]);
        assert_eq!(r["rules"][2]["domain"], json!(["domain:example.com", "full:x.org"]));
        assert_eq!(r["rules"][2]["outboundTag"], "direct");
        assert!(validate_policy(&json!({"mode": "allowlist", "domains": []})).is_err());
        assert!(validate_policy(&json!({"mode": "denylist", "domains": ["1.2.3.4"]})).is_err());
        assert!(validate_policy(&json!({"mode": "denylist", "domains": ["http://x.com/"]})).is_err());
    }
}
