//! 探测历史：旧接口只给每个节点“最新一次”的 TCP / 认证出口 / 实际入口到出口结果，fleetd 每次轮询把它们
//! 记下来（按 measured_at 去重），保留 24 h：原始样本 + 5 分钟桶（avg/min/max/丢失率/样本数）。
//! 失败、超时、出口不一致一律记状态，**绝不**把它们当成延迟值（ms 为 NULL）。
use crate::rules::{head_name, short_id};
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::{json, Value};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS probe_raw(node TEXT NOT NULL, kind TEXT NOT NULL, ts REAL NOT NULL, status TEXT NOT NULL, ms REAL,
  detail TEXT, PRIMARY KEY(node, kind, ts)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS probe_raw_ts ON probe_raw(ts);
CREATE TABLE IF NOT EXISTS probe_5m(node TEXT NOT NULL, kind TEXT NOT NULL, bucket INTEGER NOT NULL, n INTEGER NOT NULL,
  ok INTEGER NOT NULL, bad INTEGER NOT NULL, retry INTEGER NOT NULL, sum_ms REAL NOT NULL, min_ms REAL, max_ms REAL,
  PRIMARY KEY(node, kind, bucket)) WITHOUT ROWID;
"#;

pub const KINDS: &[&str] = &["tcp", "egress", "path"];
pub const RECENT: usize = 20;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    pub node: String,
    pub kind: &'static str,
    pub ts: f64,
    /// ok | fail | timeout | mismatch | retry（认证出口一个周期内未到最后一次的失败尝试）
    pub status: &'static str,
    pub ms: Option<f64>,
    pub detail: Value,
}

fn is_timeout(rc: Option<i64>, err: Option<&str>) -> bool {
    rc == Some(28) || err.map(|e| e.to_ascii_lowercase().contains("timed out") || e.to_ascii_lowercase().contains("timeout")).unwrap_or(false)
}

fn num(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

fn text<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty())
}

/// 从一份 /overview 里取出每个节点当前的三类探测结果。
pub fn extract(ov: &Value) -> Vec<Sample> {
    let mut out = vec![];
    let Some(assets) = ov.get("assets").and_then(|x| x.as_array()) else { return out };
    for a in assets {
        let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
        if a.get("team").and_then(|x| x.as_str()) == Some("线路机") {
            continue;
        }
        let node = short_id(name);
        let expected = a.get("ip").and_then(|x| x.as_str()).unwrap_or("");
        let p = a.get("probe").cloned().unwrap_or(Value::Null);
        if p.is_object() {
            if let Some(ts) = num(&p, "tcp_at") {
                let rc = p.get("tcp_rc").and_then(|x| x.as_i64());
                let err = text(&p, "tcp_error");
                let ms = num(&p, "tcp_ms");
                let ok = err.is_none() && ms.map(|m| m >= 0.0).unwrap_or(false) && rc.map(|r| r == 0).unwrap_or(true);
                let status = if ok { "ok" } else if is_timeout(rc, err) { "timeout" } else { "fail" };
                out.push(Sample {
                    node: node.clone(),
                    kind: "tcp",
                    ts,
                    status,
                    ms: if ok { ms } else { None },
                    detail: json!({"rc": rc, "phase": p.get("tcp_phase"), "error": err}),
                });
            }
            if let Some(ts) = num(&p, "egress_at") {
                let rc = p.get("egress_rc").and_then(|x| x.as_i64());
                let err = text(&p, "egress_error");
                let actual = text(&p, "actual_ip");
                let attempt = p.get("egress_attempt").and_then(|x| x.as_i64());
                let ms = num(&p, "egress_ms");
                // 只看连通性：取到出口 IP 就算通过，实际 IP 记在 detail 里、不与资产 IP 比对（线路机 IP 泄漏另有告警）
                let mismatch = false;
                let ok = err.is_none() && actual.is_some() && ms.map(|m| m >= 0.0).unwrap_or(false);
                let status = if ok {
                    "ok"
                } else if mismatch {
                    "mismatch"
                } else if attempt.map(|n| n < 3).unwrap_or(false) {
                    "retry"
                } else if is_timeout(rc, err) {
                    "timeout"
                } else {
                    "fail"
                };
                out.push(Sample {
                    node: node.clone(),
                    kind: "egress",
                    ts,
                    status,
                    ms: if ok { ms } else { None },
                    detail: json!({"rc": rc, "phase": p.get("egress_phase"), "error": err, "attempt": attempt,
                        "actual_ip": actual, "expected_ip": expected}),
                });
            }
        }
        let path = a.get("path").cloned().unwrap_or(Value::Null);
        if path.is_object() {
            let ts = num(&path, "measured_at").or_else(|| num(&path, "checked_at")).or_else(|| num(&path, "egress_at"));
            if let Some(ts) = ts {
                let rc = path.get("rc").and_then(|x| x.as_i64());
                let actual = text(&path, "actual_ip");
                let healthy = path.get("healthy").and_then(|x| x.as_bool()).unwrap_or(false);
                let ms = num(&path, "egress_ms");
                let mismatch = false;
                let ok = healthy && actual.is_some() && ms.map(|m| m >= 0.0).unwrap_or(false);
                let status = if ok { "ok" } else if mismatch { "mismatch" } else if is_timeout(rc, None) { "timeout" } else { "fail" };
                out.push(Sample {
                    node,
                    kind: "path",
                    ts,
                    status,
                    ms: if ok { ms } else { None },
                    detail: json!({"rc": rc, "phase": path.get("phase"), "actual_ip": actual, "expected_ip": expected}),
                });
            }
        }
    }
    out
}

/// 线路机自身的探测（relayd 直接上报到 ingest 的 probe_land）：对端 TCP 心跳（node = 被测线路机 uuid，
/// 由另外两台线路机测）与经主节点的出口验证（自己测）。节点键 = 线路机短名（r1 / r2 / r3），detail.from = 测量方。
pub fn relay_samples(ing: &fleetd_ingest::Shared, ov: &Value, since: f64) -> Vec<Sample> {
    let relays: Vec<(String, String, String)> = ov
        .get("assets")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter(|a| a.get("team").and_then(|x| x.as_str()) == Some("线路机"))
                .filter_map(|a| Some((a.get("uuid")?.as_str()?.to_string(), short_id(a.get("name")?.as_str()?), a.get("ip").and_then(|x| x.as_str()).unwrap_or("").to_string())))
                .collect()
        })
        .unwrap_or_default();
    if relays.is_empty() {
        return vec![];
    }
    let rows: Vec<(String, String, String, f64, String)> = ing.with_db(|c| {
        let q = format!(
            "SELECT relay, node, check_kind, measured_at, doc FROM probe_land WHERE measured_at>?1 AND check_kind IN ('tcp','egress') AND node IN ({}) ORDER BY measured_at",
            vec!["?"; relays.len()].join(",")
        );
        let mut p: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Real(since)];
        p.extend(relays.iter().map(|r| rusqlite::types::Value::Text(r.0.clone())));
        c.prepare(&q)
            .and_then(|mut st| st.query_map(rusqlite::params_from_iter(p.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).map(|it| it.flatten().collect()))
            .unwrap_or_default()
    });
    let mut out = vec![];
    for (from, uuid, kind, ts, doc) in rows {
        let Some((_, node, ip)) = relays.iter().find(|r| r.0 == uuid) else { continue };
        let d: Value = serde_json::from_str(&doc).unwrap_or(Value::Null);
        let reason = d["reason_code"].as_str();
        // 只看连通性：出口取到了（哪怕 IP 与记录不同）就算通过
        let ok = d["status"] == "ok" || (reason == Some("egress_mismatch") && d["actual_ip"].is_string());
        let actual = d["actual_ip"].as_str();
        let expected = d["expected_ip"].as_str().unwrap_or(ip);
        let (kind, status): (&'static str, &'static str) = match kind.as_str() {
            "tcp" => ("tcp", if ok { "ok" } else if reason.is_some_and(|r| r.contains("timeout")) { "timeout" } else { "fail" }),
            _ => (
                "egress",
                if ok {
                    "ok"
                } else if d["final"] == false {
                    "retry"
                } else if reason.is_some_and(|r| r.contains("timeout")) {
                    "timeout"
                } else {
                    "fail"
                },
            ),
        };
        out.push(Sample {
            node: node.clone(),
            kind,
            ts,
            status,
            ms: if ok { d["value_ms"].as_f64() } else { None },
            detail: json!({"rc": d["legacy_rc"], "error": reason, "from": from, "target": d["target"], "attempt": d["attempt"],
                "actual_ip": actual, "expected_ip": if kind == "egress" { json!(expected) } else { Value::Null }, "source": d["source"]}),
        });
    }
    out
}

/// 写入（按 node+kind+ts 去重），新样本同时累加进 5 分钟桶。返回新增条数。
pub fn record(c: &mut Connection, samples: &[Sample]) -> rusqlite::Result<usize> {
    let tx = c.transaction()?;
    let mut added = 0;
    for s in samples {
        let n = tx.execute(
            "INSERT OR IGNORE INTO probe_raw(node,kind,ts,status,ms,detail) VALUES(?,?,?,?,?,?)",
            params![s.node, s.kind, s.ts, s.status, s.ms, s.detail.to_string()],
        )?;
        if n == 0 {
            continue;
        }
        added += 1;
        let bucket = (s.ts as i64) / 300 * 300;
        let ok = (s.status == "ok") as i64;
        let retry = (s.status == "retry") as i64;
        let bad = 1 - ok - retry;
        tx.execute(
            "INSERT INTO probe_5m(node,kind,bucket,n,ok,bad,retry,sum_ms,min_ms,max_ms) VALUES(?1,?2,?3,1,?4,?5,?6,coalesce(?7,0),?7,?7)
             ON CONFLICT(node,kind,bucket) DO UPDATE SET n=n+1, ok=ok+excluded.ok, bad=bad+excluded.bad, retry=retry+excluded.retry,
               sum_ms=sum_ms+excluded.sum_ms,
               min_ms=CASE WHEN excluded.min_ms IS NULL THEN min_ms WHEN min_ms IS NULL THEN excluded.min_ms ELSE min(min_ms, excluded.min_ms) END,
               max_ms=CASE WHEN excluded.max_ms IS NULL THEN max_ms WHEN max_ms IS NULL THEN excluded.max_ms ELSE max(max_ms, excluded.max_ms) END",
            params![s.node, s.kind, bucket, ok, bad, retry, s.ms],
        )?;
    }
    tx.commit()?;
    Ok(added)
}

pub fn retention(c: &Connection, now: f64) -> rusqlite::Result<usize> {
    let cut = now - 86400.0 - 600.0;
    let a = c.execute("DELETE FROM probe_raw WHERE ts < ?", [cut])?;
    let b = c.execute("DELETE FROM probe_5m WHERE bucket < ?", [cut as i64])?;
    Ok(a + b)
}

fn sample_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let d: Option<String> = r.get("detail")?;
    let mut v = json!({"ts": r.get::<_, f64>("ts")?, "status": r.get::<_, String>("status")?, "ms": r.get::<_, Option<f64>>("ms")?});
    if let Some(Value::Object(m)) = d.and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
        for (k, x) in m {
            if !x.is_null() {
                v[k] = x;
            }
        }
    }
    Ok(v)
}

/// 增量版：`since` 给出时只返回 ts > since 的新样本（没有新样本的节点不出现），并给出 `until` 供下次使用。
pub fn latest_since(c: &Connection, ov: Option<&Value>, since: Option<f64>) -> rusqlite::Result<Value> {
    let Some(since) = since else {
        let mut v = latest(c, ov)?;
        let until: Option<f64> = c.query_row("SELECT max(ts) FROM probe_raw", [], |r| r.get(0))?;
        v["until"] = json!(until);
        return Ok(v);
    };
    let mut st = c.prepare("SELECT node,kind,ts,status,ms,detail FROM probe_raw WHERE ts > ?1 ORDER BY node, kind, ts LIMIT 20000")?;
    let mut nodes = serde_json::Map::new();
    let mut until = since;
    let mut rows = st.query([since])?;
    while let Some(r) = rows.next()? {
        let node: String = r.get("node")?;
        let kind: String = r.get("kind")?;
        until = until.max(r.get::<_, f64>("ts")?);
        let e = nodes.entry(node.clone()).or_insert_with(|| json!({"node": node, "tcp": {"new": []}, "egress": {"new": []}, "path": {"new": []}}));
        e[&kind]["new"].as_array_mut().unwrap().push(sample_json(r)?);
    }
    Ok(json!({"since": since, "until": until, "nodes": nodes, "recent_size": RECENT}))
}

/// 每个节点每类最近 20 个样本（旧→新），卡片一打开就能画满 20 格。
pub fn latest(c: &Connection, ov: Option<&Value>) -> rusqlite::Result<Value> {
    // 每个 (node, kind) 走主键取最近 RECENT 条（不再对整张 probe_raw 开窗排序）；(node, kind) 清单取自小得多的 5 分钟桶
    let pairs: Vec<(String, String)> = {
        let mut st = c.prepare_cached("SELECT DISTINCT node, kind FROM probe_5m ORDER BY node, kind")?;
        let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        it.flatten().collect()
    };
    let mut nodes = serde_json::Map::new();
    let mut st = c.prepare_cached("SELECT node,kind,ts,status,ms,detail FROM probe_raw WHERE node=?1 AND kind=?2 ORDER BY ts DESC LIMIT ?3")?;
    for (node, kind) in pairs {
        let mut rows = st.query(params![node, kind, RECENT as i64])?;
        let mut v = vec![];
        while let Some(r) = rows.next()? {
            v.push(sample_json(r)?);
        }
        if v.is_empty() {
            continue;
        }
        v.reverse();
        let e = nodes.entry(node.clone()).or_insert_with(|| json!({"node": node, "tcp": {"recent": []}, "egress": {"recent": []}, "path": {"recent": []}}));
        e[&kind]["recent"] = Value::Array(v);
    }
    // 名称、线路机、uuid 与各类的“最后一次”
    if let Some(assets) = ov.and_then(|o| o.get("assets")).and_then(|x| x.as_array()) {
        for a in assets {
            let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
            if let Some(e) = nodes.get_mut(&short_id(name)) {
                e["name"] = json!(head_name(name));
                e["uuid"] = a.get("uuid").cloned().unwrap_or(Value::Null);
                e["relay"] = a.get("relay").cloned().unwrap_or(Value::Null);
            }
        }
    }
    for e in nodes.values_mut() {
        for k in KINDS {
            let last = e[*k]["recent"].as_array().and_then(|r| r.last().cloned()).unwrap_or(Value::Null);
            let n = e[*k]["recent"].as_array().map(|r| r.len()).unwrap_or(0);
            e[*k]["last"] = last;
            e[*k]["samples"] = json!(n);
        }
    }
    Ok(json!({"nodes": nodes, "recent_size": RECENT}))
}

/// 某节点一段时间的 5 分钟桶（+ 可选原始样本）。
pub fn series(c: &Connection, node: &str, kinds: &[&str], since: f64, raw: bool) -> rusqlite::Result<Value> {
    let mut out = serde_json::Map::new();
    for k in kinds {
        let mut st = c.prepare(
            "SELECT bucket,n,ok,bad,retry,sum_ms,min_ms,max_ms FROM probe_5m WHERE node=? AND kind=? AND bucket>=? ORDER BY bucket",
        )?;
        let buckets: Vec<Value> = st
            .query_map(params![node, k, (since as i64) / 300 * 300], |r| {
                let (n, ok, bad, retry): (i64, i64, i64, i64) = (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
                let sum: f64 = r.get(5)?;
                let judged = n - retry;
                Ok(json!({"t": r.get::<_, i64>(0)?, "samples": n, "ok": ok, "failed": bad, "retry": retry,
                    "loss_pct": if judged > 0 { json!(((bad as f64) * 1000.0 / judged as f64).round() / 10.0) } else { Value::Null },
                    "avg_ms": if ok > 0 { json!((sum / ok as f64 * 10.0).round() / 10.0) } else { Value::Null },
                    "min_ms": r.get::<_, Option<f64>>(6)?, "max_ms": r.get::<_, Option<f64>>(7)?}))
            })?
            .filter_map(Result::ok)
            .collect();
        let mut entry = json!({"step": 300, "buckets": buckets});
        if raw {
            let mut st = c.prepare("SELECT ts,status,ms,detail FROM probe_raw WHERE node=? AND kind=? AND ts>=? ORDER BY ts LIMIT 5000")?;
            let v: Vec<Value> = st.query_map(params![node, k, since], sample_json)?.filter_map(Result::ok).collect();
            entry["raw"] = json!(v);
        }
        out.insert(k.to_string(), entry);
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ov(tcp_ms: f64, tcp_err: Option<&str>, eg_ip: &str, eg_err: Option<&str>, attempt: i64, t: f64) -> Value {
        json!({"assets": [
            {"name": "团队1-01 · ATT · 75.37.237.38", "team": "团队1", "ip": "75.37.237.38", "uuid": "u1", "relay": "线路机2",
             "probe": {"tcp_ms": tcp_ms, "tcp_at": t, "tcp_rc": if tcp_err.is_some() { 28 } else { 0 }, "tcp_error": tcp_err,
                       "egress_ms": 600, "egress_at": t + 1.0, "egress_rc": if eg_err.is_some() { 35 } else { 0 }, "egress_error": eg_err,
                       "actual_ip": eg_ip, "egress_attempt": attempt},
             "path": {"healthy": eg_err.is_none(), "measured_at": t + 2.0, "egress_ms": 333.7, "rc": 0, "actual_ip": eg_ip}},
            {"name": "线路机2", "team": "线路机", "probe": null, "path": null}
        ]})
    }

    #[test]
    fn failures_are_status_not_latency() {
        let s = extract(&ov(-1.0, Some("Connection timed out"), "75.37.237.38", None, 1, 100.0));
        let tcp = s.iter().find(|x| x.kind == "tcp").unwrap();
        assert_eq!((tcp.status, tcp.ms), ("timeout", None));
        let s = extract(&ov(8.0, None, "", Some("curl: (35) tls"), 3, 100.0));
        let eg = s.iter().find(|x| x.kind == "egress").unwrap();
        assert_eq!((eg.status, eg.ms), ("fail", None));
        let s = extract(&ov(8.0, None, "", Some("curl: (35) tls"), 1, 100.0));
        assert_eq!(s.iter().find(|x| x.kind == "egress").unwrap().status, "retry");
        let s = extract(&ov(8.0, None, "203.0.113.9", None, 1, 100.0));
        // 出口 IP 与记录不同：只看连通性，照样是 ok（IP 记在 detail 里）
        let eg = s.iter().find(|x| x.kind == "egress").unwrap();
        assert_eq!(eg.status, "ok");
        assert_eq!(eg.detail["actual_ip"], "203.0.113.9");
        assert_eq!(s.iter().find(|x| x.kind == "path").unwrap().status, "ok");
        let s = extract(&ov(8.0, None, "75.37.237.38", None, 1, 100.0));
        assert!(s.iter().all(|x| x.status == "ok" && x.ms.is_some()));
        assert!(s.iter().all(|x| x.node == "t1-01"), "线路机本身不记");
    }

    #[test]
    fn dedupe_buckets_and_recent_20() {
        let st = crate::store::Store::open_memory();
        st.call_blocking(|c| {
            let t0 = 1_790_000_100.0;
            for i in 0..30 {
                let t = t0 + i as f64 * 30.0;
                let s = extract(&ov(if i % 10 == 9 { -1.0 } else { 10.0 + i as f64 }, if i % 10 == 9 { Some("timeout") } else { None },
                    "75.37.237.38", None, 1, t));
                assert_eq!(record(c, &s).unwrap(), 3);
                assert_eq!(record(c, &s).unwrap(), 0, "同一 measured_at 不重复记");
            }
            let l = latest(c, None).unwrap();
            let tcp = &l["nodes"]["t1-01"]["tcp"];
            assert_eq!(tcp["recent"].as_array().unwrap().len(), 20);
            assert_eq!(tcp["samples"], 20);
            assert!(tcp["recent"].as_array().unwrap().iter().filter(|x| x["status"] == "timeout").all(|x| x["ms"].is_null()));
            let s = series(c, "t1-01", &["tcp"], t0 - 1.0, true).unwrap();
            let b = s["tcp"]["buckets"].as_array().unwrap();
            let total: i64 = b.iter().map(|x| x["samples"].as_i64().unwrap()).sum();
            assert_eq!(total, 30);
            let failed: i64 = b.iter().map(|x| x["failed"].as_i64().unwrap()).sum();
            assert_eq!(failed, 3);
            assert!(b.iter().all(|x| x["min_ms"].as_f64().map(|m| m >= 10.0).unwrap_or(true)), "失败不进 min/max");
            assert_eq!(s["tcp"]["raw"].as_array().unwrap().len(), 30);
            assert_eq!(retention(c, t0 + 86400.0 * 2.0).unwrap() > 0, true);
        });
    }
}
