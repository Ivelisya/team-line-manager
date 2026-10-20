//! 画像与检测器：学习期、新运营商、新城市、一号多地、机房来源、内部地址、出口变更（sticky）、到期自动关闭、告警输出、HTTP。
mod common;
use axum::body::{to_bytes, Body};
use axum::http::Request;
use ipintel::{Config, EgressObs, IpIntel, NodeMeta, SourceEvent};
use serde_json::Value;
use std::net::IpAddr;
use std::sync::Arc;
use tower::ServiceExt;

const T0: f64 = 1_790_000_000.0;

fn svc(dir: &std::path::Path) -> Arc<IpIntel> {
    let cfg = Config { db_dir: dir.to_path_buf(), learn_s: 3600.0, ..Config::default() };
    IpIntel::with_conn(cfg, rusqlite::Connection::open_in_memory().unwrap()).unwrap()
}

fn ev(id: &str, ts: f64, ip: &str, client: &str) -> SourceEvent {
    SourceEvent {
        id: Some(id.into()),
        ts,
        node: "uuid-1".into(),
        relay: Some("线路机2".into()),
        team: Some("团队1".into()),
        source_ip: ip.parse::<IpAddr>().unwrap(),
        client: Some(client.into()),
    }
}

fn kinds(s: &IpIntel) -> Vec<String> {
    let v = s.anomalies(&Default::default()).unwrap();
    let mut k: Vec<String> = v["anomalies"].as_array().unwrap().iter().map(|a| a["kind"].as_str().unwrap().to_string()).collect();
    k.sort();
    k
}

#[test]
fn detectors_end_to_end() {
    let dir = need_dbs!();
    let s = svc(&dir);
    s.upsert_nodes(
        vec![NodeMeta {
            node: "uuid-1".into(),
            short: "t1-01".into(),
            name: "团队1-01".into(),
            relay: Some("线路机2".into()),
            team: Some("团队1".into()),
            egress_ip: Some("12.34.56.78".parse().unwrap()),
        }],
        T0,
    )
    .unwrap();

    // 学习期内：郑州联通，不报
    let r = s.ingest_sources(vec![ev("a1", T0, "202.102.224.68", "c1"), ev("a1", T0, "202.102.224.68", "c1")], T0).unwrap();
    assert_eq!(r.events, 1);
    assert_eq!(r.duplicates, 1);
    assert!(r.anomalies_opened.is_empty());

    // 学习期后：广州电信 → 新运营商 + 新城市
    let r = s.ingest_sources(vec![ev("a2", T0 + 7200.0, "202.96.128.86", "c2")], T0 + 7200.0).unwrap();
    assert_eq!(r.anomalies_opened.len(), 2, "{:?}", kinds(&s));
    assert_eq!(kinds(&s), vec!["new_city", "new_isp"]);

    // 同一客户端 c1：先广州（离上次郑州 7260 s，超出窗口，不算），40 s 后又从郑州 → 一号多地
    s.ingest_sources(vec![ev("a3", T0 + 7260.0, "202.96.128.86", "c1")], T0 + 7260.0).unwrap();
    assert!(!kinds(&s).contains(&"shared_identity".to_string()));
    let r = s.ingest_sources(vec![ev("a4", T0 + 7300.0, "202.102.224.68", "c1")], T0 + 7300.0).unwrap();
    assert_eq!(r.anomalies_opened.len(), 1);
    let v = s.anomalies(&ipintel::queries::AnomalyFilter { kind: Some("shared_identity".into()), ..Default::default() }).unwrap();
    let a = &v["anomalies"][0];
    assert_eq!(a["subject"], "c1");
    let km = a["evidence"]["max_km"].as_f64().unwrap();
    assert!(km > 1000.0, "{km}");
    // 再来一次只累加，不新开
    let r = s.ingest_sources(vec![ev("a5", T0 + 7310.0, "202.96.128.86", "c1")], T0 + 7310.0).unwrap();
    assert!(r.anomalies_opened.is_empty());
    assert!(r.anomalies_bumped >= 1);

    // 机房来源（Google）
    let r = s.ingest_sources(vec![ev("a6", T0 + 7400.0, "8.8.8.8", "c3")], T0 + 7400.0).unwrap();
    assert!(kinds(&s).contains(&"datacenter_source".to_string()), "{r:?}");

    // 内部地址（节点自己的出口）不报任何东西
    let before = kinds(&s).len();
    s.ingest_sources(vec![ev("a7", T0 + 7500.0, "12.34.56.78", "c4")], T0 + 7500.0).unwrap();
    assert_eq!(kinds(&s).len(), before);

    // 出口：基线 → 新地址 1 次（待确认）→ 同一 measured_at 重复读到不计 → 第 2 次确认 → critical sticky
    let eg = |ts: f64, ip: &str| EgressObs { node: "uuid-1".into(), ts, ip: ip.parse().unwrap(), via: "egress".into(), expected: Some("12.34.56.78".parse().unwrap()) };
    s.ingest_egress(vec![eg(T0, "12.34.56.78")], T0).unwrap();
    let r = s.ingest_egress(vec![eg(T0 + 300.0, "12.34.56.79"), eg(T0 + 300.0, "12.34.56.79")], T0 + 300.0).unwrap();
    assert_eq!((r.events, r.duplicates), (1, 1));
    assert!(r.anomalies_opened.is_empty());
    let r = s.ingest_egress(vec![eg(T0 + 600.0, "12.34.56.79")], T0 + 600.0).unwrap();
    assert_eq!(r.anomalies_opened.len(), 1);
    let eid = r.anomalies_opened[0];

    // 告警输出
    let obs = s.observations(T0 + 7600.0).unwrap();
    let rules: Vec<&str> = obs.iter().map(|o| o.rule.as_str()).collect();
    for r in ["ipintel_new_isp", "ipintel_new_city", "ipintel_shared_identity", "ipintel_datacenter_source", "ipintel_egress_ip_changed"] {
        assert!(rules.contains(&r), "缺 {r}: {rules:?}");
    }
    let e = obs.iter().find(|o| o.rule == "ipintel_egress_ip_changed").unwrap();
    assert_eq!(e.severity, "critical");
    assert_eq!(e.labels["node"], "t1-01");
    assert_eq!(e.labels["relay"], "线路机2");
    assert_eq!(e.labels["kind"], "node");
    assert!(e.notify_resolved);
    assert!(e.summary.contains("12.34.56.78 → 12.34.56.79"), "{}", e.summary);
    let sh = obs.iter().find(|o| o.rule == "ipintel_shared_identity").unwrap();
    assert_eq!(sh.labels["kind"], "identity");
    assert_eq!(sh.labels["client"], "c1");

    // 两天后：一次性异常自动关闭，出口变更仍在
    let m = s.maintain(T0 + 7600.0 + 2.0 * 86400.0).unwrap();
    assert!(m["auto_closed"].as_u64().unwrap() >= 4, "{m}");
    let obs = s.observations(T0 + 3.0 * 86400.0).unwrap();
    assert_eq!(obs.len(), 1);
    assert_eq!(obs[0].rule, "ipintel_egress_ip_changed");
    // 人工关闭
    s.close_anomaly(eid, "admin", Some("已核实：供应商换了 IP"), T0 + 3.0 * 86400.0).unwrap();
    assert!(s.close_anomaly(eid, "admin", None, T0 + 3.0 * 86400.0).is_err());
    assert!(s.observations(T0 + 3.0 * 86400.0).unwrap().is_empty());

    // 画像查询
    let ns = s.node_sources("t1-01", 30.0 * 86400.0, 10, T0 + 7600.0).unwrap().unwrap();
    assert_eq!(ns["node"]["id"], "t1-01");
    assert!(ns["distinct_sources"].as_u64().unwrap() >= 4);
    let isps: Vec<&str> = ns["distribution"]["isp"].as_array().unwrap().iter().map(|r| r["key"].as_str().unwrap()).collect();
    assert!(isps.contains(&"中国联通") && isps.contains(&"中国电信"), "{isps:?}");
    assert_eq!(ns["egress"]["current"][0]["ip"], "12.34.56.79");
    assert_eq!(ns["egress"]["history"].as_array().unwrap().len(), 2);
    let d = s.ip_detail("202.102.224.68".parse().unwrap(), T0 + 7600.0).unwrap();
    assert_eq!(d["intel"]["isp"], "中国联通");
    assert_eq!(d["seen"]["as_source"]["nodes"][0]["node"], "t1-01");
}

async fn get(app: &axum::Router, uri: &str) -> (u16, Value) {
    let r = app.clone().oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
    let st = r.status().as_u16();
    let b = to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (st, serde_json::from_slice(&b).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn http_routes() {
    let dir = need_dbs!();
    let s = svc(&dir);
    s.ingest_sources(vec![ev("h1", T0, "202.102.224.68", "c1")], T0).unwrap();
    let app: axum::Router = ipintel::api::router(s.clone());
    let (st, v) = get(&app, "/v2/api/v1/ipintel/8.8.8.8").await;
    assert_eq!(st, 200);
    assert_eq!(v["data"]["intel"]["asn"], 15169);
    let (st, v) = get(&app, "/v2/api/v1/ipintel/not-an-ip").await;
    assert_eq!(st, 400);
    assert_eq!(v["error"], "IP 地址格式不正确");
    let (st, v) = get(&app, "/v2/api/v1/ipintel/anomalies?status=all").await;
    assert_eq!(st, 200);
    assert!(v["data"]["counts"]["open"].is_number());
    let (st, _) = get(&app, "/v2/api/v1/ipintel/anomalies?status=bogus").await;
    assert_eq!(st, 400);
    // 窗口按当前时间算：30 天窗口包含 T0，1 小时窗口不包含
    let (st, v) = get(&app, "/v2/api/v1/nodes/uuid-1/sources?window=30d").await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["total"], 1);
    assert_eq!(v["data"]["top_sources"][0]["intel"]["isp"], "中国联通");
    let (_, v) = get(&app, "/v2/api/v1/nodes/uuid-1/sources?window=1h").await;
    assert_eq!(v["data"]["total"], 0);
    let (st, v) = get(&app, "/v2/api/v1/nodes/uuid-1/sources?window=bad").await;
    assert_eq!(st, 400, "{v}");
    let (st, _) = get(&app, "/v2/api/v1/nodes/nope/sources").await;
    assert_eq!(st, 404);
    let (st, v) = get(&app, "/v2/api/v1/ipintel/status").await;
    assert_eq!(st, 200);
    assert_eq!(v["data"]["loaded"], 5);
    // 关闭：不存在的 → 409
    let r = app
        .clone()
        .oneshot(Request::builder().method("POST").uri("/v2/api/v1/ipintel/anomalies/999/close").header("content-type", "application/json").body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 409);
}
