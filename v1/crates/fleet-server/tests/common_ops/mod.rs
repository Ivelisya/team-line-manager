//! tests/ops.rs 的公共代码：带不同鉴权方式的请求封装（建库与登录见 common）。
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

pub use crate::common::{ADMIN_PW, T};

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

pub async fn setup() -> T { crate::common::boot(false, "").await }
