//! 中控自己慢不慢、慢在哪（`server-stats`）：GET /server/stats（中控 selfmon/http.rs），不用登录中控看日志。
//! 连接池（主池、机器视图池、访问监控池）的现状与最近 15 分钟的采样，各接口（按路由模板）最近 15 分钟与启动以来的耗时分布。

use crate::cmds::Ctx;
use crate::table;
use serde_json::Value;

/// 毫秒：一位小数以内、超过 1 秒写成秒；没有为 -
fn ms(v: &Value) -> String {
    match v.as_f64() {
        None => "-".into(),
        Some(x) if x >= 1000.0 => format!("{:.1}s", x / 1000.0),
        Some(x) if x >= 10.0 => format!("{x:.0}"),
        Some(x) => format!("{x:.1}"),
    }
}

fn n(v: &Value) -> String { v.as_u64().map_or("-".into(), |x| x.to_string()) }

/// 人看的：概况、连接池表、接口表（`all` 为假时只列最近 15 分钟有请求的前 `top` 个）
fn text(d: &Value, top: usize, all: bool) -> String {
    let mins = d["recent_minutes"].as_u64().unwrap_or(15);
    let head = table::kv(&[
        ("启动于", format!("{}（已运行 {}）", table::ts(d["started_at"].as_str().unwrap_or("")), uptime(d["uptime_s"].as_i64().unwrap_or(0)))),
        ("进行中的请求", format!("{}（启动以来最多同时 {}）；连着的实时流 / 网页终端 {}", n(&d["inflight"]), n(&d["inflight_max"]), n(&d["streams"]))),
        (
            "算慢的门槛",
            format!("接口 {} ms，agent {} ms，期望状态长轮询 {} ms", n(&d["slow_ms"]["api"]), n(&d["slow_ms"]["agent"]), n(&d["slow_ms"]["long_poll"])),
        ),
    ]);
    let pools: Vec<Vec<String>> = d["pools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let a = &p["acquire_ms"];
            vec![
                pool_name(p["name"].as_str().unwrap_or("")),
                format!("{}/{}", n(&p["in_use"]), n(&p["max"])),
                n(&p["idle"]),
                n(&p["max_in_use"]),
                format!("{}/{}", n(&p["full_samples"]), n(&p["samples"])),
                if a["mean"].is_null() { "-".into() } else { format!("{} / {} / {}", ms(&a["last"]), ms(&a["mean"]), ms(&a["max"])) },
                n(&a["timeouts"]),
            ]
        })
        .collect();
    let pools = table::render(&["连接池", "占用/上限", "空闲", "最近最多占用", "占满的采样", "取连接 ms（最近/平均/最大）", "取不到"], &pools);
    let routes: Vec<&Value> = d["routes"].as_array().into_iter().flatten().filter(|r| all || r["recent"]["count"].as_u64().unwrap_or(0) > 0).collect();
    let shown = if all { routes.len() } else { routes.len().min(top) };
    let rows: Vec<Vec<String>> = routes[..shown]
        .iter()
        .map(|r| {
            let (x, t) = (&r["recent"], &r["total"]);
            vec![
                format!("{} {}", r["method"].as_str().unwrap_or(""), r["route"].as_str().unwrap_or("")),
                n(&x["count"]),
                n(&x["slow"]),
                n(&x["errors"]),
                ms(&x["p50_ms"]),
                ms(&x["p90_ms"]),
                ms(&x["p99_ms"]),
                ms(&x["max_ms"]),
                format!("{} / {} / {}", n(&t["count"]), ms(&t["p99_ms"]), ms(&t["max_ms"])),
            ]
        })
        .collect();
    let mut out = format!("{head}\n\n{pools}\n\n");
    if rows.is_empty() {
        out.push_str(&format!("最近 {mins} 分钟没有请求（加 --all 看启动以来的）"));
    } else {
        out.push_str(&table::render(
            &["接口", &format!("{mins}分钟内次数"), "慢", "5xx", "p50", "p90", "p99", "最大", "启动以来 次数 / p99 / 最大"],
            &rows,
        ));
        if shown < routes.len() {
            out.push_str(&format!("\n（按最近 {mins} 分钟占用的总时间排，只列前 {shown} 个，共 {} 个；--all 看全部）", routes.len()));
        }
    }
    out.push_str("\n分位数是分桶的上界（量级，不是精确值）；耗时量到响应头发出为止。日志里的慢请求：fleetctl server-logs 里找「慢请求」「请求出错」");
    out
}

fn pool_name(s: &str) -> String {
    match s {
        "main" => "主池".into(),
        "view" => "机器视图".into(),
        "access" => "访问监控".into(),
        x => x.into(),
    }
}

fn uptime(s: i64) -> String {
    match s {
        s if s >= 86_400 => format!("{} 天 {} 小时", s / 86_400, s % 86_400 / 3600),
        s if s >= 3600 => format!("{} 小时 {} 分", s / 3600, s % 3600 / 60),
        s => format!("{} 分 {} 秒", s / 60, s % 60),
    }
}

/// `server-stats [--top N] [--all]`；`--json` 是 /server/stats 的 data 原样
pub fn server_stats(ctx: &Ctx, top: usize, all: bool) -> anyhow::Result<u8> {
    let d = ctx.c.get("/server/stats")?;
    ctx.emit(&d, || text(&d, top, all));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_pools_and_busiest_routes() {
        let route = |r: &str, count: u64, p99: f64| {
            json!({"method": "GET", "route": r, "kind": "api",
                   "recent": {"count": count, "slow": 1, "errors": 0, "p50_ms": 10.0, "p90_ms": 50.0, "p99_ms": p99, "max_ms": p99},
                   "total": {"count": count * 3, "p99_ms": p99, "max_ms": 1500.0}})
        };
        let d = json!({
            "started_at": "2026-11-22T00:00:00Z", "uptime_s": 7322, "recent_minutes": 15, "inflight": 2, "inflight_max": 9, "streams": 1,
            "slow_ms": {"api": 500, "agent": 2000, "long_poll": 32000},
            "pools": [{"name": "main", "max": 20, "size": 6, "idle": 4, "in_use": 2, "max_in_use": 7, "samples": 180, "full_samples": 0,
                       "acquire_ms": {"last": 0.2, "mean": 0.4, "max": 1200.0, "timeouts": 0}},
                      {"name": "access", "max": 4, "size": 0, "idle": 0, "in_use": 0, "max_in_use": 0, "samples": 180, "full_samples": 0,
                       "acquire_ms": {"last": null, "mean": null, "max": null, "timeouts": 0}}],
            "routes": [route("/api/v1/overview", 90, 250.0), route("/api/v1/meta", 3, 5.0), route("/api/v1/jobs", 0, 5.0)],
        });
        let t = text(&d, 1, false);
        assert!(t.contains("2 小时 2 分") && t.contains("主池") && t.contains("2/20") && t.contains("0.2 / 0.4 / 1.2s"), "{t}");
        assert!(t.contains("GET /api/v1/overview") && !t.contains("/api/v1/meta") && t.contains("只列前 1 个，共 2 个"), "{t}");
        assert!(text(&d, 10, true).contains("GET /api/v1/jobs"), "--all 连最近没请求的也列");
    }
}
