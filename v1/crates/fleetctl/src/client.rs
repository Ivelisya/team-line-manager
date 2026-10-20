//! 中控 HTTP 客户端（阻塞式）。成功返回 `data`；失败返回带状态码与错误码的 `ApiError`。

use anyhow::{anyhow, Context};
use reqwest::blocking::{Client as Http, RequestBuilder, Response};
use reqwest::Method;
use serde_json::{json, Value};
use std::fmt;
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}（{}，HTTP {}）", self.message, self.code, self.status) }
}
impl std::error::Error for ApiError {}

pub enum Cred {
    Bearer(String),
    /// 会话 Cookie 的 `name=value`（密钥管理只认会话）
    Cookie(String),
    None,
}

pub struct Client {
    base: String,
    cred: Cred,
    http: Http,
}

impl Client {
    pub fn new(base: &str, cred: Cred) -> anyhow::Result<Self> {
        let http = Http::builder().connect_timeout(Duration::from_secs(10)).user_agent(concat!("fleetctl/", env!("CARGO_PKG_VERSION"))).build()?;
        Ok(Self { base: base.trim_end_matches('/').to_string(), cred, http })
    }

    fn builder(&self, method: Method, path: &str) -> RequestBuilder {
        let mut r = self.http.request(method, format!("{}/api/v1{path}", self.base)).header("x-fleet-request", "1");
        match &self.cred {
            Cred::Bearer(t) => r = r.bearer_auth(t),
            Cred::Cookie(c) => r = r.header("cookie", c),
            Cred::None => {}
        }
        r
    }

    fn check(res: Response) -> anyhow::Result<Response> {
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        let text = res.text().unwrap_or_default();
        Err(api_error(status.as_u16(), &text).into())
    }

    /// 通用调用，返回响应（已检查状态码）
    fn send(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>, timeout: Duration) -> anyhow::Result<Response> {
        let mut r = self.builder(method, path).timeout(timeout);
        if !query.is_empty() {
            r = r.query(query);
        }
        if let Some(b) = body {
            r = r.json(b);
        }
        let res = r.send().map_err(|e| anyhow!("连不上中控 {}：{e}", self.base))?;
        Self::check(res)
    }

    /// 调用并取出 `data`
    pub fn call(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>, timeout: Duration) -> anyhow::Result<Value> {
        let res = self.send(method, path, query, body, timeout)?;
        let text = res.text().context("读取响应")?;
        let mut v: Value = serde_json::from_str(&text).with_context(|| format!("中控返回的不是 JSON：{}", text.chars().take(200).collect::<String>()))?;
        Ok(v.get_mut("data").map(Value::take).unwrap_or(Value::Null))
    }

    pub fn get(&self, path: &str) -> anyhow::Result<Value> { self.call(Method::GET, path, &[], None, DEFAULT_TIMEOUT) }
    pub fn get_q(&self, path: &str, query: &[(&str, String)]) -> anyhow::Result<Value> { self.call(Method::GET, path, query, None, DEFAULT_TIMEOUT) }
    pub fn post(&self, path: &str, body: &Value) -> anyhow::Result<Value> { self.call(Method::POST, path, &[], Some(body), DEFAULT_TIMEOUT) }
    pub fn post_t(&self, path: &str, body: &Value, timeout: Duration) -> anyhow::Result<Value> { self.call(Method::POST, path, &[], Some(body), timeout) }
    pub fn patch(&self, path: &str, body: &Value) -> anyhow::Result<Value> { self.call(Method::PATCH, path, &[], Some(body), DEFAULT_TIMEOUT) }
    pub fn delete(&self, path: &str) -> anyhow::Result<Value> { self.call(Method::DELETE, path, &[], None, DEFAULT_TIMEOUT) }

    /// 用户名密码登录，返回会话 Cookie（`name=value`）
    pub fn login_session(&self, user: &str, password: &str) -> anyhow::Result<String> {
        let res = self.send(Method::POST, "/auth/login", &[], Some(&json!({"username": user, "password": password})), DEFAULT_TIMEOUT)?;
        res.headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|c| c.split(';').next().filter(|kv| kv.starts_with("fleet_session=")).map(String::from))
            .ok_or_else(|| anyhow!("登录成功但没有拿到会话 Cookie"))
    }

    /// 长连接（SSE）：不设整体超时
    pub fn stream(&self, path: &str) -> anyhow::Result<Response> {
        let res = self
            .builder(Method::GET, path)
            .header("accept", "text/event-stream")
            .timeout(Duration::from_secs(60 * 60 * 24))
            .send()
            .map_err(|e| anyhow!("连不上中控 {}：{e}", self.base))?;
        Self::check(res)
    }
}

/// 把 `{"error":{"code","message"}}` 解析成 ApiError；不是这个格式时给出前 200 字符
pub fn api_error(status: u16, body: &str) -> ApiError {
    let v: Option<Value> = serde_json::from_str(body).ok();
    let e = v.as_ref().and_then(|v| v.get("error"));
    ApiError {
        status,
        code: e.and_then(|e| e.get("code")).and_then(|c| c.as_str()).unwrap_or("http").to_string(),
        message: e
            .and_then(|e| e.get("message"))
            .and_then(|c| c.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("中控返回 HTTP {status}：{}", body.chars().take(200).collect::<String>())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_error_body() {
        let e = api_error(403, r#"{"error":{"code":"forbidden","message":"这个 API 密钥没有 write 权限","detail":null}}"#);
        assert_eq!((e.status, e.code.as_str()), (403, "forbidden"));
        assert!(e.to_string().contains("没有 write 权限") && e.to_string().contains("403"));
        let e = api_error(502, "<html>bad gateway</html>");
        assert_eq!(e.code, "http");
        assert!(e.message.contains("502") && e.message.contains("bad gateway"));
    }
}
