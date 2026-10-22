//! 网页终端：开票（只给会话用户、再验证与 5 分钟窗口、目标检查、限速）、握手（Origin、Cookie、API 密钥）、
//! 票据（一次性、绑定会话）、每人 / 全中控的并发上限、浏览器断开即结束 ssh、会话撤销即断开、消息大小、审计。
//! 需要 PostgreSQL（见 common）与本机的 ssh 客户端：终端连的是本机的假 SSH 服务（只回版本行、之后不说话，
//! ssh 停在密钥交换上，最多 30 秒），或者一个没人听的端口（ssh 立刻报连接被拒）。
//! 最后一个 `lab_e2e` 是实验室的端到端（默认忽略，见它的说明）。

mod common;
mod common_ops;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{ADMIN_PW, T};
use common_ops::Auth;
use futures::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;
use uuid::Uuid;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 测试环境的 public_url 是 http://fleet.test
const ORIGIN: &str = "http://fleet.test";

/// 用指定 Cookie 调接口
async fn call_as(t: &T, cookie: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-fleet-request", "1")
        .header("cookie", cookie)
        .body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty))
        .unwrap();
    let res = t.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn ticket(t: &T, cookie: &str, body: Value) -> (StatusCode, Value) { call_as(t, cookie, "POST", "/api/v1/terminal/tickets", Some(body)).await }

fn code_of(v: &Value) -> &str { v["error"]["code"].as_str().unwrap_or("") }

/// 登录拿一个新会话的 Cookie
async fn login(t: &T, user: &str) -> String {
    let req = Request::post("/api/v1/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(json!({"username": user, "password": ADMIN_PW}).to_string()))
        .unwrap();
    let res = t.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string()
}

/// 会话的 id_hash（Cookie 值的 sha256）
fn session_hash(cookie: &str) -> Vec<u8> { fleet_server::crypto::sha256(cookie.split_once('=').unwrap().1.as_bytes()) }

async fn seed_team(t: &T) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into team (id, name) values ($1, '团队1')").bind(id).execute(&t.st.db).await.unwrap();
    id
}

/// 库里放一台机器；`host` 给了就是挂在它上面的同机 IP 节点（它不存 SSH 端口，读宿主机的）
async fn seed_machine(t: &T, team: Uuid, number: i32, kind: &str, lifecycle: &str, (ip, port): (&str, u16), host: Option<Uuid>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into machine (id, kind, team_id, number, primary_ip, ssh_port, lifecycle, activated_at, retired_at, host_id)
         values ($1, $2::machine_kind, $3, $4, $5::inet, $6, $7::machine_lifecycle, now(), case when $7 = 'retired' then now() end, $8)",
    )
    .bind(id)
    .bind(kind)
    .bind(team)
    .bind(number)
    .bind(ip)
    .bind(host.is_none().then_some(port as i32))
    .bind(lifecycle)
    .bind(host)
    .execute(&t.st.db)
    .await
    .unwrap();
    id
}

/// 把应用起在本机随机端口上
async fn serve(t: &T) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = t.app.clone();
    tokio::spawn(async move { axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap() });
    addr
}

async fn connect(addr: SocketAddr, cookie: Option<&str>, origin: Option<&str>, bearer: Option<&str>) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{addr}/api/v1/terminal/ws").into_client_request().unwrap();
    if let Some(c) = cookie {
        req.headers_mut().insert("cookie", c.parse().unwrap());
    }
    if let Some(o) = origin {
        req.headers_mut().insert("origin", o.parse().unwrap());
    }
    if let Some(b) = bearer {
        req.headers_mut().insert("authorization", format!("Bearer {b}").parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(s, _)| s)
}

fn http_status(r: Result<Ws, tokio_tungstenite::tungstenite::Error>) -> StatusCode {
    match r {
        Err(tokio_tungstenite::tungstenite::Error::Http(res)) => res.status(),
        other => panic!("握手应当失败：{:?}", other.map(|_| ())),
    }
}

async fn send_ticket(ws: &mut Ws, tk: &str) { ws.send(Message::Text(json!({"type": "auth", "ticket": tk}).to_string().into())).await.unwrap(); }

/// 开票（会话 5 分钟内验证过时密码不看）、连上、送票据
async fn open_term(t: &T, addr: SocketAddr, cookie: &str, machine: Uuid) -> Ws {
    let (s, v) = ticket(t, cookie, json!({"kind": "machine", "id": machine, "password": ADMIN_PW})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let mut ws = connect(addr, Some(cookie), Some(ORIGIN), None).await.unwrap();
    send_ticket(&mut ws, v["data"]["ticket"].as_str().unwrap()).await;
    ws
}

/// 读到服务端结束为止（最多 `secs` 秒）：返回（途中收到的终端输出, exit 的原因, 关闭码）
async fn until_exit(ws: &mut Ws, secs: u64) -> (String, String, Option<u16>) {
    let (mut out, mut reason, mut code) = (Vec::new(), String::new(), None);
    let r = tokio::time::timeout(Duration::from_secs(secs), async {
        while let Some(m) = ws.next().await {
            match m {
                Ok(Message::Binary(b)) => out.extend_from_slice(&b),
                Ok(Message::Text(t)) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    assert_eq!(v["type"], "exit", "{v}");
                    reason = v["reason"].as_str().unwrap().to_string();
                }
                Ok(Message::Close(f)) => {
                    code = f.map(|f| u16::from(f.code));
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    })
    .await;
    let out = String::from_utf8_lossy(&out).into_owned();
    assert!(r.is_ok(), "{secs} 秒内没有结束，输出：{out}");
    (out, reason, code)
}

/// 本机假 SSH 服务：接上就回一行版本号，之后只读不说，ssh 停在密钥交换上（ServerAliveInterval 10 × 3，最多 30 秒）。
/// 记着当前还连着几个：终端一断、ssh 被杀，这边马上读到 EOF
struct FakeSshd {
    port: u16,
    open: Arc<AtomicUsize>,
}

impl FakeSshd {
    async fn start() -> FakeSshd {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let open = Arc::new(AtomicUsize::new(0));
        let o = open.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                o.fetch_add(1, SeqCst);
                let o = o.clone();
                tokio::spawn(async move {
                    let _ = s.write_all(b"SSH-2.0-OpenSSH_9.9 fake\r\n").await;
                    let mut buf = [0u8; 4096];
                    while matches!(s.read(&mut buf).await, Ok(n) if n > 0) {}
                    o.fetch_sub(1, SeqCst);
                });
            }
        });
        FakeSshd { port, open }
    }

    /// 等连着的个数变成 n（最多 10 秒）
    async fn wait_open(&self, n: usize) {
        for _ in 0..200 {
            if self.open.load(SeqCst) == n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("假 SSH 服务连着 {} 个，等不到 {n}", self.open.load(SeqCst));
    }
}

async fn audits(t: &T, action: &str) -> Vec<Value> {
    sqlx::query_scalar("select detail from audit_log where action = $1 order by id").bind(action).fetch_all(&t.st.db).await.unwrap()
}

/// 开票：只给会话用户；目标与名额先查；再验证（没开两步验证要密码，开了要验证码），5 分钟窗口；错误码；限速
#[tokio::test]
async fn ticket_needs_session_and_reverification() {
    let t = common_ops::setup().await;
    // 网页取上限的地方
    let lim = t.get("/api/v1/meta").await["terminal"].clone();
    assert_eq!((lim["per_user"].as_u64(), lim["global"].as_u64(), lim["max_message"].as_u64(), lim["max_dim"].as_u64()), (Some(3), Some(8), Some(65536), Some(500)), "{lim}");
    let team = seed_team(&t).await;
    let m = seed_machine(&t, team, 1, "vps", "active", ("127.0.0.1", 22), None).await;
    let body = |extra: Value| {
        let mut b = json!({"kind": "machine", "id": m});
        b.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        b
    };

    // API 密钥（scope 再全）一律 403
    let (_, key) = t.make_key("ops", &["read", "write", "secrets", "exec"]).await;
    let (s, v) = t.call("POST", "/api/v1/terminal/tickets", Some(body(json!({"password": ADMIN_PW}))), Auth::Key(&key)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");

    // 目标不对、名额不够在再验证之前就回（不必先输密码）
    for (b, want) in [
        (json!({"kind": "router", "id": m}), StatusCode::BAD_REQUEST),
        (json!({"kind": "vm", "id": m}), StatusCode::BAD_REQUEST),
        (json!({"kind": "machine", "id": Uuid::now_v7()}), StatusCode::NOT_FOUND),
    ] {
        let (s, v) = ticket(&t, &t.cookie, b).await;
        assert_eq!(s, want, "{v}");
    }
    let (_, v) = ticket(&t, &t.cookie, json!({"kind": "router", "id": m})).await;
    assert_eq!(v["error"]["message"], "软路由终端稍后开放");
    for (i, (kind, lc)) in [("vps", "retired"), ("vps", "provisioning"), ("vps", "failed"), ("vps", "draft"), ("proxy", "active")].into_iter().enumerate() {
        let id = seed_machine(&t, team, 10 + i as i32, kind, lc, (&format!("192.0.2.{}", 10 + i), 22), None).await;
        let (s, v) = ticket(&t, &t.cookie, json!({"kind": "machine", "id": id})).await;
        assert_eq!(s, StatusCode::CONFLICT, "{kind}/{lc}: {v}");
    }
    // 同机 IP 节点：它登录的宿主机还没上架完成（或已退役）同样不行
    let host = seed_machine(&t, team, 20, "vps", "provisioning", ("192.0.2.20", 22), None).await;
    let node = seed_machine(&t, team, 21, "vps", "active", ("192.0.2.21", 22), Some(host)).await;
    let (s, v) = ticket(&t, &t.cookie, json!({"kind": "machine", "id": node})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("团队1-21 的宿主机 团队1-20 还没上架完成"), "{v}");

    // 没开两步验证：要当前密码。缺了 password_required（不算失败），错了 password_invalid（算失败、写审计）
    let (s, v) = ticket(&t, &t.cookie, body(json!({}))).await;
    assert_eq!((s, code_of(&v)), (StatusCode::UNAUTHORIZED, "password_required"), "{v}");
    let (s, v) = ticket(&t, &t.cookie, body(json!({"code": "123456"}))).await;
    assert_eq!(code_of(&v), "password_required", "没开两步验证时给验证码不算：{s} {v}");
    let (s, v) = ticket(&t, &t.cookie, body(json!({"password": "wrong-password"}))).await;
    assert_eq!((s, code_of(&v)), (StatusCode::UNAUTHORIZED, "password_invalid"), "{v}");
    let fails = audits(&t, "auth.verify_failed").await;
    assert_eq!((fails.len(), fails[0]["op"].as_str(), fails[0]["reason"].as_str()), (1, Some("terminal"), Some("password")));
    let (s, v) = ticket(&t, &t.cookie, body(json!({"password": ADMIN_PW}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["ticket"].as_str().unwrap().len(), 43, "32 字节随机数，base64url");
    let exp = time::OffsetDateTime::parse(v["data"]["expires_at"].as_str().unwrap(), &time::format_description::well_known::Rfc3339).unwrap();
    let left = (exp - time::OffsetDateTime::now_utc()).whole_seconds();
    assert!((55..=60).contains(&left), "60 秒有效：{left}");
    let failures: i64 = sqlx::query_scalar("select count(*) from login_failure").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(failures, 0, "验证通过后清掉本来源的失败记录");
    // 5 分钟内这个会话再开票免输；别的会话不算
    let (s, v) = ticket(&t, &t.cookie, body(json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let other = login(&t, "admin").await;
    let (_, v) = ticket(&t, &other, body(json!({}))).await;
    assert_eq!(code_of(&v), "password_required", "按会话记：{v}");

    // 输错与登录共用限速：同一用户名 + 来源 15 分钟 5 次
    for _ in 0..5 {
        let (_, v) = ticket(&t, &other, body(json!({"password": "wrong-password"}))).await;
        assert_eq!(code_of(&v), "password_invalid", "{v}");
    }
    let (s, v) = ticket(&t, &other, body(json!({"password": ADMIN_PW}))).await;
    assert_eq!((s, code_of(&v)), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"), "{v}");
}

/// 开了两步验证：只要验证码（或恢复码），密码不顶用；错了 totp_invalid 并记作验证码错误
#[tokio::test]
async fn ticket_with_totp_needs_code() {
    let t = common_ops::setup().await;
    let team = seed_team(&t).await;
    let m = seed_machine(&t, team, 1, "vps", "active", ("127.0.0.1", 22), None).await;
    let s = t.ok("POST", "/api/v1/account/totp/setup", Some(json!({"password": ADMIN_PW}))).await;
    let secret = data_encoding::BASE32_NOPAD.decode(s["secret"].as_str().unwrap().as_bytes()).unwrap();
    let code = |step: i64| format!("{:06}", fleet_server::totp::hotp(&secret, step as u64, 6));
    let n = fleet_server::totp::now_step();
    let codes = t.ok("POST", "/api/v1/account/totp/enable", Some(json!({"code": code(n)}))).await["recovery_codes"].clone();

    let (s, v) = ticket(&t, &t.cookie, json!({"kind": "machine", "id": m, "password": ADMIN_PW})).await;
    assert_eq!((s, code_of(&v)), (StatusCode::UNAUTHORIZED, "totp_required"), "开了两步验证只认验证码：{v}");
    let wrong = format!("{:06}", (fleet_server::totp::hotp(&secret, n as u64, 6) + 1) % 1_000_000);
    let (_, v) = ticket(&t, &t.cookie, json!({"kind": "machine", "id": m, "code": wrong})).await;
    assert_eq!(code_of(&v), "totp_invalid", "{v}");
    // 开启时用过的时间步不能再用（重放）
    let (_, v) = ticket(&t, &t.cookie, json!({"kind": "machine", "id": m, "code": code(n)})).await;
    assert_eq!(code_of(&v), "totp_invalid", "{v}");
    let wrong_codes: i64 = sqlx::query_scalar("select count(*) from login_failure where wrong_code").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(wrong_codes, 2, "计入验证码错误的每小时上限");
    // 恢复码也行（用掉一个，写审计）
    let (s, v) = ticket(&t, &t.cookie, json!({"kind": "machine", "id": m, "code": codes[0]})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let used = audits(&t, "auth.totp_recovery_used").await;
    assert_eq!((used.len(), used[0]["op"].as_str()), (1, Some("terminal")));
    // 5 分钟内免输
    let (s, v) = ticket(&t, &t.cookie, json!({"kind": "machine", "id": m})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // 另一个会话（用恢复码登录的）：照样要验证码
    let req = Request::post("/api/v1/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(json!({"username": "admin", "password": ADMIN_PW, "totp": codes[1]}).to_string()))
        .unwrap();
    let res = t.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let other = res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
    let (_, v) = ticket(&t, &other, json!({"kind": "machine", "id": m})).await;
    assert_eq!(code_of(&v), "totp_required", "{v}");
}

/// 握手：Cookie、Origin、API 密钥；第一条消息是票据：一次性、绑定会话；ssh 连不上时把它的报错带给浏览器、写审计
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websocket_checks_origin_ticket_and_session() {
    let t = common_ops::setup().await;
    let team = seed_team(&t).await;
    // 没人听的端口：ssh 立刻报连接被拒、退出 255
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let host = seed_machine(&t, team, 1, "vps", "active", ("127.0.0.1", closed), None).await;
    let ipnode = seed_machine(&t, team, 2, "vps", "active", ("192.0.2.7", 22), Some(host)).await;
    let addr = serve(&t).await;
    let (_, key) = t.make_key("ops", &["read", "write", "secrets", "exec"]).await;

    assert_eq!(http_status(connect(addr, None, Some(ORIGIN), None).await), StatusCode::UNAUTHORIZED);
    for origin in [Some("https://evil.example"), Some("http://fleet.test:8080"), None] {
        assert_eq!(http_status(connect(addr, Some(&t.cookie), origin, None).await), StatusCode::FORBIDDEN, "{origin:?}");
    }
    assert_eq!(http_status(connect(addr, None, Some(ORIGIN), Some(&key)).await), StatusCode::FORBIDDEN, "API 密钥不行");
    // 握手之后还没送票据的连接：每个会话最多 5 个，第 6 个握手就被拒；别的会话不受影响
    let crowd = login(&t, "admin").await;
    let mut waiting = Vec::new();
    for _ in 0..5 {
        waiting.push(connect(addr, Some(&crowd), Some(ORIGIN), None).await.unwrap());
    }
    assert_eq!(http_status(connect(addr, Some(&crowd), Some(ORIGIN), None).await), StatusCode::TOO_MANY_REQUESTS);
    let mut ws = connect(addr, Some(&t.cookie), Some(ORIGIN), None).await.unwrap();
    ws.close(None).await.unwrap();
    for mut w in waiting {
        w.close(None).await.unwrap();
    }

    // 第一条不是票据 / 票据不对
    let mut ws = connect(addr, Some(&t.cookie), Some(ORIGIN), None).await.unwrap();
    ws.send(Message::Text("ls".into())).await.unwrap();
    assert!(until_exit(&mut ws, 5).await.1.contains("第一条消息应当是票据"));
    let mut ws = connect(addr, Some(&t.cookie), Some(ORIGIN), None).await.unwrap();
    send_ticket(&mut ws, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").await;
    assert!(until_exit(&mut ws, 5).await.1.contains("票据不对"));

    // 别的会话拿去用：不认，这张票也就作废了
    let (s, v) = ticket(&t, &t.cookie, json!({"kind": "machine", "id": ipnode, "password": ADMIN_PW})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let tk = v["data"]["ticket"].as_str().unwrap().to_string();
    let other = login(&t, "admin").await;
    let mut ws = connect(addr, Some(&other), Some(ORIGIN), None).await.unwrap();
    send_ticket(&mut ws, &tk).await;
    assert!(until_exit(&mut ws, 5).await.1.contains("不是这个登录会话"));
    let mut ws = connect(addr, Some(&t.cookie), Some(ORIGIN), None).await.unwrap();
    send_ticket(&mut ws, &tk).await;
    assert!(until_exit(&mut ws, 5).await.1.contains("票据不对或已经用过"));

    // 同机 IP 节点：登录的是宿主机（127.0.0.1:closed），ssh 连不上，报错原样到终端里，结束原因说明 255
    let (_, v) = ticket(&t, &t.cookie, json!({"kind": "machine", "id": ipnode})).await;
    let tk = v["data"]["ticket"].as_str().unwrap().to_string();
    let mut ws = connect(addr, Some(&t.cookie), Some(ORIGIN), None).await.unwrap();
    ws.send(Message::Text(json!({"type": "auth", "ticket": tk, "cols": 120, "rows": 40}).to_string().into())).await.unwrap();
    let (out, reason, code) = until_exit(&mut ws, 15).await;
    assert!(out.contains("Connection refused") && out.contains(&format!("port {closed}")), "{out}");
    assert!(reason.contains("255"), "{reason}");
    assert_eq!(code, Some(1000));
    // 同一张票不能再用
    let mut ws = connect(addr, Some(&t.cookie), Some(ORIGIN), None).await.unwrap();
    send_ticket(&mut ws, &tk).await;
    assert!(until_exit(&mut ws, 5).await.1.contains("票据不对或已经用过"));

    // 审计：开、关各一条，记目标（同机 IP 节点写明宿主机）、会话、时长、字节数、原因、退出码；不记输入内容
    let open = audits(&t, "terminal.open").await;
    let close = audits(&t, "terminal.close").await;
    assert_eq!((open.len(), close.len()), (1, 1));
    assert_eq!((open[0]["name"].as_str(), open[0]["host"]["name"].as_str(), open[0]["ip"].as_str()), (Some("团队1-02"), Some("团队1-01"), Some("127.0.0.1")));
    assert_eq!(open[0]["session"].as_str().unwrap().len(), 16);
    let c = &close[0];
    assert_eq!((c["why"].as_str(), c["exit_code"].as_i64(), c["bytes_in"].as_u64()), (Some("remote_exit"), Some(255), Some(0)));
    assert!(c["bytes_out"].as_u64().unwrap() > 0 && c["duration_s"].is_u64() && c["reason"].as_str().unwrap().contains("255"), "{c}");
    assert_eq!(c["open_id"], json!(sqlx::query_scalar::<_, i64>("select id from audit_log where action = 'terminal.open'").fetch_one(&t.st.db).await.unwrap()));
    // 「操作记录」里的中文标题与摘要
    let log = t.get(&format!("/api/v1/audit?target_id={ipnode}")).await;
    let titles: Vec<&str> = log.as_array().unwrap().iter().map(|a| a["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["关闭网页终端", "打开网页终端"]);
    let summary = log[0]["summary"].as_str().unwrap();
    assert!(summary.starts_with("团队1-02（登录宿主机 团队1-01） · ") && summary.contains("上行 0 字节"), "{summary}");
    assert_eq!(t.st.terminals.live(), 0);
}

/// 并发上限（每人 3、全中控 8：开票时先拦，连上时为准）、浏览器断开立刻结束 ssh、会话被撤销立刻断开（撤销通知）、
/// 会话没了而没有通知时由复核断开、改窗口大小的消息、太大的消息
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn limits_disconnect_and_revocation() {
    let t = common_ops::setup().await;
    let team = seed_team(&t).await;
    let sshd = FakeSshd::start().await;
    let m = seed_machine(&t, team, 1, "vps", "active", ("127.0.0.1", sshd.port), None).await;
    let addr = serve(&t).await;
    for u in ["u2", "u3"] {
        fleet_server::auth::upsert_user(&t.st.db, u, ADMIN_PW).await.unwrap();
    }

    // admin：开 2 个，再开两张票（都还在名额内），第 3 个连上后第 4 张票连不上；这时再开票直接 429
    let mut a1 = open_term(&t, addr, &t.cookie, m).await;
    let mut a2 = open_term(&t, addr, &t.cookie, m).await;
    sshd.wait_open(2).await;
    let tk3 = ticket(&t, &t.cookie, json!({"kind": "machine", "id": m})).await.1["data"]["ticket"].as_str().unwrap().to_string();
    let tk4 = ticket(&t, &t.cookie, json!({"kind": "machine", "id": m})).await.1["data"]["ticket"].as_str().unwrap().to_string();
    let mut a3 = connect(addr, Some(&t.cookie), Some(ORIGIN), None).await.unwrap();
    send_ticket(&mut a3, &tk3).await;
    sshd.wait_open(3).await;
    let mut a4 = connect(addr, Some(&t.cookie), Some(ORIGIN), None).await.unwrap();
    send_ticket(&mut a4, &tk4).await;
    assert!(until_exit(&mut a4, 5).await.1.contains("每人最多同时开 3 个"));
    let (s, v) = ticket(&t, &t.cookie, json!({"kind": "machine", "id": m})).await;
    assert_eq!((s, code_of(&v)), (StatusCode::TOO_MANY_REQUESTS, "terminal_limit"), "{v}");

    // 改窗口大小（越界的夹到 1..=500）、不认识的控制消息：不影响终端
    for msg in [r#"{"type":"resize","cols":120,"rows":40}"#, r#"{"type":"resize","cols":0,"rows":9999}"#, r#"{"type":"bell"}"#, "garbage"] {
        a1.send(Message::Text(msg.into())).await.unwrap();
    }
    // 浏览器关掉一个：对应的 ssh 立刻被杀（假服务读到 EOF，而不是等 30 秒）
    a2.close(None).await.unwrap();
    sshd.wait_open(2).await;

    // 全中控 8 个：u2 开 3 个、u3 开 2 个、admin 再开 1 个；u3 先拿好一张票
    let c2 = login(&t, "u2").await;
    let c3 = login(&t, "u3").await;
    let mut u2: Vec<Ws> = Vec::new();
    for _ in 0..3 {
        u2.push(open_term(&t, addr, &c2, m).await);
    }
    let mut u3 = [open_term(&t, addr, &c3, m).await, open_term(&t, addr, &c3, m).await];
    let tk = ticket(&t, &c3, json!({"kind": "machine", "id": m})).await.1["data"]["ticket"].as_str().unwrap().to_string();
    let mut a5 = open_term(&t, addr, &t.cookie, m).await;
    sshd.wait_open(8).await;
    assert_eq!(t.st.terminals.live(), 8);
    let mut x = connect(addr, Some(&c3), Some(ORIGIN), None).await.unwrap();
    send_ticket(&mut x, &tk).await;
    assert!(until_exit(&mut x, 5).await.1.contains("中控同时最多开 8 个"));
    let (s, v) = ticket(&t, &c3, json!({"kind": "machine", "id": m})).await;
    assert_eq!((s, code_of(&v)), (StatusCode::TOO_MANY_REQUESTS, "terminal_limit"), "{v}");

    // admin 的会话在另一台设备上被撤销：它的 3 个终端马上断（撤销通知），关闭码 4401
    let admin2 = login(&t, "admin").await;
    let (_, list) = call_as(&t, &admin2, "GET", "/api/v1/account/sessions", None).await;
    let first = list["data"].as_array().unwrap().iter().find(|s| s["current"] == false && s["id"].as_str().unwrap() == hex::encode(&session_hash(&t.cookie)[..8])).unwrap()["id"].as_str().unwrap().to_string();
    let (s, _) = call_as(&t, &admin2, "DELETE", &format!("/api/v1/account/sessions/{first}"), None).await;
    assert_eq!(s, StatusCode::OK);
    for ws in [&mut a1, &mut a3, &mut a5] {
        let (_, reason, code) = until_exit(ws, 5).await;
        assert!(reason.contains("登录会话已失效"), "{reason}");
        assert_eq!(code, Some(4401));
    }
    sshd.wait_open(5).await;

    // u2 的会话被直接删掉（没有撤销通知，比如数据库里手工删）：60 秒一次的复核（这里直接调）把它的终端断开
    sqlx::query("delete from session where id_hash = $1").bind(session_hash(&c2)).execute(&t.st.db).await.unwrap();
    assert!(!fleet_server::terminal::recheck(&t.st, &session_hash(&c2)).await.unwrap());
    for ws in u2.iter_mut() {
        assert!(until_exit(ws, 5).await.1.contains("登录会话已失效"));
    }
    sshd.wait_open(2).await;

    // 一条消息超过 64 KiB：断开
    u3[0].send(Message::Binary(vec![b'x'; 70 * 1024].into())).await.unwrap();
    let (_, reason, _) = until_exit(&mut u3[0], 5).await;
    assert!(reason.contains("太大"), "{reason}");
    u3[1].close(None).await.unwrap();
    sshd.wait_open(0).await;

    // 审计里的断开原因
    let whys: Vec<String> = audits(&t, "terminal.close").await.iter().map(|d| d["why"].as_str().unwrap().to_string()).collect();
    for w in ["client_closed", "session_invalid", "too_large"] {
        assert!(whys.iter().any(|x| x == w), "{w}：{whys:?}");
    }
    // 真正开起来过的 9 个（因名额被拒的两次没开、不写审计）
    assert_eq!(whys.len(), 9, "{whys:?}");
    for _ in 0..100 {
        if t.st.terminals.live() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("名额没有全部归还：{}", t.st.terminals.live());
}

/// 中控退出（main 收到 SIGTERM 后调 terminal::shutdown）：以「中控正在重启」断开全部终端（关闭码 1012、写「关闭」审计、ssh 结束），
/// 之后不再开票，事先拿到的票据也作废
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_closes_terminals() {
    let t = common_ops::setup().await;
    let team = seed_team(&t).await;
    let sshd = FakeSshd::start().await;
    let m = seed_machine(&t, team, 1, "vps", "active", ("127.0.0.1", sshd.port), None).await;
    let addr = serve(&t).await;
    let mut a = open_term(&t, addr, &t.cookie, m).await;
    let tk = ticket(&t, &t.cookie, json!({"kind": "machine", "id": m})).await.1["data"]["ticket"].as_str().unwrap().to_string();
    sshd.wait_open(1).await;
    fleet_server::terminal::shutdown(&t.st, Duration::from_secs(2)).await;
    assert_eq!(t.st.terminals.live(), 0, "2 秒内收完尾（名额在写完「关闭」审计后才还）");
    let close = audits(&t, "terminal.close").await;
    assert_eq!((close.len(), close[0]["why"].as_str()), (1, Some("shutdown")));
    let (_, reason, code) = until_exit(&mut a, 5).await;
    assert!(reason.contains("中控正在重启"), "{reason}");
    assert_eq!(code, Some(1012));
    sshd.wait_open(0).await;
    let (s, v) = ticket(&t, &t.cookie, json!({"kind": "machine", "id": m})).await;
    assert_eq!((s, code_of(&v)), (StatusCode::SERVICE_UNAVAILABLE, "unavailable"), "{v}");
    let mut b = connect(addr, Some(&t.cookie), Some(ORIGIN), None).await.unwrap();
    send_ticket(&mut b, &tk).await;
    assert!(until_exit(&mut b, 5).await.1.contains("票据不对"));
}

/// 实验室端到端（默认忽略）：连一个真的中控与真的机器，登录 → 开票 → 连上 → `stty size; echo ok` → 改大小 → 再看 `stty size` →
/// 打印远端 shell 的 pid，断开。环境变量：FLEET_E2E_URL（如 http://192.168.31.60:18082）、FLEET_E2E_USER、FLEET_E2E_PASSWORD、
/// FLEET_E2E_MACHINE（机器 id）。断开后远端与中控上有没有残留进程，由跑它的人另外核对
#[tokio::test]
#[ignore]
async fn lab_e2e() {
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("要设 {k}"));
    let (url, user, pw, machine) = (env("FLEET_E2E_URL"), env("FLEET_E2E_USER"), env("FLEET_E2E_PASSWORD"), env("FLEET_E2E_MACHINE"));
    let http = reqwest::Client::new();
    let res = http.post(format!("{url}/api/v1/auth/login")).json(&json!({"username": user, "password": pw})).send().await.unwrap();
    assert!(res.status().is_success(), "登录：{}", res.status());
    let cookie = res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
    let res = http
        .post(format!("{url}/api/v1/terminal/tickets"))
        .header("cookie", &cookie)
        .header("x-fleet-request", "1")
        .json(&json!({"kind": "machine", "id": machine, "password": pw}))
        .send()
        .await
        .unwrap();
    let v: Value = res.json().await.unwrap();
    let tk = v["data"]["ticket"].as_str().unwrap_or_else(|| panic!("开票：{v}")).to_string();
    let mut req = format!("{}/api/v1/terminal/ws", url.replacen("http", "ws", 1)).into_client_request().unwrap();
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    req.headers_mut().insert("origin", url.trim_end_matches('/').parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::Text(json!({"type": "auth", "ticket": tk, "cols": 100, "rows": 30}).to_string().into())).await.unwrap();
    let mut seen = String::new();
    // 输出里出现 want 为止（最多 20 秒）
    async fn wait(ws: &mut Ws, seen: &mut String, want: &str) {
        let r = tokio::time::timeout(Duration::from_secs(20), async {
            while !seen.contains(want) {
                match ws.next().await {
                    Some(Ok(Message::Binary(b))) => seen.push_str(&String::from_utf8_lossy(&b)),
                    Some(Ok(Message::Text(t))) => panic!("提前结束：{t}"),
                    Some(Ok(_)) => {}
                    other => panic!("连接断了：{other:?}"),
                }
            }
        })
        .await;
        assert!(r.is_ok(), "等不到 {want:?}，输出：{seen}");
    }
    // 像敲键盘一样发一行（回车是 \r）。等的标记都拆开写，免得回显的命令本身就匹配上
    async fn cmd(ws: &mut Ws, s: &str) { ws.send(Message::Binary(format!("{s}\r").into_bytes().into())).await.unwrap() }
    cmd(&mut ws, "stty size; echo o''k; echo PID=$$=").await;
    wait(&mut ws, &mut seen, "ok\r\n").await;
    assert!(seen.contains("30 100"), "初始大小：{seen}");
    let pid: String = seen.split("PID=").filter_map(|s| s.split('=').next()).filter(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty()).last().unwrap().to_string();
    ws.send(Message::Text(json!({"type": "resize", "cols": 120, "rows": 40}).to_string().into())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    seen.clear();
    cmd(&mut ws, "stty size; echo d''one").await;
    wait(&mut ws, &mut seen, "done\r\n").await;
    assert!(seen.contains("40 120"), "改大小之后：{seen}");
    // 远端的 TERM；ssh 的转义字符关掉了：行首的 ~. 原样交给远端 shell，连接不断
    seen.clear();
    cmd(&mut ws, "echo T=$TERM=").await;
    wait(&mut ws, &mut seen, "T=xterm-256color=").await;
    cmd(&mut ws, "~.").await;
    cmd(&mut ws, "echo s''till").await;
    wait(&mut ws, &mut seen, "still\r\n").await;
    ws.close(None).await.unwrap();
    println!("E2E_OK remote_shell_pid={pid}");
}
