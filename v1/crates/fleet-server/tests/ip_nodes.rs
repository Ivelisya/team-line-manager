//! 同机多 IP 节点：一台 VPS 节点（宿主机）上的附加 IP 各是一台节点，SSH、agent、主机读数都用宿主机的。
//! 需要 PostgreSQL（见 common）。宿主机不真的登录：要跑到 SSH 的作业让宿主机指向 127.0.0.1:1（必定连接被拒），
//! 退役用 cleanup=false。

mod common;

use axum::http::StatusCode;
use common::{seed_node, seed_relay, seed_team, setup, T};
use fleet_proto::{DesiredState, Outbound};
use serde_json::{json, Value};
use uuid::Uuid;

/// 直接在库里放一台 VPS 节点当宿主机（同机 IP 节点 `host` 指向它时就是同机 IP 节点；同机 IP 节点不存 SSH 端口，`ssh_port` 忽略）
async fn seed_vps(t: &T, team: Uuid, number: i32, ip: &str, ssh_port: i32, lifecycle: &str, host: Option<Uuid>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into machine (id, kind, team_id, number, primary_ip, ssh_port, lifecycle, host_id, activated_at, retired_at)
         values ($1, 'vps', $2, $3, $4::inet, $5, $6::machine_lifecycle, $7, now(), case when $6 = 'retired' then now() end)",
    )
    .bind(id)
    .bind(team)
    .bind(number)
    .bind(ip)
    .bind(host.is_none().then_some(ssh_port))
    .bind(lifecycle)
    .bind(host)
    .execute(&t.st.db)
    .await
    .unwrap();
    id
}

async fn seed_outbound(t: &T, machine: Uuid, ip: &str, port: i32) {
    sqlx::query("insert into node_outbound (machine_id, protocol, host, port, method, secret_enc) values ($1, 'shadowsocks', $2, $3, 'aes-256-gcm', $4)")
        .bind(machine)
        .bind(ip)
        .bind(port)
        .bind(t.st.secrets.seal_str("pw"))
        .execute(&t.st.db)
        .await
        .unwrap();
}

fn ip_item(ip: &str, host: Uuid, team: Uuid, relay: Uuid) -> Value {
    // 同机 IP 节点不用的字段也带上：SSH 端口、SS 端口、root 密码都要被忽略
    json!({"kind": "vps", "ip": ip, "host_id": host, "team_id": team, "relay_id": relay, "ssh_port": 22, "ss_port": 9999, "login": {"password": "root-pw"}})
}

fn errors(chk: &Value, i: usize) -> String { chk["items"][i]["errors"].as_array().unwrap().iter().map(|e| e.as_str().unwrap()).collect::<Vec<_>>().join("；") }

fn keys(v: &Value) -> Vec<String> { v["health"]["reasons"].as_array().unwrap().iter().map(|r| r["key"].as_str().unwrap().to_string()).collect() }

async fn lifecycle_of(t: &T, id: Uuid) -> String { t.machine_lifecycle(id).await }

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provision_checks_host_and_allocates_ports() {
    let t = setup(false).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let pv: Uuid = t.post("/api/v1/providers", json!({"name": "甲", "code": "A"})).await["id"].as_str().unwrap().parse().unwrap();
    let host = seed_vps(&t, team, 1, "203.0.113.5", 2222, "active", None).await;
    sqlx::query("update machine set provider_id = $2 where id = $1").bind(host).bind(pv).execute(&t.st.db).await.unwrap();

    // 一批两台：SS 端口从 8389 起依次分；SSH 端口与供应商取宿主机的、不带套餐，不收 root 密码
    let items = json!({"items": [ip_item("203.0.113.7", host, team, relay), ip_item("203.0.113.8", host, team, relay)]});
    let chk = t.post("/api/v1/provision/check", items.clone()).await;
    assert_eq!((chk["items"][0]["ss_port"].as_i64(), chk["items"][1]["ss_port"].as_i64()), (Some(8389), Some(8390)), "{chk}");
    let created = t.post("/api/v1/provision", items).await;
    let mut xs = vec![];
    for (j, port) in created["jobs"].as_array().unwrap().iter().zip([8389, 8390]) {
        let x: Uuid = j["machine_id"].as_str().unwrap().parse().unwrap();
        let row: (Option<Uuid>, Option<i32>, Option<Uuid>, Option<Uuid>, String) =
            sqlx::query_as("select host_id, ssh_port, provider_id, plan_id, kind::text from machine where id = $1").bind(x).fetch_one(&t.st.db).await.unwrap();
        assert_eq!(row, (Some(host), None, Some(pv), None, "vps".to_string()), "SSH 端口不抄宿主机的");
        assert_eq!(t.get(&format!("/api/v1/machines/{x}")).await["ssh_port"], 2222, "显示取宿主机的");
        let ob: (String, String, i32, Option<String>) =
            sqlx::query_as("select protocol, host, port, method from node_outbound where machine_id = $1").bind(x).fetch_one(&t.st.db).await.unwrap();
        assert_eq!((ob.0.as_str(), ob.2, ob.3.as_deref()), ("shadowsocks", port, Some("aes-256-gcm")));
        assert_eq!(ob.1, sqlx::query_scalar::<_, String>("select host(primary_ip) from machine where id = $1").bind(x).fetch_one(&t.st.db).await.unwrap());
        let job = t.job(j["job_id"].as_str().unwrap()).await;
        assert_eq!(job["kind"], "provision_ip");
        let steps: Vec<&str> = job["steps"].as_array().unwrap().iter().map(|s| s["key"].as_str().unwrap()).collect();
        assert_eq!(steps, ["ip_check", "converge", "attach", "wait_applied", "verify", "activate"]);
        let p: Value = sqlx::query_scalar("select params from job where id = $1::uuid").bind(j["job_id"].as_str().unwrap()).fetch_one(&t.st.db).await.unwrap();
        assert!(p.get("login").is_none() && p.get("reset_host_key").is_none(), "{p}");
        xs.push(x);
    }
    // 宿主机改了 SSH 端口：同机节点读到的跟着变（没有副本）
    sqlx::query("update machine set ssh_port = 2200 where id = $1").bind(host).execute(&t.st.db).await.unwrap();
    assert_eq!(t.get(&format!("/api/v1/machines/{}", xs[0])).await["ssh_port"], 2200);
    // 再加一台：接着库里已占的往上分
    let chk = t.post("/api/v1/provision/check", json!({"items": [ip_item("203.0.113.9", host, team, relay)]})).await;
    assert_eq!(chk["items"][0]["ss_port"], 8391, "{chk}");

    // 视图：宿主机列出同机节点，同机节点指回宿主机；精简列表带 host_id（fleetctl 解析 --host 用）
    let hv = t.get(&format!("/api/v1/machines/{host}")).await;
    let ids: Vec<&str> = hv["ip_nodes"].as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [xs[0].to_string(), xs[1].to_string()]);
    let xv = t.get(&format!("/api/v1/machines/{}", xs[0])).await;
    assert_eq!(xv["host_machine"], json!({"id": host, "name": "团队1-01"}));
    let brief = t.get("/api/v1/machines?view=brief").await;
    assert!(brief.as_array().unwrap().iter().any(|m| m["id"] == xs[0].to_string() && m["host_id"] == host.to_string()), "{brief}");

    // 宿主机校验：不存在、不是普通 VPS 节点（线路机 / 同机 IP 节点）、只有 VPS 节点能加、宿主机自己的 IP 不能再用
    let proxy = json!({"kind": "proxy", "ip": "203.0.113.23", "host_id": host, "team_id": team, "relay_id": relay,
                       "upstream": {"protocol": "socks5", "host": "p.example", "port": 1080}});
    let chk = t
        .post(
            "/api/v1/provision/check",
            json!({"items": [ip_item("203.0.113.20", Uuid::now_v7(), team, relay), ip_item("203.0.113.21", relay, team, relay),
                             ip_item("203.0.113.22", xs[0], team, relay), proxy, ip_item("203.0.113.5", host, team, relay)]}),
        )
        .await;
    for (i, want) in ["宿主机不存在", "不能当宿主机", "不能当宿主机", "只有 VPS 节点", "已经属于 团队1-01"].iter().enumerate() {
        assert!(errors(&chk, i).contains(want), "{i}: {}", errors(&chk, i));
    }
    // 宿主机不在用
    sqlx::query("update machine set lifecycle = 'retiring' where id = $1").bind(host).execute(&t.st.db).await.unwrap();
    let chk = t.post("/api/v1/provision/check", json!({"items": [ip_item("203.0.113.30", host, team, relay)]})).await;
    assert!(errors(&chk, 0).contains("只能加在运行中的 VPS 节点上"), "{chk}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ip_node_borrows_host_agent_and_metrics() {
    let t = setup(false).await;
    let (relay, tok) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let host = seed_vps(&t, team, 1, "203.0.113.5", 22, "active", None).await;
    let created = t.post("/api/v1/provision", json!({"items": [ip_item("203.0.113.7", host, team, relay)]})).await;
    let x: Uuid = created["jobs"][0]["machine_id"].as_str().unwrap().parse().unwrap();
    // 当作已经上架完：在用，有一条在用入口
    sqlx::query("update job set state = 'cancelled' where target_id = $1").bind(x).execute(&t.st.db).await.unwrap();
    sqlx::query("update machine set lifecycle = 'active', activated_at = now() where id = $1").bind(x).execute(&t.st.db).await.unwrap();
    let mut tx = t.st.db.begin().await.unwrap();
    fleet_server::entries::lock_policy_shared(&mut tx).await.unwrap();
    let (e, _, _) = fleet_server::entries::create(&mut tx, relay, x).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("update entry set lifecycle = 'active' where id = $1").bind(e).execute(&t.st.db).await.unwrap();
    // 只有宿主机有 agent 和主机样本：10 秒收 10000 字节 = 8000 bit/s
    sqlx::query("insert into agent_state (machine_id, agent_version, last_report_at) values ($1, 'test', now())").bind(host).execute(&t.st.db).await.unwrap();
    sqlx::query(
        "insert into host_metric (machine_id, ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bytes, net_tx_bytes, tcp_conns)
         values ($1, now() - interval '10 seconds', 5, 0.1, 100, 1000, 100, 1000, 1000, 0, 1), ($1, now(), 5, 0.1, 100, 1000, 100, 1000, 11000, 0, 1)",
    )
    .bind(host)
    .execute(&t.st.db)
    .await
    .unwrap();

    let xv = t.get(&format!("/api/v1/machines/{x}")).await;
    assert_eq!((xv["agent"]["online"].clone(), xv["agent"]["version"].clone()), (json!(true), json!("test")), "{xv}");
    assert_eq!(xv["host"]["net_rx_bps"], 8000.0, "{xv}");
    let pts = t.get(&format!("/api/v1/machines/{x}/series?metric=host&range=1h")).await["points"].as_array().unwrap().len();
    assert!(pts > 0, "主机曲线是宿主机的");
    // 总览：两台节点都在线，网卡速率只算一次
    let ov = t.get("/api/v1/overview").await;
    assert_eq!((ov["nodes"]["total"].as_i64(), ov["online"].as_i64(), ov["net_rx_bps"].as_f64()), (Some(2), Some(2), Some(8000.0)), "{ov}");

    // 线路机的期望状态：出站是同机节点自己的 IP 和 SS 端口，出口验收期望它自己的 IP
    let (s, v) = t.req("GET", "/agent/v1/desired?after=0", None, Some(&tok)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let ds: DesiredState = serde_json::from_value(v).unwrap();
    match &ds.entries[0].outbound {
        Outbound::Shadowsocks { host, port, .. } => assert_eq!((host.as_str(), *port), ("203.0.113.7", 8389)),
        o => panic!("{o:?}"),
    }
    assert_eq!(ds.entries[0].expect_egress_ip.to_string(), "203.0.113.7");

    // 宿主机 agent 离线：只有宿主机报 agent_offline，同机节点跟着显示离线但不重复报
    sqlx::query("update agent_state set last_report_at = now() - interval '10 minutes' where machine_id = $1").bind(host).execute(&t.st.db).await.unwrap();
    let hv = t.get(&format!("/api/v1/machines/{host}")).await;
    assert!(keys(&hv).contains(&"agent_offline".to_string()), "{hv}");
    let xv = t.get(&format!("/api/v1/machines/{x}")).await;
    assert_eq!(xv["agent"]["online"], false);
    assert!(!keys(&xv).contains(&"agent_offline".to_string()), "{xv}");

    // 宿主机维护：同机节点也在维护
    let (f, now) = (time::format_description::well_known::Rfc3339, time::OffsetDateTime::now_utc());
    t.post(
        "/api/v1/silences",
        json!({"subject_id": host, "is_maintenance": true, "starts_at": (now - time::Duration::minutes(1)).format(&f).unwrap(),
               "ends_at": (now + time::Duration::hours(1)).format(&f).unwrap()}),
    )
    .await;
    let xv = t.get(&format!("/api/v1/machines/{x}")).await;
    assert_eq!((xv["maintenance"].clone(), xv["health"]["status"].clone()), (json!(true), json!("maintenance")), "{xv}");

    // 流量额度：宿主机按自己的网卡计量（同机节点的流量也走这块网卡），同机节点没有自己的网卡读数，是它入口的业务流量
    sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, date_trunc('hour', now()), 100, 200)")
        .bind(e)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into host_metric_hourly (machine_id, hour, net_rx, net_tx) values ($1, date_trunc('hour', now()), 320, 310)")
        .bind(host)
        .execute(&t.st.db)
        .await
        .unwrap();
    t.st.live.rolled_up();
    let tr = &t;
    let q = |id: Uuid| async move { tr.get(&format!("/api/v1/machines/{id}")).await["quota"].clone() };
    assert_eq!((q(host).await["used"].as_i64(), q(host).await["rule"].as_str()), (Some(320), Some("max")));
    assert_eq!((q(x).await["used"].as_i64(), q(x).await["rule"].clone()), (Some(300), Value::Null));
    sqlx::query("insert into subscription (machine_id, price_cents, due_date, traffic_gb, traffic_reset_day) values ($1, 500, current_date + 60, 1, 1)")
        .bind(host)
        .execute(&t.st.db)
        .await
        .unwrap();
    assert_eq!(q(host).await["used"], 320);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unreachable_host_fails_ip_check_and_cancel_keeps_host() {
    let t = setup(true).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let host = seed_vps(&t, team, 1, "127.0.0.1", 1, "active", None).await;
    let kh = t.dir.path().join("known_hosts");
    std::fs::write(&kh, "[127.0.0.1]:1 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGWzfakefakefakefakefakefakefakefakefakefakefake\n").unwrap();
    let created = t.post("/api/v1/provision", json!({"items": [ip_item("203.0.113.7", host, team, relay)]})).await;
    let (x, job): (Uuid, String) = (created["jobs"][0]["machine_id"].as_str().unwrap().parse().unwrap(), created["jobs"][0]["job_id"].as_str().unwrap().into());

    // 宿主机的 SSH 连不上：第一步就报网络错误，同机节点标上架失败
    let j = t.wait_job(&job, "failed").await;
    assert_eq!((j["steps"][0]["key"].clone(), j["steps"][0]["error_kind"].clone()), (json!("ip_check"), json!("network")), "{j}");
    assert!(j["steps"][0]["error"].as_str().unwrap().contains("宿主机 团队1-01"), "{j}");
    assert_eq!(lifecycle_of(&t, x).await, "failed");
    // 同机节点没有自己的 agent；查 ssserver 日志登录的是宿主机、看它自己的实例
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{x}/upgrade-agent"), None, None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let logs = t.get(&format!("/api/v1/machines/{x}/logs?unit=ssserver")).await;
    assert_eq!((logs["unit"].clone(), logs["exit_code"].clone()), (json!("fleet-ssserver@8389"), json!(255)), "{logs}");
    assert!(logs["stderr"].as_str().unwrap().contains("port 1"), "{logs}");

    // 取消：同机节点退役、释放 IP 与编号；宿主机不受影响，主机公钥（宿主机的）还在
    t.post(&format!("/api/v1/jobs/{job}/cancel"), json!({})).await;
    t.wait_job(&job, "cancelled").await;
    assert_eq!((lifecycle_of(&t, x).await, lifecycle_of(&t, host).await), ("retired".to_string(), "active".to_string()));
    let ob: (i32, bool) = sqlx::query_as("select port, secret_enc is null from node_outbound where machine_id = $1").bind(x).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(ob, (8389, true), "出站参数留着、秘密清掉");
    assert!(std::fs::read_to_string(&kh).unwrap().contains("[127.0.0.1]:1 "), "宿主机的主机公钥不能被忘掉");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retiring_host_retires_its_ip_nodes() {
    let t = setup(true).await;
    let t1 = seed_team(&t, "团队1").await;
    let t2 = seed_team(&t, "团队2").await;
    let host = seed_vps(&t, t1, 1, "203.0.113.5", 22, "active", None).await;
    let x1 = seed_vps(&t, t1, 2, "203.0.113.7", 22, "active", Some(host)).await;
    let x2 = seed_vps(&t, t2, 1, "203.0.113.8", 22, "provisioning", Some(host)).await;
    let n3 = seed_node(&t, t1, 3, "203.0.113.10", "active").await;
    // X2 有一个还没跑的作业、X1 有一条未结告警
    let pending = Uuid::now_v7();
    sqlx::query("insert into job (id, kind, target_id, state, created_by, run_after) values ($1, 'provision_ip', $2, 'queued', 't', now() + interval '1 day')")
        .bind(pending)
        .bind(x2)
        .execute(&t.st.db)
        .await
        .unwrap();
    let inc = Uuid::now_v7();
    sqlx::query("insert into incident (id, rule, subject_id, severity, summary) values ($1, 'egress_fail', $2, 'critical', 's')").bind(inc).bind(x1).execute(&t.st.db).await.unwrap();

    // 预览：列出一并退役的同机节点；两个团队里剩下的节点按压缩前移
    let pv = t.get(&format!("/api/v1/machines/{host}/retire")).await;
    assert_eq!(pv["ip_nodes"], json!([{"id": x1, "name": "团队1-02"}, {"id": x2, "name": "团队2-01"}]), "{pv}");
    assert_eq!(pv["renames"], json!([{"machine_id": n3, "old_name": "团队1-03", "new_name": "团队1-01"}]), "{pv}");

    let r = t.post(&format!("/api/v1/machines/{host}/retire"), json!({"confirm": "团队1-01", "cleanup": false, "reason": "到期"})).await;
    let ij = r["ip_node_jobs"].as_array().unwrap();
    assert_eq!(ij.iter().map(|j| j["machine_id"].as_str().unwrap().to_string()).collect::<Vec<_>>(), [x1.to_string(), x2.to_string()], "{r}");
    for j in ij {
        let p: Value = sqlx::query_scalar("select params from job where id = $1::uuid").bind(j["job_id"].as_str().unwrap()).fetch_one(&t.st.db).await.unwrap();
        assert_eq!((p["cleanup"].clone(), t.job(j["job_id"].as_str().unwrap()).await["kind"].clone()), (json!(false), json!("retire")), "{p}");
    }
    let reason: Option<String> = sqlx::query_scalar("select lifecycle_reason from machine where id = $1").bind(x2).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(reason.as_deref(), Some("随宿主机「团队1-01」退役"));
    assert_eq!(t.job(&pending.to_string()).await["state"], "cancelled", "同机节点进行中的作业作废");
    let closed: (String, Value) = sqlx::query_as("select state::text, detail from incident where id = $1").bind(inc).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((closed.0.as_str(), &closed.1["closed_by"]), ("resolved", &json!("retire")));

    // 三个退役作业同时收尾（同团队的两台一起压缩编号）：都完成，剩下的节点前移
    for j in std::iter::once(&r["job_id"]).chain(ij.iter().map(|j| &j["job_id"])) {
        t.wait_job(j.as_str().unwrap(), "succeeded").await;
    }
    for m in [host, x1, x2] {
        assert_eq!(lifecycle_of(&t, m).await, "retired");
    }
    assert_eq!(t.get(&format!("/api/v1/machines/{n3}")).await["name"], "团队1-01");

    // 彻底删除：宿主机要等同机节点（含已退役的）先删
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{host}/purge"), Some(json!({"confirm": "团队1-01"})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("同机 IP 节点"), "{v}");
    for x in [x1, x2] {
        let name = t.get(&format!("/api/v1/machines/{x}")).await["name"].as_str().unwrap().to_string();
        let j = t.post(&format!("/api/v1/machines/{x}/purge"), json!({"confirm": name})).await;
        t.wait_job(j["job_id"].as_str().unwrap(), "succeeded").await;
    }
    let j = t.post(&format!("/api/v1/machines/{host}/purge"), json!({"confirm": "团队1-01"})).await;
    t.wait_job(j["job_id"].as_str().unwrap(), "succeeded").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_ip_node_needs_active_host() {
    let t = setup(false).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let host = seed_vps(&t, team, 1, "203.0.113.5", 22, "retired", None).await;
    let x = seed_vps(&t, team, 2, "203.0.113.7", 22, "retired", Some(host)).await;
    let path = format!("/api/v1/machines/{x}/restore");

    let pv = t.get(&path).await;
    assert_eq!(pv["restorable"], false, "{pv}");
    assert!(pv["blockers"].to_string().contains("宿主机「团队1-01」现在是「已退役」"), "{pv}");
    let (s, v) = t.req("POST", &path, Some(json!({"relay_id": relay})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");

    // 宿主机在用、上面已有一台同机节点占着 x 原来的 8389：恢复时重新分到 8390；不试 SSH、不要密码
    sqlx::query("update machine set lifecycle = 'active', retired_at = null where id = $1").bind(host).execute(&t.st.db).await.unwrap();
    let other = seed_vps(&t, team, 3, "203.0.113.8", 22, "active", Some(host)).await;
    seed_outbound(&t, other, "203.0.113.8", 8389).await;
    seed_outbound(&t, x, "203.0.113.7", 8389).await;
    sqlx::query("update node_outbound set secret_enc = null where machine_id = $1").bind(x).execute(&t.st.db).await.unwrap();
    let pv = t.get(&path).await;
    assert_eq!((pv["restorable"].clone(), pv["ssh"].clone(), pv["needs"]["password"].clone()), (json!(true), Value::Null, json!("no")), "{pv}");
    let r = t.post(&path, json!({"relay_id": relay, "login": {"password": "pw"}, "ss_port": 9999})).await;
    let job = t.job(r["job_id"].as_str().unwrap()).await;
    assert_eq!(job["kind"], "restore_ip");
    assert_eq!(lifecycle_of(&t, x).await, "provisioning");
    let (h, port): (String, i32) = sqlx::query_as("select host, port from node_outbound where machine_id = $1").bind(x).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((h.as_str(), port), ("203.0.113.7", 8390));
    let p: Value = sqlx::query_scalar("select params from job where id = $1::uuid").bind(r["job_id"].as_str().unwrap()).fetch_one(&t.st.db).await.unwrap();
    assert!(p.get("login").is_none(), "{p}");

    // 原来的端口没被占：沿用
    let y = seed_vps(&t, team, 4, "203.0.113.9", 22, "retired", Some(host)).await;
    seed_outbound(&t, y, "203.0.113.9", 8395).await;
    sqlx::query("update node_outbound set secret_enc = null where machine_id = $1").bind(y).execute(&t.st.db).await.unwrap();
    assert_eq!(t.get(&format!("/api/v1/machines/{y}/restore")).await["outbound"]["port"], 8395);
    t.post(&format!("/api/v1/machines/{y}/restore"), json!({"relay_id": relay})).await;
    let (port, gone): (i32, bool) = sqlx::query_as("select port, secret_enc is null from node_outbound where machine_id = $1").bind(y).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((port, gone), (8395, false), "沿用原端口、重新生成秘密");
}
