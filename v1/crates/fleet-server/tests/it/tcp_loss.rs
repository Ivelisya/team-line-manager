//! 持续握手丢包（tcp_loss_high）：近 30 分钟入口 TCP 探测的失败率 / SYN 重传率持续偏高 → 节点降级、开警告，摘要带供应商 / 网段关联；
//! 小样本、偶发 1–2 次失败不开；恢复有滞回（低于一半阈值满 10 分钟）；中控重启后在触发的不因比例落在两个阈值之间而恢复；闲置备用只降级不告警。
//! 需要 PostgreSQL（见 common）。不起后台循环：丢包评估与告警评估都由测试按给定时刻手动驱动，探测记录直接写库。

use crate::common;

use common::tele::*;
use fleet_server::alerts::{self, AlertState};
use fleet_server::tcp_loss::{self, LossState};
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// 丢包评估一轮，再评估告警（视图有 3 秒短缓存，前后都作废）
async fn pass(t: &T, ls: &mut LossState, s: &mut AlertState, at: OffsetDateTime) {
    t.st.live.invalidate();
    tcp_loss::evaluate(&t.st, ls, at).await.unwrap();
    t.st.live.invalidate();
    alerts::evaluate_once(&t.st, s, at).await.unwrap();
}

async fn firing(t: &T) -> Vec<Value> {
    let v = t.get("/api/v1/incidents?state=firing").await["items"].as_array().unwrap().clone();
    v.into_iter().filter(|i| i["rule"] == tcp_loss::RULE).collect()
}

/// `k` 次均匀撒在 `n` 个位置里（第 0 个，即最新的一次，`k < n` 时不在里面）
fn spread(n: i64, k: i64) -> Vec<bool> { (0..n).map(|i| (i + 1) * k / n > i * k / n).collect() }

/// `entry` 在 `end` 之前每 30 秒一次、共 `n` 次 TCP 探测：`fail` 次「超时 10 秒」、`retrans` 次连上但 SYN 重传过，都均匀撒开
/// （零散的失败，最新一次是成功的：不会顺带触发看连续失败的 tcp_fail）
async fn probes(t: &T, entry: Uuid, end: OffsetDateTime, n: i64, fail: i64, retrans: i64) {
    let ts: Vec<OffsetDateTime> = (0..n).map(|i| end - Duration::seconds(5 + 30 * i)).collect();
    let ok: Vec<bool> = spread(n, fail).into_iter().map(|f| !f).collect();
    let mut rs = spread(n - fail, retrans).into_iter();
    let re: Vec<Option<i16>> = ok.iter().map(|ok| ok.then(|| rs.next().unwrap() as i16)).collect();
    sqlx::query(
        "insert into probe_result (entry_id, ts, kind, ok, latency_ms, detail, retrans)
         select $1, x.ts, 'tcp', x.ok, case when x.ok then 3 end, case when not x.ok then '超时 10 秒' end, x.re
         from unnest($2::timestamptz[], $3::bool[], $4::int2[]) x(ts, ok, re)",
    )
    .bind(entry)
    .bind(&ts)
    .bind(&ok)
    .bind(&re)
    .execute(&t.st.db)
    .await
    .unwrap();
}

async fn provider(t: &T, name: &str, code: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into provider (id, name, code) values ($1, $2, $3)").bind(id).bind(name).bind(code).execute(&t.st.db).await.unwrap();
    id
}

async fn set_provider(t: &T, node: Uuid, p: Uuid) {
    sqlx::query("update machine set provider_id = $2 where id = $1").bind(node).bind(p).execute(&t.st.db).await.unwrap();
}

/// 一台在线的线路机（经它的探测才算数）
async fn online_relay(t: &T) -> Uuid {
    let relay = seed_relay(t, "线路机1", "198.51.100.1", "tok1").await;
    let (s, v) = t.report("tok1", &blank_report(1)).await;
    assert_eq!(s.as_u16(), 200, "{v}");
    relay
}

/// 同供应商两台一起丢：各开一条警告、节点降级，摘要写失败率、重传率和「同供应商 DeerVPS 共 2 台同时」，建议 `fleetctl diag`。
/// 偶发 2 次失败（60 次里）、样本不够（20 次里 5 次失败）的不开；别家供应商同一个 /24 的一台单独丢时按网段也不凑数
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opens_with_provider_correlation_and_ignores_noise() {
    let t = setup().await;
    let relay = online_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let deer = provider(&t, "DeerVPS", "DV").await;
    let other = provider(&t, "GOMAMI", "GM").await;
    let (a, ea) = seed_node(&t, relay, team, 1, "203.0.113.5", 20001).await;
    let (b, eb) = seed_node(&t, relay, team, 2, "203.0.113.6", 20002).await;
    let (c, ec) = seed_node(&t, relay, team, 3, "198.51.100.30", 20003).await;
    let (d, ed) = seed_node(&t, relay, team, 4, "198.51.100.40", 20004).await;
    for n in [a, b] {
        set_provider(&t, n, deer).await;
    }
    for n in [c, d] {
        set_provider(&t, n, other).await;
    }
    // 记录写到开告警那一轮（now + 60 秒）为止：那一轮的窗口里正好 60 次
    let now = OffsetDateTime::now_utc();
    let end = now + Duration::seconds(60);
    probes(&t, ea, end, 60, 8, 18).await;
    probes(&t, eb, end, 60, 5, 15).await;
    probes(&t, ec, end, 60, 2, 0).await;
    probes(&t, ed, end, 20, 5, 0).await;
    let (mut ls, mut s) = (LossState::default(), AlertState::default());
    pass(&t, &mut ls, &mut s, now).await;
    assert!(firing(&t).await.is_empty(), "告警照常去抖 60 秒");
    assert_eq!(t.health(a).await, ("degraded".to_string(), vec!["tcp_loss_high".to_string()]), "健康马上降级");
    // 这几台没有出口探测，健康是「未知」而不是「正常」：只看有没有这条原因
    assert!(t.health(c).await.1.is_empty(), "60 次里偶发 2 次失败");
    assert!(t.health(d).await.1.is_empty(), "20 次的小样本不判");
    pass(&t, &mut ls, &mut s, now + Duration::seconds(60)).await;
    let inc = firing(&t).await;
    assert_eq!(inc.len(), 2, "{inc:?}");
    let ia = inc.iter().find(|i| i["subject_id"] == json!(a)).unwrap();
    assert_eq!((ia["rule_name"].as_str(), ia["severity"].as_str()), (Some("持续握手丢包"), Some("warning")));
    assert_eq!(
        ia["summary"],
        "团队1-01：近 30 分钟线路机到节点 TCP 探测 60 次，失败 8 次（13%），成功的 52 次里 18 次 SYN 重传（35%）；同供应商 DeerVPS 共 2 台同时（可能是供应商入站丢包）"
    );
    assert_eq!(ia["suggested"], json!(["fleetctl diag 团队1-01"]));
    let m = t.get(&format!("/api/v1/machines/{b}")).await;
    assert_eq!(m["health"]["reasons"][0]["rule_name"], "持续握手丢包", "{m}");
}

/// 恢复有滞回：落在两个阈值之间（3%）不关；中控重启后（新的 LossState）在告警的照样不关；
/// 两个比例都低于一半阈值连续 10 分钟才算恢复，告警再去抖 60 秒后恢复
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_has_hysteresis_and_survives_restart() {
    let t = setup().await;
    let relay = online_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let (a, ea) = seed_node(&t, relay, team, 1, "203.0.113.5", 20001).await;
    let t0 = OffsetDateTime::now_utc();
    probes(&t, ea, t0 + Duration::seconds(60), 60, 0, 12).await;
    let (mut ls, mut s) = (LossState::default(), AlertState::default());
    pass(&t, &mut ls, &mut s, t0).await;
    pass(&t, &mut ls, &mut s, t0 + Duration::seconds(60)).await;
    let inc = firing(&t).await;
    assert_eq!(inc.len(), 1);
    assert_eq!(inc[0]["summary"], "团队1-01：近 30 分钟线路机到节点 TCP 探测 60 次，失败 0 次（0%），成功的 60 次里 12 次 SYN 重传（20%）", "只有重传高也开");
    // 下一个 30 分钟：60 次里 6 次重传（10%）——低于 15%，但不低于一半（7.5%）：不关（窗口往后挪也一直在 10% 上下）。中控同时重启了
    let t1 = t0 + Duration::minutes(32);
    probes(&t, ea, t1, 60, 0, 6).await;
    let mut ls = LossState::default();
    let mut s = AlertState::default();
    for k in 0..12 {
        pass(&t, &mut ls, &mut s, t1 + Duration::minutes(k)).await;
    }
    assert_eq!(firing(&t).await.len(), 1, "比例落在两个阈值之间：重启后也不恢复");
    assert_eq!(t.health(a).await.0, "degraded");
    // 再下一个 30 分钟全好：开始计时，满 10 分钟才放开，告警再去抖 60 秒
    let t2 = t1 + Duration::minutes(31);
    probes(&t, ea, t2, 60, 0, 0).await;
    pass(&t, &mut ls, &mut s, t2).await;
    pass(&t, &mut ls, &mut s, t2 + Duration::minutes(9)).await;
    assert_eq!(firing(&t).await.len(), 1, "低于一半阈值不满 10 分钟");
    pass(&t, &mut ls, &mut s, t2 + Duration::minutes(10)).await;
    assert!(t.health(a).await.1.is_empty());
    pass(&t, &mut ls, &mut s, t2 + Duration::minutes(11)).await;
    assert!(firing(&t).await.is_empty());
}

/// 闲置备用（备用团队里、没有软路由出口指着）：健康照常降级（卡片上看得到），不开告警，也不算进别人的「同供应商 N 台」
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_spare_degrades_but_does_not_alert() {
    let t = setup().await;
    let relay = online_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let deer = provider(&t, "DeerVPS", "DV").await;
    let (a, ea) = seed_node(&t, relay, team, 1, "203.0.113.5", 20001).await;
    let (sp, es) = seed_node(&t, relay, team, 2, "203.0.113.6", 20002).await;
    for n in [a, sp] {
        set_provider(&t, n, deer).await;
    }
    let spare: Uuid = sqlx::query_scalar("select id from team where spare").fetch_one(&t.st.db).await.unwrap();
    sqlx::query("update machine set team_id = $2, number = 1, spare_since = now() where id = $1").bind(sp).bind(spare).execute(&t.st.db).await.unwrap();
    let now = OffsetDateTime::now_utc();
    probes(&t, ea, now + Duration::seconds(60), 60, 8, 18).await;
    probes(&t, es, now + Duration::seconds(60), 60, 8, 18).await;
    let (mut ls, mut s) = (LossState::default(), AlertState::default());
    pass(&t, &mut ls, &mut s, now).await;
    pass(&t, &mut ls, &mut s, now + Duration::seconds(60)).await;
    assert_eq!(t.health(sp).await.0, "degraded");
    let inc = firing(&t).await;
    assert_eq!(inc.len(), 1, "闲置备用不开：{inc:?}");
    assert_eq!(inc[0]["subject_id"], json!(a));
    assert!(!inc[0]["summary"].as_str().unwrap().contains("同供应商"), "{}", inc[0]["summary"]);
}

/// 经离线线路机的入口不算：线路机 agent 不报了，样本就不够，不开（开着的也不恢复，同 tcp_fail）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_relay_is_not_counted() {
    let t = setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (a, ea) = seed_node(&t, relay, team, 1, "203.0.113.5", 20001).await;
    let now = OffsetDateTime::now_utc();
    probes(&t, ea, now, 60, 30, 0).await;
    let (mut ls, mut s) = (LossState::default(), AlertState::default());
    pass(&t, &mut ls, &mut s, now).await;
    pass(&t, &mut ls, &mut s, now + Duration::seconds(60)).await;
    assert!(firing(&t).await.is_empty());
    assert!(!t.health(a).await.1.contains(&"tcp_loss_high".to_string()));
}

/// 入口切成穿透：线路机 → 节点不再有 TCP 探测，窗口里切换之前的探测也不再算；节点上没有别的要探测的入口时开着的直接撤掉
/// （不能按「样本不够、保持」一直开着——生产团队2-08 开穿透后就会这样）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switching_to_reverse_clears() {
    let t = setup().await;
    let relay = online_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let (a, ea) = seed_node(&t, relay, team, 1, "203.0.113.5", 20001).await;
    let now = OffsetDateTime::now_utc();
    probes(&t, ea, now + Duration::seconds(60), 60, 8, 18).await;
    let (mut ls, mut s) = (LossState::default(), AlertState::default());
    pass(&t, &mut ls, &mut s, now).await;
    pass(&t, &mut ls, &mut s, now + Duration::seconds(60)).await;
    assert_eq!(firing(&t).await.len(), 1);
    sqlx::query("update entry set mode = 'reverse' where id = $1").bind(ea).execute(&t.st.db).await.unwrap();
    pass(&t, &mut ls, &mut s, now + Duration::seconds(120)).await;
    assert!(!t.health(a).await.1.contains(&"tcp_loss_high".to_string()), "切到穿透：立刻不再降级");
    pass(&t, &mut ls, &mut s, now + Duration::seconds(180)).await;
    assert!(firing(&t).await.is_empty(), "告警去抖后关");
}
