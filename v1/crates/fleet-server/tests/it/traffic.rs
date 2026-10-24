//! 每日流量 `GET /traffic/daily`（口径见 src/traffic.rs）。需要 PostgreSQL（见 common）。

use crate::common::{seed_node, seed_relay, seed_relay_b, seed_team, setup};
use axum::http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

/// 组织时区（默认北京时间）零点前后一小时分到昨天、今天；节点含换线前的旧入口、已退役的照列、直出单列；
/// 线路机的计费量按供应商规则（取小）逐天算；按机器过滤只留经过它的入口
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daily_split_by_org_midnight() {
    let t = setup(false).await;
    let db = &t.st.db;
    let (ra, _) = seed_relay(&t).await;
    let (rb, _) = seed_relay_b(&t).await;
    let pv = t.post("/api/v1/providers", json!({"name": "甲", "code": "A", "traffic_rule": "min"})).await["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    sqlx::query("update machine set provider_id = $2 where id = $1").bind(ra).bind(pv).execute(db).await.unwrap();
    let team = seed_team(&t, "团队1").await;
    let n1 = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let n2 = seed_node(&t, team, 2, "203.0.113.11", "retired").await;
    // 节点 1 从线路机2 换到线路机3（旧入口已移除）；节点 2 已退役；线路机2 的直出
    let entry = |relay: Uuid, node: Uuid, port: i32, lc: &'static str| {
        let id = Uuid::now_v7();
        async move {
            sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, $4, $5, 'ab', $6::entry_lifecycle)")
                .bind(id)
                .bind(relay)
                .bind(node)
                .bind(port)
                .bind(Uuid::now_v7())
                .bind(lc)
                .execute(db)
                .await
                .unwrap();
            id
        }
    };
    let e1 = entry(ra, n1, 20001, "removed").await;
    let e2 = entry(rb, n1, 20001, "active").await;
    let e3 = entry(ra, n2, 20002, "removed").await;
    let e4 = entry(ra, ra, 20003, "active").await;
    let (today, t0): (time::Date, time::OffsetDateTime) =
        sqlx::query_as("select (now() at time zone 'Asia/Shanghai')::date, (now() at time zone 'Asia/Shanghai')::date::timestamp at time zone 'Asia/Shanghai'")
            .fetch_one(db)
            .await
            .unwrap();
    let h = time::Duration::hours(1);
    for (e, at, up, down) in [(e1, t0 - h, 10i64, 100i64), (e2, t0, 20, 200), (e3, t0 - h, 1, 1000), (e4, t0, 3, 30)] {
        sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, $2, $3, $4)").bind(e).bind(at).bind(up).bind(down).execute(db).await.unwrap();
    }
    for (at, rx, tx) in [(t0 - h, 500i64, 700i64), (t0, 900, 400)] {
        sqlx::query("insert into host_metric_hourly (machine_id, hour, net_rx, net_tx) values ($1, $2, $3, $4)").bind(ra).bind(at).bind(rx).bind(tx).execute(db).await.unwrap();
    }
    t.st.live.rolled_up();

    let v = t.get("/api/v1/traffic/daily?days=7").await;
    let dates = v["dates"].as_array().unwrap();
    assert_eq!((dates.len(), dates[6].as_str()), (7, Some(today.to_string().as_str())), "{v}");
    let pair = |x: &Value| (x["down"].as_i64().unwrap(), x["up"].as_i64().unwrap());
    let last2 = |a: &Value| -> Vec<(i64, i64)> { a.as_array().unwrap()[5..].iter().map(pair).collect() };
    assert_eq!(last2(&v["total"]), [(1100, 11), (230, 23)], "{v}");
    let nodes = v["nodes"].as_array().unwrap();
    let names: Vec<&str> = nodes.iter().map(|n| n["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["团队1-02", "团队1-01", "线路机2-直出"], "按区间合计降序");
    assert_eq!((nodes[0]["lifecycle"].as_str(), pair(&nodes[0])), (Some("retired"), (1000, 1)));
    assert_eq!((pair(&nodes[1]), last2(&nodes[1]["daily"])), ((300, 30), vec![(100, 10), (200, 20)]), "换线前后都算节点 1");
    assert_eq!((nodes[2]["direct"].as_bool(), nodes[2]["id"].as_str()), (Some(true), Some(ra.to_string().as_str())));
    let relay = |v: &Value, id: Uuid| v["relays"].as_array().unwrap().iter().find(|r| r["id"] == id.to_string()).cloned();
    let a = relay(&v, ra).unwrap();
    let billed: Vec<i64> = a["daily"].as_array().unwrap()[5..].iter().map(|d| d["billed"].as_i64().unwrap()).collect();
    assert_eq!((a["rule"].as_str(), billed, a["billed"].as_i64()), (Some("min"), vec![500, 400], Some(900)), "{a}");
    assert_eq!(last2(&a["business"]), [(1100, 11), (30, 3)], "换线前的量留在旧线路机");
    assert_eq!(last2(&relay(&v, rb).unwrap()["business"]), [(0, 0), (200, 20)]);

    // 按节点过滤：只剩它的入口，经过的两台线路机都在；不存在的 404
    let v = t.get("/api/v1/traffic/daily?days=7&machine=%E5%9B%A2%E9%98%9F1-01").await;
    assert_eq!((last2(&v["total"]), v["nodes"].as_array().unwrap().len(), v["relays"].as_array().unwrap().len()), (vec![(100, 10), (200, 20)], 1, 2), "{v}");
    assert_eq!(t.req("GET", "/api/v1/traffic/daily?machine=nope", None, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(t.req("GET", "/api/v1/traffic/daily?days=93", None, None).await.0, StatusCode::BAD_REQUEST);
}
