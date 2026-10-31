//! 作业引擎：取消与补偿、退役后旧作业不能复活、purge 幂等、租约持有者校验、线路机退役守卫、退役即关告警、作业清理、重试重置主机指纹的范围、有步骤在执行时拒绝退役、
//! 不重复领本进程还在推进的作业、步骤里库暂时不可用时从这一步重做、优雅退出与先占端口、中控刚恢复时不判线路机不在线。
//! 需要 PostgreSQL（见 common）。
//!
//! 「上架」类测试用 VPS 指向 127.0.0.1:1（必定连接被拒）：预检会失败，不需要真的 SSH 到任何机器。

use crate::common;

use axum::http::StatusCode;
use common::{dist_agent, seed_node, seed_relay, seed_team, setup};
use fleet_server::jobs;
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

/// 「没有被再领」这类反向断言只能等一段时间再看：集中在这里，调快调慢只改一处
const NOT_RECLAIMED_WAIT: Duration = Duration::from_millis(1500);

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

/// 上架前的「检查」看 SSH 端口通不通：连不上的不算通过（不用等上架作业失败），写明是被拒绝还是没有响应。
/// 私网地址照常去连（这里用本机出网卡的地址，端口 1 必定被拒绝）；回环地址直接报错、不去连
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn check_reports_unreachable_ssh() {
    let t = setup(false).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let udp = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    udp.connect("192.0.2.1:9").unwrap();
    let me = udp.local_addr().unwrap().ip().to_string();
    let item = |ip: &str| json!({"kind": "vps", "ip": ip, "ssh_port": 1, "team_id": team, "relay_id": relay, "login": {"password": "s3cret-root-password"}});
    let chk = t.post("/api/v1/provision/check", json!({"items": [item(&me), item("127.0.0.1")]})).await;
    let errs = |i: usize| chk["items"][i]["errors"].to_string();
    assert!(chk["items"][0]["ok"] == false && errs(0).contains(&format!("SSH {me}:1 连不上（连接被拒绝")), "{chk}");
    assert!(errs(1).starts_with("[\"IP 127.0.0.1 是回环地址") && !errs(1).contains("SSH"), "{chk}");
    // 连得上 TCP 但不说 SSH（中控出网经透明代理时，不存在的地址也是这样）：不算通过
    let silent = tokio::net::TcpListener::bind((me.as_str(), 0)).await.unwrap();
    let port = silent.local_addr().unwrap().port();
    let mut it = item(&me);
    it["ssh_port"] = json!(port);
    let chk = t.post("/api/v1/provision/check", json!({"items": [it]})).await;
    assert!(chk["items"][0]["errors"].to_string().contains(&format!("SSH {me}:{port} 没有响应 SSH 握手")), "{chk}");
    drop(silent);
}

/// 作业预检同「检查」：TCP 连得上但不说 SSH 的（透明代理）判网络错误，不去登录、等 ssh 自己超时
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preflight_needs_ssh_banner() {
    let t = setup(true).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = silent.local_addr().unwrap().port();
    let created = t.post("/api/v1/provision", json!({"items": [{"kind": "vps", "ip": "127.0.0.1", "ssh_port": port, "team_id": team, "relay_id": relay}]})).await;
    let j = t.wait_job(created["jobs"][0]["job_id"].as_str().unwrap(), "failed").await;
    assert_eq!(j["steps"][0]["error_kind"], "network", "{j}");
    assert!(j["steps"][0]["error"].as_str().unwrap().contains(&format!("SSH 127.0.0.1:{port} 没有响应 SSH 握手")), "{j}");
    drop(silent);
}

/// 作业失败只在系统发起（或停在需要人工处理）时通知渠道：人手动发起的失败不发，改成系统发起的同一个作业重试再失败发一条
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_job_notifies_only_when_system_started() {
    let t = setup(true).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    sqlx::query("insert into notify_channel (id, kind, name, config_enc, min_severity) values ($1, 'feishu', 'wh', '\\x00', 'critical')")
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();
    let (_, job) = provision_unreachable_vps(&t, team, relay).await;
    t.wait_job(&job, "failed").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // 渠道配置是假的，发不出去的会很快被放弃记进 notify_log：队列和记录一起数
    let q = "select (select count(*) from notify_outbox where notice->>'kind' = 'jobfail') + (select count(*) from notify_log where kind = 'jobfail')";
    assert_eq!(t.count(q).await, 0, "人手动发起的作业失败不通知");
    sqlx::query("update job set created_by = '系统' where id = $1::uuid").bind(&job).execute(&t.st.db).await.unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({"ssh_password": "new-root-pw"})).await;
    for _ in 0..100 {
        if t.count(q).await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(t.count(q).await, 1);
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
    let au = t.get(&format!("/api/v1/audit?target_id={job}")).await;
    assert_eq!((&au[0]["title"], &au[0]["summary"]), (&json!("取消作业"), &json!("上架 VPS 节点 · 团队1-01")), "审计写明取消的是哪个作业：{au}");

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
    // 只要最后几行：给出前面没给的行数和续拉的游标
    let all = logs.as_array().unwrap();
    let tail = t.get(&format!("/api/v1/jobs/{job}/logs?tail=2")).await;
    assert_eq!(tail["logs"].as_array().unwrap()[..], all[all.len() - 2..], "{tail}");
    assert_eq!((tail["skipped"].as_u64(), &tail["after"]), (Some(all.len() as u64 - 2), &all[all.len() - 1]["id"]), "{tail}");
    let rest = t.get(&format!("/api/v1/jobs/{job}/logs?tail=5000&after={}", tail["after"])).await;
    assert_eq!((rest["logs"].as_array().map(Vec::len), rest["skipped"].as_u64(), &rest["after"]), (Some(0), Some(0), &tail["after"]), "{rest}");
    let lease: Option<String> = sqlx::query_scalar("select lease_owner from job where id = $1::uuid").bind(&job).fetch_one(&t.st.db).await.unwrap();
    assert!(lease.is_none(), "结束后不留租约");
}

/// 预检没通过（这里是主机公钥拿不到）就取消：机器上跑的不一定是这次要装的（可能是换了 IP 的在用节点），补偿不能登录去清理，只写一行日志说明
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_before_preflight_passes_does_not_clean_machine() {
    use tokio::io::AsyncWriteExt;
    let t = setup(true).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    // 只回一行 SSH 版本行的假 SSH 端口：SSH 探测通过，但拿不到主机公钥，预检失败
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = vec![];
        while let Ok((mut s, _)) = l.accept().await {
            let _ = s.write_all(b"SSH-2.0-fake\r\n").await;
            held.push(s);
        }
    });
    let created = t.post("/api/v1/provision", json!({"items": [{"kind": "vps", "ip": "127.0.0.1", "ssh_port": port, "team_id": team, "relay_id": relay}]})).await;
    let (machine, job) = (created["jobs"][0]["machine_id"].as_str().unwrap().parse::<Uuid>().unwrap(), created["jobs"][0]["job_id"].as_str().unwrap().to_string());
    t.wait_job(&job, "failed").await;
    t.post(&format!("/api/v1/jobs/{job}/cancel"), json!({})).await;
    t.wait_job(&job, "cancelled").await;
    assert_eq!(t.machine_lifecycle(machine).await, "retired");
    let logs = t.get(&format!("/api/v1/jobs/{job}/logs")).await;
    let msgs: Vec<&str> = logs.as_array().unwrap().iter().map(|l| l["msg"].as_str().unwrap()).collect();
    assert!(msgs.iter().any(|m| m.contains("没有清理机器上的服务")), "{msgs:?}");
    assert!(!msgs.iter().any(|m| m.contains("清理机器上的服务」失败")), "没有去登录清理：{msgs:?}");
}

/// 「上线」已经把机器改成了在用，作业才被取消（取消赶在上线之后，或上线后记账失败再取消）：补偿什么都不动，
/// 入口保持在用（摘掉会让客户断线）、机器保持在用
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_after_machine_active_keeps_entry_and_machine() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队1").await;
    let (relay, _) = seed_relay(&t).await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let eid = Uuid::now_v7();
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, 20001, $4, 'ab', 'active')")
        .bind(eid)
        .bind(relay)
        .bind(node)
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();
    let mut tx = t.st.db.begin().await.unwrap();
    let job = jobs::enqueue(&mut tx, "provision_proxy", Some(node), json!({"entry_id": eid}), "test").await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("update job set state = 'cancelling' where id = $1").bind(job).execute(&t.st.db).await.unwrap();
    let j = run_one(&t, job).await;
    assert_eq!(j["state"], "cancelled", "{j}");
    assert_eq!(t.machine_lifecycle(node).await, "active", "机器没有被退役");
    let lc: String = sqlx::query_scalar("select lifecycle::text from entry where id = $1").bind(eid).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(lc, "active", "入口没有被摘掉");
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
    // 上架失败：机器不再停在上架中，原因是失败步骤的错误，详情给出失败的作业
    assert_eq!(t.machine_lifecycle(machine).await, "failed");
    let m = t.get(&format!("/api/v1/machines/{machine}")).await;
    assert!(m["lifecycle_reason"].as_str().unwrap().starts_with("预检与身份："), "{m}");
    assert_eq!(m["failed_job"]["id"], job.as_str(), "{m}");
    assert!(m["active_job"].is_null());

    // 列表形状：不带 steps，带当前步骤标题（网页进度浮层只靠 step/step_count/step_title）
    let list = t.get(&format!("/api/v1/jobs?target_id={machine}")).await;
    let item = &list["items"][0];
    assert_eq!(item["id"], job.as_str());
    assert!(item.get("steps").is_none(), "{item}");
    assert_eq!(item["step"], 0);
    assert_eq!(item["step_count"], 10);
    assert_eq!(item["step_title"], "预检与身份");

    // 带 root 密码重试（原密码已被清掉）：机器回到上架中，密码和上架时一样加密存进作业参数供重试的作业使用；再次失败又回到上架失败
    sqlx::query("update job set params = params - 'login' where id = $1::uuid").bind(&job).execute(&t.st.db).await.unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({"ssh_password": "new-root-pw"})).await;
    assert_eq!(t.machine_lifecycle(machine).await, "provisioning");
    t.wait_job(&job, "failed").await;
    assert_eq!(t.machine_lifecycle(machine).await, "failed");
    let enc: String = sqlx::query_scalar("select params->'login'->>'password_enc' from job where id = $1::uuid").bind(&job).fetch_one(&t.st.db).await.unwrap();
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD.decode(enc).unwrap();
    assert_eq!(t.st.secrets.open_str(&raw).unwrap(), "new-root-pw");
    let (s, v) = t.req("POST", &format!("/api/v1/jobs/{job}/retry"), Some(json!({"ssh_password": 5})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    // 上架失败计入总览
    let ov = t.get("/api/v1/overview").await;
    assert_eq!(ov["failed"], 1, "{ov}");

    // 把这台上架失败的机器退役
    let name = t.get(&format!("/api/v1/machines/{machine}")).await["name"].as_str().unwrap().to_string();
    let rj = t.post(&format!("/api/v1/machines/{machine}/retire"), json!({"now": true, "confirm": name})).await["job_id"].as_str().unwrap().to_string();
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

/// 退役中的机器、退役作业失败：详情带 failed_job（网页据此给「查看作业 / 重试」）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retiring_machine_shows_failed_retire_job() {
    let t = setup(true).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "retiring").await;
    let job = Uuid::now_v7();
    sqlx::query("insert into job (id, kind, target_id, state, created_by, finished_at) values ($1, 'retire', $2, 'failed', 't', now())")
        .bind(job)
        .bind(node)
        .execute(&t.st.db)
        .await
        .unwrap();
    let m = t.get(&format!("/api/v1/machines/{node}")).await;
    assert_eq!(m["failed_job"], json!({"id": job, "kind": "retire", "state": "failed"}), "{m}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_runs_twice_without_error() {
    let t = setup(true).await;
    let team = seed_team(&t, "团队1").await;
    // 先以退役中建出来、数据备齐再改成已退役：后台维护起来时先跑的那一轮会收走并删除已退役的机器，
    // 数据还没备齐就被选中的话，删除作业会和下面的插入赛跑（外键约束）
    let node = seed_node(&t, team, 1, "203.0.113.10", "retiring").await;
    let name = t.get(&format!("/api/v1/machines/{node}")).await["name"].as_str().unwrap().to_string();
    // 该节点的访问汇总和分区表里的主机指标，彻底删除时要一并清掉
    for sql in [
        "insert into access_hourly (node_id, relay_id, hour, dest_host, kind, conns) values ($1, gen_random_uuid(), now(), 'a.com', 'domain', 1)",
        "insert into access_daily (node_id, relay_id, day, dest_host, kind, conns) values ($1, gen_random_uuid(), now(), 'a.com', 'domain', 1)",
        "insert into access_source_hourly (node_id, relay_id, hour, src_ip, conns) values ($1, gen_random_uuid(), now(), '1.1.1.1', 1)",
        "insert into access_target_seen (node_id, dest_host, first_seen, last_seen, total) values ($1, 'a.com', now(), now(), 1)",
        "insert into access_source_late (node_id, hour, src_ip, delta) values ($1, now(), '1.1.1.1', 1)",
        "insert into host_metric (machine_id, ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bytes, net_tx_bytes, tcp_conns) values ($1, now(), 1, 1, 1, 1, 1, 1, 1, 1, 1)",
    ] {
        sqlx::query(sql).bind(node).execute(&t.st.db).await.unwrap();
    }
    // 入口流量都在 93 天前：不再计入对端线路机的统计，可以删
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
    sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, date_trunc('hour', now()) - interval '94 days', 100, 0)")
        .bind(eid)
        .execute(&t.st.db)
        .await
        .unwrap();
    // 客户端订阅的 include_ids / exclude_ids 引用它（无外键）：彻底删除要一并摘掉，别的 id 留着
    let other = Uuid::now_v7();
    sqlx::query("insert into client_sub (id, name, token_hash, include_ids, exclude_ids) values ($1, '订阅', decode('01', 'hex'), $2, $3)")
        .bind(Uuid::now_v7())
        .bind(vec![node, other])
        .bind(vec![node])
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("update machine set lifecycle = 'retired' where id = $1").bind(node).execute(&t.st.db).await.unwrap();
    // 后台起来时维护任务先跑一轮：可能已经把它收进回收站、建好了物理删除作业
    let mut rx = t.st.events.subscribe();
    let first = match fleet_server::api::machines::start_purge(&t.st, "test", node, &name, None).await {
        Ok(j) => j.to_string(),
        Err(_) => sqlx::query_scalar::<_, Uuid>("select id from job where target_id = $1 and kind = 'purge'").bind(node).fetch_one(&t.st.db).await.unwrap().to_string(),
    };
    let j = t.wait_job(&first, "succeeded").await;
    assert_eq!(j["steps"][0]["state"], "done");
    // 机器没了：结束时要发 Machines（整表重取）
    let mut seen = vec![];
    while let Ok(e) = rx.try_recv() {
        seen.push(format!("{e:?}"));
    }
    assert!(seen.iter().any(|e| e == "Machines"), "purge 结束没发 Machines：{seen:?}");
    for tbl in ["access_hourly", "access_daily", "access_source_hourly", "access_target_seen", "access_source_late"] {
        let n: i64 = sqlx::query_scalar(&format!("select count(*) from {tbl} where node_id = $1")).bind(node).fetch_one(&t.st.db).await.unwrap();
        assert_eq!(n, 0, "{tbl} 没清干净");
    }
    let n: i64 = sqlx::query_scalar("select count(*) from host_metric where machine_id = $1").bind(node).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 0);
    let (s, _) = t.req("GET", &format!("/api/v1/machines/{node}"), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let n: i64 = sqlx::query_scalar("select count(*) from entry_traffic_hourly where entry_id = $1").bind(eid).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 0);
    let (inc, exc): (Vec<Uuid>, Vec<Uuid>) = sqlx::query_as("select include_ids, exclude_ids from client_sub where name = '订阅'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!((inc, exc), (vec![other], vec![]), "订阅里对已删机器的引用没摘掉");

    // 机器已经不在了，再来一个 purge 作业（比如上一次跑到一半进程挂了、重新领取）也应该直接成功
    let mut tx = t.st.db.begin().await.unwrap();
    let second = jobs::enqueue(&mut tx, "purge", Some(node), json!({}), "test").await.unwrap().to_string();
    tx.commit().await.unwrap();
    let j = t.wait_job(&second, "succeeded").await;
    assert_eq!(j["steps"][0]["output"]["already_gone"], true, "{j}");
}

/// 机器事件只在改了访问页显示的东西（名字、团队、编号、机器增删）时才作废访问查询缓存；连通性、续费、调优这类用 emit_keep_access
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn machine_events_clear_access_cache_only_when_names_may_change() {
    use fleet_server::state::Event;
    let t = setup(false).await;
    let id = Uuid::now_v7();
    let g = t.st.access_cache.gen();
    t.st.emit_keep_access(Event::Machine { id });
    t.st.emit_keep_access(Event::Machines);
    t.st.emit(Event::Job { id, target: None });
    assert_eq!(t.st.access_cache.gen(), g, "不改名字的事件不该清访问缓存");
    t.st.emit(Event::Machine { id });
    assert_eq!(t.st.access_cache.gen(), g + 1);
    t.st.emit(Event::Machines);
    assert_eq!(t.st.access_cache.gen(), g + 2);
}

/// 领取并推进一个作业，返回作业视图
async fn run_one(t: &common::T, job: Uuid) -> Value {
    let j = jobs::claim(&t.st, "test").await.unwrap().expect("有可领取的作业");
    assert_eq!(j.id, job);
    jobs::advance(&t.st, j, "test").await.unwrap();
    t.job(&job.to_string()).await
}

/// 领取顺序：耗时短的作业（退役、换线路机…）排在上架前面，上架排在升级 / 调优 / 下发 SS 前面，同类按创建先后
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claim_puts_short_jobs_before_provision_before_upgrade() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队1").await;
    let mut want = vec![];
    // 先建的是升级和上架，后建的退役、摘入口
    for (i, (kind, rank)) in [("upgrade_agent", 2), ("provision_proxy", 1), ("provision_vps", 1), ("retire", 0), ("attach_entry", 0)].into_iter().enumerate() {
        let node = seed_node(&t, team, i as i32 + 1, &format!("203.0.113.{}", i + 10), "active").await;
        let mut tx = t.st.db.begin().await.unwrap();
        let id = jobs::enqueue(&mut tx, kind, Some(node), json!({}), "test").await.unwrap();
        tx.commit().await.unwrap();
        want.push((rank, i, id));
    }
    want.sort();
    let mut got = vec![];
    while let Some(j) = jobs::claim(&t.st, "test").await.unwrap() {
        got.push(j.id);
    }
    assert_eq!(got, want.into_iter().map(|w| w.2).collect::<Vec<_>>());
}

/// 「上线」已经把机器改成了在用、但记账失败：重跑不被「机器状态不对」判死，这一步照常跑完（改状态那条语句是幂等的）；别的步骤遇到同样的状态仍然失败
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn activate_rerun_after_machine_already_active_completes() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let mut tx = t.st.db.begin().await.unwrap();
    let at_activate = jobs::enqueue(&mut tx, "provision_proxy", Some(node), json!({}), "test").await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("update job_step set state = 'done' where job_id = $1 and idx < 3").bind(at_activate).execute(&t.st.db).await.unwrap();
    sqlx::query("update job set step = 3 where id = $1").bind(at_activate).execute(&t.st.db).await.unwrap();
    let j = run_one(&t, at_activate).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    assert_eq!(j["steps"][3]["state"], "done", "{j}");
    assert_eq!(t.machine_lifecycle(node).await, "active");

    // 同样的机器状态，停在上线之前的步骤：照旧判定作业不能继续
    let mut tx = t.st.db.begin().await.unwrap();
    let at_verify = jobs::enqueue(&mut tx, "provision_proxy", Some(node), json!({}), "test").await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("update job set step = 2 where id = $1").bind(at_verify).execute(&t.st.db).await.unwrap();
    let j = run_one(&t, at_verify).await;
    assert_eq!(j["state"], "failed", "{j}");
    assert!(j["error"].as_str().unwrap().contains("不能再继续"), "{j}");
}

/// 「完成退役」已经把机器改成了已退役、但提交后的收尾（忘记主机公钥）没做完：重跑不被「机器状态不对」判死，
/// 也不整步跳过，状态不再改、收尾照做
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finalize_rerun_after_machine_already_retired_still_forgets_host() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "retired").await;
    let kh = t.dir.path().join("known_hosts");
    std::fs::write(&kh, "203.0.113.10 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGWzfakefakefakefakefakefakefakefakefakefakefake
").unwrap();
    let mut tx = t.st.db.begin().await.unwrap();
    let job = jobs::enqueue(&mut tx, "retire", Some(node), json!({}), "test").await.unwrap();
    tx.commit().await.unwrap();
    let idx = jobs::defs("retire").iter().position(|d| d.key == "finalize").unwrap() as i32;
    sqlx::query("update job_step set state = 'done' where job_id = $1 and idx < $2").bind(job).bind(idx).execute(&t.st.db).await.unwrap();
    sqlx::query("update job set step = $2 where id = $1").bind(job).bind(idx).execute(&t.st.db).await.unwrap();
    let j = run_one(&t, job).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    assert_eq!(j["steps"][idx as usize]["state"], "done", "{j}");
    assert!(!std::fs::read_to_string(&kh).unwrap().contains("203.0.113.10"), "主机公钥被忘掉了");
    let bins: i64 = sqlx::query_scalar("select count(*) from recycle_bin where machine_id = $1").bind(node).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(bins, 0, "状态已经是已退役：不再往回收站记");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_refused_while_peer_still_counts_traffic() {
    let t = setup(false).await; // 手动推进作业
    let team = seed_team(&t, "团队1").await;
    // 先以退役中建出来、数据备齐再改成已退役：后台维护起来时先跑的那一轮会收走并删除已退役的机器，
    // 数据还没备齐就被选中的话，删除作业会和下面的插入赛跑（外键约束）
    let node = seed_node(&t, team, 1, "203.0.113.10", "retiring").await;
    let name = t.get(&format!("/api/v1/machines/{node}")).await["name"].as_str().unwrap().to_string();
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
    // 10 天前还经这条入口走过流量：线路机的本月 / 周期统计还算着它。5 天前那行是 0 字节：不改任何统计，不往后推
    sqlx::query(
        "insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values
           ($1, date_trunc('hour', now()) - interval '10 days', 100, 0), ($1, date_trunc('hour', now()) - interval '5 days', 0, 0)",
    )
        .bind(eid)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("update machine set lifecycle = 'retired' where id = $1").bind(node).execute(&t.st.db).await.unwrap();
    let want: String = sqlx::query_scalar("select (((date_trunc('hour', now()) - interval '10 days' + interval '93 days 1 hour') at time zone 'Asia/Shanghai')::date + 1)::text")
        .fetch_one(&t.st.db)
        .await
        .unwrap();
    let msg = fleet_server::db::purge_blocker(&t.st.db, node).await.unwrap();
    assert_eq!(msg, Some(format!("这台机器的流量数据（入口业务流量、网卡）在 {want} 前仍能在流量页、月报里回看，届时再彻底删除")));

    // 直接建的作业：步骤里再查一次，同样拒绝，什么都不删
    let mut tx = t.st.db.begin().await.unwrap();
    let j1 = jobs::enqueue(&mut tx, "purge", Some(node), json!({}), "test").await.unwrap();
    tx.commit().await.unwrap();
    let j = run_one(&t, j1).await;
    assert_eq!(j["state"], "failed", "{j}");
    assert!(j["error"].as_str().unwrap().contains("前仍能在流量页、月报里回看"), "{j}");
    assert_eq!(t.machine_lifecycle(node).await, "retired");
    let n: i64 = sqlx::query_scalar("select count(*) from entry_traffic_hourly where entry_id = $1").bind(eid).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 2);
    // 只剩 0 字节的行：不拦
    sqlx::query("delete from entry_traffic_hourly where entry_id = $1 and up_bytes > 0").bind(eid).execute(&t.st.db).await.unwrap();
    assert_eq!(fleet_server::db::purge_blocker(&t.st.db, node).await.unwrap(), None);

    // 网卡小时数据也算（线路机的流量页、月报回看用）：近 93 天内有网卡流量的等，0 字节的行不算
    sqlx::query("insert into host_metric_hourly (machine_id, hour, net_rx, net_tx) values ($1, date_trunc('hour', now()) - interval '20 days', 5, 6), ($2, date_trunc('hour', now()) - interval '20 days', 0, 0)")
        .bind(node)
        .bind(relay)
        .execute(&t.st.db)
        .await
        .unwrap();
    let why = fleet_server::db::purge_blocker(&t.st.db, node).await.unwrap();
    assert!(why.is_some_and(|w| w.contains("前仍能在流量页、月报里回看")), "节点自己的网卡数据");
    assert_eq!(fleet_server::db::purge_blocker(&t.st.db, relay).await.unwrap(), None, "线路机只有 0 字节的网卡行");
    // 入口另一端（线路机）已退役也一样等：回看窗口里的数字不能变小（等待有天数上限，互为对端的两台不会卡死）
    sqlx::query("update machine set lifecycle = 'retired' where id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, date_trunc('hour', now()) - interval '10 days', 100, 0)")
        .bind(eid)
        .execute(&t.st.db)
        .await
        .unwrap();
    for m in [node, relay] {
        assert!(fleet_server::db::purge_blocker(&t.st.db, m).await.unwrap().is_some(), "两端都退役了入口流量也等：{m}");
    }
    // 数据过了回看窗口（这里直接清掉）：两台按任意顺序都能删
    sqlx::query("delete from entry_traffic_hourly where entry_id = $1").bind(eid).execute(&t.st.db).await.unwrap();
    sqlx::query("delete from host_metric_hourly where machine_id = any($1)").bind(vec![node, relay]).execute(&t.st.db).await.unwrap();
    let rname = t.get(&format!("/api/v1/machines/{relay}")).await["name"].as_str().unwrap().to_string();
    let nj = fleet_server::api::machines::start_purge(&t.st, "test", node, &name, None).await.unwrap();
    assert_eq!(run_one(&t, nj).await["state"], "succeeded");
    let rj = fleet_server::api::machines::start_purge(&t.st, "test", relay, &rname, None).await.unwrap();
    assert_eq!(run_one(&t, rj).await["state"], "succeeded");
    for m in [node, relay] {
        assert_eq!(t.req("GET", &format!("/api/v1/machines/{m}"), None, None).await.0, StatusCode::NOT_FOUND);
    }
}

/// 等待期是流量页能回看的天数加一天（traffic::MAX_DAYS + 1 = 93）：40 天前、92 天前的流量还拦着（流量页还看得到），94 天前的不拦
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_waits_as_long_as_traffic_page_looks_back() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "retiring").await;
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
    assert_eq!(fleet_server::db::PURGE_WAIT_DAYS as i64, fleet_server::traffic::MAX_DAYS + 1);
    for (days, blocked) in [(40, true), (92, true), (94, false)] {
        sqlx::query("delete from entry_traffic_hourly where entry_id = $1").bind(eid).execute(&t.st.db).await.unwrap();
        sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, date_trunc('hour', now()) - make_interval(days => $2), 100, 0)")
            .bind(eid)
            .bind(days)
            .execute(&t.st.db)
            .await
            .unwrap();
        let why = fleet_server::db::purge_blocker(&t.st.db, node).await.unwrap();
        assert_eq!(why.is_some(), blocked, "{days} 天前的流量：{why:?}");
    }
}

/// 没做完的改软路由出口作业（参数 routers_applied 记着它写的新入口）还要照那条入口改回：入口所在的节点和线路机都不能物理删除，
/// 作业结束（成功 / 取消）后放行；没有引用的机器不受影响
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_waits_for_unfinished_router_job() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "retiring").await;
    let other = seed_node(&t, team, 2, "203.0.113.11", "retiring").await;
    let (relay, _) = seed_relay(&t).await;
    let eid = Uuid::now_v7();
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, 20001, $4, 'ab', 'active')")
        .bind(eid)
        .bind(relay)
        .bind(node)
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();
    let mut tx = t.st.db.begin().await.unwrap();
    let job = jobs::enqueue(&mut tx, "replace", None, json!({}), "test").await.unwrap();
    tx.commit().await.unwrap();
    let applied = json!({"routers_applied": [{"id": Uuid::nil(), "name": "路由A", "port": 1, "stamp": "S", "exits": [{"exit": "节点01", "new_entry_id": eid}]}]});
    sqlx::query("update job set state = 'failed', params = params || $2::jsonb where id = $1").bind(job).bind(&applied).execute(&t.st.db).await.unwrap();
    for m in [node, relay] {
        let why = fleet_server::db::purge_blocker(&t.st.db, m).await.unwrap();
        assert!(why.is_some_and(|w| w.contains("改软路由出口作业")), "{m}");
    }
    assert_eq!(fleet_server::db::purge_blocker(&t.st.db, other).await.unwrap(), None);
    // 作业改回完清空 routers_applied（或取消）后放行
    sqlx::query("update job set params = params || '{\"routers_applied\": []}' where id = $1").bind(job).execute(&t.st.db).await.unwrap();
    assert_eq!(fleet_server::db::purge_blocker(&t.st.db, node).await.unwrap(), None);
    sqlx::query("update job set params = params || $2::jsonb, state = 'cancelled' where id = $1").bind(job).bind(&applied).execute(&t.st.db).await.unwrap();
    assert_eq!(fleet_server::db::purge_blocker(&t.st.db, relay).await.unwrap(), None);
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

/// 在用的 VPS 节点（kind vps），名字「团队-编号」
async fn seed_vps(t: &common::T, team: Uuid, number: i32, ip: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, lifecycle) values ($1, 'vps', $2, $3, $4::inet, 'active')")
        .bind(id)
        .bind(team)
        .bind(number)
        .bind(ip)
        .execute(&t.st.db)
        .await
        .unwrap();
    id
}

/// 再放一台在用的线路机（升级只看机器行，不用 relay 参数）
async fn seed_relay_named(t: &common::T, label: &str, ip: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle) values ($1, 'relay', $2, $3::inet, 'active')")
        .bind(id)
        .bind(label)
        .bind(ip)
        .execute(&t.st.db)
        .await
        .unwrap();
    id
}

/// 一直领到领不到为止，返回领到的作业
async fn claim_all(t: &common::T) -> Vec<Uuid> {
    let mut got = vec![];
    while let Some(j) = jobs::claim(&t.st, "test").await.unwrap() {
        got.push(j.id);
    }
    got
}

/// 「全部升级」：一批升级作业不挡住之后建的其它作业；先升金丝雀（线路机里在用入口最少的，一样少按名字），
/// 它成功后线路机一台接一台，线路机都成功后 VPS 节点才领得到（一起领：并发）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bulk_upgrade_runs_canary_then_relays_one_by_one() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队1").await;
    // VPS 节点先插入（id 更小）：领取顺序看编排，不是机器 id
    let mut vps = vec![];
    for i in 1..=3 {
        vps.push(seed_vps(&t, team, i, &format!("203.0.113.{}", 20 + i)).await);
    }
    let (r2, _) = seed_relay(&t).await;
    seed_relay_named(&t, "线路机4", "198.51.100.4").await;
    seed_relay_named(&t, "线路机3", "198.51.100.3").await;
    // 线路机2 上有一个经团队1-01 的入口：线路机2 排在线路机里最后、团队1-01 排在 VPS 节点里最后
    let mut tx = t.st.db.begin().await.unwrap();
    fleet_server::entries::lock_policy_shared(&mut tx).await.unwrap();
    fleet_server::entries::create(&mut tx, r2, vps[0]).await.unwrap();
    tx.commit().await.unwrap();

    dist_agent(&t, "0.1.0+a");
    let up = t.post("/api/v1/agents/upgrade", json!({})).await;
    let list = up["jobs"].as_array().unwrap();
    let names: Vec<&str> = list.iter().map(|j| j["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["线路机3", "线路机4", "线路机2", "团队1-02", "团队1-03", "团队1-01"], "{up}");
    assert_eq!(
        up["order"],
        "先升「线路机3」做金丝雀，成功后其余 2 台线路机逐台升级，线路机都成功后 3 台 VPS 节点并发升级；新版本在任何一台线路机上出问题，没开始的都会取消"
    );
    // 编排：线路机一台等前一台，VPS 节点都等最后一台线路机
    let ids: Vec<Uuid> = list.iter().map(|j| j["job_id"].as_str().unwrap().parse().unwrap()).collect();
    let after: Vec<Option<Uuid>> = list.iter().map(|j| j["after"].as_str().map(|s| s.parse().unwrap())).collect();
    assert_eq!(after, [None, Some(ids[0]), Some(ids[1]), Some(ids[2]), Some(ids[2]), Some(ids[2])]);
    // 每个作业都记着发起时 dist 里 agent 的 sha256 与版本号
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(std::fs::read(t.dir.path().join("dist/fleet-agent")).unwrap()));
    let rollouts: Vec<Value> = sqlx::query_scalar("select params->'rollout' from job where id = any($1)").bind(&ids).fetch_all(&t.st.db).await.unwrap();
    assert!(rollouts.iter().all(|r| *r == json!({"sha256": sha, "version": "0.1.0+a"})), "{rollouts:?}");

    let node = seed_node(&t, team, 9, "203.0.113.10", "provisioning").await;
    let mut tx = t.st.db.begin().await.unwrap();
    let prov = jobs::enqueue(&mut tx, "provision_proxy", Some(node), json!({"relay_id": r2}), "test").await.unwrap();
    tx.commit().await.unwrap();

    // 后建的上架作业先领，然后只有金丝雀
    assert_eq!(claim_all(&t).await, [prov, ids[0]]);
    // 前一台成功了才轮到下一台线路机；线路机都成功后三台 VPS 节点一起领得到
    let done = |id: Uuid| {
        let db = t.st.db.clone();
        async move {
            sqlx::query("update job set state = 'succeeded', finished_at = now(), lease_owner = null, lease_until = null where id = $1")
                .bind(id)
                .execute(&db)
                .await
                .unwrap();
        }
    };
    for k in 1..3 {
        done(ids[k - 1]).await;
        assert_eq!(claim_all(&t).await, [ids[k]], "第 {} 台线路机", k + 1);
    }
    done(ids[2]).await;
    assert_eq!(claim_all(&t).await, ids[3..]);
}

/// 「全部升级」记下发起时 dist 里 agent 的 sha256 与版本号：中途重新部署过中控（dist 换了），推送前核对出来就不推、失败，
/// 没开始的都取消、写明原因，后面的机器不会装上金丝雀没验过的版本。dist 里没有 agent 时不发起
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bulk_upgrade_stops_when_dist_changes() {
    let t = setup(false).await;
    seed_relay(&t).await;
    seed_relay_named(&t, "线路机3", "198.51.100.3").await;
    seed_relay_named(&t, "线路机4", "198.51.100.4").await;
    let (s, v) = t.req("POST", "/api/v1/agents/upgrade", Some(json!({})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().starts_with("中控上没有可推送的 agent 程序"), "{v}");
    let n: i64 = sqlx::query_scalar("select count(*) from job").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 0);

    dist_agent(&t, "0.1.0+a");
    let up = t.post("/api/v1/agents/upgrade", json!({})).await;
    let ids: Vec<Uuid> = up["jobs"].as_array().unwrap().iter().map(|j| j["job_id"].as_str().unwrap().parse().unwrap()).collect();
    // 金丝雀已经开始、还没推送时中控被重新部署（先换 dist、后重启），重启后接着做这一步：推送前核对出来，不推、失败
    sqlx::query("update job_step set state = 'running', started_at = now() where job_id = $1 and idx = 0").bind(ids[0]).execute(&t.st.db).await.unwrap();
    sqlx::query("update job set params = params || '{\"pushed\": false}' where id = $1").bind(ids[0]).execute(&t.st.db).await.unwrap();
    dist_agent(&t, "0.1.0+b");
    let why = "中控上的 agent 在升级过程中换了版本（发起时是 0.1.0+a），请重新发起全部升级";
    let j = run_one(&t, ids[0]).await;
    assert_eq!((j["state"].as_str(), j["error"].as_str()), (Some("failed"), Some(format!("推送并重启 agent：{why}").as_str())), "{j}");
    // 没开始的：开始前核对出来，一步不做、取消
    for id in &ids[1..] {
        let j = run_one(&t, *id).await;
        assert_eq!((j["state"].as_str(), j["error"].as_str()), (Some("cancelled"), Some(why)), "{j}");
        assert!(j["started_at"].is_null(), "{j}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_with_entries_cannot_be_retired() {
    let t = setup(false).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let mut tx = t.st.db.begin().await.unwrap();
    fleet_server::entries::lock_policy_shared(&mut tx).await.unwrap();
    let (entry, _, _) = fleet_server::entries::create(&mut tx, relay, node).await.unwrap();
    tx.commit().await.unwrap();

    // 预览里先列出同一条阻断原因
    let pv = t.get(&format!("/api/v1/machines/{relay}/retire")).await;
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{relay}/retire"), Some(json!({"now": true, "confirm": "线路机2"})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("节点入口"), "{v}");
    assert_eq!(pv["blockers"], json!([v["error"]["message"]]), "{pv}");
    assert_eq!(t.machine_lifecycle(relay).await, "active");

    // 入口摘光之后才允许退役
    sqlx::query("update entry set lifecycle = 'removed', removed_at = now() where id = $1").bind(entry).execute(&t.st.db).await.unwrap();
    assert_eq!(t.get(&format!("/api/v1/machines/{relay}/retire")).await["blockers"], json!([]));
    let r: Value = t.post(&format!("/api/v1/machines/{relay}/retire"), json!({"now": true, "confirm": "线路机2"})).await;
    assert!(r["job_id"].is_string());
    assert_eq!(t.machine_lifecycle(relay).await, "retiring");
    // 已经在退役：再点一次是 409，而不是又排一个作业
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{relay}/retire"), Some(json!({"now": true, "confirm": "线路机2"})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // 退役开始后，不能再往这台线路机上加入口
    let mut tx = t.st.db.begin().await.unwrap();
    let node2 = seed_node(&t, team, 2, "203.0.113.11", "active").await;
    fleet_server::entries::lock_policy_shared(&mut tx).await.unwrap();
    assert!(fleet_server::entries::create(&mut tx, relay, node2).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retire_closes_open_incidents_up_front() {
    let t = setup(false).await;
    let (relay, _) = seed_relay(&t).await;
    let inc = Uuid::now_v7();
    sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary) values ($1, 'machine', 'relay_xray_down', $2, 'critical', 's')")
        .bind(inc)
        .bind(relay)
        .execute(&t.st.db)
        .await
        .unwrap();
    let r: Value = t.post(&format!("/api/v1/machines/{relay}/retire"), json!({"now": true, "confirm": "线路机2"})).await;
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

    // 手动清理（POST /jobs/prune）：不等 90 天，清掉剩下的成功 / 已取消的；失败的不动。dry_run 只数，before 之后结束的不清
    let pv = t.post("/api/v1/jobs/prune", json!({"dry_run": true})).await;
    assert_eq!((pv["jobs"].as_i64(), exists(recent_ok).await), (Some(1), true), "{pv}");
    assert_eq!(t.post("/api/v1/jobs/prune", json!({"before": "2000-01-01T00:00:00Z"})).await["jobs"], 0);
    assert_eq!(t.post("/api/v1/jobs/prune", json!({})).await["jobs"], 1);
    assert!(!exists(recent_ok).await && exists(old_failed).await && exists(fresh_failed).await);
    let au = t.get("/api/v1/audit?cat=machine").await;
    assert_eq!((&au[0]["title"], au[0]["summary"].as_str().unwrap().starts_with("1 个（结束早于 ")), (&json!("清理作业记录"), true), "{au}");
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

    // 按类型：provision 是大类，其余按原值；和 state 一起用
    for (kind, state) in [("provision_vps", "succeeded"), ("provision_relay", "running"), ("move_entry", "failed"), ("upgrade_agent", "succeeded")] {
        sqlx::query("insert into job (id, kind, params, state, created_by) values ($1, $2, '{}', $3::job_state, 'test')")
            .bind(Uuid::now_v7())
            .bind(kind)
            .bind(state)
            .execute(&t.st.db)
            .await
            .unwrap();
    }
    let kinds = |v: &Value| v["items"].as_array().unwrap().iter().map(|j| j["kind"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let p = t.get("/api/v1/jobs?kind=provision").await;
    assert_eq!((p["total"].as_i64(), kinds(&p)), (Some(2), vec!["provision_relay".to_string(), "provision_vps".into()]));
    assert_eq!(kinds(&t.get("/api/v1/jobs?kind=provision&state=done").await), ["provision_vps"]);
    assert_eq!(kinds(&t.get("/api/v1/jobs?kind=move_entry").await), ["move_entry"]);
    assert_eq!(t.get("/api/v1/jobs?kind=purge").await["total"], 5);
    assert_eq!(t.get("/api/v1/jobs?kind=all").await["total"], 9);
    let (s, _) = t.req("GET", "/api/v1/jobs?kind=nope", None, None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// 直接在库里放一个失败在第 `step` 步、类别为 `ek` 的作业
async fn seed_failed_job(t: &common::T, kind: &str, target: Uuid, step: i32, ek: &str) -> Uuid {
    let mut tx = t.st.db.begin().await.unwrap();
    let id = jobs::enqueue(&mut tx, kind, Some(target), json!({}), "test").await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("update job set state = 'failed', step = $2, finished_at = now() where id = $1").bind(id).bind(step).execute(&t.st.db).await.unwrap();
    sqlx::query("update job_step set state = 'failed', error = 'x', error_kind = $3 where job_id = $1 and idx = $2")
        .bind(id)
        .bind(step)
        .bind(ek)
        .execute(&t.st.db)
        .await
        .unwrap();
    id
}

/// 重试时重置主机指纹：只给因指纹不符失败的作业（不限种类）、目标不是同机 IP 节点；其它情形 409，且不碰 known_hosts、不改作业与机器
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_reset_host_key_is_narrow() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队1").await;
    let kh = t.dir.path().join("known_hosts");
    let pinned = "[127.0.0.1]:1 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGWzOldOldOldOldOldOldOldOldOldOldOldOldOld\n";
    std::fs::write(&kh, pinned).unwrap();
    let vps = seed_vps(&t, team, 1, "127.0.0.1").await;
    sqlx::query("update machine set ssh_port = 1, lifecycle = 'failed' where id = $1").bind(vps).execute(&t.st.db).await.unwrap();
    let (tr, khr) = (&t, &kh);
    let retry = |job: Uuid| async move { tr.req("POST", &format!("/api/v1/jobs/{job}/retry"), Some(json!({"reset_host_key": true})), None).await };
    let unchanged = |job: Uuid| async move {
        let (s, ek): (String, Option<String>) =
            sqlx::query_as("select j.state::text, s.error_kind from job j join job_step s on s.job_id = j.id and s.idx = j.step where j.id = $1")
                .bind(job)
                .fetch_one(&tr.st.db)
                .await
                .unwrap();
        assert_eq!(s, "failed", "被拒的重试不改作业");
        assert!(ek.is_some(), "失败步骤的类别还在");
        assert_eq!(std::fs::read_to_string(khr).unwrap(), pinned, "known_hosts 没被动过");
    };

    // 上架作业，但不是因为指纹失败的
    let j = seed_failed_job(&t, "provision_vps", vps, 2, "network").await;
    let (s, v) = retry(j).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("不是因为主机指纹不符"), "{v}");
    unchanged(j).await;
    assert_eq!(t.machine_lifecycle(vps).await, "failed", "事务回滚，机器没被拉回上架中");

    // 指纹不符失败的上架作业：检查都过了才去 keyscan（127.0.0.1:1 连不上 → 重置失败），整个重试作废
    sqlx::query("update job_step set error_kind = 'host_key' where job_id = $1").bind(j).execute(&t.st.db).await.unwrap();
    let (s, v) = retry(j).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("重置主机指纹失败"), "{v}");
    unchanged(j).await;
    assert_eq!(t.machine_lifecycle(vps).await, "failed");

    // 机器状态不对：先按作业种类检查机器，不去重置
    sqlx::query("update machine set lifecycle = 'retired' where id = $1").bind(vps).execute(&t.st.db).await.unwrap();
    let (s, v) = retry(j).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("已退役"), "{v}");
    unchanged(j).await;
    sqlx::query("update job set state = 'cancelled' where id = $1").bind(j).execute(&t.st.db).await.unwrap();

    // 不限作业种类（升级 agent，在用的机器）：不是指纹失败的拒绝；指纹失败的放行到 keyscan（127.0.0.2:1 连不上 → 重置失败、重试作废）
    let host = seed_vps(&t, team, 2, "127.0.0.2").await;
    sqlx::query("update machine set ssh_port = 1 where id = $1").bind(host).execute(&t.st.db).await.unwrap();
    let u = seed_failed_job(&t, "upgrade_agent", host, 0, "remote").await;
    let (s, v) = retry(u).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("不是因为主机指纹不符"), "{v}");
    unchanged(u).await;
    sqlx::query("update job_step set error_kind = 'host_key' where job_id = $1").bind(u).execute(&t.st.db).await.unwrap();
    let (s, v) = retry(u).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("重置主机指纹失败"), "{v}");
    unchanged(u).await;
    sqlx::query("update job set state = 'cancelled' where id = $1").bind(u).execute(&t.st.db).await.unwrap();

    // 同机 IP 节点：登录的是宿主机，不能借它重置宿主机的指纹
    let ipn = seed_vps(&t, team, 3, "198.51.100.10").await;
    sqlx::query("update machine set host_id = $2, ssh_port = null, lifecycle = 'failed' where id = $1").bind(ipn).bind(host).execute(&t.st.db).await.unwrap();
    let p = seed_failed_job(&t, "provision_ip", ipn, 0, "host_key").await;
    let (s, v) = retry(p).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("同机 IP 节点"), "{v}");
    unchanged(p).await;
    assert_eq!(t.machine_lifecycle(ipn).await, "failed");
}

/// 退役：有作业正在执行一步（租约未过期）时拒绝；租约过期（执行者已不在）照常作废它
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retire_refused_while_a_step_is_running() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let mut tx = t.st.db.begin().await.unwrap();
    let job = jobs::enqueue(&mut tx, "attach_entry", Some(node), json!({}), "test").await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("update job set state = 'running', lease_owner = 'w', lease_until = now() + interval '5 minutes' where id = $1")
        .bind(job)
        .execute(&t.st.db)
        .await
        .unwrap();
    let name = t.get(&format!("/api/v1/machines/{node}")).await["name"].as_str().unwrap().to_string();
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{node}/retire"), Some(json!({"now": true, "confirm": name})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("正在执行一步"), "{v}");
    assert_eq!(t.machine_lifecycle(node).await, "active");
    let st0: String = sqlx::query_scalar("select state::text from job where id = $1").bind(job).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(st0, "running");

    // 退役原因有长度上限
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{node}/retire"), Some(json!({"now": true, "confirm": name, "reason": "x".repeat(2001)})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // 租约过期：执行者已经不在，作废它、照常退役
    sqlx::query("update job set lease_until = now() - interval '1 second' where id = $1").bind(job).execute(&t.st.db).await.unwrap();
    t.post(&format!("/api/v1/machines/{node}/retire"), json!({"now": true, "confirm": name})).await;
    assert_eq!(t.machine_lifecycle(node).await, "retiring");
    let st0: String = sqlx::query_scalar("select state::text from job where id = $1").bind(job).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(st0, "cancelled");
}

/// 退役：这台是进行中的替换 / 改出口指向的新节点（作业的目标是旧节点，路由要改成或已改成指向它）时拒绝，排队中的也算；作业结束后照常退役
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retire_refused_while_it_is_the_new_node_of_a_job() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队1").await;
    let old = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let new = seed_node(&t, team, 2, "203.0.113.11", "active").await;
    let name = t.get(&format!("/api/v1/machines/{new}")).await["name"].as_str().unwrap().to_string();
    for kind in ["replace", "repoint"] {
        let mut tx = t.st.db.begin().await.unwrap();
        let job = jobs::enqueue(&mut tx, kind, Some(old), json!({"new_id": new}), "test").await.unwrap();
        tx.commit().await.unwrap();
        let (s, v) = t.req("POST", &format!("/api/v1/machines/{new}/retire"), Some(json!({"now": true, "confirm": name})), None).await;
        assert_eq!(s, StatusCode::CONFLICT, "{kind} {v}");
        assert!(v["error"]["message"].as_str().unwrap().contains("的新节点"), "{v}");
        assert!(t.get(&format!("/api/v1/machines/{new}/retire")).await["blockers"].to_string().contains("的新节点"), "{kind} 预览也列出");
        assert_eq!(t.machine_lifecycle(new).await, "active");
        let st0: String = sqlx::query_scalar("select state::text from job where id = $1").bind(job).fetch_one(&t.st.db).await.unwrap();
        assert_eq!(st0, "queued");
        sqlx::query("update job set state = 'cancelled', finished_at = now() where id = $1").bind(job).execute(&t.st.db).await.unwrap();
    }
    t.post(&format!("/api/v1/machines/{new}/retire"), json!({"now": true, "confirm": name})).await;
    assert_eq!(t.machine_lifecycle(new).await, "retiring");
}

/// 一台在用的家宽代理发起退役，返回作业 id。退役的步骤不登录机器；最后一步「完成退役」要 for update 锁团队行：
/// 先拿着 `team` 行的 for share 锁（不挡插入机器时外键要的 key share）再调，作业就停在这一步（库里等锁）
async fn retire_blocked_on_team(t: &common::T, team: Uuid) -> String {
    let node = seed_node(t, team, 1, "203.0.113.10", "active").await;
    let name = t.get(&format!("/api/v1/machines/{node}")).await["name"].as_str().unwrap().to_string();
    t.post(&format!("/api/v1/machines/{node}/retire"), json!({"now": true, "confirm": name})).await["job_id"].as_str().unwrap().to_string()
}

/// 退役步骤里会等行锁的语句开头：「完成退役」锁团队行、「撤销令牌」改令牌行
const FINALIZE: &str = "select 1 from team where id = $1 for update";
const REVOKE: &str = "update agent_token set revoked_at = now() where machine_id = $1";

/// 本测试里正在执行 `$1` 开头的语句、等行锁的连接。后台任务等咨询锁的不算：那把锁各测试共用
const WAITERS: &str = "from pg_stat_activity where application_name = current_setting('application_name') and wait_event_type = 'Lock'
                       and left(query, length($1)) = $1";

async fn lock_waiters(t: &common::T, stmt: &str) -> i64 {
    sqlx::query_scalar(&format!("select count(*) {WAITERS}")).bind(stmt).fetch_one(&t.st.db).await.unwrap()
}

/// 等到恰好有 `n` 个连接在执行 `stmt` 时等锁
async fn wait_lock_waiters(t: &common::T, stmt: &str, n: i64) {
    for _ in 0..100 {
        if lock_waiters(t, stmt).await == n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("等锁的连接没到 {n} 个：{}", lock_waiters(t, stmt).await);
}

/// 本进程还在推进的作业，租约过期了（墙钟前跳、续租前库卡住）也不会被本进程再领一次：同一步不会两个任务同时跑
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_does_not_reclaim_a_job_it_is_still_running() {
    let t = setup(true).await;
    let team = seed_team(&t, "团队1").await;
    let mut lock = t.st.db.begin().await.unwrap();
    sqlx::query("select 1 from team where id = $1 for share").bind(team).execute(&mut *lock).await.unwrap();
    let job = retire_blocked_on_team(&t, team).await;
    wait_lock_waiters(&t, FINALIZE, 1).await;
    let owner = "select lease_owner from job where id = $1::uuid";
    let before: Option<String> = sqlx::query_scalar(owner).bind(&job).fetch_one(&t.st.db).await.unwrap();
    assert!(before.is_some(), "作业在推进中");

    sqlx::query("update job set lease_until = now() - interval '1 second' where id = $1::uuid").bind(&job).execute(&t.st.db).await.unwrap();
    t.st.wake_jobs();
    tokio::time::sleep(NOT_RECLAIMED_WAIT).await;
    let after: Option<String> = sqlx::query_scalar(owner).bind(&job).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(after, before, "没有被再领一次");
    assert_eq!(lock_waiters(&t, FINALIZE).await, 1, "「完成退役」只有一个任务在跑");

    lock.rollback().await.unwrap();
    let j = t.wait_job(&job, "succeeded").await;
    assert_eq!(j["steps"][4]["state"], "done", "{j}");
}

/// 步骤里遇到库暂时不可用（这里把等锁的连接断开，同 PG 重启时被断开的查询）：作业不失败，放开租约，10 秒后从这一步重做
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn step_retried_when_the_database_drops_its_query() {
    let t = setup(true).await;
    let team = seed_team(&t, "团队1").await;
    let mut lock = t.st.db.begin().await.unwrap();
    sqlx::query("select 1 from team where id = $1 for share").bind(team).execute(&mut *lock).await.unwrap();
    let job = retire_blocked_on_team(&t, team).await;
    wait_lock_waiters(&t, FINALIZE, 1).await;
    sqlx::query(&format!("select pg_terminate_backend(pid) {WAITERS}")).bind(FINALIZE).execute(&t.st.db).await.unwrap();

    let mut released = false;
    for _ in 0..100 {
        let (owner, later): (Option<String>, bool) = sqlx::query_as("select lease_owner, run_after > now() + interval '5 seconds' from job where id = $1::uuid")
            .bind(&job)
            .fetch_one(&t.st.db)
            .await
            .unwrap();
        if owner.is_none() && later {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(released, "放开租约、约 10 秒后重做");
    let j = t.job(&job).await;
    assert_eq!((j["state"].as_str(), j["step"].as_i64(), j["steps"][4]["state"].as_str()), (Some("running"), Some(4), Some("running")), "{j}");
    assert!(j["error"].is_null() && j["steps"][4]["error"].is_null(), "{j}");
    let logs = t.get(&format!("/api/v1/jobs/{job}/logs")).await;
    assert!(logs.as_array().unwrap().iter().any(|l| l["msg"].as_str().unwrap().contains("10 秒后从这一步重做")), "{logs}");

    // 库恢复（锁放开）后重做这一步，作业完成
    lock.rollback().await.unwrap();
    sqlx::query("update job set run_after = now() where id = $1::uuid").bind(&job).execute(&t.st.db).await.unwrap();
    t.st.wake_jobs();
    let j = t.wait_job(&job, "succeeded").await;
    assert_eq!(j["steps"][4]["state"], "done", "{j}");
}

/// 优雅退出：不再领新作业；推进中的做完当前这一步就放开租约，不往下做，重启后从下一步接着做；等不完的到时返回还在推进的个数
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_lets_the_running_step_finish_then_lets_go() {
    let t = setup(true).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    sqlx::query("insert into agent_token (id, machine_id, token_hash) values ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(node)
        .bind(fleet_server::crypto::sha256(b"fa1_drain"))
        .execute(&t.st.db)
        .await
        .unwrap();
    // 拿着令牌行的锁：退役停在「撤销令牌」
    let mut lock = t.st.db.begin().await.unwrap();
    sqlx::query("select 1 from agent_token where machine_id = $1 for share").bind(node).execute(&mut *lock).await.unwrap();
    let name = t.get(&format!("/api/v1/machines/{node}")).await["name"].as_str().unwrap().to_string();
    let job = t.post(&format!("/api/v1/machines/{node}/retire"), json!({"now": true, "confirm": name})).await["job_id"].as_str().unwrap().to_string();
    wait_lock_waiters(&t, REVOKE, 1).await;
    assert_eq!(jobs::drain(&t.st, Duration::from_millis(300)).await, 1, "「撤销令牌」还没做完");

    lock.rollback().await.unwrap();
    assert_eq!(jobs::drain(&t.st, Duration::from_secs(10)).await, 0);
    let j = t.job(&job).await;
    let steps = |i: usize| j["steps"][i]["state"].as_str().unwrap().to_string();
    assert_eq!((j["state"].as_str(), j["step"].as_i64(), steps(3), steps(4)), (Some("running"), Some(4), "done".into(), "pending".into()), "{j}");
    let logs = t.get(&format!("/api/v1/jobs/{job}/logs")).await;
    assert!(logs.as_array().unwrap().iter().any(|l| l["msg"] == "中控正在重启：「完成退役」等重启后接着做"), "{logs}");
    // 租约放开、到点了，也没人再领
    t.st.wake_jobs();
    tokio::time::sleep(NOT_RECLAIMED_WAIT).await;
    let (owner, step4): (Option<String>, String) =
        sqlx::query_as("select j.lease_owner, s.state from job j join job_step s on s.job_id = j.id and s.idx = 4 where j.id = $1::uuid")
            .bind(&job)
            .fetch_one(&t.st.db)
            .await
            .unwrap();
    assert_eq!((owner, step4.as_str()), (None, "pending"));
}

/// 中控刚恢复：等摘除的退役不因线路机「不在线」失败——线路机停机前一直在报，只是重发还没到（停机这段收不到上报，不算它的沉默）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_detached_tolerates_central_outage() {
    let t = setup(false).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, 20001, $4, 'ab', 'active')")
        .bind(Uuid::now_v7())
        .bind(relay)
        .bind(node)
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();
    // 中控停了 5 分钟：线路机最后一次上报在停的那一刻
    sqlx::query("insert into agent_state (machine_id, agent_version, last_report_at) values ($1, 'v', now() - interval '5 minutes')").bind(relay).execute(&t.st.db).await.unwrap();
    let name = t.get(&format!("/api/v1/machines/{node}")).await["name"].as_str().unwrap().to_string();
    let job: Uuid = t.post(&format!("/api/v1/machines/{node}/retire"), json!({"now": true, "confirm": name})).await["job_id"].as_str().unwrap().parse().unwrap();
    let j = run_one(&t, job).await;
    assert_eq!((j["state"].as_str(), j["step"].as_i64()), (Some("waiting"), Some(1)), "摘除入口，开始等线路机摘除：{j}");
    // 「等线路机摘除」已经等了 2 分钟
    let stale = "update job_step set started_at = now() - interval '2 minutes' where job_id = $1 and idx = 1";
    let due = "update job set run_after = now() where id = $1";
    for q in [stale, due] {
        sqlx::query(q).bind(job).execute(&t.st.db).await.unwrap();
    }
    // 按墙钟算线路机 5 分钟没报：判不在线，失败
    let j = run_one(&t, job).await;
    assert!(j["state"] == "failed" && j["error"].as_str().unwrap().contains("线路机2 不在线"), "{j}");

    // 中控刚启动（停机这段收不到上报）：线路机算在线，接着等
    t.st.report_gaps.started(&t.st.db).await.unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    for q in [stale, due] {
        sqlx::query(q).bind(job).execute(&t.st.db).await.unwrap();
    }
    let j = run_one(&t, job).await;
    assert_eq!((j["state"].as_str(), j["step"].as_i64()), (Some("waiting"), Some(1)), "{j}");
}

/// fleet-server serve 进程：先占端口再碰库——同一份配置再起一个，因端口被占在碰库之前退出，不会清掉在跑实例手里的作业租约；
/// SIGTERM 时优雅退出
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_serve_exits_before_touching_the_database() {
    let t = setup(false).await;
    let schema: String = sqlx::query_scalar("select current_schema()").fetch_one(&t.st.db).await.unwrap();
    let url = std::env::var("FLEET_TEST_DB").unwrap_or_else(|_| "postgres://fleet:fleet@127.0.0.1/fleet_test".into());
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let d = t.dir.path().display().to_string();
    std::fs::write(t.dir.path().join("secret.key"), [7u8; 32]).unwrap();
    let cfg = t.dir.path().join("fleet.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"listen = "127.0.0.1:{port}"
               database_url = "{url}?options[search_path]={schema}"
               secret_key_file = "{d}/secret.key"
               public_url = "http://fleet.test"
               ssh_key = "/nonexistent"
               known_hosts = "{d}/known_hosts"
               dist_dir = "{d}/dist"
               ipintel.db_dir = "{d}/ipintel"
               ipintel.overrides = "/nonexistent/ipintel-overrides.toml""#
        ),
    )
    .unwrap();
    let serve = || {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_fleet-server"))
            .args(["serve", "-c"])
            .arg(&cfg)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };
    let first = serve();
    // 端口一开始就占住了（迁移、分区维护在后面）：等到能应答请求，才算起来了
    let mut up = false;
    for _ in 0..300 {
        let me = reqwest::Client::new().get(format!("http://127.0.0.1:{port}/api/v1/auth/me")).timeout(Duration::from_secs(1)).send().await;
        if me.is_ok_and(|r| r.status().as_u16() == 401) {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(up, "第一个没起来");
    // 在跑实例手里的一个作业（租约有效）
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let mut tx = t.st.db.begin().await.unwrap();
    let job = jobs::enqueue(&mut tx, "attach_entry", Some(node), json!({}), "test").await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("update job set state = 'running', lease_owner = 'w', lease_until = now() + interval '5 minutes' where id = $1").bind(job).execute(&t.st.db).await.unwrap();

    let second = tokio::time::timeout(Duration::from_secs(30), serve().wait_with_output()).await.expect("第二个应当马上退出").unwrap();
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(!second.status.success() && err.contains(&format!("监听 127.0.0.1:{port}")), "{err}");
    let owner: Option<String> = sqlx::query_scalar("select lease_owner from job where id = $1").bind(job).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(owner.as_deref(), Some("w"), "第二个没有碰库");

    let pid = first.id().unwrap().to_string();
    assert!(tokio::process::Command::new("kill").args(["-TERM", &pid]).status().await.unwrap().success());
    let out = tokio::time::timeout(Duration::from_secs(30), first.wait_with_output()).await.expect("SIGTERM 后应当退出").unwrap();
    let log = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && log.contains("收到退出信号") && log.contains("fleet-server 已退出"), "{log}");
}
