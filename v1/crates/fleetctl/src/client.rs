//! 中控 HTTP 客户端（阻塞式）。成功返回 `data`；失败返回带状态码与错误码的 `ApiError`。

use anyhow::{anyhow, Context};
use reqwest::blocking::{Client as Http, RequestBuilder, Response};
use reqwest::Method;
use serde_json::{json, Value};
use std::fmt;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::handshake::HandshakeError;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::WebSocket;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// 实时流建连 + 握手的超时（各自计）
const WS_CONNECT: Duration = Duration::from_secs(10);

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

/// 没拿到中控响应的网络错误。`code`：`unreachable` = 连不上（请求没发出去，可以直接重试）；
/// `timeout` = 请求已发出、等响应超时（操作可能已在中控执行）
#[derive(Debug)]
pub struct NetError {
    pub code: &'static str,
    pub message: String,
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.message) }
}
impl std::error::Error for NetError {}

/// 错误连同 source 链拼成一行，另给最里层的那个（reqwest 的顶层只有「error sending request for url (…)」，真正原因在 source 里）
fn error_chain(e: &dyn std::error::Error) -> (String, String) {
    let (mut s, mut inner) = (e.to_string(), e.to_string());
    let mut src = e.source();
    while let Some(x) = src {
        inner = x.to_string();
        s.push_str(": ");
        s.push_str(&inner);
        src = x.source();
    }
    (s, inner)
}

/// 发请求失败 → 按是否连上区分。连接超时（connect_timeout）也算连不上，所以先判 is_connect
fn send_error(base: &str, timeout: Duration, e: &reqwest::Error) -> anyhow::Error {
    let (full, inner) = error_chain(e);
    if e.is_connect() {
        NetError { code: "unreachable", message: format!("连不上中控 {base}：{inner}") }.into()
    } else if e.is_timeout() {
        let message = format!("等中控响应超过 {} 秒：操作可能已在中控执行，先用 fleetctl jobs / audit 核对再决定是否重试", timeout.as_secs());
        NetError { code: "timeout", message }.into()
    } else {
        anyhow!("与中控 {base} 的连接出错：{full}（操作可能已在中控执行，先用 fleetctl jobs / audit 核对再决定是否重试）")
    }
}

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
        let res = r.send().map_err(|e| send_error(&self.base, timeout, &e))?;
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
    pub fn put(&self, path: &str, body: &Value) -> anyhow::Result<Value> { self.call(Method::PUT, path, &[], Some(body), DEFAULT_TIMEOUT) }
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

    /// 实时流 WebSocket。建连、握手各 10 s 超时；服务端每 15 s 发 ping（读的时候自动回 pong），读超时 45 s 当连接已死。
    /// 不走 HTTPS_PROXY（REST 请求走）
    pub fn ws(&self, path: &str) -> anyhow::Result<WebSocket<MaybeTlsStream<TcpStream>>> {
        let url = format!("{}/api/v1{path}", self.base.replacen("http", "ws", 1));
        let mut req = url.as_str().into_client_request()?;
        let h = req.headers_mut();
        h.insert("user-agent", HeaderValue::from_static(concat!("fleetctl/", env!("CARGO_PKG_VERSION"))));
        match &self.cred {
            Cred::Bearer(t) => {
                h.insert("authorization", HeaderValue::from_str(&format!("Bearer {t}"))?);
            }
            Cred::Cookie(c) => {
                h.insert("cookie", HeaderValue::from_str(c)?);
            }
            Cred::None => {}
        }
        let fail = |e: &dyn fmt::Display| anyhow!("连不上中控 {}：{e}", self.base);
        let uri = req.uri();
        let host = uri.host().ok_or_else(|| anyhow!("中控地址缺主机名：{url}"))?.trim_matches(['[', ']']).to_string();
        let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("wss") { 443 } else { 80 });
        let mut err = std::io::Error::other("域名没有解析出地址");
        let tcp = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| fail(&e))?
            .find_map(|a| TcpStream::connect_timeout(&a, WS_CONNECT).map_err(|e| err = e).ok())
            .ok_or_else(|| fail(&err))?;
        tcp.set_read_timeout(Some(WS_CONNECT))?;
        tcp.set_write_timeout(Some(WS_CONNECT))?;
        let (ws, _) = tungstenite::client_tls(req, tcp).map_err(|e| match e {
            HandshakeError::Failure(tungstenite::Error::Http(r)) => anyhow::Error::from(api_error(r.status().as_u16(), &String::from_utf8_lossy(r.body().as_deref().unwrap_or_default()))),
            e => fail(&e),
        })?;
        let tcp = match ws.get_ref() {
            MaybeTlsStream::Plain(s) => s,
            MaybeTlsStream::Rustls(s) => s.get_ref(),
            _ => unreachable!("只启用了 rustls"),
        };
        tcp.set_read_timeout(Some(Duration::from_secs(45)))?;
        Ok(ws)
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
