//! 穿透入口（crate::reverse）：期望状态里的形状、开关的门槛（agent 版本、机器类型、dist 里的 xray、进行中的作业）、探测口径、
//! 隧道挂不上时叫节点重启 bridge、新入口默认模式（设置、上架、加入口）。要 SSH 的开关作业本身在 tests/reverse.rs（假 ssh）。
//! 需要 PostgreSQL（见 common）。

use crate::common;

use axum::http::StatusCode;
use common::{now_ms, seed_relay, seed_team, setup, Sim, T};
use fleet_proto::{Outbound, Report, ReportAck};
use serde_json::{json, Value};
use uuid::Uuid;

/// 一台在用的 VPS 节点（有 SS 出站参数），agent 报没报过会穿透由 `caps` 定。返回机器 id
async fn vps(t: &T, team: Uuid, number: i32, ip: &str, caps: bool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, ssh_port, lifecycle, activated_at) values ($1, 'vps', $2, $3, $4::inet, 22, 'active', now())")
        .bind(id)
        .bind(team)
        .bind(number)
        .bind(ip)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into node_outbound (machine_id, protocol, host, port, method, secret_enc) values ($1, 'shadowsocks', $2, 8388, 'aes-256-gcm', $3)")
        .bind(id)
        .bind(ip)
        .bind(t.st.secrets.seal_str("sspw"))
        .execute(&t.st.db)
        .await
        .unwrap();
    set_caps(t, id, caps).await;
    id
}

/// 机器的 agent 报过的能力（agent_state.info.caps）
async fn set_caps(t: &T, m: Uuid, caps: bool) {
    let c: Vec<&str> = if caps { vec!["reverse"] } else { vec![] };
    sqlx::query(
        "insert into agent_state (machine_id, agent_version, last_report_at, info) values ($1, 'v', now(), jsonb_build_object('caps', to_jsonb($2::text[])))
         on conflict (machine_id) do update set info = agent_state.info || jsonb_build_object('caps', to_jsonb($2::text[]))",
    )
    .bind(m)
    .bind(&c)
    .execute(&t.st.db)
    .await
    .unwrap();
}

/// 在用入口（模式 `mode`）
async fn entry(t: &T, relay: Uuid, node: Uuid, mode: &str) -> Uuid {
    let mut tx = t.st.db.begin().await.unwrap();
    let (e, _, _) = fleet_server::entries::create_with(&mut tx, relay, node, mode).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("update entry set lifecycle = 'active' where id = $1").bind(e).execute(&t.st.db).await.unwrap();
    e
}

fn dist_xray(t: &T) {
    let p = t.dir.path().join("dist/xray");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, "#!/bin/sh\n").unwrap();
}

async fn preview(t: &T, e: Uuid, on: bool) -> Value { t.get(&format!("/api/v1/entries/{e}/reverse?on={on}")).await }

fn blockers(v: &Value) -> String { v["blockers"].as_array().unwrap().iter().map(|b| b.as_str().unwrap()).collect::<Vec<_>>().join("｜") }

/// 期望状态：穿透入口下发 `reverse` 出站（带 bridge_uuid，没有线路机 → 节点的地址），出口探测照常期望节点自己的 IP；
/// 视图里入口带 mode，TCP 探测一格恒为 null（切换前留下的不算），健康只看出口
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desired_state_and_view() {
    let t = setup(false).await;
    let (relay, tok) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let (a, b) = (vps(&t, team, 1, "203.0.113.11", true).await, vps(&t, team, 2, "203.0.113.12", true).await);
    let (ea, eb) = (entry(&t, relay, a, "reverse").await, entry(&t, relay, b, "normal").await);
    let bridge: Uuid = sqlx::query_scalar("select bridge_uuid from entry where id = $1").bind(ea).fetch_one(&t.st.db).await.unwrap();
    let mut sim = Sim::new(tok);
    let ds = sim.round(&t).await;
    let get = |id: Uuid| ds.entries.iter().find(|e| e.id == id).unwrap().clone();
    assert_eq!(get(ea).outbound, Outbound::Reverse { bridge_uuid: bridge });
    assert_eq!(get(ea).expect_egress_ip.to_string(), "203.0.113.11");
    assert!(get(ea).outbound.addr().is_none(), "线路机不连节点：没有 TCP 探测");
    assert_ne!(get(ea).client_uuid, bridge, "bridge 与客户各用各的 UUID");
    assert!(matches!(get(eb).outbound, Outbound::Shadowsocks { ref host, port: 8388, .. } if host == "203.0.113.12"));
    // 切换之前留下一条失败的 TCP 探测：穿透入口的视图里不显示、不进健康
    sqlx::query("insert into probe_result (entry_id, ts, kind, ok, detail) values ($1, now(), 'tcp', false, '超时')").bind(ea).execute(&t.st.db).await.unwrap();
    t.st.live.invalidate();
    let m = t.get(&format!("/api/v1/machines/{a}")).await;
    let e = &m["entries"][0];
    assert_eq!((e["mode"].as_str(), e["tcp"].is_null(), e["egress"]["ok"].as_bool()), (Some("reverse"), true, Some(true)), "{e}");
    let reasons: Vec<&str> = m["health"]["reasons"].as_array().unwrap().iter().filter_map(|r| r["key"].as_str()).collect();
    assert!(!reasons.contains(&"tcp_fail"), "{reasons:?}");
    assert_eq!(t.get(&format!("/api/v1/machines/{b}")).await["entries"][0]["mode"], "normal");
}

/// 开关的门槛：线路机 / 节点 agent 没报过会穿透、家宽代理、同机 IP 节点、直出、dist 里没有 xray、节点上有进行中的作业、已经是这个模式。
/// 通过时建 reverse 作业（目标是节点）并记审计
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gates_and_enqueue() {
    let t = setup(false).await;
    let (relay, tok) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let node = vps(&t, team, 1, "203.0.113.21", false).await;
    let e = entry(&t, relay, node, "normal").await;
    // 线路机还没报过（没有 caps）：线路机 agent 太旧
    let p = preview(&t, e, true).await;
    assert_eq!(p["ok"], false);
    assert!(blockers(&p).contains("线路机 线路机2 的 agent 版本过旧") && blockers(&p).contains("fleetctl upgrade-agent 线路机2"), "{p}");
    Sim::new(tok).round(&t).await;
    assert_eq!(sqlx::query_scalar::<_, Value>("select info->'caps' from agent_state where machine_id = $1").bind(relay).fetch_one(&t.st.db).await.unwrap(), json!(["reverse"]));
    let p = preview(&t, e, true).await;
    assert!(blockers(&p).contains("节点 团队1-01 的 agent 版本过旧"), "{p}");
    set_caps(&t, node, true).await;
    let p = preview(&t, e, true).await;
    assert!(blockers(&p).contains("dist 里没有 xray"), "{p}");
    dist_xray(&t);
    let p = preview(&t, e, true).await;
    assert_eq!((p["ok"].as_bool(), p["from"].as_str(), p["to"].as_str()), (Some(true), Some("normal"), Some("reverse")), "{p}");
    let impacts = p["impacts"].to_string();
    assert!(impacts.contains("不重启 Xray") && impacts.contains("分享链接、客户端订阅、软路由出口都不变") && impacts.contains("198.51.100.2:"), "{impacts}");
    assert!(p["warnings"].to_string().contains("-（穿透）"));
    // 关闭的预览：已经是普通
    assert!(blockers(&preview(&t, e, false).await).contains("已经是普通模式"));
    // 发起
    let v = t.post(&format!("/api/v1/entries/{e}/reverse"), json!({"on": true})).await;
    let job = v["job_id"].as_str().unwrap().to_string();
    let j = t.job(&job).await;
    assert_eq!((j["kind"].as_str(), j["target"]["id"].as_str()), (Some("reverse"), Some(node.to_string().as_str())), "{j}");
    let params: Value = sqlx::query_scalar("select params from job where id = $1::uuid").bind(&job).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((params["entry_id"].as_str(), params["on"].as_bool(), params["from"].as_str()), (Some(e.to_string().as_str()), Some(true), Some("normal")));
    let audit: (String, Value) = sqlx::query_as("select action, detail from audit_log where action = 'entry.reverse'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!((audit.1["on"].as_bool(), audit.1["port"].as_i64().is_some()), (Some(true), true));
    let (title, summary) = fleet_server::api::audit::audit_text(&audit.0, &audit.1);
    assert_eq!(title, "开启穿透");
    assert!(summary.starts_with("团队1-01 · 线路机2:"), "{summary}");
    // 作业还在：再发起 409，写明在等哪个作业
    let (s, v) = t.req("POST", &format!("/api/v1/entries/{e}/reverse"), Some(json!({"on": true})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v["error"]["message"].as_str().unwrap().contains("切换穿透"), "{v}");
    // 家宽代理、同机 IP 节点、直出：都不支持
    let proxy = common::seed_node(&t, team, 2, "203.0.113.22", "active").await;
    sqlx::query("insert into node_outbound (machine_id, protocol, host, port, secret_enc) values ($1, 'socks5', 'p', 1080, $2)").bind(proxy).bind(t.st.secrets.seal_str("x")).execute(&t.st.db).await.unwrap();
    let ep = entry(&t, relay, proxy, "normal").await;
    assert!(blockers(&preview(&t, ep, true).await).contains("家宽代理"));
    let host = vps(&t, team, 3, "203.0.113.23", true).await;
    let ipn = vps(&t, team, 4, "203.0.113.24", true).await;
    sqlx::query("update machine set host_id = $2, ssh_port = null where id = $1").bind(ipn).bind(host).execute(&t.st.db).await.unwrap();
    let ei = entry(&t, relay, ipn, "normal").await;
    assert!(blockers(&preview(&t, ei, true).await).contains("同机 IP 节点暂不支持穿透"));
    let ed = entry(&t, relay, relay, "normal").await;
    assert!(blockers(&preview(&t, ed, true).await).contains("直出"));
    // 不存在的入口 404
    assert_eq!(t.req("GET", &format!("/api/v1/entries/{}/reverse?on=true", Uuid::now_v7()), None, None).await.0, StatusCode::NOT_FOUND);
}

async fn report(t: &T, tok: &str, seq: u64, tunnels_down: Vec<Uuid>, caps: bool) -> ReportAck {
    let r = Report {
        seq,
        boot_id: "b".into(),
        agent_version: "v".into(),
        sent_at_ms: now_ms(),
        caps: if caps { vec!["reverse".into()] } else { vec![] },
        tunnels_down,
        ..Default::default()
    };
    let (s, v) = t.req("POST", "/agent/v1/report", Some(serde_json::to_value(&r).unwrap()), Some(tok)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    serde_json::from_value(v).unwrap()
}

/// 线路机报隧道没挂上 → 记下要重启（同一入口 3 分钟内不重复）→ 节点的回执带上 bridge_kick；普通入口、别的线路机的入口不算。
/// caps 每份都写：换回不带 caps 的旧 agent 之后就不算会穿透了
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tunnel_down_kicks_node_bridge() {
    let t = setup(false).await;
    let (relay, rtok) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let (a, b) = (vps(&t, team, 1, "203.0.113.31", true).await, vps(&t, team, 2, "203.0.113.32", true).await);
    let (ea, eb) = (entry(&t, relay, a, "reverse").await, entry(&t, relay, b, "normal").await);
    let ntok = "fa1_test_node_token";
    sqlx::query("insert into agent_token (id, machine_id, token_hash) values ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(a)
        .bind(fleet_server::crypto::sha256(ntok.as_bytes()))
        .execute(&t.st.db)
        .await
        .unwrap();
    assert_eq!(report(&t, ntok, 1, vec![], true).await.bridge_kick, None, "没叫过");
    let ack = report(&t, &rtok, 1, vec![ea, eb, Uuid::now_v7()], true).await;
    assert_eq!(ack.bridge_kick, None, "线路机的回执不带");
    let kicked: Vec<(Uuid, Option<time::OffsetDateTime>)> = sqlx::query_as("select id, reverse_kick_at from entry where id = any($1) order by id").bind(vec![ea, eb]).fetch_all(&t.st.db).await.unwrap();
    let at = |id: Uuid| kicked.iter().find(|k| k.0 == id).unwrap().1;
    assert!(at(ea).is_some() && at(eb).is_none(), "只记穿透入口：{kicked:?}");
    let k1 = report(&t, ntok, 2, vec![], true).await.bridge_kick.expect("节点回执带上重启请求");
    assert!((k1 - now_ms()).abs() < 60_000);
    // 3 分钟内再报：不重复叫（值不变，节点不会再重启）
    report(&t, &rtok, 2, vec![ea], true).await;
    assert_eq!(report(&t, ntok, 3, vec![], true).await.bridge_kick, Some(k1));
    // 过了 3 分钟还没挂上：再叫一次（新值）
    sqlx::query("update entry set reverse_kick_at = now() - interval '4 minutes' where id = $1").bind(ea).execute(&t.st.db).await.unwrap();
    report(&t, &rtok, 3, vec![ea], true).await;
    let k2 = report(&t, ntok, 4, vec![], true).await.bridge_kick.unwrap();
    assert!(k2 > k1);
    // 10 分钟以前的不再带；切回普通模式的不带
    sqlx::query("update entry set reverse_kick_at = now() - interval '11 minutes' where id = $1").bind(ea).execute(&t.st.db).await.unwrap();
    assert_eq!(report(&t, ntok, 5, vec![], true).await.bridge_kick, None);
    // 旧 agent（不报 caps）：能力清空，开关被挡
    report(&t, &rtok, 4, vec![], false).await;
    assert_eq!(sqlx::query_scalar::<_, Value>("select info->'caps' from agent_state where machine_id = $1").bind(relay).fetch_one(&t.st.db).await.unwrap(), json!([]));
}

/// 线路机只开 AnyTLS（Reality 入站听回环，bridge 连不上）：加入口退回普通并写明原因、上架检查同样、手动开穿透的预览被拒，也核不进 set_mode
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anytls_only_relay_has_no_reverse() {
    let t = setup(false).await;
    let (relay, tok) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let node = vps(&t, team, 1, "203.0.113.51", true).await;
    let e = entry(&t, relay, node, "normal").await;
    Sim::new(tok).round(&t).await;
    dist_xray(&t);
    assert_eq!(preview(&t, e, true).await["ok"], true);
    sqlx::query("update relay set protocols = 'anytls' where machine_id = $1").bind(relay).execute(&t.st.db).await.unwrap();
    let p = preview(&t, e, true).await;
    assert_eq!(p["ok"], false, "{p}");
    assert!(blockers(&p).contains("线路机 线路机2 只开 AnyTLS，穿透要 Reality"), "{p}");
    let mut tx = t.st.db.begin().await.unwrap();
    assert!(fleet_server::reverse::set_mode(&mut tx, e, "reverse").await.is_err());
    drop(tx);
    // 加入口不给模式（默认穿透）：退回普通
    let node2 = vps(&t, team, 2, "203.0.113.52", true).await;
    let v = t.post(&format!("/api/v1/machines/{node2}/entries"), json!({"relay_id": relay})).await;
    assert_eq!(v["entry_mode"], "normal", "{v}");
    assert!(v["note"].as_str().unwrap().contains("只开 AnyTLS"), "{v}");
    // 上架检查
    let c = t.post("/api/v1/provision/check", json!({"items": [{"kind": "vps", "ip": "203.0.113.53", "team_id": team, "relay_id": relay}]})).await;
    assert_eq!(c["items"][0]["entry_mode"], "normal", "{c}");
    assert!(c["items"][0]["warnings"].to_string().contains("只开 AnyTLS"), "{c}");
}

/// 新入口默认模式：没设过是穿透；改了写审计；上架家宽代理要穿透时退回普通（检查的提醒、作业参数里写明）；加入口不给模式按默认，
/// 直出 / 家宽代理退回普通
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_mode_setting_and_new_entries() {
    let t = setup(false).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let v = t.get("/api/v1/settings/entry-mode").await;
    assert_eq!((v["default"].as_str(), v["set"].as_bool(), v["label"].as_str()), (Some("reverse"), Some(false), Some("穿透")));
    let (s, _) = t.req("PUT", "/api/v1/settings/entry-mode", Some(json!({"default": "穿透"})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, v) = t.req("PUT", "/api/v1/settings/entry-mode", Some(json!({"default": "normal"})), None).await;
    assert_eq!((s, v["data"]["default"].as_str(), v["data"]["set"].as_bool()), (StatusCode::OK, Some("normal"), Some(true)));
    let d: Value = sqlx::query_scalar("select detail from audit_log where action = 'settings.entry_mode'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(d, json!({"from": "reverse", "to": "normal"}));
    assert_eq!(fleet_server::api::audit::audit_text("settings.entry_mode", &d).1, "穿透 → 普通");
    t.req("PUT", "/api/v1/settings/entry-mode", Some(json!({"default": "reverse"})), None).await;

    // 上架家宽代理（默认穿透）：检查通过、带提醒，按普通建
    let item = json!({"kind": "proxy", "ip": "203.0.113.41", "team_id": team, "relay_id": relay, "upstream": {"protocol": "socks5", "host": "p.example", "port": 1080}});
    let c = t.post("/api/v1/provision/check", json!({"items": [item.clone()]})).await;
    let it = &c["items"][0];
    assert_eq!((it["ok"].as_bool(), it["entry_mode"].as_str()), (Some(true), Some("normal")), "{it}");
    assert!(it["warnings"].to_string().contains("按普通模式建入口：家宽代理"), "{it}");
    // 写错模式：校验不过
    let mut bad = item.clone();
    bad["entry_mode"] = json!("tunnel");
    let c = t.post("/api/v1/provision/check", json!({"items": [bad]})).await;
    assert!(c["items"][0]["errors"].to_string().contains("normal（普通）或 reverse（穿透）"));
    let created = t.post("/api/v1/provision", json!({"items": [item]})).await;
    let j = &created["jobs"][0];
    assert_eq!(j["entry_mode"], "normal");
    let params: Value = sqlx::query_scalar("select params from job where id = $1::uuid").bind(j["job_id"].as_str().unwrap()).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(params["entry_mode"], "normal");
    assert!(params["entry_mode_note"].as_str().unwrap().contains("家宽代理"));
    // 上架 VPS 节点不写模式：按默认（穿透），作业建入口时再核 agent
    let c = t.post("/api/v1/provision/check", json!({"items": [{"kind": "vps", "ip": "203.0.113.42", "team_id": team, "relay_id": relay, "entry_mode": "normal"}]})).await;
    assert_eq!(c["items"][0]["entry_mode"], "normal", "明确给了普通");

    // 加入口：VPS 节点不给模式按默认（穿透）；线路机直出给了穿透也按普通
    let node = vps(&t, team, 5, "203.0.113.43", true).await;
    let v = t.post(&format!("/api/v1/machines/{node}/entries"), json!({"relay_id": relay})).await;
    assert_eq!((v["entry_mode"].as_str(), v["note"].is_null()), (Some("reverse"), true), "{v}");
    let v = t.post(&format!("/api/v1/machines/{relay}/entries"), json!({"relay_id": relay, "mode": "reverse"})).await;
    assert_eq!(v["entry_mode"], "normal");
    assert!(v["note"].as_str().unwrap().contains("直出"));
}
