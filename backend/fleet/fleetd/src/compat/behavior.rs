//! 旧 `teamops.behavior` 的读路径：/behavior 汇总（默认查询走 behavior_cache）与 /activity 默认查询的聚合缓存。
use super::py::*;
use super::Ctx;
use serde_json::{json, Map, Value};

/// Behavior.__init__ 从 behavior_cache 载入：value 反序列化后用行上的 computed_at 覆盖
pub fn cached(cx: &Ctx, key: &str) -> R<Option<Value>> {
    let has = scalar(cx.db, "SELECT count(*) FROM sqlite_master WHERE name='behavior_cache'", &[])?;
    if int_of(&has).unwrap_or(0) == 0 {
        return Ok(None);
    }
    let rs = rows(cx.db, "SELECT key,computed_at,value FROM behavior_cache WHERE key=?", &[st(key)])?;
    match rs.into_iter().next() {
        Some(r) if cx.cfg.cache_max_age.is_some_and(|age| cx.now - num(&r["computed_at"]).unwrap_or(0.0) > age) => Ok(None),
        Some(r) => {
            let mut v = loads(&r["value"])?;
            if let Some(m) = v.as_object_mut() {
                m.insert("computed_at".into(), r["computed_at"].clone());
            }
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

/// Behavior._refresh 里 activity 那一份（缓存缺失时现算）
pub fn activity_aggregates(cx: &Ctx) -> R<Value> {
    let args = [sf(cx.now - 86400.0)];
    let domains = rows(cx.db, "SELECT host,kind,count(*) connections,count(DISTINCT source_ip) sources,max(ts) last_seen,sum(action='blocked') blocked FROM activity INDEXED BY activity_time WHERE ts>=? GROUP BY host,kind ORDER BY connections DESC LIMIT 40", &args)?;
    let sources = rows(cx.db, "SELECT source_ip,count(*) connections,count(DISTINCT host) domains,max(ts) last_seen FROM activity WHERE ts>=? GROUP BY source_ip ORDER BY connections DESC LIMIT 30", &args)?;
    let total = scalar(cx.db, "SELECT count(*) FROM activity WHERE ts>=?", &args)?;
    Ok(json!({"domains": domains, "sources": sources, "total": total, "computed_at": f(cx.now)}))
}

const KEYS: [&str; 10] = ["asset", "team", "source_ip", "host", "action", "kind", "protocol", "search", "offset", "limit"];

/// Behavior.summary(params)
/// 默认参数的 summary 用增量窗口算（与 summary_live({}) 同结果）
pub fn summary_window(cx: &Ctx) -> Option<R<Value>> {
    let memo = cx.cfg.memo.as_ref()?;
    let parts = memo.with_window(cx.db, cx.now, |w| w.summary_parts(cx.db, 100))?;
    Some((|| {
        let (nodes, targets, sources) = parts?;
        let ev = events(cx, &Params::new())?;
        let n = targets.len();
        let mut out = Map::new();
        out.insert("now".into(), f(cx.now));
        out.insert("nodes".into(), Value::Array(nodes));
        out.insert("targets".into(), Value::Array(targets));
        out.insert("sources".into(), Value::Array(sources));
        out.insert("events".into(), Value::Array(ev));
        out.insert("next_offset".into(), if n == 100 { i(100) } else { Value::Null });
        out.insert("offset".into(), i(0));
        out.insert("started_at".into(), meta(cx.db, "behavior_started_at", Value::Null)?);
        out.insert("raw_retention_days".into(), i(14));
        out.insert("target_retention_days".into(), i(90));
        Ok(Value::Object(out))
    })())
}

/// 默认 /activity 的聚合用增量窗口算（与 activity_aggregates 同结果）
pub fn activity_window(cx: &Ctx) -> Option<R<Value>> {
    let memo = cx.cfg.memo.as_ref()?;
    let r = memo.with_window(cx.db, cx.now, |w| Ok(w.activity_parts()))?;
    Some(r.map(|(domains, sources, total)| json!({"domains": domains, "sources": sources, "total": total, "computed_at": f(cx.now)})))
}

pub fn summary(cx: &Ctx, p: &Params) -> R<Value> {
    if !KEYS.iter().any(|k| p.contains_key(*k)) && p_int(p, "seconds", 86400)? == 86400 {
        if let Some(r) = summary_window(cx) {
            let mut v = r?;
            v.as_object_mut().unwrap().insert("computed_at".into(), f(cx.now));
            return Ok(v);
        }
        if let Some(v) = cached(cx, "summary")? {
            return Ok(v);
        }
    }
    let mut v = match summary_rollup(cx, p)? {
        Some(v) => v,
        None => summary_live(cx, p)?,
    };
    v.as_object_mut().unwrap().insert("computed_at".into(), f(cx.now));
    Ok(v)
}

/// 长窗口（> 24 h）、只有等值过滤（asset/team/host/kind/source_ip）时走小时汇总（compat::rollup）
fn summary_rollup(cx: &Ctx, p: &Params) -> R<Option<Value>> {
    let Some(memo) = cx.cfg.memo.as_ref() else { return Ok(None) };
    let seconds = (14 * 86400).min(300.max(p_int(p, "seconds", 86400)?));
    if seconds <= 86400 || ["action", "protocol", "search"].iter().any(|k| p_truthy(p, k).is_some()) {
        return Ok(None);
    }
    let flt = super::window::ActFilter {
        asset: p_truthy(p, "asset").map(String::from),
        team: p_truthy(p, "team").map(String::from),
        kind: p_truthy(p, "kind").map(String::from),
        source_ip: p_truthy(p, "source_ip").map(String::from),
        host: p_truthy(p, "host").map(String::from),
        blocked: None,
    };
    let limit = 200.min(10.max(p_int(p, "limit", 100)?));
    let offset = 100000.min(0.max(p_int(p, "offset", 0)?));
    let Some(parts) = memo.with_rollup(cx.db, cx.now, |r| r.summary_parts(cx.db, cx.now, seconds, &flt, limit as usize, offset as usize)) else { return Ok(None) };
    let (nodes, targets, sources) = parts?;
    let ev = events(cx, p)?;
    let n = targets.len() as i64;
    let mut out = Map::new();
    out.insert("now".into(), f(cx.now));
    out.insert("nodes".into(), Value::Array(nodes));
    out.insert("targets".into(), Value::Array(targets));
    out.insert("sources".into(), Value::Array(sources));
    out.insert("events".into(), Value::Array(ev));
    out.insert("next_offset".into(), if n == limit { i(offset + limit) } else { Value::Null });
    out.insert("offset".into(), i(offset));
    out.insert("started_at".into(), meta(cx.db, "behavior_started_at", Value::Null)?);
    out.insert("raw_retention_days".into(), i(14));
    out.insert("target_retention_days".into(), i(90));
    Ok(Some(Value::Object(out)))
}

fn events(cx: &Ctx, p: &Params) -> R<Vec<Value>> {
    let secs = (14 * 86400).min(300.max(p_int(p, "seconds", 86400)?));
    let mut clauses = vec!["ts>=?".to_string()];
    let mut args = vec![sf(cx.now - secs as f64)];
    if let Some(a) = p_truthy(p, "asset") {
        clauses.push("asset=?".into());
        args.push(st(a));
    }
    if let Some(t) = p_truthy(p, "team") {
        let uuids: Vec<String> = super::store::raw_assets(cx)?
            .iter()
            .filter(|a| get(a, "team").as_str() == Some(t))
            .filter_map(|a| a["uuid"].as_str().map(String::from))
            .collect();
        if uuids.is_empty() {
            clauses.push("0".into());
        } else {
            clauses.push(format!("asset IN ({})", vec!["?"; uuids.len()].join(",")));
            args.extend(uuids.iter().map(|u| st(u)));
        }
    }
    let w = clauses.join(" AND ");
    let (sql, args) = if cx.cfg.events_split {
        let mut a2 = args.clone();
        a2.extend(args.iter().cloned());
        (format!("SELECT * FROM (SELECT * FROM behavior_events WHERE {w} AND kind<>'new_domain' ORDER BY ts DESC,id DESC LIMIT 200)
                  UNION ALL SELECT * FROM (SELECT * FROM behavior_events WHERE {w} AND kind='new_domain' ORDER BY ts DESC,id DESC LIMIT 200)
                  ORDER BY ts DESC,id DESC"), a2)
    } else {
        (format!("SELECT * FROM behavior_events WHERE {w} ORDER BY ts DESC,id DESC LIMIT 200"), args)
    };
    Ok(rows(cx.db, &sql, &args)?
        .into_iter()
        .map(Value::Object)
        .collect())
}

pub fn summary_live(cx: &Ctx, p: &Params) -> R<Value> {
    let now = cx.now;
    let seconds = (14 * 86400).min(300.max(p_int(p, "seconds", 86400)?));
    let mut filters = vec!["ts>=?".to_string()];
    let mut args = vec![sf(now - seconds as f64)];
    for field in ["asset", "team", "source_ip", "host", "action", "kind", "protocol"] {
        if let Some(v) = p_truthy(p, field) {
            filters.push(format!("{field}=?"));
            args.push(st(v));
        }
    }
    if let Some(s) = p_truthy(p, "search") {
        filters.push("(host LIKE ? OR source_ip LIKE ? OR client LIKE ?)".into());
        let pat = format!("%{}%", head(s, 200));
        for _ in 0..3 {
            args.push(st(&pat));
        }
    }
    let wh = filters.join(" AND ");
    let index = [("asset", "activity_asset_time"), ("team", "activity_team_time"), ("host", "activity_host_time")]
        .iter()
        .find(|(k, _)| p_truthy(p, k).is_some())
        .map(|(_, i)| *i)
        .unwrap_or("activity_time");
    let source = format!("activity INDEXED BY {index}");
    let limit = 200.min(10.max(p_int(p, "limit", 100)?));
    let offset = 100000.min(0.max(p_int(p, "offset", 0)?));
    let nodes = rows(cx.db, &format!("SELECT asset,team,count(*) connections,count(DISTINCT CASE WHEN kind='domain' THEN host END) domains,count(DISTINCT source_ip) sources,sum(action='blocked') blocked,sum(kind='ip') ip_only,max(ts) last_seen FROM {source} WHERE {wh} GROUP BY asset ORDER BY last_seen DESC"), &args)?;
    let mut targs = args.clone();
    targs.push(si(limit));
    targs.push(si(offset));
    let targets = rows(cx.db, &format!("WITH grouped AS (SELECT asset,host,kind,count(*) connections,count(DISTINCT source_ip) sources,sum(action='blocked') blocked,min(ts) first_in_window,max(ts) last_seen FROM {source} WHERE {wh} GROUP BY asset,host ORDER BY last_seen DESC,asset,host LIMIT ? OFFSET ?) SELECT g.*,COALESCE(d.first_seen,g.first_in_window) first_seen,COALESCE(d.connections,g.connections) total_recorded FROM grouped g LEFT JOIN destinations d ON d.asset=g.asset AND d.host=g.host ORDER BY g.last_seen DESC,g.asset,g.host"), &targs)?;
    let sources = rows(cx.db, &format!("SELECT asset,source_ip,count(*) connections,count(DISTINCT host) targets,max(ts) last_seen FROM {source} WHERE {wh} GROUP BY asset,source_ip ORDER BY last_seen DESC LIMIT 100"), &args)?;
    let ev = events(cx, p)?;
    let n = targets.len() as i64;
    let mut out = Map::new();
    out.insert("now".into(), f(now));
    out.insert("nodes".into(), json!(nodes));
    out.insert("targets".into(), json!(targets));
    out.insert("sources".into(), json!(sources));
    out.insert("events".into(), Value::Array(ev));
    out.insert("next_offset".into(), if n == limit { i(offset + limit) } else { Value::Null });
    out.insert("offset".into(), i(offset));
    out.insert("started_at".into(), meta(cx.db, "behavior_started_at", Value::Null)?);
    out.insert("raw_retention_days".into(), i(14));
    out.insert("target_retention_days".into(), i(90));
    Ok(Value::Object(out))
}
