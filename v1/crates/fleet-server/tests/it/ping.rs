//! 三网 PING（ping.rs）：全局目标与每台的覆盖合成进期望状态（改了代数 +1）、上报入库、卡片的心跳格与当前读数、曲线、小时汇总、校验。

use crate::common;

use axum::http::StatusCode;
use common::tele::*;
use fleet_proto::{DesiredState, PingSample};
use serde_json::{json, Value};
use uuid::Uuid;

async fn desired(t: &T, tok: &str) -> DesiredState {
    let (s, v) = t.req("GET", "/agent/v1/desired?after=0", None, Some(tok)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    serde_json::from_value(v).unwrap()
}

async fn generation(t: &T, relay: Uuid) -> i64 {
    sqlx::query_scalar("select generation from relay where machine_id = $1").bind(relay).fetch_one(&t.st.db).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn targets_global_and_override_reach_desired_state() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    // 默认：三网（武汉电信 / 联通 / 移动），TCP；BGP 与 Node 1–4 留空不测
    let g = t.get("/api/v1/settings/ping-targets").await;
    assert_eq!((g["mode"].clone(), g["slots"].as_array().unwrap().len(), g["tcp_default_port"].clone()), (json!("tcp"), 8, json!(80)));
    let d = desired(&t, "tok1").await;
    let p = d.ping.clone().unwrap();
    assert_eq!((p.interval_s, p.count, p.mode.as_str()), (60, 5, "tcp"));
    assert_eq!(p.targets.iter().map(|x| (x.slot, x.name.as_str(), x.target.as_str())).collect::<Vec<_>>(), [
        (0, "武汉电信", "hb-ct-v4.ip.zstaticcdn.com"),
        (1, "武汉联通", "hb-cu-v4.ip.zstaticcdn.com"),
        (2, "武汉移动", "hb-cm-v4.ip.zstaticcdn.com")
    ]);
    // 改全局：在用线路机代数 +1，期望状态跟着变；审计一条
    let g0 = generation(&t, relay).await;
    let mut slots = g["slots"].as_array().unwrap().clone();
    slots[3] = json!({"name": "BGP", "target": "ip.zstaticcdn.com:443"});
    let r = t.req("PUT", "/api/v1/settings/ping-targets", Some(json!({"mode": "tcp", "slots": slots})), None).await;
    assert_eq!(r.0, StatusCode::OK, "{}", r.1);
    assert_eq!(generation(&t, relay).await, g0 + 1);
    assert_eq!(desired(&t, "tok1").await.ping.unwrap().targets.len(), 4);
    assert_eq!(t.count("select count(*) from audit_log where action = 'settings.ping_targets'").await, 1);
    // 校验：槽位数、目标、方式
    let mut bad = slots.clone();
    bad[0] = json!({"name": "x", "target": "a b"});
    assert_eq!(t.req("PUT", "/api/v1/settings/ping-targets", Some(json!({"mode": "tcp", "slots": bad})), None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.req("PUT", "/api/v1/settings/ping-targets", Some(json!({"mode": "udp", "slots": slots})), None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.req("PUT", "/api/v1/settings/ping-targets", Some(json!({"mode": "tcp", "slots": slots[..7]})), None).await.0, StatusCode::BAD_REQUEST);
    // 这台单独设置：ICMP，联通换成自定义目标，移动这台不测
    let o = json!({"override": {"mode": "icmp", "slots": [null, {"name": "联通 9929", "target": "203.0.113.9"}, {"name": "武汉移动", "target": ""}]}});
    let v = t.req("PUT", &format!("/api/v1/machines/{relay}/ping"), Some(o), None).await;
    assert_eq!(v.0, StatusCode::OK, "{}", v.1);
    let v = &v.1["data"];
    assert_eq!((v["mode"].clone(), v["slots"][1]["source"].clone(), v["slots"][0]["source"].clone()), (json!("icmp"), json!("override"), json!("global")));
    let p = desired(&t, "tok1").await.ping.unwrap();
    assert_eq!((p.mode.as_str(), p.targets.iter().map(|x| x.slot).collect::<Vec<_>>()), ("icmp", vec![0, 1, 3]));
    assert_eq!(p.targets[1].target, "203.0.113.9");
    // 清掉覆盖：全部跟随全局
    t.req("PUT", &format!("/api/v1/machines/{relay}/ping"), Some(json!({"override": null})), None).await;
    assert_eq!(desired(&t, "tok1").await.ping.unwrap().mode, "tcp");
    assert_eq!(t.count("select count(*) from relay where ping_override is not null").await, 0);
    // 节点没有三网 PING
    let team = seed_team(&t, "团队1").await;
    let (node, _) = seed_node(&t, relay, team, 1, "203.0.113.5", 20001).await;
    assert_eq!(t.req("GET", &format!("/api/v1/machines/{node}/ping"), None, None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_feed_beats_series_and_hourly() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let now = common::now_ms();
    let s = |slot: u8, ago_min: i64, lost: u16, rtt: Option<f32>, target: &str| PingSample {
        slot,
        ts_ms: now - ago_min * 60_000,
        target: target.into(),
        mode: "tcp".into(),
        sent: 5,
        lost,
        rtt_ms: rtt,
    };
    let ct = "hb-ct-v4.ip.zstaticcdn.com";
    let mut r = blank_report(1);
    r.ping = vec![s(0, 2, 0, Some(150.0), ct), s(0, 1, 1, Some(170.0), ct), s(0, 0, 0, Some(160.0), ct), s(1, 0, 5, None, "hb-cu-v4.ip.zstaticcdn.com"),
                  s(9, 0, 0, Some(1.0), ct), s(0, 60 * 24 * 8, 0, Some(1.0), ct)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    assert_eq!(t.count("select count(*) from ping_result").await, 4, "槽位不对的、超过 7 天的不收");
    // 重报同一时刻的不重复写
    let mut r2 = blank_report(2);
    r2.ping = vec![s(0, 0, 0, Some(160.0), ct)];
    t.report("tok1", &r2).await;
    assert_eq!(t.count("select count(*) from ping_result").await, 4);
    let b = t.get("/api/v1/ping-beats").await;
    let rows = b[relay.to_string()].as_array().unwrap_or_else(|| panic!("{b}"));
    assert_eq!(rows.len(), 3, "每个在测的目标一行：{b}");
    assert_eq!((rows[0]["name"].clone(), rows[0]["cur_ms"].clone(), rows[0]["cur_loss"].clone()), (json!("武汉电信"), json!(160.0), json!(0.0)));
    assert_eq!((rows[1]["cur_ms"].clone(), rows[1]["cur_loss"].clone()), (Value::Null, json!(100.0)), "全丢");
    assert!(rows[2]["cur_ms"].is_null() && rows[2]["cells"].as_array().unwrap().iter().all(|c| c == &json!([null, null])), "移动还没数据");
    let cells = rows[0]["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 20);
    let (sent_cells, any_loss): (usize, bool) = (cells.iter().filter(|c| !c[0].is_null()).count(), cells.iter().any(|c| c[1].as_f64().is_some_and(|l| l > 0.0)));
    assert!(sent_cells >= 1 && any_loss, "{b}");
    // 曲线：1h 原始点，7d 小时汇总
    let v = t.get(&format!("/api/v1/machines/{relay}/ping-series?range=1h")).await;
    assert_eq!((v["step_s"].clone(), v["series"][0]["points"].as_array().unwrap().len()), (json!(60), 3), "{v}");
    assert_eq!(v["series"][0]["points"][1]["loss_pct"], json!(20.0));
    assert!(fleet_server::ping::rollup(&t.st.db, 7).await.unwrap() >= 2);
    let v = t.get(&format!("/api/v1/machines/{relay}/ping-series?range=7d")).await;
    let pts = v["series"][0]["points"].as_array().unwrap();
    assert!(!pts.is_empty() && pts.iter().all(|p| p["rtt_ms"].as_f64().is_some()), "{v}");
    assert_eq!(t.req("GET", &format!("/api/v1/machines/{relay}/ping-series?range=2d"), None, None).await.0, StatusCode::BAD_REQUEST);
    // 改了目标之后旧目标的数据不算进新目标
    let g = t.get("/api/v1/settings/ping-targets").await;
    let mut slots = g["slots"].as_array().unwrap().clone();
    slots[0] = json!({"name": "武汉电信", "target": "1.1.1.1"});
    t.req("PUT", "/api/v1/settings/ping-targets", Some(json!({"mode": "tcp", "slots": slots})), None).await;
    let b = t.get("/api/v1/ping-beats").await;
    assert!(b[relay.to_string()][0]["cur_ms"].is_null(), "{b}");
    let m = t.get(&format!("/api/v1/machines/{relay}/ping")).await;
    assert_eq!(m["current"].as_array().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hourly_rollup_covers_late_backfill() {
    let t = setup().await;
    seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    // 断线 10 小时后补传：上报收 7 天以内的，每 5 分钟一轮只重算最近 7 小时，每小时一轮重算最近 3 天
    let mut r = blank_report(1);
    r.ping = vec![PingSample {
        slot: 0,
        ts_ms: common::now_ms() - 10 * 3_600_000,
        target: "hb-ct-v4.ip.zstaticcdn.com".into(),
        mode: "tcp".into(),
        sent: 5,
        lost: 0,
        rtt_ms: Some(150.0),
    }];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    assert_eq!(t.count("select count(*) from ping_result").await, 1);
    fleet_server::ping::rollup(&t.st.db, 7).await.unwrap();
    assert_eq!(t.count("select count(*) from ping_hourly").await, 0, "7 小时的窗口够不到");
    fleet_server::ping::rollup(&t.st.db, 72).await.unwrap();
    assert_eq!(t.count("select count(*) from ping_hourly").await, 1, "每小时一轮的 3 天窗口补得上");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn machine_view_reads_only_its_own_relay() {
    let t = setup().await;
    let r1 = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let r2 = seed_relay(&t, "线路机2", "198.51.100.2", "tok2").await;
    for (tok, rtt) in [("tok1", 150.0), ("tok2", 80.0)] {
        let mut r = blank_report(1);
        r.ping = vec![PingSample {
            slot: 0,
            ts_ms: common::now_ms(),
            target: "hb-ct-v4.ip.zstaticcdn.com".into(),
            mode: "tcp".into(),
            sent: 5,
            lost: 0,
            rtt_ms: Some(rtt),
        }];
        assert_eq!(t.report(tok, &r).await.0, StatusCode::OK);
    }
    for (relay, rtt) in [(r1, 150.0), (r2, 80.0)] {
        let m = t.get(&format!("/api/v1/machines/{relay}/ping")).await;
        assert_eq!(m["current"].as_array().unwrap().len(), 3, "{m}");
        assert_eq!(m["current"][0]["cur_ms"], json!(rtt), "{m}");
    }
    let b = t.get("/api/v1/ping-beats").await;
    assert_eq!((b[r1.to_string()][0]["cur_ms"].clone(), b[r2.to_string()][0]["cur_ms"].clone()), (json!(150.0), json!(80.0)), "{b}");
}
