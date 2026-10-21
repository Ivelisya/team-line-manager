//! 运维接口：API 密钥与 scope、登录限速、远程执行的前置校验。
//! 需要 PostgreSQL（见 common_ops.rs）。用 `--test-threads=1` 运行。

#[path = "common_ops.rs"]
mod common_ops;

use axum::http::StatusCode;
use common_ops::{setup, Auth, ADMIN_PW};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn api_key_scopes_and_revoke() {
    let t = setup().await;
    let (rid, read_tok) = t.make_key("ci-read", &["read"]).await;

    // 列表不泄露 token / 哈希，且带前缀
    let keys = t.ok("GET", "/api/v1/api-keys", None).await;
    let k = &keys.as_array().unwrap()[0];
    assert_eq!(k["name"], "ci-read");
    assert!(read_tok.starts_with(&format!("flk_{}_", k["prefix"].as_str().unwrap())));
    assert!(k.get("token").is_none() && k.get("key_hash").is_none());

    // read：GET 成功，且不需要 X-Fleet-Request
    let (s, _) = t.call("GET", "/api/v1/machines", None, Auth::Key(&read_tok)).await;
    assert_eq!(s, StatusCode::OK);
    // read 调写接口 403；调 share 403（先于 404）；调 exec / 日志 403；管理密钥 403
    let (s, _) = t.call("POST", "/api/v1/teams", Some(json!({"name": "x"})), Auth::Key(&read_tok)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let eid = Uuid::now_v7();
    let (s, _) = t.call("GET", &format!("/api/v1/entries/{eid}/share"), None, Auth::Key(&read_tok)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = t.call("POST", &format!("/api/v1/machines/{eid}/exec"), Some(json!({"command": "id"})), Auth::Key(&read_tok)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = t.call("GET", "/api/v1/server/logs", None, Auth::Key(&read_tok)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = t.call("GET", "/api/v1/api-keys", None, Auth::Key(&read_tok)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = t.call("POST", "/api/v1/api-keys", Some(json!({"name": "evil", "scopes": ["read"]})), Auth::Key(&read_tok)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = t.call("DELETE", &format!("/api/v1/api-keys/{rid}"), None, Auth::Key(&read_tok)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // secrets：能过 scope 这一关（入口不存在 → 404），但没有 read 不能读列表
    let (_, sec_tok) = t.make_key("ci-secrets", &["secrets"]).await;
    let (s, _) = t.call("GET", &format!("/api/v1/entries/{eid}/share"), None, Auth::Key(&sec_tok)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = t.call("GET", "/api/v1/machines", None, Auth::Key(&sec_tok)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // write：不带 X-Fleet-Request 也能写
    let (_, w_tok) = t.make_key("ci-write", &["write"]).await;
    let (s, v) = t.call("POST", "/api/v1/teams", Some(json!({"name": "团队9"})), Auth::Key(&w_tok)).await;
    assert!(s.is_success(), "{s} {v}");

    // exec：过 scope 后对不存在的机器 404
    let (_, x_tok) = t.make_key("ci-exec", &["exec"]).await;
    let (s, _) = t.call("POST", &format!("/api/v1/machines/{eid}/exec"), Some(json!({"command": "id"})), Auth::Key(&x_tok)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // 参数校验
    let (s, _) = t.call("POST", "/api/v1/api-keys", Some(json!({"name": "bad", "scopes": ["root"]})), Auth::Session).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = t.call("POST", "/api/v1/api-keys", Some(json!({"name": "bad", "scopes": []})), Auth::Session).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // 撤销后 401；重复撤销幂等；last_used_at 已被记录
    let (s, _) = t.call("DELETE", &format!("/api/v1/api-keys/{rid}"), None, Auth::Session).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = t.call("GET", "/api/v1/machines", None, Auth::Key(&read_tok)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = t.call("DELETE", &format!("/api/v1/api-keys/{rid}"), None, Auth::Session).await;
    assert_eq!(s, StatusCode::OK);
    let keys = t.ok("GET", "/api/v1/api-keys", None).await;
    let r = keys.as_array().unwrap().iter().find(|k| k["id"] == rid.as_str()).unwrap();
    assert!(!r["revoked_at"].is_null() && !r["last_used_at"].is_null());

    // 审计：创建与撤销都有；密钥自己的写操作记在 apikey:<名> 名下
    let audit = t.ok("GET", "/api/v1/audit?limit=100", None).await;
    let acts: Vec<String> = audit.as_array().unwrap().iter().map(|a| format!("{}|{}", a["actor"].as_str().unwrap(), a["action"].as_str().unwrap())).collect();
    assert!(acts.contains(&"admin|apikey.create".to_string()), "{acts:?}");
    assert!(acts.contains(&"admin|apikey.revoke".to_string()), "{acts:?}");
    assert!(acts.contains(&"apikey:ci-write|team.create".to_string()), "{acts:?}");
    // 审计里绝不出现明文 token
    assert!(!audit.to_string().contains(&read_tok));
}

/// 并发的错误密码一起涌进来：最多 5 次真正校验密码，其余被限速
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_rate_limit_holds_under_concurrency() {
    let t = setup().await;
    let res = futures::future::join_all((0..12).map(|_| t.login("admin", "wrong-password"))).await;
    let n401 = res.iter().filter(|(s, _)| *s == StatusCode::UNAUTHORIZED).count();
    let n429 = res.iter().filter(|(s, _)| *s == StatusCode::TOO_MANY_REQUESTS).count();
    // 同时在判断的两条互相看得见，可能多拦一次（只会更严，不会放过第 6 次）
    assert!((4..=5).contains(&n401) && n401 + n429 == 12, "{res:?}");
    let (n,): (i64,) = sqlx::query_as("select count(*) from login_failure where username = 'admin'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, n401 as i64, "被限速的尝试不留记录");
}

#[tokio::test]
async fn login_rate_limit_and_audit() {
    let t = setup().await;
    for i in 0..5 {
        let (s, v) = t.login("admin", "wrong-password").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "第 {i} 次：{v}");
    }
    // 第 6 次：即使密码正确也被限速
    let (s, v) = t.login("admin", ADMIN_PW).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("分钟") || msg.contains("秒"), "{msg}");
    assert!(v["error"]["detail"]["retry_after_s"].as_i64().unwrap() > 0);
    // 别的用户名不受影响
    let (s, _) = t.login("someone-else", "x").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // 窗口过后（把失败记录挪到 16 分钟前）可以再登，成功后清零
    sqlx::query("update login_failure set ts = now() - interval '16 minutes'").execute(&t.st.db).await.unwrap();
    let (s, _) = t.login("admin", ADMIN_PW).await;
    assert_eq!(s, StatusCode::OK);
    let (n,): (i64,) = sqlx::query_as("select count(*) from login_failure where username = 'admin'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 0);

    // 登出写审计
    let (s, _) = t.call("POST", "/api/v1/auth/logout", None, Auth::Session).await;
    assert_eq!(s, StatusCode::OK);
    let acts: Vec<(String,)> = sqlx::query_as("select action from audit_log where action like 'auth.%' order by id").fetch_all(&t.st.db).await.unwrap();
    let acts: Vec<String> = acts.into_iter().map(|a| a.0).collect();
    assert_eq!(acts.iter().filter(|a| *a == "auth.login_failed").count(), 6, "{acts:?}");
    assert!(acts.contains(&"auth.login".to_string()) && acts.contains(&"auth.logout".to_string()), "{acts:?}");
}

#[tokio::test]
async fn exec_preconditions() {
    let t = setup().await;
    let missing = Uuid::now_v7();
    let (s, _) = t.call("POST", &format!("/api/v1/machines/{missing}/exec"), Some(json!({"command": "id"})), Auth::Session).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = t.call("GET", &format!("/api/v1/machines/{missing}/logs"), None, Auth::Session).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // 已退役 → 409
    let retired = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle, retired_at) values ($1, 'relay', '线路机7', '198.51.100.7', 'retired', now())")
        .bind(retired)
        .execute(&t.st.db)
        .await
        .unwrap();
    let (s, v) = t.call("POST", &format!("/api/v1/machines/{retired}/exec"), Some(json!({"command": "id"})), Auth::Session).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, _) = t.call("GET", &format!("/api/v1/machines/{retired}/logs?unit=fleet-agent"), None, Auth::Session).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // 上游代理没有 SSH → 409
    let team = t.ok("POST", "/api/v1/teams", Some(json!({"name": "团队1"}))).await;
    let tid: Uuid = team["id"].as_str().unwrap().parse().unwrap();
    let proxy = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, lifecycle) values ($1, 'proxy', $2, 1, '198.51.100.8', 'active')")
        .bind(proxy)
        .bind(tid)
        .execute(&t.st.db)
        .await
        .unwrap();
    let (s, _) = t.call("POST", &format!("/api/v1/machines/{proxy}/exec"), Some(json!({"command": "id"})), Auth::Session).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // 参数校验在碰机器之前
    for (body, why) in [
        (json!({"command": "   "}), "空命令"),
        (json!({"command": "id", "timeout_s": 0}), "超时为 0"),
        (json!({"command": "id", "timeout_s": 601}), "超时过大"),
    ] {
        let (s, _) = t.call("POST", &format!("/api/v1/machines/{retired}/exec"), Some(body), Auth::Session).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{why}");
    }
    let (s, _) = t.call("GET", &format!("/api/v1/machines/{retired}/logs?unit=sshd"), None, Auth::Session).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn last_used_updates_are_throttled_to_once_a_minute() {
    let t = setup().await;
    let (kid, tok) = t.make_key("ci-throttle", &["read"]).await;
    let age = |sql: &'static str, id: String| {
        let db = t.st.db.clone();
        async move { sqlx::query_scalar::<_, f64>(sql).bind(id).fetch_one(&db).await.unwrap() }
    };
    let key_age = || age("select extract(epoch from now() - last_used_at)::float8 from api_key where id::text = $1", kid.to_string());
    let (s, _) = t.call("GET", "/api/v1/machines", None, Auth::Key(&tok)).await;
    assert_eq!(s, StatusCode::OK);
    assert!(key_age().await < 5.0, "第一次使用要记录");
    // 10 秒前用过：一分钟内不重写；两分钟前用过：重写
    sqlx::query("update api_key set last_used_at = now() - interval '10 seconds' where id::text = $1").bind(kid.to_string()).execute(&t.st.db).await.unwrap();
    assert_eq!(t.call("GET", "/api/v1/machines", None, Auth::Key(&tok)).await.0, StatusCode::OK);
    assert!(key_age().await > 9.0);
    sqlx::query("update api_key set last_used_at = now() - interval '2 minutes' where id::text = $1").bind(kid.to_string()).execute(&t.st.db).await.unwrap();
    assert_eq!(t.call("GET", "/api/v1/machines", None, Auth::Key(&tok)).await.0, StatusCode::OK);
    assert!(key_age().await < 5.0);
    // 会话同理
    sqlx::query("update session set last_seen_at = now() - interval '10 seconds'").execute(&t.st.db).await.unwrap();
    assert_eq!(t.call("GET", "/api/v1/machines", None, Auth::Session).await.0, StatusCode::OK);
    let n: i64 = sqlx::query_scalar("select count(*) from session where last_seen_at < now() - interval '9 seconds'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 1);
    sqlx::query("update session set last_seen_at = now() - interval '2 minutes'").execute(&t.st.db).await.unwrap();
    assert_eq!(t.call("GET", "/api/v1/machines", None, Auth::Session).await.0, StatusCode::OK);
    let n: i64 = sqlx::query_scalar("select count(*) from session where last_seen_at > now() - interval '5 seconds'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 1);
}
