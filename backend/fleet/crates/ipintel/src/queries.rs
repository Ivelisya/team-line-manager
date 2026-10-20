//! 只读查询（API 用）。
use crate::lookup;
use crate::service::{anomaly_json, brief, IpIntel};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::IpAddr;

#[derive(Clone, Debug, Default)]
pub struct AnomalyFilter {
    /// open | closed | all
    pub status: Option<String>,
    pub kind: Option<String>,
    pub node: Option<String>,
    pub severity: Option<String>,
    pub since: Option<f64>,
    pub limit: Option<usize>,
}

impl IpIntel {
    /// 节点 id 解析：资产 uuid、短名（t1-01）或全名都行。
    pub fn resolve_node(&self, id: &str) -> Option<(String, Value)> {
        self.with_conn_ref(|c| resolve_node(c, id))
    }

    /// GET /ipintel/{ip}
    pub fn ip_detail(&self, ip: IpAddr, now: f64) -> Result<Value, String> {
        let ip = lookup::canonical(ip);
        let x = self.intel(ip, now);
        let ips = ip.to_string();
        self.with_conn_ref(|c| -> Result<Value, String> {
            let mut st = c
                .prepare(
                    "SELECT s.node, n.short, n.name, n.relay, n.team, s.first_seen, s.last_seen, s.count
                     FROM node_sources s LEFT JOIN nodes n ON n.node=s.node WHERE s.ip=?1 ORDER BY s.last_seen DESC LIMIT 200",
                )
                .map_err(|e| e.to_string())?;
            let nodes: Vec<Value> = st
                .query_map([&ips], |r| {
                    Ok(json!({"node": r.get::<_, Option<String>>(1)?, "node_uuid": r.get::<_, String>(0)?, "node_name": r.get::<_, Option<String>>(2)?,
                        "relay": r.get::<_, Option<String>>(3)?, "team": r.get::<_, Option<String>>(4)?,
                        "first_seen": r.get::<_, f64>(5)?, "last_seen": r.get::<_, f64>(6)?, "count": r.get::<_, i64>(7)?}))
                })
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            let mut st = c
                .prepare("SELECT relay, client, first_seen, last_seen, count FROM client_sources WHERE ip=?1 ORDER BY last_seen DESC LIMIT 50")
                .map_err(|e| e.to_string())?;
            let clients: Vec<Value> = st
                .query_map([&ips], |r| {
                    Ok(json!({"relay": r.get::<_, String>(0)?, "client": r.get::<_, String>(1)?, "first_seen": r.get::<_, f64>(2)?,
                        "last_seen": r.get::<_, f64>(3)?, "count": r.get::<_, i64>(4)?}))
                })
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            let mut st = c
                .prepare(
                    "SELECT e.node, n.short, n.name, e.first_seen, e.last_seen, e.count, e.via FROM egress_history e LEFT JOIN nodes n ON n.node=e.node
                     WHERE e.ip=?1 ORDER BY e.last_seen DESC",
                )
                .map_err(|e| e.to_string())?;
            let egress_of: Vec<Value> = st
                .query_map([&ips], |r| {
                    Ok(json!({"node": r.get::<_, Option<String>>(1)?, "node_uuid": r.get::<_, String>(0)?, "node_name": r.get::<_, Option<String>>(2)?,
                        "first_seen": r.get::<_, f64>(3)?, "last_seen": r.get::<_, f64>(4)?, "count": r.get::<_, i64>(5)?, "via": r.get::<_, Option<String>>(6)?}))
                })
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            let mut st = c
                .prepare("SELECT * FROM anomalies WHERE subject=?1 OR evidence LIKE ?2 ORDER BY id DESC LIMIT 50")
                .map_err(|e| e.to_string())?;
            let like = format!("%\"{ips}\"%");
            let anomalies: Vec<Value> = st.query_map(params![ips, like], anomaly_json).map_err(|e| e.to_string())?.filter_map(|r| r.ok()).collect();
            let total: i64 = nodes.iter().filter_map(|n| n["count"].as_i64()).sum();
            let first = nodes.iter().filter_map(|n| n["first_seen"].as_f64()).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.min(v))));
            let last = nodes.iter().filter_map(|n| n["last_seen"].as_f64()).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))));
            Ok(json!({
                "intel": &*x,
                "seen": {"as_source": {"total": total, "first_seen": first, "last_seen": last, "nodes": nodes, "clients": clients},
                         "as_egress": egress_of},
                "anomalies": anomalies,
                "attribution": "IP Geolocation by DB-IP（db-ip.com，CC BY 4.0）；ip2region（Apache-2.0）；iptoasn.com（PDDL）",
            }))
        })
    }

    /// GET /nodes/{id}/sources?window=86400&limit=50
    pub fn node_sources(&self, id: &str, window_s: f64, limit: usize, now: f64) -> Result<Option<Value>, String> {
        let Some((node, meta)) = self.resolve_node(id) else { return Ok(None) };
        let since_hour = ((now - window_s) / 3600.0).floor() as i64;
        // 窗口内每个来源的计数（小时桶）
        let rows: Vec<(String, i64, i64)> = self.with_conn_ref(|c| -> Result<_, String> {
            let mut st = c
                .prepare("SELECT ip, SUM(count), MAX(hour) FROM node_source_hours WHERE node=?1 AND hour>=?2 GROUP BY ip ORDER BY SUM(count) DESC")
                .map_err(|e| e.to_string())?;
            let v = st
                .query_map(params![node, since_hour], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            Ok(v)
        })?;
        let total: i64 = rows.iter().map(|r| r.1).sum();
        let mut dist: BTreeMap<&'static str, BTreeMap<String, (i64, i64, String)>> = BTreeMap::new();
        let mut top = Vec::new();
        for (i, (ip, cnt, _)) in rows.iter().enumerate() {
            let Ok(addr) = ip.parse::<IpAddr>() else { continue };
            let x = self.intel(addr, now);
            for (dim, val) in lookup::dims(&x) {
                let label = match dim {
                    "city" => crate::service::place_text(&x),
                    "province" => x.region.clone().unwrap_or_default(),
                    "type" => x.type_word.clone(),
                    "asn" => format!("AS{} {}", val, x.as_org.clone().unwrap_or_default()),
                    "country" => x.country_name.clone().unwrap_or(val.clone()),
                    _ => val.clone(),
                };
                let e = dist.entry(dim).or_default().entry(val).or_insert((0, 0, label));
                e.0 += cnt;
                e.1 += 1;
            }
            if i < limit {
                let pair: Option<(f64, f64, i64)> = self.with_conn_ref(|c| {
                    c.query_row("SELECT first_seen,last_seen,count FROM node_sources WHERE node=?1 AND ip=?2", params![node, ip], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .optional()
                    .ok()
                    .flatten()
                });
                top.push(json!({"ip": ip, "count": cnt, "pct": pct(*cnt, total),
                    "first_seen": pair.map(|p| p.0), "last_seen": pair.map(|p| p.1), "count_all": pair.map(|p| p.2),
                    "intel": brief(&x), "type_reason": x.type_reason}));
            }
        }
        let dist_json: serde_json::Map<String, Value> = dist
            .into_iter()
            .map(|(dim, m)| {
                let mut v: Vec<(String, (i64, i64, String))> = m.into_iter().collect();
                v.sort_by_key(|x| std::cmp::Reverse(x.1 .0));
                (
                    dim.to_string(),
                    Value::Array(v.into_iter().map(|(k, (c, n, label))| json!({"key": k, "label": label, "count": c, "sources": n, "pct": pct(c, total)})).collect()),
                )
            })
            .collect();
        // 首次出现的维度值（学习到的基线），给页面画“这个节点见过哪些运营商 / 城市”
        let known: Value = self.with_conn_ref(|c| -> Result<Value, String> {
            let mut st = c
                .prepare("SELECT dim, value, first_seen, last_seen, count FROM node_dims WHERE node=?1 AND dim IN ('isp','city') ORDER BY dim, first_seen")
                .map_err(|e| e.to_string())?;
            let mut m: BTreeMap<String, Vec<Value>> = BTreeMap::new();
            for r in st
                .query_map([&node], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?, r.get::<_, f64>(3)?, r.get::<_, i64>(4)?)))
                .map_err(|e| e.to_string())?
                .flatten()
            {
                m.entry(r.0).or_default().push(json!({"value": r.1, "first_seen": r.2, "last_seen": r.3, "count": r.4}));
            }
            Ok(json!(m))
        })?;
        let egress: Value = self.with_conn_ref(|c| -> Result<Value, String> {
            let mut st = c
                .prepare("SELECT family, ip, first_seen, last_seen, count, via FROM egress_history WHERE node=?1 ORDER BY family, first_seen")
                .map_err(|e| e.to_string())?;
            let hist: Vec<(i64, String, f64, f64, i64, Option<String>)> = st
                .query_map([&node], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
                .map_err(|e| e.to_string())?
                .flatten()
                .collect();
            let cur: Vec<(i64, String, f64)> = c
                .prepare("SELECT family, current_ip, current_since FROM egress_state WHERE node=?1")
                .and_then(|mut s| s.query_map([&node], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect())
                .map_err(|e| e.to_string())?;
            Ok(json!({"current": cur.iter().map(|(f, ip, s)| json!({"family": f, "ip": ip, "since": s})).collect::<Vec<_>>(),
                      "history": hist.iter().map(|h| json!({"family": h.0, "ip": h.1, "first_seen": h.2, "last_seen": h.3, "count": h.4, "via": h.5})).collect::<Vec<_>>()}))
        })?;
        // 补上出口 IP 的情报
        let egress = {
            let mut e = egress;
            if let Some(Value::Array(h)) = e.get_mut("history") {
                for row in h.iter_mut() {
                    if let Some(ip) = row["ip"].as_str().and_then(|s| s.parse::<IpAddr>().ok()) {
                        row["intel"] = brief(&self.intel(ip, now));
                    }
                }
            }
            e
        };
        let anomalies: Vec<Value> = self.with_conn_ref(|c| {
            c.prepare("SELECT * FROM anomalies WHERE node=?1 ORDER BY (status='open') DESC, last_at DESC LIMIT 50")
                .and_then(|mut s| s.query_map([&node], anomaly_json)?.collect())
                .unwrap_or_default()
        });
        Ok(Some(json!({
            "node": meta,
            "window_s": window_s,
            "total": total,
            "distinct_sources": rows.len(),
            "distribution": dist_json,
            "top_sources": top,
            "known": known,
            "egress": egress,
            "anomalies": anomalies,
        })))
    }

    /// GET /ipintel/anomalies
    pub fn anomalies(&self, f: &AnomalyFilter) -> Result<Value, String> {
        self.with_conn_ref(|c| -> Result<Value, String> {
            let mut sql = String::from("SELECT * FROM anomalies WHERE 1=1");
            let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            match f.status.as_deref().unwrap_or("open") {
                "all" => {}
                s => {
                    sql.push_str(" AND status=?");
                    args.push(Box::new(s.to_string()));
                }
            }
            if let Some(k) = &f.kind {
                sql.push_str(" AND kind=?");
                args.push(Box::new(k.trim_start_matches("ipintel_").to_string()));
            }
            if let Some(s) = &f.severity {
                sql.push_str(" AND severity=?");
                args.push(Box::new(s.clone()));
            }
            if let Some(n) = &f.node {
                let key = resolve_node(c, n).map(|x| x.0).unwrap_or_else(|| n.clone());
                sql.push_str(" AND node=?");
                args.push(Box::new(key));
            }
            if let Some(t) = f.since {
                sql.push_str(" AND last_at>=?");
                args.push(Box::new(t));
            }
            sql.push_str(" ORDER BY (status='open') DESC, CASE severity WHEN 'critical' THEN 0 WHEN 'warning' THEN 1 ELSE 2 END, last_at DESC LIMIT ?");
            args.push(Box::new(f.limit.unwrap_or(200).min(1000) as i64));
            let mut st = c.prepare(&sql).map_err(|e| e.to_string())?;
            let list: Vec<Value> = st
                .query_map(rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())), anomaly_json)
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            let mut counts = serde_json::Map::new();
            let mut st = c.prepare("SELECT kind, severity, COUNT(*) FROM anomalies WHERE status='open' GROUP BY kind, severity").map_err(|e| e.to_string())?;
            let mut by_sev: BTreeMap<String, i64> = BTreeMap::new();
            let mut by_kind: BTreeMap<String, i64> = BTreeMap::new();
            for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))).map_err(|e| e.to_string())?.flatten() {
                *by_kind.entry(r.0).or_default() += r.2;
                *by_sev.entry(r.1).or_default() += r.2;
            }
            counts.insert("open".into(), json!(by_kind.values().sum::<i64>()));
            counts.insert("by_kind".into(), json!(by_kind));
            counts.insert("by_severity".into(), json!(by_sev));
            Ok(json!({"counts": counts, "anomalies": list}))
        })
    }

    /// 状态（给 /system 或页面脚注用）：库版本、文件大小、缓存与表行数。
    pub fn status(&self) -> Value {
        let d = self.dbs();
        let counts = self.with_conn_ref(|c| {
            let n = |t: &str| c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).unwrap_or(-1);
            json!({"ip_intel": n("ip_intel"), "nodes": n("nodes"), "node_sources": n("node_sources"), "node_source_hours": n("node_source_hours"),
                   "client_sources": n("client_sources"), "egress_history": n("egress_history"), "anomalies": n("anomalies")})
        });
        json!({"enabled": self.cfg.enabled, "db_dir": self.cfg.db_dir, "loaded": d.loaded(), "files": d.files, "errors": d.errors,
               "version": d.version, "hosting_asns_from_iptoasn": d.hosting_scan.len(), "loaded_at": *self.loaded_at.read().unwrap(),
               "rows": counts, "rss": crate::mem::rss()})
    }
}

fn pct(c: i64, total: i64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (c as f64 * 10000.0 / total as f64).round() / 100.0
    }
}

fn resolve_node(c: &Connection, id: &str) -> Option<(String, Value)> {
    c.query_row(
        "SELECT node, short, name, relay, team, egress_ip, first_event FROM nodes WHERE node=?1 OR short=?1 OR name=?1 OR name=?2 ORDER BY (node=?1) DESC LIMIT 1",
        // 「团队1-01 · ATT · 75.37.237.38」这样的资产全名：取第一段
        [id, id.split(" · ").next().unwrap_or(id).trim()],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                json!({"uuid": r.get::<_, String>(0)?, "id": r.get::<_, Option<String>>(1)?, "name": r.get::<_, Option<String>>(2)?,
                       "relay": r.get::<_, Option<String>>(3)?, "team": r.get::<_, Option<String>>(4)?, "egress_ip": r.get::<_, Option<String>>(5)?,
                       "first_event": r.get::<_, Option<f64>>(6)?}),
            ))
        },
    )
    .optional()
    .ok()
    .flatten()
}
