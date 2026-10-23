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
