//! 换线路机（move_entry）、搬空线路机（evacuate）、恢复已退役的机器（restore_*）。
//! 需要 PostgreSQL（见 common）。
//!
//! 线路机用 `Sim` 模拟：拉期望状态、回报全部应用成功、给每个入口报一个成功的出口探测（和 tests/flow.rs 一样）。
//! 节点用上游代理（没有 SSH，整条流程可以在测试里走通）；VPS 恢复只测到预检和作业创建，不真的 ssh。

mod common;

use axum::http::StatusCode;
use common::{seed_node, seed_relay, seed_team, setup, T};
use fleet_proto::{ApplyReport, DesiredState, EntryApply, Outbound, ProbeKind, ProbeSample, Report};
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

fn now_ms() -> i64 { (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64 }

struct Sim {
    tok: String,
    seq: u64,
}

impl Sim {
    fn new(tok: String) -> Self { Sim { tok, seq: 0 } }

    /// 一轮：拉期望状态 → 回报已应用 → 上报出口探测
    async fn round(&mut self, t: &T) -> DesiredState {
        let (s, v) = t.req("GET", "/agent/v1/desired?after=0", None, Some(&self.tok)).await;
        assert_eq!(s, StatusCode::OK, "desired: {v}");
        let ds: DesiredState = serde_json::from_value(v).unwrap();
        let rep = ApplyReport {
            generation: ds.generation,
            ok: true,
            error: None,
            entries: ds.entries.iter().map(|e| EntryApply { id: e.id, ok: true, error: None }).collect(),
        };
        let (s, _) = t.req("POST", "/agent/v1/applied", Some(serde_json::to_value(&rep).unwrap()), Some(&self.tok)).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        self.seq += 1;
        let r = Report {
            seq: self.seq,
            boot_id: "b1".into(),
            agent_version: "test".into(),
            sent_at_ms: now_ms(),
            info: None,
            host: vec![],
            entries: vec![],
            probes: ds
                .entries
                .iter()
                .map(|e| ProbeSample { entry_id: e.id, ts_ms: now_ms(), kind: ProbeKind::Egress, ok: true, latency_ms: Some(12.0), detail: Some(e.expect_egress_ip.to_string()) })
                .collect(),
            access: vec![],
            xray: None,
            agent_health: None,
        };
        let (s, v) = t.req("POST", "/agent/v1/report", Some(serde_json::to_value(&r).unwrap()), Some(&self.tok)).await;
        assert_eq!(s, StatusCode::OK, "report: {v}");
        ds
    }
}

/// 第二台线路机「线路机3」（198.51.100.3）
async fn seed_relay_b(t: &T) -> (Uuid, String) {
    let id = Uuid::now_v7();
    let (sk, pk) = fleet_server::crypto::reality_keypair();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle, activated_at) values ($1, 'relay', '线路机3', '198.51.100.3', 'active', now())")
        .bind(id)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into relay (machine_id, public_host, reality_private_enc, reality_public_key, min_client_ver, generation, applied_generation) values ($1, 'relay3.test', $2, $3, '1.8.2', 1, 1)")
        .bind(id)
        .bind(t.st.secrets.seal_str(&sk))
        .bind(pk)
        .execute(&t.st.db)
        .await
        .unwrap();
    let tok = "fa1_test_relay_token_b";
    sqlx::query("insert into agent_token (id, machine_id, token_hash) values ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(id)
        .bind(fleet_server::crypto::sha256(tok.as_bytes()))
        .execute(&t.st.db)
        .await
        .unwrap();
    (id, tok.to_string())
}

fn upstream(host: &str) -> Value { json!({"protocol": "socks5", "host": host, "port": 1080, "username": "u", "password": "p"}) }

/// 上架一台上游代理节点，返回 (machine_id, job_id)
async fn provision_proxy(t: &T, team: Uuid, relay: Uuid, ip: &str) -> (Uuid, String) { provision_proxies(t, team, relay, &[ip]).await.remove(0) }

/// 一次上架几台（编号按顺序），各自的作业并行跑
async fn provision_proxies(t: &T, team: Uuid, relay: Uuid, ips: &[&str]) -> Vec<(Uuid, String)> {
    let items: Vec<Value> = ips.iter().map(|ip| json!({"kind": "proxy", "ip": ip, "team_id": team, "relay_id": relay, "upstream": upstream("p.example")})).collect();
    let created = t.post("/api/v1/provision", json!({"items": items})).await;
    created["jobs"].as_array().unwrap().iter().map(|j| (j["machine_id"].as_str().unwrap().parse().unwrap(), j["job_id"].as_str().unwrap().to_string())).collect()
}

/// 一边模拟线路机一边等作业到某个状态
async fn drive(t: &T, sims: &mut [&mut Sim], job: &str, want: &str) -> Value {
    for _ in 0..300 {
        for s in sims.iter_mut() {
            s.round(t).await;
        }
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

/// 节点的入口（relay_id, lifecycle, id），按创建时间
async fn entries_of(t: &T, node: Uuid) -> Vec<(Uuid, String, Uuid)> {
    sqlx::query_as("select relay_id, lifecycle::text, id from entry where node_id = $1 order by created_at, id").bind(node).fetch_all(&t.st.db).await.unwrap()
}

async fn wait_until<F, Fut>(what: &str, f: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..150 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("等不到：{what}");
}

async fn log_msgs(t: &T, job: &str) -> Vec<String> {
    t.get(&format!("/api/v1/jobs/{job}/logs")).await.as_array().unwrap().iter().map(|l| l["msg"].as_str().unwrap().to_string()).collect()
}

async fn lifecycle_of(t: &T, id: Uuid) -> String { t.machine_lifecycle(id).await }

async fn name_of(t: &T, id: Uuid) -> String { t.get(&format!("/api/v1/machines/{id}")).await["name"].as_str().unwrap().to_string() }

async fn mv_req(t: &T, node: Uuid, to: Uuid) -> (StatusCode, Value) {
    t.req("POST", &format!("/api/v1/machines/{node}/move"), Some(json!({"to_relay_id": to})), None).await
}

// ── 换线路机 ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_proxy_node_never_loses_its_entry() {
    let t = setup(true).await;
    let (ra, ta) = seed_relay(&t).await;
    let (rb, tb) = seed_relay_b(&t).await;
    let (mut a, mut b) = (Sim::new(ta), Sim::new(tb));
    let team = seed_team(&t, "团队1").await;
    let (node, pj) = provision_proxy(&t, team, ra, "203.0.113.10").await;
    drive(&t, &mut [&mut a], &pj, "succeeded").await;
    let es = entries_of(&t, node).await;
    assert_eq!(es.len(), 1);
    assert_eq!((es[0].0, es[0].1.as_str()), (ra, "active"));
    let old_entry = es[0].2;

    let r = t.post(&format!("/api/v1/machines/{node}/move"), json!({"to_relay_id": rb})).await;
    assert_eq!(r["share_changed"], true);
    assert!(r["note"].as_str().unwrap().contains("分享链接"), "{r}");
    assert_eq!(r["from"], "线路机2");
    assert_eq!(r["to"], "线路机3");
    let job = r["job_id"].as_str().unwrap().to_string();

    // 节点已有进行中的作业：不能再发起第二个
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{node}/move"), Some(json!({"to_relay_id": rb})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // 新入口建在目标线路机上（pending），旧入口原封不动
    wait_until("新入口建好", || async { entries_of(&t, node).await.len() == 2 }).await;
    let es = entries_of(&t, node).await;
    assert_eq!((es[0].0, es[0].1.as_str()), (ra, "active"));
    assert_eq!((es[1].0, es[1].1.as_str()), (rb, "pending"));
    let new_entry = es[1].2;
    assert_ne!(new_entry, old_entry);

    // 只模拟目标线路机：新入口 applied + 出口探测通过 → 作业摘旧入口（旧线路机还没回报，所以停在 removing）
    let mut stage = None;
    for _ in 0..300 {
        b.round(&t).await;
        let es = entries_of(&t, node).await;
        assert!(es.iter().any(|e| e.1 == "active"), "换线期间节点至少有一个 active 入口：{es:?}");
        if es.iter().any(|e| e.2 == old_entry && e.1 == "removing") {
            stage = Some(es);
            break;
        }
        t.nudge_jobs().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let es = stage.expect("旧入口应当进入 removing");
    assert_eq!(es.iter().find(|e| e.2 == new_entry).unwrap().1, "active", "新入口已经生效");
    // 摘了旧入口之后不能再取消（取消会让节点没有入口）
    let (s, v) = t.req("POST", &format!("/api/v1/jobs/{job}/cancel"), None, None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(t.job(&job).await["cancellable"], false);

    // 旧线路机回报 → 旧入口 removed → 作业完成
    let j = drive(&t, &mut [&mut a, &mut b], &job, "succeeded").await;
    let es = entries_of(&t, node).await;
    assert_eq!(es.iter().find(|e| e.2 == old_entry).unwrap().1, "removed");
    assert_eq!((es.iter().find(|e| e.2 == new_entry).unwrap().0, es.iter().find(|e| e.2 == new_entry).unwrap().1.as_str()), (rb, "active"));
    // 作业里记着新入口 id，并提示分享链接变了
    assert_eq!(j["result"]["new_entry_id"], new_entry.to_string());
    assert_eq!(j["result"]["share_changed"], true);
    let keys: Vec<&str> = j["steps"].as_array().unwrap().iter().map(|s| s["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["attach_new", "firewall_widen", "wait_applied_new", "verify_new", "detach_old", "wait_detached_old", "firewall_narrow"]);
    assert_eq!(j["steps"][1]["state"], "skipped", "家宽代理没有防火墙");
    assert!(log_msgs(&t, &job).await.iter().any(|m| m.contains("新的分享链接")));
    // 新入口的分享链接指向新线路机
    let share = t.get(&format!("/api/v1/entries/{new_entry}/share")).await;
    assert!(share["url"].as_str().unwrap().contains("@relay3.test:"), "{share}");
    assert_eq!(lifecycle_of(&t, node).await, "active");
    // 审计
    let audit = t.get("/api/v1/audit?limit=100").await;
    assert!(audit.as_array().unwrap().iter().any(|x| x["action"] == "entry.move"));

    // 旧入口 removed 之后，它本月的流量仍算在节点和旧线路机上（不进入口列表），新线路机上不重复计
    for (e, up) in [(old_entry, 100i64), (new_entry, 7)] {
        sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, date_trunc('hour', now()), $2, 0)")
            .bind(e)
            .bind(up)
            .execute(&t.st.db)
            .await
            .unwrap();
    }
    t.st.live.rolled_up(); // 视图里的流量来自小时汇总，汇总跑完才刷新
    let m = t.get(&format!("/api/v1/machines/{node}")).await;
    assert_eq!(m["entries"].as_array().unwrap().len(), 1);
    assert_eq!((m["entries"][0]["traffic"]["month_up"].as_i64(), m["removed_traffic"]["month_up"].as_i64()), (Some(7), Some(100)));
    let rs = t.get("/api/v1/relays").await;
    let removed_of = |id: Uuid| rs.as_array().unwrap().iter().find(|r| r["id"] == id.to_string()).unwrap()["removed_traffic"]["month_up"].as_i64();
    assert_eq!((removed_of(ra), removed_of(rb)), (Some(100), Some(0)));
    let o = t.get("/api/v1/overview").await;
    assert_eq!(o["traffic_month"]["up"], 107, "总览 = 节点现有入口 + 已移除入口");

    // 流量重置日：按账单时区里最近一次重置日的零点起算（含已移除入口）。重置日取洛杉矶的今天 → 周期从洛杉矶今天零点开始
    let (today, day): (time::Date, i32) =
        sqlx::query_as("select (now() at time zone 'America/Los_Angeles')::date, extract(day from now() at time zone 'America/Los_Angeles')::int")
            .fetch_one(&t.st.db)
            .await
            .unwrap();
    // 周期起点前一小时的流量不算
    sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, ($2::timestamp at time zone 'America/Los_Angeles') - interval '1 hour', 1000, 0)")
        .bind(old_entry)
        .bind(today)
        .execute(&t.st.db)
        .await
        .unwrap();
    let (s, v) = t
        .req("PUT", &format!("/api/v1/machines/{node}/subscription"), Some(json!({"price_cents": 100, "due_date": "2030-01-01", "billing_tz": "America/Los_Angeles", "traffic_reset_day": day})), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    t.st.live.rolled_up();
    let m = t.get(&format!("/api/v1/machines/{node}")).await;
    assert_eq!(m["quota"]["period_start"], today.to_string());
    assert_eq!(m["quota"]["used"], 107, "周期内：新入口 7 + 已移除入口 100，起点前的 1000 不算：{}", m["quota"]);
    // 重置日越界：中文 400，不漏约束名
    for bad in [0, 45] {
        let (s, v) = t
            .req("PUT", &format!("/api/v1/machines/{node}/subscription"), Some(json!({"price_cents": 100, "due_date": "2030-01-01", "traffic_reset_day": bad})), None)
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains("1–31") && !msg.contains("check"), "{msg}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_tz_is_validated_on_write() {
    let t = setup(false).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let put = |tz: &'static str| {
        let path = format!("/api/v1/machines/{node}/subscription");
        let t = &t;
        async move { t.req("PUT", &path, Some(json!({"price_cents": 500, "due_date": "2027-01-01", "billing_tz": tz})), None).await }
    };
    t.get("/api/v1/machines").await; // 先让在用机器视图进缓存：写入后的事件要让它作废
    let (s, v) = put("Mars/Olympus").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("账单时区"), "{v}");
    assert_eq!(put("").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(put("America/Los_Angeles").await.0, StatusCode::OK);
    let m = t.get(&format!("/api/v1/machines/{node}")).await;
    assert_eq!(m["subscription"]["billing_tz"], "America/Los_Angeles");
    assert!(m["subscription"]["due_days"].is_i64());
    let list = t.get("/api/v1/machines").await;
    let in_list = list.as_array().unwrap().iter().find(|x| x["id"] == node.to_string()).unwrap();
    assert_eq!(in_list["subscription"]["billing_tz"], "America/Los_Angeles", "列表缓存已随事件作废");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_period_in_half_hour_tz_counts_start_bucket() {
    let t = setup(true).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
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
    let (today, day): (time::Date, i32) =
        sqlx::query_as("select (now() at time zone 'Asia/Kolkata')::date, extract(day from now() at time zone 'Asia/Kolkata')::int")
            .fetch_one(&t.st.db)
            .await
            .unwrap();
    // 加尔各答零点 = UTC xx:30：起点所在的那一桶（起点前 30 分钟开始）算，再前一桶不算
    for (back, up) in [("30 minutes", 50i64), ("90 minutes", 1000)] {
        sqlx::query("insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes) values ($1, ($2::timestamp at time zone 'Asia/Kolkata') - $3::interval, $4, 0)")
            .bind(eid)
            .bind(today)
            .bind(back)
            .bind(up)
            .execute(&t.st.db)
            .await
            .unwrap();
    }
    let (s, v) = t
        .req("PUT", &format!("/api/v1/machines/{node}/subscription"), Some(json!({"price_cents": 100, "due_date": "2030-01-01", "billing_tz": "Asia/Kolkata", "traffic_reset_day": day})), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let m = t.get(&format!("/api/v1/machines/{node}")).await;
    assert_eq!(m["quota"]["period_start"], today.to_string());
    assert_eq!(m["quota"]["used"], 50, "{}", m["quota"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_create_defaults_and_patch() {
    let t = setup(false).await;
    // 时区 null / 空白 = 列默认值 UTC（界面清空时区原先发 null 直接 400）
    let a = t.post("/api/v1/providers", json!({"name": "甲", "code": "A", "panel_url": null, "timezone": null})).await["id"].as_str().unwrap().to_string();
    t.post("/api/v1/providers", json!({"name": "乙", "code": "B", "timezone": " "})).await;
    let ps = t.get("/api/v1/providers").await;
    assert!(ps.as_array().unwrap().iter().all(|p| p["timezone"] == "UTC" && p["panel_url"].is_null()), "{ps}");
    let (s, v) = t.req("POST", "/api/v1/providers", Some(json!({"name": "甲", "code": "C"})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("已有同名供应商"), "{v}");

    let path = format!("/api/v1/providers/{a}");
    let get = || async { t.get("/api/v1/providers").await.as_array().unwrap().iter().find(|p| p["id"] == a.as_str()).unwrap().clone() };
    let (s, v) = t.req("PATCH", &path, Some(json!({"name": " 甲改 ", "panel_url": "https://panel.example", "timezone": "Asia/Shanghai"})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let p = get().await;
    assert_eq!((p["name"].as_str(), p["code"].as_str(), p["panel_url"].as_str(), p["timezone"].as_str()), (Some("甲改"), Some("A"), Some("https://panel.example"), Some("Asia/Shanghai")));
    // 校验：坏时区 400、缩写撞了 409、空名 400、不存在 404；失败不改任何字段
    let (s, v) = t.req("PATCH", &path, Some(json!({"name": "甲二", "timezone": "Mars/Olympus"})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = t.req("PATCH", &path, Some(json!({"code": "B"})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("已有同缩写的供应商"), "{v}");
    assert_eq!(t.req("PATCH", &path, Some(json!({"name": "  "})), None).await.0, StatusCode::BAD_REQUEST);
    // 控制台地址只能是 http(s)（网页上是链接）
    assert_eq!(t.req("PATCH", &path, Some(json!({"panel_url": "javascript:alert(1)"})), None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.req("POST", "/api/v1/providers", Some(json!({"name": "丙", "code": "D", "panel_url": "panel.example"})), None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.req("PATCH", &format!("/api/v1/providers/{}", Uuid::now_v7()), Some(json!({"name": "x"})), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(get().await["name"], "甲改");
    // 清空：panel_url null、时区空串 = 回到 UTC；没传的字段不动
    let (s, v) = t.req("PATCH", &path, Some(json!({"panel_url": null, "timezone": ""})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let p = get().await;
    assert_eq!((p["name"].as_str(), p["panel_url"].is_null(), p["timezone"].as_str()), (Some("甲改"), true, Some("UTC")));
    let audit = t.get("/api/v1/audit?limit=50").await;
    assert_eq!(audit.as_array().unwrap().iter().filter(|e| e["action"] == "provider.update" && e["title"] == "修改供应商").count(), 2, "{audit}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_tz_is_validated_like_billing_tz() {
    let t = setup(false).await;
    let (s, v) = t.req("POST", "/api/v1/providers", Some(json!({"name": "坏时区", "code": "BAD", "timezone": "Mars/Olympus"})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    // 缩写时区 at time zone 认（pg_timezone_names 里不一定有）：供应商和账单两处标准一致，上架时抄过去照样能用
    t.post("/api/v1/providers", json!({"name": "缩写时区", "code": "CST", "timezone": "CST"})).await;
    let tz = t.get("/api/v1/providers").await[0]["timezone"].as_str().unwrap().to_string();
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let created = t
        .post(
            "/api/v1/provision",
            json!({"items": [{"kind": "proxy", "ip": "203.0.113.10", "team_id": team, "relay_id": relay, "upstream": upstream("p.example"),
                              "subscription": {"price_cents": 500, "due_date": "2027-01-01", "billing_tz": tz}}]}),
        )
        .await;
    let node = created["jobs"][0]["machine_id"].as_str().unwrap();
    let m = t.get(&format!("/api/v1/machines/{node}")).await;
    assert_eq!(m["subscription"]["billing_tz"], "CST");
    assert!(m["subscription"]["due_days"].is_i64());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provision_subscription_defaults_from_plan_and_provider() {
    let t = setup(false).await;
    let pv = t.post("/api/v1/providers", json!({"name": "甲", "code": "A", "timezone": "America/Los_Angeles"})).await["id"].clone();
    let plan = t.post("/api/v1/plans", json!({"provider_id": pv, "name": "年付", "price_cents": 12000, "currency": "CNY", "cycle": "yearly"})).await["id"].clone();
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    // 只给到期日：价格 / 币种 / 周期取套餐，时区取套餐的供应商
    let item = |ip: &str, sub: Value| json!({"kind": "proxy", "ip": ip, "team_id": team, "relay_id": relay, "plan_id": plan, "upstream": upstream("p.example"), "subscription": sub});
    let created = t.post("/api/v1/provision", json!({"items": [item("203.0.113.10", json!({"due_date": "2027-01-01"}))]})).await;
    let node = created["jobs"][0]["machine_id"].as_str().unwrap();
    let s = t.get(&format!("/api/v1/machines/{node}")).await["subscription"].clone();
    assert_eq!((s["price_cents"].as_i64(), s["currency"].as_str(), s["cycle"].as_str(), s["billing_tz"].as_str()), (Some(12000), Some("CNY"), Some("yearly"), Some("America/Los_Angeles")), "{s}");
    // 显式给的优先
    let sub = json!({"due_date": "2027-01-01", "price_cents": 900, "currency": "USD", "cycle": "monthly", "billing_tz": "Asia/Tokyo"});
    let created = t.post("/api/v1/provision", json!({"items": [item("203.0.113.11", sub)]})).await;
    let node = created["jobs"][0]["machine_id"].as_str().unwrap();
    let s = t.get(&format!("/api/v1/machines/{node}")).await["subscription"].clone();
    assert_eq!((s["price_cents"].as_i64(), s["currency"].as_str(), s["cycle"].as_str(), s["billing_tz"].as_str()), (Some(900), Some("USD"), Some("monthly"), Some("Asia/Tokyo")), "{s}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_move_removes_new_entry_and_keeps_old() {
    let t = setup(true).await;
    let (ra, ta) = seed_relay(&t).await;
    let (rb, tb) = seed_relay_b(&t).await;
    let (mut a, mut b) = (Sim::new(ta), Sim::new(tb));
    let team = seed_team(&t, "团队1").await;
    let (node, pj) = provision_proxy(&t, team, ra, "203.0.113.10").await;
    drive(&t, &mut [&mut a], &pj, "succeeded").await;
    let old_entry = entries_of(&t, node).await[0].2;

    let job = t.post(&format!("/api/v1/machines/{node}/move"), json!({"to_relay_id": rb})).await["job_id"].as_str().unwrap().to_string();
    wait_until("新入口建好", || async { entries_of(&t, node).await.len() == 2 }).await;
    // 目标线路机一直没生效：中途取消
    t.post(&format!("/api/v1/jobs/{job}/cancel"), json!({})).await;
    let j = t.wait_job(&job, "cancelled").await;
    assert_eq!(j["steps"][0]["state"], "compensated");

    let es = entries_of(&t, node).await;
    assert_eq!(es.iter().find(|e| e.2 == old_entry).unwrap().1, "active", "旧入口保持不动");
    let new = es.iter().find(|e| e.2 != old_entry).unwrap();
    assert_eq!(new.1, "removing", "新入口被摘除");
    assert!(log_msgs(&t, &job).await.iter().any(|m| m.contains("旧入口保持不动")));
    // 目标线路机回报之后新入口 removed，节点还是只有旧入口在用
    for _ in 0..40 {
        b.round(&t).await;
        if entries_of(&t, node).await.iter().any(|e| e.2 == new.2 && e.1 == "removed") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let es = entries_of(&t, node).await;
    assert_eq!(es.iter().filter(|e| e.1 == "active").count(), 1);
    assert_eq!(es.iter().find(|e| e.2 == new.2).unwrap().1, "removed");
    assert_eq!(lifecycle_of(&t, node).await, "active");
    // 取消之后可以重新发起
    t.post(&format!("/api/v1/machines/{node}/move"), json!({"to_relay_id": rb})).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_is_validated() {
    let t = setup(false).await;
    let (ra, _) = seed_relay(&t).await;
    let (rb, _) = seed_relay_b(&t).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let mv = |to: Uuid| mv_req(&t, node, to);

    // 没有在用入口
    let (s, v) = mv(rb).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let mut tx = t.st.db.begin().await.unwrap();
    fleet_server::entries::lock_policy_shared(&mut tx).await.unwrap();
    let (e, _, _) = fleet_server::entries::create(&mut tx, ra, node).await.unwrap();
    tx.commit().await.unwrap();
    // 入口还没生效（pending）
    let (s, _) = mv(rb).await;
    assert_eq!(s, StatusCode::CONFLICT);
    sqlx::query("update entry set lifecycle = 'active' where id = $1").bind(e).execute(&t.st.db).await.unwrap();
    // 目标就是当前线路机
    let (s, v) = mv(ra).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    // 目标线路机不在用
    sqlx::query("update machine set lifecycle = 'retired', retired_at = now() where id = $1").bind(rb).execute(&t.st.db).await.unwrap();
    let (s, _) = mv(rb).await;
    assert_eq!(s, StatusCode::CONFLICT);
    sqlx::query("update machine set lifecycle = 'active', retired_at = null where id = $1").bind(rb).execute(&t.st.db).await.unwrap();
    // 目标线路机上已有这个节点的入口
    let mut tx = t.st.db.begin().await.unwrap();
    fleet_server::entries::lock_policy_shared(&mut tx).await.unwrap();
    fleet_server::entries::create(&mut tx, rb, node).await.unwrap();
    tx.commit().await.unwrap();
    let (s, v) = mv(rb).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    // 节点不是在用状态
    let other = seed_node(&t, team, 2, "203.0.113.11", "retired").await;
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{other}/move"), Some(json!({"to_relay_id": rb})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // 线路机本身不能换
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{ra}/move"), Some(json!({"to_relay_id": rb})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evacuate_creates_one_job_per_node_and_guard_mentions_it() {
    let t = setup(false).await;
    let (ra, _) = seed_relay(&t).await;
    let (rb, _) = seed_relay_b(&t).await;
    let team = seed_team(&t, "团队1").await;
    let mut nodes = vec![];
    for i in 0..3 {
        let n = seed_node(&t, team, i + 1, &format!("203.0.113.{}", 10 + i), "active").await;
        let mut tx = t.st.db.begin().await.unwrap();
        fleet_server::entries::lock_policy_shared(&mut tx).await.unwrap();
        let (e, _, _) = fleet_server::entries::create(&mut tx, ra, n).await.unwrap();
        tx.commit().await.unwrap();
        // 第三个节点的入口是停用的：搬空时跳过并说明原因
        sqlx::query("update entry set lifecycle = $2::entry_lifecycle where id = $1").bind(e).bind(if i == 2 { "disabled" } else { "active" }).execute(&t.st.db).await.unwrap();
        nodes.push(n);
    }

    // 线路机上还有入口：不能退役，提示里指向「搬空线路机」
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{ra}/retire"), Some(json!({"confirm": "线路机2"})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v["error"]["message"].as_str().unwrap().contains("搬空线路机"), "{v}");

    let (s, _) = t.req("POST", &format!("/api/v1/relays/{ra}/evacuate"), Some(json!({"to_relay_id": ra})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "搬到自己身上没有意义");
    let r = t.post(&format!("/api/v1/relays/{ra}/evacuate"), json!({"to_relay_id": rb})).await;
    let jobs = r["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2, "{r}");
    assert_eq!(r["skipped"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["skipped"][0]["machine_id"], nodes[2].to_string());
    assert!(r["skipped"][0]["reason"].as_str().unwrap().contains("已停用"), "{r}");
    for j in jobs {
        let jj = t.job(j["job_id"].as_str().unwrap()).await;
        assert_eq!(jj["kind"], "move_entry");
        assert_eq!(jj["state"], "queued");
    }
    // 再搬一次：两个节点都已有进行中的作业，全部跳过
    let r2 = t.post(&format!("/api/v1/relays/{ra}/evacuate"), json!({"to_relay_id": rb})).await;
    assert_eq!(r2["jobs"].as_array().unwrap().len(), 0);
    assert_eq!(r2["skipped"].as_array().unwrap().len(), 3);
    let audit = t.get("/api/v1/audit?limit=100").await;
    assert!(audit.as_array().unwrap().iter().any(|x| x["action"] == "relay.evacuate"));
    assert_eq!(audit.as_array().unwrap().iter().filter(|x| x["action"] == "entry.move").count(), 2);
}

// ── 恢复已退役的机器 ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_retired_proxy_node_appends_number_and_gets_new_entry() {
    let t = setup(true).await;
    let (ra, ta) = seed_relay(&t).await;
    let mut a = Sim::new(ta);
    let team = seed_team(&t, "团队1").await;
    let ps = provision_proxies(&t, team, ra, &["203.0.113.10", "203.0.113.11"]).await;
    let ((n1, j1), (n2, j2)) = (ps[0].clone(), ps[1].clone());
    drive(&t, &mut [&mut a], &j1, "succeeded").await;
    drive(&t, &mut [&mut a], &j2, "succeeded").await;
    assert_eq!((name_of(&t, n1).await, name_of(&t, n2).await), ("团队1-01".to_string(), "团队1-02".to_string()));
    // 给 n1 挂一份账单，恢复后要还在
    t.req("PUT", &format!("/api/v1/machines/{n1}/subscription"), Some(json!({"price_cents": 500, "due_date": "2027-01-01"})), None).await;

    // 退役 n1：n2 压缩成 01
    let rj = t.post(&format!("/api/v1/machines/{n1}/retire"), json!({"confirm": "团队1-01", "reason": "测试退役"})).await["job_id"].as_str().unwrap().to_string();
    drive(&t, &mut [&mut a], &rj, "succeeded").await;
    assert_eq!(lifecycle_of(&t, n1).await, "retired");
    assert_eq!(name_of(&t, n2).await, "团队1-01");
    let old_entry = entries_of(&t, n1).await[0].2;

    // 只有已退役的机器能恢复
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{n2}/restore"), Some(json!({"relay_id": ra})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // 预检：接在团队末尾（团队1-02），上游代理要重新填出站参数，不需要密码
    let pv = t.get(&format!("/api/v1/machines/{n1}/restore")).await;
    assert_eq!(pv["restorable"], true, "{pv}");
    assert_eq!(pv["name"], "团队1-02");
    assert_eq!(pv["number"], 2);
    assert_eq!(pv["old_name"], "团队1-01");
    assert_eq!(pv["needs"]["upstream"], true);
    assert_eq!(pv["needs"]["relay"], true);
    assert_eq!(pv["needs"]["password"], "no");
    assert!(pv["ip_conflict"].is_null());
    assert_eq!(pv["last_relay"]["id"], ra.to_string());
    // 缺中转线路机 / 缺上游参数 → 400
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{n1}/restore"), Some(json!({"upstream": upstream("p.example")})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{n1}/restore"), Some(json!({"relay_id": ra})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let r = t.post(&format!("/api/v1/machines/{n1}/restore"), json!({"relay_id": ra, "upstream": upstream("p2.example"), "reason": "客户续约"})).await;
    assert_eq!(r["name"], "团队1-02");
    assert_eq!(r["machine_id"], n1.to_string());
    let job = r["job_id"].as_str().unwrap().to_string();
    assert_eq!(t.job(&job).await["kind"], "restore_proxy");
    assert_eq!(lifecycle_of(&t, n1).await, "provisioning");
    // 恢复期间不能再被恢复第二次，也不能被改名
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{n1}/restore"), Some(json!({"relay_id": ra, "upstream": upstream("x")})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);

    let j = drive(&t, &mut [&mut a], &job, "succeeded").await;
    let keys: Vec<&str> = j["steps"].as_array().unwrap().iter().map(|s| s["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["attach", "wait_applied", "verify", "activate"]);
    assert_eq!(lifecycle_of(&t, n1).await, "active");
    let m = t.get(&format!("/api/v1/machines/{n1}")).await;
    assert_eq!(m["name"], "团队1-02");
    assert!(m["retired_at"].is_null(), "{m}");
    assert_eq!(m["lifecycle_reason"], "已恢复：客户续约");
    // 同一个机器 id：账单还在；旧入口仍是 removed，新入口是另一条 active
    let sub: i64 = sqlx::query_scalar("select count(*) from subscription where machine_id = $1").bind(n1).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(sub, 1);
    let es = entries_of(&t, n1).await;
    assert_eq!(es.len(), 2);
    assert_eq!(es.iter().find(|e| e.2 == old_entry).unwrap().1, "removed", "旧入口不复活");
    assert_eq!(es.iter().filter(|e| e.1 == "active").count(), 1);
    let audit = t.get("/api/v1/audit?limit=100").await;
    assert!(audit.as_array().unwrap().iter().any(|x| x["action"] == "machine.restore"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_is_blocked_when_ip_is_taken() {
    let t = setup(false).await;
    let (ra, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let old = seed_node(&t, team, 1, "203.0.113.10", "retired").await;
    // 退役后 IP 释放，被别的机器上架占用了
    let squatter = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let pv = t.get(&format!("/api/v1/machines/{old}/restore?probe=false")).await;
    assert_eq!(pv["restorable"], false);
    assert_eq!(pv["ip_conflict"]["machine_id"], squatter.to_string());
    assert!(pv["blockers"][0].as_str().unwrap().contains("203.0.113.10"));
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{old}/restore"), Some(json!({"relay_id": ra, "upstream": upstream("p")})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(lifecycle_of(&t, old).await, "retired", "被拒绝后机器不变");
    // 手动指定的编号被占用也是 409
    let free = seed_node(&t, team, 2, "203.0.113.12", "retired").await;
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{free}/restore"), Some(json!({"relay_id": ra, "upstream": upstream("p"), "number": 1})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // 输入错误是 400：编号超范围、指定的团队不存在
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{free}/restore"), Some(json!({"relay_id": ra, "upstream": upstream("p"), "number": 1000})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let body = json!({"relay_id": ra, "upstream": upstream("p"), "team_id": uuid::Uuid::now_v7()});
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{free}/restore"), Some(body), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_restore_returns_machine_to_retired_and_compacts_numbers() {
    let t = setup(true).await;
    let (ra, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    seed_node(&t, team, 1, "203.0.113.1", "active").await;
    seed_node(&t, team, 2, "203.0.113.2", "active").await;
    let gap = seed_node(&t, team, 4, "203.0.113.4", "active").await; // 编号有空档：取消后要压缩成 3
    let old = seed_node(&t, team, 7, "203.0.113.7", "retired").await;
    assert_eq!(name_of(&t, old).await, "团队1-07");

    let pv = t.get(&format!("/api/v1/machines/{old}/restore?probe=false")).await;
    assert_eq!(pv["number"], 3, "默认取最小的空号");
    let r = t.post(&format!("/api/v1/machines/{old}/restore"), json!({"relay_id": ra, "upstream": upstream("p"), "reason": "试试"})).await;
    let job = r["job_id"].as_str().unwrap().to_string();
    assert_eq!(name_of(&t, old).await, "团队1-03");
    // 作业已经建了入口（等线路机生效，线路机不回报）→ 取消
    wait_until("入口建好", || async { !entries_of(&t, old).await.is_empty() }).await;
    t.post(&format!("/api/v1/jobs/{job}/cancel"), json!({})).await;
    t.wait_job(&job, "cancelled").await;

    assert_eq!(lifecycle_of(&t, old).await, "retired", "不能留在 provisioning 占着 IP 和编号");
    let m = t.get(&format!("/api/v1/machines/{old}")).await;
    assert_eq!(m["name"], "团队1-07", "团队、编号回到恢复之前");
    assert!(!m["retired_at"].is_null());
    assert_eq!(entries_of(&t, old).await[0].1, "removing", "摘掉了作业建的入口");
    let outbound: i64 = sqlx::query_scalar("select count(*) from node_outbound where machine_id = $1").bind(old).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(outbound, 0, "出站秘密清掉");
    // 编号压缩：空档消失
    assert_eq!(name_of(&t, gap).await, "团队1-03");
    let audit = t.get("/api/v1/audit?limit=100").await;
    assert!(audit.as_array().unwrap().iter().any(|x| x["action"] == "team.renumber"));
    let msgs = log_msgs(&t, &job).await;
    assert!(msgs.iter().any(|m| m.contains("恢复已取消")), "{msgs:?}");
    // IP 和编号都释放了：同一个 IP 又可以上架
    let chk = t.post("/api/v1/provision/check", json!({"items": [{"kind": "proxy", "ip": "203.0.113.7", "team_id": team, "relay_id": ra, "upstream": upstream("x")}]})).await;
    assert_eq!(chk["items"][0]["ok"], true, "{chk}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_vps_precheck_and_job_creation() {
    let t = setup(true).await;
    let (ra, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    // 一台退役的 VPS，指向 127.0.0.1:1（必定连接被拒）
    let vps = Uuid::now_v7();
    sqlx::query(
        "insert into machine (id, kind, team_id, number, primary_ip, ssh_port, lifecycle, activated_at, retired_at, lifecycle_reason)
         values ($1, 'vps', $2, 1, '127.0.0.1', 1, 'retired', now(), now(), '到期不续')",
    )
    .bind(vps)
    .bind(team)
    .execute(&t.st.db)
    .await
    .unwrap();

    let pv = t.get(&format!("/api/v1/machines/{vps}/restore")).await;
    assert_eq!(pv["kind"], "vps");
    assert_eq!(pv["restorable"], true, "{pv}");
    assert_eq!(pv["name"], "团队1-01");
    assert_eq!(pv["needs"]["upstream"], false);
    assert_eq!(pv["needs"]["relay"], true);
    assert_eq!(pv["needs"]["password"], "optional", "连不上的机器没法判断中控密钥能不能登录：密码可选");
    assert_eq!(pv["ssh"]["key_login"], "unreachable");
    assert!(!pv["warnings"].as_array().unwrap().is_empty());
    assert_eq!(pv["retired_reason"], "到期不续");
    // 试登录记审计；和 POST 恢复一样要 write（不要 exec），只读密钥只能 probe=false
    let n: i64 = sqlx::query_scalar("select count(*) from audit_log where action = 'restore.probe' and target_id = $1").bind(vps).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 1);
    let key = t.post("/api/v1/api-keys", json!({"name": "ci-read", "scopes": ["read"]})).await;
    let tok = key["token"].as_str().unwrap();
    let (s, v) = t.req("GET", &format!("/api/v1/machines/{vps}/restore"), None, Some(tok)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, _) = t.req("GET", &format!("/api/v1/machines/{vps}/restore?probe=false"), None, Some(tok)).await;
    assert_eq!(s, StatusCode::OK);
    let key = t.post("/api/v1/api-keys", json!({"name": "ci-write", "scopes": ["read", "write"]})).await;
    let (s, v) = t.req("GET", &format!("/api/v1/machines/{vps}/restore"), None, Some(key["token"].as_str().unwrap())).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // 缺线路机 → 400
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{vps}/restore"), Some(json!({"login": {"password": "x"}})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let r = t.post(&format!("/api/v1/machines/{vps}/restore"), json!({"relay_id": ra, "login": {"password": "root-pw"}, "reason": "回炉"})).await;
    let job = r["job_id"].as_str().unwrap().to_string();
    let j0 = t.job(&job).await;
    assert_eq!(j0["kind"], "restore_vps");
    assert_eq!(j0["steps"][0]["key"], "preflight");
    assert_eq!(j0["steps"].as_array().unwrap().len(), 9, "和上架同样的 9 步");
    assert_eq!(lifecycle_of(&t, vps).await, "provisioning");
    let outbound: i64 = sqlx::query_scalar("select count(*) from node_outbound where machine_id = $1 and protocol = 'shadowsocks'").bind(vps).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(outbound, 1, "恢复时重新生成 SS 出站参数");
    let enc: bool = sqlx::query_scalar("select (params->'login'->>'password_enc') is not null from job where id = $1::uuid").bind(&job).fetch_one(&t.st.db).await.unwrap();
    assert!(enc, "root 密码加密存在作业参数里");

    // SSH 连不上 → 预检失败（网络类）；取消后机器回到 retired，密码清掉
    let failed = t.wait_job(&job, "failed").await;
    assert_eq!(failed["steps"][0]["error_kind"], "network");
    t.post(&format!("/api/v1/jobs/{job}/cancel"), json!({})).await;
    t.wait_job(&job, "cancelled").await;
    assert_eq!(lifecycle_of(&t, vps).await, "retired");
    let reason: Option<String> = sqlx::query_scalar("select lifecycle_reason from machine where id = $1").bind(vps).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(reason.as_deref(), Some("到期不续"), "退役原因回到恢复之前");
    let gone: bool = sqlx::query_scalar("select (params->'login') is null from job where id = $1::uuid").bind(&job).fetch_one(&t.st.db).await.unwrap();
    assert!(gone, "取消后加密的 root 密码要清掉");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_relay_label_conflict() {
    let t = setup(false).await;
    let (_live, _) = seed_relay(&t).await; // 线路机2 在用
    let old = Uuid::now_v7();
    let (sk, pk) = fleet_server::crypto::reality_keypair();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle, retired_at) values ($1, 'relay', '线路机2', '198.51.100.9', 'retired', now())")
        .bind(old)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into relay (machine_id, public_host, reality_private_enc, reality_public_key, generation, applied_generation) values ($1, 'old.test', $2, $3, 5, 5)")
        .bind(old)
        .bind(t.st.secrets.seal_str(&sk))
        .bind(pk)
        .execute(&t.st.db)
        .await
        .unwrap();
    let pv = t.get(&format!("/api/v1/machines/{old}/restore?probe=false")).await;
    assert_eq!(pv["restorable"], false);
    assert!(pv["blockers"][0].as_str().unwrap().contains("同名线路机"), "{pv}");
    assert_eq!(pv["needs"]["relay"], false);
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{old}/restore"), Some(json!({})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // 换个名字就可以：作业类型 restore_relay，期望状态代数 +1（agent 装好后必须真的应用一次）
    let r = t.post(&format!("/api/v1/machines/{old}/restore"), json!({"label": "线路机9", "login": {"password": "pw"}})).await;
    assert_eq!(r["name"], "线路机9");
    assert_eq!(t.job(r["job_id"].as_str().unwrap()).await["kind"], "restore_relay");
    let (g, a): (i64, i64) = sqlx::query_as("select generation, applied_generation from relay where machine_id = $1").bind(old).fetch_one(&t.st.db).await.unwrap();
    assert!(g > a, "代数 {g} 必须大于已应用代数 {a}");
}

/// 加入口：线路机不存在 404；不在用、节点在上面已有入口 409；都不留作业
#[tokio::test]
async fn add_entry_checks_relay_before_enqueue() {
    let t = setup(false).await;
    let (ra, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.20", "active").await;
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle, changed_gen) values ($1, $2, $3, 20001, $4, 'ab', 'active', 1)")
        .bind(Uuid::now_v7())
        .bind(ra)
        .bind(node)
        .bind(Uuid::new_v4())
        .execute(&t.st.db)
        .await
        .unwrap();
    let gone = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle, retired_at) values ($1, 'relay', '线路机8', '198.51.100.8', 'retired', now())")
        .bind(gone)
        .execute(&t.st.db)
        .await
        .unwrap();
    let path = format!("/api/v1/machines/{node}/entries");
    let (s, _) = t.req("POST", &path, Some(json!({"relay_id": Uuid::now_v7()})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, v) = t.req("POST", &path, Some(json!({"relay_id": gone})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, v) = t.req("POST", &path, Some(json!({"relay_id": ra})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("已经有入口"), "{v}");
    let n: i64 = sqlx::query_scalar("select count(*) from job").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_patch_null_clears_field() {
    let t = setup(false).await;
    let p = t.post("/api/v1/providers", json!({"name": "甲", "code": "A"})).await["id"].clone();
    let id = t.post("/api/v1/plans", json!({"provider_id": p, "name": "基础", "price_cents": 500, "traffic_gb": 1000, "cpu_cores": 2})).await["id"].as_str().unwrap().to_string();
    let (s, v) = t.req("PATCH", &format!("/api/v1/plans/{id}"), Some(json!({"traffic_gb": null})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let plan = t.get("/api/v1/plans").await[0].clone();
    assert!(plan["traffic_gb"].is_null() && plan["cpu_cores"] == 2, "null 清空、没传的不动：{plan}");
    let (s, v) = t.req("POST", "/api/v1/plans", Some(json!({"provider_id": p, "name": "基础", "price_cents": 1})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v.to_string().contains("同名套餐"), "{v}");
    // 改供应商
    let p2 = t.post("/api/v1/providers", json!({"name": "乙", "code": "B"})).await["id"].clone();
    let (s, v) = t.req("PATCH", &format!("/api/v1/plans/{id}"), Some(json!({"provider_id": p2})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(t.get("/api/v1/plans").await[0]["provider_id"], p2);
}

// ── 线路机直出入口（节点就是线路机自己）────────────────────────────────────

/// 直接在库里给一台在用的入口（不走作业）：`node` 等于 `relay` 就是直出入口
async fn seed_entry(t: &T, relay: Uuid, node: Uuid) -> Uuid {
    let mut tx = t.st.db.begin().await.unwrap();
    fleet_server::entries::lock_policy_shared(&mut tx).await.unwrap();
    let (e, _, _) = fleet_server::entries::create(&mut tx, relay, node).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("update entry set lifecycle = 'active' where id = $1").bind(e).execute(&t.st.db).await.unwrap();
    e
}

/// 新增：作业建入口 → 线路机应用 → 出口探测（期望线路机自己的 IP）→ 完成；期望状态、视图、分享链接、启停轮换、改名、一台一条
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_entry_create_desired_view_and_one_per_relay() {
    let t = setup(true).await;
    let (ra, tok) = seed_relay(&t).await;
    let (rb, _) = seed_relay_b(&t).await;
    let mut sim = Sim::new(tok.clone());
    let path = format!("/api/v1/machines/{ra}/entries");
    // 线路机只能给自己加
    let (s, v) = t.req("POST", &path, Some(json!({"relay_id": rb})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let job = t.post(&path, json!({"relay_id": ra})).await["job_id"].as_str().unwrap().to_string();
    let j = drive(&t, &mut [&mut sim], &job, "succeeded").await;
    assert_eq!(j["kind"], "attach_entry");
    let fw = j["steps"].as_array().unwrap().iter().find(|s| s["key"] == "firewall").unwrap().clone();
    assert_eq!(fw["state"], "skipped", "{fw}");
    let audit = t.get("/api/v1/audit?limit=20").await;
    assert!(audit.as_array().unwrap().iter().any(|x| x["action"] == "entry.add" && x["detail"]["direct"] == true), "{audit}");

    // 库里：节点就是线路机自己
    let (eid, node, lc, port): (Uuid, Uuid, String, i32) =
        sqlx::query_as("select id, node_id, lifecycle::text, port from entry where relay_id = $1").bind(ra).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((node, lc.as_str(), port), (ra, "active", 20000));
    // 期望状态：直出出站，备注「线路机名-直出」，出口探测期望线路机自己的 IP，没有访问策略
    let ds = sim.round(&t).await;
    assert_eq!(ds.entries.len(), 1);
    let e = &ds.entries[0];
    assert_eq!(e.outbound, Outbound::Direct);
    assert_eq!(e.remark, "线路机2-直出");
    assert_eq!(e.expect_egress_ip.to_string(), "198.51.100.2");
    assert!(e.enabled && e.policy.is_none());
    let (_, raw) = t.req("GET", "/agent/v1/desired?after=0", None, Some(&tok)).await;
    assert_eq!(raw["entries"][0]["outbound"], json!({"protocol": "direct"}), "{raw}");

    // 视图：线路机的入口里只出现一次，标 direct，节点是它自己、名字「线路机2-直出」；没有 TCP 探测，健康正常
    let m = t.get(&format!("/api/v1/machines/{ra}")).await;
    let es = m["entries"].as_array().unwrap();
    assert_eq!(es.len(), 1, "{m}");
    assert_eq!((es[0]["direct"].clone(), es[0]["node"]["id"].clone(), es[0]["node"]["name"].clone()), (json!(true), json!(ra), json!("线路机2-直出")));
    assert!(es[0]["tcp"].is_null() && es[0]["egress"]["ok"] == true, "{m}");
    assert_eq!(m["health"]["status"], "ok", "{m}");
    assert!(t.get("/api/v1/relays").await.as_array().unwrap().iter().all(|r| r["entries"].as_array().unwrap().len() <= 1));

    // 每台线路机至多一条：接口先拦，库里的唯一索引兜底
    let (s, v) = t.req("POST", &path, Some(json!({"relay_id": ra})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("已经有直出入口"), "{v}");
    let mut tx = t.st.db.begin().await.unwrap();
    fleet_server::entries::lock_policy_shared(&mut tx).await.unwrap();
    assert!(fleet_server::entries::create(&mut tx, ra, ra).await.is_err());
    drop(tx);
    // 线路机不能当别的线路机的节点
    let mut tx = t.st.db.begin().await.unwrap();
    fleet_server::entries::lock_policy_shared(&mut tx).await.unwrap();
    assert!(fleet_server::entries::create(&mut tx, rb, ra).await.is_err());
    drop(tx);

    // 分享链接：线路机地址 + 入口端口，#后是「线路机2-直出」
    let url = t.get(&format!("/api/v1/entries/{eid}/share")).await["url"].as_str().unwrap().to_string();
    assert!(url.contains(&format!("@relay.test:{port}?")), "{url}");
    assert!(url.ends_with("#%E7%BA%BF%E8%B7%AF%E6%9C%BA2-%E7%9B%B4%E5%87%BA"), "{url}");

    // 停用、启用、轮换
    t.post(&format!("/api/v1/entries/{eid}/actions"), json!({"action": "disable"})).await;
    assert!(!sim.round(&t).await.entries[0].enabled);
    t.post(&format!("/api/v1/entries/{eid}/actions"), json!({"action": "enable"})).await;
    let before = sim.round(&t).await.entries[0].client_uuid;
    t.post(&format!("/api/v1/entries/{eid}/actions"), json!({"action": "rotate", "confirm": format!("线路机2:{port}")})).await;
    let after = sim.round(&t).await;
    assert!(after.entries[0].enabled && after.entries[0].client_uuid != before);

    // 线路机改名：直出入口的备注跟着变（代数 +1，agent 拿到新名字）
    let gen0: i64 = sqlx::query_scalar("select generation from relay where machine_id = $1").bind(ra).fetch_one(&t.st.db).await.unwrap();
    let rev = t.get(&format!("/api/v1/machines/{ra}")).await["revision"].clone();
    let (s, v) = t.req("PATCH", &format!("/api/v1/machines/{ra}"), Some(json!({"revision": rev, "label": "线路机9"})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let ds = sim.round(&t).await;
    assert!(ds.generation as i64 > gen0);
    assert_eq!(ds.entries[0].remark, "线路机9-直出");

    // 名字、备注有长度上限
    let rev = t.get(&format!("/api/v1/machines/{ra}")).await["revision"].clone();
    let (s, v) = t.req("PATCH", &format!("/api/v1/machines/{ra}"), Some(json!({"revision": rev, "label": "长".repeat(201)})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = t.req("PATCH", &format!("/api/v1/machines/{ra}"), Some(json!({"revision": rev, "note": "x".repeat(2001)})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = t.req("PATCH", &format!("/api/v1/machines/{ra}"), Some(json!({"revision": rev, "note": "备".repeat(2000)})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// 搬空：只给节点建换线路机作业，直出入口不搬、也不算「跳过」；线路机本身不能换线路机
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evacuate_skips_direct_entry() {
    let t = setup(false).await;
    let (ra, _) = seed_relay(&t).await;
    let (rb, _) = seed_relay_b(&t).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let de = seed_entry(&t, ra, ra).await;
    seed_entry(&t, ra, node).await;
    let r = t.post(&format!("/api/v1/relays/{ra}/evacuate"), json!({"to_relay_id": rb})).await;
    assert_eq!(r["jobs"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["jobs"][0]["machine_id"], node.to_string());
    assert!(r["skipped"].as_array().unwrap().is_empty(), "{r}");
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{ra}/move"), Some(json!({"to_relay_id": rb})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let lc: String = sqlx::query_scalar("select lifecycle::text from entry where id = $1").bind(de).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(lc, "active");
    assert_eq!(entries_of(&t, ra).await.len(), 1, "目标线路机上没有给它建入口");
}

/// 退役线路机：守卫只数节点入口（不让先搬空直出入口）；退役作业自动摘掉直出入口
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retire_relay_removes_direct_entry() {
    let t = setup(true).await;
    let (ra, tok) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let de = seed_entry(&t, ra, ra).await;
    let ne = seed_entry(&t, ra, node).await;
    let retire = format!("/api/v1/machines/{ra}/retire");
    let (s, v) = t.req("POST", &retire, Some(json!({"confirm": "线路机2", "cleanup": false})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v["error"]["message"].as_str().unwrap().contains("还有 1 个节点入口"), "{v}");
    // 节点换走了（这里直接标成已移除）：只剩直出入口，照常退役
    sqlx::query("update entry set lifecycle = 'removed', removed_at = now() where id = $1").bind(ne).execute(&t.st.db).await.unwrap();
    let job = t.post(&retire, json!({"confirm": "线路机2", "cleanup": false})).await["job_id"].as_str().unwrap().to_string();
    // 模拟线路机：拉期望状态、回报已应用，直到直出入口摘除（之后令牌会被撤销）
    let mut emptied = false;
    for _ in 0..200 {
        let lc: String = sqlx::query_scalar("select lifecycle::text from entry where id = $1").bind(de).fetch_one(&t.st.db).await.unwrap();
        if lc == "removed" {
            break;
        }
        let (s, v) = t.req("GET", "/agent/v1/desired?after=0", None, Some(&tok)).await;
        if s == StatusCode::OK {
            let ds: DesiredState = serde_json::from_value(v).unwrap();
            emptied |= ds.entries.is_empty();
            let rep = ApplyReport { generation: ds.generation, ok: true, error: None, entries: vec![] };
            t.req("POST", "/agent/v1/applied", Some(serde_json::to_value(&rep).unwrap()), Some(&tok)).await;
        }
        t.nudge_jobs().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(emptied, "退役中的线路机拿到的期望状态里没有直出入口");
    t.wait_job(&job, "succeeded").await;
    assert_eq!(lifecycle_of(&t, ra).await, "retired");
    let lc: String = sqlx::query_scalar("select lifecycle::text from entry where id = $1").bind(de).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(lc, "removed");
}
