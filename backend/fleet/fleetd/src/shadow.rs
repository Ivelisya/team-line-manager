//! 下线前的影子比对（API.md §12，GET /v2/api/v1/shadow/legacy）。
//!
//! 第一部分：流量账本。旧系统按入口（x-ui 入站计数器，采集时刻落分钟桶）记 `minutes`；
//! relayd 按 Xray 入站计数器记 `xray_1m`（每分钟增量）。两者对同一个入口、同一段时间应当相等——
//! 这就是“切换时既不断档也不重复”的依据：切换点之前用旧账本，之后用 relayd 的分钟增量。
//! 比较最近一小时（只比已经结束的 5 分钟桶；采样落桶时刻可能差几秒，所以另给整小时合计）。
use crate::app::App;
use crate::rules::short_id;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const BUCKET: i64 = 300;

/// relayd 的入站分钟增量按 5 分钟桶求和：tag → bucket → (up, down)
fn relayd_buckets(app: &App, host: &str, since: i64, until: i64) -> BTreeMap<String, BTreeMap<i64, (i64, i64)>> {
    let h = host.to_string();
    app.ingest.with_db(move |c| {
        let mut out: BTreeMap<String, BTreeMap<i64, (i64, i64)>> = BTreeMap::new();
        if let Ok(mut st) = c.prepare("SELECT tag, (minute/300)*300 b, SUM(up), SUM(down) FROM xray_1m WHERE host_id=?1 AND kind='inbound' AND minute>=?2 AND minute<?3 GROUP BY tag, b") {
            if let Ok(rows) = st.query_map(rusqlite::params![h, since, until], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))) {
                for (tag, b, u, d) in rows.flatten() {
                    out.entry(tag).or_default().insert(b, (u, d));
                }
            }
        }
        out
    })
}

pub async fn ledger_compare(app: &App) -> Value {
    let now = app.clock.now() as i64;
    let until = now / BUCKET * BUCKET; // 只比已结束的桶
    let since = until - 3600;
    let Some(ov) = app.overview_data().await else {
        return json!({"error": "旧系统 /overview 不可用"});
    };
    let assets = ov["assets"].as_array().cloned().unwrap_or_default();
    let mut rows = vec![];
    let (mut lu_all, mut ld_all, mut ru_all, mut rd_all) = (0i64, 0i64, 0i64, 0i64);
    for (host, relay) in &app.cfg.edge.relay_hosts {
        // relayd 首次查询只记基线：比较从该主机第一条 xray_1m 之后两分钟起，按 5 分钟桶取整
        let h2 = host.clone();
        let first: Option<i64> = app.ingest.with_db(move |c| c.query_row("SELECT MIN(minute) FROM xray_1m WHERE host_id=?1", [h2], |r| r.get(0)).ok().flatten());
        let since = match first { Some(f) => since.max((f + 120 + BUCKET - 1) / BUCKET * BUCKET), None => since };
        if since >= until {
            rows.push(json!({"relay": relay, "host_id": host, "note": "relayd 数据不足一个完整比较窗口"}));
            continue;
        }
        let rb = relayd_buckets(app, host, since, until);
        if rb.is_empty() {
            rows.push(json!({"relay": relay, "host_id": host, "note": "relayd 这一小时没有入站计数（未上报或空闲）"}));
            continue;
        }
        // 入口端口 → 资产：节点按 relay_port；线路机主入口（没有 outbound_tag 的入站）归线路机资产
        let relay_asset = assets.iter().find(|a| a["team"] == "线路机" && a["name"] == relay.as_str()).cloned();
        let masters: Vec<i64> = ov["relays"][relay.as_str()]["inbounds"].as_array().map(|v| v.iter()
            .filter(|i| i.get("outbound_tag").map(|t| t.is_null() || t == "").unwrap_or(true)).filter_map(|i| i["port"].as_i64()).collect()).unwrap_or_default();
        let mut by_asset: BTreeMap<String, (Value, Vec<String>)> = BTreeMap::new();
        for a in assets.iter().filter(|a| a["relay"] == relay.as_str()) {
            if let (Some(u), Some(p)) = (a["uuid"].as_str(), a["relay_port"].as_i64()) {
                by_asset.entry(u.to_string()).or_insert_with(|| (a.clone(), vec![])).1.push(format!("inbound-{p}"));
            }
        }
        if let Some(ra) = &relay_asset {
            let u = ra["uuid"].as_str().unwrap_or("").to_string();
            for p in &masters {
                by_asset.entry(u.clone()).or_insert_with(|| (ra.clone(), vec![])).1.push(format!("inbound-{p}"));
            }
        }
        for (uuid, (a, tags)) in by_asset {
            let key = crate::legacy::normalize("/history", Some(&format!("asset={uuid}&hours=2"))).unwrap();
            let (r, _) = app.legacy.get(&key, 60.0).await;
            let legacy: Vec<Value> = r.ok().filter(|e| e.status == 200)
                .and_then(|e| serde_json::from_slice::<Value>(&e.body).ok())
                .and_then(|v| v["data"].as_array().cloned()).unwrap_or_default();
            let mut lb: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
            for x in legacy {
                let t = x["t"].as_i64().unwrap_or(0);
                if t >= since && t < until {
                    lb.insert(t, (x["up"].as_i64().unwrap_or(0), x["down"].as_i64().unwrap_or(0)));
                }
            }
            let mut rsum: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
            for t in &tags {
                if let Some(m) = rb.get(t) {
                    for (b, (u, d)) in m {
                        let e = rsum.entry(*b).or_default();
                        e.0 += u;
                        e.1 += d;
                    }
                }
            }
            let (lu, ld): (i64, i64) = lb.values().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
            let (ru, rd): (i64, i64) = rsum.values().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
            lu_all += lu;
            ld_all += ld;
            ru_all += ru;
            rd_all += rd;
            let mut worst = 0i64;
            let mut b = since;
            while b < until {
                let l = lb.get(&b).copied().unwrap_or((0, 0));
                let r = rsum.get(&b).copied().unwrap_or((0, 0));
                worst = worst.max((l.0 - r.0).abs() + (l.1 - r.1).abs());
                b += BUCKET;
            }
            let pct = |l: i64, r: i64| if l.max(r) > 0 { ((r - l) as f64 * 1000.0 / l.max(r) as f64).round() / 10.0 } else { 0.0 };
            rows.push(json!({"relay": relay, "since": since, "asset": short_id(a["name"].as_str().unwrap_or("")), "uuid": uuid, "tags": tags,
                "legacy": {"up": lu, "down": ld}, "relayd": {"up": ru, "down": rd},
                "diff_pct": {"up": pct(lu, ru), "down": pct(ld, rd)}, "worst_5m_bucket_bytes": worst}));
        }
    }
    let pct = |l: i64, r: i64| if l.max(r) > 0 { ((r - l) as f64 * 1000.0 / l.max(r) as f64).round() / 10.0 } else { 0.0 };
    json!({"window": {"since": since, "until": until}, "total": {"legacy": {"up": lu_all, "down": ld_all}, "relayd": {"up": ru_all, "down": rd_all},
        "diff_pct": {"up": pct(lu_all, ru_all), "down": pct(ld_all, rd_all)}}, "entries": rows,
        "method": "旧账本（/history 5 分钟桶，来自 minutes 表）对比 relayd xray_1m（同一入站 tag 的分钟增量按 5 分钟求和），最近一个完整小时"})
}
