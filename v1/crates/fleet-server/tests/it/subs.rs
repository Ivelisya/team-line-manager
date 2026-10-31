//! 客户端订阅：拉取按规则选入口（默认不带备用团队、带直出）、轮换后旧链接 404 且计入按来源的失败限速

use crate::common::{self, tele, T};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;
use uuid::Uuid;

/// 不登录拉订阅（从 192.0.2.9 来），返回 (状态码, Cache-Control, 正文)
async fn pull(t: &T, path: &str) -> (StatusCode, String, String) {
    let req = Request::get(path).header("x-forwarded-for", "192.0.2.9").body(Body::empty()).unwrap();
    let res = t.app.clone().oneshot(req).await.unwrap();
    let (s, cc) = (res.status(), res.headers().get("cache-control").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default());
    (s, cc, String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
}

/// 链接里 /sub/ 起的那段
fn sub_path(url: &serde_json::Value) -> String { url.as_str().unwrap().strip_prefix("http://fleet.test").unwrap().to_string() }

async fn seed(t: &T) -> Uuid {
    let relay = tele::seed_relay(t, "线路机1", "198.51.100.1", "tok1").await;
    let team = tele::seed_team(t, "团队1").await;
    tele::seed_node(t, relay, team, 1, "203.0.113.5", 20000).await;
    // 迁移建好的备用团队
    let spare: Uuid = sqlx::query_scalar("select id from team where spare").fetch_one(&t.st.db).await.unwrap();
    tele::seed_node(t, relay, spare, 3, "203.0.113.9", 20001).await;
    // 线路机自己的直出入口
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $2, 20002, $3, '01234567', 'active')")
        .bind(Uuid::now_v7())
        .bind(relay)
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();
    spare
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_rotate_and_failure_limit() {
    let t = common::boot(false, "trust_forwarded_for = true").await;
    let spare = seed(&t).await;
    let d = t.post("/api/v1/client-subs", json!({"name": "家里"})).await;
    assert_eq!(d["items"], 2, "{d}");
    let path = sub_path(&d["url"]);
    let (s, cc, y) = pull(&t, &path).await;
    assert_eq!((s, cc.as_str()), (StatusCode::OK, "no-store"), "{y}");
    // 测试库没有供应商、离线 IP 库是空的：没有国家、供应商那两段
    assert!(y.contains("name: \"线路机1 直出 · 198.51.100.1 · VLESS Reality\""), "{y}");
    assert!(y.contains("name: \"家宽 · 团队1-01 · 203.0.113.5 · VLESS Reality\"") && !y.contains("203.0.113.9"), "不带备用团队：{y}");
    assert!(y.contains("short-id: \"01234567\"") && y.contains("port: 20002"), "{y}");
    let (s, _, b) = pull(&t, &format!("{path}?format=base64")).await;
    let lines = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(b.trim()).unwrap()).unwrap();
    assert_eq!((s, lines.lines().filter(|l| l.starts_with("vless://")).count()), (StatusCode::OK, 2), "{lines}");
    let fetched: Option<String> = sqlx::query_scalar("select host(last_fetch_ip) from client_sub").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(fetched.as_deref(), Some("192.0.2.9"));

    // 轮换：旧链接 404（错的令牌记一次失败；成功的不留记录），新链接照常
    let r = t.post(&format!("/api/v1/client-subs/{}/rotate", d["id"].as_str().unwrap()), json!({})).await;
    assert_eq!(pull(&t, &path).await.0, StatusCode::NOT_FOUND);
    let n: i64 = sqlx::query_scalar("select count(*) from login_failure where ip = '192.0.2.9'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 1);
    let new = sub_path(&r["url"]);
    assert_eq!(pull(&t, &new).await.0, StatusCode::OK);
    let acts: Vec<String> = sqlx::query_scalar("select action from audit_log where target_kind = 'client_sub' order by id").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(acts, ["client_sub.create", "client_sub.rotate"], "拉取不记审计");
    // 只要备用团队、不含直出
    let d = t.post("/api/v1/client-subs", json!({"name": "备用", "rule": {"team_mode": "only", "team_ids": [spare], "direct": false}})).await;
    let (_, _, y) = pull(&t, &sub_path(&d["url"])).await;
    assert!(d["items"] == 1 && y.contains("203.0.113.9") && !y.contains("203.0.113.5"), "{d} {y}");
    // 同一来源失败满 20 次（和登录共用）：对的令牌也先 429
    sqlx::query("insert into login_failure (username, ip) select 'x' || g, '192.0.2.9' from generate_series(1, 19) g").execute(&t.st.db).await.unwrap();
    let (s, _, _) = pull(&t, &new).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
}

/// 删团队把它从订阅的 team_ids 里摘掉：订阅照常能存（规则校验不再因已删的团队 400），审计记下受影响的订阅；
/// 「只要这些团队」的订阅只选了它时成了空列表（只剩直出与另加的机器），原样再存回 400，要重新选团队
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn team_delete_detaches_from_subs() {
    let t = common::boot(false, "").await;
    let (a, b) = (tele::seed_team(&t, "团队1").await, tele::seed_team(&t, "团队2").await);
    let except = t.post("/api/v1/client-subs", json!({"name": "排除", "rule": {"team_mode": "except", "team_ids": [a, b]}})).await;
    let only = t.post("/api/v1/client-subs", json!({"name": "只要2", "rule": {"team_mode": "only", "team_ids": [b]}})).await;
    let (s, v) = t.req("DELETE", &format!("/api/v1/teams/{b}"), None, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let ids: Vec<(String, Vec<Uuid>)> = sqlx::query_as("select name, team_ids from client_sub order by name").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(ids, [("只要2".to_string(), vec![]), ("排除".to_string(), vec![a])]);
    let d: serde_json::Value = sqlx::query_scalar("select detail from audit_log where action = 'team.delete'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(d["client_subs"], json!(["只要2", "排除"]), "{d}");
    // 前端整份带 rule 保存：用读回来的规则
    let sub = t.get(&format!("/api/v1/client-subs/{}", except["id"].as_str().unwrap())).await;
    assert_eq!(sub["rule"]["team_ids"], json!([a]));
    let (s, v) = t.req("PATCH", &format!("/api/v1/client-subs/{}", except["id"].as_str().unwrap()), Some(json!({"name": "排除2", "rule": sub["rule"]})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let sub = t.get(&format!("/api/v1/client-subs/{}", only["id"].as_str().unwrap())).await;
    assert_eq!(sub["rule"]["team_ids"], json!([]));
    t.fail("PATCH", &format!("/api/v1/client-subs/{}", only["id"].as_str().unwrap()), Some(json!({"rule": sub["rule"]})), StatusCode::BAD_REQUEST).await;
    let (s, v) = t.req("PATCH", &format!("/api/v1/client-subs/{}", only["id"].as_str().unwrap()), Some(json!({"rule": {"team_mode": "only", "team_ids": [a]}})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// 只改名 / 启停不碰规则列：load 之后别处改了 team_ids（并发的删团队摘团队）不会被 patch 用旧值写回
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_name_keeps_rule_columns() {
    let t = common::boot(false, "").await;
    let (a, b) = (tele::seed_team(&t, "团队1").await, tele::seed_team(&t, "团队2").await);
    let d = t.post("/api/v1/client-subs", json!({"name": "旧名", "rule": {"team_mode": "only", "team_ids": [a, b]}})).await;
    let id = d["id"].as_str().unwrap();
    sqlx::query("update client_sub set team_ids = $1").bind(vec![a]).execute(&t.st.db).await.unwrap();
    let (s, v) = t.req("PATCH", &format!("/api/v1/client-subs/{id}"), Some(json!({"name": "新名", "enabled": false})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["data"]["name"].as_str(), v["data"]["enabled"].as_bool()), (Some("新名"), Some(false)), "{v}");
    let ids: Vec<Uuid> = sqlx::query_scalar("select unnest(team_ids) from client_sub").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(ids, [a], "team_ids 不被覆盖");
    // 带规则时校验团队存在
    t.fail("PATCH", &format!("/api/v1/client-subs/{id}"), Some(json!({"rule": {"team_mode": "only", "team_ids": [Uuid::now_v7()]}})), StatusCode::BAD_REQUEST).await;
}

/// 批量取分享链接：一次查回、按请求顺序（重复的各出一条），有一条不存在整批 404，只写一条审计
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn share_batch_keeps_request_order() {
    let t = common::boot(false, "").await;
    seed(&t).await;
    let ids: Vec<Uuid> = sqlx::query_scalar("select id from entry order by port").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(ids.len(), 3);
    let want = [ids[2], ids[0], ids[2]];
    let v = t.post("/api/v1/entries/share", json!({"ids": want})).await;
    let got: Vec<&str> = v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert_eq!(got, want.map(|i| i.to_string()), "{v}");
    assert!(v["items"][0]["url"].as_str().unwrap().starts_with("vless://"), "{v}");
    assert_eq!(v["items"][0], v["items"][2], "{v}");
    assert_ne!(v["items"][0]["name"], v["items"][1]["name"], "{v}");
    t.fail("POST", "/api/v1/entries/share", Some(json!({"ids": [ids[0], Uuid::now_v7()]})), StatusCode::NOT_FOUND).await;
    let n: i64 = sqlx::query_scalar("select count(*) from audit_log where action = 'entry.share'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 1);
}

/// AnyTLS：期望状态给每个启用入口带 anytls_port（入口端口 + 20000）；订阅 protocols=both 时，线路机报了 sing-box 在跑才出 AnyTLS 条目
/// （Clash 的 type: anytls、base64 的 anytls://），分享链接同样；只要 anytls 的订阅只出 AnyTLS 条目；默认（reality）不变
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anytls_in_desired_subs_and_share() {
    let t = common::boot(false, "trust_forwarded_for = true").await;
    seed(&t).await;
    // 节点要有出站参数才进期望状态（AnyTLS 端口按进期望状态的入口算）
    sqlx::query("insert into node_outbound (machine_id, protocol, host, port, method, secret_enc) select id, 'shadowsocks', host(primary_ip), 8388, 'aes-256-gcm', $1 from machine where kind = 'proxy'")
        .bind(t.st.secrets.seal_str("pw"))
        .execute(&t.st.db)
        .await
        .unwrap();
    let (s, v) = t.req("GET", "/agent/v1/desired?after=0", None, Some("tok1")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let ports: Vec<(u64, Option<u64>)> = v["entries"].as_array().unwrap().iter().map(|e| (e["port"].as_u64().unwrap(), e["anytls_port"].as_u64())).collect();
    assert_eq!(ports, [(20000, Some(40000)), (20001, Some(40001)), (20002, Some(40002))]);
    let both = t.post("/api/v1/client-subs", json!({"name": "两种", "protocols": "both"})).await;
    let only = t.post("/api/v1/client-subs", json!({"name": "只 AnyTLS", "protocols": "anytls"})).await;
    let plain = t.post("/api/v1/client-subs", json!({"name": "默认"})).await;
    assert_eq!((both["protocols"].as_str(), plain["protocols"].as_str()), (Some("both"), Some("reality")));
    t.fail("POST", "/api/v1/client-subs", Some(json!({"name": "x", "protocols": "hy2"})), StatusCode::BAD_REQUEST).await;
    let (bp, op, pp) = (sub_path(&both["url"]), sub_path(&only["url"]), sub_path(&plain["url"]));
    let ids: Vec<Uuid> = sqlx::query_scalar("select id from entry order by port").fetch_all(&t.st.db).await.unwrap();

    // 线路机还没报 sing-box 在跑：一条 AnyTLS 都不出，分享链接也没有
    let (_, _, y) = pull(&t, &bp).await;
    assert!(!y.contains("anytls") && y.contains("type: vless"), "{y}");
    assert_eq!(pull(&t, &op).await.2, "proxies: []\n");
    assert!(t.get(&format!("/api/v1/entries/{}/share", ids[0])).await["anytls_url"].is_null());

    // 报 sing-box 在跑
    let mut r = tele::blank_report(1);
    r.singbox = Some(fleet_proto::SingboxStatus { installed: true, running: true, version: Some("1.14.4".into()), last_error: None });
    let (s, v) = t.req("POST", "/agent/v1/report", Some(serde_json::to_value(&r).unwrap()), Some("tok1")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let relay: Uuid = sqlx::query_scalar("select machine_id from relay").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(t.get(&format!("/api/v1/machines/{relay}")).await["agent"]["anytls"]["running"], true);
    let (pw, sni): (Uuid, String) = sqlx::query_as("select e.client_uuid, r.reality_sni from entry e join relay r on r.machine_id = e.relay_id where e.port = 20000")
        .fetch_one(&t.st.db)
        .await
        .unwrap();
    let (_, _, y) = pull(&t, &bp).await;
    let block = format!(
        "  - name: \"家宽 · 团队1-01 · 203.0.113.5 · AnyTLS\"\n    type: anytls\n    server: \"relay.test\"\n    port: 40000\n    password: \"{pw}\"\n    sni: \"{sni}\"\n    skip-cert-verify: true\n    client-fingerprint: chrome\n    udp: true\n"
    );
    assert!(y.contains(&block), "{y}");
    // 同一入口 Reality 在前、AnyTLS 紧跟；备用团队的不出
    assert_eq!((y.matches("type: vless").count(), y.matches("type: anytls").count()), (2, 2), "{y}");
    assert!(y.find("· 203.0.113.5 · VLESS Reality\"").unwrap() < y.find(&block).unwrap() && !y.contains("40001"), "{y}");
    let (_, _, b) = pull(&t, &format!("{bp}?format=base64")).await;
    let lines = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(b.trim()).unwrap()).unwrap();
    let at: Vec<&str> = lines.lines().filter(|l| l.starts_with("anytls://")).collect();
    assert_eq!((lines.lines().filter(|l| l.starts_with("vless://")).count(), at.len()), (2, 2), "{lines}");
    assert!(lines.contains(&format!("anytls://{pw}@relay.test:40000/?sni={sni}&insecure=1#%E5%AE%B6%E5%AE%BD%20%C2%B7%20")), "{lines}");
    // 只要 AnyTLS：只有 AnyTLS 条目；默认的不变
    let (_, _, y) = pull(&t, &op).await;
    assert_eq!((y.matches("type: vless").count(), y.matches("type: anytls").count()), (0, 2), "{y}");
    assert!(!pull(&t, &pp).await.2.contains("anytls"));
    let show = t.get(&format!("/api/v1/client-subs/{}", both["id"].as_str().unwrap())).await;
    assert_eq!(show["entries"].as_array().unwrap().iter().filter_map(|e| e["anytls_port"].as_u64()).collect::<Vec<_>>(), [40002, 40000], "{show}");
    // 分享链接同时给 AnyTLS；改协议
    let sh = t.get(&format!("/api/v1/entries/{}/share", ids[0])).await;
    assert!(sh["url"].as_str().unwrap().starts_with("vless://") && sh["anytls_url"].as_str().unwrap().starts_with(&format!("anytls://{pw}@relay.test:40000/")), "{sh}");
    let (s, v) = t.req("PATCH", &format!("/api/v1/client-subs/{}", plain["id"].as_str().unwrap()), Some(json!({"protocols": "both"})), None).await;
    assert_eq!((s, v["data"]["protocols"].as_str()), (StatusCode::OK, Some("both")), "{v}");
    // sing-box 停了：又只剩 Reality
    r.seq = 2;
    r.singbox = Some(fleet_proto::SingboxStatus { installed: true, running: false, version: None, last_error: Some("x".into()) });
    t.req("POST", "/agent/v1/report", Some(serde_json::to_value(&r).unwrap()), Some("tok1")).await;
    assert!(!pull(&t, &bp).await.2.contains("anytls"));
}

/// 线路机的入口协议（PATCH /relays/{id}）：改成只 AnyTLS 的三条 409（AnyTLS 没在跑 / agent 不认、有穿透入口、有软路由出口指着），
/// 改成后下发 reality_public=false、分享与订阅只给 AnyTLS；改成只 Reality 后不再下发、不再给 AnyTLS
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_protocols_switch() {
    let t = common::boot(false, "trust_forwarded_for = true").await;
    seed(&t).await;
    sqlx::query("insert into node_outbound (machine_id, protocol, host, port, method, secret_enc) select id, 'shadowsocks', host(primary_ip), 8388, 'aes-256-gcm', $1 from machine where kind = 'proxy'")
        .bind(t.st.secrets.seal_str("pw"))
        .execute(&t.st.db)
        .await
        .unwrap();
    let relay: Uuid = sqlx::query_scalar("select machine_id from relay").fetch_one(&t.st.db).await.unwrap();
    let path = format!("/api/v1/relays/{relay}");
    let fail = |b: serde_json::Value| { let (t, path) = (&t, path.clone()); async move { t.req("PATCH", &path, Some(b), None).await } };
    let anytls = json!({"protocols": "anytls"});
    let (s, v) = fail(anytls.clone()).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("先升级 agent"), "{v}");
    let report = |seq: u64, caps: Vec<&str>| {
        let mut r = tele::blank_report(seq);
        r.singbox = Some(fleet_proto::SingboxStatus { installed: true, running: true, version: Some("1.14.4".into()), last_error: None });
        r.caps = caps.into_iter().map(String::from).collect();
        serde_json::to_value(&r).unwrap()
    };
    // sing-box 在跑但 agent 不认 reality_public：仍 409
    assert_eq!(t.req("POST", "/agent/v1/report", Some(report(1, vec!["reverse"])), Some("tok1")).await.0, StatusCode::OK);
    let (s, v) = fail(anytls.clone()).await;
    assert!(s == StatusCode::CONFLICT && v["error"]["message"].as_str().unwrap().contains("版本旧"), "{v}");
    assert_eq!(t.req("POST", "/agent/v1/report", Some(report(2, fleet_proto::CAPS.to_vec())), Some("tok1")).await.0, StatusCode::OK);
    // 有穿透入口
    sqlx::query("update entry set mode = 'reverse' where port = 20000").execute(&t.st.db).await.unwrap();
    let (s, v) = fail(anytls.clone()).await;
    assert!(s == StatusCode::CONFLICT && v["error"]["message"].as_str().unwrap().contains("穿透入口：团队1-01（20000）"), "{v}");
    sqlx::query("update entry set mode = 'normal' where port = 20000").execute(&t.st.db).await.unwrap();
    // 有软路由出口指着这台线路机上的入口
    let (eid, nid): (Uuid, Uuid) = sqlx::query_as("select id, node_id from entry where port = 20000").fetch_one(&t.st.db).await.unwrap();
    let rid = Uuid::now_v7();
    sqlx::query("insert into router (id, name, tunnel_port) values ($1, '团队1-路由A', 23006)").bind(rid).execute(&t.st.db).await.unwrap();
    let entry = json!({"id": eid, "name": "团队1-01 · 线路机1:20000", "node_id": nid, "relay_id": relay, "node_name": "团队1-01", "relay_name": "线路机1", "port": 20000, "lifecycle": "active"});
    let snap = json!({"exits": [{"name": "节点01", "entry": entry, "sync": "ok", "linked": true}]});
    sqlx::query("insert into router_status (router_id, ts, reachable, data, last_ok_at) values ($1, now(), true, $2, now())").bind(rid).bind(snap).execute(&t.st.db).await.unwrap();
    let (s, v) = fail(anytls.clone()).await;
    assert!(s == StatusCode::CONFLICT && v["error"]["message"].as_str().unwrap().contains("团队1-路由A 的 节点01"), "{v}");
    sqlx::query("delete from router_status").execute(&t.st.db).await.unwrap();
    t.fail("PATCH", &path, Some(json!({"protocols": "vmess"})), StatusCode::BAD_REQUEST).await;

    // 改成只 AnyTLS：代数 +1、审计；下发 reality_public=false
    let g0: i64 = sqlx::query_scalar("select generation from relay").fetch_one(&t.st.db).await.unwrap();
    let (s, v) = fail(anytls.clone()).await;
    assert_eq!((s, v["data"]["protocols"].as_str(), v["data"]["generation"].as_i64()), (StatusCode::OK, Some("anytls"), Some(g0 + 1)), "{v}");
    let n: i64 = sqlx::query_scalar("select count(*) from audit_log where action = 'relay.protocols'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 1);
    assert_eq!(t.get(&format!("/api/v1/machines/{relay}")).await["relay"]["protocols"], "anytls");
    let (_, d) = t.req("GET", "/agent/v1/desired?after=0", None, Some("tok1")).await;
    assert_eq!(d["relay"]["reality_public"], false, "{d}");
    assert!(d["entries"].as_array().unwrap().iter().all(|e| e["anytls_port"].is_u64()), "{d}");
    // 分享只给 AnyTLS；订阅：both 只出 AnyTLS，只要 Reality 的整条不出
    let sh = t.get(&format!("/api/v1/entries/{eid}/share")).await;
    assert!(sh["url"].is_null() && sh["anytls_url"].as_str().unwrap().starts_with("anytls://"), "{sh}");
    let both = t.post("/api/v1/client-subs", json!({"name": "两种", "protocols": "both"})).await;
    let plain = t.post("/api/v1/client-subs", json!({"name": "默认"})).await;
    let (_, _, y) = pull(&t, &sub_path(&both["url"])).await;
    assert_eq!((y.matches("type: vless").count(), y.matches("type: anytls").count()), (0, 2), "{y}");
    assert_eq!(pull(&t, &sub_path(&plain["url"])).await.2, "proxies: []\n");
    assert_eq!(plain["items"], 0, "{plain}");

    // 改成只 Reality：不下发 AnyTLS 端口、Reality 对公网开，分享只给 Reality
    let (s, _) = fail(json!({"protocols": "reality"})).await;
    assert_eq!(s, StatusCode::OK);
    let (_, d) = t.req("GET", "/agent/v1/desired?after=0", None, Some("tok1")).await;
    assert!(d["relay"].get("reality_public").is_none() && d["entries"].as_array().unwrap().iter().all(|e| e.get("anytls_port").is_none()), "{d}");
    let sh = t.get(&format!("/api/v1/entries/{eid}/share")).await;
    assert!(sh["url"].as_str().unwrap().starts_with("vless://") && sh["anytls_url"].is_null(), "{sh}");
}
