//! 通用告警对齐（incident.rs：软路由、探测点、中控、来源画像共用）与机器的告警口径一致：
//! 恢复后 30 分钟内复发重开原告警（复发计数 +1、清掉确认）、报过恢复后 5 分钟内复发不再报触发（抖动限流）、恢复不重复报、
//! 严重度升级时清掉确认并马上再发「触发」、悄悄关掉的（带 closed_by）不重开。
//! 直接调 `incident::reconcile`（公开接口不变），评估时钟可以快进；通知的时刻由测试改成评估时钟上的（同 telemetry 的抖动测试）。

use crate::common;

use common::tele::*;
use fleet_server::incident::{self, Close, Want};
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
    let open = incident::firing(&t.st.db, "central").await.unwrap();
    incident::reconcile(&t.st, open, w.into_iter().collect(), "true", at, |_| close()).await.unwrap();
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
}
