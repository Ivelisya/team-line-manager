//! 旧 `teamops.monitor` 的读路径：/incidents、/node（节点详情与可用率窗口）。
use super::py::*;
use super::Ctx;
use serde_json::{json, Map, Value};

pub fn incidents(cx: &Ctx, asset: Option<&str>, open_only: bool) -> R<Value> {
    let mut wh = vec![];
    let mut args = vec![];
    if let Some(a) = asset.filter(|s| !s.is_empty()) {
        wh.push("asset=?");
        args.push(st(a));
    }
    if open_only {
        wh.push("resolved_at IS NULL");
    }
    let sql = format!("SELECT * FROM incidents{} ORDER BY opened_at DESC,id DESC LIMIT 250", if wh.is_empty() { String::new() } else { format!(" WHERE {}", wh.join(" AND ")) });
    let mut out = vec![];
    for mut r in rows(cx.db, &sql, &args)? {
        let ev = loads(&r["evidence"])?;
        r.insert("evidence".into(), ev);
        out.push(Value::Object(r));
    }
    Ok(Value::Array(out))
}

pub(crate) fn availability_windows(cx: &Ctx, asset: &str, periods: &[i64]) -> R<Value> {
    let now = cx.now;
    let global = meta(cx.db, "monitor_started_at", f(now))?;
    let start = meta(cx.db, &format!("monitor_started:{asset}"), global)?;
    let statistic = meta(cx.db, "availability_statistic_v2", json!({}))?;
    let mut out = Map::new();
    for days in periods {
        let since = max2(start.clone(), f(now - (days * 86400) as f64));
        let b = floor_div_int(&since, 3600) * 3600;
        let r = rows(cx.db, "SELECT sum(ok),sum(unknown),sum(failed),sum(maintenance) FROM health_hours WHERE asset=? AND bucket>=?", &[st(asset), si(b)])?;
        let vals: Vec<i64> = r[0].values().map(|v| num(v).map(|x| x as i64).unwrap_or(0)).collect();
        let (ok, unknown, failed, maint) = (vals[0], vals[1], vals[2], vals[3]);
        let observed = ok + unknown + failed + maint;
        let span = now - num(&since).unwrap_or(now);
        let expected = observed.max((span / 60.0).ceil() as i64);
        let eligible = expected - maint;
        let avail = if ok + failed != 0 { f(super::pyjson::py_round((ok * 100) as f64 / (ok + failed) as f64, 3)) } else { Value::Null };
        let cov = if eligible != 0 { f(super::pyjson::py_round(((ok + failed) * 100) as f64 / eligible as f64, 2)) } else { Value::Null };
        out.insert(days.to_string(), json!({"availability_percent": avail, "coverage_percent": cov, "successful_samples": ok, "failed_samples": failed,
            "unknown_samples": unknown, "missing_samples": 0.max(expected - observed), "maintenance_samples": maint, "since": since,
            "statistic": statistic}));
    }
    Ok(Value::Object(out))
}

/// Monitor.detail(asset)
pub fn detail(cx: &Ctx, asset: Option<&str>) -> R<Value> {
    let ov = super::store::overview(cx)?;
    let a = asset.and_then(|u| ov["assets"].as_array().and_then(|v| v.iter().find(|a| get(a, "uuid").as_str() == Some(u))).cloned());
    let (Some(a), Some(asset)) = (a, asset) else { return Err(Err::Bad("节点不存在".into())) };
    let mut history = vec![];
    for mut r in rows(cx.db, "SELECT * FROM health_samples WHERE asset=? AND bucket>=? ORDER BY bucket", &[st(asset), sf(cx.now - 86400.0)])? {
        let d = loads(&r["data"])?;
        r.insert("data".into(), d);
        history.push(Value::Object(r));
    }
    let windows = availability_windows(cx, asset, &[1, 7, 30])?;
    let monitor = meta(cx.db, "monitor", json!({}))?;
    let health = get(get(&monitor, "nodes"), asset).clone();
    let inc = incidents(cx, Some(asset), false)?;
    let mut audit = vec![];
    for mut r in rows(cx.db, "SELECT * FROM audit WHERE target=? ORDER BY id DESC LIMIT 60", &[st(asset)])? {
        let d = loads(&r["detail"])?;
        r.insert("detail".into(), d);
        audit.push(Value::Object(r));
    }
    let traffic = super::store::history(cx, Some(asset), 24)?;
    let mut p = Params::new();
    p.insert("asset".into(), asset.to_string());
    p.insert("limit".into(), "15".into());
    let activity = super::store::activity(cx, &p)?;
    Ok(json!({"asset": a, "health": health, "history": history, "windows": windows, "incidents": inc, "audit": audit,
        "traffic": traffic, "activity": activity}))
}
