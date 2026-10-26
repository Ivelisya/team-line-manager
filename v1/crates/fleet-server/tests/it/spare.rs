//! 备用团队与回收站：退役 = 移进唯一的备用团队（编号、spare_from、原团队压缩、审计、宿主机带上同机 IP 节点、进不了备用的说明原因）；
//! 到期回收（spare.rs）——按账单时区判定「过期满 1 天」，跳过非备用团队、没有订阅、已在退役、有进行中作业的机器，同一台不重复建作业；
//! 退役作业收尾时同一个事务移进回收站（行内容、recycled_at、视图里看不到），已退役没进回收站的由维护任务收进去，
//! db::purge_blocker 放行后自动物理删除（同机 IP 节点先于宿主机）；旧面板导入的幂等与防导错。
//! 需要 PostgreSQL（见 common）。

use crate::common;

use axum::http::StatusCode;
use common::{seed_node, seed_relay, seed_team, setup, T};
use fleet_server::spare;
use serde_json::{json, Value};
use time::macros::datetime;
use uuid::Uuid;

/// 迁移建好的唯一备用团队
async fn spare_team(t: &T) -> Uuid { sqlx::query_scalar("select id from team where spare").fetch_one(&t.st.db).await.unwrap() }

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

/// 机器视图里看不到了（进了回收站，或物理删除了）
async fn hidden(t: &T, m: Uuid) -> bool { t.req("GET", &format!("/api/v1/machines/{m}"), None, None).await.0 == StatusCode::NOT_FOUND }

async fn gone(t: &T, m: Uuid) -> bool {
    sqlx::query_scalar::<_, i64>("select count(*) from machine where id = $1").bind(m).fetch_one(&t.st.db).await.unwrap() == 0
}

/// 回收站里这台机器的一行
async fn bin_of(t: &T, m: Uuid) -> Value {
    sqlx::query_scalar("select to_jsonb(b) from recycle_bin b where source = 'fleet' and machine_id = $1").bind(m).fetch_one(&t.st.db).await.unwrap()
}

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

/// 备用团队由系统管理：迁移建好唯一的「备用」，不能删、可以改名；视图带 spare 标记，续费提醒写明哪天自动回收
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spare_team_is_system_managed() {
    let t = setup(false).await;
    let sp = spare_team(&t).await;
    let teams = t.get("/api/v1/teams").await;
    let me = teams.as_array().unwrap().iter().find(|x| x["id"] == sp.to_string()).unwrap().clone();
    assert_eq!((me["name"].clone(), me["spare"].clone()), (json!("备用"), json!(true)), "{teams}");
    let (s, v) = t.req("DELETE", &format!("/api/v1/teams/{sp}"), None, None).await;
    assert!(s == StatusCode::CONFLICT && v.to_string().contains("备用团队"), "{s} {v}");
    // 第二个备用团队建不出来（唯一索引兜底；接口本来就不收 spare）
    let normal = seed_team(&t, "团队1").await;
    assert!(sqlx::query("update team set spare = true where id = $1").bind(normal).execute(&t.st.db).await.is_err());

    let m = seed_node(&t, sp, 1, "203.0.113.11", "active").await;
    let n = seed_node(&t, normal, 1, "203.0.113.12", "active").await;
    for x in [m, n] {
        sqlx::query("insert into subscription (machine_id, price_cents, currency, due_date, billing_tz) values ($1, 500, 'USD', (now() at time zone 'UTC')::date - 1, 'UTC')")
            .bind(x)
            .execute(&t.st.db)
            .await
            .unwrap();
    }
    let v = t.get(&format!("/api/v1/machines/{m}")).await;
    assert_eq!((v["spare"].clone(), v["recycle_on"].is_string()), (json!(true), true), "{v}");
    assert_eq!(t.get(&format!("/api/v1/machines/{n}")).await["recycle_on"], Value::Null);
    let renewal = |v: Value| v["health"]["reasons"].as_array().unwrap().iter().find(|r| r["key"] == "renewal_due").map(|r| r["text"].clone());
    assert_eq!(renewal(v), None, "备用的不提醒续费（没有软路由出口指向它）");
    assert_eq!(renewal(t.get(&format!("/api/v1/machines/{n}")).await), Some(json!("已过期 1 天")));
    // 改名照旧：显示名跟着变
    let (s, v) = t.req("PATCH", &format!("/api/v1/teams/{sp}"), Some(json!({"name": "团队2 备用"})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(t.get(&format!("/api/v1/machines/{m}")).await["name"], "团队2 备用-01");
}

/// 退役默认移进备用：最小的空号、记下原名和时刻、原团队压缩、不建作业、审计写去向；宿主机带上它的同机 IP 节点（编号挨着）。
/// 进不了备用的说明原因（加 now 才能立即回收）；移出备用 = 撤销退役，标记清掉。开着的自动续费进备用时关掉（移出不自动打开），
/// 预览与结果都写明；原团队压缩时提示撤销退役回不到原编号
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retire_moves_into_spare() {
    let t = setup(false).await;
    let sp = spare_team(&t).await;
    let team = seed_team(&t, "团队1").await;
    seed_node(&t, sp, 1, "203.0.113.10", "active").await;
    let n1 = seed_node(&t, team, 1, "203.0.113.11", "active").await;
    let n2 = seed_node(&t, team, 2, "203.0.113.12", "active").await;
    let n3 = seed_node(&t, team, 3, "203.0.113.13", "active").await;
    sub(&t, n1, "2030-01-01", "UTC").await;
    sqlx::query("update subscription set auto_renew = true where machine_id = $1").bind(n1).execute(&t.st.db).await.unwrap();
    let auto = |m: Uuid| {
        let db = t.st.db.clone();
        async move { sqlx::query_scalar::<_, bool>("select auto_renew from subscription where machine_id = $1").bind(m).fetch_one(&db).await.unwrap() }
    };

    let pv = t.get(&format!("/api/v1/machines/{n1}/retire")).await;
    assert_eq!(pv["spare"]["ok"], true, "{pv}");
    assert_eq!(
        pv["spare"]["notes"],
        json!([
            "这台开着自动续费：进备用时会关掉，到期后照常回收（供应商那边的自动扣款要自己去关）",
            "原团队后面的节点会前移补号：撤销退役（改回原团队）拿到的是那时最小的空号，回不到团队1-01"
        ])
    );
    assert_eq!((pv["spare"]["to_name"].as_str(), pv["spare"]["recycle_on"].as_str()), (Some("备用-02"), Some("2030-01-03")), "{pv}");
    assert_eq!(pv["renames"], json!([{"machine_id": n2, "old_name": "团队1-02", "new_name": "团队1-01"}, {"machine_id": n3, "old_name": "团队1-03", "new_name": "团队1-02"}]));
    assert_eq!(pv["routers"], json!([]));
    let r = t.post(&format!("/api/v1/machines/{n1}/retire"), json!({"confirm": "团队1-01", "reason": "换新机"})).await;
    assert_eq!(
        r,
        json!({"mode": "spare", "name": "备用-02", "from": "团队1-01", "recycle_on": "2030-01-03", "ip_nodes": [], "auto_renew_off": true,
               "notes": ["自动续费已关闭（供应商那边的自动扣款要自己去关）", "原团队后面的节点已前移补号：撤销退役（改回原团队）拿到的是那时最小的空号，回不到团队1-01"]})
    );
    assert!(!auto(n1).await, "进备用关掉自动续费");
    let au = t.get(&format!("/api/v1/audit?target_id={n1}")).await;
    assert!(au.as_array().unwrap().iter().any(|a| a["action"] == "machine.rename" && a["detail"]["auto_renew_off"] == true), "{au}");
    let v = t.get(&format!("/api/v1/machines/{n1}")).await;
    assert_eq!(
        (v["name"].as_str(), v["lifecycle"].as_str(), v["spare_from"].as_str(), v["lifecycle_reason"].as_str(), v["spare_since"].is_string()),
        (Some("备用-02"), Some("active"), Some("团队1-01"), Some("换新机"), true),
        "{v}"
    );
    assert_eq!(t.get(&format!("/api/v1/machines/{n2}")).await["name"], "团队1-01", "原团队压缩");
    assert!(last_job(&t, n1, "retire").await.is_none(), "进备用不建作业");
    let au = t.get("/api/v1/audit?cat=machine").await;
    assert!(au.as_array().unwrap().iter().any(|a| a["action"] == "machine.retire" && a["summary"] == "团队1-01 → 备用-02（换新机）"), "{au}");

    // 进不了备用：没有订阅的、本来就在备用里的；加 now 立即回收照常
    let pv = t.get(&format!("/api/v1/machines/{n2}/retire")).await;
    assert!(pv["spare"]["ok"] == false && pv["spare"]["why"].as_str().unwrap().contains("没有订阅"), "{pv}");
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{n2}/retire"), Some(json!({"confirm": "团队1-01"})), None).await;
    assert!(s == StatusCode::CONFLICT && v.to_string().contains("立即回收"), "{s} {v}");
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{n1}/retire"), Some(json!({"confirm": "备用-02"})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "已经在备用里");
    let r = t.post(&format!("/api/v1/machines/{n2}/retire"), json!({"confirm": "团队1-01", "now": true, "cleanup": false})).await;
    assert!(r["mode"] == "now" && r["job_id"].is_string(), "{r}");

    // 移出备用 = 撤销退役：标记、退役原因清掉，不再自动回收。照提示只给团队（备用里的 2 号在原团队已经有了）：排到原团队的最小空号
    let rev = t.get(&format!("/api/v1/machines/{n1}")).await["revision"].clone();
    let (s, v) = t.req("PATCH", &format!("/api/v1/machines/{n1}"), Some(json!({"revision": rev, "team_id": team})), None).await;
    let v = &v["data"];
    assert_eq!((s, v["name"].as_str(), v["spare"].as_bool()), (StatusCode::OK, Some("团队1-03"), Some(false)), "{v}");
    assert_eq!((v["spare_from"].clone(), v["spare_since"].clone(), v["recycle_on"].clone(), v["lifecycle_reason"].clone()), (Value::Null, Value::Null, Value::Null, Value::Null), "{v}");
    assert!(!auto(n1).await, "移出备用不自动打开");

    // 宿主机进备用：上面的同机 IP 节点一起进，编号挨着
    let host = seed_vps(&t, team, 7, "203.0.113.20", None).await;
    let x = seed_vps(&t, team, 8, "203.0.113.21", Some(host)).await;
    sub(&t, host, "2030-01-01", "UTC").await;
    let pv = t.get(&format!("/api/v1/machines/{host}/retire")).await;
    assert_eq!(pv["spare"]["ip_nodes"], json!([{"id": x, "name": "团队1-08", "to_name": "备用-03"}]), "{pv}");
    let r = t.post(&format!("/api/v1/machines/{host}/retire"), json!({"confirm": "团队1-07"})).await;
    assert_eq!((r["name"].as_str(), r["ip_nodes"].clone()), (Some("备用-02"), json!([{"id": x, "name": "备用-03", "from": "团队1-08"}])), "{r}");
    let xv = t.get(&format!("/api/v1/machines/{x}")).await;
    assert_eq!((xv["spare_from"].as_str(), xv["lifecycle_reason"].as_str()), (Some("团队1-08"), Some("随宿主机「团队1-07」退役")), "{xv}");
}

/// 撤销退役（宿主机从备用改回原团队）只带与它同一次进备用的 IP 节点：之前单独退役进备用的 IP 节点留在备用（spare_since 不清、仍会自动回收）；
/// 带走的 IP 节点有进行中的作业时 409，预览与实际一致
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_retire_only_takes_ip_nodes_retired_with_the_host() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队1").await;
    let host = seed_vps(&t, team, 1, "203.0.113.41", None).await;
    let x = seed_vps(&t, team, 2, "203.0.113.42", Some(host)).await;
    let y = seed_vps(&t, team, 3, "203.0.113.43", Some(host)).await;
    for m in [host, x, y] {
        sub(&t, m, "2030-01-01", "UTC").await;
    }
    // X 先单独退役进备用（宿主机还在团队1），再宿主机退役：带走 Y，X 已在备用不动
    let r = t.post(&format!("/api/v1/machines/{x}/retire"), json!({"confirm": "团队1-02"})).await;
    assert_eq!(r["mode"], "spare", "{r}");
    let r = t.post(&format!("/api/v1/machines/{host}/retire"), json!({"confirm": "团队1-01"})).await;
    assert_eq!(r["ip_nodes"].as_array().unwrap().len(), 1, "{r}");
    let spare_since = |m: Uuid| {
        let db = t.st.db.clone();
        async move { sqlx::query_scalar::<_, Option<time::OffsetDateTime>>("select spare_since from machine where id = $1").bind(m).fetch_one(&db).await.unwrap() }
    };
    let x_since = spare_since(x).await;
    assert!(x_since.is_some() && spare_since(y).await == spare_since(host).await && spare_since(host).await != x_since);

    // 预览：只列 Y
    let pv = t.get(&format!("/api/v1/machines/{host}/rename-preview?team_id={team}")).await;
    let ips: Vec<&Value> = pv["ip_nodes"].as_array().unwrap().iter().collect();
    assert!(ips.len() == 1 && ips[0]["machine_id"] == y.to_string(), "{pv}");
    // Y 有进行中的作业：整个 PATCH 回滚
    let bj = Uuid::now_v7();
    sqlx::query("insert into job (id, kind, target_id, state, created_by) values ($1, 'upgrade_agent', $2, 'running', 'test')").bind(bj).bind(y).execute(&t.st.db).await.unwrap();
    let rev = t.get(&format!("/api/v1/machines/{host}")).await["revision"].clone();
    let (s, v) = t.req("PATCH", &format!("/api/v1/machines/{host}"), Some(json!({"revision": rev, "team_id": team})), None).await;
    assert!(s == StatusCode::CONFLICT && v.to_string().contains("进行中的作业") && v.to_string().contains("备用-"), "{s} {v}");
    assert_eq!(t.get(&format!("/api/v1/machines/{host}")).await["spare"], true, "回滚：宿主机还在备用");
    sqlx::query("update job set state = 'succeeded' where id = $1").bind(bj).execute(&t.st.db).await.unwrap();

    let rev = t.get(&format!("/api/v1/machines/{host}")).await["revision"].clone();
    let (s, v) = t.req("PATCH", &format!("/api/v1/machines/{host}"), Some(json!({"revision": rev, "team_id": team})), None).await;
    assert_eq!((s, v["data"]["spare"].clone()), (StatusCode::OK, json!(false)), "{v}");
    let (yv, xv) = (t.get(&format!("/api/v1/machines/{y}")).await, t.get(&format!("/api/v1/machines/{x}")).await);
    assert_eq!((yv["spare"].clone(), yv["spare_since"].clone()), (json!(false), Value::Null), "Y 跟着回团队1：{yv}");
    assert_eq!(xv["spare"], true, "X 仍在备用：{xv}");
    assert_eq!(spare_since(x).await, x_since, "X 的 spare_since 不变");
}

/// 到期判断：账单时区里「到期日 + 1 天」那天结束后才回收；不同时区各按各的；非备用、没订阅、退役中的不动；有进行中作业的这一轮跳过；不重复建作业
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expiry_boundaries_by_billing_timezone() {
    let t = setup(false).await;
    let sp = spare_team(&t).await;
    let normal = seed_team(&t, "团队1").await;
    let sh = seed_node(&t, sp, 1, "203.0.113.11", "active").await;
    sub(&t, sh, "2026-10-20", "Asia/Shanghai").await;
    sqlx::query("update machine set lifecycle_reason = '换新机' where id = $1").bind(sh).execute(&t.st.db).await.unwrap();
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
    // 只收「严重」的渠道也要收到自动回收（不看级别）
    sqlx::query("insert into notify_channel (id, kind, name, config_enc, min_severity) values ($1, 'feishu', 'wh', '\\x00', 'critical')")
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();

    // 到期日当天、上海的到期日 + 1 天最后一秒（10-21 23:59:59）：都还留着
    for now in [datetime!(2026-10-20 12:00 UTC), datetime!(2026-10-21 15:59:59 UTC)] {
        let r = spare::run_once(&t.st, now).await.unwrap();
        assert!(r.retired.is_empty() && r.busy.is_empty() && r.purged.is_empty(), "{now}: {r:?}");
    }
    // 上海 10-22 00:00：上海的回收；洛杉矶此刻是 10-21 09:00，还留着；有进行中作业的跳过
    let r = spare::run_once(&t.st, datetime!(2026-10-21 16:00 UTC)).await.unwrap();
    assert_eq!((r.retired.as_slice(), r.busy.as_slice()), ([sh].as_slice(), [busy].as_slice()), "{r:?}");
    assert_eq!(t.machine_lifecycle(sh).await, "retiring");
    // 进备用时的退役原因留着，后面接上为什么回收
    let (_, p, by) = last_job(&t, sh, "retire").await.unwrap();
    assert_eq!((p["auto"].clone(), p["cleanup"].clone(), p["reason"].clone(), by.as_str()), (json!("spare_expiry"), json!(true), json!("换新机；备用到期自动回收（到期日 2026-10-20）"), "系统"));
    let (actor, detail): (String, Value) = sqlx::query_as("select actor, detail from audit_log where target_id = $1 and action = 'machine.retire'").bind(sh).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((actor.as_str(), &detail["auto"], &detail["mode"]), ("系统", &json!("spare_expiry"), &json!("now")), "{detail}");
    let sent: i64 = sqlx::query_scalar(
        "select (select count(*) from notify_outbox where notice->>'kind' = 'auto' and notice->>'machine' = '备用-01' and notice->>'summary' like '%到期日 2026-10-20%')
              + (select count(*) from notify_log where kind = 'auto')",
    )
    .fetch_one(&t.st.db)
    .await
    .unwrap();
    assert_eq!(sent, 1, "发一条自动回收通知");

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
    // 进行中的作业结束了：下一轮回收
    sqlx::query("update job set state = 'succeeded', finished_at = now() where id = $1").bind(bj).execute(&t.st.db).await.unwrap();
    let r = spare::run_once(&t.st, datetime!(2026-10-22 09:00 UTC)).await.unwrap();
    assert_eq!((r.retired.as_slice(), r.busy.len()), ([busy].as_slice(), 0), "{r:?}");
    // 非备用团队、没有订阅、续费顺延了的照旧在用；本来就在退役中的不另建作业
    for m in [other, nosub, renewed] {
        assert_eq!(t.machine_lifecycle(m).await, "active");
    }
    assert!(last_job(&t, retiring, "retire").await.is_none());
}

/// 开着自动续费的备用机器过期满 1 天也不回收（自动续费那一轮出错没顺延成时，不能当成过期清掉）；关掉自动续费的照回收
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_renew_spare_is_not_recycled() {
    let t = setup(false).await;
    let sp = spare_team(&t).await;
    let on = seed_node(&t, sp, 1, "203.0.113.11", "active").await;
    sub(&t, on, "2026-10-20", "Asia/Shanghai").await;
    sqlx::query("update subscription set auto_renew = true where machine_id = $1").bind(on).execute(&t.st.db).await.unwrap();
    let off = seed_node(&t, sp, 2, "203.0.113.12", "active").await;
    sub(&t, off, "2026-10-20", "Asia/Shanghai").await;
    let r = spare::run_once(&t.st, datetime!(2026-10-21 16:00 UTC)).await.unwrap();
    assert_eq!(r.retired, [off], "{r:?}");
    assert_eq!(t.machine_lifecycle(on).await, "active");
    assert!(last_job(&t, on, "retire").await.is_none());
}

/// 到期满 1 天：退役作业收尾时同一个事务进回收站（行内容、recycled_at、视图里看不到）；之后 purge_blocker 放行就自动物理删除，
/// 入口流量还计入线路机统计的等满 93 天；回收站这一行一直留着
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_spare_is_recycled_then_purged() {
    let t = setup(true).await;
    let sp = spare_team(&t).await;
    let a = seed_node(&t, sp, 1, "203.0.113.21", "active").await;
    sub(&t, a, "2030-01-01", "UTC").await;
    sqlx::query("update machine set spare_from = '团队2-07', spare_since = '2029-12-20T10:00:00Z' where id = $1").bind(a).execute(&t.st.db).await.unwrap();
    let b = seed_node(&t, sp, 2, "203.0.113.22", "active").await;
    sub(&t, b, "2030-01-01", "UTC").await;
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
    sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, date_trunc('hour', now()) - interval '1 hour', 100, 7)")
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
    // 收尾时已经进了回收站：名字取进备用前的，团队取那个名字的团队部分
    let ba = bin_of(&t, a).await;
    assert_eq!(
        (ba["name"].as_str(), ba["last_name"].as_str(), ba["team"].as_str(), ba["kind"].as_str(), ba["actor"].as_str(), ba["price_cents"].as_i64()),
        (Some("团队2-07"), Some("备用-01"), Some("团队2"), Some("proxy"), Some("系统"), Some(500)),
        "{ba}"
    );
    assert_eq!((ba["reason"].as_str(), ba["due_date"].as_str()), (Some("备用到期自动回收（到期日 2030-01-01）"), Some("2030-01-01")));
    let at: bool = sqlx::query_scalar("select retired_at = '2029-12-20T10:00:00Z' from recycle_bin where machine_id = $1").bind(a).fetch_one(&t.st.db).await.unwrap();
    assert!(at, "退役时刻取进备用的时刻");
    assert_eq!(ba["snapshot"]["name"], "备用-01", "{ba}");
    let bb = bin_of(&t, b).await;
    // b 回收时叫备用-01 还是 02 看 a 先收尾没有（收尾时压缩备用团队的编号）
    assert!(bb["name"].as_str().unwrap().starts_with("备用-0"), "{bb}");
    assert_eq!((bb["team"].as_str(), bb["traffic_up"].as_i64(), bb["traffic_down"].as_i64(), bb["nic_rx"].clone()), (Some("备用"), Some(100), Some(7), Value::Null));
    assert_eq!(bb["snapshot"]["entries"][0]["port"], 20001, "{bb}");
    for m in [a, b] {
        assert!(hidden(&t, m).await, "回收站里的机器视图里看不到");
    }
    let all = t.get("/api/v1/machines?scope=all").await;
    assert!(!all.to_string().contains(&a.to_string()), "scope=all 也不含回收站里的");
    let au: (String, Value) = sqlx::query_as("select actor, detail from audit_log where target_id = $1 and action = 'machine.recycle'").bind(a).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((au.0.as_str(), &au.1["auto"], &au.1["from"]), ("系统", &json!("spare_expiry"), &json!("团队2-07")), "{:?}", au.1);
    let list = t.get("/api/v1/recycle-bin?source=fleet").await;
    assert_eq!(list["total"], 2, "{list}");
    assert!(list["items"][0].get("snapshot").is_none(), "列表不带快照");

    // a 没有入口流量：下一轮物理删除；b 的入口近 93 天还有流量计入线路机的统计，先留着
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert!(r.recycled.is_empty() && r.purged == [a], "{r:?}");
    let (pj, p, by) = last_job(&t, a, "purge").await.unwrap();
    assert_eq!((p["auto"].clone(), by.as_str()), (json!("spare_expiry"), "系统"));
    t.wait_job(&pj.to_string(), "succeeded").await;
    assert!(gone(&t, a).await && !gone(&t, b).await);
    assert_eq!(bin_of(&t, a).await["name"], "团队2-07", "物理删除后回收站这一行还在");

    sqlx::query("update entry_traffic_hourly set hour = date_trunc('hour', now()) - interval '94 days' where entry_id = $1").bind(eid).execute(&t.st.db).await.unwrap();
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert_eq!(r.purged, [b], "{r:?}");
    let (pj, _, _) = last_job(&t, b, "purge").await.unwrap();
    t.wait_job(&pj.to_string(), "succeeded").await;
    assert!(gone(&t, b).await);
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert!(r.purged.is_empty() && r.retired.is_empty() && r.recycled.is_empty(), "{r:?}");
}

/// 不经退役作业收尾的已退役机器（上架取消的、这一版之前就退役了的）由维护任务收进回收站；有进行中作业的等下一轮；收过的不再收
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retired_machines_are_swept_into_recycle_bin() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队2").await;
    let old = seed_node(&t, team, 7, "203.0.113.31", "retired").await;
    sqlx::query("update machine set lifecycle_reason = '换新机（DeerVPS）' where id = $1").bind(old).execute(&t.st.db).await.unwrap();
    let cancelled = seed_node(&t, team, 8, "203.0.113.32", "retired").await;
    sqlx::query("update machine set lifecycle_reason = '上架取消' where id = $1").bind(cancelled).execute(&t.st.db).await.unwrap();
    let busy = seed_node(&t, team, 9, "203.0.113.33", "retired").await;
    let bj = Uuid::now_v7();
    sqlx::query("insert into job (id, kind, target_id, state, created_by) values ($1, 'purge', $2, 'queued', 'test')").bind(bj).bind(busy).execute(&t.st.db).await.unwrap();
    assert_eq!(t.get(&format!("/api/v1/machines/{old}")).await["lifecycle"], "retired", "收进回收站之前 scope=all 还看得到");

    let now = datetime!(2030-01-05 00:00 UTC);
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert_eq!(r.recycled, { let mut v = vec![old, cancelled]; v.sort(); v }, "{r:?}");
    let b = bin_of(&t, old).await;
    assert_eq!((b["name"].as_str(), b["team"].as_str(), b["reason"].as_str(), b["actor"].as_str()), (Some("团队2-07"), Some("团队2"), Some("换新机（DeerVPS）"), Some("系统")), "{b}");
    assert_eq!(bin_of(&t, cancelled).await["reason"], "上架取消");
    assert!(hidden(&t, old).await && !hidden(&t, busy).await);
    let au: Value = sqlx::query_scalar("select detail from audit_log where target_id = $1 and action = 'machine.recycle'").bind(old).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(au["auto"], spare::SWEEP);
    // 同一轮里没有被拦的接着建物理删除作业（引擎没开，作业排着）；下一轮不重复收、不重复建
    assert_eq!(r.purged.len(), 2, "{r:?}");
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert!(r.recycled.is_empty() && r.purged.is_empty(), "{r:?}");
    let n: i64 = sqlx::query_scalar("select count(*) from recycle_bin").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 2);
}

/// 宿主机到期：上面有不在备用团队的同机 IP 节点时不回收（退役会连带它们）；同机 IP 节点也在备用团队时一并退役
/// （不清理、作业也带 auto），收尾时各自进回收站；物理删除时先删同机 IP 节点，宿主机下一轮再删
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_cascades_to_ip_nodes() {
    let t = setup(true).await;
    let sp = spare_team(&t).await;
    let normal = seed_team(&t, "团队1").await;
    // 宿主机指向 127.0.0.1:1：清理时连不上，跳过清理（令牌照样撤销）
    let host = seed_vps(&t, sp, 1, "127.0.0.1", None).await;
    sub(&t, host, "2030-01-01", "UTC").await;
    let x = seed_vps(&t, normal, 1, "203.0.113.31", Some(host)).await;

    let now = datetime!(2030-01-05 00:00 UTC);
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert!(r.retired.is_empty() && r.kept == [host], "同机 IP 节点还在正常团队：宿主机不回收 {r:?}");
    assert!(last_job(&t, host, "retire").await.is_none() && last_job(&t, x, "retire").await.is_none());

    sqlx::query("update machine set team_id = $2, number = 2 where id = $1").bind(x).bind(sp).execute(&t.st.db).await.unwrap();
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert_eq!(r.retired, [host], "{r:?}");
    let (hj, hp, _) = last_job(&t, host, "retire").await.unwrap();
    let (xj, xp, xby) = last_job(&t, x, "retire").await.unwrap();
    assert_eq!((hp["cleanup"].clone(), xp["cleanup"].clone(), xp["auto"].clone(), xby.as_str()), (json!(true), json!(false), json!("spare_expiry"), "系统"));
    for j in [hj, xj] {
        t.wait_job(&j.to_string(), "succeeded").await;
    }
    assert_eq!(bin_of(&t, x).await["host_name"], "备用-01");
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

/// 备用到期自动退役在锁住机器后再核对一次：查出过期列表之后续了费（到期日改了）或移出了备用团队的，不退役
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_retire_rechecks_spare_and_expiry_under_lock() {
    let t = setup(true).await;
    let sp = spare_team(&t).await;
    let team = seed_team(&t, "团队2").await;
    let renewed = seed_node(&t, sp, 1, "203.0.113.21", "active").await;
    sub(&t, renewed, "2030-03-01", "UTC").await;
    let moved_out = seed_node(&t, team, 1, "203.0.113.22", "active").await;
    sub(&t, moved_out, "2030-01-01", "UTC").await;
    let now = datetime!(2030-01-05 00:00 UTC);
    for m in [renewed, moved_out] {
        let r = fleet_server::api::machines::start_retire(&t.st, spare::ACTOR, m, "x", "proxy", None, true, Some((spare::AUTO, now))).await;
        assert!(r.is_err(), "{m}");
        let lc: String = sqlx::query_scalar("select lifecycle::text from machine where id = $1").bind(m).fetch_one(&t.st.db).await.unwrap();
        assert_eq!(lc, "active");
    }
}

/// 迁移 0042：0041 之前进备用的机器按最近一条改进备用团队的改名审计回填 spare_since / spare_from（旧记录没有 old_name，按 before 拼）；
/// 备用团队里改编号的不算；查不到的留空。没结束的 renewal_overdue 静默改成 renewal_due
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migration_0042_backfills_spare_columns_from_audit() {
    let t = setup(false).await;
    let sp = spare_team(&t).await;
    let team = seed_team(&t, "团队2").await;
    let (a, b, c) = (
        seed_node(&t, sp, 1, "203.0.113.31", "active").await,
        seed_node(&t, sp, 2, "203.0.113.32", "active").await,
        seed_node(&t, sp, 3, "203.0.113.33", "active").await,
    );
    let audit = |m: Uuid, ts: &'static str, detail: Value| {
        sqlx::query("insert into audit_log (ts, actor, action, target_kind, target_id, detail) values ($1::timestamptz, 'u', 'machine.rename', 'machine', $2, $3)")
            .bind(ts)
            .bind(m)
            .bind(detail)
            .execute(&t.st.db)
    };
    audit(a, "2026-10-01T00:00:00Z", json!({"name": "团队2 备用-01", "old_name": "团队2-07", "before": {"team_id": team, "number": 7}, "after": {"team_id": sp, "number": 1}})).await.unwrap();
    audit(a, "2026-10-05T00:00:00Z", json!({"name": "团队2 备用-01", "old_name": "团队2 备用-03", "before": {"team_id": sp, "number": 3}, "after": {"team_id": sp, "number": 1}})).await.unwrap();
    audit(b, "2026-10-02T00:00:00Z", json!({"before": {"team_id": team, "number": 9, "label": null}, "after": {"team_id": sp, "number": 2}})).await.unwrap();
    sqlx::query("insert into silence (id, rule, starts_at, ends_at, created_by) values ($1, 'renewal_overdue', now(), now() + interval '1 day', 'u'), ($2, 'renewal_overdue', now() - interval '2 days', now() - interval '1 day', 'u')")
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../../../migrations/0042_reach_port_spare_backfill.sql")).execute(&t.st.db).await.unwrap();
    let got: Vec<(Uuid, Option<String>, Option<String>)> =
        sqlx::query_as("select id, spare_from, to_char(spare_since at time zone 'UTC', 'YYYY-MM-DD') from machine where id = any($1) order by number")
            .bind(vec![a, b, c])
            .fetch_all(&t.st.db)
            .await
            .unwrap();
    assert_eq!(
        got,
        [(a, Some("团队2-07".into()), Some("2026-10-01".into())), (b, Some("团队2-09".into()), Some("2026-10-02".into())), (c, None, None)]
    );
    let rules: Vec<String> = sqlx::query_scalar("select rule from silence order by ends_at").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(rules, ["renewal_overdue", "renewal_due"], "结束了的不动");
}

/// 旧面板导入：默认只预演；同一个 source_id 导两次只有一行；IP 或（供应商, 供应商编号）对上面板里还没回收的机器的跳过并说明；格式不对 400
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_import_is_idempotent_and_skips_live_machines() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队2").await;
    let pv = t.post("/api/v1/providers", json!({"name": "ATTVPS", "code": "ATT"})).await["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    seed_node(&t, team, 5, "75.37.237.9", "active").await;
    let other = seed_node(&t, team, 6, "75.37.237.10", "active").await;
    sqlx::query("update machine set provider_id = $2, service_ref = '60999' where id = $1").bind(other).bind(pv).execute(&t.st.db).await.unwrap();
    let item = |sid: &str, ip: &str, sref: &str| {
        json!({"source_id": sid, "kind": "vps", "name": format!("团队1-{sid}"), "team": "团队1", "ip": ip, "provider": "att", "service_ref": sref,
               "price_cents": 2888, "currency": "USD", "cycle": "monthly", "retired_at": "2026-09-30T08:00:00Z", "due_date": "2026-10-01",
               "reason": "旧面板已退役", "traffic_up": 5, "snapshot": {"uuid": sid}})
    };
    let body = json!({"items": [item("01", "75.37.237.38", "60131"), item("02", "75.37.237.9", "1"), item("03", "75.37.237.99", "60999")]});

    let r = t.post("/api/v1/recycle-bin/import", body.clone()).await;
    assert_eq!((r["dry_run"].clone(), r["inserted"].clone()), (json!(true), json!([{"source_id": "01", "name": "团队1-01", "id": null}])), "{r}");
    let why: Vec<&str> = r["skipped"].as_array().unwrap().iter().map(|s| s["why"].as_str().unwrap()).collect();
    assert_eq!(why, ["是在用机器 团队2-05", "是在用机器 团队2-06"], "{r}");
    let n: i64 = sqlx::query_scalar("select count(*) from recycle_bin").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 0, "预演不写");

    let mut confirm = body.clone();
    confirm["confirm"] = json!(true);
    let r = t.post("/api/v1/recycle-bin/import", confirm.clone()).await;
    let id = r["inserted"][0]["id"].as_i64().unwrap();
    let r = t.post("/api/v1/recycle-bin/import", confirm).await;
    assert!(r["inserted"].as_array().unwrap().is_empty() && r["skipped"][0]["why"].as_str().unwrap().contains("已经导入过"), "{r}");
    let list = t.get("/api/v1/recycle-bin?source=legacy&q=237.38").await;
    assert_eq!((list["total"].as_i64(), list["items"][0]["id"].as_i64(), list["items"][0]["ip"].as_str()), (Some(1), Some(id), Some("75.37.237.38")), "{list}");
    let one = t.get(&format!("/api/v1/recycle-bin/{id}")).await;
    assert_eq!((one["source"].as_str(), one["snapshot"].clone(), one["traffic_down"].clone(), one["due_date"].as_str()), (Some("legacy"), json!({"uuid": "01"}), Value::Null, Some("2026-10-01")), "{one}");
    // 旧面板没有「回收」这一步：回收时刻取退役时刻（不是导入时刻），按回收时间排序时不会全排在最前
    assert_eq!(one["recycled_at"], one["retired_at"], "{one}");
    let au = t.get("/api/v1/audit?cat=machine").await;
    assert!(au.as_array().unwrap().iter().any(|a| a["action"] == "recycle.import" && a["summary"] == "导入 1 台，跳过 2 台"), "{au}");

    // 同一台已经以面板机器的身份进了回收站（同 IP，或同供应商 + 服务编号）：跳过，写出是哪一行
    let fleet_bin: Vec<i64> = sqlx::query_scalar(
        "insert into recycle_bin (source, source_id, kind, name, ip, provider, service_ref, actor, snapshot)
         values ('fleet', 'm1', 'vps', '团队2-07', '75.37.237.50', null, null, '系统', '{}'), ('fleet', 'm2', 'vps', '团队2-08', null, 'ATTVPS', '61000', '系统', '{}')
         returning id",
    )
    .fetch_all(&t.st.db)
    .await
    .unwrap();
    let r = t.post("/api/v1/recycle-bin/import", json!({"items": [item("04", "75.37.237.50", "1"), item("05", "75.37.237.51", "61000")]})).await;
    let why: Vec<&str> = r["skipped"].as_array().unwrap().iter().map(|s| s["why"].as_str().unwrap()).collect();
    assert_eq!(why, [format!("已经在回收站（#{}）", fleet_bin[0]), format!("已经在回收站（#{}）", fleet_bin[1])], "{r}");

    let (s, _) = t.req("POST", "/api/v1/recycle-bin/import", Some(json!({"items": [{"source_id": "x", "kind": "router", "name": "x"}]})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// 自动续费：过了到期日按周期顺延到不早于今天（账单日不变，每期一条续费记录与审计），所以开了自动续费的备用机器不会被回收；
/// 没开的照常到期回收，不在用的不续
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_renew_extends_due_date_before_spare_expiry() {
    let t = setup(false).await;
    let sp = spare_team(&t).await;
    let a = seed_node(&t, sp, 1, "203.0.113.41", "active").await;
    let off = seed_node(&t, sp, 2, "203.0.113.42", "active").await;
    let retired = seed_node(&t, sp, 3, "203.0.113.43", "retired").await;
    for m in [a, off, retired] {
        sub(&t, m, "2030-01-31", "UTC").await;
    }
    sqlx::query("update subscription set auto_renew = true, cycle = 'monthly' where machine_id = any($1)").bind(vec![a, retired]).execute(&t.st.db).await.unwrap();

    let now = datetime!(2030-03-15 00:00 UTC);
    assert_eq!(fleet_server::renew::run_once(&t.st, now).await.unwrap(), [a]);
    let due: String = sqlx::query_scalar("select due_date::text from subscription where machine_id = $1").bind(a).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(due, "2030-03-31", "01-31 → 02-28 → 03-31：账单日 31 号不变");
    let rs: Vec<(String, String, Option<i32>, String, String)> =
        sqlx::query_as("select old_due::text, new_due::text, amount_cents, note, actor from renewal where machine_id = $1 order by old_due").bind(a).fetch_all(&t.st.db).await.unwrap();
    let want = |o: &str, n: &str| (o.to_string(), n.to_string(), Some(500), "自动续费".to_string(), "系统".to_string());
    assert_eq!(rs, [want("2030-01-31", "2030-02-28"), want("2030-02-28", "2030-03-31")]);
    let au = t.get("/api/v1/audit?cat=machine").await;
    assert!(au.as_array().unwrap().iter().any(|x| x["action"] == "renewal" && x["summary"] == "到期 2030-02-28 → 2030-03-31（自动续费）"), "{au}");
    assert!(fleet_server::renew::run_once(&t.st, now).await.unwrap().is_empty(), "已经顺延过的不再续");

    let r = spare::run_once(&t.st, now).await.unwrap();
    assert_eq!(r.retired, [off], "没开自动续费的照常回收：{r:?}");
}

/// 自动续费每台单独处理：一台写库出错（这里用触发器造）只记日志，排在后面的照常续
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_renew_one_failure_does_not_stop_the_rest() {
    let t = setup(false).await;
    let sp = spare_team(&t).await;
    let bad = seed_node(&t, sp, 1, "203.0.113.51", "active").await;
    let good = seed_node(&t, sp, 2, "203.0.113.52", "active").await;
    for m in [bad, good] {
        sub(&t, m, "2030-01-31", "UTC").await;
    }
    sqlx::query("update subscription set auto_renew = true, cycle = 'monthly' where machine_id = any($1)").bind(vec![bad, good]).execute(&t.st.db).await.unwrap();
    sqlx::query(&format!("create function renewal_fail() returns trigger language plpgsql as $$ begin if new.machine_id = '{bad}' then raise exception 'boom'; end if; return new; end $$"))
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("create trigger renewal_fail before insert on renewal for each row execute function renewal_fail()").execute(&t.st.db).await.unwrap();

    assert!(bad < good, "uuid v7 递增，出错的这台排在前面（按 machine_id 排序）");
    let done = fleet_server::renew::run_once(&t.st, datetime!(2030-03-15 00:00 UTC)).await.unwrap();
    assert_eq!(done, [good]);
    let due = |m: Uuid| {
        let db = t.st.db.clone();
        async move { sqlx::query_scalar::<_, String>("select due_date::text from subscription where machine_id = $1").bind(m).fetch_one(&db).await.unwrap() }
    };
    assert_eq!((due(bad).await.as_str(), due(good).await.as_str()), ("2030-01-31", "2030-03-31"));
}

/// 软路由出口还指向的备用节点到期不自动回收（回收会摘入口，出口跟着断）：归进 kept、不建退役作业；出口改走之后下一轮照常回收
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_spare_still_pointed_by_router_exit_is_kept() {
    let t = setup(false).await;
    let sp = spare_team(&t).await;
    let n = seed_node(&t, sp, 1, "203.0.113.41", "active").await;
    sub(&t, n, "2026-10-20", "UTC").await;
    let rid = Uuid::now_v7();
    sqlx::query("insert into router (id, name, tunnel_port) values ($1, '办公室', 23001)").bind(rid).execute(&t.st.db).await.unwrap();
    let snap = |entry: Value| json!({"exits": [{"name": "美国", "entry": entry, "sync": "ok", "linked": true}]});
    let pointing = json!({"id": Uuid::now_v7(), "name": "备用-01 · 线路机1:20001", "node_id": n, "relay_id": Uuid::now_v7(), "node_name": "备用-01", "relay_name": "线路机1", "port": 20001, "lifecycle": "active"});
    sqlx::query("insert into router_status (router_id, ts, reachable, data, last_ok_at) values ($1, now(), true, $2, now())")
        .bind(rid)
        .bind(snap(pointing))
        .execute(&t.st.db)
        .await
        .unwrap();
    let now = datetime!(2026-10-23 00:00 UTC);
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert!(r.retired.is_empty() && r.kept == [n], "{r:?}");
    assert!(last_job(&t, n, "retire").await.is_none());
    assert_eq!(t.machine_lifecycle(n).await, "active");
    // 出口改指向了别处（快照里不再对上它）：照常回收
    sqlx::query("update router_status set data = $2 where router_id = $1").bind(rid).bind(json!({"exits": []})).execute(&t.st.db).await.unwrap();
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert!(r.retired == [n] && r.kept.is_empty(), "{r:?}");
}

/// 改团队预览的 router_exits：改到备用团队时列出还指向宿主机和随它一起移入的同机 IP 节点的软路由出口；改到非备用团队、没有出口指着时为空
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_preview_lists_router_exits_when_moving_to_spare() {
    let t = setup(false).await;
    let sp = spare_team(&t).await;
    let (a, b) = (seed_team(&t, "团队1").await, seed_team(&t, "团队2").await);
    let host = seed_vps(&t, a, 1, "203.0.113.51", None).await;
    let ip = seed_vps(&t, a, 2, "203.0.113.52", Some(host)).await;
    let other = seed_vps(&t, a, 3, "203.0.113.53", None).await;
    let rid = Uuid::now_v7();
    sqlx::query("insert into router (id, name, tunnel_port) values ($1, '办公室', 23002)").bind(rid).execute(&t.st.db).await.unwrap();
    let point = |node: Uuid, name: &str| json!({"id": Uuid::now_v7(), "name": name, "node_id": node, "relay_id": Uuid::now_v7(), "node_name": name, "relay_name": "线路机1", "port": 20001, "lifecycle": "active"});
    let exit = |name: &str, entry: Value| json!({"name": name, "entry": entry, "sync": "ok", "linked": true});
    let snap = json!({"exits": [exit("节点01", point(host, "团队1-01")), exit("节点02", point(ip, "团队1-02")), exit("节点03", point(other, "团队1-03"))]});
    sqlx::query("insert into router_status (router_id, ts, reachable, data, last_ok_at) values ($1, now(), true, $2, now())").bind(rid).bind(snap).execute(&t.st.db).await.unwrap();

    // 宿主机进备用：它和 IP 节点的出口列出来（别的节点的不算）
    let pv = t.get(&format!("/api/v1/machines/{host}/rename-preview?team_id={sp}")).await;
    assert_eq!(
        pv["router_exits"],
        json!([{"node": "团队1-01", "router": "办公室", "exit": "节点01"}, {"node": "团队1-02", "router": "办公室", "exit": "节点02"}]),
        "{pv}"
    );
    // IP 节点自己进备用只看它自己
    let pv = t.get(&format!("/api/v1/machines/{ip}/rename-preview?team_id={sp}")).await;
    assert_eq!(pv["router_exits"], json!([{"node": "团队1-02", "router": "办公室", "exit": "节点02"}]), "{pv}");
    // 改到非备用团队、没换团队：不返回
    for q in [format!("team_id={b}"), "number=9".to_string()] {
        let pv = t.get(&format!("/api/v1/machines/{host}/rename-preview?{q}")).await;
        assert_eq!(pv["router_exits"], json!([]), "{pv}");
    }
}

/// 软路由出口指向备用宿主机上的同机 IP 节点（宿主机自己没被指）：宿主机同样不是闲置备用——视图的 router_exits 带上经 IP 节点的出口、不给回收日
/// （recycle_held），续费提醒列出出口，到期满 1 天也不自动回收（kept）；出口改走之后照常回收
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spare_host_is_held_by_router_exit_on_its_ip_node() {
    let t = setup(false).await;
    let sp = spare_team(&t).await;
    let host = seed_vps(&t, sp, 1, "203.0.113.61", None).await;
    let ip = seed_vps(&t, sp, 2, "203.0.113.62", Some(host)).await;
    sub(&t, host, "2026-10-20", "UTC").await;
    let rid = Uuid::now_v7();
    sqlx::query("insert into router (id, name, tunnel_port) values ($1, '办公室', 23003)").bind(rid).execute(&t.st.db).await.unwrap();
    let entry = json!({"id": Uuid::now_v7(), "name": "备用-02 · 线路机1:20001", "node_id": ip, "relay_id": Uuid::now_v7(), "node_name": "备用-02", "relay_name": "线路机1", "port": 20001, "lifecycle": "active"});
    let snap = json!({"exits": [{"name": "节点02", "entry": entry, "sync": "ok", "linked": true}]});
    sqlx::query("insert into router_status (router_id, ts, reachable, data, last_ok_at) values ($1, now(), true, $2, now())").bind(rid).bind(snap).execute(&t.st.db).await.unwrap();

    let v = t.get(&format!("/api/v1/machines/{host}")).await;
    assert_eq!(v["router_exits"], json!(["办公室 · 节点02（经同机 IP 节点 备用-02）"]), "{v}");
    assert_eq!((v["recycle_on"].clone(), v["recycle_held"].clone()), (Value::Null, json!(true)), "{v}");
    let renewal = v["health"]["reasons"].as_array().unwrap().iter().find(|r| r["key"] == "renewal_due").map(|r| r["text"].as_str().unwrap().to_string());
    assert!(renewal.is_some_and(|x| x.contains("办公室 · 节点02（经同机 IP 节点 备用-02）") && x.contains("未自动回收")), "{v}");

    let now = datetime!(2026-10-23 00:00 UTC);
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert!(r.retired.is_empty() && r.kept == [host], "{r:?}");
    assert!(last_job(&t, host, "retire").await.is_none());

    // 出口改走：不再被指着，回收日恢复，下一轮照常回收
    sqlx::query("update router_status set data = $2 where router_id = $1").bind(rid).bind(json!({"exits": []})).execute(&t.st.db).await.unwrap();
    t.st.live.invalidate();
    let v = t.get(&format!("/api/v1/machines/{host}")).await;
    assert_eq!((v["router_exits"].clone(), v["recycle_on"].clone(), v["recycle_held"].clone()), (json!([]), json!("2026-10-22"), json!(false)), "{v}");
    let r = spare::run_once(&t.st, now).await.unwrap();
    assert!(r.retired == [host] && r.kept.is_empty(), "{r:?}");
}
