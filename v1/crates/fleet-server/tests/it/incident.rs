//! 通用告警对齐（incident.rs：软路由、探测点、中控、来源画像共用）与机器的告警口径一致：
//! 恢复后 30 分钟内复发重开原告警（复发计数 +1、清掉确认）、报过恢复后 5 分钟内复发不再报触发（抖动限流）、恢复不重复报、
//! 严重度升级时清掉确认并马上再发「触发」、悄悄关掉的（带 closed_by）不重开。
//! 引擎分地盘（`incident.engine`）：同名规则 / 同一个对象在不同引擎下互不相干，规则名不带任何前缀也不会被别的引擎关掉，迁移按旧前缀回填。
//! 直接调 `incident::reconcile`（公开接口不变），评估时钟可以快进；通知的时刻由测试改成评估时钟上的（同 telemetry 的抖动测试）。

use crate::common;

use common::tele::*;
use fleet_server::incident::{self, Close, Engine, Want};
use fleet_server::{alerts, probe_points, routers, selfmon};
use serde_json::json;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

const RULE: &str = "central_test_flap";

fn want(subject: Uuid, severity: &'static str) -> Want<'static> {
    Want { rule: RULE, subject, name: "测试对象", severity, text: "出问题了".into(), detail: None, ready: true }
}

/// 一轮：`up` = 这一轮问题成立（给出 Want），否则不成立时按 `close` 关
async fn round(t: &T, at: OffsetDateTime, w: Option<Want<'static>>, close: fn() -> Close<'static>) {
    let open = incident::firing(&t.st.db, Engine::Central).await.unwrap();
    incident::reconcile(&t.st, Engine::Central, open, w.into_iter().collect(), "true", at, |_| close()).await.unwrap();
}

fn resolve() -> Close<'static> { Close::Resolve("测试对象") }
fn quiet() -> Close<'static> { Close::Quiet("test_removed") }

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

/// 把最新一条通知记录的时间改成评估时钟上的某一刻
async fn stamp_last_notice(t: &T, at: OffsetDateTime) {
    sqlx::query("update notify_log set ts = $1 where id = (select max(id) from notify_log)").bind(at).execute(&t.st.db).await.unwrap();
}

async fn state(t: &T) -> Vec<(Uuid, String, i32, bool, String)> {
    sqlx::query_as("select id, state::text, reopen_count, ack_at is not null, severity::text from incident where rule = $1 order by opened_at, id")
        .bind(RULE)
        .fetch_all(&t.st.db)
        .await
        .unwrap()
}

async fn ack(t: &T, id: Uuid) { sqlx::query("update incident set ack_at = now(), ack_by = 'admin' where id = $1").bind(id).execute(&t.st.db).await.unwrap(); }

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_reopens_throttles_and_clears_ack_like_machine_alerts() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "webhook", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    let events = || mock.bodies("/hook/a").iter().map(|b| b["event"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let subject = Uuid::now_v7();
    let base = OffsetDateTime::now_utc();
    let at = |s: i64| base + time::Duration::seconds(s);

    // 触发 → 确认 → 恢复：各通知一次
    round(&t, at(0), Some(want(subject, "warning")), resolve).await;
    let first = state(&t).await[0].0;
    log_settles(&t, 1).await;
    stamp_last_notice(&t, at(0)).await;
    ack(&t, first).await;
    round(&t, at(60), None, resolve).await;
    assert_eq!(state(&t).await[0].1, "resolved");
    log_settles(&t, 2).await;
    stamp_last_notice(&t, at(60)).await;

    // 恢复通知后 5 分钟内复发：重开同一条（复发计数 +1、确认清掉），先不通知；满 5 分钟仍在触发再发「触发」
    round(&t, at(120), Some(want(subject, "warning")), resolve).await;
    let s = state(&t).await;
    assert_eq!(s.len(), 1, "不新建：{s:?}");
    assert_eq!((s[0].0, s[0].1.as_str(), s[0].2, s[0].3), (first, "firing", 1, false), "{s:?}");
    round(&t, at(300), Some(want(subject, "warning")), resolve).await;
    log_settles(&t, 2).await;
    round(&t, at(361), Some(want(subject, "warning")), resolve).await;
    log_settles(&t, 3).await;
    stamp_last_notice(&t, at(361)).await;
    assert_eq!(events(), ["firing", "resolved", "firing"]);

    // 严重度升级：马上再发「触发」，清掉之前的确认
    ack(&t, first).await;
    round(&t, at(400), Some(want(subject, "critical")), resolve).await;
    let s = state(&t).await;
    assert_eq!((s[0].3, s[0].4.as_str()), (false, "critical"), "升级后确认清掉：{s:?}");
    log_settles(&t, 4).await;
    stamp_last_notice(&t, at(400)).await;
    assert_eq!(mock.bodies("/hook/a").last().unwrap()["severity"], "critical");

    // 恢复（报过触发：报恢复）；5 分钟内又抖一下就恢复：重开但不通知，也不重复报恢复
    round(&t, at(460), None, resolve).await;
    log_settles(&t, 5).await;
    stamp_last_notice(&t, at(460)).await;
    round(&t, at(490), Some(want(subject, "critical")), resolve).await;
    assert_eq!(state(&t).await[0].2, 2);
    round(&t, at(520), None, resolve).await;
    log_settles(&t, 5).await;
    assert_eq!(events(), ["firing", "resolved", "firing", "firing", "resolved"]);

    // 距恢复通知已满 5 分钟才复发：重开时立即发「触发」（真故障不被压住）
    round(&t, at(800), Some(want(subject, "warning")), resolve).await;
    assert_eq!(state(&t).await[0].2, 3);
    log_settles(&t, 6).await;

    // 悄悄关掉的（对象删了等，带 closed_by）不重开：再出现时新开一条
    round(&t, at(830), None, quiet).await;
    round(&t, at(860), Some(want(subject, "warning")), resolve).await;
    let s = state(&t).await;
    assert_eq!(s.len(), 2, "{s:?}");
    assert_eq!((s[0].1.as_str(), s[1].1.as_str(), s[1].2), ("resolved", "firing", 0));

    // 超过重开窗口再出现：新开一条
    round(&t, at(890), None, resolve).await;
    sqlx::query("update incident set resolved_at = now() - interval '1 hour' where rule = $1").bind(RULE).execute(&t.st.db).await.unwrap();
    round(&t, at(920), Some(want(subject, "warning")), resolve).await;
    let s = state(&t).await;
    assert_eq!((s.len(), s[2].1.as_str(), s[2].2), (3, "firing", 0), "{s:?}");

    // 隔得远的老毛病：近 7 天开了 3 次，告警列表带 recent_count；触发通知摘要带「近 24 小时第 N 次」
    tokio::time::sleep(Duration::from_millis(500)).await;
    let items = t.get("/api/v1/incidents?state=all").await["items"].as_array().unwrap().clone();
    let counts: Vec<_> = items.iter().filter(|i| i["rule"] == RULE).map(|i| i["recent_count"].as_i64().unwrap()).collect();
    assert_eq!(counts, [3, 3, 3], "{items:?}");
    assert!(mock.bodies("/hook/a").iter().any(|b| b["summary"].as_str().unwrap().ends_with("（近 24 小时第 3 次）")));
}

/// 一轮：这个引擎此刻成立的问题只有 `w`，不成立的恢复（`exists` 恒真）
async fn round_of(t: &T, engine: Engine, at: OffsetDateTime, w: Option<Want<'static>>) {
    let open = incident::firing(&t.st.db, engine).await.unwrap();
    incident::reconcile(&t.st, engine, open, w.into_iter().collect(), "true", at, |_| resolve()).await.unwrap();
}

/// 同一条规则、同一个对象在两个引擎下各有一条：各开各的、各恢复各的、各复发各的，互不影响
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_rule_and_subject_in_two_engines_do_not_interfere() {
    let t = setup().await;
    let subject = Uuid::now_v7();
    let base = OffsetDateTime::now_utc();
    let at = |s: i64| base + time::Duration::seconds(s);
    let rows = || async {
        sqlx::query_as::<_, (String, String, i32)>("select engine, state::text, reopen_count from incident where rule = $1 order by engine, opened_at")
            .bind(RULE)
            .fetch_all(&t.st.db)
            .await
            .unwrap()
    };
    let row = |e: &str, st: &str, n: i32| (e.to_string(), st.to_string(), n);

    round_of(&t, Engine::Central, at(0), Some(want(subject, "warning"))).await;
    round_of(&t, Engine::Router, at(0), Some(want(subject, "warning"))).await;
    assert_eq!(rows().await, [row("central", "firing", 0), row("router", "firing", 0)], "同名同对象各开一条");

    // 中控侧的问题没了：只恢复它自己的；软路由引擎再对齐一轮（问题仍成立）不会新开、不会动中控那条
    round_of(&t, Engine::Central, at(60), None).await;
    round_of(&t, Engine::Router, at(60), Some(want(subject, "warning"))).await;
    assert_eq!(rows().await, [row("central", "resolved", 0), row("router", "firing", 0)]);

    // 中控侧复发：只重开自己恢复的那条（不会因为软路由那条还开着而不开，也不会动软路由那条）
    round_of(&t, Engine::Central, at(120), Some(want(subject, "warning"))).await;
    assert_eq!(rows().await, [row("central", "firing", 1), row("router", "firing", 0)]);

    // 软路由引擎一个问题都没了：只恢复它的，中控那条照旧
    round_of(&t, Engine::Router, at(180), None).await;
    assert_eq!(rows().await, [row("central", "firing", 1), row("router", "resolved", 0)]);

    // 两个引擎的 firing 读取也各看各的
    let (c, r) = (incident::firing(&t.st.db, Engine::Central).await.unwrap(), incident::firing(&t.st.db, Engine::Router).await.unwrap());
    assert_eq!((c.len(), r.len()), (1, 0));
}

/// 新加一条规则（或新引擎）不用改任何前缀：规则名不带 central_ / router_ / probe_ / ipintel_，别的引擎的评估一轮也不会把它当成「原因消失」关掉
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rule_without_any_prefix_is_not_closed_by_other_engines() {
    let t = setup().await;
    let subject = Uuid::now_v7();
    let base = OffsetDateTime::now_utc();
    let w = || Want { rule: "plain_new_rule", subject, name: "对象", severity: "warning", text: "出问题了".into(), detail: None, ready: true };
    for e in [Engine::Machine, Engine::Ipintel, Engine::Central, Engine::Router, Engine::Probe] {
        incident::reconcile(&t.st, e, vec![], vec![w()], "true", base, |_| resolve()).await.unwrap();
    }
    let firing = || async {
        sqlx::query_scalar::<_, String>("select engine from incident where rule = 'plain_new_rule' and state = 'firing' order by engine")
            .fetch_all(&t.st.db)
            .await
            .unwrap()
    };
    assert_eq!(firing().await, ["central", "ipintel", "machine", "probe", "router"]);

    // 各引擎的评估循环各跑一轮：每一轮只关它自己的那条（对象不存在），其余原样
    routers::alerts::evaluate(&t.st, &mut Default::default(), base).await.unwrap();
    assert_eq!(firing().await, ["central", "ipintel", "machine", "probe"]);
    probe_points::check_once(&t.st, base).await.unwrap();
    assert_eq!(firing().await, ["central", "ipintel", "machine"]);
    selfmon::check_once(&t.st, base).await.unwrap();
    assert_eq!(firing().await, ["ipintel", "machine"]);
    // 机器引擎只处理在用机器的告警：这个对象不是机器，原样；来源画像引擎这里没有评估，原样
    alerts::evaluate_once(&t.st, &mut Default::default(), base).await.unwrap();
    assert_eq!(firing().await, ["ipintel", "machine"]);
}
