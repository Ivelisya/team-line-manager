//! 遥测/告警/通知测试共用的搭建代码（不起后台循环，告警评估由测试手动驱动）。
//! 放在子目录里，cargo 不会把它当成独立的测试目标。
#![allow(dead_code)]

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, Uri};
use axum::{Json, Router};
use fleet_proto::{AccessEvent, EntrySample, HostSample, ProbeKind, ProbeSample, Report, XrayStatus};
use fleet_server::{config::Config, crypto::SecretBox, db, state::AppState};
use futures::StreamExt;
use http_body_util::BodyExt;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use uuid::Uuid;

pub struct T {
    pub app: Router,
    pub st: AppState,
    pub cookie: String,
}

impl T {
    pub async fn req(&self, method: &str, path: &str, body: Option<Value>, auth: Option<&str>) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path).header("content-type", "application/json").header("x-fleet-request", "1");
        match auth {
            Some(tok) => b = b.header("authorization", format!("Bearer {tok}")),
            None => b = b.header("cookie", &self.cookie),
        }
        let req = b.body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty)).unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    pub async fn get(&self, p: &str) -> Value {
        let (s, v) = self.req("GET", p, None, None).await;
        assert_eq!(s, StatusCode::OK, "GET {p}: {v}");
        v["data"].clone()
    }
    pub async fn post(&self, p: &str, b: Value) -> Value {
        let (s, v) = self.req("POST", p, Some(b), None).await;
        assert!(s.is_success(), "POST {p}: {s} {v}");
        v["data"].clone()
    }
    /// agent 上报，返回 (状态码, 响应体)
    pub async fn report(&self, tok: &str, r: &Report) -> (StatusCode, Value) {
        self.req("POST", "/agent/v1/report", Some(serde_json::to_value(r).unwrap()), Some(tok)).await
    }
    pub async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(sql).fetch_one(&self.st.db).await.unwrap()
    }
    /// 某台机器在 /machines 里的健康状态与原因键
    pub async fn health(&self, id: Uuid) -> (String, Vec<String>) {
        let m = self.get(&format!("/api/v1/machines/{id}")).await;
        let rs = m["health"]["reasons"].as_array().unwrap().iter().map(|r| r["key"].as_str().unwrap().to_string()).collect();
        (m["health"]["status"].as_str().unwrap().to_string(), rs)
    }
    /// 把应用起在本机随机端口上，连实时流 WebSocket（`cookie` 为假则不带会话）
    pub async fn ws(&self, cookie: bool) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let app = self.app.clone();
        tokio::spawn(async move { axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap() });
        let mut req = format!("ws://{addr}/api/v1/stream").into_client_request().unwrap();
        if cookie {
            req.headers_mut().insert("cookie", self.cookie.parse().unwrap());
        }
        tokio_tungstenite::connect_async(req).await.map(|(s, _)| s)
    }
}

pub type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 5 秒内读到第一条 `type` 为 `kind` 的消息（其余消息跳过）
pub async fn ws_next(ws: &mut Ws, kind: &str) -> Value {
    let read = async {
        while let Some(m) = ws.next().await {
            if let Message::Text(s) = m.unwrap() {
                let v: Value = serde_json::from_str(&s).unwrap();
                if v["type"] == kind {
                    return v;
                }
            }
        }
        panic!("实时流断开，没等到 {kind}");
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), read).await.unwrap_or_else(|_| panic!("5 秒内没等到 {kind}"))
}

/// 清空测试库并起一个不带后台循环的应用，登录好 admin
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
           job_workers = 1"#
    ))
    .unwrap();
    let st = AppState::new(cfg, pool.clone(), SecretBox::from_key(&[9u8; 32]).unwrap());
    fleet_server::auth::upsert_user(&pool, "admin", "correct horse battery").await.unwrap();
    let app = fleet_server::app(st.clone());
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(json!({"username": "admin", "password": "correct horse battery"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
    T { app, st, cookie }
}

/// 在库里放一台在用的线路机，并签一个 agent 令牌
pub async fn seed_relay(t: &T, label: &str, ip: &str, tok: &str) -> Uuid {
    let id = Uuid::now_v7();
    let (sk, pk) = fleet_server::crypto::reality_keypair();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle, activated_at) values ($1, 'relay', $2, $3::inet, 'active', now())")
        .bind(id)
        .bind(label)
        .bind(ip)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into relay (machine_id, public_host, reality_private_enc, reality_public_key, generation, applied_generation, applied_at) values ($1, 'relay.test', $2, $3, 1, 1, now())")
        .bind(id)
        .bind(t.st.secrets.seal_str(&sk))
        .bind(pk)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into agent_token (id, machine_id, token_hash) values ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(id)
        .bind(fleet_server::crypto::sha256(tok.as_bytes()))
        .execute(&t.st.db)
        .await
        .unwrap();
    id
}

pub async fn seed_team(t: &T, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into team (id, name) values ($1, $2)").bind(id).bind(name).execute(&t.st.db).await.unwrap();
    id
}

/// 一个上游代理节点（无 agent）和挂在 `relay` 上的一条在用入口。返回 (节点, 入口)
pub async fn seed_node(t: &T, relay: Uuid, team: Uuid, number: i32, ip: &str, port: i32) -> (Uuid, Uuid) {
    let node = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, lifecycle, activated_at) values ($1, 'proxy', $2, $3, $4::inet, 'active', now())")
        .bind(node)
        .bind(team)
        .bind(number)
        .bind(ip)
        .execute(&t.st.db)
        .await
        .unwrap();
    let entry = Uuid::now_v7();
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, $4, $5, 'abcd', 'active')")
        .bind(entry)
        .bind(relay)
        .bind(node)
        .bind(port)
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();
    (node, entry)
}

pub fn now_ms() -> i64 { (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64 }

pub fn host(ts_ms: i64, cpu: f32, rx: u64, tx: u64) -> HostSample {
    HostSample { ts_ms, cpu_pct: cpu, load1: Some(0.5), mem_used: 400, mem_total: 1000, disk_used: 300, disk_total: 1000, net_rx_bytes: rx, net_tx_bytes: tx, tcp_conns: 3 }
}

pub fn entry_sample(entry_id: Uuid, ts_ms: i64, up: u64, down: u64) -> EntrySample {
    EntrySample { entry_id, ts_ms, up_bytes: up, down_bytes: down, conns: 2 }
}

pub fn probe(entry_id: Uuid, ts_ms: i64, kind: ProbeKind, ok: bool) -> ProbeSample {
    ProbeSample { entry_id, ts_ms, kind, ok, latency_ms: Some(10.0), detail: Some(if ok { "203.0.113.1".into() } else { "timeout".into() }) }
}

pub fn access(entry_id: Uuid, ts_ms: i64) -> AccessEvent {
    AccessEvent { entry_id, ts_ms, src_ip: Some("198.51.100.9".parse().unwrap()), dest_host: "example.com".into(), dest_port: Some(443), network: "tcp".into(), blocked: false }
}

pub fn xray(running: bool) -> XrayStatus {
    XrayStatus { running, version: Some("1.8.0".into()), started_at_ms: None, applied_generation: 1 }
}

/// 空报告；调用方按需往里填
pub fn blank_report(seq: u64) -> Report {
    Report {
        seq,
        boot_id: "b1".into(),
        agent_version: "test".into(),
        sent_at_ms: now_ms(),
        info: None,
        host: vec![],
        entries: vec![],
        probes: vec![],
        access: vec![],
        xray: None,
        agent_health: None,
    }
}

/// 本机 HTTP 假服务：记下每次请求的 (路径, JSON 体)。
/// 路径含 `fail` 回 500；含 `bad` 回企业微信风格的 errcode 错误；含 `bot` 回 Telegram 风格；其余回 200。
pub type Hits = Arc<Mutex<Vec<(String, Value)>>>;

pub struct Mock {
    pub addr: SocketAddr,
    pub hits: Hits,
}

impl Mock {
    pub async fn start() -> Mock {
        let hits: Hits = Arc::default();
        async fn handler(State(h): State<Hits>, uri: Uri, body: Option<Json<Value>>) -> (StatusCode, Json<Value>) {
            let path = uri.path().to_string();
            h.lock().unwrap().push((path.clone(), body.map(|b| b.0).unwrap_or(Value::Null)));
            if path.contains("fail") {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({})))
            } else if path.contains("bad") {
                (StatusCode::OK, Json(json!({"errcode": 93000, "errmsg": "invalid webhook url"})))
            } else if path.contains("/bot") {
                (StatusCode::OK, Json(json!({"ok": true, "result": {}})))
            } else if path.contains("wecom") {
                (StatusCode::OK, Json(json!({"errcode": 0, "errmsg": "ok"})))
            } else {
                (StatusCode::OK, Json(json!({})))
            }
        }
        let app = Router::new().fallback(handler).with_state(hits.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Mock { addr, hits }
    }
    pub fn url(&self, path: &str) -> String { format!("http://{}{}", self.addr, path) }
    pub fn count(&self, path_part: &str) -> usize { self.hits.lock().unwrap().iter().filter(|(p, _)| p.contains(path_part)).count() }
    /// 等某路径收到至少 `n` 次请求（通知是后台任务发的），最多等 5 秒
    pub async fn wait(&self, path_part: &str, n: usize) {
        for _ in 0..100 {
            if self.count(path_part) >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("等不到 {path_part} 的 {n} 次请求，实际 {:?}", self.hits.lock().unwrap());
    }
    pub fn bodies(&self, path_part: &str) -> Vec<Value> {
        self.hits.lock().unwrap().iter().filter(|(p, _)| p.contains(path_part)).map(|(_, b)| b.clone()).collect()
    }
}
