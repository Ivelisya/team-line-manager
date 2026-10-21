//! 遥测入库 / 小时汇总 / 健康判定 / 告警 / 通知 / 实时流 / 保留期。
//! 需要 PostgreSQL（见 common）。
//! 不起后台循环：告警评估、小时汇总、实时指标比对都由测试手动驱动（告警评估的「当前时间」可以快进，不用真等去抖）。

mod common;
mod common_tele;

use axum::http::StatusCode;
use common_tele::*;
use fleet_proto::ProbeKind;
use fleet_server::alerts::{self, AlertState};
use fleet_server::state::Event;
use fleet_server::{db, live};
use serde_json::{json, Value};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

const H: i64 = 3_600_000;
const D: i64 = 86_400_000;

fn hour_floor(ms: i64) -> i64 { ms.div_euclid(H) * H }

/// 评估一轮。告警用的在用机器视图有 3 s 短缓存，测试里上报完马上评估，先作废
async fn pass(t: &T, s: &mut AlertState, base: OffsetDateTime, secs: i64) {
    t.st.live.invalidate();
    alerts::evaluate_once(&t.st, s, base + time::Duration::seconds(secs)).await.unwrap();
}

async fn incidents(t: &T, state: &str) -> Vec<Value> { t.get(&format!("/api/v1/incidents?state={state}")).await["items"].as_array().unwrap().clone() }

fn has_rule(v: &[Value], rule: &str) -> bool { v.iter().any(|i| i["rule"] == rule) }

// ── T1 / T3：入库 ───────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_drops_bad_timestamps_and_keeps_the_rest() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (_node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let now = now_ms();
    let old = now - 2 * D; // 在保留期（默认 3 天）内，但常规维护只建了昨天起的分区：要按需补建

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

/// 入库失败按类回：数据错误 422（agent 移入 dead/），基础设施错误 503（agent 继续重试）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_ingest_errors_are_classified() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (_node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let mut r = blank_report(1);
    let mut a = access(entry, now_ms() - 5_000);
    a.dest_host = "bad\0host".into();
    r.access = vec![a];
    let (s, v) = t.report("tok1", &r).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("无法入库"), "{v}");

    t.st.db.close().await;
    let (s, v) = t.report("tok1", &blank_report(2)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
}

/// 连接池用尽：等 5 秒拿不到连接就回 503（agent 退避重试），不按 sqlx 默认的 30 秒干等（测试池的等待时间与 db::connect 相同）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_gets_503_when_pool_exhausted() {
    let t = setup().await;
    seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let mut held = vec![];
    while held.len() < t.st.db.options().get_max_connections() as usize {
        held.push(t.st.db.acquire().await.unwrap());
    }
    let t0 = std::time::Instant::now();
    let (s, v) = t.report("tok1", &blank_report(1)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
    drop(held);
    assert_eq!(t.report("tok1", &blank_report(1)).await.0, StatusCode::OK);
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
    let h0 = hour_floor(now_ms() - 2 * D); // 2 天前的整点（原始数据默认保留 3 天）

    // 上一小时最后一个样本 + 本小时四个样本（第三个起计数器清零，模拟重启）；tx 恒为 rx 的 2 倍
    let mut r = blank_report(1);
    r.host = [(h0 - 10_000, 800u64), (h0 + 10_000, 1000), (h0 + 20_000, 1500), (h0 + 30_000, 200), (h0 + 40_000, 400)]
        .iter()
        .map(|(ts, rx)| host(*ts, 20.0, *rx, rx * 2))
        .collect();
    r.entries = vec![entry_sample(entry, h0 + 5_000, 100, 200), entry_sample(entry, h0 + 15_000, 50, 25)];
    r.probes = vec![probe(entry, h0 + 5_000, ProbeKind::Egress, true), probe(entry, h0 + 15_000, ProbeKind::Egress, false)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
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
    assert_eq!(t.count(&format!("select count(*) from rollup_dirty where extract(epoch from hour)::bigint * 1000 = {h0}")).await, 1);
    assert_eq!(t.count("select count(*) from rollup_dirty").await, 2, "另一个是当前小时的标记，下个小时第一轮收尾");
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

    // 整点前 15 分钟内没有样本（agent 离线跨过整点）：本小时第一个样本没有前值，缺口那段的增量不记进这一小时
    let hb = h0 - 2 * H;
    let mut r3 = blank_report(3);
    r3.host = vec![host(hb - 20 * 60_000, 20.0, 100, 200), host(hb + 10_000, 20.0, 300, 600), host(hb + 20_000, 20.0, 400, 800)];
    assert_eq!(t.report("tok1", &r3).await.0, StatusCode::OK);
    db::rollup_hours(&t.st.db).await.unwrap();
    let rx: i64 = sqlx::query_scalar("select net_rx from host_metric_hourly where machine_id = $1 and extract(epoch from hour)::bigint * 1000 = $2")
        .bind(relay)
        .bind(hb)
        .fetch_one(&t.st.db)
        .await
        .unwrap();
    assert_eq!(rx, 400 - 300, "20 分钟前的样本不参与本小时第一个增量");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overview_series_sums_minute_rates() {
    let t = setup().await;
    seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    // 只给结束满 20 s 的分钟：cur 是这些分钟之后的第一个分钟起点
    let cur = (now_ms() - 20_000).div_euclid(60_000) * 60_000;
    // 三个分钟各一条：第二分钟计数器回绕（那一段不计），第三分钟 60 s 涨 60 000 字节 = 8 000 bit/s
    let mut r = blank_report(1);
    r.host = vec![host(cur - 175_000, 1.0, 50_000, 100_000), host(cur - 115_000, 1.0, 1_000, 2_000), host(cur - 55_000, 1.0, 61_000, 122_000)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    let v = t.get("/api/v1/overview/series?minutes=5").await;
    let pts = v["points"].as_array().unwrap();
    assert_eq!(pts.len(), 5, "{v}");
    let at = |ms: i64| {
        let ts = OffsetDateTime::from_unix_timestamp(ms / 1000).unwrap().format(&time::format_description::well_known::Rfc3339).unwrap();
        pts.iter().find(|p| p["ts"] == ts.as_str()).unwrap_or_else(|| panic!("缺 {ts}：{v}")).clone()
    };
    assert_eq!((at(cur - 60_000)["net_rx_bps"].as_f64(), at(cur - 60_000)["net_tx_bps"].as_f64()), (Some(8_000.0), Some(16_000.0)));
    assert_eq!(at(cur - 120_000)["net_rx_bps"].as_f64(), Some(0.0), "计数器回绕的那一分钟不计");
    assert!(pts.windows(2).all(|w| w[0]["ts"].as_str() < w[1]["ts"].as_str()), "旧→新");
    assert_eq!(pts[4]["ts"], at(cur - 60_000)["ts"], "最后一点是结束满 20 s 的最近一分钟：{v}");
    for bad in ["0", "61", "x"] {
        assert_eq!(t.req("GET", &format!("/api/v1/overview/series?minutes={bad}"), None, None).await.0, StatusCode::BAD_REQUEST);
    }
    assert_eq!(t.get("/api/v1/overview/series").await["points"].as_array().unwrap().len(), 30);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_beats_bucket_tcp_probes_per_machine() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let (_node2, _) = seed_node(&t, relay, team, 2, "203.0.113.6", 20001).await;
    // 格子对齐墙钟 3 分钟：cb 是当前这格的起点，最早一格从 cb - 57 分钟开始
    let cb = now_ms().div_euclid(180_000) * 180_000;
    let mut r = blank_report(1);
    r.probes = vec![
        probe(entry, cb, ProbeKind::Tcp, true),
        probe(entry, cb - 1_000, ProbeKind::Tcp, true),
        probe(entry, cb - 2_000, ProbeKind::Tcp, true),
        probe(entry, cb - 4 * 60_000, ProbeKind::Tcp, false),
        probe(entry, cb, ProbeKind::Egress, true),
        probe(entry, cb - 57 * 60_000, ProbeKind::Tcp, true),
        probe(entry, cb - 57 * 60_000 - 1_000, ProbeKind::Tcp, true),
    ];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    let v = t.get("/api/v1/probe-beats").await;
    assert_eq!(v.as_object().unwrap().len(), 2, "只有有探测的机器（线路机汇总它的入口）：{v}");
    for id in [relay, node] {
        let b = v[id.to_string()].as_array().unwrap();
        assert_eq!(b.len(), 20);
        assert_eq!(b[19], json!([1, 1, 10.0]), "当前这格（没走完）：{v}");
        assert_eq!(b[18], json!([2, 2, 10.0]), "上一格：{v}");
        assert_eq!(b[17], json!([1, 0, null]), "cb 前 4 分钟落在倒数第三格：{v}");
        assert_eq!(b[0], json!([1, 1, 10.0]), "最早一格从 cb - 57 分钟起，再早的不算：{v}");
        assert!(b[1..17].iter().all(|x| *x == json!([0, 0, null])), "{v}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql_display_name_matches_rust() {
    let t = setup().await;
    let cases = [
        ("relay", Some("线路机2"), None, None),
        ("vps", None, Some("团队1"), Some(3)),
        ("vps", None, Some("团队1"), Some(120)),
        ("proxy", Some("备用"), None, None),
        ("vps", Some("旧名"), Some("团队1"), Some(4)),
        ("vps", None, None, None),
    ];
    for (k, l, tm, n) in cases {
        let s: String = sqlx::query_scalar("select display_name($1, $2, $3, $4)").bind(k).bind(l).bind(tm).bind(n).fetch_one(&t.st.db).await.unwrap();
        assert_eq!(s, fleet_server::names::display_name(k, l, tm, n));
    }
    let none: Option<String> = sqlx::query_scalar("select display_name(null, null, null, null)").fetch_one(&t.st.db).await.unwrap();
    assert!(none.is_none(), "没连上机器（kind 为空）给 null");
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

/// 令牌最近一次使用距今多少秒（没用过为 None）
async fn token_used_ago(t: &T, machine: Uuid) -> Option<f64> {
    sqlx::query_scalar("select extract(epoch from now() - last_used_at)::float8 from agent_token where machine_id = $1").bind(machine).fetch_one(&t.st.db).await.unwrap()
}

/// agent 鉴权同一条语句里记下令牌最近使用时间（一分钟内不重复写）；撤销的令牌 401，也不再刷新
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_auth_records_token_use() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    assert_eq!(token_used_ago(&t, relay).await, None);
    assert_eq!(t.report("tok1", &blank_report(1)).await.0, StatusCode::OK);
    assert!(token_used_ago(&t, relay).await.is_some_and(|s| s < 5.0));
    let used = |ago: &'static str| {
        let db = t.st.db.clone();
        async move {
            sqlx::query(&format!("update agent_token set last_used_at = now() - interval '{ago}' where machine_id = $1")).bind(relay).execute(&db).await.unwrap();
        }
    };
    used("30 seconds").await;
    assert_eq!(t.report("tok1", &blank_report(2)).await.0, StatusCode::OK);
    assert!(token_used_ago(&t, relay).await.is_some_and(|s| s >= 29.0), "一分钟内不重复写");
    used("2 minutes").await;
    assert_eq!(t.report("tok1", &blank_report(3)).await.0, StatusCode::OK);
    assert!(token_used_ago(&t, relay).await.is_some_and(|s| s < 5.0));
    used("2 minutes").await;
    sqlx::query("update agent_token set revoked_at = now() where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    assert_eq!(t.report("tok1", &blank_report(4)).await.0, StatusCode::UNAUTHORIZED);
    assert!(token_used_ago(&t, relay).await.is_some_and(|s| s >= 119.0), "撤销的令牌不刷新");
}

/// agent 自报的健康数据出现在机器视图的 agent.health；之后不带它的上报不把它冲掉
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_health_is_shown_in_machine_view() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    assert!(t.get(&format!("/api/v1/machines/{relay}")).await["agent"]["health"].is_null(), "没报过为 null");
    let mut r = blank_report(1);
    r.agent_health = Some(fleet_proto::AgentHealth {
        queue_len: 3,
        queue_dead: 1,
        access_rotation_gaps: 2,
        last_apply_error: Some("xray 配置校验失败".into()),
        ..Default::default()
    });
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    assert_eq!(t.report("tok1", &blank_report(2)).await.0, StatusCode::OK);
    t.st.live.invalidate();
    let m = t.get(&format!("/api/v1/machines/{relay}")).await;
    let h = &m["agent"]["health"];
    assert_eq!(
        (h["queue_len"].as_u64(), h["queue_dead"].as_u64(), h["access_rotation_gaps"].as_u64(), h["access_truncations"].as_u64()),
        (Some(3), Some(1), Some(2), Some(0)),
        "{m}"
    );
    assert_eq!(h["last_apply_error"], "xray 配置校验失败");
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
async fn probe_alerts_debounce_longer() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (_node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let mut r = blank_report(1);
    r.xray = Some(xray(true));
    r.probes = vec![probe(entry, now_ms() - 2_000, ProbeKind::Egress, false), probe(entry, now_ms() - 2_000, ProbeKind::Tcp, false)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    let rules = |v: Vec<Value>| {
        let mut r: Vec<String> = v.iter().map(|i| i["rule"].as_str().unwrap().to_string()).collect();
        r.sort();
        r
    };

    // 开：TCP 不通满 90 s、出口不通满 150 s 才开（其余规则 60 s）
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 61).await;
    assert!(incidents(&t, "firing").await.is_empty(), "探测类 60 s 还不开");
    pass(&t, &mut state, base, 91).await;
    assert_eq!(rules(incidents(&t, "firing").await), ["tcp_fail"]);
    pass(&t, &mut state, base, 151).await;
    assert_eq!(rules(incidents(&t, "firing").await), ["egress_fail", "tcp_fail"]);

    // 关：探测恢复后同样按各自时长去抖
    let mut r = blank_report(2);
    r.xray = Some(xray(true));
    r.probes = vec![probe(entry, now_ms(), ProbeKind::Egress, true), probe(entry, now_ms(), ProbeKind::Tcp, true)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    pass(&t, &mut state, base, 200).await;
    pass(&t, &mut state, base, 261).await;
    assert_eq!(rules(incidents(&t, "firing").await), ["egress_fail", "tcp_fail"], "消失 60 s 还不恢复");
    pass(&t, &mut state, base, 291).await;
    assert_eq!(rules(incidents(&t, "firing").await), ["egress_fail"]);
    pass(&t, &mut state, base, 351).await;
    assert!(incidents(&t, "firing").await.is_empty());
}

/// 探测结果过期（线路机 agent 离线、探测停了）：节点按未知处理、不给出口 / TCP 原因，但这两条告警不当成恢复；
/// 有了新探测再判，去抖从那时起算
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_probes_do_not_resolve_probe_alerts() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let mut r = blank_report(1);
    r.xray = Some(xray(true));
    r.probes = vec![probe(entry, now_ms() - 2_000, ProbeKind::Egress, false), probe(entry, now_ms() - 2_000, ProbeKind::Tcp, false)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 151).await;
    assert_eq!(incidents(&t, "firing").await.len(), 2);

    // 15 分钟内没有探测结果
    sqlx::query("delete from probe_result where entry_id = $1").bind(entry).execute(&t.st.db).await.unwrap();
    assert_eq!(t.health(node).await, ("unknown".to_string(), vec![]));
    for secs in [181, 400, 1200] {
        pass(&t, &mut state, base, secs).await;
    }
    assert_eq!(incidents(&t, "firing").await.len(), 2, "探测过期不是恢复");

    let mut r = blank_report(2);
    r.xray = Some(xray(true));
    r.probes = vec![probe(entry, now_ms(), ProbeKind::Egress, true), probe(entry, now_ms(), ProbeKind::Tcp, true)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    pass(&t, &mut state, base, 1230).await;
    assert_eq!(incidents(&t, "firing").await.len(), 2, "去抖从有新探测时起算");
    pass(&t, &mut state, base, 1400).await;
    assert!(incidents(&t, "firing").await.is_empty());
}

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

    // 恢复：Xray 起来后原因要连续消失满 60 秒才算恢复（和开告警的去抖对称）
    let mut r = blank_report(3);
    r.xray = Some(xray(true));
    r.probes = vec![probe(entry, now_ms(), ProbeKind::Egress, true), probe(entry, now_ms(), ProbeKind::Tcp, true)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    pass(&t, &mut state, base, 181).await;
    pass(&t, &mut state, base, 211).await;
    assert_eq!(incidents(&t, "firing").await.len(), 1, "消失不满 60 秒还不恢复");
    pass(&t, &mut state, base, 241).await;
    assert!(incidents(&t, "firing").await.is_empty());
    assert!(has_rule(&incidents(&t, "resolved").await, "relay_xray_down"));
    assert!(matches!(ev.try_recv(), Ok(Event::Incidents)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retiring_machine_alerts_are_left_to_retire_flow() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    xray_report(&t, 1, false).await;
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 61).await;
    assert_eq!(incidents(&t, "firing").await.len(), 1);
    // 直接改库成退役中（不走退役接口，告警没被关）：evaluate 不再给原因，但这不是「恢复」
    sqlx::query("update machine set lifecycle = 'retiring' where id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    for secs in [91, 151, 211] {
        pass(&t, &mut state, base, secs).await;
    }
    assert_eq!(incidents(&t, "firing").await.len(), 1, "退役中的机器的告警不自动恢复");
    // 改回 active：照常评估，原因消失满 60 秒恢复
    sqlx::query("update machine set lifecycle = 'active' where id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    xray_report(&t, 2, true).await;
    pass(&t, &mut state, base, 241).await;
    pass(&t, &mut state, base, 301).await;
    assert!(incidents(&t, "firing").await.is_empty());
}

/// 退役事务在告警评估取完机器视图之后才提交：评估拿着旧视图（机器还在用、有原因）也不能给退役中的机器新建或重开告警
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_skip_machine_retired_after_view_loaded() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    assert_eq!(t.report("tok1", &blank_report(1)).await.0, StatusCode::OK);
    // 已过期：续费类不去抖，评估一轮就开
    sqlx::query("insert into subscription (machine_id, price_cents, due_date) values ($1, 500, current_date - 3)").bind(relay).execute(&t.st.db).await.unwrap();
    let set = |sql: &'static str| {
        let db = t.st.db.clone();
        async move {
            sqlx::query(sql).bind(relay).execute(&db).await.unwrap();
        }
    };
    let retire = "update machine set lifecycle = 'retiring' where id = $1";
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();

    // 新建：视图已取（3 s 缓存），随后机器进入退役，评估用的还是那份视图
    t.st.live.invalidate();
    fleet_server::views::live(&t.st).await.unwrap();
    set(retire).await;
    alerts::evaluate_once(&t.st, &mut state, base).await.unwrap();
    assert_eq!(t.count("select count(*) from incident").await, 0, "不给退役中的机器开告警");

    // 重开：在用时开过、续费后恢复（30 分钟内复发会重开原告警），同样的时序下不重开
    set("update machine set lifecycle = 'active' where id = $1").await;
    pass(&t, &mut state, base, 0).await;
    assert_eq!(t.count("select count(*) from incident where state = 'firing'").await, 1);
    set("update subscription set due_date = current_date + 30 where machine_id = $1").await;
    pass(&t, &mut state, base, 30).await;
    assert_eq!(t.count("select count(*) from incident where state = 'resolved'").await, 1);
    set("update subscription set due_date = current_date - 3 where machine_id = $1").await;
    t.st.live.invalidate();
    fleet_server::views::live(&t.st).await.unwrap();
    set(retire).await;
    alerts::evaluate_once(&t.st, &mut state, base + time::Duration::seconds(60)).await.unwrap();
    assert_eq!(t.count("select count(*) from incident where state = 'firing' or reopen_count > 0").await, 0, "不重开");
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
    t.st.live.invalidate();
    alerts::evaluate_once(&t.st, &mut state, OffsetDateTime::now_utc()).await.unwrap();
    assert!(has_rule(&incidents(&t, "firing").await, "renewal_due"), "续费类告警不去抖，立即开");
    // 登记续费后下一轮评估立即恢复（续费类关也不去抖）
    let new_due = OffsetDateTime::now_utc().date() + time::Duration::days(60);
    t.post(&format!("/api/v1/machines/{relay}/renewals"), json!({"new_due": new_due.to_string(), "amount_cents": 500})).await;
    alerts::evaluate_once(&t.st, &mut state, OffsetDateTime::now_utc()).await.unwrap();
    assert!(!has_rule(&incidents(&t, "firing").await, "renewal_due"));
    assert!(has_rule(&incidents(&t, "resolved").await, "renewal_due"));

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

/// 把入口在 `hour` 这一桶的业务流量设成 `mib` MiB，并让视图里的流量缓存作废（同小时汇总跑完）
async fn set_traffic(t: &T, entry: Uuid, hour: OffsetDateTime, mib: i64) {
    sqlx::query(
        "insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, $2, 0, $3)
         on conflict (entry_id, hour) do update set down_bytes = excluded.down_bytes",
    )
    .bind(entry)
    .bind(hour)
    .bind(mib * 1024 * 1024)
    .execute(&t.st.db)
    .await
    .unwrap();
    t.st.live.rolled_up();
}

/// 流量额度：本周期已用 ≥ 90% 立即开警告，≥ 100% 升严重并马上再通知一次，回落（新周期已用归零）立即恢复；
/// 只开告警，健康状态不变；没设上限的机器不评估
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn traffic_quota_alert_lifecycle() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "webhook", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    xray_report(&t, 1, true).await;
    // 线路机额度 1 GiB、每月 1 号重置（账单时区北京）；节点没有订阅也没有套餐：不限
    sqlx::query("insert into subscription (machine_id, price_cents, due_date, traffic_gb, traffic_reset_day) values ($1, 500, current_date + 60, 1, 1)")
        .bind(relay)
        .execute(&t.st.db)
        .await
        .unwrap();
    let (hour, days): (OffsetDateTime, i32) = sqlx::query_as(
        "select date_trunc('hour', now()), (date_trunc('month', now() at time zone 'Asia/Shanghai') + interval '1 month')::date - (now() at time zone 'Asia/Shanghai')::date",
    )
    .fetch_one(&t.st.db)
    .await
    .unwrap();
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();

    set_traffic(&t, entry, hour, 911).await;
    assert_eq!(t.health(relay).await, ("ok".to_string(), vec![]), "89% 不报");
    // 92%：不去抖，评估一轮就开警告；健康状态仍是正常；节点同样有这些流量，但不限额度，不评估
    set_traffic(&t, entry, hour, 950).await;
    assert_eq!(t.health(relay).await, ("ok".to_string(), vec!["traffic_quota_high".to_string()]));
    assert!(t.health(node).await.1.is_empty());
    pass(&t, &mut state, base, 0).await;
    let inc = incidents(&t, "firing").await;
    assert_eq!(inc.len(), 1, "{inc:?}");
    assert_eq!((inc[0]["rule"].as_str(), inc[0]["rule_name"].as_str(), inc[0]["severity"].as_str()), (Some("traffic_quota_high"), Some("流量额度吃紧"), Some("warning")));
    assert_eq!(inc[0]["subject_id"], relay.to_string());
    assert_eq!(inc[0]["summary"], format!("线路机1：本周期流量已用 92%（950 MiB / 1 GiB），{days} 天后重置"));
    log_settles(&t, 1).await;
    t.post(&format!("/api/v1/incidents/{}/ack", inc[0]["id"].as_str().unwrap()), json!({})).await;

    // 超过 100%：同一条告警升为严重，马上再通知一次（不等每天一次的提醒），对「吃紧」的确认清掉；健康状态仍不变
    set_traffic(&t, entry, hour, 1100).await;
    pass(&t, &mut state, base, 30).await;
    let inc = incidents(&t, "firing").await;
    assert_eq!((inc.len(), inc[0]["severity"].as_str()), (1, Some("critical")));
    assert!(inc[0]["ack_at"].is_null(), "{inc:?}");
    assert_eq!(inc[0]["summary"], format!("线路机1：本周期流量已超出额度（已用 107%，1.07 GiB / 1 GiB），{days} 天后重置"));
    assert_eq!(t.health(relay).await.0, "ok");
    log_settles(&t, 2).await;
    pass(&t, &mut state, base, 60).await;
    log_settles(&t, 2).await;

    // 上限调到 2 GiB、回到吃紧：摘要跟着变，严重度不降（收到过「严重」的渠道恢复时按严重收到），也不再通知
    sqlx::query("update subscription set traffic_gb = 2 where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    set_traffic(&t, entry, hour, 1900).await;
    pass(&t, &mut state, base, 75).await;
    let inc = incidents(&t, "firing").await;
    assert_eq!((inc.len(), inc[0]["severity"].as_str()), (1, Some("critical")));
    assert_eq!(inc[0]["summary"], format!("线路机1：本周期流量已用 92%（1.86 GiB / 2 GiB），{days} 天后重置"));
    log_settles(&t, 2).await;

    // 新周期开始、已用归零：下一轮立即恢复
    sqlx::query("delete from entry_traffic_hourly").execute(&t.st.db).await.unwrap();
    t.st.live.rolled_up();
    pass(&t, &mut state, base, 90).await;
    assert!(incidents(&t, "firing").await.is_empty());
    log_settles(&t, 3).await;
    let sent: Vec<(String, String)> = mock.bodies("/hook/a").iter().map(|b| (b["event"].as_str().unwrap().into(), b["severity"].as_str().unwrap().into())).collect();
    let want = [("firing", "warning"), ("firing", "critical"), ("resolved", "critical")].map(|(e, s)| (e.to_string(), s.to_string()));
    assert_eq!(sent, want);
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
    assert!(log.iter().all(|l| l["kind"] == "test" && l["subject_name"].is_null() && l["rule_name"].is_null()));
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
    // 坏渠道每条按 0.5 s、2 s 退避重试两次，两条排队发完约 5 秒
    for _ in 0..200 {
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
    assert!(log.iter().all(|l| l["incident_id"].is_string() && l["kind"] == "firing" && l["subject_name"] == "线路机1"));
    assert!(log.iter().any(|l| l["rule_name"] == "Xray 未运行") && log.iter().any(|l| l["rule_name"] == "配置未应用"), "{log:?}");
    assert!(log.iter().all(|l| l.get("rule").is_none()));

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

    // 恢复：Xray 起来、配置也应用了；原因消失满 60 秒后各发一条「恢复」（只发给收到过触发的渠道）
    let mut r = blank_report(2);
    r.xray = Some(xray(true));
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    sqlx::query("update relay set applied_generation = 3, applied_at = now() where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    pass(&t, &mut state, base, 181).await;
    assert_eq!(incidents(&t, "firing").await.len(), 2);
    pass(&t, &mut state, base, 241).await;
    assert!(incidents(&t, "firing").await.is_empty());
    mock.wait("/wecom/send", 3).await;
    mock.wait("/bot123:ABC", 5).await;
    mock.wait("/hook/a", 5).await;
    let last_wc = mock.bodies("/wecom/send").pop().unwrap();
    assert!(last_wc["markdown"]["content"].as_str().unwrap().contains("告警已恢复"), "{last_wc}");
    assert!(mock.bodies("/hook/a").iter().any(|b| b["event"] == "resolved" && b["rule"] == "relay_xray_down"));

    // 机器退役后，记录里的对象名带「（已退役）」，不和接过同名的新机器混淆
    sqlx::query("update machine set lifecycle = 'retired', retired_at = now() where id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    let log = t.get("/api/v1/notify-log?limit=100").await;
    assert!(log.as_array().unwrap().iter().all(|l| l["subject_name"] == "线路机1（已退役）"), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifications_sent_in_order_and_wait_on_429() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "tg", "kind": "telegram", "config": {"bot_token": "1:A", "chat_id": 1, "api_base": mock.url("/limited")}, "min_severity": "info", "enabled": true})).await;
    // 同一轮开两条告警（Xray 未运行 + 配置未应用）
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
    // 第一条被限流（retry_after = 1 s）：等满 1 秒前不重发，第二条也排在后面不并发发出
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(mock.count("/limited"), 1);
    mock.wait("/limited", 3).await;
    log_settles(&t, 2).await;
    assert_eq!(t.count("select count(*) from notify_log where ok").await, 2, "限流后重试成功，两条都送达");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_channel_does_not_delay_other_channels() {
    let t = setup().await;
    let mock = Mock::start().await;
    for (name, path) in [("slow", "/slow/a"), ("fast", "/hook/b")] {
        t.post("/api/v1/notify-channels", json!({"name": name, "kind": "webhook", "config": {"url": mock.url(path)}, "min_severity": "info", "enabled": true})).await;
    }
    // 同一轮开两条告警（Xray 未运行 + 配置未应用）
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    xray_report(&t, 1, false).await;
    sqlx::query("update relay set generation = 3, applied_generation = 1, generation_at = now() - interval '5 minutes' where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 61).await;
    assert_eq!(incidents(&t, "firing").await.len(), 2);
    // 慢渠道每条 1.5 秒、还在发第一条；快渠道不等它，两条都已送到
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!((mock.count("/slow/a"), mock.count("/hook/b")), (1, 2));
    // 慢渠道自己仍按顺序把两条发完
    mock.wait("/slow/a", 2).await;
    log_settles(&t, 4).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolved_sent_while_firing_still_queued() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "slow", "kind": "webhook", "config": {"url": mock.url("/slow/a")}, "min_severity": "info", "enabled": true})).await;
    seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    xray_report(&t, 1, false).await;
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 61).await;
    // 「触发」还在发（没写进 notify_log）时告警就恢复了：照样报恢复，排在触发之后
    xray_report(&t, 2, true).await;
    pass(&t, &mut state, base, 91).await;
    pass(&t, &mut state, base, 151).await;
    assert!(incidents(&t, "firing").await.is_empty());
    assert_eq!(t.count("select count(*) from notify_log").await, 0);
    log_settles(&t, 2).await;
    let events: Vec<Value> = mock.bodies("/slow/a").iter().map(|b| b["event"].clone()).collect();
    assert_eq!(events, [json!("firing"), json!("resolved")]);
}

async fn xray_report(t: &T, seq: u64, up: bool) {
    let mut r = blank_report(seq);
    r.xray = Some(xray(up));
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
}

/// 等通知记录写到 n 条（发送是后台的），再多等一会确认没有多发
async fn log_settles(t: &T, n: i64) {
    for _ in 0..100 {
        if t.count("select count(*) from notify_log").await >= n {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(t.count("select count(*) from notify_log").await, n);
}

/// 把最新一条通知记录的时间改成「告警评估时钟」上的某一刻（评估的 now 是快进的，通知按真实时间记）
async fn stamp_last_notice(t: &T, at: OffsetDateTime) {
    sqlx::query("update notify_log set ts = $1 where id = (select max(id) from notify_log)").bind(at).execute(&t.st.db).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flapping_alert_reopens_same_incident_without_spam() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "webhook", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let events = || mock.bodies("/hook/a").iter().map(|b| b["event"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    let at = |s: i64| base + time::Duration::seconds(s);

    // 触发 → 确认 → 恢复：各通知一次
    xray_report(&t, 1, false).await;
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 61).await;
    let first = incidents(&t, "firing").await[0]["id"].clone();
    log_settles(&t, 1).await;
    stamp_last_notice(&t, at(61)).await;
    t.post(&format!("/api/v1/incidents/{}/ack", first.as_str().unwrap()), json!({})).await;
    xray_report(&t, 2, true).await;
    pass(&t, &mut state, base, 91).await;
    pass(&t, &mut state, base, 151).await;
    assert!(incidents(&t, "firing").await.is_empty());
    log_settles(&t, 2).await;
    stamp_last_notice(&t, at(151)).await;

    // 恢复通知后 5 分钟内复发：重开同一条（复发计数 +1、确认清掉），先不通知；满 5 分钟仍在触发再发「触发」
    xray_report(&t, 3, false).await;
    pass(&t, &mut state, base, 181).await;
    pass(&t, &mut state, base, 241).await;
    let inc = incidents(&t, "firing").await;
    assert_eq!((inc.len(), &inc[0]["id"], inc[0]["reopen_count"].as_i64()), (1, &first, Some(1)), "{inc:?}");
    assert!(inc[0]["ack_at"].is_null() && inc[0]["ack_by"].is_null(), "{inc:?}");
    assert_eq!(t.count("select count(*) from incident").await, 1);
    pass(&t, &mut state, base, 421).await;
    log_settles(&t, 2).await;
    pass(&t, &mut state, base, 451).await;
    log_settles(&t, 3).await;
    stamp_last_notice(&t, at(451)).await;
    xray_report(&t, 4, true).await;
    pass(&t, &mut state, base, 481).await;
    pass(&t, &mut state, base, 541).await;
    log_settles(&t, 4).await;
    stamp_last_notice(&t, at(541)).await;
    assert_eq!(events(), ["firing", "resolved", "firing", "resolved"]);

    // 又在 5 分钟内抖一下就恢复：不通知，也不重复报恢复
    xray_report(&t, 5, false).await;
    pass(&t, &mut state, base, 571).await;
    pass(&t, &mut state, base, 631).await;
    assert_eq!(incidents(&t, "firing").await[0]["reopen_count"], 2);
    xray_report(&t, 6, true).await;
    pass(&t, &mut state, base, 661).await;
    pass(&t, &mut state, base, 721).await;
    assert!(incidents(&t, "firing").await.is_empty());
    log_settles(&t, 4).await;

    // 距恢复通知已满 5 分钟才复发：重开时立即发「触发」（真故障不被压住），恢复照常配对
    xray_report(&t, 7, false).await;
    pass(&t, &mut state, base, 901).await;
    pass(&t, &mut state, base, 961).await;
    assert_eq!(incidents(&t, "firing").await[0]["reopen_count"], 3);
    log_settles(&t, 5).await;
    stamp_last_notice(&t, at(961)).await;
    xray_report(&t, 8, true).await;
    pass(&t, &mut state, base, 991).await;
    pass(&t, &mut state, base, 1051).await;
    log_settles(&t, 6).await;
    assert_eq!(events(), ["firing", "resolved", "firing", "resolved", "firing", "resolved"]);

    // 超过重开窗口再出现：新开一条
    sqlx::query("update incident set resolved_at = now() - interval '1 hour'").execute(&t.st.db).await.unwrap();
    xray_report(&t, 9, false).await;
    pass(&t, &mut state, base, 1081).await;
    pass(&t, &mut state, base, 1141).await;
    let inc = incidents(&t, "firing").await;
    assert!(inc[0]["id"] != first && inc[0]["reopen_count"] == 0, "{inc:?}");
    log_settles(&t, 7).await;
}

// ── T7 / T8：实时流（WebSocket）与错误 ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_requires_auth_and_resyncs_when_subscriber_lags() {
    let t = setup().await;
    match t.ws(false).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => assert_eq!(r.status(), StatusCode::UNAUTHORIZED),
        other => panic!("未登录应 401：{:?}", other.map(|_| ())),
    }
    let mut ws = t.ws(true).await.unwrap();
    // 一口气广播远超缓冲（256）的事件，服务端逐帧往外写跟不上 → 落后
    for _ in 0..5000 {
        t.st.emit(Event::Machines);
    }
    ws_next(&mut ws, "resync").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_is_pushed_as_metrics() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "203.0.113.10", "tok1").await;
    let mut ws = t.ws(true).await.unwrap();
    let mut p = live::Published::default();
    let now = now_ms();
    let mut r = blank_report(1);
    r.host = vec![host(now - 10_000, 10.0, 1_000, 2_000), host(now, 42.5, 11_000, 22_000)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    assert!(live::publish(&t.st, &mut p).await.unwrap());
    let m = ws_next(&mut ws, "metrics").await;
    let me = &m["machines"][relay.to_string()];
    // host = [ts, cpu, load1, mem_used, mem_total, disk_used, disk_total, rx_bps, tx_bps, tcp]
    assert_eq!((me["host"][1].as_f64(), me["host"][7].as_i64(), me["host"][8].as_i64()), (Some(42.5), Some(8_000), Some(16_000)), "{me}");
    assert_eq!(me["host"][0].as_i64(), Some(now / 1000));
    assert!(me["seen"].is_i64() && me["agent"]["online"] == true && me["health"]["status"].is_string(), "{me}");
    assert!(m["overview"]["relays"]["total"] == 1, "{m}");

    // 没有变化：不发
    assert!(!live::publish(&t.st, &mut p).await.unwrap());
    // 下一份上报：只带变了的字段
    let mut r = blank_report(2);
    r.host = vec![host(now + 10_000, 50.0, 21_000, 42_000)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    assert!(live::publish(&t.st, &mut p).await.unwrap());
    let m = ws_next(&mut ws, "metrics").await;
    let me = m["machines"][relay.to_string()].as_object().unwrap();
    assert_eq!(me["host"][1].as_f64(), Some(50.0));
    assert!(!me.contains_key("health") && !me.contains_key("agent"), "{me:?}");
}

/// 新建 / 删除维护静默：只发 incidents（不发 machines，前端不整表重取），同时标记实时指标该比对，「维护中」经 metrics 送达
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintenance_silence_reaches_cards_via_metrics() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "203.0.113.10", "tok1").await;
    let mut ws = t.ws(true).await.unwrap();
    let mut p = live::Published::default();
    assert!(live::publish(&t.st, &mut p).await.unwrap());
    ws_next(&mut ws, "metrics").await;

    let f = time::format_description::well_known::Rfc3339;
    let now = OffsetDateTime::now_utc();
    let mut ev = t.st.events.subscribe();
    let dirty = || t.st.metrics_dirty.swap(false, std::sync::atomic::Ordering::Relaxed);
    dirty();
    let id = t.post("/api/v1/silences", json!({
        "subject_id": relay, "is_maintenance": true, "comment": "换机房",
        "starts_at": (now - time::Duration::minutes(1)).format(&f).unwrap(), "ends_at": (now + time::Duration::hours(1)).format(&f).unwrap(),
    })).await["id"].as_str().unwrap().to_string();
    for (what, want) in [("新建", true), ("删除", false)] {
        if !want {
            let (s, v) = t.req("DELETE", &format!("/api/v1/silences/{id}"), None, None).await;
            assert_eq!(s, StatusCode::OK, "{v}");
        }
        let mut evs = vec![];
        while let Ok(e) = ev.try_recv() {
            evs.push(e);
        }
        assert!(evs.iter().any(|e| matches!(e, Event::Incidents)) && !evs.iter().any(|e| matches!(e, Event::Machines)), "{what}：{evs:?}");
        assert!(dirty(), "{what}后实时指标该比对");
        assert!(live::publish(&t.st, &mut p).await.unwrap());
        let m = ws_next(&mut ws, "metrics").await;
        let me = &m["machines"][relay.to_string()];
        assert_eq!(me["maintenance"].as_bool(), Some(want), "{what}：{me}");
        assert_eq!(me["health"]["status"] == "maintenance", want, "{what}：{me}");
    }
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

// ── 告警列表分页：limit / offset + total，顺序稳定（同一时刻开始的也不重不漏） ──────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incidents_list_pages_with_total() {
    let t = setup().await;
    let db = &t.st.db;
    // 7 条进行中（其中 4 条同一时刻开始，靠 id 兜底排序）+ 2 条已恢复
    for (state, sev, mins) in [("firing", "critical", 5), ("firing", "warning", 1), ("firing", "warning", 3), ("firing", "warning", 3), ("firing", "warning", 3), ("firing", "warning", 3), ("firing", "critical", 9), ("resolved", "critical", 2), ("resolved", "warning", 4)] {
        sqlx::query(&format!(
            "insert into incident (id, rule, subject_id, severity, state, summary, opened_at, resolved_at) values ($1, 'agent_offline', $2, '{sev}', '{state}', 's',
             now() - interval '{mins} minutes', case when '{state}' = 'resolved' then now() end)"
        ))
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .execute(db)
        .await
        .unwrap();
    }
    let tr = &t;
    let page = |q: &'static str| async move { tr.get(&format!("/api/v1/incidents?{q}")).await };
    let ids = |v: &Value| v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();

    let all = page("state=firing&limit=100").await;
    assert_eq!((all["total"].as_i64(), all["next_offset"].is_null(), ids(&all).len()), (Some(7), true, 7));
    assert_eq!(all["items"][0]["severity"], "critical", "严重的在前");
    let p1 = page("state=firing&limit=3").await;
    let p2 = page("state=firing&limit=3&offset=3").await;
    let p3 = page("state=firing&limit=3&offset=6").await;
    assert_eq!((p1["total"].as_i64(), p1["next_offset"].as_i64()), (Some(7), Some(3)));
    assert_eq!((p2["next_offset"].as_i64(), p3["next_offset"].is_null(), ids(&p3).len()), (Some(6), true, 1));
    assert_eq!([ids(&p1), ids(&p2), ids(&p3)].concat(), ids(&all), "逐页拼起来等于一次取全");
    let past = page("state=firing&limit=3&offset=30").await;
    assert_eq!((ids(&past).len(), past["total"].as_i64(), past["next_offset"].is_null()), (0, Some(7), true), "越过末尾：空页，但总数照给");
    assert_eq!(page("state=all").await["total"].as_i64(), Some(9));
    assert_eq!(page("state=resolved").await["total"].as_i64(), Some(2));
}

/// 按机器筛选（抽屉的「最近告警」）：subject_id 与 state、limit、offset 组合，total 也只数这台的
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incidents_filter_by_subject() {
    let t = setup().await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    for (subject, state, mins) in [(a, "resolved", 1), (a, "firing", 2), (a, "resolved", 3), (b, "firing", 4)] {
        sqlx::query(&format!(
            "insert into incident (id, rule, subject_id, severity, state, summary, opened_at, resolved_at) values ($1, 'agent_offline', $2, 'warning', '{state}', 's',
             now() - interval '{mins} minutes', case when '{state}' = 'resolved' then now() end)"
        ))
        .bind(Uuid::now_v7())
        .bind(subject)
        .execute(&t.st.db)
        .await
        .unwrap();
    }
    let get = |q: String| {
        let t = &t;
        async move { t.get(&format!("/api/v1/incidents?{q}")).await }
    };
    let subjects = |v: &Value| v["items"].as_array().unwrap().iter().map(|i| i["subject_id"].as_str().unwrap().to_string()).collect::<Vec<_>>();

    let p1 = get(format!("subject_id={a}&state=all&limit=2")).await;
    assert_eq!((p1["total"].as_i64(), p1["next_offset"].as_i64()), (Some(3), Some(2)));
    assert_eq!(subjects(&p1), [a.to_string(), a.to_string()]);
    assert_eq!((p1["items"][0]["state"].as_str(), p1["items"][1]["state"].as_str()), (Some("firing"), Some("resolved")), "进行中在前");
    let p2 = get(format!("subject_id={a}&state=all&limit=2&offset=2")).await;
    assert_eq!((p2["total"].as_i64(), p2["next_offset"].is_null(), subjects(&p2)), (Some(3), true, vec![a.to_string()]));
    assert_eq!(get(format!("subject_id={a}")).await["total"].as_i64(), Some(1), "默认只看进行中");
    assert_eq!(get(format!("subject_id={b}&state=resolved")).await["total"].as_i64(), Some(0));
    assert_eq!(get("state=all".into()).await["total"].as_i64(), Some(4), "不带 subject_id 照旧");
    assert_eq!(t.req("GET", "/api/v1/incidents?subject_id=x", None, None).await.0, StatusCode::BAD_REQUEST);
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
