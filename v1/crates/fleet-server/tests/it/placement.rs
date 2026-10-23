//! 放置矩阵（placement.rs）：在用 VPS 节点进期望状态，上报的往返累加进小时汇总，/placement 选出最优线路机并给出批量换线命令。

use crate::common;

use axum::http::StatusCode;
use common::tele::*;
use fleet_proto::{DesiredState, RttSample};
use serde_json::json;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rtt_reports_roll_up_and_feed_placement() {
    let t = setup().await;
    let r1 = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let r2 = seed_relay(&t, "线路机2", "198.51.100.2", "tok2").await;
    let team = seed_team(&t, "团队1").await;
    let (node, _) = seed_node(&t, r1, team, 1, "203.0.113.5", 20001).await;
    sqlx::query("update machine set kind = 'vps', ssh_port = 2222 where id = $1").bind(node).execute(&t.st.db).await.unwrap();
    // 家宽代理不测
    seed_node(&t, r2, team, 2, "203.0.113.6", 20002).await;

    // 期望状态：全部在用 VPS 节点（不只是挂在这台上的）
    let (s, v) = t.req("GET", "/agent/v1/desired?after=0", None, Some("tok2")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let d: DesiredState = serde_json::from_value(v).unwrap();
    let rtt = d.rtt.unwrap();
    assert_eq!((rtt.interval_s, rtt.targets.len(), rtt.targets[0].node_id, rtt.targets[0].host.as_str(), rtt.targets[0].port), (300, 1, node, "203.0.113.5", 2222));
    // 节点清单第一次记下：全部在用线路机代数 +1；没变不再动
    assert_eq!(fleet_server::placement::sync_targets(&t.st).await.unwrap(), 2);
    assert_eq!(fleet_server::placement::sync_targets(&t.st).await.unwrap(), 0);

    // 同一小时里 r1：通 3 轮 + 不通 1 轮，再补一轮；不存在的节点、7 天前的不收
    let now = common::now_ms() / 3_600_000 * 3_600_000 + 60_000;
    let s = |node_id: Uuid, ago_ms: i64, ok: bool, rtt: Option<f32>| RttSample { node_id, ts_ms: now - ago_ms, ok, rtt_ms: rtt };
    let mut r = blank_report(1);
    r.xray = Some(xray(true));
    r.rtt = vec![s(node, 0, true, Some(100.0)), s(node, 1, true, Some(120.0)), s(node, 2, true, Some(110.0)), s(node, 3, false, None),
                 s(Uuid::now_v7(), 0, true, Some(1.0)), s(node, 8 * 86_400_000, true, Some(1.0))];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    let mut r = blank_report(2);
    r.xray = Some(xray(true));
    r.rtt = vec![s(node, 4, true, Some(130.0))];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    let row: (i32, i32, Option<f32>, Option<f32>) =
        sqlx::query_as("select samples, ok, p50_ms, max_ms from relay_node_rtt_hourly where relay_id = $1 and node_id = $2").bind(r1).bind(node).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(row, (5, 4, Some(115.0), Some(130.0)));
    assert_eq!(t.count("select count(*) from relay_node_rtt_hourly").await, 1);

    // r2 全通、更快：当前 r1 成功率 80% → 建议换到 r2
    let mut r = blank_report(1);
    r.xray = Some(xray(true));
    r.rtt = vec![s(node, 0, true, Some(60.0)), s(node, 1, true, Some(62.0))];
    assert_eq!(t.report("tok2", &r).await.0, StatusCode::OK);
    let p = t.get("/api/v1/placement?team=团队1").await;
    let n = &p["nodes"][0];
    assert_eq!(p["nodes"].as_array().unwrap().len(), 1, "{p}");
    assert_eq!((n["current_relay_id"].clone(), n["best_relay_id"].clone(), n["suggest"].clone(), n["reason"].clone()), (json!(r1), json!(r2), json!(true), json!("当前成功率 80%")), "{p}");
    let c1 = n["cells"].as_array().unwrap().iter().find(|c| c["relay_id"] == json!(r1)).unwrap();
    assert_eq!((c1["samples"].clone(), c1["success_pct"].clone(), c1["p50_ms"].clone()), (json!(5), json!(80.0), json!(115.0)));
    let sg = &p["suggestions"][0];
    assert_eq!(sg["command"], json!(format!("fleetctl move {} --to 线路机2", n["name"].as_str().unwrap())), "{p}");
    assert_eq!(t.req("GET", "/api/v1/placement?team=没有", None, None).await.0, StatusCode::NOT_FOUND);
}
