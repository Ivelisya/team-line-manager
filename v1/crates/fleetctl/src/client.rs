//! 中控 HTTP 客户端（阻塞式）。成功返回 `data`；失败返回带状态码与错误码的 `ApiError`。

use anyhow::{anyhow, Context};
use reqwest::blocking::{Client as Http, RequestBuilder, Response};
use reqwest::Method;
use serde_json::{json, Value};
use std::fmt;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
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

impl ApiError {
    /// 「消息（错误码，HTTP 状态）」，不带下一步提示
    pub fn summary(&self) -> String { format!("{}（{}，HTTP {}）", self.message, self.code, self.status) }
}

/// 没登录 / 没权限时 Display 多一行下一步（`--json` 的 message 也带上，因为有换行时 error_json 用完整文字）
impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())?;
        match (self.status, self.code.as_str()) {
            (401, "unauthorized") => f.write_str("\n检查 FLEET_TOKEN / FLEET_URL 对不对，或让用户重新登录"),
            (403, "forbidden") => f.write_str("\n让用户在网页「设置 → API 密钥」里给这把密钥加权限（fleetctl whoami 可看当前权限）"),
            _ => Ok(()),
        }
    }
}
impl std::error::Error for ApiError {}

/// 没拿到中控响应的网络错误。`code`：`unreachable` = 连不上（请求没发出去，可以直接重试）；
/// `timeout` = 请求已发出、等响应超时（操作可能已在中控执行）。
/// 另外 `confirm_required`（ops::confirm 拒绝没加 --yes 的危险操作）也借这个类型带稳定的 code
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

/// fleetctl 自己的版本，写法同中控（fleet_proto::build_version）：编译时带了 FLEET_GIT_HASH 就是 `0.1.0+<哈希>`
fn own_version() -> String {
    match option_env!("FLEET_GIT_HASH") {
        Some(h) => format!("{}+{h}", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").into(),
    }
}

/// 中控每个接口响应都带 `x-fleet-version`；和自己不一致就在 stderr 提醒一次（不影响 stdout 与 --json）
fn check_version(res: &Response) {
    static DONE: AtomicBool = AtomicBool::new(false);
    let Some(server) = res.headers().get("x-fleet-version").and_then(|v| v.to_str().ok()) else { return };
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    let mine = own_version();
    if mine != server {
        eprintln!("fleetctl 版本 {mine} 与中控 {server} 不一致，用中控上的 fleetctl 或重新编译");
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
        check_version(&res);
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

    /// 调用并取出 `data`（中控成功的响应一律是 `{"data": …}`；没有 data 多半是地址指到了别的服务上，报错而不是当成 null）
    pub fn call(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>, timeout: Duration) -> anyhow::Result<Value> {
        let res = self.send(method, path, query, body, timeout)?;
        let text = res.text().context("读取响应")?;
        let snippet = || text.chars().take(200).collect::<String>();
        let mut v: Value = serde_json::from_str(&text).with_context(|| format!("中控返回的不是 JSON：{}", snippet()))?;
        v.get_mut("data").map(Value::take).ok_or_else(|| anyhow!("中控返回的 JSON 里没有 data：{}", snippet()))
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
        assert!(e.to_string().contains("没有 write 权限") && e.to_string().contains("403") && e.to_string().contains("加权限"));
        let e = api_error(401, r#"{"error":{"code":"unauthorized","message":"需要登录","detail":null}}"#);
        assert!(e.to_string().contains("FLEET_TOKEN"));
        // 登录失败等别的 401 不带令牌提示
        let e = api_error(401, r#"{"error":{"code":"bad_credentials","message":"用户名或密码不对","detail":null}}"#);
        assert!(!e.to_string().contains("FLEET_TOKEN") && !e.to_string().contains('\n'));
        let e = api_error(502, "<html>bad gateway</html>");
        assert_eq!(e.code, "http");
        assert!(e.message.contains("502") && e.message.contains("bad gateway"));
    }
}
