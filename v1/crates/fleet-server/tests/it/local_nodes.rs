//! 本机节点：节点就在线路机上（`host_id` = 线路机，`relay_local`），从线路机自己的 IP 出去——团队直接用机房 IP 的做法。
//! 需要 PostgreSQL（见 common）。线路机用 `Sim` 模拟；本机节点不登录任何机器，整条上架、退役流程都能在测试里走通。

use crate::common;

use axum::http::StatusCode;
use common::{seed_node, seed_relay, seed_relay_b, seed_team, setup, Sim, T};
use fleet_proto::Outbound;
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

/// 线路机「线路机2」的 IP（common::seed_relay）
const RELAY_IP: &str = "198.51.100.2";

fn errors(chk: &Value, i: usize) -> String { chk["items"][i]["errors"].as_array().unwrap().iter().map(|e| e.as_str().unwrap()).collect::<Vec<_>>().join("；") }

/// 一边模拟线路机一边等作业到某个状态
async fn drive(t: &T, sim: &mut Sim, job: &str, want: &str) -> Value {
    for _ in 0..300 {
        sim.round(t).await;
        let j = t.job(job).await;
        if j["state"] == want {
            return j;
        }
        if matches!(j["state"].as_str(), Some("failed" | "cancelled" | "succeeded")) {
            let logs = t.get(&format!("/api/v1/jobs/{job}/logs")).await;
            panic!("作业 {job} 想要 {want}，实际 {}：{} / {logs}", j["state"], j["error"]);
        }
        t.nudge_jobs().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("作业 {job} 没到 {want}：{}", t.job(job).await);
}

/// 在线路机上上架一台本机节点并跑到上线，返回机器 id
async fn provision_local(t: &T, sim: &mut Sim, relay: Uuid, team: Uuid) -> Uuid {
    let created = t.post("/api/v1/provision", json!({"items": [{"kind": "vps", "host_id": relay, "team_id": team}]})).await;
    let j = &created["jobs"][0];
    drive(t, sim, j["job_id"].as_str().unwrap(), "succeeded").await;
    j["machine_id"].as_str().unwrap().parse().unwrap()
}

/// 加在在用的线路机上：不填 IP、不选线路机；不登录、不装东西，入口从线路机直接出去，按节点归团队
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_node_on_relay_end_to_end() {
    let t = setup(true).await;
    let (relay, tok) = seed_relay(&t).await;
    let mut sim = Sim::new(tok);
    let (meng, chen) = (seed_team(&t, "MENG").await, seed_team(&t, "CHEN").await);
    let pv: Uuid = t.post("/api/v1/providers", json!({"name": "甲", "code": "A"})).await["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("update machine set provider_id = $2 where id = $1").bind(relay).bind(pv).execute(&t.st.db).await.unwrap();

    let item = json!({"kind": "vps", "host_id": relay, "team_id": meng});
    let chk = t.post("/api/v1/provision/check", json!({"items": [item.clone()]})).await;
    assert_eq!(errors(&chk, 0), "", "{chk}");
    assert_eq!((chk["items"][0]["name"].as_str(), chk["items"][0]["local"].as_bool(), chk["items"][0]["entry_mode"].as_str()), (Some("MENG-01"), Some(true), Some("normal")), "{chk}");
    assert_eq!(chk["items"][0]["warnings"], json!([]), "本机节点按普通建不用提醒：{chk}");

    let created = t.post("/api/v1/provision", json!({"items": [item]})).await;
    let j = &created["jobs"][0];
    let x: Uuid = j["machine_id"].as_str().unwrap().parse().unwrap();
    let job = t.job(j["job_id"].as_str().unwrap()).await;
    assert_eq!(job["kind"], "provision_local");
    let steps: Vec<&str> = job["steps"].as_array().unwrap().iter().map(|s| s["key"].as_str().unwrap()).collect();
    assert_eq!(steps, ["wait_host", "attach", "wait_applied", "verify", "activate"]);
    // 库里：VPS 节点、宿主机是线路机、IP 同线路机、供应商取线路机的、不带套餐、没有 SSH 端口和出站参数
    let row: (String, Option<Uuid>, bool, String) =
        sqlx::query_as("select kind::text, host_id, relay_local, host(primary_ip) from machine where id = $1").bind(x).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(row, ("vps".to_string(), Some(relay), true, RELAY_IP.to_string()));
    let row: (Option<i32>, Option<Uuid>, Option<Uuid>) = sqlx::query_as("select ssh_port, provider_id, plan_id from machine where id = $1").bind(x).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(row, (None, Some(pv), None));
    assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from node_outbound where machine_id = $1").bind(x).fetch_one(&t.st.db).await.unwrap(), 0);

    drive(&t, &mut sim, j["job_id"].as_str().unwrap(), "succeeded").await;
    assert_eq!(t.machine_lifecycle(x).await, "active");

    // 视图：节点指回线路机，线路机列出它；入口是「本机」而不是线路机自己的直出，名字是节点名
    let xv = t.get(&format!("/api/v1/machines/{x}")).await;
    assert_eq!((xv["name"].as_str(), xv["relay_local"].as_bool(), xv["ip"].as_str()), (Some("MENG-01"), Some(true), Some(RELAY_IP)), "{xv}");
    assert_eq!(xv["host_machine"], json!({"id": relay, "name": "线路机2"}));
    let e = &xv["entries"][0];
    assert_eq!((e["local"].as_bool(), e["direct"].as_bool(), e["mode"].as_str(), e["node"]["name"].as_str()), (Some(true), Some(false), Some("normal"), Some("MENG-01")), "{e}");
    assert_eq!(xv["health"]["status"], "ok", "没有 TCP 探测不算判不出：{xv}");
    let rv = t.get(&format!("/api/v1/machines/{relay}")).await;
    assert_eq!(rv["ip_nodes"], json!([{"id": x, "name": "MENG-01"}]), "{rv}");

    // 期望状态：出站是 freedom（同直出），备注是节点名，出口验收期望线路机的 IP，访问策略按节点 / 团队走
    let ds = sim.round(&t).await;
    assert_eq!(ds.entries.len(), 1);
    assert_eq!(ds.entries[0].outbound, Outbound::Direct);
    assert_eq!((ds.entries[0].remark.as_str(), ds.entries[0].expect_egress_ip.to_string()), ("MENG-01", RELAY_IP.to_string()));

    // 同一台线路机上别的团队也能有一台（同一个 IP，各自的入口与凭据），线路机自己的直出入口也照常能加
    let y = provision_local(&t, &mut sim, relay, chen).await;
    assert_eq!(t.get(&format!("/api/v1/machines/{y}")).await["name"], "CHEN-01");
    let r = t.post(&format!("/api/v1/machines/{relay}/entries"), json!({"relay_id": relay})).await;
    drive(&t, &mut sim, r["job_id"].as_str().unwrap(), "succeeded").await;
    let ds = sim.round(&t).await;
    let mut remarks: Vec<&str> = ds.entries.iter().map(|e| e.remark.as_str()).collect();
    remarks.sort();
    assert_eq!(remarks, ["CHEN-01", "MENG-01", "线路机2-直出"]);
    assert!(ds.entries.iter().all(|e| e.outbound == Outbound::Direct));
    // 同团队再加一台：编号接着排
    let chk = t.post("/api/v1/provision/check", json!({"items": [{"kind": "vps", "host_id": relay, "team_id": meng}]})).await;
    assert_eq!(chk["items"][0]["name"], "MENG-02", "{chk}");

    // 计费按节点自动记：节点进团队就是一个计费节点
    let n: i64 = sqlx::query_scalar("select count(*) from machine m join team t on t.id = m.team_id where m.kind in ('vps', 'proxy') and m.lifecycle = 'active' and t.name = 'MENG'")
        .fetch_one(&t.st.db)
        .await
        .unwrap();
    assert_eq!(n, 1);
    // 团队列表的在用节点数
    let teams = t.get("/api/v1/teams").await;
    let m = teams.as_array().unwrap().iter().find(|x| x["name"] == "MENG").unwrap();
    assert_eq!(m["node_count"], 1, "{teams}");
}

/// 校验与限制：IP 只能是线路机的、只能挂在所在线路机上；不能换线路机、不能穿透、不能在别的线路机上加入口；
/// 线路机的 IP 对别的机器照旧查重
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_node_checks_and_limits() {
    let t = setup(true).await;
    let (relay, tok) = seed_relay(&t).await;
    let (relay_b, _) = seed_relay_b(&t).await;
    let mut sim = Sim::new(tok);
    let team = seed_team(&t, "MENG").await;
    let proxy = seed_node(&t, team, 7, "203.0.113.70", "active").await;

    let chk = t
        .post(
            "/api/v1/provision/check",
            json!({"items": [
                // 填了别的 IP
                {"kind": "vps", "host_id": relay, "team_id": team, "ip": "203.0.113.9"},
                // 选了别的线路机
                {"kind": "vps", "host_id": relay, "team_id": team, "relay_id": relay_b},
                // 填线路机自己的 IP、选它自己：可以
                {"kind": "vps", "host_id": relay, "team_id": team, "ip": RELAY_IP, "relay_id": relay},
                // 家宽代理不能当宿主机
                {"kind": "vps", "host_id": proxy, "team_id": team, "ip": "203.0.113.10"},
                // 普通 VPS 节点没选线路机：提示里写怎么让节点自己当线路机
                {"kind": "vps", "team_id": team, "ip": "203.0.113.11"},
                // 线路机的 IP 对别的机器照旧查重
                {"kind": "vps", "team_id": team, "ip": RELAY_IP, "relay_id": relay},
            ]}),
        )
        .await;
    for (i, want) in ["从线路机「线路机2」自己的 IP", "只能挂在它所在的线路机「线路机2」上", "", "不能当宿主机", "按线路机上架并选团队", "已经属于 线路机2"].iter().enumerate() {
        let got = errors(&chk, i);
        assert!(if want.is_empty() { got.is_empty() } else { got.contains(want) }, "{i}: {got}");
    }
    // 宿主线路机不在用
    sqlx::query("update machine set lifecycle = 'retiring' where id = $1").bind(relay_b).execute(&t.st.db).await.unwrap();
    let chk = t.post("/api/v1/provision/check", json!({"items": [{"kind": "vps", "host_id": relay_b, "team_id": team}]})).await;
    assert!(errors(&chk, 0).contains("本机节点只能加在运行中的线路机上"), "{chk}");
    sqlx::query("update machine set lifecycle = 'active' where id = $1").bind(relay_b).execute(&t.st.db).await.unwrap();

    let x = provision_local(&t, &mut sim, relay, team).await;
    let eid: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(x).fetch_one(&t.st.db).await.unwrap();

    // 换线路机：预览与发起都拒绝，原因写明是本机节点
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{x}/move"), Some(json!({"to_relay_id": relay_b})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("本机节点"), "{v}");
    // 搬空它所在的线路机：它留下（跳过并写原因），不建作业
    let pv = t.get(&format!("/api/v1/relays/{relay}/evacuate?to_relay_id={relay_b}")).await;
    assert_eq!(pv["jobs"], json!([]), "{pv}");
    assert!(pv["skipped"][0]["reason"].as_str().unwrap().contains("本机节点"), "{pv}");
    // 在别的线路机上加入口
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{x}/entries"), Some(json!({"relay_id": relay_b})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("只能在它所在的线路机上有入口"), "{v}");
    // 穿透：预览给出原因，发起被拒
    let pv = t.get(&format!("/api/v1/entries/{eid}/reverse?on=true")).await;
    assert!(pv["blockers"].as_array().unwrap().iter().any(|b| b.as_str().unwrap().contains("用不着穿透")), "{pv}");
    let (s, _) = t.req("POST", &format!("/api/v1/entries/{eid}/reverse"), Some(json!({"on": true})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // 没有自己的机器可操作的：调优、升级 agent、下发 SS 都指向宿主线路机 / 说明没有
    for (path, want) in [("tune", "线路机2"), ("upgrade-agent", "线路机2"), ("ss-refresh", "不经 ssserver")] {
        let (s, v) = t.req("POST", &format!("/api/v1/machines/{x}/{path}"), Some(json!({})), None).await;
        assert_eq!(s, StatusCode::CONFLICT, "{path}: {v}");
        assert!(v["error"]["message"].as_str().unwrap().contains(want), "{path}: {v}");
    }
    // 放置矩阵不测它（IP 就是线路机自己）
    let targets = fleet_server::placement::targets(&t.st.db).await.unwrap();
    assert!(targets.iter().all(|r| r.node_id != x), "本机节点不进放置矩阵");
    // 能换团队（只是归属）：名字跟着变，入口备注随期望状态同步
    let other = seed_team(&t, "YANG").await;
    let rev = t.get(&format!("/api/v1/machines/{x}")).await["revision"].as_i64().unwrap();
    let (s, v) = t.req("PATCH", &format!("/api/v1/machines/{x}"), Some(json!({"revision": rev, "team_id": other})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["name"], "YANG-01", "{v}");
    assert_eq!(sim.round(&t).await.entries[0].remark, "YANG-01");
}

/// 线路机带团队上架：同时建线路机和它的本机节点；节点的作业等线路机上线后才建入口。
/// 线路机的上架要 SSH，这里把它直接改成在用（等同上架完成），只看节点这一半
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_with_team_brings_its_local_node() {
    let t = setup(true).await;
    let team = seed_team(&t, "MENG").await;
    let item = json!({"kind": "relay", "ip": "198.51.100.9", "label": "线路机9", "team_id": team, "country": "RU"});
    let chk = t.post("/api/v1/provision/check", json!({"items": [item.clone()]})).await;
    // SSH 探测连不上测试地址是另一回事：只看库里的校验结果与名字
    assert_eq!((chk["items"][0]["name"].as_str(), chk["items"][0]["node"].as_str()), (Some("线路机9"), Some("MENG-01")), "{chk}");
    // 线路机不带团队时给编号没有意义
    let bad = t.post("/api/v1/provision/check", json!({"items": [{"kind": "relay", "ip": "198.51.100.10", "label": "线路机10", "number": 3}]})).await;
    assert!(errors(&bad, 0).contains("线路机没有编号"), "{bad}");

    let created = t.post("/api/v1/provision", json!({"items": [item]})).await;
    let jobs = created["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2, "{created}");
    let (relay, relay_job): (Uuid, &str) = (jobs[0]["machine_id"].as_str().unwrap().parse().unwrap(), jobs[0]["job_id"].as_str().unwrap());
    let (node, node_job): (Uuid, &str) = (jobs[1]["machine_id"].as_str().unwrap().parse().unwrap(), jobs[1]["job_id"].as_str().unwrap());
    assert_eq!((jobs[0]["name"].as_str(), jobs[1]["name"].as_str()), (Some("线路机9"), Some("MENG-01")));
    assert_eq!((t.job(relay_job).await["kind"].clone(), t.job(node_job).await["kind"].clone()), (json!("provision_relay"), json!("provision_local")));
    let row: (Option<Uuid>, bool, String, Option<String>, Option<Uuid>, String) =
        sqlx::query_as("select host_id, relay_local, host(primary_ip), country, team_id, lifecycle::text from machine where id = $1").bind(node).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(row, (Some(relay), true, "198.51.100.9".to_string(), Some("RU".to_string()), Some(team), "provisioning".to_string()));
    // 线路机自己不进团队
    assert_eq!(sqlx::query_scalar::<_, Option<Uuid>>("select team_id from machine where id = $1").bind(relay).fetch_one(&t.st.db).await.unwrap(), None);

    // 线路机还在上架：节点的作业停在第一步等它
    let j = t.wait_job(node_job, "waiting").await;
    assert_eq!(j["steps"][0]["key"], "wait_host");
    assert!(j["steps"][1]["state"] != "done", "{j}");
    assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from entry where node_id = $1").bind(node).fetch_one(&t.st.db).await.unwrap(), 0);

    // 线路机上架完成（这里直接改库：停掉它的作业、置为在用、签一个 agent 令牌）
    sqlx::query("update job set state = 'cancelled', lease_owner = null where id = $1::uuid").bind(relay_job).execute(&t.st.db).await.unwrap();
    sqlx::query("update machine set lifecycle = 'active', activated_at = now() where id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    let tok = "fa1_test_relay9_token";
    sqlx::query("insert into agent_token (id, machine_id, token_hash) values ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(relay)
        .bind(fleet_server::crypto::sha256(tok.as_bytes()))
        .execute(&t.st.db)
        .await
        .unwrap();
    let mut sim = Sim::new(tok.to_string());
    drive(&t, &mut sim, node_job, "succeeded").await;
    assert_eq!(t.machine_lifecycle(node).await, "active");
    let ds = sim.round(&t).await;
    assert_eq!((ds.entries.len(), ds.entries[0].remark.as_str(), &ds.entries[0].outbound), (1, "MENG-01", &Outbound::Direct));
}

/// 线路机没上架成（取消 / 退役了）：等它的本机节点作业失败并写明原因，不一直等
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_node_job_fails_when_relay_is_gone() {
    let t = setup(true).await;
    let team = seed_team(&t, "MENG").await;
    let created = t.post("/api/v1/provision", json!({"items": [{"kind": "relay", "ip": "198.51.100.9", "label": "线路机9", "team_id": team}]})).await;
    let jobs = created["jobs"].as_array().unwrap();
    let (relay, relay_job): (Uuid, &str) = (jobs[0]["machine_id"].as_str().unwrap().parse().unwrap(), jobs[0]["job_id"].as_str().unwrap());
    let node_job = jobs[1]["job_id"].as_str().unwrap();
    t.wait_job(node_job, "waiting").await;
    sqlx::query("update job set state = 'cancelled', lease_owner = null where id = $1::uuid").bind(relay_job).execute(&t.st.db).await.unwrap();
    sqlx::query("update machine set lifecycle = 'retired', retired_at = now() where id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    let j = t.wait_job(node_job, "failed").await;
    assert!(j["error"].as_str().unwrap().contains("本机节点没有地方放了"), "{j}");
}

/// 退役：单独回收本机节点只摘它的入口（线路机上没有它的服务可清理），线路机照常；
/// 回收线路机时它上面的本机节点一并回收
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retiring_local_node_and_its_relay() {
    let t = setup(true).await;
    let (relay, tok) = seed_relay(&t).await;
    let mut sim = Sim::new(tok);
    let team = seed_team(&t, "MENG").await;
    let (a, b) = (provision_local(&t, &mut sim, relay, team).await, provision_local(&t, &mut sim, relay, team).await);

    // 单独回收一台：不登录线路机（清理一步跳过）
    let r = t.post(&format!("/api/v1/machines/{a}/retire"), json!({"now": true, "confirm": "MENG-01", "reason": "不用了"})).await;
    let job = r["job_id"].as_str().unwrap();
    let j = drive(&t, &mut sim, job, "succeeded").await;
    let cleanup = j["steps"].as_array().unwrap().iter().find(|s| s["key"] == "cleanup").unwrap();
    assert_eq!(cleanup["state"], "skipped", "{cleanup}");
    assert_eq!((t.machine_lifecycle(a).await, t.machine_lifecycle(relay).await), ("retired".to_string(), "active".to_string()));
    // 剩下那台压缩成 01，入口还在
    let bv = t.get(&format!("/api/v1/machines/{b}")).await;
    assert_eq!((bv["name"].as_str(), bv["entries"].as_array().unwrap().len()), (Some("MENG-01"), 1), "{bv}");

    // 回收线路机：预览列出会一并回收的本机节点，发起后两台都退役
    let pv = t.get(&format!("/api/v1/machines/{relay}/retire")).await;
    assert!(pv["ip_nodes"].as_array().unwrap().iter().any(|x| x["id"] == b.to_string()), "{pv}");
    let r = t.post(&format!("/api/v1/machines/{relay}/retire"), json!({"now": true, "confirm": "线路机2", "cleanup": false, "reason": "到期"})).await;
    let mut ids: Vec<String> = vec![r["job_id"].as_str().unwrap().to_string()];
    ids.extend(r["ip_node_jobs"].as_array().unwrap().iter().map(|x| x["job_id"].as_str().unwrap().to_string()));
    assert_eq!(ids.len(), 2, "{r}");
    // 线路机的退役作业做到「撤销令牌」之后模拟的线路机就连不上了：令牌还在时才跑一轮，其余步骤不需要它
    for _ in 0..300 {
        let live: bool = sqlx::query_scalar("select exists (select 1 from agent_token where machine_id = $1 and revoked_at is null)").bind(relay).fetch_one(&t.st.db).await.unwrap();
        let left: i64 = sqlx::query_scalar("select count(*) from entry where relay_id = $1 and lifecycle <> 'removed'").bind(relay).fetch_one(&t.st.db).await.unwrap();
        if live && left > 0 {
            sim.round(&t).await;
        }
        let mut done = true;
        for id in &ids {
            done &= t.job(id).await["state"] == "succeeded";
        }
        if done {
            break;
        }
        t.nudge_jobs().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for id in &ids {
        assert_eq!(t.job(id).await["state"], "succeeded", "{}", t.job(id).await);
    }
    assert_eq!((t.machine_lifecycle(b).await, t.machine_lifecycle(relay).await), ("retired".to_string(), "retired".to_string()));
}
