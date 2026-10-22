//! 端到端：上架（上游代理节点）→ 线路机应用 → 出口验收 → 上线 → 改名同步 → 退役（摘入口、压缩编号）→ 彻底删除。
//! 需要 PostgreSQL（见 common）。

use crate::common;

use axum::http::StatusCode;
use common::{seed_relay, setup, T};
use fleet_proto::{ApplyReport, DesiredState, EntryApply, ProbeKind, ProbeSample, Report};
use serde_json::{json, Value};
use std::time::Duration;

fn now_ms() -> i64 { (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64 }

/// 模拟线路机 agent：拉期望状态、回报全部成功，并给每个入口报一个成功的出口探测
async fn relay_round(t: &T, tok: &str, _after: u64, seq: &mut u64) -> DesiredState {
    let (s, v) = t.req("GET", "/agent/v1/desired?after=0", None, Some(tok)).await;
    assert_eq!(s, StatusCode::OK, "desired: {v}");
    let ds: DesiredState = serde_json::from_value(v).unwrap();
    let rep = ApplyReport {
        generation: ds.generation,
        ok: true,
        error: None,
        entries: ds.entries.iter().map(|e| EntryApply { id: e.id, ok: true, error: None }).collect(),
    };
    let (s, _) = t.req("POST", "/agent/v1/applied", Some(serde_json::to_value(&rep).unwrap()), Some(tok)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    *seq += 1;
    let r = Report {
        seq: *seq,
        boot_id: "b1".into(),
        agent_version: "test".into(),
        sent_at_ms: now_ms(),
        info: None,
        host: vec![],
        entries: vec![],
        probes: ds
            .entries
            .iter()
            .map(|e| ProbeSample { entry_id: e.id, ts_ms: now_ms(), kind: ProbeKind::Egress, ok: true, latency_ms: Some(12.0), detail: Some(e.expect_egress_ip.to_string()), retrans: None })
            .collect(),
        access: vec![],
        xray: None,
        agent_health: None,
    };
    let (s, v) = t.req("POST", "/agent/v1/report", Some(serde_json::to_value(&r).unwrap()), Some(tok)).await;
    assert_eq!(s, StatusCode::OK, "report: {v}");
    ds
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_lifecycle() {
    let t = setup(true).await;
    let (relay, rtok) = seed_relay(&t).await;
    let team = t.post("/api/v1/teams", json!({"name": "团队1"})).await["id"].as_str().unwrap().to_string();
    let mut seq = 0u64;

    // 上架三台上游代理节点（编号自动 01、02、03）
    let items: Vec<Value> = (0..3)
        .map(|i| json!({"kind": "proxy", "ip": format!("203.0.113.{}", 10 + i), "team_id": team, "relay_id": relay,
                        "upstream": {"protocol": "socks5", "host": format!("p{i}.example"), "port": 1080, "username": "u", "password": "p"}}))
        .collect();
    let chk = t.post("/api/v1/provision/check", json!({"items": items})).await;
    let names: Vec<&str> = chk["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["团队1-01", "团队1-02", "团队1-03"]);
    // 同一个 IP 重复 → 校验失败
    let (s, v) = t.req("POST", "/api/v1/provision", Some(json!({"items": [items[0].clone(), items[0].clone()]})), None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");

    let created = t.post("/api/v1/provision", json!({"items": items})).await;
    let jobs: Vec<String> = created["jobs"].as_array().unwrap().iter().map(|j| j["job_id"].as_str().unwrap().to_string()).collect();
    let ids: Vec<String> = created["jobs"].as_array().unwrap().iter().map(|j| j["machine_id"].as_str().unwrap().to_string()).collect();

    // 作业建好入口后，线路机拉到期望状态并回报；之后出口探测通过 → 上线
    let mut gen = 1;
    for _ in 0..150 {
        let ds = relay_round(&t, &rtok, gen, &mut seq).await;
        gen = ds.generation;
        let done = futures::future::join_all(jobs.iter().map(|j| t.job(j))).await;
        if done.iter().all(|j| j["state"] == "succeeded") {
            break;
        }
        t.nudge_jobs().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for j in &jobs {
        t.wait_job(j, "succeeded").await;
    }
    let ms = t.get("/api/v1/machines").await;
    let nodes: Vec<&Value> = ms.as_array().unwrap().iter().filter(|m| m["kind"] == "proxy").collect();
    assert_eq!(nodes.len(), 3);
    assert!(nodes.iter().all(|m| m["lifecycle"] == "active"));
    assert_eq!(nodes[0]["name"], "团队1-01");
    let mut ports: Vec<i64> = nodes.iter().map(|m| m["entries"][0]["port"].as_i64().unwrap()).collect();
    ports.sort();
    assert_eq!(ports, [20000, 20001, 20002]);

    // 改名：团队1 → 一队；线路机备注随之同步（代数 +1，期望状态里的 remark 变化）
    t.req("PATCH", &format!("/api/v1/teams/{team}"), Some(json!({"name": "一队"})), None).await;
    let ds = relay_round(&t, &rtok, gen, &mut seq).await;
    assert!(ds.generation > gen);
    gen = ds.generation;
    let mut rm: Vec<&str> = ds.entries.iter().map(|e| e.remark.as_str()).collect();
    rm.sort();
    assert_eq!(rm, ["一队-01", "一队-02", "一队-03"]);

    // 立即回收 一队-01：预览显示后面两台前移
    let pv = t.get(&format!("/api/v1/machines/{}/retire", ids[0])).await;
    assert_eq!(pv["renames"].as_array().unwrap().len(), 2);
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{}/retire", ids[0]), Some(json!({"now": true, "confirm": "错的名字"})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let job = t.post(&format!("/api/v1/machines/{}/retire", ids[0]), json!({"now": true, "confirm": "一队-01"})).await["job_id"].as_str().unwrap().to_string();
    for _ in 0..150 {
        let ds = relay_round(&t, &rtok, gen, &mut seq).await;
        gen = ds.generation;
        if t.job(&job).await["state"] == "succeeded" {
            break;
        }
        t.nudge_jobs().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    t.wait_job(&job, "succeeded").await;
    // 期望状态里不再有它的入口；剩下两台编号压缩成 01、02，备注同步
    let ds = relay_round(&t, &rtok, 0, &mut seq).await;
    assert_eq!(ds.entries.len(), 2);
    let mut rm: Vec<&str> = ds.entries.iter().map(|e| e.remark.as_str()).collect();
    rm.sort();
    assert_eq!(rm, ["一队-01", "一队-02"]);
    let live = t.get("/api/v1/machines").await;
    assert!(live.as_array().unwrap().iter().all(|m| m["id"] != ids[0].as_str()), "退役的不在默认列表");
    // 退役作业收尾时进了回收站：所有视图里都看不到，回收站里有它
    let (s, _) = t.req("GET", &format!("/api/v1/machines/{}", ids[0]), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "回收站里的机器");
    let bin = t.get("/api/v1/recycle-bin").await;
    assert_eq!((bin["total"].as_i64(), bin["items"][0]["name"].as_str()), (Some(1), Some("一队-01")), "{bin}");
    // 精简列表（fleetctl 按名字找机器用）：顺序和这几个字段与完整视图一致
    let (brief, full) = (t.get("/api/v1/machines?scope=all&view=brief").await, t.get("/api/v1/machines?scope=all").await);
    assert_eq!(brief.as_array().unwrap().len(), full.as_array().unwrap().len());
    for (b, f) in brief.as_array().unwrap().iter().zip(full.as_array().unwrap()) {
        for k in ["id", "kind", "name", "label", "ip", "lifecycle", "team"] {
            assert_eq!(b[k], f[k], "{k}：{b}");
        }
    }
    // 新上架接在末尾：一队-03
    let chk = t.post("/api/v1/provision/check", json!({"items": [{"kind": "proxy", "ip": "203.0.113.10", "team_id": team, "relay_id": relay,
        "upstream": {"protocol": "socks5", "host": "x", "port": 1}}]})).await;
    assert_eq!(chk["items"][0]["name"], "一队-03", "退役机器的 IP 已释放、编号接在末尾");
    // 物理删除由维护任务在 purge_blocker 放行后自动做（tests/it/spare.rs）
    // 审计里有重编号记录
    let audit = t.get("/api/v1/audit?limit=200").await;
    assert!(audit.as_array().unwrap().iter().any(|a| a["action"] == "team.renumber"));
}
