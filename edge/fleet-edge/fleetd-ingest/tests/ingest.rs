use axum::body::Body;
use axum::http::{Request, StatusCode};
use fleetd_ingest::{api, now, Ingest, IngestCfg};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn app(ing: &fleetd_ingest::Shared) -> axum::Router {
    api::ingest_routes(ing.clone()).merge(api::read_routes(ing.clone()))
}

fn sample(ts: f64, mem: f64, disk: f64, steal: f64) -> Value {
    json!({"ts": ts, "cpu": {"pct": 20.0, "user": 10.0, "system": 5.0, "iowait": 1.0, "steal": steal},
           "load": [0.5, 0.4, 0.3], "mem": {"total": 4_000_000_000u64, "used": 1_000_000_000u64, "avail": 3_000_000_000u64, "pct": mem},
           "swap": {"total": 0, "used": 0, "pct": 0.0}, "disk": {"total": 40_000_000_000u64, "used": 10_000_000_000u64, "pct": disk},
           "net": {"ifaces": ["eth0"], "rx_bytes": 1000, "tx_bytes": 2000, "rx_bps": 1500.0, "tx_bps": 2500.0},
           "sock": {"tcp": 10, "udp": 2, "tcp_tw": 0, "tcp_orphan": 0}, "procs": 99, "uptime": 1000.0})
}

fn batch(host: &str, kind: &str, seq: u64, samples: Vec<Value>) -> Value {
    json!({"v": 1, "kind": kind, "host_id": host, "boot_id": "b-1", "seq": seq, "agent": "nodeagent/0.1.0",
           "created_at": now(), "meta": {"kernel": "6.1", "os": "Debian 12", "ncpu": 2}, "outbox": {"pending": 0, "dropped": 0}, "samples": samples})
}

async fn post(app: &axum::Router, path: &str, token: Option<&str>, body: Vec<u8>) -> (StatusCode, Value) {
    let mut rb = Request::post(path).header("content-type", "application/json");
    if let Some(t) = token {
        rb = rb.header("authorization", format!("Bearer {t}"));
    }
    let r = app.clone().oneshot(rb.body(Body::from(body)).unwrap()).await.unwrap();
    let st = r.status();
    let b = r.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let r = app.clone().oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap();
    let st = r.status();
    let b = r.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

#[tokio::test]
async fn auth_dedupe_rollup_and_read() {
    let ing = Ingest::open_memory(IngestCfg::default());
    let a = app(&ing);
    let tok = ing.issue_token("n1", "default").unwrap();
    let t0 = (now() as i64 / 60 * 60 - 120) as f64;
    let b1 = batch("n1", "node", 1, vec![sample(t0, 50.0, 30.0, 1.0), sample(t0 + 10.0, 60.0, 30.0, 1.0), sample(t0 + 20.0, 70.0, 30.0, 1.0)]);
    let body = serde_json::to_vec(&b1).unwrap();

    // 无令牌 / 错令牌
    assert_eq!(post(&a, "/v2/ingest/node", None, body.clone()).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(post(&a, "/v2/ingest/node", Some("fe1_0000000000000000000000000000"), body.clone()).await.0, StatusCode::UNAUTHORIZED);
    // kind 不一致
    assert_eq!(post(&a, "/v2/ingest/relay", Some(&tok), body.clone()).await.0, StatusCode::BAD_REQUEST);
    // 正常
    let (st, v) = post(&a, "/v2/ingest/node", Some(&tok), body.clone()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "ok");
    // 重复
    let (st, v) = post(&a, "/v2/ingest/node", Some(&tok), body.clone()).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["status"], "duplicate");
    // 同一分钟的第二批（seq 2）累加进同一行
    let b2 = batch("n1", "node", 2, vec![sample(t0 + 30.0, 80.0, 30.0, 1.0)]);
    assert_eq!(post(&a, "/v2/ingest/node", Some(&tok), serde_json::to_vec(&b2).unwrap()).await.0, StatusCode::OK);

    // host_id 冒充别的主机
    let evil = batch("n2", "node", 3, vec![sample(t0, 1.0, 1.0, 0.0)]);
    assert_eq!(post(&a, "/v2/ingest/node", Some(&tok), serde_json::to_vec(&evil).unwrap()).await.0, StatusCode::FORBIDDEN);

    let (st, v) = get(&a, "/v2/api/v1/hosts").await;
    assert_eq!(st, StatusCode::OK);
    let h = &v["data"][0];
    assert_eq!(h["host_id"], "n1");
    assert_eq!(h["online"], true);
    assert_eq!(h["latest"]["mem"]["pct"], 80.0);
    assert_eq!(h["batches"], 2);

    let (st, v) = get(&a, "/v2/api/v1/hosts/n1/series?metric=mem,rx&range=1h").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let pts = v["data"]["series"]["mem"].as_array().unwrap();
    assert_eq!(pts.len(), 1);
    assert_eq!(pts[0][1], 65.0); // (50+60+70+80)/4
    assert_eq!(pts[0][2], 80.0);
    assert_eq!(v["data"]["series"]["rx"][0][1], 1500.0);
    assert_eq!(get(&a, "/v2/api/v1/hosts/n1/series?metric=bogus").await.0, StatusCode::BAD_REQUEST);

    // 撤销后拒收
    ing.revoke_tokens("n1", None).unwrap();
    let b3 = batch("n1", "node", 4, vec![sample(t0 + 40.0, 10.0, 10.0, 0.0)]);
    assert_eq!(post(&a, "/v2/ingest/node", Some(&tok), serde_json::to_vec(&b3).unwrap()).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn body_cap_and_rate_limit() {
    let ing = Ingest::open_memory(IngestCfg { burst: 3.0, rate_per_s: 0.01, ..Default::default() });
    let a = app(&ing);
    let tok = ing.issue_token("n1", "default").unwrap();
    let big = vec![b' '; 1200 * 1024];
    assert_eq!(post(&a, "/v2/ingest/node", Some(&tok), big).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    let mut codes = vec![];
    for seq in 1..=5u64 {
        let b = batch("n1", "node", seq, vec![sample(now(), 10.0, 10.0, 0.0)]);
        codes.push(post(&a, "/v2/ingest/node", Some(&tok), serde_json::to_vec(&b).unwrap()).await.0);
    }
    assert!(codes.contains(&StatusCode::TOO_MANY_REQUESTS), "{codes:?}");
}

#[tokio::test]
async fn relay_xray_cumulative_and_window() {
    let ing = Ingest::open_memory(IngestCfg::default());
    let a = app(&ing);
    let tok = ing.issue_token("line3", "default").unwrap();
    let t = now();
    let mut s1 = sample(t - 20.0, 10.0, 10.0, 0.0);
    s1["xray"] = json!({"ok": true, "d": [["inbound", "inbound-20001", 100, 1000], ["user", "client-1", 100, 1000]], "conns": {"20001": 3}, "latency_ms": 0.4});
    let mut s2 = sample(t - 10.0, 10.0, 10.0, 0.0);
    s2["xray"] = json!({"ok": true, "d": [["inbound", "inbound-20001", 50, 500]], "conns": {"20001": 4}, "latency_ms": 0.5});
    let mut b = batch("line3", "relay", 1, vec![s1, s2]);
    b["xray"] = json!({"api": "127.0.0.1:62789", "xray": {"running": true, "pid": 1, "version": "26.7.28"}, "xui": {"running": true},
        "accum_since": 1.0, "entries": [["inbound", "inbound-20001", 5150, 51500, 150, 1500, 5.0, 50.0], ["user", "client-1", 100, 1000, 100, 1000, 0.0, 0.0]]});
    let (st, v) = post(&a, "/v2/ingest/relay", Some(&tok), serde_json::to_vec(&b).unwrap()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = get(&a, "/v2/api/v1/relays/line3/xray?range=1h&kind=inbound&tag=inbound-20001").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let d = &v["data"];
    assert_eq!(d["process"]["xray"]["version"], "26.7.28");
    let e = d["entries"].as_array().unwrap().iter().find(|e| e["tag"] == "inbound-20001").unwrap();
    assert_eq!(e["cum_down"], 51500);
    assert_eq!(e["window_up"], 150);
    assert_eq!(e["window_down"], 1500);
    assert_eq!(e["rate_down_bps"], 50.0);
    assert_eq!(e["conns"], 4);
    let pts: i64 = d["series"]["points"].as_array().unwrap().iter().map(|p| p[2].as_i64().unwrap()).sum();
    assert_eq!(pts, 1500);
    assert_eq!(get(&a, "/v2/api/v1/relays/nope/xray").await.0, StatusCode::NOT_FOUND);

    // 探测结果：自然键幂等（第二批重复带同一条也只落一次）
    let pr = |id: i64, at: f64, fin: bool| json!({"id": id, "node": "n-26", "check": "egress", "status": "ok", "value_ms": 40.0, "actual_ip": "1.2.3.4",
        "reason_code": null, "measured_at": at, "attempt": 1, "final": fin, "legacy_rc": 0});
    for (seq, res) in [(2u64, vec![pr(1, t - 30.0, true), pr(2, t - 5.0, false)]), (3u64, vec![pr(2, t - 5.0, false)])] {
        let mut b = batch("line3", "relay", seq, vec![sample(t, 10.0, 10.0, 0.0)]);
        b["probe"] = json!({"schema": "fleet.probe.v1", "relay": "line3", "generated_at": t, "first_id": 1, "last_id": 2, "results": res,
            "targets": [{"node": "n-26", "name": "团队1-26", "tag": "ss-1-2-3-4", "addr": "1.2.3.4", "port": 8388, "method": "aes-256-gcm", "expected_ip": "1.2.3.4", "udp": false}]});
        assert_eq!(post(&a, "/v2/ingest/relay", Some(&tok), serde_json::to_vec(&b).unwrap()).await.0, StatusCode::OK);
    }
    let n: i64 = ing.with_db(|c| c.query_row("SELECT COUNT(*) FROM probe_land", [], |r| r.get(0)).unwrap());
    assert_eq!(n, 2);
    let (st, v) = get(&a, "/v2/api/v1/relays/line3/probes?range=1h").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["targets"][0]["tag"], "ss-1-2-3-4");
    // L2 最新取 final 的那次
    // 独立编译（未开 serde_json/float_roundtrip）时 f64 解析可能差最后一位
    assert!((v["data"]["latest"][0]["measured_at"].as_f64().unwrap() - (t - 30.0)).abs() < 1e-6);
}

#[tokio::test]
async fn native_alerts() {
    let ing = Ingest::open_memory(IngestCfg::default());
    let a = app(&ing);
    let tok = ing.issue_token("hot", "default").unwrap();
    let t = now();
    // 内存 95% 持续 150 s；steal 20% 持续 12 分钟
    let mut samples = vec![];
    let mut ts = t - 12.0 * 60.0;
    while ts <= t {
        let mem = if ts >= t - 150.0 { 95.0 } else { 50.0 };
        samples.push(sample(ts, mem, 30.0, 20.0));
        ts += 10.0;
    }
    let b = batch("hot", "node", 1, samples);
    assert_eq!(post(&a, "/v2/ingest/node", Some(&tok), serde_json::to_vec(&b).unwrap()).await.0, StatusCode::OK);
    let al = ing.evaluate(t + 1.0);
    let res = al.iter().find(|x| x.rule == "native_host_resource").expect("resource");
    assert!(res.firing, "{res:?}");
    let st = al.iter().find(|x| x.rule == "native_cpu_steal").expect("steal");
    assert!(st.firing, "{st:?}");
    assert!(!al.iter().any(|x| x.rule == "native_cpu_iowait"));
    assert!(!al.iter().any(|x| x.rule == "native_host_offline"));
    // 200 s 后没有新批次 → 离线（且不再评估资源类）
    let al = ing.evaluate(t + 200.0);
    assert_eq!(al.len(), 1);
    assert_eq!(al[0].rule, "native_host_offline");
    // 撤销令牌 = 下线主机，不再告警
    ing.revoke_tokens("hot", None).unwrap();
    assert!(ing.evaluate(t + 200.0).is_empty());
}

#[test]
fn purge_and_range() {
    assert_eq!(api::parse_range(Some("24h"), 1).unwrap(), 86400);
    assert_eq!(api::parse_range(Some("90d"), 1).unwrap(), 90 * 86400);
    assert_eq!(api::parse_range(None, 7).unwrap(), 7);
    assert!(api::parse_range(Some("x"), 1).is_err());
    let ing = Ingest::open_memory(IngestCfg::default());
    let _ = ing.purge(now());
}
