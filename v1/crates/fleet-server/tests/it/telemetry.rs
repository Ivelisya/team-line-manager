//! 遥测入库 / 小时汇总 / 健康判定 / 告警 / 通知 / 实时流 / 保留期。
//! 需要 PostgreSQL（见 common）。
//! 不起后台循环：告警评估、小时汇总、实时指标比对都由测试手动驱动（告警评估的「当前时间」可以快进，不用真等去抖）。

use crate::common;

use axum::http::StatusCode;
use common::tele::*;
use fleet_proto::ProbeKind;
use fleet_server::alerts::{self, AlertState};
use fleet_server::crypto::SecretBox;
use fleet_server::state::{AppState, Event};
use fleet_server::{db, live, selfmon};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};
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

/// 入库失败按类回：数据错误 422（agent 移入 dead/），基础设施错误 503（agent 继续重试）。
/// agent 给的字符串里的 NUL（text / jsonb 不收）入库前去掉；入库失败的那份不扣令牌、不推进 seq，
/// 故意让一份带极大 seq 的报告失败之后，后面的照样限流
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
    r.boot_id = "b\0oot".into();
    let (s, v) = t.report("tok1", &r).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(t.count("select count(*) from access_event where dest_host = 'badhost'").await, 1);

    // 违反约束（23 类）→ 422
    sqlx::query("alter table agent_state add constraint test_bad_boot check (boot_id <> 'bad')").execute(&t.st.db).await.unwrap();
    let mut r = blank_report(1 << 62);
    r.boot_id = "bad".into();
    let (s, v) = t.report("tok1", &r).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("无法入库"), "{v}");
    // 突发 600 份（第一份已扣）：之后的照常扣，到不了 700 份就 429
    let limited = {
        let mut hit = None;
        for seq in 2..700u64 {
            let (s, v) = t.report("tok1", &blank_report(seq)).await;
            if s == StatusCode::TOO_MANY_REQUESTS {
                hit = Some(seq);
                break;
            }
            assert_eq!(s, StatusCode::OK, "{seq} {v}");
        }
        hit
    };
    assert!(limited.is_some_and(|s| s >= 600), "{limited:?}");

    t.st.db.close().await;
    let (s, v) = t.report("tok1", &blank_report(2)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
}

/// 同一台机器同时只处理一份上报：令牌入库提交后才扣，并发的几份（seq 各不相同）会一起看到余额、都放进来。
/// 一份还在处理时来的第二份回 429（agent 按 Retry-After 1 秒后重发），前一份照常入库
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reports_for_one_machine_get_429() {
    let t = setup().await;
    let id = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    assert_eq!(t.report("tok1", &blank_report(1)).await.0, StatusCode::OK);
    // 第一份卡在 agent_state 的行锁上（占着处理位置），这时来第二份
    let mut lock = t.st.db.begin().await.unwrap();
    sqlx::query("select 1 from agent_state where machine_id = $1 for update").bind(id).execute(&mut *lock).await.unwrap();
    let second = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let r = t.report("tok1", &blank_report(3)).await;
        lock.commit().await.unwrap();
        r
    };
    let first = blank_report(2);
    let ((s1, v1), (s2, v2)) = tokio::join!(t.report("tok1", &first), second);
    assert_eq!((s1, v1["accepted_seq"].as_u64()), (StatusCode::OK, Some(2)), "{v1}");
    assert_eq!((s2, v2["error"]["code"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited")), "{v2}");
    // 处理完就放开：接着发的照收
    assert_eq!(t.report("tok1", &blank_report(3)).await.0, StatusCode::OK);
}

/// 上报事务里的语句卡住（等别人的锁）：10 秒语句超时后回 503（agent 退避重发），放开处理位置，不让这台机器之后的上报一直 429
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stuck_report_times_out_with_503() {
    let t = setup().await;
    let id = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    assert_eq!(t.report("tok1", &blank_report(1)).await.0, StatusCode::OK);
    let mut lock = t.st.db.begin().await.unwrap();
    sqlx::query("select 1 from agent_state where machine_id = $1 for update").bind(id).execute(&mut *lock).await.unwrap();
    let t0 = std::time::Instant::now();
    let (s, v) = t.report("tok1", &blank_report(2)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert!(t0.elapsed() < Duration::from_secs(14), "{:?}", t0.elapsed());
    lock.rollback().await.unwrap();
    assert_eq!(t.report("tok1", &blank_report(2)).await.0, StatusCode::OK);
}

/// 主机样本：同一时刻只收一条（重报不多占）；最多超前 10 分钟；每台机器按余额收（24 小时的量，之后 5 秒补一个）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_samples_dedup_future_window_and_budget() {
    let t = setup().await;
    seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let now = now_ms();
    let mut r = blank_report(1);
    r.host = vec![host(now - 10_000, 1.0, 1, 1), host(now - 10_000, 2.0, 1, 1), host(now + 5 * 60_000, 1.0, 1, 1), host(now + 15 * 60_000, 1.0, 1, 1)];
    let (s, v) = t.report("tok1", &r).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(t.count("select count(*) from host_metric").await, 2, "重复的一条、超前 15 分钟的一条不收");
    let mut r = blank_report(2);
    r.host = vec![host(now - 10_000, 3.0, 1, 1)];
    let (s, v) = t.report("tok1", &r).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(t.count("select count(*) from host_metric").await, 2, "下一份重报同一时刻：照样 200，不多一条");
    // 一口气补传 9000 个（25 份 × 360，1 秒一个）：前面扣了 4 个，余额只够 8636 个（加上这几秒补的）
    for j in 0..25i64 {
        let mut r = blank_report(3 + j as u64);
        r.host = (0..360).map(|i| host(now - 20_000 - (j * 360 + i) * 1000, 1.0, 1, 1)).collect();
        let (s, v) = t.report("tok1", &r).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    let n = t.count("select count(*) from host_metric").await;
    assert!((2 + 8636..2 + 8700).contains(&n), "{n}");
}

/// 应用回报：错误文本截到 512 字、不认识的入口忽略；没变化的重发不推送；一台线路机连发超过突发量回 429
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn applied_report_is_clipped_quiet_when_unchanged_and_rate_limited() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (_node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let mut ev = t.st.events.subscribe();
    let mut machines_events = || {
        let mut n = 0;
        while let Ok(e) = ev.try_recv() {
            n += matches!(e, Event::Machines) as usize;
        }
        n
    };
    let rep = json!({"generation": 1, "ok": false, "error": "x".repeat(600_000),
        "entries": [{"id": entry, "ok": false, "error": "y".repeat(600_000)}, {"id": Uuid::now_v7(), "ok": false, "error": "z"}]});
    let (s, v) = t.req("POST", "/agent/v1/applied", Some(rep.clone()), Some("tok1")).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    let (re, ee): (String, String) = sqlx::query_as("select r.apply_error, e.apply_error from relay r, entry e where r.machine_id = $1 and e.id = $2")
        .bind(relay)
        .bind(entry)
        .fetch_one(&t.st.db)
        .await
        .unwrap();
    assert_eq!((re.len(), ee.len()), (512, 512));
    assert_eq!(machines_events(), 1);
    let (s, _) = t.req("POST", "/agent/v1/applied", Some(rep), Some("tok1")).await;
    assert_eq!((s, machines_events()), (StatusCode::NO_CONTENT, 0), "同一份重发：没变化不推送");

    let mut limited = None;
    for i in 0..200 {
        let (s, v) = t.req("POST", "/agent/v1/applied", Some(json!({"generation": 1, "ok": true})), Some("tok1")).await;
        if s == StatusCode::TOO_MANY_REQUESTS {
            limited = Some((i, v));
            break;
        }
        assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    }
    let (i, v) = limited.expect("连发 200 份应被限流");
    assert!(i >= 110 && v["error"]["code"] == "rate_limited", "{i} {v}");
    assert_eq!(machines_events(), 1, "只有第一份成功回报（清掉错误）推送");
}

/// 取分享链接的审计记是哪条入口，不记链接
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn share_audit_names_the_entry() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (_node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let url = t.get(&format!("/api/v1/entries/{entry}/share")).await["url"].as_str().unwrap().to_string();
    let d: Value = sqlx::query_scalar("select detail from audit_log where action = 'entry.share' and target_id = $1").bind(entry).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((d["relay"].as_str(), d["port"].as_i64()), (Some("线路机1"), Some(20000)), "{d}");
    assert!(d["entry"].as_str().is_some_and(|s| !s.is_empty()) && !d.to_string().contains(&url), "{d}");
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

/// 写一行探测小时汇总：时区 `tz` 里「今天往前 back 天」零点之后第 h 小时的那一桶（h = -1 是前一天的最后一桶）
async fn put_probe_hour(t: &T, entry: Uuid, tz: &str, (back, h): (i32, i32), (kind, total, ok): (&str, i32, i32)) {
    sqlx::query("insert into probe_hourly (entry_id, hour, kind, total, ok) values ($1, ((local_date($2) - $3)::timestamp at time zone $2) + make_interval(hours => $4), $5, $6, $7)")
        .bind(entry)
        .bind(tz)
        .bind(back)
        .bind(h)
        .bind(kind)
        .bind(total)
        .bind(ok)
        .execute(&t.st.db)
        .await
        .unwrap();
}

/// 近 N 天可用率：小时汇总按账单时区分天（没有订阅按北京时间），覆盖完整的 N 天、最后一天是今天，没有探测的天各项为 0；
/// 线路机汇总挂在它上面的入口；days 越界 400、机器不存在 404
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn availability_days_follow_billing_tz() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let relay2 = seed_relay(&t, "线路机2", "198.51.100.2", "tok2").await;
    let team = seed_team(&t, "团队1").await;
    // 节点 1 单独挂在线路机 2 上，账单时区洛杉矶；线路机 1 和它的两个节点都没有订阅
    let (n1, e1) = seed_node(&t, relay2, team, 1, "203.0.113.5", 20000).await;
    let (n2, e2) = seed_node(&t, relay, team, 2, "203.0.113.6", 20001).await;
    let (n3, e3) = seed_node(&t, relay, team, 3, "203.0.113.7", 20002).await;
    sqlx::query("insert into subscription (machine_id, price_cents, due_date, billing_tz) values ($1, 500, current_date + 60, 'America/Los_Angeles')")
        .bind(n1)
        .execute(&t.st.db)
        .await
        .unwrap();
    let (la, sh) = ("America/Los_Angeles", "Asia/Shanghai");
    put_probe_hour(&t, e1, la, (0, 0), ("tcp", 120, 119)).await;
    put_probe_hour(&t, e1, la, (0, 0), ("egress", 60, 60)).await;
    put_probe_hour(&t, e1, la, (0, -1), ("tcp", 120, 120)).await;
    put_probe_hour(&t, e1, la, (2, 5), ("tcp", 100, 50)).await;
    put_probe_hour(&t, e1, la, (29, 0), ("tcp", 7, 7)).await;
    put_probe_hour(&t, e1, la, (29, -1), ("tcp", 9, 0)).await;
    put_probe_hour(&t, e2, sh, (0, 0), ("tcp", 10, 10)).await;
    put_probe_hour(&t, e2, sh, (0, -1), ("tcp", 10, 0)).await;
    put_probe_hour(&t, e2, sh, (0, -1), ("egress", 4, 4)).await;
    put_probe_hour(&t, e3, sh, (0, 0), ("tcp", 30, 27)).await;
    let (la_today, sh_today): (time::Date, time::Date) = sqlx::query_as("select local_date($1), local_date($2)").bind(la).bind(sh).fetch_one(&t.st.db).await.unwrap();
    let day = |v: &Value, i: usize| {
        let d = &v["items"][i];
        (d["tcp_total"].as_i64().unwrap(), d["tcp_ok"].as_i64().unwrap(), d["egress_total"].as_i64().unwrap(), d["egress_ok"].as_i64().unwrap())
    };

    // 默认 30 天：洛杉矶的今天和往前 29 天；零点那一桶算当天，零点前一小时那一桶算前一天，窗口第一天之前的不算
    let v = t.get(&format!("/api/v1/machines/{n1}/availability")).await;
    assert_eq!((v["days"].as_i64(), v["items"].as_array().unwrap().len()), (Some(30), 30), "{v}");
    assert_eq!(v["items"][0]["date"], (la_today - time::Duration::days(29)).to_string());
    assert_eq!(v["items"][29]["date"], la_today.to_string());
    assert_eq!(day(&v, 29), (120, 119, 60, 60));
    assert_eq!(day(&v, 28), (120, 120, 0, 0));
    assert_eq!(day(&v, 27), (100, 50, 0, 0));
    assert_eq!(day(&v, 0), (7, 7, 0, 0));
    assert!((1..27).all(|i| day(&v, i) == (0, 0, 0, 0)), "没有探测的天各项为 0：{v}");
    assert_eq!(v["tcp_pct"].as_f64(), Some(100.0 * 296.0 / 347.0));
    assert_eq!(v["egress_pct"].as_f64(), Some(100.0));

    // 线路机 1 没有订阅：按北京时间分天，汇总节点 2、3 的入口（不含线路机 2 上节点 1 的）
    let v = t.get(&format!("/api/v1/machines/{relay}/availability?days=2")).await;
    assert_eq!(v["items"][0]["date"], (sh_today - time::Duration::days(1)).to_string());
    assert_eq!(v["items"][1]["date"], sh_today.to_string());
    assert_eq!((day(&v, 0), day(&v, 1)), ((10, 0, 4, 4), (40, 37, 0, 0)), "{v}");
    assert_eq!((v["tcp_pct"].as_f64(), v["egress_pct"].as_f64()), (Some(74.0), Some(100.0)));
    // 节点只算自己的入口；整段没有出口探测：egress_pct 为 null
    let v = t.get(&format!("/api/v1/machines/{n2}/availability?days=1")).await;
    assert_eq!((v["items"].as_array().unwrap().len(), day(&v, 0)), (1, (10, 10, 0, 0)), "{v}");
    let v = t.get(&format!("/api/v1/machines/{n3}/availability?days=2")).await;
    assert_eq!((v["tcp_pct"].as_f64(), v["egress_pct"].is_null()), (Some(90.0), true), "{v}");

    for bad in ["0", "91", "x"] {
        let (s, v) = t.req("GET", &format!("/api/v1/machines/{n1}/availability?days={bad}"), None, None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    }
    assert_eq!(t.req("GET", &format!("/api/v1/machines/{}/availability", Uuid::now_v7()), None, None).await.0, StatusCode::NOT_FOUND);
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
    assert_eq!(t.count("select count(*) from rollup_dirty").await, 3, "还有下一小时（它第一个网卡增量的前值可能是这个样本）和当前小时的标记");
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

    // 整点前 20 分钟的样本（agent 离线跨过整点）：照样是本小时第一个增量的前值，缺口那段记进这一小时
    let hb = h0 - 2 * H;
    let mut r3 = blank_report(3);
    r3.host = vec![host(hb - 20 * 60_000, 20.0, 100, 200), host(hb + 10_000, 20.0, 300, 600), host(hb + 20_000, 20.0, 400, 800)];
    assert_eq!(t.report("tok1", &r3).await.0, StatusCode::OK);
    db::rollup_hours(&t.st.db).await.unwrap();
    let nic_at = |h: i64| {
        sqlx::query_as::<_, (i64, i64)>("select net_rx, net_tx from host_metric_hourly where machine_id = $1 and extract(epoch from hour)::bigint * 1000 = $2")
            .bind(relay)
            .bind(h)
            .fetch_optional(&t.st.db)
    };
    assert_eq!(nic_at(hb).await.unwrap(), Some((400 - 100, 800 - 200)), "20 分钟前的样本参与本小时第一个增量");

    // 7 小时缺口（离线队列淘汰了中间的报告）：恢复后那一小时记下整段增量，中间的小时没有行；读数变小仍按读数即增量
    let (ha, hg) = (h0 + 2 * H, h0 + 9 * H);
    let mut r4 = blank_report(4);
    r4.host = vec![host(ha + 10_000, 20.0, 2000, 4000), host(hg + 10_000, 20.0, 6000, 12000), host(hg + 20_000, 20.0, 50, 100)];
    assert_eq!(t.report("tok1", &r4).await.0, StatusCode::OK);
    db::rollup_hours(&t.st.db).await.unwrap();
    assert_eq!(nic_at(ha).await.unwrap(), Some((2000 - 600, 4000 - 1200)));
    assert_eq!(nic_at(hg).await.unwrap(), Some((4000 + 50, 8000 + 100)));
    assert_eq!(nic_at(ha + 3 * H).await.unwrap(), None);

    // 机器的本月 / 今天网卡流量 = 组织时区本月 / 今天起的小时表之和（当前小时补一行，保证今天有数）
    sqlx::query("insert into host_metric_hourly (machine_id, hour, net_rx, net_tx) values ($1, date_trunc('hour', now()), 7, 9)").bind(relay).execute(&t.st.db).await.unwrap();
    t.st.live.rolled_up();
    let want: (i64, i64, i64, i64) = sqlx::query_as(
        "select sum(net_rx)::bigint, sum(net_tx)::bigint, (sum(net_rx) filter (where hour >= date_trunc('day', now() at time zone org_tz()) at time zone org_tz()))::bigint,
                (sum(net_tx) filter (where hour >= date_trunc('day', now() at time zone org_tz()) at time zone org_tz()))::bigint
         from host_metric_hourly where machine_id = $1 and hour >= date_trunc('month', now() at time zone org_tz()) at time zone org_tz()",
    )
    .bind(relay)
    .fetch_one(&t.st.db)
    .await
    .unwrap();
    let nic = t.get(&format!("/api/v1/machines/{relay}")).await["nic"].clone();
    assert_eq!((nic["month_rx"].as_i64(), nic["month_tx"].as_i64(), nic["today_rx"].as_i64(), nic["today_tx"].as_i64()), (Some(want.0), Some(want.1), Some(want.2), Some(want.3)), "{nic}");
    assert!(want.2 >= 7, "{want:?}");
}

/// 持久累计值往回跳一点（异常重启后 nic.json 比上报旧）记 0，不把整个累计值当增量；读数明显回到小值（新计数器）照旧记读数
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollup_ignores_small_backward_jump_of_persistent_counter() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let h0 = hour_floor(now_ms() - 2 * D);
    let mut r = blank_report(1);
    r.host = [(h0 + 10_000, 5_000_000_000_000u64), (h0 + 70_000, 4_990_000_000_000), (h0 + 80_000, 4_990_000_001_000), (h0 + 90_000, 300), (h0 + 100_000, 500)]
        .iter()
        .map(|(ts, rx)| host(*ts, 1.0, *rx, rx * 2))
        .collect();
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    db::rollup_hours(&t.st.db).await.unwrap();
    let got: (i64, i64) = sqlx::query_as("select net_rx, net_tx from host_metric_hourly where machine_id = $1").bind(relay).fetch_one(&t.st.db).await.unwrap();
    // 往回跳 10 GB 记 0（不是 60 s × 10 Gbit/s 的 75 GB）+ 1000 + 300（回到小值：读数即增量）+ 200
    assert_eq!(got, (1000 + 300 + 200, 2 * (1000 + 300 + 200)));
}

/// agent 报离谱的计数器（0 → u64::MAX，一小时里跳两次）：汇总不溢出（每段增量按 10 Gbit/s 封顶），
/// 库里已有的超大小时行也不让机器列表、详情报错
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn absurd_counters_do_not_overflow_rollup_or_views() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let h0 = hour_floor(now_ms() - 2 * D);
    let mut r = blank_report(1);
    r.host = [(h0 + 10_000, 0u64), (h0 + 20_000, u64::MAX), (h0 + 30_000, 0), (h0 + 40_000, u64::MAX)].iter().map(|(ts, v)| host(*ts, 1.0, *v, *v)).collect();
    r.entries = vec![entry_sample(entry, h0 + 5_000, u64::MAX, u64::MAX), entry_sample(entry, h0 + 15_000, u64::MAX, u64::MAX)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    db::rollup_hours(&t.st.db).await.unwrap();
    let rx: i64 = sqlx::query_scalar("select net_rx from host_metric_hourly where machine_id = $1").bind(relay).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(rx, 2 * 1_250_000_000 * 10, "两次跳变各按 10 秒 × 10 Gbit/s 封顶");
    let up: i64 = sqlx::query_scalar("select up_bytes from entry_traffic_hourly where entry_id = $1").bind(entry).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(up, 4_500_000_000_000);

    // 升级前已经入库的超大小时行：本周期求和、今天 / 本月求和都超过 bigint
    for m in [relay, node] {
        sqlx::query(
            "insert into host_metric_hourly (machine_id, hour, cpu_avg, cpu_max, mem_used_avg, mem_total_max, disk_used_max, disk_total_max, net_rx, net_tx)
             select $1, date_trunc('hour', now()) + h * interval '1 hour', 1, 1, 1, 1, 1, 1, 9e18, 9e18 from generate_series(0, 1) h",
        )
        .bind(m)
        .execute(&t.st.db)
        .await
        .unwrap();
    }
    sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes, conns_max) select $1, date_trunc('hour', now()) + h * interval '1 hour', 9e18, 9e18, 1 from generate_series(0, 1) h")
        .bind(entry)
        .execute(&t.st.db)
        .await
        .unwrap();
    t.st.live.invalidate();
    t.get("/api/v1/machines").await;
    let m = t.get(&format!("/api/v1/machines/{relay}")).await;
    assert_eq!(m["quota"]["used"].as_i64(), Some(9_000_000_000_000_000_000), "夹到 9e18：{}", m["quota"]);
    t.get(&format!("/api/v1/machines/{node}")).await;
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
    // 新 agent 报的 SYN 重发次数入库，按格计数
    sqlx::query("update probe_result set retrans = 2 where entry_id = $1 and ts = to_timestamp($2::float8 / 1000) and kind = 'tcp'")
        .bind(entry)
        .bind((cb - 1_000) as f64)
        .execute(&t.st.db)
        .await
        .unwrap();
    let v = t.get("/api/v1/probe-beats").await;
    assert_eq!(v.as_object().unwrap().len(), 1, "只有有探测的机器；线路机的格子是中控 → 线路机，不再汇总它的入口：{v}");
    let b = v[node.to_string()].as_array().unwrap();
    assert_eq!(b.len(), 20);
    assert_eq!(b[19], json!([1, 1, 10.0, 0]), "当前这格（没走完）：{v}");
    assert_eq!(b[18], json!([2, 2, 10.0, 1]), "上一格：中位数、重发过的次数：{v}");
    assert_eq!(b[17], json!([1, 0, null, 0]), "cb 前 4 分钟落在倒数第三格：{v}");
    assert_eq!(b[0], json!([1, 1, 10.0, 0]), "最早一格从 cb - 57 分钟起，再早的不算：{v}");
    assert!(b[1..17].iter().all(|x| *x == json!([0, 0, null, 0])), "{v}");
    assert!(v.get(relay.to_string()).is_none());
}

/// 卡片读数：TCP 近 1 小时成功样本的中位数（旧 agent 报的含 SYN 重发等待的尖刺不左右它），最新一次照旧给；出口探测没有中位数
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn entry_view_carries_tcp_median_of_last_hour() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let now = now_ms();
    let lat = |ms: i64, l: f32, retrans: Option<u32>| fleet_proto::ProbeSample { latency_ms: Some(l), retrans, ..probe(entry, now - ms, ProbeKind::Tcp, true) };
    let mut r = blank_report(1);
    r.probes = vec![
        lat(2 * 3_600_000, 900.0, None), // 一小时前的不算
        lat(50 * 60_000, 66.0, None),
        lat(40 * 60_000, 3091.0, None), // 旧 agent：第二次重发才连上
        lat(30 * 60_000, 67.0, Some(0)),
        probe(entry, now - 20 * 60_000, ProbeKind::Tcp, false),
        lat(1_000, 64.0, Some(2)),
        probe(entry, now, ProbeKind::Egress, true),
    ];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    let m = t.get(&format!("/api/v1/machines/{node}")).await;
    let e = &m["entries"][0];
    assert_eq!((e["tcp"]["latency_ms"].as_f64(), e["tcp"]["median_ms"].as_f64()), (Some(64.0), Some(66.5)), "{m}");
    assert!(e["egress"]["median_ms"].is_null(), "{m}");
}

/// 中控 → 线路机：轮流连客户入口（public_host + 各在用入口端口），只记通不通；对照目标都连不上时整轮不记，
/// 两台线路机同时不通照记；同一台最近 3 轮都连不上报 relay_unreachable
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_reach_rounds_feed_relay_beats() {
    let t = setup().await;
    let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ctl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (c1, c2) = (tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(), tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
    let (closed, closed2) = (c1.local_addr().unwrap().port(), c2.local_addr().unwrap().port());
    drop((c1, c2));
    let up = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let down = seed_relay(&t, "线路机2", "198.51.100.2", "tok2").await;
    let idle = seed_relay(&t, "线路机3", "198.51.100.3", "tok3").await;
    let team = seed_team(&t, "团队1").await;
    seed_node(&t, up, team, 1, "203.0.113.5", open.local_addr().unwrap().port() as i32).await;
    seed_node(&t, down, team, 2, "203.0.113.6", closed as i32).await;
    sqlx::query("update relay set public_host = '127.0.0.1'").execute(&t.st.db).await.unwrap();
    let ctl_ok = ctl.local_addr().unwrap().to_string();
    let ctl_bad = format!("127.0.0.1:{closed}");
    use fleet_server::reach::round;
    assert_eq!(round(&t.st, &[&ctl_bad, &ctl_ok], 0).await.unwrap(), 2, "没有在用入口的线路机不探");
    assert_eq!(round(&t.st, &[&ctl_bad], 1).await.unwrap(), 0, "对照目标都连不上：中控自己的问题，不记");
    // 线路机1 再挂一条入口（端口不通）：每轮换下一个端口，两个口轮流连到；线路机2 一直不通，两台同时不通也照记
    seed_node(&t, up, team, 3, "203.0.113.7", closed2 as i32).await;
    let unreachable = |v: &[fleet_server::views::MachineView]| v.iter().find(|m| m.id == down).unwrap().health.reasons.iter().any(|r| r.key == "relay_unreachable");
    assert!(!unreachable(&fleet_server::views::live(&t.st).await.unwrap()));
    assert_eq!(round(&t.st, &[&ctl_ok], 1).await.unwrap(), 2);
    assert_eq!(round(&t.st, &[&ctl_ok], 2).await.unwrap(), 2);
    // 缓存的视图（列表、告警评估）不等完整重算：结论变了，这一轮发了机器事件
    assert!(unreachable(&fleet_server::views::live(&t.st).await.unwrap()));
    let v = t.get("/api/v1/probe-beats").await;
    let sum = |id: Uuid| {
        v[id.to_string()].as_array().unwrap().iter().fold((0, 0), |(n, ok), b| (n + b[0].as_i64().unwrap(), ok + b[1].as_i64().unwrap()))
    };
    assert_eq!(sum(up), (3, 2), "{v}");
    assert_eq!(sum(down), (3, 0), "{v}");
    assert!(v.get(idle.to_string()).is_none(), "{v}");
    assert!(v[up.to_string()].as_array().unwrap().iter().all(|b| b[2].is_null() && b[3] == 0), "线路机只判通不通：{v}");
    let d: Vec<Option<String>> = sqlx::query_scalar("select detail from relay_reach where relay_id = $1 and not ok").bind(up).fetch_all(&t.st.db).await.unwrap();
    assert_eq!(d, [Some(format!("端口 {closed2}：连接被拒绝"))], "失败原因带上端口");
    // 线路机2 连续 3 轮连不上：严重告警；线路机1 只有一个口不通，不报
    let h = t.health(down).await;
    assert!(h.1.contains(&"relay_unreachable".to_string()), "{h:?}");
    assert!(!t.health(up).await.1.contains(&"relay_unreachable".to_string()));
    let m = t.get(&format!("/api/v1/machines/{down}")).await;
    let text = m["health"]["reasons"].as_array().unwrap().iter().find(|r| r["key"] == "relay_unreachable").unwrap()["text"].clone();
    assert_eq!(text, format!("中控连续 3 次以上连不上客户入口（端口 {closed}：连接被拒绝）"));
    // 再轮换两遍：线路机1 坏的那个口凑齐 3 次失败（中间隔着好口），按端口报，摘要只写这个口
    for turn in 3..7 {
        assert_eq!(round(&t.st, &[&ctl_ok], turn).await.unwrap(), 2);
    }
    let m = t.get(&format!("/api/v1/machines/{up}")).await;
    let text = m["health"]["reasons"].as_array().unwrap().iter().find(|r| r["key"] == "relay_unreachable").map(|r| r["text"].clone());
    assert_eq!(text, Some(json!(format!("中控连续 3 次以上连不上客户入口（端口 {closed2}：连接被拒绝）"))), "{m}");
    assert_eq!(t.count("select count(*) from relay_reach where port is null").await, 0, "每行记下端口");
    // 每条在用入口带上中控对它端口最近一次检查：通的 ok、detail 为 null；不通的带原因；线路机2 的入口一直不通
    let reach = |m: &Value, port: i32| m["entries"].as_array().unwrap().iter().find(|e| e["port"] == port).unwrap()["reach"].clone();
    let (good, bad) = (reach(&m, open.local_addr().unwrap().port() as i32), reach(&m, closed2 as i32));
    assert_eq!((good["ok"].clone(), good["detail"].clone()), (json!(true), Value::Null), "{m}");
    assert!(good["ts"].is_string(), "{m}");
    assert_eq!((bad["ok"].clone(), bad["detail"].clone()), (json!(false), json!(format!("端口 {closed2}：连接被拒绝"))), "{m}");
    // fails：这个端口到最近一次为止连续连不上几次（告警按连续 3 次）
    assert_eq!((good["fails"].clone(), bad["fails"].clone()), (json!(0), json!(3)), "{m}");
    let m = t.get(&format!("/api/v1/machines/{down}")).await;
    assert_eq!((reach(&m, closed as i32)["ok"].clone(), reach(&m, closed as i32)["fails"].clone()), (json!(false), json!(7)), "{m}");
    // 线路机上的入口表和节点那一侧是同一份；没检查过（线路机3 没有入口，新建的入口还没轮到）为 null
    let node = t.get(&format!("/api/v1/machines/{}", m["entries"][0]["node"]["id"].as_str().unwrap())).await;
    assert_eq!(node["entries"][0]["reach"]["ok"], json!(false), "{node}");
    // 整台连不上（最近 3 轮都失败，同告警）：这台上还没轮到的新入口也按「中控连不上」（次数至少 3），不等自己的端口凑够 3 次
    seed_node(&t, down, team, 4, "203.0.113.8", 29999).await;
    t.st.live.invalidate();
    let m = t.get(&format!("/api/v1/machines/{down}")).await;
    let fresh = reach(&m, 29999);
    assert_eq!((fresh["ok"].clone(), fresh["fails"].clone(), fresh["detail"].clone()), (json!(false), json!(3), json!(format!("端口 {closed}：连接被拒绝"))), "{m}");
    assert_eq!(reach(&m, closed as i32)["fails"], json!(7), "自己已经更多次的照旧");
    sqlx::query("delete from relay_reach where relay_id = $1").bind(down).execute(&t.st.db).await.unwrap();
    t.st.live.invalidate();
    let m = t.get(&format!("/api/v1/machines/{down}")).await;
    assert!(reach(&m, closed as i32).is_null(), "{m}");
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
    // 上面的重发在内存里就认出来了；中控重启过（内存里记的 seq 比库里的旧）时靠库里的 last_seq 去重，同样只确认、刷新最近上报时间
    sqlx::query("update agent_state set last_seq = 9, last_report_at = now() - interval '10 minutes' where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    let (s, v) = t.report("tok1", &blank_report(7)).await;
    assert_eq!((s, v["accepted_seq"].as_u64()), (StatusCode::OK, Some(9)), "{v}");
    let age: f64 = sqlx::query_scalar("select extract(epoch from now() - last_report_at)::float8 from agent_state where machine_id = $1").bind(relay).fetch_one(&t.st.db).await.unwrap();
    assert!(age < 5.0, "{age}");
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
    assert!(token_used_ago(&t, relay).await.is_some_and(|s| s >= 119.0), "鉴权结果有缓存（30 秒）：没查库");
    // 缓存过期（这里直接清掉）才再查库、刷新
    t.st.agent_auth.forget(relay);
    assert_eq!(t.report("tok1", &blank_report(4)).await.0, StatusCode::OK);
    assert!(token_used_ago(&t, relay).await.is_some_and(|s| s < 5.0));
    used("2 minutes").await;
    // 撤销令牌的地方清掉它的缓存（agent_auth.forget），马上 401
    sqlx::query("update agent_token set revoked_at = now() where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    t.st.agent_auth.forget(relay);
    assert_eq!(t.report("tok1", &blank_report(5)).await.0, StatusCode::UNAUTHORIZED);
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
        disk_write_error: Some("No space left on device (os error 28)".into()),
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
    assert_eq!(h["disk_write_error"], "No space left on device (os error 28)");
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

    // 关：连续 2 次探测成功就恢复，不等去抖时长；结果翻转、翻转后的确认探测，上报一入库就唤醒告警评估
    let woken = || async { tokio::time::timeout(Duration::ZERO, t.st.alerts_wake.notified()).await.is_ok() };
    woken().await;
    let ok = |seq: u64, at: i64| {
        let mut r = blank_report(seq);
        r.xray = Some(xray(true));
        r.probes = vec![probe(entry, at, ProbeKind::Egress, true), probe(entry, at, ProbeKind::Tcp, true)];
        r
    };
    assert_eq!(t.report("tok1", &ok(2, now_ms() - 1_000)).await.0, StatusCode::OK);
    assert!(woken().await, "探测结果翻转：马上评估");
    pass(&t, &mut state, base, 200).await;
    assert_eq!(rules(incidents(&t, "firing").await), ["egress_fail", "tcp_fail"], "只成功 1 次还不恢复");
    assert_eq!(t.report("tok1", &ok(3, now_ms())).await.0, StatusCode::OK);
    assert!(woken().await, "确认探测：马上评估");
    pass(&t, &mut state, base, 201).await;
    assert!(incidents(&t, "firing").await.is_empty(), "连续 2 次成功即恢复，不等 150 秒");
    // 结果没变、视图里已连续 3 次：不唤醒
    assert_eq!(t.report("tok1", &ok(4, now_ms())).await.0, StatusCode::OK);
    pass(&t, &mut state, base, 202).await;
    woken().await;
    assert_eq!(t.report("tok1", &ok(5, now_ms())).await.0, StatusCode::OK);
    assert!(!woken().await, "探测结果照旧：不用马上评估");
    // 翻转成失败要唤醒；之后连续失败（视图里才连续 1 次）不再唤醒，确认交给 30 秒一轮
    let bad = |seq: u64| {
        let mut r = blank_report(seq);
        r.xray = Some(xray(true));
        r.probes = vec![probe(entry, now_ms(), ProbeKind::Egress, false), probe(entry, now_ms(), ProbeKind::Tcp, false)];
        r
    };
    assert_eq!(t.report("tok1", &bad(6)).await.0, StatusCode::OK);
    assert!(woken().await, "翻转成失败：马上评估");
    pass(&t, &mut state, base, 203).await;
    assert_eq!(t.report("tok1", &bad(7)).await.0, StatusCode::OK);
    assert!(!woken().await, "连续失败：不唤醒");
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

/// 中控停机后恢复：停机这段不算 agent 的沉默——停之前在报的，恢复后头一两分钟里重发还没到也不算离线、不开告警；
/// 停之前就不报了的照样离线、照样开告警
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn central_outage_is_not_agent_silence() {
    let t = setup().await;
    let a = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let b = seed_relay(&t, "线路机2", "198.51.100.2", "tok2").await;
    let c = seed_relay(&t, "线路机3", "198.51.100.3", "tok3").await;
    // 中控 5 分钟前停的：c 停的那一刻还在报，a 停之前 20 秒报过最后一次，b 停之前一小时就不报了
    for (m, ago) in [(a, 320), (b, 3900), (c, 300)] {
        sqlx::query("insert into agent_state (machine_id, agent_version, last_report_at) values ($1, 'v', now() - make_interval(secs => $2))")
            .bind(m)
            .bind(ago as f64)
            .execute(&t.st.db)
            .await
            .unwrap();
    }
    let offline = |r: Vec<String>| r.contains(&"agent_offline".to_string());
    assert!(offline(t.health(a).await.1), "按墙钟算 a 5 分多钟没报");

    // 中控启动：停机这段收不到上报
    t.st.report_gaps.started(&t.st.db).await.unwrap();
    t.st.live.invalidate();
    assert!(!offline(t.health(a).await.1) && !offline(t.health(c).await.1));
    assert!(offline(t.health(b).await.1));
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 61).await;
    let inc = incidents(&t, "firing").await;
    assert_eq!(inc.iter().map(|i| (i["rule"].as_str().unwrap(), i["subject_id"].as_str().unwrap())).collect::<Vec<_>>(), [("agent_offline", b.to_string().as_str())]);

    // 恢复后不久又重启了一次（连着两次部署、崩溃重启）：c 在两次启动之间报过，库里最新的上报已是恢复之后的；
    // 前一段停机从库里接着算，a 的重发还没到照样不算离线，b 照样离线
    sqlx::query("update agent_state set last_report_at = now() where machine_id = $1").bind(c).execute(&t.st.db).await.unwrap();
    let again = fleet_server::report_gaps::ReportGaps::default();
    again.started(&t.st.db).await.unwrap();
    let now = OffsetDateTime::now_utc();
    assert!(again.online(now - time::Duration::seconds(330), now), "前一段停机仍不算 a 的沉默");
    assert!(!again.online(now - time::Duration::seconds(3900), now));
    // 一直没收到上报时连着重启：每次补记的起点相同，并成一段，不重复扣（否则 b 这种早就不报的也会被扣成在线）
    let third = fleet_server::report_gaps::ReportGaps::default();
    third.started(&t.st.db).await.unwrap();
    third.started(&t.st.db).await.unwrap();
    assert!(!third.online(now - time::Duration::seconds(3900), OffsetDateTime::now_utc()));
    assert!(!third.online(now - time::Duration::seconds(480), OffsetDateTime::now_utc()), "停机约 300 秒只扣一次");
}

/// 上一个进程一直在跑、只是全网都没上报（模拟器停了、边缘机挂了）：重启补记的中断从它自己记的存活时刻算，
/// 不从库里最新的上报算——一直不报的机器重启后照样离线；刚停的那一小段照样扣
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_gap_starts_at_central_alive_time() {
    let t = setup().await;
    let a = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    sqlx::query("insert into agent_state (machine_id, agent_version, last_report_at) values ($1, 'v', now() - interval '3 hours')").bind(a).execute(&t.st.db).await.unwrap();
    // 上一个进程 20 秒前还记了一次存活；之后 10 秒内再记不重写
    let alive = OffsetDateTime::now_utc() - time::Duration::seconds(20);
    let prev = fleet_server::report_gaps::ReportGaps::default();
    prev.alive(&t.st.db, alive).await.unwrap();
    prev.alive(&t.st.db, alive + time::Duration::seconds(10)).await.unwrap();
    let saved: String = sqlx::query_scalar("select value #>> '{}' from app_setting where key = 'central_alive_at'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(OffsetDateTime::parse(&saved, &time::format_description::well_known::Rfc3339).unwrap(), alive);

    t.st.report_gaps.started(&t.st.db).await.unwrap();
    t.st.live.invalidate();
    assert!(t.health(a).await.1.contains(&"agent_offline".to_string()), "3 小时没报：重启后照样离线");
    let now = OffsetDateTime::now_utc();
    assert!(t.st.report_gaps.online(now - time::Duration::seconds(130), now), "停机的约 20 秒不算沉默");
    assert!(!fleet_server::report_gaps::ReportGaps::default().online(now - time::Duration::seconds(130), now));
}

/// 有 agent 的在用机器同时离线超过一半且至少 3 台：不开单机的 agent 离线，开一条中控侧的「agent 同时失联」；
/// 大部分回来后那条恢复，还没回来的去抖之后再按单机报
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mass_agent_loss_opens_one_central_alert() {
    let t = setup().await;
    let a = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let b = seed_relay(&t, "线路机2", "198.51.100.2", "tok2").await;
    let c = seed_relay(&t, "线路机3", "198.51.100.3", "tok3").await;
    for m in [a, b, c] {
        sqlx::query("insert into agent_state (machine_id, agent_version, last_report_at) values ($1, 'v', now() - interval '10 minutes')").bind(m).execute(&t.st.db).await.unwrap();
    }
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    let firing = || async { incidents(&t, "firing").await.into_iter().map(|i| (i["rule"].as_str().unwrap().to_string(), i["summary"].as_str().unwrap().to_string())).collect::<Vec<_>>() };
    for secs in [0, 61, 121] {
        pass(&t, &mut state, base, secs).await;
        selfmon::check_once(&t.st, base + time::Duration::seconds(secs)).await.unwrap();
    }
    assert_eq!(firing().await, [("central_ingress".to_string(), "中控：3/3 台 agent 同时失联：检查边缘机、Caddy、域名解析".to_string())]);
    // a、b 回来了：中控侧的恢复；c 从这时起去抖，满 60 秒按单机报
    for m in [a, b] {
        sqlx::query("update agent_state set last_report_at = now() where machine_id = $1").bind(m).execute(&t.st.db).await.unwrap();
    }
    t.st.live.invalidate();
    selfmon::check_once(&t.st, base + time::Duration::seconds(150)).await.unwrap();
    pass(&t, &mut state, base, 150).await;
    assert!(firing().await.is_empty());
    pass(&t, &mut state, base, 211).await;
    let f = firing().await;
    assert_eq!(f.len(), 1);
    assert_eq!((f[0].0.as_str(), f[0].1.starts_with("线路机3：")), ("agent_offline", true), "{f:?}");
}

/// 维护中健康判定不给原因：进行中的告警不算恢复，保持原状，维护结束后按那时的状态再判
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintenance_does_not_resolve_open_alerts() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    xray_report(&t, 1, false).await;
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 61).await;
    assert!(has_rule(&incidents(&t, "firing").await, "relay_xray_down"));
    let f = time::format_description::well_known::Rfc3339;
    let now = OffsetDateTime::now_utc();
    let win = t.post("/api/v1/silences", json!({
        "subject_id": relay, "is_maintenance": true, "comment": "换机房",
        "starts_at": (now - time::Duration::minutes(1)).format(&f).unwrap(), "ends_at": (now + time::Duration::hours(1)).format(&f).unwrap(),
    })).await["id"].as_str().unwrap().to_string();
    for secs in [91, 151, 211] {
        pass(&t, &mut state, base, secs).await;
    }
    assert!(has_rule(&incidents(&t, "firing").await, "relay_xray_down"), "维护中不恢复");
    assert!(incidents(&t, "resolved").await.is_empty());
    // 维护结束、Xray 已经好了：原因消失满 60 秒恢复
    let (s, _) = t.req("DELETE", &format!("/api/v1/silences/{win}"), None, None).await;
    assert_eq!(s, StatusCode::OK);
    xray_report(&t, 2, true).await;
    pass(&t, &mut state, base, 241).await;
    pass(&t, &mut state, base, 302).await;
    assert!(incidents(&t, "firing").await.is_empty());
}

/// 线路机维护中：经它探测的节点出口 / TCP 结果判不出——开着的不因此恢复，节点状态是「未知」
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_maintenance_freezes_node_probe_alerts() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let mut r = blank_report(1);
    r.xray = Some(xray(true));
    r.probes = vec![probe(entry, now_ms() - 2_000, ProbeKind::Egress, true), probe(entry, now_ms() - 2_000, ProbeKind::Tcp, false)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 91).await;
    assert!(has_rule(&incidents(&t, "firing").await, "tcp_fail"));
    let f = time::format_description::well_known::Rfc3339;
    let now = OffsetDateTime::now_utc();
    t.post("/api/v1/silences", json!({
        "subject_id": relay, "is_maintenance": true, "comment": "换内核",
        "starts_at": (now - time::Duration::minutes(1)).format(&f).unwrap(), "ends_at": (now + time::Duration::hours(1)).format(&f).unwrap(),
    })).await;
    for secs in [121, 181, 241] {
        pass(&t, &mut state, base, secs).await;
    }
    assert!(has_rule(&incidents(&t, "firing").await, "tcp_fail"), "线路机维护中：节点的 TCP 告警不恢复");
    assert!(incidents(&t, "resolved").await.is_empty());
    assert_eq!(t.health(node).await.0, "unknown");
}

/// 节点两条入口：线路机2 那条 TCP 不通开了告警；之后线路机1 进维护、线路机2 那条恢复了——告警照常恢复
/// （只看当初失败的那条入口判不判得出，另一条入口的线路机在维护不挡）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_relay_maintenance_does_not_hold_node_probe_alerts() {
    let t = setup().await;
    let r1 = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let r2 = seed_relay(&t, "线路机2", "198.51.100.2", "tok2").await;
    let team = seed_team(&t, "团队1").await;
    let (node, e1) = seed_node(&t, r1, team, 1, "203.0.113.5", 20000).await;
    let e2 = Uuid::now_v7();
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, 20001, $4, 'abcd', 'active')")
        .bind(e2)
        .bind(r2)
        .bind(node)
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();
    let report = |tok: &'static str, seq: u64, e: Uuid, tcp_ok: bool| {
        let t = &t;
        async move {
            let mut r = blank_report(seq);
            r.xray = Some(xray(true));
            r.probes = vec![probe(e, now_ms() - 2_000, ProbeKind::Egress, true), probe(e, now_ms() - 2_000, ProbeKind::Tcp, tcp_ok)];
            assert_eq!(t.report(tok, &r).await.0, StatusCode::OK);
        }
    };
    report("tok1", 1, e1, true).await;
    report("tok2", 1, e2, false).await;
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 91).await;
    let firing = incidents(&t, "firing").await;
    assert!(firing.iter().any(|i| i["rule"] == "tcp_fail" && i["summary"].as_str().unwrap().contains("线路机2 到节点 TCP 不通")), "{firing:?}");
    let f = time::format_description::well_known::Rfc3339;
    let now = OffsetDateTime::now_utc();
    t.post("/api/v1/silences", json!({
        "subject_id": r1, "is_maintenance": true, "comment": "换内核",
        "starts_at": (now - time::Duration::minutes(1)).format(&f).unwrap(), "ends_at": (now + time::Duration::hours(4)).format(&f).unwrap(),
    })).await;
    report("tok2", 2, e2, true).await;
    for secs in [121, 181, 241, 301] {
        pass(&t, &mut state, base, secs).await;
    }
    assert!(!has_rule(&incidents(&t, "firing").await, "tcp_fail"), "线路机1 维护不挡线路机2 那条的恢复");
    assert!(has_rule(&incidents(&t, "resolved").await, "tcp_fail"));
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
    // 到期天数按账单时区（这里跟随组织时区）的今天算：到期日也按它取，不用库会话的 current_date（UTC 傍晚以后差一天）
    sqlx::query("insert into subscription (machine_id, price_cents, due_date, remind_days) values ($1, 500, (now() at time zone org_tz())::date + 3, 7)").bind(relay).execute(&t.st.db).await.unwrap();
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

/// 把机器网卡在 `hour` 这一桶的出站设成 `mb` MB（十进制）、入站设成它的一半，并让视图里的额度缓存作废（同小时汇总跑完）
async fn set_nic(t: &T, machine: Uuid, hour: OffsetDateTime, mb: i64) {
    sqlx::query(
        "insert into host_metric_hourly (machine_id, hour, net_rx, net_tx) values ($1, $2, $3 / 2, $3)
         on conflict (machine_id, hour) do update set net_rx = excluded.net_rx, net_tx = excluded.net_tx",
    )
    .bind(machine)
    .bind(hour)
    .bind(mb * 1_000_000)
    .execute(&t.st.db)
    .await
    .unwrap();
    t.st.live.rolled_up();
}

/// 流量额度：本周期已用 ≥ 90% 立即开警告，≥ 100% 升严重并马上再通知一次，回落（新周期已用归零）立即恢复；
/// 只开告警，健康状态不变；没设上限的机器不评估。已用按网卡（线路机没有供应商，规则取大），上限是十进制 GB
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn traffic_quota_alert_lifecycle() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, _) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    xray_report(&t, 1, true).await;
    // 上个月就上架了：本周期数据完整，摘要不带「从 X 开始统计」
    sqlx::query("update machine set activated_at = now() - interval '40 days' where id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    // 线路机额度 1 GB、重置日定在 15 天后的那个日号（账单时区北京，周期起点在 13 天以前）；节点没有订阅也没有套餐：不限
    sqlx::query(
        "insert into subscription (machine_id, price_cents, due_date, traffic_gb, traffic_reset_day)
         values ($1, 500, current_date + 60, 1, extract(day from (now() at time zone 'Asia/Shanghai')::date + 15))",
    )
        .bind(relay)
        .execute(&t.st.db)
        .await
        .unwrap();
    // 周期里 10 天前的一小时用掉这些流量：在近 7 天窗口之外，日均为 0，不触发「预计用满」，下面只看 90% 的原逻辑
    let (hour, recent, days): (OffsetDateTime, OffsetDateTime, i32) =
        sqlx::query_as("select date_trunc('hour', now() - interval '10 days'), date_trunc('hour', now() - interval '2 days'), 15").fetch_one(&t.st.db).await.unwrap();
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();

    set_nic(&t, relay, hour, 899).await;
    assert_eq!(t.health(relay).await, ("ok".to_string(), vec![]), "89% 不报");
    // 预计用满：早先 500 MB，近 7 天窗口里（2 天前）又用 280 MB → 日均约 40 MB，剩 220 MB 约 5.5 天用满，早于重置日（15 天后）：
    // 还没到 90%（78%）也开警告，摘要写预计日期；quota 带日均与预计用满时刻
    set_nic(&t, relay, hour, 500).await;
    set_nic(&t, relay, recent, 280).await;
    let m = t.get(&format!("/api/v1/machines/{relay}")).await;
    let (daily, full) = (m["quota"]["daily_avg"].as_i64().unwrap(), OffsetDateTime::parse(m["quota"]["forecast_full_at"].as_str().unwrap(), &time::format_description::well_known::Rfc3339).unwrap());
    assert!((39_000_000..42_000_000).contains(&daily), "{}", m["quota"]);
    let left = (full - OffsetDateTime::now_utc()).as_seconds_f64() / 86400.0;
    assert!((4.5..6.5).contains(&left), "{left} {}", m["quota"]);
    let text = m["health"]["reasons"][0]["text"].as_str().unwrap_or_default();
    assert!(text.starts_with("本周期流量已用 78%（0.78 GB / 1 GB），按近 7 天的速度预计 ") && text.ends_with(&format!(" 用满，{days} 天后重置")), "{text}");
    sqlx::query("delete from host_metric_hourly where hour = $1").bind(recent).execute(&t.st.db).await.unwrap();
    t.st.live.rolled_up();
    set_nic(&t, relay, hour, 899).await;
    // 92%：不去抖，评估一轮就开警告；健康状态仍是正常；节点同样有这些流量，但不限额度，不评估
    set_nic(&t, relay, hour, 920).await;
    set_nic(&t, node, hour, 920).await;
    assert_eq!(t.health(relay).await, ("ok".to_string(), vec!["traffic_quota_high".to_string()]));
    assert!(t.health(node).await.1.is_empty());
    pass(&t, &mut state, base, 0).await;
    let inc = incidents(&t, "firing").await;
    assert_eq!(inc.len(), 1, "{inc:?}");
    assert_eq!((inc[0]["rule"].as_str(), inc[0]["rule_name"].as_str(), inc[0]["severity"].as_str()), (Some("traffic_quota_high"), Some("流量额度吃紧"), Some("warning")));
    assert_eq!(inc[0]["subject_id"], relay.to_string());
    assert_eq!(inc[0]["summary"], format!("线路机1：本周期流量已用 92%（0.92 GB / 1 GB），{days} 天后重置"));
    log_settles(&t, 1).await;
    t.post(&format!("/api/v1/incidents/{}/ack", inc[0]["id"].as_str().unwrap()), json!({})).await;

    // 超过 100%：同一条告警升为严重，马上再通知一次（不等每天一次的提醒），对「吃紧」的确认清掉；健康状态仍不变
    set_nic(&t, relay, hour, 1070).await;
    pass(&t, &mut state, base, 30).await;
    let inc = incidents(&t, "firing").await;
    assert_eq!((inc.len(), inc[0]["severity"].as_str()), (1, Some("critical")));
    assert!(inc[0]["ack_at"].is_null(), "{inc:?}");
    assert_eq!(inc[0]["summary"], format!("线路机1：本周期流量已超出额度（已用 107%，1.07 GB / 1 GB），{days} 天后重置"));
    assert_eq!(t.health(relay).await.0, "ok");
    // 超额度时告警照样建议 rebalance，rebalance 也按超额度的口径回答（这里近 7 天没有新流量：不用搬）
    assert_eq!(inc[0]["suggested"], json!(["fleetctl rebalance 线路机1"]));
    let rb = t.get(&format!("/api/v1/relays/{relay}/rebalance")).await;
    assert_eq!((rb["over_limit"].as_bool(), rb["needed"].as_bool()), (Some(true), Some(false)), "{rb}");
    assert!(rb["reason"].as_str().unwrap_or_default().contains("近 7 天没有新增流量"), "{rb}");
    log_settles(&t, 2).await;
    pass(&t, &mut state, base, 60).await;
    log_settles(&t, 2).await;

    // 上限调到 2 GB、回到吃紧：摘要跟着变，严重度不降（收到过「严重」的渠道恢复时按严重收到），也不再通知
    sqlx::query("update subscription set traffic_gb = 2 where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    set_nic(&t, relay, hour, 1840).await;
    pass(&t, &mut state, base, 75).await;
    let inc = incidents(&t, "firing").await;
    assert_eq!((inc.len(), inc[0]["severity"].as_str()), (1, Some("critical")));
    assert_eq!(inc[0]["summary"], format!("线路机1：本周期流量已用 92%（1.84 GB / 2 GB），{days} 天后重置"));
    log_settles(&t, 2).await;

    // 新周期开始、已用归零：下一轮立即恢复
    sqlx::query("delete from host_metric_hourly").execute(&t.st.db).await.unwrap();
    t.st.live.rolled_up();
    pass(&t, &mut state, base, 90).await;
    assert!(incidents(&t, "firing").await.is_empty());
    log_settles(&t, 3).await;
    let sent: Vec<(&str, String)> = mock.events("/hook/a").into_iter().zip(mock.texts("/hook/a")).map(|(e, x)| (e, x.chars().take(4).collect())).collect();
    let want = [("firing", "【警告】"), ("firing", "【严重】"), ("resolved", "【严重】")].map(|(e, s)| (e, s.to_string()));
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
        json!({"name": "x", "kind": "wecom", "config": {"url": "http://x.test"}}),
        json!({"name": "x", "kind": "feishu", "config": {"url": "ftp://x"}}),
        json!({"name": "x", "kind": "feishu", "config": {"url": "http://x.test"}, "min_severity": "fatal"}),
        json!({"name": " ", "kind": "feishu", "config": {"url": "http://x.test"}}),
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

    // 测试接口：不可达地址返回 ok:false 而不是 500；可达返回 ok:true；对方拒绝（code≠0）也是 ok:false
    let dead = t.post("/api/v1/notify-channels", json!({"name": "死链接", "kind": "feishu", "config": {"url": "http://127.0.0.1:9/hook"}, "min_severity": "info", "enabled": true})).await["id"].as_str().unwrap().to_string();
    let (s, v) = t.req("POST", &format!("/api/v1/notify-channels/{dead}/test"), None, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["ok"], false);
    assert!(v["data"]["error"].as_str().is_some_and(|e| !e.is_empty()), "{v}");
    let good = t.post("/api/v1/notify-channels", json!({"name": "好 hook", "kind": "feishu", "config": {"url": mock.url("/hook/ok")}, "min_severity": "info", "enabled": false})).await["id"].as_str().unwrap().to_string();
    let r = t.post(&format!("/api/v1/notify-channels/{good}/test"), json!({})).await;
    assert_eq!((r["ok"].as_bool(), r["error"].is_null()), (Some(true), true), "停用的渠道也能测试：{r}");
    assert_eq!(mock.events("/hook/ok"), ["test"]);
    // 飞书签名密钥：缺省 / 留空沿用，`secret: null` 才清除
    let hint = || async { t.get("/api/v1/notify-channels").await.as_array().unwrap().iter().find(|c| c["name"] == "好 hook").unwrap()["config_hint"].as_str().unwrap().to_string() };
    let tr = &t;
    let patch = |cfg: serde_json::Value| {
        let path = format!("/api/v1/notify-channels/{good}");
        async move { tr.req("PATCH", &path, Some(json!({ "config": cfg })), None).await.0 }
    };
    assert_eq!(patch(json!({"secret": "abc"})).await, StatusCode::OK);
    assert!(hint().await.ends_with("已加签"));
    assert_eq!(patch(json!({"secret": ""})).await, StatusCode::OK);
    assert!(hint().await.ends_with("已加签"), "留空沿用");
    assert_eq!(patch(json!({"secret": null})).await, StatusCode::OK);
    assert!(!hint().await.contains("已加签"), "null 清除");
    let wc = t.post("/api/v1/notify-channels", json!({"name": "飞书拒绝", "kind": "feishu", "config": {"url": mock.url("/feishu/bad")}, "min_severity": "info", "enabled": true})).await["id"].as_str().unwrap().to_string();
    let r = t.post(&format!("/api/v1/notify-channels/{wc}/test"), json!({})).await;
    assert_eq!(r["ok"], false);
    assert!(r["error"].as_str().unwrap().contains("Key Words Not Found"), "{r}");
    assert_eq!(mock.bodies("/feishu/bad")[0]["msg_type"], "post");
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

/// 默认（不允许内网）：字面内网 IP 建渠道时就拒；域名解析到内网的发送时拒，发送记录里只有原因
#[tokio::test]
async fn notify_refuses_private_destinations() {
    let t = common::boot(false, "").await;
    // 飞书只许飞书自己的 https 主机
    for url in ["http://127.0.0.1:9/hook", "https://10.1.2.3/x", "https://evil.example.com/open-apis/bot/v2/hook/k", "http://open.feishu.cn/open-apis/bot/v2/hook/k", "https://open.feishu.cn.evil.com/x"] {
        let (s, v) = t.req("POST", "/api/v1/notify-channels", Some(json!({"name": "x", "kind": "feishu", "config": {"url": url}})), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{url} → {v}");
    }
    let (s, v) = t.req("POST", "/api/v1/notify-channels", Some(json!({"name": "x", "kind": "feishu", "config": {"url": "https://open.feishu.cn/open-apis/bot/v2/hook/k", "secret": "s"}})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    for base in ["http://192.168.0.1", "http://169.254.169.254/latest", "http://[::1]:8080/", "http://[::ffff:192.168.1.1]/", "http://100.64.0.1/", "http://0x7f.1/"] {
        let (s, v) = t.req("POST", "/api/v1/notify-channels", Some(json!({"name": "x", "kind": "telegram", "config": {"bot_token": "1:a", "chat_id": 1, "api_base": base}})), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{base} → {v}");
    }
    let id = t.post("/api/v1/notify-channels", json!({"name": "本机", "kind": "telegram", "config": {"bot_token": "1:a", "chat_id": 1, "api_base": "http://localhost:9"}})).await["id"].as_str().unwrap().to_string();
    let r = t.post(&format!("/api/v1/notify-channels/{id}/test"), json!({})).await;
    assert_eq!(r["ok"], false);
    assert!(r["error"].as_str().unwrap().contains("内网"), "{r}");
    let (s, _) = t.req("PATCH", &format!("/api/v1/notify-channels/{id}"), Some(json!({"config": {"bot_token": "2:b", "api_base": "http://127.0.0.1/x"}})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alert_notifications_firing_repeat_silence_and_resolved() {
    let t = setup().await;
    let mock = Mock::start().await;
    let mk = |name: &str, kind: &str, config: Value, sev: &str, enabled: bool| json!({"name": name, "kind": kind, "config": config, "min_severity": sev, "enabled": enabled});
    t.post("/api/v1/notify-channels", mk("tg", "telegram", json!({"bot_token": "123:ABC", "chat_id": 42, "api_base": mock.url("")}), "info", true)).await;
    t.post("/api/v1/notify-channels", mk("wc", "feishu", json!({"url": mock.url("/feishu/send"), "secret": "sec"}), "critical", true)).await;
    t.post("/api/v1/notify-channels", mk("wh", "feishu", json!({"url": mock.url("/hook/a")}), "warning", true)).await;
    t.post("/api/v1/notify-channels", mk("off", "feishu", json!({"url": mock.url("/disabled/x")}), "info", false)).await;
    t.post("/api/v1/notify-channels", mk("broken", "feishu", json!({"url": mock.url("/fail/x")}), "info", true)).await;

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
    mock.wait("/feishu/send", 1).await;
    mock.wait("/hook/a", 2).await;
    mock.wait("/fail/x", 2).await;
    // 坏渠道一直 500：第一条按 0.5 s、2 s、10 s… 退避重发（入队满 30 分钟才放弃），第二条排在它后面；别的渠道照常发完
    log_settles(&t, 5).await;
    assert_eq!((mock.count("/bot123:ABC"), mock.count("/feishu/send"), mock.count("/hook/a"), mock.count("/disabled")), (2, 1, 2, 0));
    let tg: Vec<String> = mock.bodies("/bot123:ABC").iter().map(|b| b["text"].as_str().unwrap().to_string()).collect();
    let crit = tg.iter().find(|x| x.contains("Xray 未运行")).expect("严重告警的 Telegram 消息");
    assert!(crit.contains("【严重】告警触发") && crit.contains("机器：线路机1") && crit.contains("规则：Xray 未运行") && crit.contains("（北京时间）"), "{crit}");
    assert_eq!(mock.bodies("/bot123:ABC")[0]["chat_id"], "42");
    let wc = &mock.bodies("/feishu/send")[0];
    assert_eq!((wc["msg_type"].as_str(), wc["sign"].is_string(), wc["timestamp"].is_string()), (Some("post"), true, true), "配了密钥的带签名");
    assert!(mock.texts("/feishu/send")[0].contains("线路机1"));
    assert!(mock.bodies("/hook/a").iter().all(|b| b.get("sign").is_none()));
    let wh = mock.texts("/hook/a");
    let wh_crit = wh.iter().find(|x| x.contains("规则：Xray 未运行")).unwrap();
    assert!(wh_crit.starts_with("【严重】告警触发\n机器：线路机1\n"), "{wh_crit}");
    assert!(wh.iter().any(|x| x.starts_with("【警告】告警触发\n") && x.contains("规则：配置未应用")));
    // 记录：发出去的都记成功；坏渠道的两条还在库里排着重试（不记），渠道记着连续失败与原因。一个渠道失败不影响别的
    let log = t.get("/api/v1/notify-log?limit=100").await;
    let log = log.as_array().unwrap();
    assert!(log.iter().all(|l| l["ok"] == true && l["channel_name"] != "broken"), "{log:?}");
    let pending = t.count("select count(*) from notify_outbox o join notify_channel c on c.id = o.channel_id where c.name = 'broken'").await;
    let (fails, err): (i32, String) = sqlx::query_as("select fail_count, last_error from notify_channel where name = 'broken'").fetch_one(&t.st.db).await.unwrap();
    assert!(pending == 2 && fails >= 2 && err.contains("500"), "{pending} {fails} {err}");
    assert!(log.iter().all(|l| l["incident_id"].is_string() && l["kind"] == "firing" && l["subject_name"] == "线路机1"));
    assert!(log.iter().any(|l| l["rule_name"] == "Xray 未运行") && log.iter().any(|l| l["rule_name"] == "配置未应用"), "{log:?}");
    assert!(log.iter().all(|l| l.get("rule").is_none()));

    // 持续提醒：严重告警 4 小时后再提醒一次（警告不重复）；之后同一周期内不再发
    sqlx::query("update incident set notified_at = now() - interval '5 hours'").execute(&t.st.db).await.unwrap();
    pass(&t, &mut state, base, 91).await;
    mock.wait("/bot123:ABC", 3).await;
    mock.wait("/feishu/send", 2).await;
    mock.wait("/hook/a", 3).await;
    pass(&t, &mut state, base, 121).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!((mock.count("/bot123:ABC"), mock.count("/feishu/send"), mock.count("/hook/a")), (3, 2, 3), "提醒后计时重置");
    assert!(mock.texts("/hook/a").iter().any(|x| x.starts_with("【严重】告警仍未恢复") && x.contains("规则：Xray 未运行")));

    // 静默：被静默的规则不再提醒。建、删此刻生效的静默本身各通知所有启用的渠道一次（不看渠道级别）
    let f = time::format_description::well_known::Rfc3339;
    let now = OffsetDateTime::now_utc();
    let sil = t.post("/api/v1/silences", json!({"subject_id": relay, "rule": "relay_xray_down", "comment": "", "starts_at": (now - time::Duration::minutes(1)).format(&f).unwrap(), "ends_at": (now + time::Duration::hours(1)).format(&f).unwrap()})).await["id"].as_str().unwrap().to_string();
    mock.wait("/bot123:ABC", 4).await;
    mock.wait("/feishu/send", 3).await;
    mock.wait("/hook/a", 4).await;
    assert!(mock.texts("/feishu/send")[2].contains("静默 / 维护变更"));
    sqlx::query("update incident set notified_at = now() - interval '5 hours'").execute(&t.st.db).await.unwrap();
    pass(&t, &mut state, base, 151).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!((mock.count("/bot123:ABC"), mock.count("/feishu/send")), (4, 3), "被静默的不发");
    let (s, _) = t.req("DELETE", &format!("/api/v1/silences/{sil}"), None, None).await;
    assert_eq!(s, StatusCode::OK);
    mock.wait("/bot123:ABC", 5).await;
    mock.wait("/feishu/send", 4).await;
    mock.wait("/hook/a", 5).await;

    // 恢复：Xray 起来、配置也应用了；原因消失满 60 秒后各发一条「恢复」（只发给收到过触发的渠道）
    let mut r = blank_report(2);
    r.xray = Some(xray(true));
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    sqlx::query("update relay set applied_generation = 3, applied_at = now() where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    pass(&t, &mut state, base, 181).await;
    assert_eq!(incidents(&t, "firing").await.len(), 2);
    pass(&t, &mut state, base, 241).await;
    assert!(incidents(&t, "firing").await.is_empty());
    mock.wait("/feishu/send", 5).await;
    mock.wait("/bot123:ABC", 7).await;
    mock.wait("/hook/a", 7).await;
    let last_wc = mock.texts("/feishu/send").pop().unwrap();
    assert!(last_wc.contains("告警已恢复"), "{last_wc}");
    assert!(mock.texts("/hook/a").iter().any(|x| x.contains("告警已恢复") && x.contains("规则：Xray 未运行")));

    // 机器退役后，记录里的对象名带「（已退役）」，不和接过同名的新机器混淆
    sqlx::query("update machine set lifecycle = 'retired', retired_at = now() where id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    let log = t.get("/api/v1/notify-log?limit=100").await;
    assert!(log.as_array().unwrap().iter().filter(|l| l["kind"] != "silence").all(|l| l["subject_name"] == "线路机1（已退役）"), "{log}");
}

/// 中控重启 / 部署：排着没发的通知在库里，新进程起来接着发；渠道一时发不出去（前两次 500）就按 0.5 秒、2 秒…退避重发到送达，
/// 只记一条成功、渠道的连续失败随之清零；入队满 30 分钟还没发出的放弃，记一条失败
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_notices_survive_restart_and_retry_until_delivered() {
    let t = setup().await;
    let mock = Mock::start().await;
    for (name, path) in [("a", "/flaky/a"), ("b", "/hook/b")] {
        t.post("/api/v1/notify-channels", json!({"name": name, "kind": "feishu", "config": {"url": mock.url(path)}, "min_severity": "info", "enabled": true})).await;
    }
    seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    // 「旧进程」入队后还没来得及发就退出了：不起发送循环，通知只在库里
    t.st.notify_started.store(true, Ordering::Relaxed);
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    xray_report(&t, 1, false).await;
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 61).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!((mock.count("/hook"), t.count("select count(*) from notify_outbox").await), (0, 2));
    // 发给 b 的那条已经排了 31 分钟
    sqlx::query("update notify_outbox set created_at = now() - interval '31 minutes' where channel_id = (select id from notify_channel where name = 'b')")
        .execute(&t.st.db)
        .await
        .unwrap();
    // 「新进程」：同一个库、新的内存状态，起发送循环
    let st2 = AppState::new(t.st.cfg.clone(), t.st.db.clone(), SecretBox::from_key(&[9u8; 32]).unwrap());
    fleet_server::notify::start(&st2);
    mock.wait("/flaky/a", 3).await;
    log_settles(&t, 2).await;
    assert_eq!((mock.events("/flaky/a")[0], mock.count("/hook/b")), ("firing", 0));
    assert_eq!(t.count("select count(*) from notify_log where ok and kind = 'firing'").await, 1);
    let (fails, since): (i32, Option<OffsetDateTime>) = sqlx::query_as("select fail_count, failing_since from notify_channel where name = 'a'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!((fails, since), (0, None));
    let (ok, err): (bool, Option<String>) =
        sqlx::query_as("select ok, error from notify_log where channel_id = (select id from notify_channel where name = 'b')").fetch_one(&t.st.db).await.unwrap();
    assert!(!ok && err.as_deref().is_some_and(|e| e.contains("30 分钟还没发出，放弃")), "{err:?}");
    assert_eq!(t.count("select count(*) from notify_outbox").await, 0);
}

/// 某个渠道没收到这条告警的「触发」（被拒了），恢复时也不给它发「恢复」——哪怕渠道这时已经修好；收到过触发的照发
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolved_only_to_channels_that_got_firing() {
    let t = setup().await;
    let mock = Mock::start().await;
    let wc = t.post("/api/v1/notify-channels", json!({"name": "飞书", "kind": "feishu", "config": {"url": mock.url("/feishu/bad")}, "min_severity": "info", "enabled": true})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    xray_report(&t, 1, false).await;
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 61).await;
    // 飞书拒绝（code≠0，重试也没用）：记一条失败；另一个渠道收到触发
    log_settles(&t, 2).await;
    assert_eq!(t.count("select count(*) from notify_log where not ok").await, 1);
    // 改好了地址，之后告警恢复
    let (s, v) = t.req("PATCH", &format!("/api/v1/notify-channels/{wc}"), Some(json!({"config": {"url": mock.url("/feishu/ok")}})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    xray_report(&t, 2, true).await;
    pass(&t, &mut state, base, 91).await;
    pass(&t, &mut state, base, 151).await;
    assert!(incidents(&t, "firing").await.is_empty());
    log_settles(&t, 3).await;
    assert_eq!(mock.count("/feishu/ok"), 0, "没收到触发的渠道不发恢复");
    assert_eq!(mock.events("/hook/a"), ["firing", "resolved"]);
    assert_eq!(t.count("select count(*) from notify_outbox").await, 0);
}

/// 新建、删除静默 / 维护窗口告诉所有启用的渠道（不看渠道的最低级别、不受静默影响）：全部机器的建、删都发；单台的此刻生效才发，预约的不发。
/// API 密钥建全部机器的最长 24 小时，账号不受限
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn silence_changes_are_announced() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "critical", "enabled": true})).await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let key = t.post("/api/v1/api-keys", json!({"name": "ops", "scopes": ["read", "write"], "password": common::ADMIN_PW})).await["token"].as_str().unwrap().to_string();
    let f = time::format_description::well_known::Rfc3339;
    let now = OffsetDateTime::now_utc();
    let at = |min: i64| (now + time::Duration::minutes(min)).format(&f).unwrap();
    let global = |end_min: i64| json!({"subject_id": null, "rule": null, "is_maintenance": true, "comment": "机房迁移\n预计一天", "starts_at": at(-1), "ends_at": at(end_min)});
    let (s, v) = t.req("POST", "/api/v1/silences", Some(global(25 * 60)), Some(&key)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("24 小时"), "{v}");
    let (s, v) = t.req("POST", "/api/v1/silences", Some(global(23 * 60)), Some(&key)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    mock.wait("/hook/a", 1).await;
    assert_eq!(mock.events("/hook/a"), ["silence"]);
    let text = &mock.texts("/hook/a")[0];
    assert!(text.starts_with("【静默 / 维护变更】apikey:ops 新建了维护窗口\n对象：全部机器\n起止：") && text.contains("\n备注：机房迁移 预计一天\n"), "{text}");
    // 删掉全部机器的：照样发
    let (s, _) = t.req("DELETE", &format!("/api/v1/silences/{}", v["data"]["id"].as_str().unwrap()), None, None).await;
    assert_eq!(s, StatusCode::OK);
    mock.wait("/hook/a", 2).await;
    assert!(mock.texts("/hook/a")[1].contains("admin 删除了维护窗口"));
    // 单台：预约的（还没开始）建、删都不发；此刻生效的发
    let one = |start: i64, end: i64| json!({"subject_id": relay, "rule": "relay_xray_down", "comment": "", "starts_at": at(start), "ends_at": at(end)});
    let later = t.post("/api/v1/silences", one(60, 120)).await["id"].as_str().unwrap().to_string();
    let (s, _) = t.req("DELETE", &format!("/api/v1/silences/{later}"), None, None).await;
    assert_eq!(s, StatusCode::OK);
    t.post("/api/v1/silences", one(-1, 60)).await;
    mock.wait("/hook/a", 3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let all = mock.texts("/hook/a");
    assert_eq!(all.len(), 3);
    assert!(all[2].contains("admin 新建了静默\n对象：线路机1（只静默「Xray 未运行」）"), "{}", all[2]);
    // 账号建全部机器的不受 24 小时限制；静默期间这类通知照发
    t.post("/api/v1/silences", global(3 * 24 * 60)).await;
    mock.wait("/hook/a", 4).await;
    let log = t.get("/api/v1/notify-log?limit=10").await;
    assert!(log.as_array().unwrap().iter().all(|l| l["kind"] == "silence" && l["ok"] == true && l["subject_name"] == "静默 / 维护变更"), "{log}");
}

// ── 中控自监控 ──────────────────────────────────────────────────────────────

/// 通知渠道连续发送失败满 5 分钟：开中控自身的告警（对象「中控」，经别的渠道发、不发给它自己）；渠道发成功一次（这里是渠道测试）就恢复
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_channel_opens_central_alert() {
    let t = setup().await;
    let mock = Mock::start().await;
    let broken = t.post("/api/v1/notify-channels", json!({"name": "坏渠道", "kind": "feishu", "config": {"url": mock.url("/fail/x")}, "min_severity": "info", "enabled": true})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let mut state = AlertState::default();
    let base = OffsetDateTime::now_utc();
    xray_report(&t, 1, false).await;
    pass(&t, &mut state, base, 0).await;
    pass(&t, &mut state, base, 61).await;
    mock.wait("/fail/x", 2).await;
    mock.wait("/hook/a", 1).await;
    let central = || async { incidents(&t, "firing").await.into_iter().filter(|i| i["rule"] == "central_channel_failing").collect::<Vec<_>>() };
    // 失败还不满 5 分钟：不开
    selfmon::check_once(&t.st, OffsetDateTime::now_utc()).await.unwrap();
    assert!(central().await.is_empty());
    selfmon::check_once(&t.st, OffsetDateTime::now_utc() + time::Duration::minutes(6)).await.unwrap();
    let inc = central().await;
    assert_eq!(inc.len(), 1, "{inc:?}");
    assert_eq!(
        (inc[0]["severity"].as_str(), inc[0]["subject"].is_null(), inc[0]["subject_id"].is_null(), inc[0]["subject_name"].as_str(), inc[0]["rule_name"].as_str()),
        (Some("critical"), true, true, Some("中控"), Some("通知渠道发送失败"))
    );
    let summary = inc[0]["summary"].as_str().unwrap();
    assert!(summary.starts_with("中控：通知渠道「坏渠道」连续发送失败超过 5 分钟：飞书拒绝（HTTP 500）"), "{summary}");
    mock.wait("/hook/a", 2).await;
    let b = mock.texts("/hook/a").pop().unwrap();
    assert!(b.starts_with("【严重】告警触发\n机器：中控\n规则：通知渠道发送失败\n"), "{b}");
    let to_itself = format!("select count(*) from notify_outbox o join incident i on i.id = o.incident_id where o.channel_id = '{broken}' and i.rule = 'central_channel_failing'");
    assert_eq!(t.count(&to_itself).await, 0, "不发给失败的渠道自己");
    // 渠道修好、测试发成功：下一轮恢复，「恢复」发给收到过触发的渠道
    let (s, v) = t.req("PATCH", &format!("/api/v1/notify-channels/{broken}"), Some(json!({"config": {"url": mock.url("/hook/fixed")}})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(t.post(&format!("/api/v1/notify-channels/{broken}/test"), json!({})).await["ok"], true);
    selfmon::check_once(&t.st, OffsetDateTime::now_utc() + time::Duration::minutes(7)).await.unwrap();
    assert!(central().await.is_empty());
    mock.wait("/hook/a", 3).await;
    let b = mock.texts("/hook/a").pop().unwrap();
    assert!(b.contains("告警已恢复") && b.contains("规则：通知渠道发送失败"), "{b}");
    assert_eq!(t.count(&to_itself).await, 0);
}

/// 备份目录里最新一份超过 26 小时（或一份都没有）：开中控自身的告警，有了新的一份就恢复；目录不存在（没装备份）不检查
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_backup_opens_central_alert() {
    let dir = tempfile::tempdir().unwrap();
    let bk = dir.path().join("backups");
    let t = common::boot_in(dir, false, &format!("notify.allow_private = true\nselfmon.backup_dir = \"{}\"", bk.display())).await;
    let backup = |state: &'static str| {
        let t = &t;
        async move { incidents(t, state).await.into_iter().filter(|i| i["rule"] == "central_backup_stale").collect::<Vec<_>>() }
    };
    let check = || selfmon::check_once(&t.st, OffsetDateTime::now_utc());
    check().await.unwrap();
    assert!(backup("all").await.is_empty(), "没装备份：不检查");
    std::fs::create_dir(&bk).unwrap();
    check().await.unwrap();
    let inc = backup("firing").await;
    assert_eq!(inc.len(), 1);
    assert_eq!(
        (inc[0]["severity"].as_str(), inc[0]["subject"].is_null(), inc[0]["subject_id"].is_null(), inc[0]["subject_name"].as_str(), inc[0]["rule_name"].as_str()),
        (Some("warning"), true, true, Some("中控"), Some("备份过期"))
    );
    assert!(inc[0]["summary"].as_str().unwrap().contains("里没有备份"), "{}", inc[0]["summary"]);
    // 只有一份 30 小时前的：还在告警，摘要换成最新一份
    let old = bk.join("fleet-20261021-033000.tar");
    std::fs::write(&old, b"x").unwrap();
    std::fs::File::options().write(true).open(&old).unwrap().set_modified(SystemTime::now() - Duration::from_secs(30 * 3600)).unwrap();
    check().await.unwrap();
    let inc = backup("firing").await;
    let s = inc[0]["summary"].as_str().unwrap();
    assert!(inc.len() == 1 && s.contains("fleet-20261021-033000.tar") && s.contains("超过 26 小时没有新的"), "{inc:?}");
    // 做了新的一份（age 加密的）：恢复
    std::fs::write(bk.join("fleet-20261022-033000.tar.age"), b"x").unwrap();
    check().await.unwrap();
    assert!(backup("firing").await.is_empty());
    assert_eq!(backup("resolved").await.len(), 1);
}

/// 软路由隧道回连主机自检：没有软路由不查，回连的是本机地址算对；查出不是中控时开中控自检的警告 central_tunnel_host，对了就恢复
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tunnel_host_check_opens_central_alert() {
    let t = common::boot_in(tempfile::tempdir().unwrap(), false, "routers.ssh_host = \"127.0.0.1\"").await;
    let check = || fleet_server::routers::selfcheck::check(&t.st);
    assert_eq!(check().await.unwrap(), None, "没有软路由：不查");
    t.post("/api/v1/routers", json!({"name": "团队2-路由A"})).await;
    assert_eq!(check().await.unwrap(), None, "回连的是中控本机的地址");
    let firing = || async { incidents(&t, "firing").await.into_iter().filter(|i| i["rule"] == "central_tunnel_host").collect::<Vec<_>>() };
    let text = "软路由隧道回连 fleet.example.com:22（没配 [routers] ssh_host，用的是 public_url 的主机名）解析到 203.0.113.7，不是中控：…";
    *t.st.tunnel_host.lock().unwrap() = Some(text.into());
    selfmon::check_once(&t.st, OffsetDateTime::now_utc()).await.unwrap();
    let inc = firing().await;
    assert_eq!(inc.len(), 1);
    assert_eq!(
        (inc[0]["severity"].as_str(), inc[0]["subject_name"].as_str(), inc[0]["rule_name"].as_str(), inc[0]["summary"].as_str()),
        (Some("warning"), Some("中控"), Some("软路由回连地址不是中控"), Some(format!("中控：{text}").as_str()))
    );
    *t.st.tunnel_host.lock().unwrap() = None;
    selfmon::check_once(&t.st, OffsetDateTime::now_utc()).await.unwrap();
    assert!(firing().await.is_empty());
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
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let events = || mock.events("/hook/a");
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
    // 会话连接要求 Origin 是本站（public_url）：别的站点、没有 Origin 都 403
    for origin in [Some("https://evil.example"), Some("http://fleet.test:8080"), None] {
        match t.ws_from(true, origin).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(r)) => assert_eq!(r.status(), StatusCode::FORBIDDEN, "{origin:?}"),
            other => panic!("{origin:?} 应 403：{:?}", other.map(|_| ())),
        }
    }
    let mut ws = t.ws(true).await.unwrap();
    // 一口气广播远超缓冲（256）的事件，服务端逐帧往外写跟不上 → 落后
    for _ in 0..5000 {
        t.st.emit(Event::Machines);
    }
    ws_next(&mut ws, "resync").await;
}

/// 会话被撤销（这里是登出）：实时流的连接马上以 4401 关闭，不等 60 秒一轮的复核；别的会话被撤销时不受影响
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_closes_at_once_when_session_revoked() {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let t = setup().await;
    let mut ws = t.ws(true).await.unwrap();
    t.st.revoked();
    t.st.emit(Event::Machines);
    ws_next(&mut ws, "machines").await;
    assert_eq!(t.req("POST", "/api/v1/auth/logout", Some(json!({})), None).await.0, StatusCode::OK);
    let close = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(m) = ws.next().await {
            if let Ok(Message::Close(f)) = m {
                return f.map(|f| u16::from(f.code));
            }
        }
        None
    })
    .await
    .expect("5 秒内应关闭");
    assert_eq!(close, Some(4401));
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

/// 两次完整加载之间只在上一份视图上补指标：补出来的与完整加载的一致（agent、主机读数、探测、Xray、进行中的作业、维护中、健康）；
/// 不发事件的结构字段（这里直接改库里的线路机名）等资源事件才跟上
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_refresh_matches_full_load() {
    use fleet_server::views::{self, Scope};
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "203.0.113.10", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let now = now_ms();
    let mut r = blank_report(1);
    r.host = vec![host(now - 10_000, 10.0, 1_000, 2_000), host(now, 42.5, 11_000, 22_000)];
    r.xray = Some(xray(true));
    r.probes = vec![probe(entry, now, ProbeKind::Egress, true), probe(entry, now, ProbeKind::Tcp, true)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    // 首份上报发 machine 事件：完整加载
    views::live_fresh(&t.st).await.unwrap();

    // 第二份上报：Xray 停了、出口探测失败；另有一个进行中的作业（只发 job 事件）和节点的维护静默（只发 incidents）
    let mut r = blank_report(2);
    r.host = vec![host(now + 10_000, 50.0, 21_000, 42_000)];
    r.xray = Some(xray(false));
    r.probes = vec![probe(entry, now + 10_000, ProbeKind::Egress, false), probe(entry, now + 10_000, ProbeKind::Tcp, true)];
    assert_eq!(t.report("tok1", &r).await.0, StatusCode::OK);
    let job = Uuid::now_v7();
    sqlx::query("insert into job (id, kind, target_id, state, created_by) values ($1, 'upgrade_agent', $2, 'running', 't')").bind(job).bind(relay).execute(&t.st.db).await.unwrap();
    t.st.emit(Event::Job { id: job, target: Some(relay) });
    let f = time::format_description::well_known::Rfc3339;
    let at = OffsetDateTime::now_utc();
    t.post("/api/v1/silences", json!({
        "subject_id": node, "is_maintenance": true, "comment": "换机房",
        "starts_at": (at - time::Duration::minutes(1)).format(&f).unwrap(), "ends_at": (at + time::Duration::hours(1)).format(&f).unwrap(),
    }))
    .await;
    let fresh = views::live_fresh(&t.st).await.unwrap();
    let full = views::load(&t.st, Scope::Live, None).await.unwrap();
    assert_eq!(serde_json::to_value(&*fresh).unwrap(), serde_json::to_value(&full).unwrap());
    let (rv, nv) = (fresh.iter().find(|m| m.id == relay).unwrap(), fresh.iter().find(|m| m.id == node).unwrap());
    assert_eq!((rv.host.as_ref().and_then(|h| h.cpu_pct), rv.active_job.as_ref().map(|j| j.id)), (Some(50.0), Some(job)));
    assert!(rv.health.reasons.iter().any(|r| r.key == "relay_xray_down") && rv.entries[0].egress.as_ref().is_some_and(|p| !p.ok), "{rv:?}");
    assert!(nv.maintenance && nv.entries[0].relay_xray_down, "{nv:?}");

    sqlx::query("update machine set label = '线路机甲' where id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    let name = |v: &[views::MachineView]| v.iter().find(|m| m.id == relay).unwrap().name.clone();
    assert_eq!(name(&views::live_fresh(&t.st).await.unwrap()), "线路机1", "没有资源事件：只补指标");
    // /machines 的响应体随缓存的那份视图缓存：视图换了，响应跟着换
    assert_eq!(t.get("/api/v1/machines").await[0]["name"], "线路机1");
    t.st.emit(Event::Machines);
    assert_eq!(name(&views::live_fresh(&t.st).await.unwrap()), "线路机甲");
    assert_eq!(t.get("/api/v1/machines").await[0]["name"], "线路机甲");
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
    // 一个过期的已挂载分区（带数据）和一个脱离父表的残留表；两张表都有刚进来的数据（删分区前核对本机时钟要看它）
    sqlx::query("create table host_metric_20200101 partition of host_metric for values from ('2020-01-01') to ('2020-01-02')").execute(db).await.unwrap();
    sqlx::query("insert into host_metric (machine_id, ts) values ($1, '2020-01-01 03:00:00+00'), ($1, now())").bind(Uuid::now_v7()).execute(db).await.unwrap();
    sqlx::query("insert into probe_result (entry_id, ts, kind, ok) values ($1, now(), 'tcp', true)").bind(Uuid::now_v7()).execute(db).await.unwrap();
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

/// 删分区前的时钟健全检查：本机时钟大幅超前（开机校时之前）时保留期内的分区不能被当成过期删掉；
/// 所有表都没有最近一天的数据（核对不了时钟）时按各表最新的数据减保留期删；任何一张表有新数据就按本机时钟删
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partitions_survive_a_clock_far_ahead() {
    let t = setup().await;
    let db = &t.st.db;
    sqlx::query("insert into host_metric (machine_id, ts) values ($1, now() - interval '1 minute')").bind(Uuid::now_v7()).execute(db).await.unwrap();
    let exists = |name: String| async move { sqlx::query_scalar::<_, Option<String>>("select to_regclass($1)::text").bind(name).fetch_one(db).await.unwrap().is_some() };
    let today = format!("host_metric_{}", OffsetDateTime::now_utc().date().to_string().replace('-', ""));
    db::maintain_partitions_at(db, OffsetDateTime::now_utc() + time::Duration::days(400), 14, 7, 2).await.unwrap();
    assert!(exists(today.clone()).await, "本机时钟快了 400 天，今天的分区不算过期");
    assert_eq!(t.count("select count(*) from host_metric").await, 1);

    // 全部表都只有很久以前的数据：核对不了时钟，按这张表最新的数据减保留期切——2020-01-01 的数据在它自己的保留期里，不删
    for tbl in ["host_metric", "entry_traffic", "probe_result", "access_event"] {
        sqlx::query(&format!("delete from {tbl}")).execute(db).await.unwrap();
    }
    sqlx::query("create table probe_result_20200101 partition of probe_result for values from ('2020-01-01') to ('2020-01-02')").execute(db).await.unwrap();
    sqlx::query("insert into probe_result (entry_id, ts, kind, ok) values ($1, '2020-01-01 03:00:00+00', 'tcp', true)").bind(Uuid::now_v7()).execute(db).await.unwrap();
    db::maintain_partitions(db, 14, 7, 2).await.unwrap();
    assert!(exists("probe_result_20200101".into()).await, "核对不了本机时钟：以数据为锚，不删");
    // 别的表有了新数据（agent 在报主机读数）：时钟核对得上，这张表空闲多久都按本机时钟删
    sqlx::query("insert into host_metric (machine_id, ts) values ($1, now())").bind(Uuid::now_v7()).execute(db).await.unwrap();
    db::maintain_partitions(db, 14, 7, 2).await.unwrap();
    assert!(!exists("probe_result_20200101".into()).await && exists(today).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_cleanup_removes_only_expired_rows() {
    let t = setup().await;
    let db = &t.st.db;
    let user: Uuid = sqlx::query_scalar("select id from app_user").fetch_one(db).await.unwrap();
    for (state, days) in [("resolved", 200), ("resolved", 10), ("firing", 400)] {
        sqlx::query(&format!(
            "insert into incident (id, engine, rule, subject_id, severity, state, summary, opened_at, resolved_at) values ($1, 'machine', 'agent_offline', $2, 'critical', '{state}', 's',
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
    // 访问记录（汇总、来源画像）按面板里设的保留期删，见 tests/it/retention.rs
    let sessions_before = t.count("select count(*) from session").await;
    db::cleanup_retention(db, &t.st.cfg.retention).await.unwrap();
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
            "insert into incident (id, engine, rule, subject_id, severity, state, summary, opened_at, resolved_at) values ($1, 'machine', 'agent_offline', $2, '{sev}', '{state}', 's',
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
            "insert into incident (id, engine, rule, subject_id, severity, state, summary, opened_at, resolved_at) values ($1, 'machine', 'agent_offline', $2, 'warning', '{state}', 's',
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

/// 机器视图的失败隔离：别的请求正在重算（这里被表锁卡住）时先回上一份、不排队；之后有资源变更的才等；
/// 重算失败时回上一份（之后有资源变更的报错）并记下失败，算成一次就清掉
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_view_serves_previous_while_busy_or_failing() {
    use fleet_server::views;
    use std::sync::Arc;
    let t = setup().await;
    seed_relay(&t, "线路机1", "203.0.113.10", "tok1").await;
    assert_eq!(t.report("tok1", &blank_report(1)).await.0, StatusCode::OK);
    let v1 = views::live_fresh(&t.st).await.unwrap();

    // 补指标要读 agent_state：表锁住，重算卡在锁里（拿着视图的锁）
    let mut lock = t.st.db.begin().await.unwrap();
    sqlx::query("lock table agent_state in access exclusive mode").execute(&mut *lock).await.unwrap();
    let st = t.st.clone();
    let stuck = tokio::spawn(async move { views::live_fresh(&st).await.map(|v| v.len()) });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let v2 = tokio::time::timeout(Duration::from_secs(2), views::live_fresh(&t.st)).await.expect("锁被占时不排队").unwrap();
    assert!(Arc::ptr_eq(&v1, &v2), "回上一份");
    assert!(tokio::time::timeout(Duration::from_millis(500), views::live_synced(&t.st)).await.is_err(), "告警评估要的视图不回上一份，等锁");
    // 有了资源变更：要看到改动，排队等那次重算
    t.st.live.invalidate();
    assert!(tokio::time::timeout(Duration::from_millis(500), views::live_fresh(&t.st)).await.is_err(), "资源变了就等");
    lock.rollback().await.unwrap();
    assert_eq!(stuck.await.unwrap().unwrap(), 1);
    let v3 = views::live_fresh(&t.st).await.unwrap();
    assert!(t.st.live.failing().is_none());

    // 重算失败（表没了）：之后没有资源变更的回上一份，记下失败；有过资源变更的报错（调用方重试），不回变更前的那份；
    // 恢复后算成一次就清掉
    sqlx::query("alter table agent_state rename to agent_state_x").execute(&t.st.db).await.unwrap();
    let v4 = views::live_fresh(&t.st).await.unwrap();
    assert!(Arc::ptr_eq(&v3, &v4));
    let (_, err) = t.st.live.failing().expect("记下失败");
    assert!(err.contains("agent_state"), "{err}");
    t.st.live.invalidate();
    assert!(views::live_fresh(&t.st).await.is_err(), "资源变了不回变更前的");
    assert!(views::live(&t.st).await.is_err());
    sqlx::query("alter table agent_state_x rename to agent_state").execute(&t.st.db).await.unwrap();
    assert!(!Arc::ptr_eq(&v3, &views::live_fresh(&t.st).await.unwrap()));
    assert!(t.st.live.failing().is_none());
}

/// 视图连续 2 分钟算不出来（没有能顶的上一份）：中控自检开「机器视图算不出来」，期间判断不了的「agent 同时失联」留着；
/// 算成后前者恢复、后者按此时的情况判
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stuck_view_opens_central_alert() {
    let t = setup().await;
    seed_relay(&t, "线路机1", "203.0.113.10", "tok1").await;
    sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary) values ($1, 'central', 'central_ingress', $2, 'critical', '中控：3/3 台 agent 同时失联')")
        .bind(Uuid::now_v7())
        .bind(selfmon::CENTRAL)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("alter table agent_state rename to agent_state_x").execute(&t.st.db).await.unwrap();
    assert!(fleet_server::views::live(&t.st).await.is_err(), "没有上一份可顶");
    let rules = || async { incidents(&t, "firing").await.into_iter().map(|i| i["rule"].as_str().unwrap().to_string()).collect::<Vec<_>>() };
    let base = OffsetDateTime::now_utc();
    selfmon::check_once(&t.st, base).await.unwrap();
    assert_eq!(rules().await, ["central_ingress"], "刚失败不报；判断不了的不当恢复");
    selfmon::check_once(&t.st, base + time::Duration::minutes(3)).await.unwrap();
    let f = incidents(&t, "firing").await;
    let stuck = f.iter().find(|i| i["rule"] == "central_view_stuck").expect("满 2 分钟开告警");
    assert!(stuck["summary"].as_str().unwrap().contains("分钟算不出来"), "{stuck}");
    assert_eq!(stuck["engine"], "central", "接口带出写它的引擎");
    assert!(has_rule(&f, "central_ingress"));

    sqlx::query("alter table agent_state_x rename to agent_state").execute(&t.st.db).await.unwrap();
    selfmon::check_once(&t.st, base + time::Duration::minutes(4)).await.unwrap();
    assert!(rules().await.is_empty(), "视图算成：两条都恢复");
}

/// agent 接口先鉴权后读请求体；令牌查库没查到的按来源 IP 限速，超了 429、不查库；缓存里认得的令牌不受影响
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_auth_before_body_and_failures_limited_per_ip() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let t = crate::common::boot(false, "trust_forwarded_for = true").await;
    seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    seed_relay(&t, "线路机2", "198.51.100.2", "tok2").await;
    let send = |tok: String, ip: &'static str, body: String| {
        let app = t.app.clone();
        async move {
            let req = Request::builder()
                .method("POST")
                .uri("/agent/v1/report")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {tok}"))
                .header("x-forwarded-for", ip)
                .body(Body::from(body))
                .unwrap();
            let res = app.oneshot(req).await.unwrap();
            (res.status(), res.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse::<u64>().ok()))
        }
    };
    let report = |seq| serde_json::to_string(&blank_report(seq)).unwrap();
    const IP: &str = "203.0.113.9";
    // 坏令牌带坏请求体：401，请求体没读；好令牌带坏请求体照旧 400
    assert_eq!(send("bad".into(), IP, "{oops".into()).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send("tok1".into(), IP, "{oops".into()).await.0, StatusCode::BAD_REQUEST);
    // 突发 30 次查库失败（含上面那次）之后 429，带 Retry-After
    let mut fails = 1;
    let limited = loop {
        let (s, ra) = send(format!("bad{fails}"), IP, report(1)).await;
        if s == StatusCode::TOO_MANY_REQUESTS {
            break ra;
        }
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        fails += 1;
        assert!(fails <= 33, "一直没限速");
    };
    assert!(fails >= 30, "{fails}");
    assert!(limited.is_some_and(|w| w >= 1), "{limited:?}");
    // 记着无效的令牌照样 401（不查库）；缓存里认得的有效令牌照常上报
    assert_eq!(send("bad".into(), IP, report(1)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send("tok1".into(), IP, report(1)).await.0, StatusCode::OK);
    // 没缓存的令牌要查库：这个 IP 先等着；别的 IP 不受影响
    assert_eq!(send("tok2".into(), IP, report(1)).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(send("tok2".into(), "203.0.113.10", report(1)).await.0, StatusCode::OK);
}

/// 期望状态格式版本（db::DESIRED_FORMAT）和库里记的不同（含从没记过）：启动时所有未退役线路机代数 +1、记下新值，
/// 长轮询里拿着旧代数的线路机马上拿到新的一份；同一版本再启动什么都不动
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desired_format_change_bumps_relays_once() {
    let t = setup().await;
    let a = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let b = seed_relay(&t, "线路机2", "198.51.100.2", "tok2").await;
    sqlx::query("update machine set lifecycle = 'retired' where id = $1").bind(b).execute(&t.st.db).await.unwrap();
    let gen = |id: Uuid| {
        let db = t.st.db.clone();
        async move { sqlx::query_scalar::<_, i64>("select generation from relay where machine_id = $1").bind(id).fetch_one(&db).await.unwrap() }
    };
    let (ga, gb) = (gen(a).await, gen(b).await);
    assert_eq!(db::sync_desired_format(&t.st.db).await.unwrap(), 1, "从没记过：按变了算");
    assert_eq!((gen(a).await, gen(b).await), (ga + 1, gb), "退役的不动");
    let stored = || t.count("select (value #>> '{}')::bigint from app_setting where key = 'desired_format'");
    assert_eq!(stored().await, db::DESIRED_FORMAT);
    assert_eq!(db::sync_desired_format(&t.st.db).await.unwrap(), 0, "同一版本不再改");
    assert_eq!(gen(a).await, ga + 1);

    // 库里记的是旧版本（中控升级后第一次启动）：再 +1；拿着上一代长轮询的线路机马上拿到新的一份
    sqlx::query("update app_setting set value = to_jsonb($1::bigint) where key = 'desired_format'").bind(db::DESIRED_FORMAT - 1).execute(&t.st.db).await.unwrap();
    assert_eq!(db::sync_desired_format(&t.st.db).await.unwrap(), 1);
    assert_eq!(gen(a).await, ga + 2);
    let (s, v) = t.req("GET", &format!("/agent/v1/desired?after={}", ga + 1), None, Some("tok1")).await;
    assert_eq!((s, v["generation"].as_i64()), (StatusCode::OK, Some(ga + 2)), "{v}");
}

/// 机器告警这一轮：`fire` 里的（对象, 规则）成立，开着的别的恢复。通知只入队（发送循环没起），之后由 `send_queued` 一起发
async fn machine_round(t: &T, fire: &[(Uuid, &'static str, &'static str)]) {
    use fleet_server::incident::{self, Close, Engine, Want};
    let open = incident::firing(&t.st.db, Engine::Machine).await.unwrap();
    let want = fire.iter().map(|&(subject, rule, name)| Want { rule, subject, name, severity: "critical", text: "出口不通：timeout".into(), detail: None, ready: true }).collect();
    incident::reconcile(&t.st, Engine::Machine, open, want, "true", OffsetDateTime::now_utc(), |_| Close::Resolve("团队2-09")).await.unwrap();
}

/// 起发送循环（另一份内存状态，同一个库）：排着的一起发；`wait_s` 是合并时等多久没有新的同类就发
fn send_queued(t: &T, wait_s: u64) -> AppState {
    let mut cfg = t.st.cfg.clone();
    cfg.notify.group_wait_s = wait_s;
    let st = AppState::new(cfg, t.st.db.clone(), SecretBox::from_key(&[9u8; 32]).unwrap());
    fleet_server::notify::start(&st);
    st
}

/// 同一规则的几台同时触发：每个渠道合成一条消息（标题「N 台 规则」，逐台列出），发送记录仍每条告警各记一条
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_rule_firings_are_sent_as_one_message() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "tg", "kind": "telegram", "config": {"bot_token": "1:A", "chat_id": 1, "api_base": mock.url("")}, "min_severity": "info", "enabled": true})).await;
    t.post("/api/v1/notify-channels", json!({"name": "wc", "kind": "feishu", "config": {"url": mock.url("/feishu/send")}, "min_severity": "info", "enabled": true})).await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    t.st.notify_started.store(true, Ordering::Relaxed);
    let names = ["团队2-07", "团队2-08", "团队2-09"];
    machine_round(&t, &names.map(|n| (Uuid::new_v4(), "egress_fail", n))).await;
    send_queued(&t, 0);
    log_settles(&t, 9).await;
    assert_eq!((mock.count("/bot1:A"), mock.count("/feishu/send"), mock.count("/hook/a")), (1, 1, 1));
    let tg = mock.bodies("/bot1:A")[0]["text"].as_str().unwrap().to_string();
    assert!(tg.starts_with("【严重】告警触发：3 台 出口不通\n· 团队2-0") && names.iter().all(|n| tg.contains(&format!("· {n}：出口不通：timeout"))), "{tg}");
    let wc = &mock.texts("/feishu/send")[0];
    assert!(wc.starts_with("【严重】告警触发：3 台 出口不通\n· 团队2-0") && wc.contains("\n时间："), "{wc}");
    let wh = &mock.texts("/hook/a")[0];
    assert_eq!((wh.lines().filter(|l| l.starts_with("· ")).count(), mock.bodies("/hook/a")[0]["content"]["post"]["zh_cn"]["content"].as_array().unwrap().len()), (3, 4));
    assert_eq!(t.count("select count(distinct incident_id) from notify_log where ok and kind = 'firing'").await, 3);
}

/// 不同规则不合并，触发与恢复不合并（同一条告警的恢复排在它的触发之后发）；等着合并的时候，不合并的（中控自身的告警）不被挡住、马上发
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_rules_and_kinds_are_not_merged() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    t.st.notify_started.store(true, Ordering::Relaxed);
    let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    machine_round(&t, &[(c, "egress_fail", "团队2-09")]).await;
    // 下一轮：C 恢复，A（同规则）、B（另一条规则）触发——四条一起排着
    machine_round(&t, &[(a, "egress_fail", "团队2-07"), (b, "relay_xray_down", "团队2-08")]).await;
    // 合并等 2 秒：等的时候中控自身告警了，马上发、排在前面
    let st2 = send_queued(&t, 2);
    {
        use fleet_server::incident::{self, Close, Engine, Want};
        let want = vec![Want { rule: "central_disk_low", subject: Uuid::new_v4(), name: "中控", severity: "critical", text: "磁盘只剩 3%".into(), detail: None, ready: true }];
        incident::reconcile(&st2, Engine::Central, vec![], want, "true", OffsetDateTime::now_utc(), |_| Close::Keep).await.unwrap();
    }
    mock.wait("/hook/a", 1).await;
    let first = mock.texts("/hook/a");
    assert!(first.len() == 1 && first[0].contains("磁盘只剩 3%"), "中控自身告警不等合并：{first:?}");
    log_settles(&t, 5).await;
    let texts = mock.texts("/hook/a");
    assert_eq!(mock.events("/hook/a"), ["firing", "firing", "firing", "resolved"]);
    assert!(texts[1].contains("告警触发：2 台 出口不通") && texts[2].contains("规则：Xray 未运行") && texts[3].contains("机器：团队2-09"), "{texts:?}");
}
