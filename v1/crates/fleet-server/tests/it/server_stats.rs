//! 中控自己的接口耗时与连接池（selfmon/http.rs）：按路由模板记（日志、统计里没有原始路径里的 id），慢的记 INFO，
//! agent 接口的日志带机器；GET /server/stats 的形状。需要 PostgreSQL（见 common）。

use super::common::{self, seed_relay, Sim};
use serde_json::Value;
use std::io::Write;
use std::sync::{Arc, Mutex};

/// 收日志的缓冲（本线程的 tracing 订阅者写进来）
#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf { self.clone() }
}

fn route<'a>(d: &'a Value, method: &str, r: &str) -> &'a Value {
    d["routes"].as_array().unwrap().iter().find(|x| x["method"] == method && x["route"] == r).unwrap_or_else(|| panic!("{method} {r}：{d}"))
}

/// 单线程运行时：请求都在本线程上处理，本线程的订阅者收得到它们的日志
#[tokio::test]
async fn request_log_and_stats_by_route_template() {
    let buf = Buf::default();
    let sub = tracing_subscriber::fmt().with_writer(buf.clone()).with_max_level(tracing::Level::DEBUG).with_ansi(false).finish();
    let _g = tracing::subscriber::set_default(sub);
    // 整个进程只有一个订阅者时，tracing 按「调用点第一次被碰到的那个线程」的订阅者决定记不记：别的测试线程（没有订阅者）
    // 先碰到请求层的 DEBUG 调用点，它就被缓存成「不记」。再登记一个空订阅者，tracing 改成问遍所有登记过的订阅者；
    // 已经缓存的按现在的订阅者重算
    let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    tracing::callsite::rebuild_interest_cache();
    // 接口一律算慢（0 毫秒）：每个都记 INFO「慢请求」；agent 接口按默认 2 秒，不算慢
    let t = common::boot(false, "selfmon.slow_request_ms = 0").await;
    let (relay, tok) = seed_relay(&t).await;
    let other = uuid::Uuid::now_v7();
    for id in [relay, other] {
        t.req("GET", &format!("/api/v1/machines/{id}"), None, None).await;
    }
    t.req("GET", "/api/v1/no/such/thing", None, None).await;
    Sim::new(tok).round(&t).await;
    fleet_server::selfmon::http::sample_once(&t.st).await;
    let d = t.get("/api/v1/server/stats").await;

    // 两台机器（一台不存在、404）合在同一个路由模板下；没匹配到的不按原始路径记
    let m = route(&d, "GET", "/api/v1/machines/{id}");
    assert_eq!((m["kind"].as_str(), m["recent"]["count"].as_u64(), m["total"]["count"].as_u64(), m["recent"]["slow"].as_u64()), (Some("api"), Some(2), Some(2), Some(2)), "{m}");
    assert!(m["recent"]["p50_ms"].is_number() && m["recent"]["max_ms"].as_f64().unwrap() >= m["recent"]["p50_ms"].as_f64().unwrap(), "{m}");
    assert_eq!(route(&d, "GET", "(没有这个接口)")["total"]["count"], 1);
    let rep = route(&d, "POST", "/agent/v1/report");
    assert_eq!((rep["kind"].as_str(), rep["total"]["slow"].as_u64(), rep["total"]["errors"].as_u64()), (Some("agent"), Some(0), Some(0)), "{rep}");
    assert_eq!(route(&d, "GET", "/agent/v1/desired")["kind"], "long_poll");
    assert!(d["routes"].as_array().unwrap().iter().all(|r| !r["route"].as_str().unwrap().contains(&relay.to_string())));
    // 连接池：三个都在，主池与视图池计了取连接，访问监控池不计
    let pools = d["pools"].as_array().unwrap();
    let names: Vec<&str> = pools.iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["main", "view", "access"]);
    assert!(pools[0]["acquire_ms"]["last"].is_number() && pools[0]["max"] == 5 && pools[0]["samples"] == 1, "{}", pools[0]);
    assert!(pools[2]["acquire_ms"]["last"].is_null(), "{}", pools[2]);
    assert_eq!((d["slow_ms"]["api"].as_u64(), d["slow_ms"]["agent"].as_u64(), d["slow_ms"]["long_poll"].as_u64()), (Some(0), Some(2000), Some(32000)));
    assert!(d["inflight"].as_i64() == Some(1) && d["started_at"].is_string(), "这一个请求自己在进行中：{d}");

    let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let line = |pat: &str| logs.lines().find(|l| l.contains(pat)).unwrap_or_else(|| panic!("没有「{pat}」：{logs}")).to_string();
    let slow = line("route=/api/v1/machines/{id} status=404");
    assert!(slow.contains(" INFO ") && slow.contains("慢请求") && slow.contains("method=GET"), "{slow}");
    // 只看请求层自己的日志：别的模块的 DEBUG（比如 SQL 的绑定参数）里有 id 不归这里管
    let http: Vec<&str> = logs.lines().filter(|l| l.contains("fleet_server::http")).collect();
    assert!(http.iter().all(|l| !l.contains(&other.to_string()) && !l.contains(&format!("machines/{relay}"))), "原始路径里的 id 不进请求日志：{http:?}");
    let agent = line("route=/agent/v1/report");
    assert!(agent.contains("DEBUG") && agent.contains(&format!("machine={relay}")) && agent.contains("status=200"), "{agent}");
    if std::env::var_os("FLEET_SHOW_STATS").is_some() {
        eprintln!("{}\n{}", serde_json::to_string_pretty(&d).unwrap(), logs.lines().filter(|l| l.contains("fleet_server::http")).collect::<Vec<_>>().join("\n"));
    }
}
