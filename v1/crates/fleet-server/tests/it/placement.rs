//! 放置矩阵（placement.rs）：在用 VPS 节点从 /agent/v1/rtt 下发，上报的往返累加进小时汇总，/placement 选出最优线路机并给出批量换线命令。

use crate::common;

use axum::http::StatusCode;
use common::tele::*;
use fleet_proto::{DesiredState, RttConfig, RttSample};
use serde_json::json;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rtt_reports_roll_up_and_feed_placement() {
    let t = setup().await;
    let r1 = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let r2 = seed_relay(&t, "线路机2", "198.51.100.2", "tok2").await;
    let team = seed_team(&t, "团队1").await;
    let (node, _) = seed_node(&t, r1, team, 1, "203.0.113.5", 20001).await;
    sqlx::query("update machine set kind = 'vps' where id = $1").bind(node).execute(&t.st.db).await.unwrap();
    // 家宽代理不测
    seed_node(&t, r2, team, 2, "203.0.113.6", 20002).await;

    // 节点清单单独取（不进期望状态、不动线路机代数）：全部在用 VPS 节点，不只是挂在这台上的
    let (s, v) = t.req("GET", "/agent/v1/rtt", None, Some("tok2")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rtt: RttConfig = serde_json::from_value(v).unwrap();
    assert_eq!((rtt.interval_s, rtt.targets.len(), rtt.targets[0].node_id, rtt.targets[0].host.as_str()), (300, 1, node, "203.0.113.5"));
    let (_, v) = t.req("GET", "/agent/v1/desired?after=0", None, Some("tok2")).await;
    assert!(v.get("rtt").is_none(), "{v}");

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

    // 近 7 天的 tcp_fail 告警次数只数严重的（含复发合并的次数）：警告级的（TCP 偶发失败、出口正常）不算；
    // 出现过严重的（detail 有 critical_count）复发重开时 severity 被改成本次的警告，照样算；按 critical_count 算（严重 2 次，复发重开成警告的 6 次不算）
    sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary, reopen_count, state, resolved_at, detail) values ($1, 'machine', 'tcp_fail', $2, 'warning', 'x', 2, 'resolved', now(), '{\"critical_count\": 3}')")
        .bind(Uuid::now_v7())
        .bind(node)
        .execute(&t.st.db)
        .await
        .unwrap();
    for (sev, reopen) in [("critical", 1), ("warning", 3)] {
        sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary, reopen_count, state, resolved_at, detail) values ($1, 'machine', 'tcp_fail', $2, $3::severity, 'x', $4, 'resolved', now(), case when $3::severity = 'critical' then jsonb_build_object('critical_count', 1 + $4) else '{}'::jsonb end)")
            .bind(Uuid::now_v7())
            .bind(node)
            .bind(sev)
            .bind(reopen)
            .execute(&t.st.db)
            .await
            .unwrap();
    }

    sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary, reopen_count, state, resolved_at, detail) values ($1, 'machine', 'tcp_fail', $2, 'warning', 'x', 6, 'resolved', now(), '{\"critical_count\": 2}')")
        .bind(Uuid::now_v7())
        .bind(node)
        .execute(&t.st.db)
        .await
        .unwrap();

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
    assert_eq!(n["tcp_fail_7d"], json!(7), "严重的 1 次 + 复发 1 次，加上重开成警告的 critical_count = 3 的 3 次，加上 critical_count = 2 的 2 次；没标记的警告级不算：{p}");
    let sg = &p["suggestions"][0];
    assert_eq!(sg["command"], json!(format!("fleetctl move {} --to 线路机2", n["name"].as_str().unwrap())), "{p}");
    assert_eq!(t.req("GET", "/api/v1/placement?team=没有", None, None).await.0, StatusCode::NOT_FOUND);

    // node 过滤（显示名或 id）；不是在用的 VPS 节点 404
    let nm = n["name"].as_str().unwrap().to_string();
    assert_eq!(n["entry_relay_ids"], json!([r1]));
    assert_eq!(t.get(&format!("/api/v1/placement?node={nm}")).await["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(t.get(&format!("/api/v1/placement?node={node}")).await["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(t.req("GET", "/api/v1/placement?node=没有", None, None).await.0, StatusCode::NOT_FOUND);

    // 当前线路机的口径同换线规划：还有一条待应用的入口 → 没有「当前」、不给批量建议
    let e2 = Uuid::now_v7();
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, 20009, $4, 'abcd', 'pending')")
        .bind(e2).bind(r2).bind(node).bind(Uuid::now_v7()).execute(&t.st.db).await.unwrap();
    t.st.live.invalidate();
    let p = t.get("/api/v1/placement?team=团队1").await;
    assert_eq!((p["nodes"][0]["current_relay_id"].clone(), p["suggestions"].as_array().unwrap().len()), (json!(null), 0), "{p}");
    // 两条都在用：多入口节点只给带 --from 的单条建议（目标是节点还没有入口、更快的线路机3），不进批量命令
    sqlx::query("update entry set lifecycle = 'active' where id = $1").bind(e2).execute(&t.st.db).await.unwrap();
    seed_relay(&t, "线路机3", "198.51.100.3", "tok3").await;
    let mut r = blank_report(1);
    r.xray = Some(xray(true));
    r.rtt = vec![s(node, 0, true, Some(40.0)), s(node, 1, true, Some(42.0))];
    assert_eq!(t.report("tok3", &r).await.0, StatusCode::OK);
    t.st.live.invalidate();
    let p = t.get("/api/v1/placement?team=团队1").await;
    assert_eq!((p["nodes"][0]["entry_relay_ids"].as_array().unwrap().len(), p["suggestions"].as_array().unwrap().len()), (2, 0), "{p}");
    assert!(p["single_suggestions"].as_array().unwrap().iter().any(|g| g["command"] == json!(format!("fleetctl move {nm} --from 线路机1 --to 线路机3"))), "{p}");
    sqlx::query("delete from entry where id = $1").bind(e2).execute(&t.st.db).await.unwrap();

    // 同机 IP 节点没有自己的 ssh_port：期望状态照发（实验室曾因它解码失败、/desired 全部 500），节点清单里有它
    let (ipn, _) = seed_node(&t, r2, team, 3, "203.0.113.7", 20003).await;
    sqlx::query("update machine set kind = 'vps', ssh_port = null, host_id = $2 where id = $1").bind(ipn).bind(node).execute(&t.st.db).await.unwrap();
    let (s, v) = t.req("GET", "/agent/v1/desired?after=0", None, Some("tok2")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    serde_json::from_value::<DesiredState>(v).unwrap();
    let (s, v) = t.req("GET", "/agent/v1/rtt", None, Some("tok2")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(serde_json::from_value::<RttConfig>(v).unwrap().targets.iter().any(|x| x.node_id == ipn));
}

/// 矩阵里的往返是各小时 p50_ms 的中位数；每小时一轮清掉早于上一小时的 rtts，清空后迟到的上报只加轮数、不改 p50
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p50_is_median_of_hourly_p50_and_old_rtts_are_pruned() {
    let t = setup().await;
    let r1 = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, _) = seed_node(&t, r1, team, 1, "203.0.113.5", 20001).await;
    sqlx::query("update machine set kind = 'vps' where id = $1").bind(node).execute(&t.st.db).await.unwrap();
    // 当前小时往前 0..3 小时：p50 依次 80 / 70 / 60 / 50，每行都带 rtts
    for (ago, p50) in [(0, 80.0f32), (1, 70.0), (2, 60.0), (3, 50.0)] {
        sqlx::query(
            "insert into relay_node_rtt_hourly (relay_id, node_id, hour, samples, ok, p50_ms, max_ms, rtts)
             values ($1, $2, date_trunc('hour', now()) - make_interval(hours => $3), 10, 10, $4, $4 + 5, array[$4, $4 + 5])",
        )
        .bind(r1)
        .bind(node)
        .bind(ago)
        .bind(p50)
        .execute(&t.st.db)
        .await
        .unwrap();
    }
    let cell = |p: &serde_json::Value| p["nodes"][0]["cells"].as_array().unwrap().iter().find(|c| c["relay_id"] == json!(r1)).unwrap().clone();
    let c = cell(&t.get("/api/v1/placement?team=团队1").await);
    assert_eq!((c["samples"].clone(), c["p50_ms"].clone()), (json!(40), json!(65.0)), "{c}");

    // 早于上一小时的（2、3 小时前）清空 rtts，当前与上一小时留着
    fleet_server::placement::prune(&t.st.db).await.unwrap();
    let left: Vec<(i32, bool)> =
        sqlx::query_as("select (extract(epoch from date_trunc('hour', now()) - hour) / 3600)::int, cardinality(rtts) > 0 from relay_node_rtt_hourly order by hour desc")
            .fetch_all(&t.st.db)
            .await
            .unwrap();
    assert_eq!(left, vec![(0, true), (1, true), (2, false), (3, false)]);
    assert_eq!(cell(&t.get("/api/v1/placement?team=团队1").await)["p50_ms"], json!(65.0), "清 rtts 不改矩阵上的数");

    // 3 小时前那一小时迟到一份上报：轮数照加，p50 与 rtts 不动（否则会被只剩迟到的值重算）
    let ts = common::now_ms() / 3_600_000 * 3_600_000 - 3 * 3_600_000 + 60_000;
    let mut r = blank_report(1);
    r.xray = Some(xray(true));
    r.rtt = vec![RttSample { node_id: node, ts_ms: ts, ok: true, rtt_ms: Some(200.0) }];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    let row: (i32, i32, Option<f32>, i32) =
        sqlx::query_as("select samples, ok, p50_ms, cardinality(rtts) from relay_node_rtt_hourly where hour = date_trunc('hour', now()) - interval '3 hours'")
            .fetch_one(&t.st.db)
            .await
            .unwrap();
    assert_eq!(row, (11, 11, Some(50.0), 0));
}
