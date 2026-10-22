//! 账户安全：两步验证（启用、登录、重放、恢复码、停用、重新生成、CLI 重置）、登录会话、改密码。
//! 需要 PostgreSQL（见 common）。

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::ops::{setup, Auth, ADMIN_PW, T};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// 某个时间步的验证码
fn code(secret: &[u8], step: i64) -> String { format!("{:06}", fleet_server::totp::hotp(secret, step as u64, 6)) }

fn login_body(pw: &str, totp: Option<&str>) -> Value { json!({"username": "admin", "password": pw, "totp": totp}) }

async fn login(t: &T, pw: &str, totp: Option<&str>) -> (StatusCode, Value) {
    t.call("POST", "/api/v1/auth/login", Some(login_body(pw, totp)), Auth::None).await
}

/// 带 User-Agent 与来源 IP 登录，返回 Cookie
async fn login_cookie(t: &T, ua: &str, ip: &str) -> String {
    let req = Request::post("/api/v1/auth/login")
        .header("content-type", "application/json")
        .header("user-agent", ua)
        .header("x-forwarded-for", ip)
        .body(Body::from(login_body(ADMIN_PW, None).to_string()))
        .unwrap();
    let res = t.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string()
}

/// 带验证码、从某个来源登录
async fn login_from(t: &T, pw: &str, totp: Option<&str>, ip: &str) -> (StatusCode, Value) {
    let req = Request::post("/api/v1/auth/login")
        .header("content-type", "application/json")
        .header("x-forwarded-for", ip)
        .body(Body::from(login_body(pw, totp).to_string()))
        .unwrap();
    let res = t.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// 用指定 Cookie 调接口
async fn call_as(t: &T, cookie: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_from(t, cookie, "", method, path, body).await
}

/// 用指定 Cookie、从某个来源（空 = 不带 X-Forwarded-For）调接口
async fn call_from(t: &T, cookie: &str, ip: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path).header("content-type", "application/json").header("x-fleet-request", "1").header("cookie", cookie);
    if !ip.is_empty() {
        b = b.header("x-forwarded-for", ip);
    }
    let req = b.body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty)).unwrap();
    let res = t.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn failures(t: &T) -> i64 {
    sqlx::query_scalar("select count(*) from login_failure where username = 'admin'").fetch_one(&t.st.db).await.unwrap()
}

async fn actions(t: &T) -> Vec<String> {
    sqlx::query_scalar("select action from audit_log where action like 'auth.%' order by id").fetch_all(&t.st.db).await.unwrap()
}

/// 开启两步验证，返回 (密钥, 确认时用的时间步, 恢复码)
async fn enable(t: &T) -> (Vec<u8>, i64, Vec<String>) {
    let s = t.ok("POST", "/api/v1/account/totp/setup", Some(json!({"password": ADMIN_PW}))).await;
    let secret = data_encoding::BASE32_NOPAD.decode(s["secret"].as_str().unwrap().as_bytes()).unwrap();
    let step = fleet_server::totp::now_step();
    let d = t.ok("POST", "/api/v1/account/totp/enable", Some(json!({"code": code(&secret, step)}))).await;
    let codes = d["recovery_codes"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
    (secret, step, codes)
}

#[tokio::test]
async fn totp_enable_login_recovery_disable() {
    let t = setup().await;
    assert_eq!(t.ok("GET", "/api/v1/account/totp", None).await["enabled"], false);

    // 设置要当前密码（偷到 Cookie 的人不能绑自己的认证器）：不对 401，不生成密钥
    let (st, v) = t.call("POST", "/api/v1/account/totp/setup", Some(json!({"password": "nope"})), Auth::Session).await;
    assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("bad_password")), "{v}");
    let pending: Option<Vec<u8>> = sqlx::query_scalar("select totp_pending_enc from app_user where username = 'admin'").fetch_one(&t.st.db).await.unwrap();
    assert!(pending.is_none());
    // 设置：二维码与 otpauth 链接；库里只有密文
    let s = t.ok("POST", "/api/v1/account/totp/setup", Some(json!({"password": ADMIN_PW}))).await;
    let b32 = s["secret"].as_str().unwrap();
    assert!(s["otpauth_url"].as_str().unwrap().contains(&format!("secret={b32}")), "{s}");
    assert!(s["qr_svg"].as_str().unwrap().contains("<svg"));
    let secret = data_encoding::BASE32_NOPAD.decode(b32.as_bytes()).unwrap();
    let enc: Vec<u8> = sqlx::query_scalar("select totp_pending_enc from app_user where username = 'admin'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(t.st.secrets.open(&enc).unwrap(), secret);
    // 码不对不启用
    let n = fleet_server::totp::now_step();
    let wrong = format!("{:06}", (fleet_server::totp::hotp(&secret, n as u64, 6) + 1) % 1_000_000);
    let (st, v) = t.call("POST", "/api/v1/account/totp/enable", Some(json!({"code": wrong})), Auth::Session).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let d = t.ok("POST", "/api/v1/account/totp/enable", Some(json!({"code": code(&secret, n)}))).await;
    let codes: Vec<String> = d["recovery_codes"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
    assert_eq!(codes.len(), 10);
    let stt = t.ok("GET", "/api/v1/account/totp", None).await;
    assert_eq!((stt["enabled"].as_bool(), stt["recovery_codes_left"].as_i64()), (Some(true), Some(10)));
    let (st, _) = t.call("POST", "/api/v1/account/totp/setup", Some(json!({"password": ADMIN_PW})), Auth::Session).await;
    assert_eq!(st, StatusCode::CONFLICT, "已启用时不能再设置");

    // 登录：没给码 → totp_required（只在密码对时才这么说；照样计一次，免得知道密码的人不限次数地占用算哈希的名额）；
    // 密码错仍是 bad_credentials
    let (st, v) = login(&t, ADMIN_PW, None).await;
    assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("totp_required")), "{v}");
    assert_eq!(failures(&t).await, 1);
    let (st, v) = login(&t, "wrong-password", Some(&code(&secret, n + 1))).await;
    assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("bad_credentials")));
    // 确认时用过的那一步不能再用（重放），算一次失败
    let (st, v) = login(&t, ADMIN_PW, Some(&code(&secret, n))).await;
    assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("totp_invalid")), "{v}");
    assert_eq!(failures(&t).await, 3);
    let (st, v) = login(&t, ADMIN_PW, Some(&code(&secret, n + 1))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(failures(&t).await, 0, "成功后清掉本来源的失败");
    assert_eq!(login(&t, ADMIN_PW, Some(&code(&secret, n + 1))).await.0, StatusCode::UNAUTHORIZED, "同一个码只能用一次");

    // 恢复码：一次性，大小写与分隔不敏感
    assert_eq!(login(&t, ADMIN_PW, Some(&codes[0])).await.0, StatusCode::OK);
    assert_eq!(login(&t, ADMIN_PW, Some(&codes[0])).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(login(&t, ADMIN_PW, Some(&codes[1].to_uppercase().replace('-', " "))).await.0, StatusCode::OK);
    assert_eq!(t.ok("GET", "/api/v1/account/totp", None).await["recovery_codes_left"], 8);

    // 重新生成：用过的恢复码不行，新的一组生效、旧的作废
    let (st, v) = t.call("POST", "/api/v1/account/totp/recovery-codes", Some(json!({"code": codes[0]})), Auth::Session).await;
    assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("totp_invalid")));
    let d = t.ok("POST", "/api/v1/account/totp/recovery-codes", Some(json!({"code": codes[2]}))).await;
    let fresh: Vec<String> = d["recovery_codes"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
    assert_eq!(fresh.len(), 10);
    assert_eq!(login(&t, ADMIN_PW, Some(&codes[3])).await.0, StatusCode::UNAUTHORIZED, "旧恢复码作废");

    // 开着两步验证时改密码要验证码
    let pw = json!({"current": ADMIN_PW, "new": "a much longer password"});
    let (st, v) = t.call("POST", "/api/v1/account/password", Some(pw.clone()), Auth::Session).await;
    assert_eq!(v["error"]["code"], "totp_required", "{st} {v}");
    let mut pw2 = pw.clone();
    pw2["totp"] = json!(codes[4]);
    let (st, v) = t.call("POST", "/api/v1/account/password", Some(pw2), Auth::Session).await;
    assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("totp_invalid")));

    // 停用：密码错 → 401；密码 + 恢复码 → 停用，之后不再要码
    let (st, v) = t.call("POST", "/api/v1/account/totp/disable", Some(json!({"password": "nope", "code": fresh[0]})), Auth::Session).await;
    assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("bad_password")));
    assert_eq!(failures(&t).await, 3);
    t.ok("POST", "/api/v1/account/totp/disable", Some(json!({"password": ADMIN_PW, "code": fresh[0]}))).await;
    assert_eq!(t.ok("GET", "/api/v1/account/totp", None).await["enabled"], false);
    assert_eq!(login(&t, ADMIN_PW, None).await.0, StatusCode::OK);
    let n: i64 = sqlx::query_scalar("select count(*) from recovery_code").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 0);

    let acts = actions(&t).await;
    for a in ["auth.totp_enabled", "auth.totp_recovery_used", "auth.totp_recovery_regenerated", "auth.totp_disabled", "auth.verify_failed"] {
        assert!(acts.iter().any(|x| x == a), "{a}: {acts:?}");
    }
    // 审计里不出现密钥与恢复码
    let all: Vec<String> = sqlx::query_scalar("select detail::text from audit_log").fetch_all(&t.st.db).await.unwrap();
    assert!(!all.iter().any(|d| d.contains(b32) || d.contains(&fresh[1])));
}

/// 手机与恢复码都丢了：中控上 `fleet-server totp-reset`
#[tokio::test]
async fn totp_reset_cli() {
    let t = setup().await;
    let _ = enable(&t).await;
    assert_eq!(login(&t, ADMIN_PW, None).await.1["error"]["code"], "totp_required");
    fleet_server::auth::totp_reset(&t.st.db, "admin").await.unwrap();
    assert_eq!(login(&t, ADMIN_PW, None).await.0, StatusCode::OK);
    assert!(fleet_server::auth::totp_reset(&t.st.db, "nobody").await.is_err());
    // 重置后可以重新开启
    let (secret, step, _) = enable(&t).await;
    assert_eq!(login(&t, ADMIN_PW, Some(&code(&secret, step + 1))).await.0, StatusCode::OK);
}

#[tokio::test]
async fn sessions_list_and_revoke() {
    let t = common::boot(false, "trust_forwarded_for = true").await;
    let long_ua = "x".repeat(300);
    let c2 = login_cookie(&t, "Mozilla/5.0 test", "203.0.113.7").await;
    let c3 = login_cookie(&t, &long_ua, "2001:db8::5").await;

    let list = t.ok("GET", "/api/v1/account/sessions", None).await;
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 3, "{list:?}");
    assert_eq!(list[0]["current"], true, "本设备排第一");
    assert_eq!(list.iter().filter(|s| s["current"] == true).count(), 1);
    assert!(list.iter().all(|s| s["id"].as_str().unwrap().len() == 16));
    let s2 = list.iter().find(|s| s["ip"] == "203.0.113.7").unwrap();
    assert_eq!(s2["user_agent"], "Mozilla/5.0 test");
    let s3 = list.iter().find(|s| s["ip"] == "2001:db8::5").unwrap();
    assert_eq!(s3["user_agent"].as_str().unwrap().len(), 200, "UA 截到 200 字符");
    // 列表里绝不出现 Cookie 本身
    let tok2 = c2.split('=').nth(1).unwrap();
    assert!(!Value::Array(list.clone()).to_string().contains(tok2));

    // 撤销一个：它马上失效，别的不受影响；再撤 404，乱写的 id 404
    let id2 = s2["id"].as_str().unwrap();
    t.ok("DELETE", &format!("/api/v1/account/sessions/{id2}"), None).await;
    assert_eq!(call_as(&t, &c2, "GET", "/api/v1/auth/me", None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call_as(&t, &c3, "GET", "/api/v1/auth/me", None).await.0, StatusCode::OK);
    assert_eq!(t.call("DELETE", &format!("/api/v1/account/sessions/{id2}"), None, Auth::Session).await.0, StatusCode::NOT_FOUND);
    assert_eq!(t.call("DELETE", "/api/v1/account/sessions/zz", None, Auth::Session).await.0, StatusCode::NOT_FOUND);

    // 退出其它设备：要当前密码，不对 401、谁都不退
    let (st, v) = t.call("POST", "/api/v1/account/sessions/revoke-others", Some(json!({"password": "nope"})), Auth::Session).await;
    assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("bad_password")), "{v}");
    assert_eq!(call_as(&t, &c3, "GET", "/api/v1/auth/me", None).await.0, StatusCode::OK);
    let d = t.ok("POST", "/api/v1/account/sessions/revoke-others", Some(json!({"password": ADMIN_PW}))).await;
    assert_eq!(d["revoked"], 1);
    assert_eq!(call_as(&t, &c3, "GET", "/api/v1/auth/me", None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(t.ok("GET", "/api/v1/account/sessions", None).await.as_array().unwrap().len(), 1);
    assert_eq!(actions(&t).await.iter().filter(|a| *a == "auth.session_revoked").count(), 2);

    // API 密钥一律 403
    let (_, key) = t.make_key("ops", &["read", "write", "secrets", "exec"]).await;
    for (m, p) in [("GET", "/api/v1/account/sessions"), ("GET", "/api/v1/account/totp"), ("POST", "/api/v1/account/totp/setup"), ("POST", "/api/v1/account/password"), ("POST", "/api/v1/account/sessions/revoke-others")] {
        let (st, v) = t.call(m, p, Some(json!({})), Auth::Key(&key)).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{m} {p}: {v}");
    }
}

#[tokio::test]
async fn password_change() {
    let t = setup().await;
    let other = login_cookie(&t, "other", "198.51.100.3").await;
    let new_pw = "another correct horse";

    let (st, _) = t.call("POST", "/api/v1/account/password", Some(json!({"current": ADMIN_PW, "new": "short"})), Auth::Session).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = t.call("POST", "/api/v1/account/password", Some(json!({"current": ADMIN_PW, "new": ADMIN_PW})), Auth::Session).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "与当前密码相同");
    assert_eq!(failures(&t).await, 0, "当前密码对时不留失败记录");

    // 当前密码错：401（不是 unauthorized，不会把前端踢回登录页），按登录失败计数，5 次后连对的也被限速
    for i in 0..5 {
        let (st, v) = t.call("POST", "/api/v1/account/password", Some(json!({"current": "wrong", "new": new_pw})), Auth::Session).await;
        assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("bad_password")), "第 {i} 次");
    }
    assert_eq!(failures(&t).await, 5);
    let (st, _) = t.call("POST", "/api/v1/account/password", Some(json!({"current": ADMIN_PW, "new": new_pw})), Auth::Session).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(login(&t, ADMIN_PW, None).await.0, StatusCode::TOO_MANY_REQUESTS, "与登录共用限速");

    sqlx::query("update login_failure set ts = now() - interval '16 minutes'").execute(&t.st.db).await.unwrap();
    let d = t.ok("POST", "/api/v1/account/password", Some(json!({"current": ADMIN_PW, "new": new_pw}))).await;
    assert_eq!(d["revoked_sessions"], 1);
    assert_eq!(call_as(&t, &other, "GET", "/api/v1/auth/me", None).await.0, StatusCode::UNAUTHORIZED, "别的会话被撤销");
    assert_eq!(t.call("GET", "/api/v1/auth/me", None, Auth::Session).await.0, StatusCode::OK, "本设备保留");
    assert_eq!(login(&t, ADMIN_PW, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(login(&t, new_pw, None).await.0, StatusCode::OK);
    assert!(actions(&t).await.iter().any(|a| a == "auth.password_changed"));
}

async fn sources(t: &T) -> Vec<String> {
    sqlx::query_scalar("select host(ip) from login_source order by 1").fetch_all(&t.st.db).await.unwrap()
}

/// 新建 API 密钥要当前密码；改密码清掉本来源以外登录成功过的来源，勾了 revoke_api_keys 同时撤销全部密钥；totp-reset 清掉全部来源
#[tokio::test]
async fn password_change_clears_sources_and_keys() {
    let t = common::boot(false, "trust_forwarded_for = true").await;
    login_cookie(&t, "a", "203.0.113.7").await;
    let mine = login_cookie(&t, "b", "198.51.100.3").await;
    assert_eq!(sources(&t).await, ["198.51.100.3", "203.0.113.7"]);

    let (st, v) = t.call("POST", "/api/v1/api-keys", Some(json!({"name": "k", "scopes": ["read"]})), Auth::Session).await;
    assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("bad_password")), "{v}");
    let (_, key) = t.make_key("k", &["read"]).await;
    assert_eq!(t.call("GET", "/api/v1/machines", None, Auth::Key(&key)).await.0, StatusCode::OK);

    let body = json!({"current": ADMIN_PW, "new": "another correct horse", "revoke_api_keys": true});
    let (st, v) = call_from(&t, &mine, "198.51.100.3", "POST", "/api/v1/account/password", Some(body)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["revoked_api_keys"], 1);
    assert_eq!(sources(&t).await, ["198.51.100.3"], "只留本来源");
    assert_eq!(t.call("GET", "/api/v1/machines", None, Auth::Key(&key)).await.0, StatusCode::UNAUTHORIZED, "密钥已撤销");

    fleet_server::auth::totp_reset(&t.st.db, "admin").await.unwrap();
    assert!(sources(&t).await.is_empty());
}

/// 验证码输错按用户名另计：1 小时 10 次，不分来源（知道密码的人换 /64 猜码），本人登录成功过的来源也一样先等
#[tokio::test]
async fn wrong_codes_capped_per_user() {
    let t = common::boot(false, "trust_forwarded_for = true").await;
    assert_eq!(login_from(&t, ADMIN_PW, None, "198.51.100.20").await.0, StatusCode::OK);
    let (secret, step, _) = enable(&t).await;
    for i in 0..10 {
        let (st, v) = login_from(&t, ADMIN_PW, Some(&code(&secret, step + 100)), &format!("2001:db8:{i:x}::1")).await;
        assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("totp_invalid")), "{i}: {v}");
    }
    let (st, v) = login_from(&t, ADMIN_PW, Some(&code(&secret, step + 1)), "198.51.100.20").await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{v}");
    sqlx::query("update login_failure set ts = now() - interval '61 minutes'").execute(&t.st.db).await.unwrap();
    assert_eq!(login_from(&t, ADMIN_PW, Some(&code(&secret, step + 1)), "198.51.100.20").await.0, StatusCode::OK);
}
