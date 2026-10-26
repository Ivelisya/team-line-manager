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
    // 草稿的节点没有流量，不列
    seed_node(&t, team, 3, "203.0.113.12", "draft").await;
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
    assert_eq!((a["rule"].as_str(), billed, a["billed"].as_i64()), (Some("min"), vec![500, 400], Some(1100)), "每天取小再相加是 900；区间合计先求和（下行 1400、上行 1100）再取小是 1100
{a}");
    assert_eq!(last2(&a["business"]), [(1100, 11), (30, 3)], "换线前的量留在旧线路机");
    assert_eq!(last2(&relay(&v, rb).unwrap()["business"]), [(0, 0), (200, 20)]);

    // 按节点过滤：只剩它的入口，经过的两台线路机都在；不存在的 404
    let v = t.get("/api/v1/traffic/daily?days=7&machine=%E5%9B%A2%E9%98%9F1-01").await;
    assert_eq!((last2(&v["total"]), v["nodes"].as_array().unwrap().len(), v["relays"].as_array().unwrap().len()), (vec![(100, 10), (200, 20)], 1, 2), "{v}");
    assert_eq!(t.req("GET", "/api/v1/traffic/daily?machine=nope", None, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(t.req("GET", "/api/v1/traffic/daily?days=93", None, None).await.0, StatusCode::BAD_REQUEST);
}

/// 订阅可单独设置流量计费方式：同一供应商（取大）下一台单独设双向（相加）、一台跟随；
/// 把单独设置写回与供应商相同的值就改回跟随（overrides 不再含 traffic_rule）；坏值 400
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscription_traffic_rule_override() {
    let t = setup(false).await;
    let db = &t.st.db;
    let (ra, _) = seed_relay(&t).await;
    let (rb, _) = seed_relay_b(&t).await;
    let pv = t.post("/api/v1/providers", json!({"name": "甲", "code": "A", "traffic_rule": "max"})).await["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    for r in [ra, rb] {
        sqlx::query("update machine set provider_id = $2 where id = $1").bind(r).bind(pv).execute(db).await.unwrap();
        sqlx::query("insert into host_metric_hourly (machine_id, hour, net_rx, net_tx) values ($1, date_trunc('hour', now()), 700, 300)").bind(r).execute(db).await.unwrap();
    }
    let put = |id: Uuid, rule: Value| {
        let t = &t;
        async move { t.req("PUT", &format!("/api/v1/machines/{id}/subscription"), Some(json!({"due_date": "2030-01-01", "price_cents": 100, "traffic_rule": rule})), None).await }
    };
    let view = |id: Uuid| {
        let t = &t;
        async move {
            t.st.live.rolled_up();
            let m = t.get(&format!("/api/v1/machines/{id}")).await;
            let has = m["subscription"]["overrides"].as_array().unwrap().iter().any(|o| o == "traffic_rule");
            (m["quota"]["used"].as_i64(), m["quota"]["rule"].as_str().map(String::from), m["subscription"]["traffic_rule"].as_str().map(String::from), has)
        }
    };
    assert_eq!(put(ra, json!("sum")).await.0, StatusCode::OK);
    assert_eq!(put(rb, Value::Null).await.0, StatusCode::OK);
    assert_eq!(view(ra).await, (Some(1000), Some("sum".into()), Some("sum".into()), true), "单独设双向：入 + 出");
    assert_eq!(view(rb).await, (Some(700), Some("max".into()), Some("max".into()), false), "跟随供应商：取大");
    // 单向只算下行 / 上行
    for (rule, used) in [("in", 700), ("out", 300)] {
        assert_eq!(put(ra, json!(rule)).await.0, StatusCode::OK);
        assert_eq!(view(ra).await, (Some(used), Some(rule.into()), Some(rule.into()), true));
    }
    // 不传 traffic_rule（旧 fleetctl / 旧网页改别的项）= 保留单独设置；显式 null 才改回跟随
    let (s, _) = t.req("PUT", &format!("/api/v1/machines/{ra}/subscription"), Some(json!({"due_date": "2030-01-01", "price_cents": 200})), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view(ra).await, (Some(300), Some("out".into()), Some("out".into()), true), "没传保留");
    // 写回与供应商相同的值 = 跟随
    assert_eq!(put(ra, json!("max")).await.0, StatusCode::OK);
    assert_eq!(view(ra).await, (Some(700), Some("max".into()), Some("max".into()), false), "与供应商相同按跟随存");
    // 改供应商规则，跟随的立即变；坏值 400
    t.req("PATCH", &format!("/api/v1/providers/{pv}"), Some(json!({"traffic_rule": "sum"})), None).await;
    assert_eq!(view(ra).await, (Some(1000), Some("sum".into()), Some("sum".into()), false));
    assert_eq!(put(ra, json!("avg")).await.0, StatusCode::BAD_REQUEST);
}

/// VPS 节点有自己的 agent，额度按网卡 + 生效的计费方式算：每日流量的节点行也带网卡读数与计费量（区间合计先求和再套规则，同线路机行）；
/// 家宽代理（没有自己的网卡）这些字段是 null
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daily_vps_node_nic_billed() {
    let t = setup(false).await;
    let db = &t.st.db;
    let pv = t.post("/api/v1/providers", json!({"name": "甲", "code": "A", "traffic_rule": "min"})).await["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let team = seed_team(&t, "团队1").await;
    let n1 = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    // 家宽代理没有自己的网卡：库里即使有读数也不算
    let n2 = seed_node(&t, team, 2, "203.0.113.11", "active").await;
    sqlx::query("update machine set kind = 'vps', provider_id = $2 where id = $1").bind(n1).bind(pv).execute(db).await.unwrap();
    let t0: time::OffsetDateTime =
        sqlx::query_scalar("select (now() at time zone 'Asia/Shanghai')::date::timestamp at time zone 'Asia/Shanghai'").fetch_one(db).await.unwrap();
    let h = time::Duration::hours(1);
    for (at, rx, tx) in [(t0 - h, 500i64, 700i64), (t0, 900, 400)] {
        for n in [n1, n2] {
            sqlx::query("insert into host_metric_hourly (machine_id, hour, net_rx, net_tx) values ($1, $2, $3, $4)").bind(n).bind(at).bind(rx).bind(tx).execute(db).await.unwrap();
        }
    }
    t.st.live.rolled_up();

    let v = t.get("/api/v1/traffic/daily?days=7").await;
    let nodes = v["nodes"].as_array().unwrap();
    let node = |name: &str| nodes.iter().find(|n| n["name"] == name).unwrap_or_else(|| panic!("{v}"));
    let a = node("团队1-01");
    let billed: Vec<i64> = a["nic_daily"].as_array().unwrap()[5..].iter().map(|d| d["billed"].as_i64().unwrap()).collect();
    assert_eq!(a["nic_daily"].as_array().unwrap().len(), 7, "与 dates 等长");
    assert_eq!(
        (a["rule"].as_str(), billed, a["rx"].as_i64(), a["tx"].as_i64(), a["billed"].as_i64()),
        (Some("min"), vec![500, 400], Some(1400), Some(1100), Some(1100)),
        "每天取小再相加是 900；区间合计先求和再取小是 1100\n{a}"
    );
    let b = node("团队1-02");
    assert!([&b["rule"], &b["rx"], &b["tx"], &b["billed"], &b["nic_daily"]].iter().all(|x| x.is_null()), "家宽代理没有网卡，字段是 null：{b}");
}

/// 成本：节点月费（订阅价折 USD）、USD/GB（月费 ÷ 按区间日均折成的 30 天流量）、闲置（上架满 7 天且近 7 天日均 < 0.5 GB；不满 7 天不判）；
/// 团队合计；待付清单同口径（近 30 天）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cost_per_gb_and_idle() {
    let t = setup(false).await;
    let db = &t.st.db;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let mut ids = vec![];
    for (n, price, old, today_down) in [(1, Some(3000), true, 1_000_000_000i64), (2, Some(3000), true, 4_000_000_000), (3, None, false, 0)] {
        let id = seed_node(&t, team, n, &format!("203.0.113.{}", 10 + n), "active").await;
        if old {
            sqlx::query("update machine set activated_at = now() - interval '10 days' where id = $1").bind(id).execute(db).await.unwrap();
        }
        if let Some(p) = price {
            sqlx::query("insert into subscription (machine_id, price_cents, currency, due_date) values ($1, $2, 'USD', current_date + 5)").bind(id).bind(p).execute(db).await.unwrap();
        }
        if today_down > 0 {
            let e = Uuid::now_v7();
            sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, $4, $5, 'ab', 'active')").bind(e).bind(relay).bind(id).bind(20000 + n).bind(Uuid::now_v7()).execute(db).await.unwrap();
            sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, (now() at time zone 'Asia/Shanghai')::date::timestamp at time zone 'Asia/Shanghai', 0, $2)").bind(e).bind(today_down).execute(db).await.unwrap();
        }
        ids.push(id);
    }
    t.st.live.rolled_up();

    let v = t.get("/api/v1/traffic/daily?days=7").await;
    assert_eq!((v["idle"]["days"].as_i64(), v["idle"]["gb_per_day"].as_f64()), (Some(7), Some(0.5)));
    let node = |id: Uuid| v["nodes"].as_array().unwrap().iter().find(|n| n["id"] == id.to_string()).cloned().unwrap();
    let near = |x: &Value, want: f64| (x.as_f64().unwrap() - want).abs() < 1e-6;
    // 1 GB / 7 天 → 30 天 4.2857 GB，月费 30 USD → 7 USD/GB；4 GB 日均 0.57 GB 不闲置
    let (a, b, c) = (node(ids[0]), node(ids[1]), node(ids[2]));
    assert_eq!((a["monthly_usd_cents"].as_i64(), a["idle"].as_bool()), (Some(3000), Some(true)), "{a}");
    assert!(near(&a["usd_per_gb"], 7.0) && near(&b["usd_per_gb"], 1.75), "{a} {b}");
    assert_eq!(b["idle"], false, "{b}");
    assert_eq!((c["monthly_usd_cents"].is_null(), c["usd_per_gb"].is_null(), c["idle"].as_bool()), (true, true, Some(false)), "没价格显示 —；上架不满 7 天不判闲置：{c}");
    // 团队：月费 60 USD，近 30 天 5 GB → 12 USD/GB；有 1 台闲置、1 台没价格
    let tm = &v["teams"][0];
    assert_eq!((tm["name"].as_str(), tm["nodes"].as_i64(), tm["idle"].as_i64(), tm["monthly_usd_cents"].as_i64(), tm["unpriced"].as_i64(), tm["bytes_30d"].as_i64()), (Some("团队1"), Some(3), Some(1), Some(6000), Some(1), Some(5_000_000_000)), "{tm}");
    assert!(near(&tm["usd_per_gb"], 12.0), "{tm}");
    // 待付清单同口径，近 30 天：1 GB → 30 USD/GB
    let d = t.get("/api/v1/billing/due?days=30").await;
    let m = d["groups"][0]["machines"].as_array().unwrap().iter().find(|m| m["id"] == ids[0].to_string()).cloned().unwrap();
    assert!(near(&m["usd_per_gb"], 30.0) && m["idle"] == true && m["avail_pct"].is_null(), "{m}");
}

/// 同机 IP 节点的流量同时算到宿主机上（成本 / 闲置口径）：宿主机自己只有 1 GB、IP 节点 4 GB（没订阅），
/// 宿主机按 5 GB 算每 GB 成本与闲置（近 7 天日均 0.71 GB，不闲置；只算自己的 1 GB 会被当成闲置）；
/// 节点行的下载 / 上传、团队的近 30 天流量仍只算各自入口（不重复）；团队里没订阅的 IP 节点不算「没价格」；待付清单同口径
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ip_node_traffic_counts_on_host_for_cost_and_idle() {
    let t = setup(false).await;
    let db = &t.st.db;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let mut ids = vec![];
    for (n, down) in [(1, 1_000_000_000i64), (2, 4_000_000_000)] {
        let id = seed_node(&t, team, n, &format!("203.0.113.{}", 10 + n), "active").await;
        sqlx::query("update machine set kind = 'vps', activated_at = now() - interval '10 days' where id = $1").bind(id).execute(db).await.unwrap();
        let e = Uuid::now_v7();
        sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, $4, $5, 'ab', 'active')").bind(e).bind(relay).bind(id).bind(20000 + n).bind(Uuid::now_v7()).execute(db).await.unwrap();
        sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, (now() at time zone 'Asia/Shanghai')::date::timestamp at time zone 'Asia/Shanghai', 0, $2)").bind(e).bind(down).execute(db).await.unwrap();
        ids.push(id);
    }
    let (host, ipn) = (ids[0], ids[1]);
    sqlx::query("update machine set ssh_port = null, host_id = $2 where id = $1").bind(ipn).bind(host).execute(db).await.unwrap();
    sqlx::query("insert into subscription (machine_id, price_cents, currency, due_date) values ($1, 3000, 'USD', current_date + 5)").bind(host).execute(db).await.unwrap();
    t.st.live.rolled_up();

    let v = t.get("/api/v1/traffic/daily?days=7").await;
    let node = |id: Uuid| v["nodes"].as_array().unwrap().iter().find(|n| n["id"] == id.to_string()).cloned().unwrap();
    let near = |x: &Value, want: f64| (x.as_f64().unwrap() - want).abs() < 1e-6;
    let (h, i) = (node(host), node(ipn));
    // 宿主机：行里仍是自己的 1 GB；每 GB 成本按 5 GB 算（5 GB / 7 天 → 30 天 21.43 GB，30 USD → 1.4 USD/GB）；日均 0.71 GB 不闲置
    assert_eq!(h["down"].as_i64(), Some(1_000_000_000), "{h}");
    assert!(near(&h["usd_per_gb"], 1.4), "{h}");
    assert_eq!((h["idle"].as_bool(), h["avg7"].as_i64()), (Some(false), Some(5_000_000_000 / 7)), "{h}");
    // IP 节点：没订阅、没价格，自己的 4 GB 不变
    assert_eq!((i["down"].as_i64(), i["monthly_usd_cents"].is_null(), i["usd_per_gb"].is_null()), (Some(4_000_000_000), true, true), "{i}");
    // 团队：2 台在用，没有闲置；没订阅的 IP 节点不算「没价格」；近 30 天流量 5 GB（不重复算）
    let tm = &v["teams"][0];
    assert_eq!((tm["nodes"].as_i64(), tm["idle"].as_i64(), tm["monthly_usd_cents"].as_i64(), tm["unpriced"].as_i64(), tm["bytes_30d"].as_i64()), (Some(2), Some(0), Some(3000), Some(0), Some(5_000_000_000)), "{tm}");
    assert!(near(&tm["usd_per_gb"], 6.0), "{tm}");
    // 待付清单（近 30 天）：宿主机 5 GB → 6 USD/GB，不是闲置
    let d = t.get("/api/v1/billing/due?days=30").await;
    let m = d["groups"][0]["machines"].as_array().unwrap().iter().find(|m| m["id"] == host.to_string()).cloned().unwrap();
    assert!(near(&m["usd_per_gb"], 6.0) && m["idle"] == false && m["bytes_30d"].as_i64() == Some(5_000_000_000), "{m}");

    // 没有 IP 节点时（解除挂靠）宿主机按自己的 1 GB 算：闲置
    sqlx::query("update machine set host_id = null, ssh_port = 22 where id = $1").bind(ipn).execute(db).await.unwrap();
    t.st.live.rolled_up();
    let v = t.get("/api/v1/traffic/daily?days=7").await;
    let h = v["nodes"].as_array().unwrap().iter().find(|n| n["id"] == host.to_string()).cloned().unwrap();
    assert_eq!(h["idle"].as_bool(), Some(true), "{h}");
}
