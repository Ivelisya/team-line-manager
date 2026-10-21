//! 运维接口：API 密钥与 scope、登录限速、远程执行的前置校验。
//! 需要 PostgreSQL（见 common）。

mod common;
mod common_ops;

use axum::http::StatusCode;
use common_ops::{setup, Auth, ADMIN_PW};
use serde_json::{json, Value};
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

/// 输入错误统一 400 且是 JSON：坏 uuid / 坏查询参数 / 枚举值不对 / 引用不存在的对象
#[tokio::test]
async fn bad_input_is_json_400() {
    let t = setup().await;
    for path in ["/api/v1/machines/not-a-uuid", "/api/v1/jobs?limit=abc", "/api/v1/audit?before_id=x"] {
        let (s, v) = t.call("GET", path, None, Auth::Session).await;
        assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("bad_request")), "{path}: {v}");
    }
    let p = t.ok("POST", "/api/v1/providers", Some(json!({"name": "ATTVPS", "code": "ATT"}))).await;
    let (s, v) = t.call("POST", "/api/v1/plans", Some(json!({"provider_id": p["id"], "name": "基础版", "price_cents": 500, "cycle": "weekly"})), Auth::Session).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = t.call("POST", "/api/v1/plans", Some(json!({"provider_id": Uuid::now_v7(), "name": "基础版", "price_cents": 500})), Auth::Session).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("不存在"), "{v}");
    let plan = t.ok("POST", "/api/v1/plans", Some(json!({"provider_id": p["id"], "name": "基础版", "price_cents": 500}))).await;
    let (s, v) = t.call("PATCH", &format!("/api/v1/plans/{}", plan["id"].as_str().unwrap()), Some(json!({"provider_id": Uuid::now_v7()})), Auth::Session).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
}

/// 远程执行与查日志不重试：ssh 返回 255（这里是连接被拒）原样返回，不会隔 2 秒再跑一遍。
/// 255 也可能是命令自己 exit 255 或执行中断开（reboot），重试会把用户命令执行多次。
/// 执行前先落一条审计，执行完在同一条上补结果
#[tokio::test]
async fn exec_and_logs_are_not_retried() {
    let t = setup().await;
    let id = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, label, primary_ip, ssh_port, lifecycle) values ($1, 'relay', '线路机3', '127.0.0.1', 1, 'active')")
        .bind(id)
        .execute(&t.st.db)
        .await
        .unwrap();
    let t0 = std::time::Instant::now();
    let (s, v) = t.call("POST", &format!("/api/v1/machines/{id}/exec"), Some(json!({"command": "echo hi", "timeout_s": 5})), Auth::Session).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["exit_code"], 255, "{v}");
    assert!(t0.elapsed() < std::time::Duration::from_secs(2), "连不上时不重试：{:?}", t0.elapsed());
    let rows: Vec<(Value,)> = sqlx::query_as("select detail from audit_log where action = 'exec' and target_id = $1").bind(id).fetch_all(&t.st.db).await.unwrap();
    assert_eq!(rows.len(), 1, "同一次执行只有一条审计");
    let d = &rows[0].0;
    assert_eq!(d["command"], "echo hi");
    assert!(d.get("started").is_none() && (d["error"].is_string() || d["exit_code"].is_number()), "{d}");
    let t0 = std::time::Instant::now();
    let (s, v) = t.call("GET", &format!("/api/v1/machines/{id}/logs?unit=fleet-agent"), None, Auth::Session).await;
    assert_eq!((s, v["data"]["exit_code"].as_i64()), (StatusCode::OK, Some(255)), "{v}");
    assert!(t0.elapsed() < std::time::Duration::from_secs(2), "查日志同样不重试：{:?}", t0.elapsed());
}

#[tokio::test]
async fn audit_paging_and_command_redaction() {
    let t = setup().await;
    let long = format!("export PW=hunter2 {}\nsecond line", "x".repeat(100));
    sqlx::query("insert into audit_log (actor, action, target_kind, detail) values ('admin', 'exec', 'machine', $1)")
        .bind(json!({"name": "线路机2", "command": long, "exit_code": 0}))
        .execute(&t.st.db)
        .await
        .unwrap();
    for i in 0..3 {
        t.ok("POST", "/api/v1/teams", Some(json!({"name": format!("团队{i}")}))).await;
    }
    // 翻页：before_id 只要更早的
    let p1 = t.ok("GET", "/api/v1/audit?limit=2", None).await;
    let p1 = p1.as_array().unwrap();
    assert_eq!(p1.len(), 2);
    let last = p1[1]["id"].as_i64().unwrap();
    let p2 = t.ok("GET", &format!("/api/v1/audit?limit=100&before_id={last}"), None).await;
    let p2 = p2.as_array().unwrap();
    assert!(!p2.is_empty() && p2.iter().all(|a| a["id"].as_i64().unwrap() < last));
    // 会话看完整命令；没有 exec 的密钥只看到字数，摘要里也没有命令
    let all = t.ok("GET", "/api/v1/audit?limit=100", None).await;
    let full = all.as_array().unwrap().iter().find(|a| a["action"] == "exec").unwrap();
    assert_eq!(full["detail"]["command"], long.as_str());
    assert!(full["summary"].as_str().unwrap().contains("hunter2"));
    let (_, tok) = t.make_key("ci-read", &["read"]).await;
    let (s, v) = t.call("GET", "/api/v1/audit?limit=100", None, Auth::Key(&tok)).await;
    assert_eq!(s, StatusCode::OK);
    let e = v["data"].as_array().unwrap().iter().find(|a| a["action"] == "exec").unwrap().clone();
    let hidden = format!("命令已隐藏（{} 字）", long.chars().count());
    assert_eq!(e["detail"]["command"], hidden.as_str());
    let sum = e["summary"].as_str().unwrap();
    assert!(sum.contains(&hidden) && !sum.contains("hunter2"), "{sum}");
}

#[tokio::test]
async fn ack_incident_and_silence_validation() {
    let t = setup().await;
    let mid = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle) values ($1, 'relay', '线路机2', '198.51.100.2', 'active')")
        .bind(mid)
        .execute(&t.st.db)
        .await
        .unwrap();
    let inc = Uuid::now_v7();
    sqlx::query("insert into incident (id, rule, subject_id, severity, summary) values ($1, 'agent_offline', $2, 'critical', 'agent 5 分钟没上报')")
        .bind(inc)
        .bind(mid)
        .execute(&t.st.db)
        .await
        .unwrap();
    let (s, _) = t.call("POST", &format!("/api/v1/incidents/{}/ack", Uuid::now_v7()), None, Auth::Session).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    t.ok("POST", &format!("/api/v1/incidents/{inc}/ack"), None).await;
    let (s, v) = t.call("POST", &format!("/api/v1/incidents/{inc}/ack"), None, Auth::Session).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let n: i64 = sqlx::query_scalar("select count(*) from audit_log where action = 'incident.ack' and target_id = $1").bind(inc).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 1);

    let win = |extra: Value| {
        let mut b = json!({"starts_at": "2026-01-01T00:00:00Z", "ends_at": "2026-01-02T00:00:00Z"});
        b.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        b
    };
    let (s, _) = t.call("POST", "/api/v1/silences", Some(win(json!({"rule": "no_such_rule"}))), Auth::Session).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = t.call("POST", "/api/v1/silences", Some(win(json!({"subject_id": Uuid::now_v7()}))), Auth::Session).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    t.ok("POST", "/api/v1/silences", Some(win(json!({"rule": "agent_offline", "subject_id": mid})))).await;
}

/// 批量升级 agent：一台冲突（建作业时撞上别人刚建的作业）只跳过这台
#[tokio::test]
async fn upgrade_all_skips_conflicting_machine() {
    let t = setup().await;
    let ids: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
    for (i, id) in ids.iter().enumerate() {
        sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle) values ($1, 'relay', $2, $3::inet, 'active')")
            .bind(id)
            .bind(format!("线路机{}", i + 2))
            .bind(format!("198.51.100.{}", i + 2))
            .execute(&t.st.db)
            .await
            .unwrap();
    }
    // 模拟并发：给第二台建作业时报唯一约束冲突
    let ddl = format!(
        "create function fail_job() returns trigger language plpgsql as $$ begin
           if new.target_id = '{}' then raise exception 'dup' using errcode = 'unique_violation'; end if; return new; end $$",
        ids[1]
    );
    sqlx::query(&ddl).execute(&t.st.db).await.unwrap();
    sqlx::query("create trigger fail_job before insert on job for each row execute function fail_job()").execute(&t.st.db).await.unwrap();
    let d = t.ok("POST", "/api/v1/agents/upgrade", Some(json!({}))).await;
    assert_eq!(d["jobs"].as_array().unwrap().len(), 2, "{d}");
    assert_eq!(d["skipped"][0]["machine_id"], ids[1].to_string(), "{d}");
    let n: i64 = sqlx::query_scalar("select count(*) from job where kind = 'upgrade_agent'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 2);
}
