//! 探测点（probe_points/）：一次性安装命令与令牌、`/agent/v1/probe/*` 的鉴权、上报进 relay_reach、
//! 主探测来源决定心跳格与 relay_unreachable、主来源离线时退回中控（连不上照报、连得上不算恢复）或标灰、探测点离线告警、设置的约束、升级。
//! 不起后台循环，探测点离线告警由测试手动驱动。

use crate::common;

use axum::http::StatusCode;
use common::tele::*;
use fleet_server::probe_points;
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

/// 新建一个探测点（一次性安装命令），执行命令取到安装脚本，从脚本里拿出上报令牌。返回 (id, 令牌)
async fn add_probe(t: &T, name: &str) -> (Uuid, String) {
    let v = t.post("/api/v1/probe-points", json!({"name": name, "note": "阿里云香港"})).await;
    let id: Uuid = v["id"].as_str().unwrap().parse().unwrap();
    let cmd = v["bootstrap"]["cmd"].as_str().unwrap();
    let url = cmd.split_whitespace().find(|w| w.starts_with("http://fleet.test/api/v1/probe-bootstrap/")).unwrap();
    let path = url.trim_start_matches("http://fleet.test");
    let (st, script) = raw_get(t, path).await;
    assert_eq!(st, StatusCode::OK, "{script}");
    assert!(script.contains(&format!("NAME='{name}'")) && script.contains("FETCH='1'") && script.contains("SERVER_URL='http://fleet.test'"), "{script}");
    // 只能用一次
    let (again, body) = raw_get(t, path).await;
    assert_eq!(again, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("已经用过"), "{body}");
    let tok = script.lines().find_map(|l| l.strip_prefix("TOKEN='")).unwrap().trim_end_matches('\'').to_string();
    (id, tok)
}

async fn raw_get(t: &T, path: &str) -> (StatusCode, String) {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let res = t.app.clone().oneshot(axum::http::Request::get(path).body(Body::empty()).unwrap()).await.unwrap();
    let st = res.status();
    (st, String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

/// 探测点上报一轮。`ok`：每台线路机通不通（端口取目标里第一个）。同一个探测点两份至少隔 10 秒：上报前把最近上报往前挪
async fn report(t: &T, id: Uuid, tok: &str, results: &[(Uuid, u16, bool, Option<f32>)]) -> (StatusCode, Value) {
    sqlx::query("update probe_point set last_seen_at = last_seen_at - interval '20 seconds' where id = $1").bind(id).execute(&t.st.db).await.unwrap();
    let rs: Vec<Value> = results
        .iter()
        .map(|(r, port, ok, rtt)| json!({"relay_id": r, "port": port, "ok": ok, "rtt_ms": rtt, "detail": if *ok { Value::Null } else { json!("超时 10 秒") }}))
        .collect();
    t.req("POST", "/agent/v1/probe/report", Some(json!({"version": "0.1.0+test", "results": rs})), Some(tok)).await
}

fn reason_text(m: &Value, key: &str) -> Option<String> {
    m["health"]["reasons"].as_array().unwrap().iter().find(|r| r["key"] == key).map(|r| r["text"].as_str().unwrap().to_string())
}

async fn machine(t: &T, id: Uuid) -> Value {
    t.st.live.invalidate();
    t.get(&format!("/api/v1/machines/{id}")).await
}

/// 令牌：只能用在 /agent/v1/probe/*；机器的 agent 令牌用不了探测点的接口，探测点的令牌也上报不了机器数据；删除后作废
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_token_auth() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "agent-tok").await;
    let team = seed_team(&t, "团队1").await;
    seed_node(&t, relay, team, 1, "203.0.113.5", 20001).await;
    let (id, tok) = add_probe(&t, "香港").await;
    assert_eq!(t.req("GET", "/agent/v1/probe/targets", None, Some("nonsense")).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(t.req("GET", "/agent/v1/probe/targets", None, Some("agent-tok")).await.0, StatusCode::UNAUTHORIZED, "机器的令牌不是探测点的");
    assert_eq!(t.report("agent-tok", &blank_report(1)).await.0, StatusCode::OK);
    assert_eq!(t.report(&tok, &blank_report(1)).await.0, StatusCode::UNAUTHORIZED, "探测点的令牌报不了机器数据");
    let (st, v) = t.req("GET", "/agent/v1/probe/targets", None, Some(&tok)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["interval_s"], 30);
    assert_eq!(v["controls"], json!(["1.1.1.1:443", "8.8.8.8:443"]));
    assert_eq!(v["relays"], json!([{"relay_id": relay, "host": "relay.test", "ports": [20001]}]));
    assert!(v.get("upgrade").is_none());
    // 停用：拿到空目标，上报照收心跳、不写检查
    t.req("PATCH", &format!("/api/v1/probe-points/{id}"), Some(json!({"enabled": false})), None).await;
    assert_eq!(t.req("GET", "/agent/v1/probe/targets", None, Some(&tok)).await.1["relays"], json!([]));
    let (st, v) = report(&t, id, &tok, &[(relay, 20001, true, Some(160.0))]).await;
    assert_eq!((st, v["accepted"].clone()), (StatusCode::OK, json!(0)));
    // 同一个探测点 10 秒内第二份：429
    let (st, _) = t.req("POST", "/agent/v1/probe/report", Some(json!({"version": "x", "results": []})), Some(&tok)).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
    // 重新生成安装命令不作废现在的令牌（执行之后才换）；删除后作废，检查记录一起删
    t.req("PATCH", &format!("/api/v1/probe-points/{id}"), Some(json!({"enabled": true})), None).await;
    assert_eq!(report(&t, id, &tok, &[(relay, 20001, true, Some(160.0))]).await.1["accepted"], 1);
    let d = t.req("DELETE", &format!("/api/v1/probe-points/{id}"), None, None).await;
    assert_eq!(d.0, StatusCode::OK);
    assert!(d.1["data"]["cleanup"].as_str().unwrap().contains("rc-update del fleet-probe"), "{}", d.1);
    assert_eq!(t.req("GET", "/agent/v1/probe/targets", None, Some(&tok)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(t.count("select count(*) from relay_reach where probe_point_id is not null").await, 0);
}

/// 主来源是探测点：心跳格、入口表、relay_unreachable 都看它那一路（带延迟），摘要写明是它；中控那一路照记但不参与判断。
/// 主来源离线：中控开着就退回中控（连不上照报、连得上不算恢复），开 probe_point_offline；中控关着就标灰
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn primary_probe_drives_beats_alerts_and_fallback() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    seed_node(&t, relay, team, 1, "203.0.113.5", 20001).await;
    let (id, tok) = add_probe(&t, "香港").await;
    // 没设主来源之前：探测点的检查照收，判定与心跳格仍看中控（中控还没检查过：没有数据）
    assert_eq!(report(&t, id, &tok, &[(relay, 20001, false, None)]).await.1["accepted"], 1);
    assert!(t.get("/api/v1/probe-beats").await.get(relay.to_string()).is_none());
    let src = t.get("/api/v1/settings/reach-source").await;
    assert_eq!((src["primary"].clone(), src["effective"]["state"].clone()), (json!("central"), json!("central")));
    // 设成主来源
    let src = t.req("PUT", "/api/v1/settings/reach-source", Some(json!({"primary": id})), None).await;
    assert_eq!(src.0, StatusCode::OK, "{}", src.1);
    assert_eq!((src.1["data"]["effective"]["state"].clone(), src.1["data"]["effective"]["label"].clone()), (json!("probe"), json!("香港探测点")));
    let list = t.get("/api/v1/probe-points").await;
    assert_eq!((list[0]["primary"].clone(), list[0]["state"].clone(), list[0]["version"].clone()), (json!(true), json!("online"), json!("0.1.0+test")));
    // 中控那一路一直通：不影响判断
    sqlx::query("insert into relay_reach (relay_id, ts, ok, port) select $1, now() - make_interval(secs => g * 30), true, 20001 from generate_series(0, 3) g")
        .bind(relay)
        .execute(&t.st.db)
        .await
        .unwrap();
    for _ in 0..2 {
        report(&t, id, &tok, &[(relay, 20001, false, None)]).await;
    }
    let m = machine(&t, relay).await;
    assert_eq!(reason_text(&m, "relay_unreachable").as_deref(), Some("香港探测点连续 3 次连不上客户入口（端口 20001：超时 10 秒）"), "{m}");
    assert_eq!((m["entries"][0]["reach"]["ok"].clone(), m["entries"][0]["reach"]["fails"].clone()), (json!(false), json!(3)), "{m}");
    // 通了：恢复，心跳格带延迟中位数（只算通的）
    for rtt in [150.0, 170.0, 160.0] {
        report(&t, id, &tok, &[(relay, 20001, true, Some(rtt))]).await;
    }
    let m = machine(&t, relay).await;
    assert!(reason_text(&m, "relay_unreachable").is_none(), "{m}");
    let beats = t.get("/api/v1/probe-beats").await;
    let cells = beats[relay.to_string()].as_array().unwrap();
    let (n, ok): (i64, i64) = cells.iter().fold((0, 0), |a, b| (a.0 + b[0].as_i64().unwrap(), a.1 + b[1].as_i64().unwrap()));
    assert_eq!((n, ok), (6, 3), "只算探测点那一路：{beats}");
    assert!(cells.iter().any(|c| c[2].as_f64().is_some_and(|ms| (150.0..=170.0).contains(&ms))), "{beats}");
    let list = t.get("/api/v1/probe-points").await;
    let r = &list[0]["relays"][0];
    assert_eq!((r["name"].clone(), r["total"].clone(), r["ok"].clone(), r["avg_ms"].as_f64()), (json!("线路机1"), json!(6), json!(3), Some(160.0)), "{list}");

    // 再连不上 3 次：开着 relay_unreachable；然后探测点离线 → 退回中控（中控一直通）：原因消失但判不出（不算恢复）
    for _ in 0..3 {
        report(&t, id, &tok, &[(relay, 20001, false, None)]).await;
    }
    assert!(reason_text(&machine(&t, relay).await, "relay_unreachable").is_some());
    sqlx::query("update probe_point set last_seen_at = now() - interval '10 minutes' where id = $1").bind(id).execute(&t.st.db).await.unwrap();
    let src = t.get("/api/v1/settings/reach-source").await;
    assert_eq!((src["effective"]["state"].clone(), src["effective"]["label"].clone()), (json!("fallback"), json!("中控（代替离线的香港探测点）")), "{src}");
    t.st.live.invalidate();
    let views = fleet_server::views::live(&t.st).await.unwrap();
    let v = views.iter().find(|m| m.id == relay).unwrap();
    assert!(!v.health.reasons.iter().any(|r| r.key == "relay_unreachable"), "中控连得上");
    assert!(v.health.undecided("relay_unreachable"), "但中控连得上不能当成探测点那边恢复了");
    // 中控也连不上：照报，摘要写明是中控代替的
    sqlx::query("insert into relay_reach (relay_id, ts, ok, detail, port) select $1, now() + make_interval(secs => g), false, '端口 20001：连接被拒绝', 20001 from generate_series(1, 3) g")
        .bind(relay)
        .execute(&t.st.db)
        .await
        .unwrap();
    let m = machine(&t, relay).await;
    assert_eq!(reason_text(&m, "relay_unreachable").as_deref(), Some("中控（代替离线的香港探测点）连续 3 次连不上客户入口（端口 20001：连接被拒绝）"), "{m}");
    // 探测点离线告警：警告，对象是探测点（不是机器），摘要写明主来源的处境
    probe_points::check_once(&t.st, OffsetDateTime::now_utc()).await.unwrap();
    let inc = t.get("/api/v1/incidents?state=firing").await;
    let p = inc["items"].as_array().unwrap().iter().find(|i| i["rule"] == "probe_point_offline").cloned().unwrap_or_else(|| panic!("{inc}"));
    assert_eq!((p["severity"].clone(), p["subject"].clone(), p["subject_name"].clone(), p["rule_name"].clone()), (json!("warning"), Value::Null, json!("香港"), json!("探测点离线")));
    assert!(p["summary"].as_str().unwrap().starts_with("香港：10 分钟没有上报") && p["summary"].as_str().unwrap().contains("暂由中控代替"), "{p}");
    // 中控的检查关掉：标灰（判不出，不报）
    let r = t.req("PUT", "/api/v1/settings/reach-source", Some(json!({"central_enabled": false})), None).await;
    assert_eq!(r.1["data"]["effective"]["state"], "none", "{}", r.1);
    t.st.live.invalidate();
    let views = fleet_server::views::live(&t.st).await.unwrap();
    let v = views.iter().find(|m| m.id == relay).unwrap();
    assert!(!v.health.reasons.iter().any(|r| r.key == "relay_unreachable") && v.health.undecided("relay_unreachable"));
    assert!(t.get("/api/v1/probe-beats").await.get(relay.to_string()).is_none(), "标灰：线路机没有格子");
    // 探测点回来：告警恢复，又看它那一路
    sqlx::query("update probe_point set last_seen_at = now() - interval '20 seconds' where id = $1").bind(id).execute(&t.st.db).await.unwrap();
    report(&t, id, &tok, &[(relay, 20001, true, Some(150.0))]).await;
    probe_points::check_once(&t.st, OffsetDateTime::now_utc()).await.unwrap();
    assert_eq!(t.count("select count(*) from incident where rule = 'probe_point_offline' and state = 'firing'").await, 0);
    assert_eq!(t.get("/api/v1/settings/reach-source").await["effective"]["state"], "probe");
}

/// 设置的约束：主来源是中控时不能关中控的检查；主来源的探测点不能停用、删除；停用的不能设成主来源；名称校验
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reach_source_constraints() {
    let t = setup().await;
    let r = t.req("PUT", "/api/v1/settings/reach-source", Some(json!({"central_enabled": false})), None).await;
    assert_eq!(r.0, StatusCode::CONFLICT, "{}", r.1);
    let (id, _) = add_probe(&t, "香港").await;
    assert_eq!(t.req("POST", "/api/v1/probe-points", Some(json!({"name": "香港"})), None).await.0, StatusCode::CONFLICT, "同名");
    assert_eq!(t.req("POST", "/api/v1/probe-points", Some(json!({"name": "central"})), None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.req("PUT", "/api/v1/settings/reach-source", Some(json!({"primary": Uuid::now_v7()})), None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.req("PUT", "/api/v1/settings/reach-source", Some(json!({"primary": id, "central_enabled": false})), None).await.0, StatusCode::OK);
    assert_eq!(t.req("PATCH", &format!("/api/v1/probe-points/{id}"), Some(json!({"enabled": false})), None).await.0, StatusCode::CONFLICT);
    assert_eq!(t.req("DELETE", &format!("/api/v1/probe-points/{id}"), None, None).await.0, StatusCode::CONFLICT);
    // 中控的检查关着时不跑（reach::run_forever 每轮读设置）；改回中控要同时打开它
    assert_eq!(t.req("PUT", "/api/v1/settings/reach-source", Some(json!({"primary": "central"})), None).await.0, StatusCode::CONFLICT);
    let r = t.req("PUT", "/api/v1/settings/reach-source", Some(json!({"primary": "central", "central_enabled": true})), None).await;
    assert_eq!((r.0, r.1["data"]["effective"]["state"].clone()), (StatusCode::OK, json!("central")));
    assert_eq!(t.count("select count(*) from audit_log where action = 'settings.reach_source'").await, 2);
    // 改名、停用之后不能设成主来源
    t.req("PATCH", &format!("/api/v1/probe-points/{id}"), Some(json!({"name": "香港2", "enabled": false})), None).await;
    assert_eq!(t.req("PUT", "/api/v1/settings/reach-source", Some(json!({"primary": id})), None).await.0, StatusCode::CONFLICT);
    assert_eq!(t.get("/api/v1/probe-points").await[0]["state"], "disabled");
}

/// 升级：记下 dist 里程序的 sha256，探测点取目标时看到；报上来的程序 sha256 对上了就清掉。安装命令能下载 dist 里的程序
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgrade_via_targets() {
    let t = setup().await;
    let (id, tok) = add_probe(&t, "香港").await;
    assert_eq!(t.req("POST", &format!("/api/v1/probe-points/{id}/upgrade"), Some(json!({})), None).await.0, StatusCode::CONFLICT, "dist 里没有程序");
    common::dist_agent(&t, "0.1.0+new");
    let v = t.post(&format!("/api/v1/probe-points/{id}/upgrade"), json!({})).await;
    assert_eq!(v["version"], "0.1.0+new");
    let sha = v["sha256"].as_str().unwrap().to_string();
    let tg = t.req("GET", "/agent/v1/probe/targets", None, Some(&tok)).await.1;
    assert_eq!(tg["upgrade"], json!({"sha256": sha, "version": "0.1.0+new"}));
    // 程序能下载（带令牌）
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let res = t.app.clone().oneshot(axum::http::Request::get("/agent/v1/probe/binary").header("authorization", format!("Bearer {tok}")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).contains("fleet-agent 0.1.0+new"));
    assert_eq!(t.req("GET", "/agent/v1/probe/binary", None, None).await.0, StatusCode::UNAUTHORIZED);
    // 换好了：报上来的 sha256 对上，清掉
    t.req("POST", "/agent/v1/probe/report", Some(json!({"version": "0.1.0+new", "sha256": sha, "results": []})), Some(&tok)).await;
    assert!(t.req("GET", "/agent/v1/probe/targets", None, Some(&tok)).await.1.get("upgrade").is_none());
    assert_eq!(t.get("/api/v1/probe-points").await[0]["version"], "0.1.0+new");
}
