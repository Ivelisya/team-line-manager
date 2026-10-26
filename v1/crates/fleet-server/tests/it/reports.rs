//! 月度可用率汇总与月报。需要 PostgreSQL（见 common）。

use crate::common;

use axum::http::StatusCode;
use common::tele::*;
use serde_json::{json, Value};
use uuid::Uuid;

/// 探测小时汇总：组织时区「今天」零点之后第 h 小时（数据全放在今天：不论哪天跑，本月、近 7 天的窗口都包得住）
async fn put_hour(t: &T, entry: Uuid, h: i32, kind: &str, total: i32, ok: i32) {
    sqlx::query(
        "insert into probe_hourly (entry_id, hour, kind, total, ok)
         values ($1, (local_date('Asia/Shanghai')::timestamp at time zone 'Asia/Shanghai') + make_interval(hours => $2), $3, $4, $5)",
    )
    .bind(entry)
    .bind(h)
    .bind(kind)
    .bind(total)
    .bind(ok)
    .execute(&t.st.db)
    .await
    .unwrap();
}

async fn provider(t: &T, name: &str, code: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into provider (id, name, code) values ($1, $2, $3)").bind(id).bind(name).bind(code).execute(&t.st.db).await.unwrap();
    id
}

/// 四个在用节点：01、02 属 DeerVPS，03 属 Other，04 没填供应商且没有探测。再放一个备用团队的节点（不该出现）；线路机本来就不列
async fn seed(t: &T) -> [Uuid; 4] {
    let relay = seed_relay(t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(t, "团队1").await;
    let (deer, other) = (provider(t, "DeerVPS", "DV").await, provider(t, "Other", "OT").await);
    let mut ids = vec![];
    let mut entries = vec![];
    for (n, ip, port) in [(1, "203.0.113.1", 20001), (2, "203.0.113.2", 20002), (3, "203.0.113.3", 20003), (4, "203.0.113.4", 20004)] {
        let (m, e) = seed_node(t, relay, team, n, ip, port).await;
        ids.push(m);
        entries.push(e);
    }
    for (m, p) in [(ids[0], deer), (ids[1], deer), (ids[2], other)] {
        sqlx::query("update machine set provider_id = $2 where id = $1").bind(m).bind(p).execute(&t.st.db).await.unwrap();
    }
    // 01：出口第 0、1 小时连着失败 4、3 次（断线一次，2 小时）、第 4 小时只失败 2 次（不到 3 次，不算断线）：出口 231/240；
    // TCP 第 0、1、4 小时都有失败：232/240（TCP 的失败不算断线）
    for (h, total, ok) in [(0, 60, 57), (1, 60, 57), (2, 60, 60), (4, 60, 58)] {
        put_hour(t, entries[0], h, "tcp", total, ok).await;
    }
    for (h, total, ok) in [(0, 60, 56), (1, 60, 57), (2, 60, 60), (4, 60, 58)] {
        put_hour(t, entries[0], h, "egress", total, ok).await;
    }
    // 02：出口全部成功，TCP 一个小时里失败了 10 次（出口通：不是断线）；03 出口、TCP 各一次失败
    put_hour(t, entries[1], 0, "egress", 60, 60).await;
    put_hour(t, entries[1], 0, "tcp", 60, 50).await;
    put_hour(t, entries[2], 3, "egress", 100, 99).await;
    put_hour(t, entries[2], 3, "tcp", 100, 99).await;
    // 备用团队（迁移 0041 建好的）的节点不算
    let spare: Uuid = sqlx::query_scalar("select id from team where spare").fetch_one(&t.st.db).await.unwrap();
    let (_, se) = seed_node(t, relay, spare, 1, "203.0.113.9", 20009).await;
    put_hour(t, se, 0, "tcp", 10, 0).await;
    put_hour(t, se, 0, "egress", 10, 0).await;
    [ids[0], ids[1], ids[2], ids[3]]
}

/// 可用率汇总的口径：同抽屉的 availability（成功次数 ÷ 探测次数），主指标是出口成功率；断线 = 出口失败 3 次以上的整点小时、连着的算一次
/// （TCP 的失败不算），节点和供应商都从差到好排（没有探测的在最后），备用团队与线路机不列。告警次数只数窗口内可用性相关规则、加上复发次数
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uptime_report_matches_availability_and_sorts_worst_first() {
    let t = setup().await;
    let [n1, n2, n3, _] = seed(&t).await;
    // 01 的告警：严重的 tcp_fail 开过一次、30 分钟内复发 2 次合并进去（算 3 次）；警告级的 tcp_fail（TCP 偶发失败、出口正常）不算；续费提醒不算；40 天前的不在窗口内
    for (rule, sev, reopen, ago, state) in [
        ("tcp_fail", "critical", 2, 0, "firing"),
        ("tcp_fail", "warning", 4, 1, "resolved"),
        ("renewal_due", "warning", 0, 0, "firing"),
        ("egress_fail", "warning", 0, 40, "firing"),
    ] {
        sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary, reopen_count, opened_at, state) values ($1, 'machine', $2, $3, $4::severity, 'x', $5, now() - make_interval(days => $6), $7::incident_state)")
            .bind(Uuid::now_v7())
            .bind(rule)
            .bind(n1)
            .bind(sev)
            .bind(reopen)
            .bind(ago)
            .bind(state)
            .execute(&t.st.db)
            .await
            .unwrap();
    }

    let v = t.get("/api/v1/reports/uptime?days=7").await;
    let names: Vec<&str> = v["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["团队1-01", "团队1-03", "团队1-02", "团队1-04"], "按出口成功率排：02 的 TCP 比 03 差，但出口全通，排在 03 之后：{v}");
    let a = &v["items"][0];
    assert_eq!((a["egress_total"].as_i64(), a["egress_ok"].as_i64(), a["tcp_total"].as_i64(), a["tcp_ok"].as_i64()), (Some(240), Some(231), Some(240), Some(232)), "{a}");
    assert_eq!((a["outages"].as_i64(), a["longest_outage_h"].as_i64(), a["incidents"].as_i64()), (Some(1), Some(2), Some(3)), "{a}");
    assert_eq!((v["items"][1]["outages"].as_i64(), v["items"][2]["outages"].as_i64()), (Some(0), Some(0)), "03 失败 1 次、02 只有 TCP 失败：都不算断线");
    assert_eq!((v["items"][2]["tcp_pct"].as_f64(), v["items"][2]["egress_pct"].as_f64()), (Some(100.0 * 50.0 / 60.0), Some(100.0)));
    assert!(v["items"][3]["egress_pct"].is_null() && v["items"][3]["provider"].is_null(), "没有探测：成功率为 null");
    // 同一节点的 availability 接口（抽屉、fleetctl uptime）给的成功率一样
    for (id, i) in [(n1, 0), (n2, 2), (n3, 1)] {
        let av = t.get(&format!("/api/v1/machines/{id}/availability?days=7")).await;
        assert_eq!((&av["tcp_pct"], &av["egress_pct"]), (&v["items"][i]["tcp_pct"], &v["items"][i]["egress_pct"]), "{id}");
    }
    // 供应商：平均 / 最差，从差到好；没填供应商的单列在最后
    let ps: Vec<(&str, Option<f64>, Option<&str>)> =
        v["providers"].as_array().unwrap().iter().map(|p| (p["name"].as_str().unwrap(), p["avg_egress_pct"].as_f64(), p["worst"]["name"].as_str())).collect();
    let deer = (100.0 * 231.0 / 240.0 + 100.0) / 2.0;
    assert_eq!(ps, [("DeerVPS", Some(deer), Some("团队1-01")), ("Other", Some(99.0), Some("团队1-03")), ("未填供应商", None, None)], "{v}");
    assert_eq!((v["providers"][0]["nodes"].as_i64(), v["providers"][0]["outages"].as_i64(), v["providers"][0]["incidents"].as_i64()), (Some(2), Some(1), Some(3)));

    // 默认（本月）与 month=本月 同一窗口；month 与 days 不能同时给，格式不对、将来的月份、days 越界都是 400
    let cur: String = sqlx::query_scalar("select to_char(now() at time zone 'Asia/Shanghai', 'YYYY-MM')").fetch_one(&t.st.db).await.unwrap();
    let m1 = t.get("/api/v1/reports/uptime").await;
    let m2 = t.get(&format!("/api/v1/reports/uptime?month={cur}")).await;
    assert_eq!((&m1["items"], &m1["month"]), (&m2["items"], &json!(cur)));
    for bad in [format!("month={cur}&days=3"), "month=2026-13".into(), "month=2999-01".into(), "days=91".into()] {
        assert_eq!(t.req("GET", &format!("/api/v1/reports/uptime?{bad}"), None, None).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
}

/// 月报只发给开了「月报」的渠道（不看最低级别），内容里有各项；没有这样的渠道回 409；发送写审计
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monthly_report_goes_only_to_channels_with_the_switch() {
    let t = setup().await;
    let mock = Mock::start().await;
    let [n1, ..] = seed(&t).await;
    // 01 本月用了 6 GB 流量、月费 6 美元：1 美元 / GB
    sqlx::query("insert into subscription (machine_id, price_cents, due_date) values ($1, 600, current_date + 60)").bind(n1).execute(&t.st.db).await.unwrap();
    sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) select id, date_trunc('hour', now()), 1000000000, 5000000000 from entry where node_id = $1")
        .bind(n1)
        .execute(&t.st.db)
        .await
        .unwrap();
    // 本月开过一条线路机到节点不通、30 分钟内复发 2 次合并进去：月报里算 3 次
    sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary, reopen_count) values ($1, 'machine', 'tcp_fail', $2, 'warning', 'x', 2)")
        .bind(Uuid::now_v7())
        .bind(n1)
        .execute(&t.st.db)
        .await
        .unwrap();
    let on = t
        .post("/api/v1/notify-channels", json!({"name": "月报群", "kind": "feishu", "config": {"url": mock.url("/hook/on")}, "min_severity": "critical", "enabled": true, "monthly_report": true}))
        .await;
    let off = t.post("/api/v1/notify-channels", json!({"name": "告警群", "kind": "feishu", "config": {"url": mock.url("/hook/off")}, "min_severity": "info", "enabled": true})).await;
    let list = t.get("/api/v1/notify-channels").await;
    let flag = |id: &Value| list.as_array().unwrap().iter().find(|c| c["id"] == *id).unwrap()["monthly_report"].clone();
    assert_eq!((flag(&on["id"]), flag(&off["id"])), (json!(true), json!(false)));

    let cur: String = sqlx::query_scalar("select to_char(now() at time zone 'Asia/Shanghai', 'YYYY-MM')").fetch_one(&t.st.db).await.unwrap();
    let r = t.post("/api/v1/reports/monthly/send", json!({"month": cur})).await;
    assert_eq!(r["channels"], 1, "{r}");
    mock.wait("/hook/on", 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(mock.count("/hook/off"), 0, "没开月报的渠道不发（哪怕它的级别更低）");
    let text = &mock.texts("/hook/on")[0];
    for want in [
        "【月报】",
        "总流量：6.00 GB",
        "月费合计：$6.00",
        "团队1-01 $1.000/GB（月费 $6.00，流量 6.0 GB）",
        "可用率最差的 3 台",
        "可用率最差的 3 台（出口探测成功率）",
        "团队1-01 96.25%（TCP 96.67%）",
        "断线 1 次、最长 2 小时",
        "告警次数前 3 的规则",
        "线路机到节点不通 3 次",
        "即将到期",
    ] {
        assert!(text.contains(want), "缺 {want}：{text}");
    }
    let au = t.get("/api/v1/audit?cat=config").await;
    assert_eq!((&au[0]["title"], au[0]["summary"].as_str().unwrap().contains("1 个渠道")), (&json!("发送月报"), true), "{au}");
    let log = t.get("/api/v1/notify-log").await;
    assert_eq!((&log[0]["kind"], &log[0]["subject_name"]), (&json!("report"), &json!("月报")), "{log}");

    // 关掉开关：没有可发的渠道，409
    t.req("PATCH", &format!("/api/v1/notify-channels/{}", on["id"].as_str().unwrap()), Some(json!({"monthly_report": false})), None).await;
    let (s, v) = t.req("POST", "/api/v1/reports/monthly/send", Some(json!({})), None).await;
    assert!(s == StatusCode::CONFLICT && v.to_string().contains("没有开启「月报」的通知渠道"), "{s} {v}");
}

/// 同机 IP 节点的流量同时算到宿主机上：钱在宿主机订阅上，客户流量走 IP 节点，月报「每 GB 最贵」里宿主机按 6 GB 算（1 美元 / GB），
/// 而不是 0 GB（达不到最低流量门槛，直接不上榜）；IP 节点自己没订阅，不上榜
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monthly_costly_counts_ip_node_traffic_on_host() {
    let t = setup().await;
    let [host, ipn, ..] = seed(&t).await;
    sqlx::query("update machine set kind = 'vps' where id = $1").bind(host).execute(&t.st.db).await.unwrap();
    sqlx::query("update machine set kind = 'vps', ssh_port = null, host_id = $2 where id = $1").bind(ipn).bind(host).execute(&t.st.db).await.unwrap();
    sqlx::query("insert into subscription (machine_id, price_cents, due_date) values ($1, 600, current_date + 60)").bind(host).execute(&t.st.db).await.unwrap();
    sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) select id, date_trunc('hour', now()), 1000000000, 5000000000 from entry where node_id = $1")
        .bind(ipn)
        .execute(&t.st.db)
        .await
        .unwrap();
    t.st.live.rolled_up();
    let cur: String = sqlx::query_scalar("select to_char(now() at time zone 'Asia/Shanghai', 'YYYY-MM')").fetch_one(&t.st.db).await.unwrap();
    let r = t.get(&format!("/api/v1/reports/monthly?month={cur}")).await;
    let costly = r["costly"].as_array().unwrap();
    assert_eq!(costly.len(), 1, "{r}");
    let c = &costly[0];
    assert_eq!(c["id"], host.to_string(), "{r}");
    assert!((c["gb"].as_f64().unwrap() - 6.0).abs() < 1e-6 && (c["usd_per_gb"].as_f64().unwrap() - 1.0).abs() < 1e-6, "{c}");
}

/// 本月退役的同机 IP 节点：窗口里它的流量还在，照样算到宿主机上（宿主机对应表同流量页，含已退役）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monthly_costly_counts_retired_ip_node_traffic_on_host() {
    let t = setup().await;
    let [host, ipn, ..] = seed(&t).await;
    sqlx::query("update machine set kind = 'vps' where id = $1").bind(host).execute(&t.st.db).await.unwrap();
    sqlx::query("update machine set kind = 'vps', ssh_port = null, host_id = $2 where id = $1").bind(ipn).bind(host).execute(&t.st.db).await.unwrap();
    sqlx::query("insert into subscription (machine_id, price_cents, due_date) values ($1, 600, current_date + 60)").bind(host).execute(&t.st.db).await.unwrap();
    sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) select id, date_trunc('hour', now()), 1000000000, 5000000000 from entry where node_id = $1")
        .bind(ipn)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("update machine set lifecycle = 'retired', retired_at = now() where id = $1").bind(ipn).execute(&t.st.db).await.unwrap();
    t.st.live.rolled_up();
    let cur: String = sqlx::query_scalar("select to_char(now() at time zone 'Asia/Shanghai', 'YYYY-MM')").fetch_one(&t.st.db).await.unwrap();
    let r = t.get(&format!("/api/v1/reports/monthly?month={cur}")).await;
    let costly = r["costly"].as_array().unwrap();
    assert_eq!(costly.len(), 1, "{r}");
    assert_eq!(costly[0]["id"], host.to_string(), "{r}");
    assert!((costly[0]["gb"].as_f64().unwrap() - 6.0).abs() < 1e-6, "{}", costly[0]);
}
