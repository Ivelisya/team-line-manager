//! 备用团队：团队的 spare 开关（接口、机器视图、审计）；到期自动删除（spare.rs）——按账单时区判定「过期满 1 天」，
//! 跳过非备用团队、没有订阅、已在退役、有进行中作业的机器，同一台不重复建作业；自动退役与手动同一条路径（作业参数与审计带 auto、
//! 操作人「系统」、发一条通知），退役完成后接着彻底删除（被拦的等条件满足后的那一轮），同机 IP 节点随宿主机退役、先于宿主机删除。
//! 需要 PostgreSQL（见 common）。

mod common;

use axum::http::StatusCode;
use common::{seed_node, seed_relay, seed_team, setup, T};
use fleet_server::spare;
use serde_json::{json, Value};
use time::macros::datetime;
use uuid::Uuid;

async fn spare_team(t: &T, name: &str) -> Uuid {
    t.post("/api/v1/teams", json!({"name": name, "spare": true})).await["id"].as_str().unwrap().parse().unwrap()
}

async fn sub(t: &T, m: Uuid, due: &str, tz: &str) {
    sqlx::query("insert into subscription (machine_id, price_cents, currency, due_date, billing_tz) values ($1, 500, 'USD', $2::date, $3)")
        .bind(m)
        .bind(due)
        .bind(tz)
        .execute(&t.st.db)
        .await
        .unwrap();
}

/// 机器最近的某类作业：(作业 id, 参数, 创建者)
async fn last_job(t: &T, m: Uuid, kind: &str) -> Option<(Uuid, Value, String)> {
    sqlx::query_as("select id, params, created_by from job where target_id = $1 and kind = $2 order by created_at desc limit 1")
        .bind(m)
        .bind(kind)
        .fetch_optional(&t.st.db)
        .await
        .unwrap()
}

async fn gone(t: &T, m: Uuid) -> bool { t.req("GET", &format!("/api/v1/machines/{m}"), None, None).await.0 == StatusCode::NOT_FOUND }

/// 直接在库里放一台 VPS（`host` 给了就是它上面的同机 IP 节点，不存 SSH 端口）
async fn seed_vps(t: &T, team: Uuid, number: i32, ip: &str, host: Option<Uuid>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into machine (id, kind, team_id, number, primary_ip, ssh_port, lifecycle, host_id, activated_at)
         values ($1, 'vps', $2, $3, $4::inet, $5, 'active', $6, now())",
    )
    .bind(id)
    .bind(team)
    .bind(number)
    .bind(ip)
    .bind(host.is_none().then_some(1))
    .bind(host)
    .execute(&t.st.db)
    .await
    .unwrap();
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spare_flag_in_teams_views_and_audit() {
    let t = setup(false).await;
    let sp = spare_team(&t, "备用").await;
    let normal = seed_team(&t, "团队1").await;
    let teams = t.get("/api/v1/teams").await;
    let flag = |id: Uuid| teams.as_array().unwrap().iter().find(|x| x["id"] == id.to_string()).unwrap()["spare"].clone();
    assert_eq!((flag(sp), flag(normal)), (json!(true), json!(false)), "{teams}");

    let m = seed_node(&t, sp, 1, "203.0.113.11", "active").await;
    let n = seed_node(&t, normal, 1, "203.0.113.12", "active").await;
    assert_eq!(t.get(&format!("/api/v1/machines/{m}")).await["spare"], true);
    assert_eq!(t.get(&format!("/api/v1/machines/{n}")).await["spare"], false);
    // 列表走缓存视图：改了团队要跟着变
    let live = t.get("/api/v1/machines").await;
    assert_eq!(live.as_array().unwrap().iter().find(|x| x["id"] == m.to_string()).unwrap()["spare"], true);
    // 过期第 1 天：续费告警的文字写明明天自动删除（非备用团队的不提）
    for x in [m, n] {
        sqlx::query("insert into subscription (machine_id, price_cents, currency, due_date, billing_tz) values ($1, 500, 'USD', (now() at time zone 'UTC')::date - 1, 'UTC')")
            .bind(x)
            .execute(&t.st.db)
            .await
            .unwrap();
    }
    let renewal = |v: Value| v["health"]["reasons"].as_array().unwrap().iter().find(|r| r["key"] == "renewal_overdue").map(|r| r["text"].clone());
    assert_eq!(renewal(t.get(&format!("/api/v1/machines/{m}")).await), Some(json!("已过期 1 天，备用节点明天自动删除")));
    assert_eq!(renewal(t.get(&format!("/api/v1/machines/{n}")).await), Some(json!("已过期 1 天")));

    // 只改备用、不给名字：名字和排序不变
    let (s, v) = t.req("PATCH", &format!("/api/v1/teams/{sp}"), Some(json!({"spare": false})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let live = t.get("/api/v1/machines").await;
    assert_eq!(live.as_array().unwrap().iter().find(|x| x["id"] == m.to_string()).unwrap()["spare"], false);
    let teams = t.get("/api/v1/teams").await;
    let me = teams.as_array().unwrap().iter().find(|x| x["id"] == sp.to_string()).unwrap().clone();
    assert_eq!((me["name"].clone(), me["spare"].clone()), (json!("备用"), json!(false)), "{me}");
    t.req("PATCH", &format!("/api/v1/teams/{normal}"), Some(json!({"spare": true, "sort_order": 5})), None).await;
    let au = t.get("/api/v1/audit?cat=config").await;
    let sums: Vec<String> = au.as_array().unwrap().iter().filter(|a| a["action"] == "team.update" || a["action"] == "team.create").map(|a| a["summary"].as_str().unwrap().to_string()).collect();
    assert!(sums.contains(&"备用，取消备用".to_string()), "{sums:?}");
    assert!(sums.contains(&"团队1，排序 0 → 5，设为备用".to_string()), "{sums:?}");
    assert!(sums.contains(&"备用（备用）".to_string()), "{sums:?}");
}

/// 到期判断：账单时区里「到期日 + 1 天」那天结束后才删；不同时区各按各的；非备用、没订阅、退役中的不动；有进行中作业的这一轮跳过；不重复建作业
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expiry_boundaries_by_billing_timezone() {
    let t = setup(false).await;
    let sp = spare_team(&t, "备用").await;
    let normal = seed_team(&t, "团队1").await;
    let sh = seed_node(&t, sp, 1, "203.0.113.11", "active").await;
    sub(&t, sh, "2026-10-20", "Asia/Shanghai").await;
    let la = seed_node(&t, sp, 2, "203.0.113.12", "active").await;
    sub(&t, la, "2026-10-20", "America/Los_Angeles").await;
    let other = seed_node(&t, normal, 1, "203.0.113.13", "active").await;
    sub(&t, other, "2026-10-20", "Asia/Shanghai").await;
    let nosub = seed_node(&t, sp, 3, "203.0.113.14", "active").await;
    let busy = seed_node(&t, sp, 4, "203.0.113.15", "active").await;
    sub(&t, busy, "2026-10-20", "Asia/Shanghai").await;
    let retiring = seed_node(&t, sp, 5, "203.0.113.16", "retiring").await;
    sub(&t, retiring, "2026-10-01", "Asia/Shanghai").await;
    let renewed = seed_node(&t, sp, 6, "203.0.113.17", "active").await;
    sub(&t, renewed, "2026-11-20", "Asia/Shanghai").await;
    let bj = Uuid::now_v7();
    sqlx::query("insert into job (id, kind, target_id, state, created_by) values ($1, 'upgrade_agent', $2, 'queued', 'test')").bind(bj).bind(busy).execute(&t.st.db).await.unwrap();
    // 只收「严重」的渠道也要收到自动删除（不看级别）
    sqlx::query("insert into notify_channel (id, kind, name, config_enc, min_severity) values ($1, 'webhook', 'wh', '\\x00', 'critical')")
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();

    // 到期日当天、上海的到期日 + 1 天最后一秒（10-21 23:59:59）：都还留着
    for now in [datetime!(2026-10-20 12:00 UTC), datetime!(2026-10-21 15:59:59 UTC)] {
        let r = spare::run_once(&t.st, now).await.unwrap();
        assert!(r.retired.is_empty() && r.busy.is_empty() && r.purged.is_empty(), "{now}: {r:?}");
    }
    // 上海 10-22 00:00：上海的删；洛杉矶此刻是 10-21 09:00，还留着；有进行中作业的跳过
    let r = spare::run_once(&t.st, datetime!(2026-10-21 16:00 UTC)).await.unwrap();
    assert_eq!((r.retired.as_slice(), r.busy.as_slice()), ([sh].as_slice(), [busy].as_slice()), "{r:?}");
    assert_eq!(t.machine_lifecycle(sh).await, "retiring");
    let (_, p, by) = last_job(&t, sh, "retire").await.unwrap();
    assert_eq!((p["auto"].clone(), p["cleanup"].clone(), p["reason"].clone(), by.as_str()), (json!("spare_expiry"), json!(true), json!("备用到期自动删除（到期日 2026-10-20）"), "系统"));
    let reason: Option<String> = sqlx::query_scalar("select lifecycle_reason from machine where id = $1").bind(sh).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(reason.as_deref(), Some("备用到期自动删除（到期日 2026-10-20）"));
    let (actor, detail): (String, Value) = sqlx::query_as("select actor, detail from audit_log where target_id = $1 and action = 'machine.retire'").bind(sh).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((actor.as_str(), &detail["auto"]), ("系统", &json!("spare_expiry")), "{detail}");
    let sent: i64 = sqlx::query_scalar(
        "select (select count(*) from notify_outbox where notice->>'kind' = 'auto' and notice->>'machine' = '备用-01' and notice->>'summary' like '%到期日 2026-10-20%')
              + (select count(*) from notify_log where kind = 'auto')",
    )
    .fetch_one(&t.st.db)
    .await
    .unwrap();
    assert_eq!(sent, 1, "发一条自动删除通知");

    // 洛杉矶 10-22 00:00（PDT = UTC-7）
    let r = spare::run_once(&t.st, datetime!(2026-10-22 06:59:59 UTC)).await.unwrap();
    assert!(r.retired.is_empty(), "{r:?}");
    let r = spare::run_once(&t.st, datetime!(2026-10-22 07:00 UTC)).await.unwrap();
    assert_eq!(r.retired, [la], "{r:?}");
    // 再来一轮：已经在退役的不重复建作业
    let r = spare::run_once(&t.st, datetime!(2026-10-22 08:00 UTC)).await.unwrap();
    assert!(r.retired.is_empty(), "{r:?}");
    let n: i64 = sqlx::query_scalar("select count(*) from job where kind = 'retire'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 2);
    // 进行中的作业结束了：下一轮删
    sqlx::query("update job set state = 'succeeded', finished_at = now() where id = $1").bind(bj).execute(&t.st.db).await.unwrap();
    let r = spare::run_once(&t.st, datetime!(2026-10-22 09:00 UTC)).await.unwrap();
    assert_eq!((r.retired.as_slice(), r.busy.len()), ([busy].as_slice(), 0), "{r:?}");
    // 非备用团队、没有订阅、续费顺延了的照旧在用；本来就在退役中的不另建作业
    for m in [other, nosub, renewed] {
        assert_eq!(t.machine_lifecycle(m).await, "active");
    }
    assert!(last_job(&t, retiring, "retire").await.is_none());
}

/// 自动退役完成后接着彻底删除；入口流量还计入线路机统计的等满 35 天；手动退役的备用机器不自动删
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_purge_after_retire_when_allowed() {
    let t = setup(true).await;
    let sp = spare_team(&t, "备用").await;
    let a = seed_node(&t, sp, 1, "203.0.113.21", "active").await;
    sub(&t, a, "2030-01-01", "UTC").await;
    let b = seed_node(&t, sp, 2, "203.0.113.22", "active").await;
    sub(&t, b, "2030-01-01", "UTC").await;
    let manual = seed_node(&t, sp, 3, "203.0.113.23", "retired").await;
    let (relay, _) = seed_relay(&t).await;
    let eid = Uuid::now_v7();
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, 20001, $4, 'ab', 'removed')")
        .bind(eid)
        .bind(relay)
        .bind(b)
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, date_trunc('hour', now()) - interval '1 hour', 100, 0)")
        .bind(eid)
        .execute(&t.st.db)
        .await
        .unwrap();

    let now = datetime!(2030-01-05 00:00 UTC);
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert_eq!(r.retired, [a, b], "{r:?}");
    for m in [a, b] {
        let (j, _, _) = last_job(&t, m, "retire").await.unwrap();
        t.wait_job(&j.to_string(), "succeeded").await;
    }
    // 退役完：a 接着删；b 的入口近 35 天还有流量计入线路机的统计，先不删；手动退役的不碰
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert_eq!(r.purged, [a], "{r:?}");
    let (pj, p, by) = last_job(&t, a, "purge").await.unwrap();
    assert_eq!((p["auto"].clone(), by.as_str()), (json!("spare_expiry"), "系统"));
    t.wait_job(&pj.to_string(), "succeeded").await;
    assert!(gone(&t, a).await);
    let au: (String, Value) = sqlx::query_as("select actor, detail from audit_log where target_id = $1 and action = 'machine.purge'").bind(a).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((au.0.as_str(), &au.1["auto"]), ("系统", &json!("spare_expiry")));
    assert_eq!((t.machine_lifecycle(b).await, t.machine_lifecycle(manual).await), ("retired".to_string(), "retired".to_string()));

    // 流量都在 35 天前了：下一轮删 b
    sqlx::query("update entry_traffic_hourly set hour = date_trunc('hour', now()) - interval '36 days' where entry_id = $1").bind(eid).execute(&t.st.db).await.unwrap();
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert_eq!(r.purged, [b], "{r:?}");
    let (pj, _, _) = last_job(&t, b, "purge").await.unwrap();
    t.wait_job(&pj.to_string(), "succeeded").await;
    assert!(gone(&t, b).await);
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert!(r.purged.is_empty() && r.retired.is_empty(), "{r:?}");
    assert_eq!(t.machine_lifecycle(manual).await, "retired", "手动退役的备用机器不自动删");
}

/// 宿主机到期：同机 IP 节点按现有退役规则一并退役（不清理、作业也带 auto），删除时先删同机 IP 节点，宿主机下一轮再删
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_cascades_to_ip_nodes() {
    let t = setup(true).await;
    let sp = spare_team(&t, "备用").await;
    let normal = seed_team(&t, "团队1").await;
    // 宿主机指向 127.0.0.1:1：清理时连不上，跳过清理（令牌照样撤销）
    let host = seed_vps(&t, sp, 1, "127.0.0.1", None).await;
    sub(&t, host, "2030-01-01", "UTC").await;
    let x = seed_vps(&t, normal, 1, "203.0.113.31", Some(host)).await;

    let now = datetime!(2030-01-05 00:00 UTC);
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert_eq!(r.retired, [host], "{r:?}");
    let (hj, hp, _) = last_job(&t, host, "retire").await.unwrap();
    let (xj, xp, xby) = last_job(&t, x, "retire").await.unwrap();
    assert_eq!((hp["cleanup"].clone(), xp["cleanup"].clone(), xp["auto"].clone(), xby.as_str()), (json!(true), json!(false), json!("spare_expiry"), "系统"));
    for j in [hj, xj] {
        t.wait_job(&j.to_string(), "succeeded").await;
    }
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert_eq!(r.purged, [x], "宿主机要等同机 IP 节点先删：{r:?}");
    let (pj, _, _) = last_job(&t, x, "purge").await.unwrap();
    t.wait_job(&pj.to_string(), "succeeded").await;
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert_eq!(r.purged, [host], "{r:?}");
    let (pj, _, _) = last_job(&t, host, "purge").await.unwrap();
    t.wait_job(&pj.to_string(), "succeeded").await;
    assert!(gone(&t, host).await && gone(&t, x).await);
}
