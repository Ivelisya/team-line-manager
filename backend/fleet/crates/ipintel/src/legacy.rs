//! 旧系统（/api/team-ops/*，经 fleetd 共享轮询器）数据 → 本 crate 的输入。
//!
//! - `/activity?limit=500` 的 `data.rows[]`：`{id, ts, asset, team, relay, source_ip, client, …}` → SourceEvent
//! - `/health` 的 `data.monitor.nodes{uuid: {name, team, relay, checks[]}}`：
//!   `checks[key=egress|ingress].evidence.{actual_ip, expected_ip}` + `measured_at` → EgressObs；
//!   `name`（「团队1-01 · ATT · 75.37.237.38」）→ NodeMeta（短名 t1-01，出口 IP 取 mapping/egress 的 expected）
use crate::service::{EgressObs, NodeMeta, SourceEvent};
use serde_json::Value;
use std::net::IpAddr;

fn data(v: &Value) -> &Value {
    // 同时接受 {"data": …} 和已拆包的 …（以及 fixtures 的 {"status","body":{"data"}}）
    if let Some(b) = v.get("body") {
        return data(b);
    }
    v.get("data").unwrap_or(v)
}

fn s(v: &Value, k: &str) -> Option<String> {
    match v.get(k) {
        Some(Value::String(x)) if !x.is_empty() => Some(x.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

pub fn activity_events(v: &Value) -> Vec<SourceEvent> {
    let d = data(v);
    let rows = d.get("rows").or_else(|| d.get("items")).and_then(|r| r.as_array()).cloned().or_else(|| d.as_array().cloned()).unwrap_or_default();
    rows.iter()
        .filter_map(|r| {
            let ip: IpAddr = r.get("source_ip")?.as_str()?.trim().parse().ok()?;
            let ts = r.get("ts")?.as_f64()?;
            let node = s(r, "asset")?;
            Some(SourceEvent { id: s(r, "id"), ts, node, relay: s(r, "relay"), team: s(r, "team"), source_ip: ip, client: s(r, "client") })
        })
        .collect()
}

/// 「团队1-01 · ATT · 75.37.237.38」→ t1-01；「线路机2 …」→ r2（与 fleetd rules::short_id 相同）。
pub fn short_id(name: &str) -> String {
    let head = name.split(" · ").next().unwrap_or(name).trim();
    if let Some(rest) = head.strip_prefix("团队") {
        if let Some((t, n)) = rest.split_once('-') {
            if t.chars().all(|c| c.is_ascii_digit()) && n.chars().all(|c| c.is_ascii_digit()) {
                return format!("t{t}-{n}");
            }
        }
    }
    if let Some(n) = head.strip_prefix("线路机") {
        let d: String = n.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !d.is_empty() {
            return format!("r{d}");
        }
    }
    head.to_string()
}

fn nodes_obj(v: &Value) -> Option<&serde_json::Map<String, Value>> {
    let d = data(v);
    d.pointer("/monitor/nodes").or_else(|| d.get("nodes")).and_then(|n| n.as_object())
}

pub fn health_nodes(v: &Value) -> Vec<NodeMeta> {
    let Some(nodes) = nodes_obj(v) else { return vec![] };
    nodes
        .iter()
        .map(|(uuid, n)| {
            let name = s(n, "name").unwrap_or_else(|| uuid.clone());
            let checks = n.get("checks").and_then(|c| c.as_array()).cloned().unwrap_or_default();
            let expected = checks
                .iter()
                .filter(|c| matches!(c.get("key").and_then(|k| k.as_str()), Some("egress") | Some("ingress")))
                .find_map(|c| c.pointer("/evidence/expected_ip").and_then(|x| x.as_str()).and_then(|x| x.parse::<IpAddr>().ok()))
                .or_else(|| name.rsplit(" · ").next().and_then(|x| x.trim().parse::<IpAddr>().ok()));
            let team = s(n, "team");
            let relay = s(n, "relay").or_else(|| if team.as_deref() == Some("线路机") { Some(name.split(" · ").next().unwrap_or(&name).trim().to_string()) } else { None });
            NodeMeta { node: uuid.clone(), short: short_id(&name), name: name.split(" · ").next().unwrap_or(&name).trim().to_string(), relay, team, egress_ip: expected }
        })
        .collect()
}

pub fn health_egress(v: &Value) -> Vec<EgressObs> {
    let Some(nodes) = nodes_obj(v) else { return vec![] };
    let mut out = Vec::new();
    for (uuid, n) in nodes {
        for c in n.get("checks").and_then(|c| c.as_array()).into_iter().flatten() {
            let key = c.get("key").and_then(|k| k.as_str()).unwrap_or("");
            if key != "egress" && key != "ingress" {
                continue;
            }
            let Some(ip) = c.pointer("/evidence/actual_ip").and_then(|x| x.as_str()).and_then(|x| x.trim().parse::<IpAddr>().ok()) else { continue };
            let Some(ts) = c.get("measured_at").and_then(|x| x.as_f64()) else { continue };
            let expected = c.pointer("/evidence/expected_ip").and_then(|x| x.as_str()).and_then(|x| x.parse().ok());
            out.push(EgressObs { node: uuid.clone(), ts, ip, via: key.to_string(), expected });
        }
    }
    out
}
