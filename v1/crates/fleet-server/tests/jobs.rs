//! 作业引擎：取消与补偿、退役后旧作业不能复活、purge 幂等、租约持有者校验、线路机退役守卫、退役即关告警、作业清理。
//! 需要 PostgreSQL：环境变量 FLEET_TEST_DB。会清空该库的 public schema，用 `--test-threads=1` 串行跑。
//!
//! 「上架」类测试用 VPS 指向 127.0.0.1:1（必定连接被拒）：预检会失败，不需要真的 SSH 到任何机器。

mod common;

use axum::http::StatusCode;
use common::{seed_node, seed_relay, seed_team, setup};
use fleet_server::jobs;
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

/// 上架一台连不上的 VPS，返回 (machine_id, job_id)
async fn provision_unreachable_vps(t: &common::T, team: Uuid, relay: Uuid) -> (Uuid, String) {
    let created = t
        .post(
            "/api/v1/provision",
            json!({"items": [{"kind": "vps", "ip": "127.0.0.1", "ssh_port": 1, "team_id": team, "relay_id": relay,
                              "login": {"password": "s3cret-root-password"}}]}),
        )
        .await;
    let j = &created["jobs"][0];
    (j["machine_id"].as_str().unwrap().parse().unwrap(), j["job_id"].as_str().unwrap().to_string())
}

async fn job_login_is_null(t: &common::T, job: &str) -> bool {
    sqlx::query_scalar("select (params->'login') is null from job where id = $1::uuid").bind(job).fetch_one(&t.st.db).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_while_running_ends_cancelled_and_compensates() {
    let t = setup(true).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let (machine, job) = provision_unreachable_vps(&t, team, relay).await;
    assert!(!job_login_is_null(&t, &job).await, "上架时 root 密码加密存在作业参数里");

    // 等预检开始跑（它要连 3 次、中间各睡 0.5 秒，窗口足够），在它还没失败时取消
    let mut running = false;
    for _ in 0..200 {
        let j = t.job(&job).await;
        if j["state"] == "running" && j["steps"][0]["state"] == "running" {
            running = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(running, "作业应当进入运行中");
    t.post(&format!("/api/v1/jobs/{job}/cancel"), json!({})).await;

    // 步骤随后失败，但作业不能被覆盖成 failed：最终是 cancelled，并执行了补偿
    let j = t.wait_job(&job, "cancelled").await;
    assert_eq!(j["steps"][0]["state"], "failed");
    assert_eq!(j["steps"][0]["error_kind"], "network", "SSH 端口连不上归为网络类错误");
    assert_eq!(t.machine_lifecycle(machine).await, "retired", "补偿把上架到一半的机器退役，释放 IP 与编号");
    let reason: Option<String> = sqlx::query_scalar("select lifecycle_reason from machine where id = $1").bind(machine).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(reason.as_deref(), Some("上架取消"));
    assert!(job_login_is_null(&t, &job).await, "取消后加密的 root 密码要清掉");
    let logs = t.get(&format!("/api/v1/jobs/{job}/logs")).await;
    let msgs: Vec<&str> = logs.as_array().unwrap().iter().map(|l| l["msg"].as_str().unwrap()).collect();
    assert!(msgs.iter().any(|m| m.contains("将执行补偿")), "{msgs:?}");
    assert!(msgs.iter().any(|m| m.contains("上架已取消")), "{msgs:?}");
    let lease: Option<String> = sqlx::query_scalar("select lease_owner from job where id = $1::uuid").bind(&job).fetch_one(&t.st.db).await.unwrap();
    assert!(lease.is_none(), "结束后不留租约");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retired_machine_old_provision_job_cannot_be_retried() {
    let t = setup(true).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let (machine, job) = provision_unreachable_vps(&t, team, relay).await;
    // 预检失败 → 作业 failed（可以重试的状态）
    let failed = t.wait_job(&job, "failed").await;
    assert_eq!(failed["steps"][0]["error_kind"], "network");
    assert_eq!(t.machine_lifecycle(machine).await, "provisioning");

    // 把这台上架失败的机器退役
    let name = t.get(&format!("/api/v1/machines/{machine}")).await["name"].as_str().unwrap().to_string();
    let rj = t.post(&format!("/api/v1/machines/{machine}/retire"), json!({"confirm": name})).await["job_id"].as_str().unwrap().to_string();
    t.wait_job(&rj, "succeeded").await;
    assert_eq!(t.machine_lifecycle(machine).await, "retired");

    // 旧的上架作业在退役时被作废，不再是 failed
    let old = t.job(&job).await;
    assert_eq!(old["state"], "cancelled");
    let (s, v) = t.req("POST", &format!("/api/v1/jobs/{job}/retry"), None, None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");

    // 即使作业被改回 failed，也要因为机器已退役而拒绝重试（按作业种类检查机器状态）
    sqlx::query("update job set state = 'failed' where id = $1::uuid").bind(&job).execute(&t.st.db).await.unwrap();
    let (s, v) = t.req("POST", &format!("/api/v1/jobs/{job}/retry"), None, None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("已退役"), "{v}");
    assert_eq!(t.machine_lifecycle(machine).await, "retired", "机器没有被拉回去");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_runs_twice_without_error() {
    let t = setup(true).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "retired").await;
    let name = t.get(&format!("/api/v1/machines/{node}")).await["name"].as_str().unwrap().to_string();
    // 该节点的访问汇总和分区表里的主机指标，彻底删除时要一并清掉
    for sql in [
        "insert into access_hourly (node_id, relay_id, hour, dest_host, kind, conns) values ($1, gen_random_uuid(), now(), 'a.com', 'domain', 1)",
        "insert into access_source_hourly (node_id, hour, src_ip, conns) values ($1, now(), '1.1.1.1', 1)",
        "insert into access_target_seen (node_id, dest_host, first_seen, last_seen, total) values ($1, 'a.com', now(), now(), 1)",
        "insert into access_source_late (node_id, hour, src_ip, delta) values ($1, now(), '1.1.1.1', 1)",
        "insert into host_metric (machine_id, ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bytes, net_tx_bytes, tcp_conns) values ($1, now(), 1, 1, 1, 1, 1, 1, 1, 1, 1)",
    ] {
        sqlx::query(sql).bind(node).execute(&t.st.db).await.unwrap();
    }
    // 对端线路机（有重置日）的周期已用含这台节点的流量；彻底删除后缓存要立刻作废，不等下一轮汇总
    let (relay, _) = seed_relay(&t).await;
    let eid = Uuid::now_v7();
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, 20001, $4, 'ab', 'removed')")
        .bind(eid)
        .bind(relay)
        .bind(node)
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, date_trunc('hour', now()), 100, 0)").bind(eid).execute(&t.st.db).await.unwrap();
    let day: i32 = sqlx::query_scalar("select extract(day from now() at time zone 'Asia/Shanghai')::int").fetch_one(&t.st.db).await.unwrap();
    let (s, v) = t
        .req("PUT", &format!("/api/v1/machines/{relay}/subscription"), Some(json!({"price_cents": 100, "due_date": "2030-01-01", "billing_tz": "Asia/Shanghai", "traffic_reset_day": day})), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(t.get(&format!("/api/v1/machines/{relay}")).await["quota"]["used"], 100);
    let first = t.post(&format!("/api/v1/machines/{node}/purge"), json!({"confirm": name})).await["job_id"].as_str().unwrap().to_string();
    let j = t.wait_job(&first, "succeeded").await;
    assert_eq!(j["steps"][0]["state"], "done");
    for tbl in ["access_hourly", "access_source_hourly", "access_target_seen", "access_source_late"] {
        let n: i64 = sqlx::query_scalar(&format!("select count(*) from {tbl} where node_id = $1")).bind(node).fetch_one(&t.st.db).await.unwrap();
        assert_eq!(n, 0, "{tbl} 没清干净");
    }
    let n: i64 = sqlx::query_scalar("select count(*) from host_metric where machine_id = $1").bind(node).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 0);
    let (s, _) = t.req("GET", &format!("/api/v1/machines/{node}"), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(t.get(&format!("/api/v1/machines/{relay}")).await["quota"]["used"], 0, "对端的周期已用随彻底删除刷新");

    // 机器已经不在了，再来一个 purge 作业（比如上一次跑到一半进程挂了、重新领取）也应该直接成功
    let mut tx = t.st.db.begin().await.unwrap();
    let second = jobs::enqueue(&mut tx, "purge", Some(node), json!({}), "test").await.unwrap().to_string();
    tx.commit().await.unwrap();
    let j = t.wait_job(&second, "succeeded").await;
    assert_eq!(j["steps"][0]["output"]["already_gone"], true, "{j}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_lease_holder_cannot_write() {
    let t = setup(false).await; // 不起后台引擎，手动领取、推进
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "provisioning").await;
    let mut tx = t.st.db.begin().await.unwrap();
    let job_id = jobs::enqueue(&mut tx, "provision_proxy", Some(node), json!({"relay_id": relay}), "test").await.unwrap();
    tx.commit().await.unwrap();

    let a = jobs::claim(&t.st, "owner-a").await.unwrap().expect("能领到作业");
    assert_eq!(a.id, job_id);
    assert!(jobs::claim(&t.st, "owner-x").await.unwrap().is_none(), "租约有效期内别人领不到");
    // A 卡住，租约过期，B 接管
    sqlx::query("update job set lease_until = now() - interval '1 second' where id = $1").bind(job_id).execute(&t.st.db).await.unwrap();
    let b = jobs::claim(&t.st, "owner-b").await.unwrap().expect("过期后被接管");
    assert_eq!(b.id, job_id);

    // A 苏醒后继续推进：续租被拒，什么都不能写（不会建入口，也不会改步骤状态）
    assert!(!jobs::renew(&t.st, job_id, "owner-a", 60).await.unwrap());
    jobs::advance(&t.st, a, "owner-a").await.unwrap();
    let entries: i64 = sqlx::query_scalar("select count(*) from entry").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(entries, 0, "旧执行者不能再产生副作用");
    let st0: String = sqlx::query_scalar("select state from job_step where job_id = $1 and idx = 0").bind(job_id).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(st0, "pending");
    let owner: Option<String> = sqlx::query_scalar("select lease_owner from job where id = $1").bind(job_id).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(owner.as_deref(), Some("owner-b"));

    // 现任持有者正常推进：建入口，然后进入等待
    assert!(jobs::renew(&t.st, job_id, "owner-b", 60).await.unwrap());
    jobs::advance(&t.st, b, "owner-b").await.unwrap();
    let entries: i64 = sqlx::query_scalar("select count(*) from entry").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(entries, 1);
    let (state, step): (String, i32) = sqlx::query_as("select state::text, step from job where id = $1").bind(job_id).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((state.as_str(), step), ("waiting", 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_with_entries_cannot_be_retired() {
    let t = setup(false).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let mut tx = t.st.db.begin().await.unwrap();
    let (entry, _, _) = fleet_server::entries::create(&mut tx, relay, node).await.unwrap();
    tx.commit().await.unwrap();

    let (s, v) = t.req("POST", &format!("/api/v1/machines/{relay}/retire"), Some(json!({"confirm": "线路机2"})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("节点入口"), "{v}");
    assert_eq!(t.machine_lifecycle(relay).await, "active");

    // 入口摘光之后才允许退役
    sqlx::query("update entry set lifecycle = 'removed', removed_at = now() where id = $1").bind(entry).execute(&t.st.db).await.unwrap();
    let r: Value = t.post(&format!("/api/v1/machines/{relay}/retire"), json!({"confirm": "线路机2"})).await;
    assert!(r["job_id"].is_string());
    assert_eq!(t.machine_lifecycle(relay).await, "retiring");
    // 已经在退役：再点一次是 409，而不是又排一个作业
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{relay}/retire"), Some(json!({"confirm": "线路机2"})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // 退役开始后，不能再往这台线路机上加入口
    let mut tx = t.st.db.begin().await.unwrap();
    let node2 = seed_node(&t, team, 2, "203.0.113.11", "active").await;
    assert!(fleet_server::entries::create(&mut tx, relay, node2).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retire_closes_open_incidents_up_front() {
    let t = setup(false).await;
    let (relay, _) = seed_relay(&t).await;
    let inc = Uuid::now_v7();
    sqlx::query("insert into incident (id, rule, subject_id, severity, summary) values ($1, 'relay_xray_down', $2, 'critical', 's')")
        .bind(inc)
        .bind(relay)
        .execute(&t.st.db)
        .await
        .unwrap();
    let r: Value = t.post(&format!("/api/v1/machines/{relay}/retire"), json!({"confirm": "线路机2"})).await;
    let closed: (String, Value) = sqlx::query_as("select state::text, detail from incident where id = $1").bind(inc).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((closed.0.as_str(), &closed.1["closed_by"]), ("resolved", &json!("retire")), "发起退役就关掉告警");
    // 作业失败：机器停在退役中等重试（不回 active），告警评估也不会再开或重开
    sqlx::query("update job set state = 'failed', finished_at = now() where id = $1::uuid").bind(r["job_id"].as_str().unwrap()).execute(&t.st.db).await.unwrap();
    t.st.live.invalidate();
    fleet_server::alerts::evaluate_once(&t.st, &mut Default::default(), time::OffsetDateTime::now_utc()).await.unwrap();
    assert_eq!(t.machine_lifecycle(relay).await, "retiring");
    let firing: i64 = sqlx::query_scalar("select count(*) from incident where state = 'firing'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(firing, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prune_clears_old_passwords_and_deletes_old_jobs() {
    let t = setup(false).await;
    let login = json!({"login": {"password_enc": "x"}});
    let mk = |state: &'static str, days: i32, params: Value| {
        let db = t.st.db.clone();
        async move {
            let id = Uuid::now_v7();
            sqlx::query(
                "insert into job (id, kind, params, state, created_by, finished_at)
                 values ($1, 'purge', $2, $3::job_state, 'test', now() - make_interval(days => $4))",
            )
            .bind(id)
            .bind(params)
            .bind(state)
            .bind(days)
            .execute(&db)
            .await
            .unwrap();
            sqlx::query("insert into job_step (job_id, idx, key) values ($1, 0, 'purge')").bind(id).execute(&db).await.unwrap();
            id
        }
    };
    let old_ok = mk("succeeded", 100, json!({})).await;
    let old_cancelled = mk("cancelled", 91, json!({})).await;
    let recent_ok = mk("succeeded", 10, json!({})).await;
    let old_failed = mk("failed", 100, login.clone()).await; // 失败的作业 180 天内不删，只清密码
    let ancient_failed = mk("failed", 200, json!({})).await;
    let mid_failed = mk("failed", 4, login.clone()).await;
    let fresh_failed = mk("failed", 0, login.clone()).await;

    let r = jobs::prune(&t.st.db).await.unwrap();
    assert_eq!(r, jobs::Pruned { passwords_cleared: 2, jobs_deleted: 3 });
    let exists = |id: Uuid| {
        let db = t.st.db.clone();
        async move { sqlx::query_scalar::<_, bool>("select exists (select 1 from job where id = $1)").bind(id).fetch_one(&db).await.unwrap() }
    };
    assert!(!exists(old_ok).await && !exists(old_cancelled).await && !exists(ancient_failed).await);
    assert!(exists(recent_ok).await && exists(old_failed).await);
    let steps: i64 = sqlx::query_scalar("select count(*) from job_step where job_id = any($1)").bind(vec![old_ok, old_cancelled, ancient_failed]).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(steps, 0, "步骤随作业级联删除");
    for (id, cleared) in [(old_failed, true), (mid_failed, true), (fresh_failed, false)] {
        let is_null: bool = sqlx::query_scalar("select (params->'login') is null from job where id = $1").bind(id).fetch_one(&t.st.db).await.unwrap();
        assert_eq!(is_null, cleared, "{id}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_list_pages_with_total() {
    let t = setup(false).await;
    let mut made = vec![];
    for state in ["succeeded", "failed", "succeeded", "running", "cancelled"] {
        let id = Uuid::now_v7();
        sqlx::query("insert into job (id, kind, params, state, created_by) values ($1, 'purge', '{}', $2::job_state, 'test')")
            .bind(id)
            .bind(state)
            .execute(&t.st.db)
            .await
            .unwrap();
        made.push((id.to_string(), state));
    }
    made.sort();
    made.reverse(); // 列表按 id 倒序
    let done_ids: Vec<String> = made.iter().filter(|m| m.1 != "running").map(|m| m.0.clone()).collect();
    let made: Vec<String> = made.into_iter().map(|m| m.0).collect();
    let ids = |v: &Value| v["items"].as_array().unwrap().iter().map(|j| j["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();

    let p1 = t.get("/api/v1/jobs?state=all&limit=2").await;
    let p2 = t.get("/api/v1/jobs?state=all&limit=2&offset=2").await;
    let p3 = t.get("/api/v1/jobs?state=all&limit=2&offset=4").await;
    assert_eq!((p1["total"].as_i64(), p1["next_offset"].as_i64(), p2["next_offset"].as_i64()), (Some(5), Some(2), Some(4)));
    assert!(p3["next_offset"].is_null());
    assert_eq!([ids(&p1), ids(&p2), ids(&p3)].concat(), made);
    let done = t.get("/api/v1/jobs?state=done&limit=1&offset=1").await;
    assert_eq!((done["total"].as_i64(), ids(&done)), (Some(4), vec![done_ids[1].clone()]), "筛选后的总数与偏移");
    let active = t.get("/api/v1/jobs?state=active").await;
    assert_eq!((active["total"].as_i64(), active["next_offset"].is_null()), (Some(1), true));
}
