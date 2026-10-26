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
    Want { rule: RULE, subject, name: "测试对象", severity, text: "出问题了".into(), detail: None, ready: true, quiet: false }
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
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    let events = || mock.events("/hook/a");
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
    assert!(mock.texts("/hook/a").last().unwrap().starts_with("【严重】"));

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

    // 隔得远的老毛病：近 7 天开了 3 条（第一条另合并过 3 次复发），recent_count = 3 + 3；触发通知摘要带「近 24 小时第 N 次」
    tokio::time::sleep(Duration::from_millis(500)).await;
    let items = t.get("/api/v1/incidents?state=all").await["items"].as_array().unwrap().clone();
    let counts: Vec<_> = items.iter().filter(|i| i["rule"] == RULE).map(|i| i["recent_count"].as_i64().unwrap()).collect();
    assert_eq!(counts, [6, 6, 6], "{items:?}");
    // 反复的门槛在中控：每条带 chronic；state=chronic 每个（规则, 对象）只取最近一条
    assert!(items.iter().filter(|i| i["rule"] == RULE).all(|i| i["chronic"] == true), "{items:?}");
    let chronic = t.get("/api/v1/incidents?state=chronic").await["items"].as_array().unwrap().clone();
    assert_eq!(chronic.iter().filter(|i| i["rule"] == RULE).count(), 1, "{chronic:?}");
    assert!(mock.texts("/hook/a").iter().any(|x| x.contains("（近 24 小时第 6 次）\n时间：")));
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
    let w = || Want { rule: "plain_new_rule", subject, name: "对象", severity: "warning", text: "出问题了".into(), detail: None, ready: true, quiet: false };
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

/// 已确认的告警不再发「提醒」；报过触发的告警在普通静默期间恢复照报「恢复」，维护窗口内不报
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acked_incident_gets_no_repeat_and_silenced_recovery_is_still_told() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    let events = || mock.events("/hook/a");
    let (s1, s2) = (Uuid::now_v7(), Uuid::now_v7());
    let base = OffsetDateTime::now_utc();
    let at = |s: i64| base + time::Duration::seconds(s);
    let hours = |h: i64| h * 3600;

    // 严重告警：触发后确认，过了提醒间隔（4 小时）也不再提醒
    round(&t, at(0), Some(want(s1, "critical")), resolve).await;
    log_settles(&t, 1).await;
    stamp_last_notice(&t, at(0)).await;
    let id1 = state(&t).await[0].0;
    ack(&t, id1).await;
    round(&t, at(hours(5)), Some(want(s1, "critical")), resolve).await;
    log_settles(&t, 1).await;

    // 普通静默期间恢复：触发已经发过，「恢复」照发（否则群里只留一条触发）
    sqlx::query("insert into silence (id, rule, starts_at, ends_at, created_by) values ($1, $2, now() - interval '1 minute', now() + interval '1 hour', 'u')")
        .bind(Uuid::now_v7())
        .bind(RULE)
        .execute(&t.st.db)
        .await
        .unwrap();
    round(&t, at(hours(5) + 60), None, resolve).await;
    assert_eq!(state(&t).await[0].1, "resolved");
    log_settles(&t, 2).await;
    assert_eq!(events(), ["firing", "resolved"]);

    // 维护窗口内恢复不报
    sqlx::query("update silence set deleted_at = now()").execute(&t.st.db).await.unwrap();
    round(&t, at(hours(6)), Some(want(s2, "critical")), resolve).await;
    log_settles(&t, 3).await;
    sqlx::query("insert into silence (id, starts_at, ends_at, is_maintenance, created_by) values ($1, now() - interval '1 minute', now() + interval '1 hour', true, 'u')")
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();
    round(&t, at(hours(6) + 60), None, resolve).await;
    log_settles(&t, 3).await;
    assert_eq!(events(), ["firing", "resolved", "firing"]);
}

/// 跟着别的告警一起报的（`quiet`）：开着、不发触发 / 提醒，也没有恢复通知；原因单独留下来（不再 quiet）时按「没入队过」补发一次触发
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quiet_incident_stays_silent_until_it_stands_alone() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    let (s1, s2) = (Uuid::now_v7(), Uuid::now_v7());
    let base = OffsetDateTime::now_utc();
    let at = |s: i64| base + time::Duration::seconds(s);
    let quiet_want = |s: Uuid| Want { quiet: true, ..want(s, "critical") };

    // 开了、不通知；再评估一轮（含严重告警的提醒间隔之后）也不通知
    round(&t, at(0), Some(quiet_want(s1)), resolve).await;
    round(&t, at(60), Some(quiet_want(s1)), resolve).await;
    round(&t, at(5 * 3600), Some(quiet_want(s1)), resolve).await;
    assert_eq!(state(&t).await.iter().map(|r| r.1.as_str()).collect::<Vec<_>>(), ["firing"]);
    log_settles(&t, 0).await;
    // 另一条 quiet 的直接恢复：没报过触发，也不报恢复
    round(&t, at(10), Some(quiet_want(s2)), resolve).await;
    round(&t, at(70), None, resolve).await;
    assert_eq!(state(&t).await.iter().filter(|r| r.1 == "resolved").count(), 2, "两条都恢复了");
    log_settles(&t, 0).await;
    // 再来一条：先 quiet，后来原因单独留下（不再 quiet）：补发一次触发，之后恢复才报恢复
    let s3 = Uuid::now_v7();
    round(&t, at(100), Some(quiet_want(s3)), resolve).await;
    log_settles(&t, 0).await;
    round(&t, at(130), Some(want(s3, "critical")), resolve).await;
    log_settles(&t, 1).await;
    mock.wait("/hook/a", 1).await;
    assert_eq!(mock.events("/hook/a"), ["firing"]);
    round(&t, at(200), None, resolve).await;
    log_settles(&t, 2).await;
    assert_eq!(mock.events("/hook/a"), ["firing", "resolved"]);
}

fn noted() -> Close<'static> { Close::ResolveNoted { name: "测试对象", by: "spare", note: "已无软路由出口指向，转为闲置备用" } }

/// 条件变了而不是原因消失的关闭（ResolveNoted，如被指着的备用取消了指向转为闲置）：detail 记 closed_by，报过触发的发「恢复」且摘要附说明；
/// 开的时候没通知过（quiet）的不发
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve_noted_sends_recovery_with_note_only_after_firing_notice() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    let (told, silent) = (Uuid::now_v7(), Uuid::now_v7());
    let base = OffsetDateTime::now_utc();
    let open = |subject: Uuid, quiet: bool, at: OffsetDateTime| {
        let t = &t;
        async move {
            let o = incident::firing(&t.st.db, Engine::Central).await.unwrap();
            let mut w = want(subject, "warning");
            w.quiet = quiet;
            incident::reconcile(&t.st, Engine::Central, o, vec![w], "true", at, |_| Close::Keep).await.unwrap();
        }
    };
    open(told, false, base).await;
    log_settles(&t, 1).await;
    open(silent, true, base).await;
    log_settles(&t, 1).await;

    // 两条都关：报过触发的发恢复（摘要附说明），没通知过的不发
    round(&t, base + time::Duration::seconds(60), None, noted).await;
    log_settles(&t, 2).await;
    assert_eq!(mock.events("/hook/a"), ["firing", "resolved"]);
    assert!(mock.texts("/hook/a").last().unwrap().contains("出问题了；已无软路由出口指向，转为闲置备用"), "{:?}", mock.texts("/hook/a"));
    assert_eq!(t.count("select count(*) from incident where rule = 'central_test_flap' and state = 'resolved' and detail->>'closed_by' = 'spare'").await, 2);
}
