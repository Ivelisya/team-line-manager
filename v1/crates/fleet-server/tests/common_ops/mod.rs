//! tests/ops.rs 的公共代码：起一个干净的库与应用，登录出会话 Cookie，提供带不同鉴权方式的请求封装。
//! 需要 PostgreSQL：环境变量 FLEET_TEST_DB（默认 postgres://fleet:fleet@127.0.0.1/fleet_test）。会清空该库的 public schema。
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use fleet_server::{config::Config, crypto::SecretBox, db, state::AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

pub const ADMIN_PW: &str = "correct horse battery";

pub struct T {
    pub app: axum::Router,
    pub st: AppState,
    pub cookie: String,
}

pub enum Auth<'a> {
    /// 会话 Cookie（写请求带 X-Fleet-Request）
    Session,
    /// API 密钥（不带 X-Fleet-Request，和真实脚本一致）
    Key(&'a str),
    None,
}

impl T {
    pub async fn call(&self, method: &str, path: &str, body: Option<Value>, auth: Auth<'_>) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path).header("content-type", "application/json");
        match auth {
            Auth::Session => b = b.header("cookie", &self.cookie).header("x-fleet-request", "1"),
            Auth::Key(tok) => b = b.header("authorization", format!("Bearer {tok}")),
            Auth::None => {}
        }
        let req = b.body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty)).unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// 会话调用，要求成功并返回 data
    pub async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.call(method, path, body, Auth::Session).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v["data"].clone()
    }

    /// 创建 API 密钥，返回 (id, token)
    pub async fn make_key(&self, name: &str, scopes: &[&str]) -> (String, String) {
        let d = self.ok("POST", "/api/v1/api-keys", Some(json!({"name": name, "scopes": scopes}))).await;
        (d["id"].as_str().unwrap().to_string(), d["token"].as_str().unwrap().to_string())
    }

    pub async fn login(&self, user: &str, pw: &str) -> (StatusCode, Value) {
        self.call("POST", "/api/v1/auth/login", Some(json!({"username": user, "password": pw})), Auth::None).await
    }
}

pub async fn setup() -> T {
    let url = std::env::var("FLEET_TEST_DB").unwrap_or_else(|_| "postgres://fleet:fleet@127.0.0.1/fleet_test".into());
    let pool = db::connect(&url).await.unwrap();
    sqlx::query("drop schema public cascade").execute(&pool).await.unwrap();
    sqlx::query("create schema public").execute(&pool).await.unwrap();
    db::MIGRATOR.run(&pool).await.unwrap();
    db::maintain_partitions(&pool, 14, 7, 2).await.unwrap();
    let cfg: Config = toml::from_str(&format!(
        r#"database_url = "{url}"
           secret_key_file = "/dev/null"
           public_url = "http://fleet.test"
           ssh_key = "/nonexistent"
           known_hosts = "/tmp/fleet-test-known-hosts"
           dist_dir = "/nonexistent"
           job_workers = 2"#
    ))
    .unwrap();
    let st = AppState::new(cfg, pool.clone(), SecretBox::from_key(&[9u8; 32]).unwrap());
    fleet_server::auth::upsert_user(&pool, "admin", ADMIN_PW).await.unwrap();
    let app = fleet_server::app(st.clone());
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(json!({"username": "admin", "password": ADMIN_PW}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
    T { app, st, cookie }
}
