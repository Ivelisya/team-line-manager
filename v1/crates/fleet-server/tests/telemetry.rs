//! 遥测入库 / 小时汇总 / 健康判定 / 告警 / 通知 / SSE / 保留期。
//! 需要 PostgreSQL：环境变量 FLEET_TEST_DB（默认 postgres://fleet:fleet@127.0.0.1/fleet_test）。会清空该库的 public schema。
//! 不起后台循环：告警评估、小时汇总都由测试手动驱动（告警评估的「当前时间」可以快进，不用真等去抖）。

mod common_tele;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common_tele::*;
use fleet_proto::ProbeKind;
use fleet_server::alerts::{self, AlertState};
use fleet_server::state::Event;
use fleet_server::db;
use futures::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

const H: i64 = 3_600_000;
const D: i64 = 86_400_000;

fn hour_floor(ms: i64) -> i64 { ms.div_euclid(H) * H }

/// 把告警相关表里的时间戳往回拨：汇总/通知的「刚刚」保护不挡测试
async fn age_dirty(t: &T) {
    sqlx::query("update rollup_dirty set touched_at = now() - interval '1 minute'").execute(&t.st.db).await.unwrap();
}

async fn pass(t: &T, s: &mut AlertState, base: OffsetDateTime, secs: i64) {
    alerts::evaluate_once(&t.st, s, base + time::Duration::seconds(secs)).await.unwrap();
}

async fn incidents(t: &T, state: &str) -> Vec<Value> { t.get(&format!("/api/v1/incidents?state={state}")).await.as_array().unwrap().clone() }

fn has_rule(v: &[Value], rule: &str) -> bool { v.iter().any(|i| i["rule"] == rule) }

// ── T1 / T3：入库 ───────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_drops_bad_timestamps_and_keeps_the_rest() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (_node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let now = now_ms();
    let old = now - 5 * D; // 在保留期内，但常规维护只建了昨天起的分区：要按需补建

    let mut r = blank_report(1);
    r.host = vec![
        host(now - 10_000, 10.0, 100, 100),
        host(0, 10.0, 1, 1),                  // 1970 年
        host(now - 30 * D, 10.0, 1, 1),       // 超出保留期
        host(now + 10 * D, 10.0, 1, 1),       // 远在未来
        host(old, 10.0, 50, 50),
    ];
    r.entries = vec![entry_sample(entry, now - 5_000, 10, 20), entry_sample(entry, 0, 1, 1)];
    r.probes = vec![probe(entry, now - 5_000, ProbeKind::Egress, true), probe(entry, now - 40 * D, ProbeKind::Tcp, true)];
    r.access = vec![access(entry, now - 5_000), access(entry, 0), access(entry, now - 30 * D)];
    let (s, v) = t.report("tok1", &r).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["accepted_seq"], 1);

    assert_eq!(t.count("select count(*) from host_metric").await, 2, "只剩窗口内的两条主机样本");
    assert_eq!(t.count("select count(*) from entry_traffic").await, 1);
    assert_eq!(t.count("select count(*) from probe_result").await, 1);
    assert_eq!(t.count("select count(*) from access_event").await, 1);
    // 补传的旧样本所在小时被标脏
    let dirty = t.count(&format!("select count(*) from rollup_dirty where extract(epoch from hour)::bigint * 1000 = {}", hour_floor(old))).await;
    assert_eq!(dirty, 1);

    // 再发一份全是坏样本的：照样 200，不能卡住这台机器的上报
    let mut r2 = blank_report(2);
    r2.host = vec![host(0, 1.0, 1, 1), host(now + 100 * D, 1.0, 1, 1)];
    let (s, v) = t.report("tok1", &r2).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["accepted_seq"], 2);
    assert_eq!(t.count("select count(*) from host_metric").await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_probe_series_covers_its_entries() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let mut r = blank_report(1);
    r.probes = vec![probe(entry, now_ms() - 5_000, ProbeKind::Tcp, true)];
    let (s, v) = t.report("tok1", &r).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    for id in [relay, node] {
        let v = t.get(&format!("/api/v1/machines/{id}/series?metric=probe&range=1h")).await;
        assert_eq!(v["points"].as_array().unwrap().len(), 1, "线路机与节点都能看到这条入口的探测：{v}");
        assert_eq!((v["points"][0]["tcp_n"].as_i64(), v["points"][0]["tcp_ok_n"].as_i64()), (Some(1), Some(1)));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollup_covers_backfilled_hours_and_counter_resets() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (_node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let h0 = hour_floor(now_ms() - 3 * D); // 3 天前的整点

    // 上一小时最后一个样本 + 本小时四个样本（第三个起计数器清零，模拟重启）；tx 恒为 rx 的 2 倍
    let mut r = blank_report(1);
    r.host = [(h0 - 10_000, 800u64), (h0 + 10_000, 1000), (h0 + 20_000, 1500), (h0 + 30_000, 200), (h0 + 40_000, 400)]
        .iter()
        .map(|(ts, rx)| host(*ts, 20.0, *rx, rx * 2))
        .collect();
    r.entries = vec![entry_sample(entry, h0 + 5_000, 100, 200), entry_sample(entry, h0 + 15_000, 50, 25)];
    r.probes = vec![probe(entry, h0 + 5_000, ProbeKind::Egress, true), probe(entry, h0 + 15_000, ProbeKind::Egress, false)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    // 刚标脏的（10 秒内）留到下一轮；这里把标记拨旧，模拟「已经过了一会儿」
    age_dirty(&t).await;
    db::rollup_hours(&t.st.db).await.unwrap();

    let (rx, txb): (i64, i64) = sqlx::query_as("select net_rx, net_tx from host_metric_hourly where machine_id = $1 and extract(epoch from hour)::bigint * 1000 = $2")
        .bind(relay)
        .bind(h0)
        .fetch_one(&t.st.db)
        .await
        .unwrap();
    // 1000-800 + 1500-1000 + 200（回绕：读数即增量）+ 400-200
    assert_eq!((rx, txb), (200 + 500 + 200 + 200, 2 * (200 + 500 + 200 + 200)));
    let (up, down): (i64, i64) = sqlx::query_as("select up_bytes, down_bytes from entry_traffic_hourly where entry_id = $1 and extract(epoch from hour)::bigint * 1000 = $2")
        .bind(entry)
        .bind(h0)
        .fetch_one(&t.st.db)
        .await
        .unwrap();
    assert_eq!((up, down), (150, 225));
    let (total, ok): (i32, i32) = sqlx::query_as("select total, ok from probe_hourly where entry_id = $1 and kind = 'egress' and extract(epoch from hour)::bigint * 1000 = $2")
        .bind(entry)
        .bind(h0)
        .fetch_one(&t.st.db)
        .await
        .unwrap();
    assert_eq!((total, ok), (2, 1));
    assert_eq!(t.count("select count(*) from rollup_dirty where hour <= now() - interval '2 days'").await, 0, "汇总完的脏标记已删除");

    // 又补传了同一小时的一个晚到样本：重新汇总后增量和流量都更新（幂等重算）
    let mut r2 = blank_report(2);
    r2.host = vec![host(h0 + 50_000, 20.0, 600, 1200)];
    r2.entries = vec![entry_sample(entry, h0 + 25_000, 10, 0)];
    assert_eq!(t.report("tok1", &r2).await.0, StatusCode::OK);
    assert_eq!(t.count("select count(*) from rollup_dirty").await, 1);
    age_dirty(&t).await;
    db::rollup_hours(&t.st.db).await.unwrap();
    let rx: i64 = sqlx::query_scalar("select net_rx from host_metric_hourly where machine_id = $1 and extract(epoch from hour)::bigint * 1000 = $2")
        .bind(relay)
        .bind(h0)
        .fetch_one(&t.st.db)
        .await
        .unwrap();
    assert_eq!(rx, 1100 + 200);
    let up: i64 = sqlx::query_scalar("select up_bytes from entry_traffic_hourly where entry_id = $1 and extract(epoch from hour)::bigint * 1000 = $2")
        .bind(entry)
        .bind(h0)
        .fetch_one(&t.st.db)
        .await
        .unwrap();
    assert_eq!(up, 160);
}

// ── T2 / T6 ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_report_still_refreshes_last_report_at() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let r = blank_report(5);
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    sqlx::query("update agent_state set last_report_at = now() - interval '10 minutes' where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    // 同一个 seq 重发、以及比它小的 seq：都确认 accepted_seq = 5，但要刷新最近上报时间
    for seq in [5u64, 3] {
        sqlx::query("update agent_state set last_report_at = now() - interval '10 minutes' where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();
        let (s, v) = t.report("tok1", &blank_report(seq)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["accepted_seq"], 5);
        let age: f64 = sqlx::query_scalar("select extract(epoch from now() - last_report_at)::float8 from agent_state where machine_id = $1").bind(relay).fetch_one(&t.st.db).await.unwrap();
        assert!(age < 5.0, "重复上报也算 agent 在线，实际 {age} 秒前");
    }
    let last: i64 = sqlx::query_scalar("select last_seq from agent_state where machine_id = $1").bind(relay).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(last, 5, "last_seq 不回退");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desired_long_poll_returns_when_generation_differs_even_if_smaller() {
    let t = setup().await;
    seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await; // generation = 1
    // agent 手里的代数（5）比库里的（1）还大，比如库被回滚：照样立即下发，而不是挂满 30 秒
    let r = tokio::time::timeout(Duration::from_secs(5), t.req("GET", "/agent/v1/desired?after=5", None, Some("tok1"))).await.expect("不该长轮询");
    assert_eq!(r.0, StatusCode::OK, "{}", r.1);
    assert_eq!(r.1["generation"], 1);
}

// ── T4 / T5：健康与告警 ─────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_xray_down_alert_lifecycle() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;

    // 对照：线路机 Xray 正常、探测失败 → 节点报出口/TCP 不通
    let mut r = blank_report(1);
    r.xray = Some(xray(true));
    r.probes = vec![probe(entry, now_ms() - 2_000, ProbeKind::Egress, false), probe(entry, now_ms() - 2_000, ProbeKind::Tcp, false)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    let (st, rs) = t.health(node).await;
    assert_eq!((st.as_str(), rs.as_slice()), ("down", ["egress_fail".to_string(), "tcp_fail".to_string()].as_slice()));
    assert_eq!(t.health(relay).await.1, Vec::<String>::new());

    // Xray 挂了：线路机报 relay_xray_down，节点不再重复报严重告警（按未知处理）
    let mut r = blank_report(2);
    r.xray = Some(xray(false));
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    assert_eq!(t.health(relay).await, ("down".to_string(), vec!["relay_xray_down".to_string()]));
    assert_eq!(t.health(node).await, ("unknown".to_string(), vec![]));

    let mut ev = t.st.events.subscribe();
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    pass(&t, &mut state, base, 0).await;
    assert!(incidents(&t, "firing").await.is_empty(), "去抖：刚出现不开告警");
    pass(&t, &mut state, base, 61).await;
    let inc = incidents(&t, "firing").await;
    assert_eq!(inc.len(), 1, "{inc:?}");
    assert_eq!((inc[0]["rule"].as_str(), inc[0]["severity"].as_str()), (Some("relay_xray_down"), Some("critical")));
    let mut n = 0;
    while let Ok(e) = ev.try_recv() {
        n += matches!(e, Event::Incidents) as i32;
    }
    assert_eq!(n, 1, "新建告警通知前端一次");
    // 告警持续触发：之后每个周期都不再发 Incidents 事件（原先 rows_affected 总是 1，每 30 秒刷一次）
    for secs in [91, 121, 151] {
        pass(&t, &mut state, base, secs).await;
    }
    assert!(ev.try_recv().is_err(), "没有变化就不该有事件");
    assert_eq!(incidents(&t, "firing").await.len(), 1, "同一条告警不重复开");

    // 恢复：Xray 起来后要连续两个干净周期才算恢复
    let mut r = blank_report(3);
    r.xray = Some(xray(true));
    r.probes = vec![probe(entry, now_ms(), ProbeKind::Egress, true), probe(entry, now_ms(), ProbeKind::Tcp, true)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    pass(&t, &mut state, base, 181).await;
    assert_eq!(incidents(&t, "firing").await.len(), 1, "第一个干净周期还不恢复");
    pass(&t, &mut state, base, 211).await;
    assert!(incidents(&t, "firing").await.is_empty());
    assert!(has_rule(&incidents(&t, "resolved").await, "relay_xray_down"));
    assert!(matches!(ev.try_recv(), Ok(Event::Incidents)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_renewal_and_global_maintenance() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, _entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    assert_eq!(t.report("tok1", &blank_report(1)).await.0, StatusCode::OK);
    sqlx::query("insert into subscription (machine_id, price_cents, due_date, remind_days) values ($1, 500, current_date + 3, 7)").bind(relay).execute(&t.st.db).await.unwrap();
    // 线路机也有续费提醒（原先写在节点分支里，线路机永远不报）
    assert_eq!(t.health(relay).await.1, ["renewal_due"]);
    let mut state = AlertState::default();
    alerts::evaluate_once(&t.st, &mut state, OffsetDateTime::now_utc()).await.unwrap();
    assert!(has_rule(&incidents(&t, "firing").await, "renewal_due"), "续费类告警不去抖，立即开");

    // 全局维护（不指定机器）：所有机器都显示维护中
    let f = time::format_description::well_known::Rfc3339;
    let now = OffsetDateTime::now_utc();
    t.post("/api/v1/silences", json!({
        "subject_id": null, "rule": null, "is_maintenance": true, "comment": "机房迁移",
        "starts_at": (now - time::Duration::minutes(1)).format(&f).unwrap(), "ends_at": (now + time::Duration::hours(1)).format(&f).unwrap(),
    })).await;
    assert_eq!(t.health(relay).await.0, "maintenance");
    assert_eq!(t.health(node).await.0, "maintenance");
    let m = t.get("/api/v1/machines").await;
    assert!(m.as_array().unwrap().iter().all(|x| x["health"]["status"] == "maintenance"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_lag_counts_from_generation_at() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    assert_eq!(t.report("tok1", &blank_report(1)).await.0, StatusCode::OK);
    let set = |gen_at: &'static str| {
        let t = &t;
        async move {
            sqlx::query(&format!("update relay set generation = 5, applied_generation = 4, applied_at = now() - interval '3 hours', generation_at = {gen_at} where machine_id = $1"))
                .bind(relay)
                .execute(&t.st.db)
                .await
                .unwrap();
            t.health(relay).await.1
        }
    };
    assert_eq!(set("now() - interval '30 seconds'").await, Vec::<String>::new(), "代数刚变，距上次应用多久都不报");
    assert_eq!(set("now() - interval '5 minutes'").await, ["relay_apply_lag"]);
    assert_eq!(set("null").await, ["relay_apply_lag"], "没有 generation_at 时退回 applied_at");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sustained_resource_alerts() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let now = now_ms();
    // 只有最近 1 分钟高 CPU：不报
    let mut r = blank_report(1);
    r.host = (0..6).map(|i| host(now - 50_000 + i * 10_000, 95.0, 1, 1)).collect();
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    assert_eq!(t.health(relay).await.1, Vec::<String>::new());
    // 补上更早的样本，覆盖近 5 分钟：持续高 CPU 才报；内存 40%、磁盘 30% 不报
    let mut r = blank_report(2);
    r.host = (0..24).map(|i| host(now - 290_000 + i * 10_000, 97.0, 1, 1)).collect();
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    let (st, rs) = t.health(relay).await;
    assert_eq!((st.as_str(), rs.as_slice()), ("degraded", ["host_cpu_high".to_string()].as_slice()));
    // 中间夹一个低样本：最低值没到阈值，不算持续
    let mut r = blank_report(3);
    r.host = vec![host(now - 100_000 + 500, 20.0, 1, 1)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    assert_eq!(t.health(relay).await.1, Vec::<String>::new());
}

// ── T11：通知渠道接口 ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_channel_crud_and_test_endpoint() {
    let t = setup().await;
    let mock = Mock::start().await;
    let tg = t.post("/api/v1/notify-channels", json!({"name": "运维群", "kind": "telegram", "config": {"bot_token": "123:SECRET-TOKEN", "chat_id": 12345}, "min_severity": "warning", "enabled": true})).await;
    let id = tg["id"].as_str().unwrap().to_string();

    // 列表不含明文，只有提示
    let (_, raw) = t.req("GET", "/api/v1/notify-channels", None, None).await;
    assert!(!raw.to_string().contains("SECRET-TOKEN"));
    let list = raw["data"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!((list[0]["kind"].as_str(), list[0]["config_hint"].as_str(), list[0]["min_severity"].as_str()), (Some("telegram"), Some("chat 12345"), Some("warning")));
    assert!(list[0].get("config").is_none() && list[0].get("config_enc").is_none());
    // 库里是密文
    let enc: Vec<u8> = sqlx::query_scalar("select config_enc from notify_channel").fetch_one(&t.st.db).await.unwrap();
    assert!(!String::from_utf8_lossy(&enc).contains("SECRET-TOKEN"));

    // 非法输入
    for body in [
        json!({"name": "x", "kind": "email", "config": {}}),
        json!({"name": "x", "kind": "telegram", "config": {"bot_token": "1:a"}}),
        json!({"name": "x", "kind": "webhook", "config": {"url": "ftp://x"}}),
        json!({"name": "x", "kind": "webhook", "config": {"url": "http://x.test"}, "min_severity": "fatal"}),
        json!({"name": " ", "kind": "webhook", "config": {"url": "http://x.test"}}),
    ] {
        let (s, v) = t.req("POST", "/api/v1/notify-channels", Some(body.clone()), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} → {v}");
    }

    // 改名、停用、调严重度；config 只给 chat_id：令牌沿用旧的
    let (s, v) = t.req("PATCH", &format!("/api/v1/notify-channels/{id}"), Some(json!({"name": "值班群", "enabled": false, "min_severity": "critical", "config": {"chat_id": "-100777", "bot_token": ""}})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let list = t.get("/api/v1/notify-channels").await;
    assert_eq!((list[0]["name"].as_str(), list[0]["enabled"].as_bool(), list[0]["min_severity"].as_str(), list[0]["config_hint"].as_str()), (Some("值班群"), Some(false), Some("critical"), Some("chat -100777")));
    let enc: Vec<u8> = sqlx::query_scalar("select config_enc from notify_channel").fetch_one(&t.st.db).await.unwrap();
    let cfg: Value = serde_json::from_str(&t.st.secrets.open_str(&enc).unwrap()).unwrap();
    assert_eq!(cfg, json!({"bot_token": "123:SECRET-TOKEN", "chat_id": "-100777"}));
    let (s, _) = t.req("PATCH", &format!("/api/v1/notify-channels/{}", Uuid::now_v7()), Some(json!({"name": "x"})), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // 测试接口：不可达地址返回 ok:false 而不是 500；可达返回 ok:true；对方拒绝（errcode≠0）也是 ok:false
    let dead = t.post("/api/v1/notify-channels", json!({"name": "死链接", "kind": "webhook", "config": {"url": "http://127.0.0.1:9/hook"}, "min_severity": "info", "enabled": true})).await["id"].as_str().unwrap().to_string();
    let (s, v) = t.req("POST", &format!("/api/v1/notify-channels/{dead}/test"), None, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["ok"], false);
    assert!(v["data"]["error"].as_str().is_some_and(|e| !e.is_empty()), "{v}");
    let good = t.post("/api/v1/notify-channels", json!({"name": "好 hook", "kind": "webhook", "config": {"url": mock.url("/hook/ok")}, "min_severity": "info", "enabled": false})).await["id"].as_str().unwrap().to_string();
    let r = t.post(&format!("/api/v1/notify-channels/{good}/test"), json!({})).await;
    assert_eq!((r["ok"].as_bool(), r["error"].is_null()), (Some(true), true), "停用的渠道也能测试：{r}");
    let b = &mock.bodies("/hook/ok")[0];
    assert_eq!(b["event"], "test");
    let wc = t.post("/api/v1/notify-channels", json!({"name": "企微拒绝", "kind": "wecom", "config": {"url": mock.url("/wecom/bad")}, "min_severity": "info", "enabled": true})).await["id"].as_str().unwrap().to_string();
    let r = t.post(&format!("/api/v1/notify-channels/{wc}/test"), json!({})).await;
    assert_eq!(r["ok"], false);
    assert!(r["error"].as_str().unwrap().contains("invalid webhook url"), "{r}");
    let bad = mock.bodies("/wecom/bad").remove(0);
    assert_eq!(bad["msgtype"], "markdown");
    let tgm = t.post("/api/v1/notify-channels", json!({"name": "TG 假服务", "kind": "telegram", "config": {"bot_token": "9:XYZ", "chat_id": "7", "api_base": mock.url("")}, "min_severity": "info", "enabled": true})).await["id"].as_str().unwrap().to_string();
    let r = t.post(&format!("/api/v1/notify-channels/{tgm}/test"), json!({})).await;
    assert_eq!(r["ok"], true, "{r}");
    let b = &mock.bodies("/bot9:XYZ/sendMessage")[0];
    assert_eq!((b["chat_id"].as_str(), b["text"].as_str().is_some_and(|x| x.contains("测试通知"))), (Some("7"), true));
    let (s, _) = t.req("POST", &format!("/api/v1/notify-channels/{}/test", Uuid::now_v7()), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // 发送记录
    let log = t.get("/api/v1/notify-log?limit=50").await;
    let log = log.as_array().unwrap();
    assert_eq!(log.len(), 4);
    assert!(log.iter().all(|l| l["kind"] == "test"));
    let dead_row = log.iter().find(|l| l["channel_name"] == "死链接").unwrap();
    assert_eq!(dead_row["ok"], false);
    assert!(dead_row["error"].is_string());
    assert!(log.iter().find(|l| l["channel_name"] == "好 hook").unwrap()["ok"].as_bool().unwrap());

    // 删除 + 审计（审计里没有令牌）
    let (s, _) = t.req("DELETE", &format!("/api/v1/notify-channels/{id}"), None, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = t.req("DELETE", &format!("/api/v1/notify-channels/{id}"), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let audit = t.get("/api/v1/audit?limit=100").await;
    let actions: Vec<&str> = audit.as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap()).collect();
    for a in ["notify.create", "notify.update", "notify.delete"] {
        assert!(actions.contains(&a), "审计缺 {a}：{actions:?}");
    }
    assert!(!audit.to_string().contains("SECRET-TOKEN"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alert_notifications_firing_repeat_silence_and_resolved() {
    let t = setup().await;
    let mock = Mock::start().await;
    let mk = |name: &str, kind: &str, config: Value, sev: &str, enabled: bool| json!({"name": name, "kind": kind, "config": config, "min_severity": sev, "enabled": enabled});
    t.post("/api/v1/notify-channels", mk("tg", "telegram", json!({"bot_token": "123:ABC", "chat_id": 42, "api_base": mock.url("")}), "info", true)).await;
    t.post("/api/v1/notify-channels", mk("wc", "wecom", json!({"url": mock.url("/wecom/send")}), "critical", true)).await;
    t.post("/api/v1/notify-channels", mk("wh", "webhook", json!({"url": mock.url("/hook/a")}), "warning", true)).await;
    t.post("/api/v1/notify-channels", mk("off", "webhook", json!({"url": mock.url("/disabled/x")}), "info", false)).await;
    t.post("/api/v1/notify-channels", mk("broken", "webhook", json!({"url": mock.url("/fail/x")}), "info", true)).await;

    // 线路机：Xray 挂了（严重）+ 配置 5 分钟没应用（警告）
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let mut r = blank_report(1);
    r.xray = Some(xray(false));
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    sqlx::query("update relay set generation = 3, applied_generation = 1, generation_at = now() - interval '5 minutes' where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();

    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 61).await;
    assert_eq!(incidents(&t, "firing").await.len(), 2);
    // 严重：tg、wc、wh、broken；警告：tg、wh、broken（wc 只收严重）；off 停用不发
    mock.wait("/bot123:ABC", 2).await;
    mock.wait("/wecom/send", 1).await;
    mock.wait("/hook/a", 2).await;
    mock.wait("/fail/x", 2).await;
    for _ in 0..100 {
        if t.count("select count(*) from notify_log").await == 7 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(t.count("select count(*) from notify_log").await, 7);
    assert_eq!((mock.count("/bot123:ABC"), mock.count("/wecom/send"), mock.count("/hook/a"), mock.count("/disabled")), (2, 1, 2, 0));
    let tg: Vec<String> = mock.bodies("/bot123:ABC").iter().map(|b| b["text"].as_str().unwrap().to_string()).collect();
    let crit = tg.iter().find(|x| x.contains("Xray 未运行")).expect("严重告警的 Telegram 消息");
    assert!(crit.contains("【严重】告警触发") && crit.contains("机器：线路机1") && crit.contains("规则：Xray 未运行") && crit.contains("（北京时间）"), "{crit}");
    assert_eq!(mock.bodies("/bot123:ABC")[0]["chat_id"], "42");
    let wc = &mock.bodies("/wecom/send")[0];
    assert_eq!(wc["msgtype"], "markdown");
    assert!(wc["markdown"]["content"].as_str().unwrap().contains("线路机1"));
    let wh = mock.bodies("/hook/a");
    let wh_crit = wh.iter().find(|b| b["rule"] == "relay_xray_down").unwrap();
    assert_eq!((wh_crit["event"].as_str(), wh_crit["severity"].as_str(), wh_crit["machine"].as_str(), wh_crit["rule_name"].as_str()), (Some("firing"), Some("critical"), Some("线路机1"), Some("Xray 未运行")));
    assert!(wh.iter().any(|b| b["rule"] == "relay_apply_lag" && b["rule_name"] == "配置未应用"));
    // 记录：坏渠道记失败，其余成功；一个渠道失败不影响别的
    let log = t.get("/api/v1/notify-log?limit=100").await;
    let log = log.as_array().unwrap();
    assert_eq!(log.iter().filter(|l| l["ok"] == false).count(), 2);
    assert!(log.iter().filter(|l| l["ok"] == false).all(|l| l["channel_name"] == "broken" && l["error"].as_str().is_some_and(|e| e.contains("500"))));
    assert!(log.iter().all(|l| l["incident_id"].is_string() && l["kind"] == "firing"));

    // 持续提醒：严重告警 4 小时后再提醒一次（警告不重复）；之后同一周期内不再发
    sqlx::query("update incident set notified_at = now() - interval '5 hours'").execute(&t.st.db).await.unwrap();
    pass(&t, &mut state, base, 91).await;
    mock.wait("/bot123:ABC", 3).await;
    mock.wait("/wecom/send", 2).await;
    mock.wait("/hook/a", 3).await;
    pass(&t, &mut state, base, 121).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!((mock.count("/bot123:ABC"), mock.count("/wecom/send"), mock.count("/hook/a")), (3, 2, 3), "提醒后计时重置");
    assert!(mock.bodies("/hook/a").iter().any(|b| b["event"] == "repeat" && b["rule"] == "relay_xray_down"));

    // 静默：被静默的规则不再提醒
    let f = time::format_description::well_known::Rfc3339;
    let now = OffsetDateTime::now_utc();
    let sil = t.post("/api/v1/silences", json!({"subject_id": relay, "rule": "relay_xray_down", "comment": "", "starts_at": (now - time::Duration::minutes(1)).format(&f).unwrap(), "ends_at": (now + time::Duration::hours(1)).format(&f).unwrap()})).await["id"].as_str().unwrap().to_string();
    sqlx::query("update incident set notified_at = now() - interval '5 hours'").execute(&t.st.db).await.unwrap();
    pass(&t, &mut state, base, 151).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!((mock.count("/bot123:ABC"), mock.count("/wecom/send")), (3, 2), "被静默的不发");
    let (s, _) = t.req("DELETE", &format!("/api/v1/silences/{sil}"), None, None).await;
    assert_eq!(s, StatusCode::OK);

    // 恢复：Xray 起来、配置也应用了；连续两个干净周期后各发一条「恢复」（只发给收到过触发的渠道）
    let mut r = blank_report(2);
    r.xray = Some(xray(true));
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    sqlx::query("update relay set applied_generation = 3, applied_at = now() where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    pass(&t, &mut state, base, 181).await;
    assert_eq!(incidents(&t, "firing").await.len(), 2);
    pass(&t, &mut state, base, 211).await;
    assert!(incidents(&t, "firing").await.is_empty());
    mock.wait("/wecom/send", 3).await;
    mock.wait("/bot123:ABC", 5).await;
    let last_wc = mock.bodies("/wecom/send").pop().unwrap();
    assert!(last_wc["markdown"]["content"].as_str().unwrap().contains("告警已恢复"), "{last_wc}");
    assert!(mock.bodies("/hook/a").iter().any(|b| b["event"] == "resolved" && b["rule"] == "relay_xray_down"));
}

// ── T7 / T8：SSE 与错误 ─────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_sends_resync_when_subscriber_lags() {
    let t = setup().await;
    let res = t.app.clone().oneshot(Request::get("/api/v1/stream").header("cookie", &t.cookie).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    // 订阅者还没读，广播就发了远超缓冲（256）的事件 → 它会落后
    for _ in 0..600 {
        t.st.emit(Event::Machines);
    }
    let mut body = res.into_body().into_data_stream();
    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = String::new();
        while let Some(Ok(b)) = body.next().await {
            buf.push_str(&String::from_utf8_lossy(&b));
            if buf.contains(r#"{"type":"resync"}"#) {
                return true;
            }
        }
        false
    })
    .await
    .expect("5 秒内应读到 resync");
    assert!(seen);
}

// ── T9 / T10：分区与保留期 ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_partitions_are_dropped_and_leftovers_cleaned() {
    let t = setup().await;
    let db = &t.st.db;
    // 一个过期的已挂载分区（带数据）和一个脱离父表的残留表
    sqlx::query("create table host_metric_20200101 partition of host_metric for values from ('2020-01-01') to ('2020-01-02')").execute(db).await.unwrap();
    sqlx::query("insert into host_metric (machine_id, ts) values ($1, '2020-01-01 03:00:00+00')").bind(Uuid::now_v7()).execute(db).await.unwrap();
    sqlx::query("create table probe_result_20200102 (like probe_result)").execute(db).await.unwrap();
    db::maintain_partitions(db, 14, 7, 2).await.unwrap();
    let gone: Vec<Option<String>> = sqlx::query_scalar("select to_regclass('host_metric_20200101')::text union all select to_regclass('probe_result_20200102')::text").fetch_all(db).await.unwrap();
    assert!(gone.iter().all(|g| g.is_none()), "{gone:?}");
    // 今天的分区还在，且连接池里的 lock_timeout 已复原
    let today: Option<String> = sqlx::query_scalar("select to_regclass('host_metric_' || to_char(now() at time zone 'utc', 'YYYYMMDD'))::text").fetch_one(db).await.unwrap();
    assert!(today.is_some());
    for _ in 0..3 {
        let lt: String = sqlx::query_scalar("show lock_timeout").fetch_one(db).await.unwrap();
        assert_eq!(lt, "0");
    }
    // 重复执行幂等
    db::maintain_partitions(db, 14, 7, 2).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_cleanup_removes_only_expired_rows() {
    let t = setup().await;
    let db = &t.st.db;
    let user: Uuid = sqlx::query_scalar("select id from app_user").fetch_one(db).await.unwrap();
    for (state, days) in [("resolved", 200), ("resolved", 10), ("firing", 400)] {
        sqlx::query(&format!(
            "insert into incident (id, rule, subject_id, severity, state, summary, opened_at, resolved_at) values ($1, 'agent_offline', $2, 'critical', '{state}', 's',
             now() - interval '{days} days', case when '{state}' = 'resolved' then now() - interval '{days} days' end)"
        ))
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .execute(db)
        .await
        .unwrap();
    }
    for exp in ["now() - interval '1 hour'", "now() + interval '1 day'"] {
        sqlx::query(&format!("insert into session (id_hash, user_id, expires_at) values ($1, $2, {exp})")).bind(Uuid::now_v7().as_bytes().to_vec()).bind(user).execute(db).await.unwrap();
    }
    for days in [2, 0] {
        sqlx::query(&format!("insert into login_failure (username, ts) values ('x', now() - interval '{days} days')")).execute(db).await.unwrap();
    }
    for days in [100, 1] {
        sqlx::query(&format!("insert into notify_log (ok, ts) values (true, now() - interval '{days} days')")).execute(db).await.unwrap();
    }
    for days in [400, 1] {
        sqlx::query(&format!("insert into audit_log (actor, action, ts) values ('a', 'x', now() - interval '{days} days')")).execute(db).await.unwrap();
    }
    for days in [100, 10] {
        sqlx::query(&format!("insert into silence (id, starts_at, ends_at, comment, created_by) values ($1, now() - interval '{days} days' - interval '1 hour', now() - interval '{days} days', 's', 'a')"))
            .bind(Uuid::now_v7())
            .execute(db)
            .await
            .unwrap();
    }
    let sessions_before = t.count("select count(*) from session").await;
    db::cleanup_retention(db).await.unwrap();
    assert_eq!(t.count("select count(*) from silence").await, 1, "结束超 90 天的静默删掉");
    assert_eq!(t.count("select count(*) from incident").await, 2, "只删已恢复超 180 天的");
    assert_eq!(t.count("select count(*) from incident where state = 'firing'").await, 1);
    assert_eq!(t.count("select count(*) from session").await, sessions_before - 1);
    assert_eq!(t.count("select count(*) from login_failure").await, 1);
    assert_eq!(t.count("select count(*) from notify_log").await, 1);
    assert_eq!(t.count("select count(*) from audit_log where actor = 'a'").await, 1);
}

// ── 访问汇总：agent 断网很久后补传的记录也要汇总进去 ──────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn access_rollup_covers_late_records() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let late = now_ms() - 5 * H; // 5 小时前：早已不在「最近两小时」里
    let late_hour = hour_floor(late);

    let mut r = blank_report(1);
    r.access = vec![access(entry, late)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    assert_eq!(t.count("select count(*) from access_event").await, 1);
    assert_eq!(t.count(&format!("select count(*) from access_dirty where extract(epoch from hour)::bigint * 1000 = {late_hour}")).await, 1, "迟到的访问记录所在小时被标脏");
    assert_eq!(t.count("select count(*) from rollup_dirty").await, 0, "访问记录不动遥测的脏标记");

    fleet_server::access::rollup_pending(&t.st.db).await.unwrap();
    let hourly = format!("select coalesce(sum(conns), 0)::bigint from access_hourly where node_id = '{node}' and extract(epoch from hour)::bigint * 1000 = {late_hour}");
    assert_eq!(t.count(&hourly).await, 1, "迟到的记录进了 access_hourly");
    assert_eq!(t.count(&format!("select coalesce(sum(conns), 0)::bigint from access_source_hourly where node_id = '{node}'")).await, 1);
    assert_eq!(t.count(&format!("select coalesce(sum(total), 0)::bigint from access_target_seen where node_id = '{node}'")).await, 1);
    assert_eq!(t.count(&format!("select count(*) from access_dirty where extract(epoch from hour)::bigint * 1000 = {late_hour}")).await, 0, "补算完删除脏标记");
    assert_eq!(t.count("select count(*) from access_dirty").await, 1, "当前小时留一个标记，下个小时第一轮收尾");

    // 同一小时又补传一条：再补算一次，总数 +1 而不是重复累加；来源连接数的差值记给来源画像
    let mut r2 = blank_report(2);
    r2.access = vec![access(entry, late + 1000)];
    assert_eq!(t.report("tok1", &r2).await.0, StatusCode::OK);
    fleet_server::access::rollup_pending(&t.st.db).await.unwrap();
    assert_eq!(t.count(&hourly).await, 2);
    assert_eq!(t.count(&format!("select coalesce(sum(total), 0)::bigint from access_target_seen where node_id = '{node}'")).await, 2);
    assert_eq!(t.count("select coalesce(sum(delta), 0)::bigint from access_source_late").await, 2);
    // 没有新数据时再跑一轮：什么都不变
    fleet_server::access::rollup_pending(&t.st.db).await.unwrap();
    assert_eq!(t.count(&hourly).await, 2);
    assert_eq!(t.count("select coalesce(sum(delta), 0)::bigint from access_source_late").await, 2);
}
