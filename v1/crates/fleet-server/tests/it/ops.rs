//! 运维接口：API 密钥与 scope、中控本机的全权密钥（local-admin）、登录限速、远程执行的前置校验与 SSH 登录审计；前端静态文件的 404 与缓存头。
//! 需要 PostgreSQL（见 common）。

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::ops::{setup, Auth, ADMIN_PW};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
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

/// 限速按来源分开：别人用错误密码锁不住这个用户从自己那里登录；IPv6 同一 /64 算一个来源；
/// 同一来源 20 次（不分用户名）、同一用户名 100 次（不分来源）另有上限；user-set 清掉失败记录
#[tokio::test]
async fn login_rate_limit_per_source() {
    let t = common::boot(false, "trust_forwarded_for = true").await;
    for i in 0..5 {
        let (s, v) = t.login_from("admin", "wrong-password", "203.0.113.7").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "第 {i} 次：{v}");
    }
    // 第 6 次：即使密码正确也被限速，提示里有等多久
    let (s, v) = t.login_from("admin", ADMIN_PW, "203.0.113.7").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert!(v["error"]["detail"]["retry_after_s"].as_i64().unwrap() > 0, "{v}");
    assert_eq!(t.login_from("admin", ADMIN_PW, "198.51.100.9").await.0, StatusCode::OK, "别的来源照常登录");
    // 登录成功只清本来源的失败：那边照样封着
    assert_eq!(t.login_from("admin", ADMIN_PW, "203.0.113.7").await.0, StatusCode::TOO_MANY_REQUESTS);
    // 窗口过后（失败记录挪到 16 分钟前）可以再登，成功后清零
    sqlx::query("update login_failure set ts = now() - interval '16 minutes'").execute(&t.st.db).await.unwrap();
    assert_eq!(t.login_from("admin", ADMIN_PW, "203.0.113.7").await.0, StatusCode::OK);
    let n: i64 = sqlx::query_scalar("select count(*) from login_failure where ip = '203.0.113.7'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 0);

    // IPv6：同一 /64 里换地址也算同一来源；库里存 /64 前缀，审计记完整地址
    for i in 1..=5 {
        assert_eq!(t.login_from("admin", "wrong-password", &format!("2001:db8:1:2::{i}")).await.0, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(t.login_from("admin", ADMIN_PW, "2001:db8:1:2:ffff::9").await.0, StatusCode::TOO_MANY_REQUESTS);
    let n: i64 = sqlx::query_scalar("select count(*) from login_failure where ip = '2001:db8:1:2::'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 5);
    let last: String = sqlx::query_scalar("select detail->>'ip' from audit_log where action = 'auth.login_failed' order by id desc limit 1").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(last, "2001:db8:1:2::5");
    assert_eq!(t.login_from("admin", ADMIN_PW, "2001:db8:1:3::1").await.0, StatusCode::OK);

    // 同一来源 20 次（别的用户名）
    sqlx::query("insert into login_failure (username, ip) select 'u' || g, '192.0.2.1' from generate_series(1, 20) g").execute(&t.st.db).await.unwrap();
    assert_eq!(t.login_from("admin", ADMIN_PW, "192.0.2.1").await.0, StatusCode::TOO_MANY_REQUESTS);
    // 同一用户名 100 次（各自不同的来源）
    sqlx::query("insert into login_failure (username, ip) select 'admin', ('10.9.' || g / 256 || '.' || g % 256)::inet from generate_series(1, 100) g")
        .execute(&t.st.db)
        .await
        .unwrap();
    let (s, v) = t.login_from("admin", ADMIN_PW, "198.51.100.10").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{v}");
    fleet_server::auth::upsert_user(&t.st.db, "admin", ADMIN_PW).await.unwrap();
    assert_eq!(t.login_from("admin", ADMIN_PW, "198.51.100.10").await.0, StatusCode::OK, "user-set 后马上能登");
}

/// 攻击者换 20 个 /64 各错 5 次凑满「同一用户名 100 次」：只拦没登录成功过的来源，本人 30 天内登录成功过的来源照常能登
#[tokio::test]
async fn login_global_cap_spares_known_sources() {
    let t = common::boot(false, "trust_forwarded_for = true").await;
    assert_eq!(t.login_from("admin", ADMIN_PW, "198.51.100.20").await.0, StatusCode::OK);
    for n in 0..20 {
        for i in 1..=5 {
            let (s, v) = t.login_from("admin", "wrong-password", &format!("2001:db8:0:{n:x}::{i}")).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{n}/{i}：{v}");
        }
    }
    assert_eq!(t.login_from("admin", ADMIN_PW, "203.0.113.50").await.0, StatusCode::TOO_MANY_REQUESTS, "没登录成功过的来源被总上限拦住");
    assert_eq!(t.login_from("admin", ADMIN_PW, "198.51.100.20").await.0, StatusCode::OK, "登录成功过的来源不受总上限");
    // 超过 30 天没再登录成功：不再豁免
    sqlx::query("update login_source set last_ok_at = now() - interval '31 days'").execute(&t.st.db).await.unwrap();
    assert_eq!(t.login_from("admin", ADMIN_PW, "198.51.100.20").await.0, StatusCode::TOO_MANY_REQUESTS);
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

/// 目录：套餐字段校验 400；改套餐 / 团队的审计记改了什么；供应商库里的旧控制台地址（http(s) 校验之前存的）原样带回不拦其它字段
#[tokio::test]
async fn catalog_validation_and_audit_details() {
    let t = setup().await;
    let p = t.ok("POST", "/api/v1/providers", Some(json!({"name": "ATTVPS", "code": "ATT"}))).await;
    let pid = p["id"].as_str().unwrap();
    for (body, why) in [
        (json!({"price_cents": -1}), "负价格"),
        (json!({"price_cents": 1, "currency": "US$"}), "币种只能是字母"),
        (json!({"price_cents": 1, "currency": "ABCDEFGHI"}), "币种过长"),
        (json!({"price_cents": 1, "currency": "USDT"}), "币种不是 3 个字母"),
        (json!({"price_cents": 1, "cycle": "weekly"}), "不认识的计费周期"),
        (json!({"price_cents": 1, "notes": "备".repeat(2001)}), "备注过长"),
        (json!({"price_cents": 1, "name": "名".repeat(33)}), "名字过长"),
    ] {
        let mut b = json!({"provider_id": pid, "name": "基础版"});
        b.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
        let (s, v) = t.call("POST", "/api/v1/plans", Some(b), Auth::Session).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{why}：{v}");
    }
    let plan = t.ok("POST", "/api/v1/plans", Some(json!({"provider_id": pid, "name": "基础版", "price_cents": 500, "currency": " cny "}))).await;
    let path = format!("/api/v1/plans/{}", plan["id"].as_str().unwrap());
    for body in [json!({"price_cents": -5}), json!({"currency": ""}), json!({"notes": "备".repeat(2001)})] {
        let (s, v) = t.call("PATCH", &path, Some(body.clone()), Auth::Session).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}：{v}");
    }
    t.ok("PATCH", &path, Some(json!({"price_cents": 600, "notes": "改价", "currency": "usd", "name": "基础版"}))).await;
    let db = &t.st.db;
    let audit = |action: &'static str| async move {
        sqlx::query_scalar::<_, Value>("select detail from audit_log where action = $1 order by id desc limit 1").bind(action).fetch_one(db).await.unwrap()
    };
    let d = audit("plan.update").await;
    assert_eq!(d["before"], json!({"price_cents": 500, "notes": "", "currency": "CNY"}), "{d}");
    assert_eq!(d["after"], json!({"price_cents": 600, "notes": "改价", "currency": "USD"}), "{d}");

    let team = t.ok("POST", "/api/v1/teams", Some(json!({"name": "团队1"}))).await;
    t.ok("PATCH", &format!("/api/v1/teams/{}", team["id"].as_str().unwrap()), Some(json!({"sort_order": 5}))).await;
    let d = audit("team.update").await;
    assert_eq!((d["before"]["sort_order"].as_i64(), d["after"]["sort_order"].as_i64()), (Some(0), Some(5)), "{d}");

    sqlx::query("update provider set panel_url = 'panel.example.com'").execute(&t.st.db).await.unwrap();
    let ppath = format!("/api/v1/providers/{pid}");
    t.ok("PATCH", &ppath, Some(json!({"name": "ATTVPS 2", "panel_url": "panel.example.com"}))).await;
    let (s, v) = t.call("PATCH", &ppath, Some(json!({"panel_url": "javascript:alert(1)"})), Auth::Session).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
}

/// fleetctl provider / plan 用 read + write 的 API 密钥发的请求体（见 fleetctl catalog.rs）：
/// 新建带全部可选项、改时 none = null（时区改回跟随组织、清空控制台地址、清空规格）、归档 / 取消归档、删除；不存在的 404，还有套餐的供应商删不掉
#[tokio::test]
async fn catalog_as_fleetctl_sends_it() {
    let t = setup().await;
    let (_, tok) = t.make_key("cli", &["read", "write"]).await;
    let call = |m: &'static str, p: String, b: Option<Value>| {
        let t = &t;
        let tok = tok.clone();
        async move { t.call(m, &p, b, Auth::Key(&tok)).await }
    };
    let (s, v) = call("POST", "/api/v1/providers".into(), Some(json!({"name": "ATTVPS", "code": "ATT", "timezone": "Asia/Shanghai", "panel_url": "https://p.example.com",
        "traffic_rule": "min", "traffic_reset": "billing", "remind_days": 5}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let pid = v["data"]["id"].as_str().unwrap().to_string();
    let provider = |all: &Value| all["data"].as_array().unwrap().iter().find(|p| p["id"] == pid.as_str()).cloned().unwrap();
    let p = provider(&call("GET", "/api/v1/providers".into(), None).await.1);
    assert_eq!((&p["timezone"], &p["traffic_rule"], &p["traffic_reset"], &p["remind_days"], &p["panel_url"]), (&json!("Asia/Shanghai"), &json!("min"), &json!("billing"), &json!(5), &json!("https://p.example.com")));
    let (s, v) = call("PATCH", format!("/api/v1/providers/{pid}"), Some(json!({"timezone": null, "panel_url": null, "remind_days": 9}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let p = provider(&call("GET", "/api/v1/providers".into(), None).await.1);
    assert_eq!((p["timezone"].is_null(), p["panel_url"].is_null(), &p["remind_days"], &p["code"]), (true, true, &json!(9), &json!("ATT")), "没给的不改");

    let (s, v) = call("POST", "/api/v1/plans".into(), Some(json!({"name": "基础版", "provider_id": pid, "cpu_cores": 2, "mem_mb": 2048, "traffic_gb": 0,
        "price_cents": 29999, "currency": "CNY", "cycle": "monthly"}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let plan = v["data"]["id"].as_str().unwrap().to_string();
    let path = format!("/api/v1/plans/{plan}");
    let plan_of = |all: &Value| all["data"].as_array().unwrap().iter().find(|p| p["id"] == plan.as_str()).cloned().unwrap();
    let (s, v) = call("PATCH", path.clone(), Some(json!({"mem_mb": null, "price_cents": 30999, "archived": true}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let p = plan_of(&call("GET", "/api/v1/plans".into(), None).await.1);
    assert_eq!((p["mem_mb"].is_null(), &p["cpu_cores"], &p["price_cents"], p["archived_at"].is_string()), (true, &json!(2), &json!(30999), true));
    call("PATCH", path.clone(), Some(json!({"archived": false}))).await;
    assert!(plan_of(&call("GET", "/api/v1/plans".into(), None).await.1)["archived_at"].is_null());

    for (m, p) in [("PATCH", format!("/api/v1/plans/{}", Uuid::now_v7())), ("DELETE", format!("/api/v1/plans/{}", Uuid::now_v7())), ("PATCH", format!("/api/v1/providers/{}", Uuid::now_v7()))] {
        let (s, v) = call(m, p.clone(), Some(json!({"name": "x"}))).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{m} {p}：{v}");
    }
    let (s, v) = call("DELETE", format!("/api/v1/providers/{pid}"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("1 个套餐"), "{v}");
    assert_eq!(call("DELETE", path, None).await.0, StatusCode::OK);
    assert_eq!(call("DELETE", format!("/api/v1/providers/{pid}"), None).await.0, StatusCode::OK);
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

/// 操作记录按分类、搜索（标题、摘要、操作人）筛选，与 before_id 翻页一起用
#[tokio::test]
async fn audit_filter_by_cat_and_search() {
    let t = setup().await;
    // 登录等建库时的记录不算
    sqlx::query("delete from audit_log").execute(&t.st.db).await.unwrap();
    for (actor, action, detail) in [
        ("admin", "exec", json!({"name": "线路机2", "command": "echo hunter2_x", "exit_code": 0})),
        // ua 不在摘要里：搜它不该命中这一条
        ("admin", "auth.login", json!({"ip": "203.0.113.7", "ua": "团队1 的浏览器"})),
        ("apikey:ci-bot", "agent.upgrade", json!({"count": 3})),
    ] {
        fleet_server::db::audit_insert(&t.st.db, actor, action, "machine", None, detail).await.unwrap();
    }
    for i in 0..3 {
        t.ok("POST", "/api/v1/teams", Some(json!({"name": format!("团队{i}")}))).await;
    }
    // 查询串里的中文按 UTF-8 百分号编码
    let get = |q: &'static str| {
        let t = &t;
        let q: String = q.bytes().map(|b| if b.is_ascii() { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
        async move { t.ok("GET", &format!("/api/v1/audit?limit=100&{q}"), None).await.as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap().to_string()).collect::<Vec<_>>() }
    };
    assert_eq!(get("cat=exec").await, ["exec"]);
    assert_eq!(get("cat=auth").await, ["auth.login"]);
    assert_eq!(get("cat=machine").await, ["agent.upgrade"]);
    assert_eq!(get("cat=config").await, ["team.create"; 3]);
    assert_eq!(get("cat=all&q=").await.len(), 6, "all 与空搜索等于没筛");
    // 标题、摘要（只搜摘要里看得到的，detail 里不展示的值不算）、操作人（界面写法）
    assert_eq!(get("q=新建团队").await.len(), 3);
    assert_eq!(get("q=团队1").await, ["team.create"]);
    assert_eq!(get("q=浏览器").await, Vec::<String>::new());
    assert_eq!(get("q=203.0.113").await, ["auth.login"]);
    assert_eq!(get("q=API%20密钥").await, ["agent.upgrade"]);
    assert_eq!(get("cat=config&q=hunter2").await, Vec::<String>::new());
    // _ 与 % 按字面匹配
    assert_eq!(get("q=hunter2_").await, ["exec"]);
    assert_eq!(get("q=%25").await, Vec::<String>::new());
    // 翻页：分类筛选后按 before_id 往前翻
    let p1 = t.ok("GET", "/api/v1/audit?cat=config&limit=2", None).await;
    let last = p1[1]["id"].as_i64().unwrap();
    let p2 = t.ok("GET", &format!("/api/v1/audit?cat=config&limit=2&before_id={last}"), None).await;
    assert_eq!((p2.as_array().unwrap().len(), p2[0]["summary"].as_str()), (1, Some("团队0")));
    let (s, _) = t.call("GET", "/api/v1/audit?cat=x", None, Auth::Session).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // 没有 exec 权限的密钥看不到命令，也不能靠搜索试出命令
    let (_, tok) = t.make_key("ci-read", &["read"]).await;
    let (s, v) = t.call("GET", "/api/v1/audit?q=hunter2", None, Auth::Key(&tok)).await;
    assert_eq!((s, v["data"].as_array().unwrap().len()), (StatusCode::OK, 0), "{v}");
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
    sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary) values ($1, 'machine', 'agent_offline', $2, 'critical', 'agent 5 分钟没上报')")
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

    // 窗口按现在算（默认从现在起 1 小时）：结束时间已经过去的会被拒
    let (f, now) = (time::format_description::well_known::Rfc3339, time::OffsetDateTime::now_utc());
    let (h, d) = (time::Duration::hours, time::Duration::days);
    let at = |off: time::Duration| (now + off).format(&f).unwrap();
    let win = |extra: Value| {
        let mut b = json!({"starts_at": at(h(0)), "ends_at": at(h(1))});
        b.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        b
    };
    for (extra, want) in [
        (json!({"rule": "no_such_rule"}), "没有「no_such_rule」这条告警规则"),
        (json!({"subject_id": Uuid::now_v7()}), "subject_id 指向的机器不存在"),
        (json!({"ends_at": at(h(0))}), "结束时间要晚于开始时间"),
        (json!({"ends_at": at(d(30) + h(1))}), "最长 30 天；更久的请直接把机器退役"),
        (json!({"starts_at": at(-h(2)), "ends_at": at(-h(1))}), "结束时间已经过去了"),
        (json!({"starts_at": at(d(91)), "ends_at": at(d(91) + h(1))}), "最多提前 90 天预约"),
        (json!({"rule": "agent_offline", "is_maintenance": true}), "维护窗口针对整台机器，不能只选一条规则；只想静默这条告警就不要标记为维护"),
        (json!({"comment": "长".repeat(2001)}), "备注最长 2000 个字"),
    ] {
        let (s, v) = t.call("POST", "/api/v1/silences", Some(win(extra.clone())), Auth::Session).await;
        assert_eq!((s, v["error"]["message"].as_str()), (StatusCode::BAD_REQUEST, Some(want)), "{extra}");
    }
    // 开始时间已经过去、结束时间还没到的照常建（「从现在开始」按客户端的时钟填）；整 30 天、89 天后开始的都可以
    let cur = t.ok("POST", "/api/v1/silences", Some(win(json!({"rule": "agent_offline", "subject_id": mid, "starts_at": at(-h(1))})))).await["id"].clone();
    t.ok("POST", "/api/v1/silences", Some(win(json!({"starts_at": at(d(89)), "ends_at": at(d(119))})))).await;
    // 预约：还没开始的照样列出，按开始时间排在进行中的后面；但此刻不生效（不算维护中）
    let pre = t.ok("POST", "/api/v1/silences", Some(win(json!({"subject_id": mid, "is_maintenance": true, "starts_at": at(d(1)), "ends_at": at(d(1) + h(2))})))).await["id"].clone();
    let list = t.ok("GET", "/api/v1/silences", None).await;
    let ids: Vec<Value> = list.as_array().unwrap().iter().map(|s| s["id"].clone()).collect();
    assert_eq!(ids[..2], [cur, pre], "{list}");
    assert_eq!((list[0]["rule_name"].as_str(), list[1]["rule_name"].is_null()), (Some("agent 离线"), true), "{list}");
    let live = fleet_server::silence::Silences::load(&t.st.db).await.unwrap();
    assert!(live.matches("agent_offline", mid) && !live.maintenance(mid), "预约的维护还没开始");

    // 审计：新建、删除都写明对象、维护还是静默、起止（北京时间）与备注
    let pre = ids[1].as_str().unwrap();
    t.ok("DELETE", &format!("/api/v1/silences/{pre}"), None).await;
    let log = t.ok("GET", &format!("/api/v1/audit?target_id={pre}"), None).await;
    let bj = |off| fleet_server::meta::org_time(now + off).chars().take(16).collect::<String>();
    let want = format!("{} · 维护窗口 · {} → {}", list[1]["subject_name"].as_str().unwrap(), bj(d(1)), bj(d(1) + h(2)));
    let got: Vec<(&str, &str)> = log.as_array().unwrap().iter().map(|e| (e["action"].as_str().unwrap(), e["summary"].as_str().unwrap())).collect();
    assert_eq!(got, [("silence.delete", want.as_str()), ("silence.create", want.as_str())], "{log}");
}

/// 单台机器详情走缓存视图时要比对 touched：缓存里已有一份视图（列表请求算出来的），开维护之后马上取详情，看到的是维护中，不是缓存里没有维护的旧状态
#[tokio::test]
async fn machine_detail_sees_maintenance_right_after_silence() {
    let t = setup().await;
    let mid = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle) values ($1, 'relay', '线路机2', '198.51.100.2', 'active')")
        .bind(mid)
        .execute(&t.st.db)
        .await
        .unwrap();
    // 列表请求把视图缓存起来
    let list = t.get("/api/v1/machines").await;
    assert_eq!(list[0]["maintenance"], json!(false), "{list}");
    // 直接写库再发 touched（同 `Event::Incidents` 对视图缓存的作用）：走接口会同时唤醒后台告警评估，它读视图时顺手补了缓存，测不出详情自己的比对
    sqlx::query("insert into silence (id, machine_id, starts_at, ends_at, is_maintenance, created_by) values ($1, $2, now() - interval '1 minute', now() + interval '1 hour', true, 'u')")
        .bind(Uuid::now_v7())
        .bind(mid)
        .execute(&t.st.db)
        .await
        .unwrap();
    t.st.live.touched();
    // 不走 `t.get`：它读单台机器前会先作废视图缓存，那样就测不到详情自己的比对了
    let (s, one) = t.req("GET", &format!("/api/v1/machines/{mid}"), None, None).await;
    let one = &one["data"];
    assert_eq!((s, one["maintenance"].clone(), one["health"]["status"].clone()), (StatusCode::OK, json!(true), json!("maintenance")), "{one}");
}

/// 前端静态文件：/assets/ 下缺的文件（发布后旧页面懒加载的旧 chunk）回 404、不带一年缓存，不回 index.html；
/// 其它未知路径照旧回 index.html
#[tokio::test]
async fn missing_assets_are_404() {
    let dir = tempfile::tempdir().unwrap();
    let web = dir.path().join("web");
    std::fs::create_dir_all(web.join("assets")).unwrap();
    std::fs::write(web.join("index.html"), "<!doctype html>").unwrap();
    std::fs::write(web.join("assets/index-new.js"), "export {}").unwrap();
    std::fs::write(web.join("theme-init.js"), "try {} catch (e) {}").unwrap();
    let web = web.display().to_string().replace('\\', "/");
    let t = common::boot_in(dir, false, &format!("web_dir = \"{web}\"")).await;
    for (path, status, cache, body) in [
        ("/assets/index-new.js", StatusCode::OK, Some("public, max-age=31536000, immutable"), "export {}"),
        ("/assets/index-old.js", StatusCode::NOT_FOUND, None, ""),
        ("/machines/x", StatusCode::OK, Some("no-cache"), "<!doctype html>"),
        // public/ 下的主题脚本（index.html 同步加载）：按文件给出，不是 index.html
        ("/theme-init.js", StatusCode::OK, Some("no-cache"), "try {} catch (e) {}"),
    ] {
        let res = t.app.clone().oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap();
        let cc = res.headers().get("cache-control").map(|v| v.to_str().unwrap().to_string());
        assert_eq!((res.status(), cc.as_deref()), (status, cache), "{path}");
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(String::from_utf8_lossy(&bytes), body, "{path}");
    }
}

/// 安全头：所有响应 nosniff / same-origin / DENY；前端页面带 CSP，接口不带；接口默认 no-store；http 部署不带 HSTS
#[tokio::test]
async fn security_headers() {
    let dir = tempfile::tempdir().unwrap();
    let web = dir.path().join("web");
    std::fs::create_dir_all(&web).unwrap();
    std::fs::write(web.join("index.html"), "<!doctype html>").unwrap();
    let web = web.display().to_string().replace('\\', "/");
    let t = common::boot_in(dir, false, &format!("web_dir = \"{web}\"")).await;
    for (path, page) in [("/machines/x", true), ("/api/v1/machines", false), ("/api/v1/nope", false), ("/agent/v1/report", false)] {
        let mut req = Request::get(path);
        if path.starts_with("/api/") {
            req = req.header("cookie", &t.cookie);
        }
        let res = t.app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let h = |k: &str| res.headers().get(k).map(|v| v.to_str().unwrap().to_string());
        assert_eq!((h("x-content-type-options"), h("referrer-policy"), h("x-frame-options")), (Some("nosniff".into()), Some("same-origin".into()), Some("DENY".into())), "{path}");
        assert_eq!(h("content-security-policy").is_some_and(|c| c.contains("script-src 'self';") && c.contains("frame-ancestors 'none'")), page, "{path}");
        assert_eq!(h("strict-transport-security"), None);
        if path.starts_with("/api/") {
            assert_eq!(h("cache-control").as_deref(), Some("no-store"), "{path}");
        }
    }
}

/// 续费记录记下登记时订阅的币种：之后改订阅币种，历史金额不跟着变；币种不是 3 个字母的订阅写不进库
#[tokio::test]
async fn renewal_keeps_currency_snapshot() {
    let t = setup().await;
    let team = common::seed_team(&t, "团队1").await;
    let m = common::seed_node(&t, team, 1, "203.0.113.10", "active").await;
    let sub = |cur: &str| json!({"price_cents": 500, "currency": cur, "due_date": "2027-01-01"});
    t.ok("PUT", &format!("/api/v1/machines/{m}/subscription"), Some(sub("CNY"))).await;
    t.ok("POST", &format!("/api/v1/machines/{m}/renewals"), Some(json!({"new_due": "2027-02-01", "amount_cents": 500}))).await;
    t.ok("PUT", &format!("/api/v1/machines/{m}/subscription"), Some(sub("USD"))).await;
    let rs = t.ok("GET", &format!("/api/v1/machines/{m}/renewals"), None).await;
    assert_eq!((rs[0]["amount_cents"].clone(), rs[0]["currency"].clone()), (json!(500), json!("CNY")), "{rs}");
    let (s, v) = t.call("PUT", &format!("/api/v1/machines/{m}/subscription"), Some(sub("USDT")), Auth::Session).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
}

/// root 的 fleetctl 配置里的令牌
fn cfg_token(p: &std::path::Path) -> (String, String) {
    let v: toml::Value = toml::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    (v["url"].as_str().unwrap().to_string(), v["token"].as_str().unwrap().to_string())
}

/// 有效的 central-local（id、权限、是否不过期）
async fn live_local_keys(t: &common::T) -> Vec<(Uuid, Vec<String>, bool)> {
    sqlx::query_as("select id, scopes, expires_at is null from api_key where name = 'central-local' and revoked_at is null")
        .fetch_all(&t.st.db)
        .await
        .unwrap()
}

/// 中控本机的全权密钥（fleet-server local-admin）：第一次建，权限是全部 scope、不过期；再跑不换（部署每次都跑，干活的 agent 不能半路失效）；
/// listen 的端口改了只改写地址；--rotate 换新的、旧的立即失效。令牌只进 0600 的配置文件，不出现在输出与审计里
#[tokio::test]
async fn local_admin_creates_keeps_and_rotates() {
    use fleet_server::local_admin::ensure;
    let t = setup().await;
    let cfg = t.dir.path().join("root/.config/fleetctl/config.toml");
    let url = "http://127.0.0.1:8080";

    let o = ensure(&t.st.db, url, &cfg, false).await.unwrap();
    assert!(o.created && o.revoked == 0);
    let (u, tok) = cfg_token(&cfg);
    assert_eq!(u, url);
    assert!(tok.starts_with(&format!("flk_{}_", o.prefix)) && !o.summary().contains(&tok), "{}", o.summary());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let (s, me) = t.call("GET", "/api/v1/auth/me", None, Auth::Key(&tok)).await;
    assert_eq!((s, me["data"]["username"].as_str()), (StatusCode::OK, Some("apikey:central-local")), "{me}");
    assert_eq!(me["data"]["scopes"], json!(["read", "write", "secrets", "exec"]));
    let keys = live_local_keys(&t).await;
    assert_eq!(keys.len(), 1);
    assert!(keys[0].2, "不过期");

    // 再跑：还是这一把，配置不动
    let o = ensure(&t.st.db, url, &cfg, false).await.unwrap();
    assert!(!o.created && o.summary().contains("仍有效"), "{}", o.summary());
    assert_eq!(cfg_token(&cfg), (url.to_string(), tok.clone()));
    // listen 端口改了：令牌照旧，只改地址
    let o = ensure(&t.st.db, "http://127.0.0.1:9090", &cfg, false).await.unwrap();
    assert!(!o.created);
    assert_eq!(cfg_token(&cfg), ("http://127.0.0.1:9090".to_string(), tok.clone()));

    // --rotate：新令牌，旧的立即 401，有效的始终只有一把
    let o = ensure(&t.st.db, url, &cfg, true).await.unwrap();
    assert!(o.created && o.revoked == 1, "{}", o.summary());
    let (_, tok2) = cfg_token(&cfg);
    assert_ne!(tok2, tok);
    assert_eq!(t.call("GET", "/api/v1/auth/me", None, Auth::Key(&tok)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(t.call("GET", "/api/v1/machines", None, Auth::Key(&tok2)).await.0, StatusCode::OK);
    assert_eq!(live_local_keys(&t).await.len(), 1);

    // 审计：操作者 cli，摘要写明是中控命令行建 / 撤销的；令牌不出现
    let audit = t.ok("GET", "/api/v1/audit?limit=20", None).await;
    let rows: Vec<String> = audit.as_array().unwrap().iter().map(|a| format!("{}|{}|{}", a["actor"].as_str().unwrap(), a["action"].as_str().unwrap(), a["summary"].as_str().unwrap())).collect();
    assert_eq!(rows.iter().filter(|r| r.starts_with("cli|apikey.create|central-local（") && r.ends_with("· 中控命令行 local-admin")).count(), 2, "{rows:?}");
    assert_eq!(rows.iter().filter(|r| r.starts_with("cli|apikey.revoke|central-local（")).count(), 1, "{rows:?}");
    assert!(!audit.to_string().contains(&tok) && !audit.to_string().contains(&tok2));
}

/// local-admin 修复：配置文件丢了、密钥在网页上被撤销、权限不全（以后加了 scope）时重建；网页 / 接口不能建同名密钥
#[tokio::test]
async fn local_admin_repairs_and_reserves_name() {
    use fleet_server::local_admin::ensure;
    let t = setup().await;
    let cfg = t.dir.path().join("fleetctl/config.toml");
    let url = "http://127.0.0.1:8080";
    ensure(&t.st.db, url, &cfg, false).await.unwrap();

    // 配置文件丢了：旧的撤销、建新的
    std::fs::remove_file(&cfg).unwrap();
    let o = ensure(&t.st.db, url, &cfg, false).await.unwrap();
    assert!(o.created && o.revoked == 1);
    let (_, tok) = cfg_token(&cfg);
    assert_eq!(t.call("GET", "/api/v1/machines", None, Auth::Key(&tok)).await.0, StatusCode::OK);

    // 用户在网页上撤销了它：下次运行重建（撤销过的不再算）
    let id = live_local_keys(&t).await[0].0;
    assert_eq!(t.call("DELETE", &format!("/api/v1/api-keys/{id}"), None, Auth::Session).await.0, StatusCode::OK);
    let o = ensure(&t.st.db, url, &cfg, false).await.unwrap();
    assert!(o.created && o.revoked == 0);
    let (_, tok) = cfg_token(&cfg);
    assert_eq!(t.call("GET", "/api/v1/machines", None, Auth::Key(&tok)).await.0, StatusCode::OK);

    // 权限不全（旧版本建的、后来加了 scope）：换一把全的
    sqlx::query("update api_key set scopes = '{read,write}' where name = 'central-local' and revoked_at is null").execute(&t.st.db).await.unwrap();
    let o = ensure(&t.st.db, url, &cfg, false).await.unwrap();
    assert!(o.created && o.revoked == 1);
    let keys = live_local_keys(&t).await;
    assert_eq!((keys.len(), keys[0].1.len()), (1, 4), "{keys:?}");

    // 名字留给 local-admin：审计里 apikey:central-local 只可能是中控本机
    let (s, v) = t.call("POST", "/api/v1/api-keys", Some(json!({"name": "central-local", "scopes": ["read"], "password": ADMIN_PW})), Auth::Session).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("local-admin"), "{v}");
}

/// fleetctl ssh 登录前报的审计（POST /audit/ssh）：要 exec 权限；对象名按库里的写，带没带命令照记、命令内容不记；
/// 不存在 404，已退役、家宽代理 409（同 exec），kind 不对 400
#[tokio::test]
async fn ssh_login_audit() {
    let t = setup().await;
    let team = t.ok("POST", "/api/v1/teams", Some(json!({"name": "团队1"}))).await;
    let tid: Uuid = team["id"].as_str().unwrap().parse().unwrap();
    let node = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, ssh_port, lifecycle) values ($1, 'vps', $2, 3, '198.51.100.3', 2222, 'active')")
        .bind(node)
        .bind(tid)
        .execute(&t.st.db)
        .await
        .unwrap();
    let (_, rw_tok) = t.make_key("ci-rw", &["read", "write"]).await;
    let (_, x_tok) = t.make_key("ci-exec", &["exec"]).await;
    let login = |id: Uuid, command: Option<bool>| {
        let mut b = json!({"kind": "machine", "id": id});
        if let Some(c) = command {
            b["command"] = json!(c);
        }
        b
    };
    let (s, _) = t.call("POST", "/api/v1/audit/ssh", Some(login(node, Some(true))), Auth::Key(&rw_tok)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "没有 exec 的密钥伪造不了登录记录");
    let (s, v) = t.call("POST", "/api/v1/audit/ssh", Some(login(node, Some(true))), Auth::Key(&x_tok)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = t.call("POST", "/api/v1/audit/ssh", Some(login(node, None)), Auth::Key(&x_tok)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rows: Vec<(String, String, Value)> = sqlx::query_as("select actor, target_kind, detail from audit_log where action = 'ssh.login' and target_id = $1 order by id")
        .bind(node)
        .fetch_all(&t.st.db)
        .await
        .unwrap();
    let want = |c: bool| ("apikey:ci-exec".to_string(), "machine".to_string(), json!({"name": "团队1-03", "command": c}));
    assert_eq!(rows, [want(true), want(false)]);
    let audit = t.ok("GET", &format!("/api/v1/audit?target_id={node}"), None).await;
    let sums: Vec<String> = audit.as_array().unwrap().iter().map(|a| format!("{}|{}", a["title"].as_str().unwrap(), a["summary"].as_str().unwrap())).collect();
    assert_eq!(sums, ["SSH 登录|团队1-03 · 交互式", "SSH 登录|团队1-03 · 带命令（内容不记录）"]);

    let retired = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle, retired_at) values ($1, 'relay', '线路机7', '198.51.100.7', 'retired', now())")
        .bind(retired)
        .execute(&t.st.db)
        .await
        .unwrap();
    let proxy = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, lifecycle) values ($1, 'proxy', $2, 4, '198.51.100.8', 'active')")
        .bind(proxy)
        .bind(tid)
        .execute(&t.st.db)
        .await
        .unwrap();
    for (body, want, why) in [
        (login(Uuid::now_v7(), None), StatusCode::NOT_FOUND, "不存在"),
        (login(retired, None), StatusCode::CONFLICT, "已退役"),
        (login(proxy, None), StatusCode::CONFLICT, "家宽代理"),
        (json!({"kind": "vm", "id": node}), StatusCode::BAD_REQUEST, "kind 不对"),
        (json!({"kind": "machine", "id": "团队1-03"}), StatusCode::BAD_REQUEST, "id 不是 uuid"),
    ] {
        let (s, v) = t.call("POST", "/api/v1/audit/ssh", Some(body), Auth::Key(&x_tok)).await;
        assert_eq!(s, want, "{why}：{v}");
    }
    let n: i64 = sqlx::query_scalar("select count(*) from audit_log where action = 'ssh.login'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 2, "被拒的不记");
}
