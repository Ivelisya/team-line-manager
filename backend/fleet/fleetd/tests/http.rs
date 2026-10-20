//! 端到端：假的旧后端 + 假的 Komari（回放 a395 抓的真实响应），真的 fleetd HTTP 层。
//! 检查：只发 GET、带 X-Ops-Key / X-Ops-Actor、/v2/api/legacy/* 与旧接口逐字节相同、合并与并发上限、
//! 鉴权（Komari /api/me，缓存）、CSRF、SSE（hello / change / Last-Event-ID 补发）、静默与审计。
use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use fleetd::app::{spawn_background, App};
use fleetd::clock::{Clock, FakeClock};
use fleetd::config::Config;
use fleetd::legacy::normalize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn fixtures() -> Option<PathBuf> {
    let p = std::env::var("FLEETD_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/v2/live/fixtures"));
    if p.join("index.json").exists() {
        Some(p)
    } else {
        eprintln!("skip: fixtures not found at {}", p.display());
        None
    }
}

#[derive(Default)]
struct Mock {
    /// 规范化后的路径 → (状态, 响应字节)
    routes: HashMap<String, (u16, Vec<u8>)>,
    komari: HashMap<String, Value>,
    log: Mutex<Vec<(String, String, Option<String>, Option<String>)>>,
    inflight: AtomicUsize,
    peak: AtomicUsize,
    me_calls: AtomicUsize,
}

async fn mock_handler(m: Arc<Mock>, req: Request) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let q = req.uri().query().map(String::from);
    let h = req.headers();
    let key = h.get("x-ops-key").and_then(|v| v.to_str().ok()).map(String::from);
    let actor = h.get("x-ops-actor").and_then(|v| v.to_str().ok()).map(String::from);
    let cookie = h.get("cookie").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if path == "/api/me" {
        m.me_calls.fetch_add(1, Ordering::SeqCst);
        let ok = cookie.contains("session_token=good");
        return json_response(200, &json!({"logged_in": ok, "username": if ok { "admin" } else { "" }}));
    }
    if path == "/api/rpc2" {
        let body = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        m.log.lock().unwrap().push(("KOMARI".into(), v["method"].as_str().unwrap().into(), None, None));
        if !cookie.contains("session_token=good") {
            return json_response(200, &json!({"jsonrpc": "2.0", "id": 1, "error": {"code": 401, "message": "Unauthorized"}}));
        }
        let r = m.komari.get(v["method"].as_str().unwrap()).cloned().unwrap_or(json!({"error": {"message": "unknown"}}));
        return json_response(200, &r);
    }
    m.log.lock().unwrap().push((method, format!("{path}{}", q.as_ref().map(|q| format!("?{q}")).unwrap_or_default()), key, actor));
    let n = m.inflight.fetch_add(1, Ordering::SeqCst) + 1;
    m.peak.fetch_max(n, Ordering::SeqCst);
    if path.starts_with("/history") || path.starts_with("/node") {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    let k = normalize(&path, q.as_deref()).unwrap_or_default();
    let r = match m.routes.get(&k) {
        Some((st, b)) => Response::builder().status(*st).header("content-type", "application/json; charset=utf-8").body(Body::from(b.clone())).unwrap(),
        None => json_response(404, &json!({"error": "接口不存在"})),
    };
    m.inflight.fetch_sub(1, Ordering::SeqCst);
    r
}

fn json_response(st: u16, v: &Value) -> Response {
    Response::builder().status(st).header("content-type", "application/json").body(Body::from(serde_json::to_vec(v).unwrap())).unwrap()
}

/// Python json.dumps(ensure_ascii=False, separators=(",",":")) 的等价输出（旧后端的格式）
fn py_bytes(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap()
}

async fn start(fx: &PathBuf, dir: &std::path::Path) -> (Arc<Mock>, Arc<App>, String, Arc<FakeClock>) {
    let index: Value = serde_json::from_slice(&std::fs::read(fx.join("index.json")).unwrap()).unwrap();
    let mut mock = Mock::default();
    for (route, file) in index["routes"].as_object().unwrap() {
        if route.contains('|') || !route.starts_with("/api/team-ops/") {
            continue;
        }
        let rest = &route["/api/team-ops".len()..];
        let (p, q) = rest.split_once('?').map(|(a, b)| (a, Some(b))).unwrap_or((rest, None));
        let Some(k) = normalize(p, q) else { continue };
        let v: Value = serde_json::from_slice(&std::fs::read(fx.join(file.as_str().unwrap())).unwrap()).unwrap();
        mock.routes.insert(k, (v["status"].as_u64().unwrap() as u16, py_bytes(&v["body"])));
    }
    let kp = fx.join("komari-probe");
    for (m, f) in [("admin:listClients", "clients.json"), ("admin:getAllPingTasks", "ping_tasks.json"), ("common:getNodesLatestStatus", "latest.json")] {
        mock.komari.insert(m.into(), serde_json::from_slice(&std::fs::read(kp.join(f)).unwrap()).unwrap());
    }
    let mock = Arc::new(mock);
    let m2 = mock.clone();
    let mock_app = axum::Router::new().fallback(move |req: Request| mock_handler(m2.clone(), req));
    let ml = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let maddr = ml.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ml, mock_app).await.unwrap() });

    std::fs::write(dir.join("legacy.key"), "{\"key\": \"test-ops-key\"}").unwrap();
    let catalog = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../architecture/plan-catalog/catalog.json");
    let cfg = Config::parse(&format!(
        "listen = \"127.0.0.1:0\"\ndata_dir = \"{}\"\ncatalog = \"{}\"\n[legacy]\nbase_url = \"http://{maddr}\"\nkey_file = \"{}\"\n[komari]\nbase_url = \"http://{maddr}\"\n",
        dir.display(), catalog.display(), dir.join("legacy.key").display()
    ))
    .unwrap();
    let as_of: f64 = index["as_of"].as_f64().unwrap();
    let clock = Arc::new(FakeClock::new(as_of + 1.0));
    let app = App::build(cfg, clock.clone() as Arc<dyn Clock>).unwrap();
    spawn_background(&app);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let svc = fleetd::api::into_service(app.clone());
    tokio::spawn(async move { axum::serve(l, svc).await.unwrap() });
    (mock, app, format!("http://{addr}"), clock)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_to_end() {
    let Some(fx) = fixtures() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (mock, app, base, clock) = start(&fx, dir.path()).await;
    let c = client();
    let good = "session_token=good";

    // ── 鉴权 ──
    let r = c.get(format!("{base}/v2/api/legacy/overview")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"error": "请先登录管理员账户"}));
    let r = c.get(format!("{base}/v2/api/legacy/overview")).header("cookie", "session_token=bad").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // 本机直连的 /system 探活不需要会话；经代理（带 X-Forwarded-For）的需要
    assert_eq!(c.get(format!("{base}/v2/api/v1/system")).send().await.unwrap().status(), 200);
    assert_eq!(c.get(format!("{base}/v2/api/v1/system")).header("x-forwarded-for", "1.2.3.4").send().await.unwrap().status(), 401);

    // ── 逐字节兼容：所有夹具路径（komari/* 除外）──
    let mut compared = 0;
    let routes: Vec<(String, (u16, Vec<u8>))> = mock.routes.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, (st, bytes)) in &routes {
        if k.starts_with("/komari/") || k == "/schema.md" || (k.starts_with("/activity?") && k.contains("seconds=1209600")) {
            continue;
        }
        let r = c.get(format!("{base}/v2/api/legacy{k}")).header("cookie", good).send().await.unwrap();
        assert_eq!(r.status().as_u16(), *st, "{k}");
        assert_eq!(r.headers()["content-type"], "application/json; charset=utf-8");
        let body = r.bytes().await.unwrap();
        assert!(body.as_ref() == bytes.as_slice(), "{k} 响应与旧接口不一致");
        compared += 1;
    }
    assert!(compared > 100, "compared {compared}");
    let me_calls = mock.me_calls.load(Ordering::SeqCst);
    assert!(me_calls <= 3, "会话校验按 cookie 缓存 30 s（实际 {me_calls} 次）");

    // ── ETag / 304（响应体不变）与 /activity 时间窗上限 ──
    let r = c.get(format!("{base}/v2/api/legacy/overview")).header("cookie", good).send().await.unwrap();
    let tag = r.headers()["etag"].to_str().unwrap().to_string();
    let r = c.get(format!("{base}/v2/api/legacy/overview")).header("cookie", good).header("if-none-match", &tag).send().await.unwrap();
    assert_eq!(r.status(), 304);
    let r = c.get(format!("{base}/v2/api/legacy/activity?limit=10&seconds=1209600")).header("cookie", good).send().await.unwrap();
    assert_eq!(r.status(), 400);

    // ── komari/* 直接读 Komari，形状同旧接口（去掉 token）──
    let r: Value = c.get(format!("{base}/v2/api/legacy/komari/clients")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    let rows = r["data"].as_array().unwrap();
    assert_eq!(rows.len(), 41);
    assert!(rows.iter().all(|x| x.get("token").is_none()));
    let legacy_shape: Value = serde_json::from_slice(&mock.routes["/komari/clients"].1).unwrap();
    // 数值按 JSON 语义比较（Go 输出 59，Python 镜像输出 59.0）
    fn num(v: &Value) -> Value {
        match v {
            Value::Number(n) => json!(n.as_f64()),
            Value::Array(a) => Value::Array(a.iter().map(num).collect()),
            Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), num(x))).collect()),
            x => x.clone(),
        }
    }
    assert_eq!(num(&r), num(&legacy_shape), "与旧后端 /komari/clients 相同");
    let r = c.get(format!("{base}/v2/api/legacy/komari/hosts/x/metrics?range=2h")).header("cookie", good).send().await.unwrap();
    assert_eq!(r.status(), 404);

    // ── 只读：POST 被拒，且不会到达旧后端 ──
    let r = c.post(format!("{base}/v2/api/legacy/assets/edit")).header("cookie", good).header("x-fleet-request", "1").body("{}").send().await.unwrap();
    assert_eq!(r.status(), 405);
    let r = c.get(format!("{base}/v2/api/legacy/sync/pending")).header("cookie", good).send().await.unwrap();
    assert_eq!(r.status(), 404);

    // ── 合并：10 个并发的同一按需请求只打一次旧后端 ──
    let node_key = routes.iter().map(|(k, _)| k.clone()).find(|k| k.starts_with("/node?")).unwrap();
    let before = mock.log.lock().unwrap().iter().filter(|x| x.1 == node_key).count();
    // 先让缓存过期
    clock.advance(120.0);
    let mut hs = vec![];
    for _ in 0..10 {
        let c = c.clone();
        let u = format!("{base}/v2/api/legacy{node_key}");
        hs.push(tokio::spawn(async move { c.get(u).header("cookie", good).send().await.unwrap().status() }));
    }
    for h in hs {
        assert_eq!(h.await.unwrap(), 200);
    }
    let after = mock.log.lock().unwrap().iter().filter(|x| x.1 == node_key).count();
    assert_eq!(after - before, 1, "请求合并");

    // ── 并发上限 2 ──
    clock.advance(120.0);
    mock.peak.store(0, Ordering::SeqCst);
    let hist: Vec<String> = routes.iter().map(|(k, _)| k.clone()).filter(|k| k.starts_with("/history?asset=")).take(8).collect();
    let mut hs = vec![];
    for k in hist {
        let c = c.clone();
        let u = format!("{base}/v2/api/legacy{k}");
        hs.push(tokio::spawn(async move { c.get(u).header("cookie", good).send().await.unwrap().status() }));
    }
    for h in hs {
        assert_eq!(h.await.unwrap(), 200);
    }
    let peak = mock.peak.load(Ordering::SeqCst);
    assert!((2..=3).contains(&peak), "轻请求两个槽并行（+ 轮询器独占槽），实际峰值 {peak}");

    // ── SSE：hello、change、Last-Event-ID 补发 ──
    let mut r = c.get(format!("{base}/v2/api/stream")).header("cookie", good).send().await.unwrap();
    assert_eq!(r.headers()["content-type"], "text/event-stream; charset=utf-8");
    let mut buf = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    // 有客户端 → 立即进入 active 节奏（第一次取到的资源不推 change，避免客户端全量重取）；内容变化时才推
    clock.advance(30.0);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    app.hub.publish("panel", Some("/v2/api/legacy/panel/overview".into()), 1.0);
    app.hub.publish("overview", Some("/v2/api/legacy/overview".into()), 1.0);
    while !buf.contains("\"resource\":\"panel\"") && std::time::Instant::now() < deadline {
        if let Ok(Ok(Some(chunk))) = tokio::time::timeout(std::time::Duration::from_secs(2), r.chunk()).await {
            buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
    assert!(buf.starts_with("retry: 3000\n\nevent: hello\n"), "{buf}");
    assert!(buf.contains("event: change\ndata: {\"resource\":\"panel\""), "{buf}");
    let v: Value = c.get(format!("{base}/v2/api/v1/system")).send().await.unwrap().json().await.unwrap();
    assert!(v["data"]["polls"].as_array().unwrap().iter().any(|p| p["resource"] == "panel" && p["polls"].as_u64().unwrap() >= 1), "active 节奏会拉 panel");
    assert_eq!(app.hub.clients(), 1);
    let last_id = buf.lines().filter(|l| l.starts_with("id: ")).next().unwrap()[4..].to_string();
    drop(r);
    let mut r2 = c.get(format!("{base}/v2/api/stream")).header("cookie", good).header("last-event-id", &last_id).send().await.unwrap();
    let mut b2 = String::new();
    while !b2.contains("event: change") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), r2.chunk()).await.unwrap().unwrap().unwrap();
        b2.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(!b2.contains(&format!("id: {last_id}\n")), "只补发之后的帧");
    drop(r2);
    // 旧 epoch 的 id → reset
    let mut r3 = c.get(format!("{base}/v2/api/stream")).header("cookie", good).header("last-event-id", "1-5").send().await.unwrap();
    let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), r3.chunk()).await.unwrap().unwrap().unwrap();
    assert!(String::from_utf8_lossy(&chunk).contains("event: reset"));
    drop(r3);

    // ── 所有到旧后端的请求：GET、带 key 与 actor、不含 /komari/* ──
    let log = mock.log.lock().unwrap().clone();
    let legacy: Vec<_> = log.iter().filter(|x| x.0 != "KOMARI").collect();
    assert!(legacy.len() > 100);
    for (m, p, k, a) in &legacy {
        assert_eq!(m, "GET", "{p}");
        assert_eq!(k.as_deref(), Some("test-ops-key"), "{p}");
        assert_eq!(a.as_deref(), Some("fleetd"), "{p}");
        assert!(!p.starts_with("/komari/"), "{p}");
    }
    assert!(log.iter().filter(|x| x.0 == "KOMARI").all(|x| x.1 == "admin:listClients" || x.1.starts_with("common:") || x.1.starts_with("public:") || x.1 == "admin:getAllPingTasks"));

    // ── fleetd 自有写操作：CSRF、静默、审计 ──
    let body = json!({"matchers": {"relay": "线路机1"}, "starts_at": "2026-09-23 15:00", "ends_at": "2026-09-23 18:00", "tz": "Asia/Shanghai", "comment": "测试"});
    let r = c.post(format!("{base}/v2/api/v1/maintenance")).header("cookie", good).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 403, "缺 X-Fleet-Request");
    let r = c.post(format!("{base}/v2/api/v1/maintenance")).header("cookie", good).header("x-fleet-request", "1").header("sec-fetch-site", "cross-site").json(&body).send().await.unwrap();
    assert_eq!(r.status(), 403, "跨站");
    let r = c.post(format!("{base}/v2/api/v1/maintenance")).header("cookie", good).header("x-fleet-request", "1").header("sec-fetch-site", "same-origin").json(&body).send().await.unwrap();
    assert_eq!(r.status(), 201, "{:?}", r.text().await);
    let v: Value = c.get(format!("{base}/v2/api/v1/maintenance")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    let w = &v["data"]["maintenance"][0];
    assert_eq!(w["status"], "active");
    assert_eq!(w["created_by"], "admin");
    let too_long = json!({"matchers": {"relay": "线路机1"}, "duration_minutes": 8 * 24 * 60});
    let r = c.post(format!("{base}/v2/api/v1/silences")).header("cookie", good).header("x-fleet-request", "1").json(&too_long).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let id = w["id"].as_i64().unwrap();
    let r = c.delete(format!("{base}/v2/api/v1/maintenance/{id}")).header("cookie", good).header("x-fleet-request", "1").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = c.post(format!("{base}/v2/api/v1/alerts/999999/ack")).header("cookie", good).header("x-fleet-request", "1").send().await.unwrap();
    assert_eq!(r.status(), 409);
    let v: Value = c.get(format!("{base}/v2/api/v1/audit")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["chain_ok"], true);
    let actions: Vec<&str> = v["data"]["audit"].as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap()).collect();
    assert!(actions.contains(&"maintenance.create") && actions.contains(&"maintenance.delete") && actions.contains(&"catalog.import"), "{actions:?}");
    assert!(v["data"]["audit"].as_array().unwrap().iter().filter(|a| a["action"] == "maintenance.create").all(|a| a["actor"] == "admin"));

    // ── 套餐、资产、配额、系统 ──
    let v: Value = c.get(format!("{base}/v2/api/v1/plans")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["count"], 6);
    let v: Value = c.get(format!("{base}/v2/api/v1/assets")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["count"], 41);
    let in_legacy = v["data"]["assets"].as_array().unwrap().iter().filter(|a| a["in_legacy"] == true).count();
    assert_eq!(in_legacy, 41, "目录里的 41 项都能在旧系统里按名称对上");
    let v: Value = c.get(format!("{base}/v2/api/v1/quota")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    let relays = v["data"]["relays"].as_array().unwrap();
    assert_eq!(relays.len(), 3);
    assert!(relays.iter().all(|r| r["used_bytes"].is_number() && r["quota_gb"].is_number()));
    let v: Value = c.get(format!("{base}/v2/api/v1/system")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["notify"]["shadow"], true);
    assert!(v["data"]["polls"].as_array().unwrap().iter().any(|p| p["resource"] == "overview" && p["version"].as_u64().unwrap() >= 1));
    let v: Value = c.get(format!("{base}/v2/api/v1/channels")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["shadow"], true);
    let v: Value = c.get(format!("{base}/v2/api/v1/alerts?state=all")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    assert!(v["data"]["alerts"].is_array());

    // ── 探测历史：每次轮询记下最新探测结果 ──
    let v: Value = c.get(format!("{base}/v2/api/v1/probes/latest")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    let nodes = v["data"]["nodes"].as_object().unwrap();
    assert!(nodes.len() >= 30, "{}", nodes.len());
    let n = &nodes["t1-01"];
    assert_eq!(n["name"], "团队1-01");
    for k in ["tcp", "egress", "path"] {
        assert!(!n[k]["recent"].as_array().unwrap().is_empty(), "{k}");
        assert!(n[k]["last"]["status"].is_string());
    }
    let v: Value = c.get(format!("{base}/v2/api/v1/probes?node=t1-01&range=24h&raw=1")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    assert!(!v["data"]["kinds"]["tcp"]["buckets"].as_array().unwrap().is_empty());
    let u = v["data"]["node"].as_str().unwrap().to_string();
    assert_eq!(u, "t1-01");
    assert_eq!(c.get(format!("{base}/v2/api/v1/probes?node=t1-01&range=9d")).header("cookie", good).send().await.unwrap().status(), 400);

    // ── fleetd 自己的账户：登录 / 会话 / 退出 / 限速（替代 Komari 登录） ──
    let h = fleetd::accounts::komari_hash("Komari-Imported-1");
    app.store.call_blocking(move |c| fleetd::accounts::upsert_user(c, "admin", "komari-sha256", &h, None, "komari", 1.0)).unwrap();
    let login = |u: &str, p: &str| json!({"username": u, "password": p});
    let r = c.post(format!("{base}/v2/api/v1/auth/login")).json(&login("admin", "Komari-Imported-1")).send().await.unwrap();
    assert_eq!(r.status(), 403, "登录也要 X-Fleet-Request（防登录 CSRF）");
    let r = c.post(format!("{base}/v2/api/v1/auth/login")).header("x-fleet-request", "1").json(&login("admin", "bad")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = c.post(format!("{base}/v2/api/v1/auth/login")).header("x-fleet-request", "1").json(&login("admin", "Komari-Imported-1")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let sc = r.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(sc.starts_with("fleet_session=") && sc.contains("HttpOnly") && sc.contains("SameSite=Strict") && sc.contains("Secure"), "{sc}");
    let fc = sc.split(';').next().unwrap().to_string();
    let scheme: String = app.store.call_blocking(|c| c.query_row("SELECT pw_scheme FROM users WHERE username='admin'", [], |r| r.get(0)).unwrap());
    assert_eq!(scheme, "pbkdf2-sha256", "Komari 旧哈希登录后升级");
    let v: Value = c.get(format!("{base}/v2/api/v1/auth/me")).header("cookie", &fc).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["via"], "fleetd");
    assert_eq!(c.get(format!("{base}/v2/api/legacy/overview")).header("cookie", &fc).send().await.unwrap().status(), 200, "只凭 fleetd 会话");
    let v: Value = c.get(format!("{base}/v2/api/v1/auth/sessions")).header("cookie", &fc).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"]["sessions"][0]["current"], true);
    let r = c.post(format!("{base}/v2/api/v1/auth/logout")).header("cookie", &fc).header("x-fleet-request", "1").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers()["set-cookie"].to_str().unwrap().contains("Max-Age=0"));
    assert_eq!(c.get(format!("{base}/v2/api/v1/auth/me")).header("cookie", &fc).send().await.unwrap().status(), 401, "退出后会话失效");
    let v: Value = c.get(format!("{base}/v2/api/v1/audit")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    let acts: Vec<&str> = v["data"]["audit"].as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap()).collect();
    assert!(acts.contains(&"auth.login") && acts.contains(&"auth.logout") && acts.contains(&"auth.login_failed"), "{acts:?}");
    // 限速：同一来源 15 分钟内失败 5 次 → 429
    let mut last = 0;
    for _ in 0..6 {
        last = c.post(format!("{base}/v2/api/v1/auth/login")).header("x-fleet-request", "1").json(&login("admin", "nope")).send().await.unwrap().status().as_u16();
    }
    assert_eq!(last, 429);

    // ── relayd 管理模式（fleet.relayspec.v1）+ 入口操作 ──
    let tok = app.ingest.issue_token("line3", "default").unwrap();
    let bearer = format!("Bearer {tok}");
    let managed = json!({"inbounds": [{"tag": "inbound-20007", "port": 20007, "protocol": "vless",
        "settings": {"clients": [{"id": "aaaaaaaa-1111-4222-8333-444444444444", "flow": "xtls-rprx-vision", "email": "node-07"}]},
        "streamSettings": {"security": "reality", "realitySettings": {"privateKey": "cH0QzGzV4b1cM0W3Sb0V6pYv5yD6z9z0bLzY2W1XxXg", "serverNames": ["www.cloudflare.com"], "shortIds": ["ab12"]}}}],
        "outbounds": [], "routing_rules": []});
    let w0 = c.post(format!("{base}/v2/api/v1/relays/线路机3/entries/20007/actions")).header("cookie", good).header("x-fleet-request", "1")
        .json(&json!({"action": "disable"})).send().await.unwrap();
    assert_eq!(w0.status(), 409, "relayd 没来取期望状态时不下发");
    let r = c.get(format!("{base}/v2/ingest/relay/spec?have=0")).header("authorization", &bearer).send().await.unwrap();
    assert_eq!(r.status(), 304, "未接管：没有期望");
    assert_eq!(c.get(format!("{base}/v2/ingest/relay/spec")).send().await.unwrap().status(), 401);
    let r = c.post(format!("{base}/v2/ingest/relay/observed")).header("authorization", &bearer)
        .json(&json!({"schema": "fleet.relayobs.v1", "relay": "line3", "config_sha256": "s0", "generation": 0, "managed": managed, "observed_at": 1.0})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let w = |path: &str| c.post(format!("{base}/v2/api/v1/relays/线路机3/entries/20007/{path}")).header("cookie", good).header("x-fleet-request", "1");
    assert_eq!(w("actions").json(&json!({"action": "rotate_uuid"})).send().await.unwrap().status(), 400, "轮换必须 confirm");
    let r = w("actions").json(&json!({"action": "rotate_uuid", "confirm": "线路机3:20007"})).send().await.unwrap();
    assert_eq!(r.status(), 202);
    let v: Value = r.json().await.unwrap();
    let op = v["data"]["op_id"].as_str().unwrap().to_string();
    assert_eq!(v["data"]["generation"], 1);
    let spec: Value = c.get(format!("{base}/v2/ingest/relay/spec?have=0")).header("authorization", &bearer).send().await.unwrap().json().await.unwrap();
    assert_eq!(spec["generation"], 1);
    assert_eq!(spec["base_config_sha256"], "s0");
    assert_ne!(spec["managed"]["inbounds"][0]["settings"]["clients"][0]["id"], "aaaaaaaa-1111-4222-8333-444444444444");
    assert_eq!(c.get(format!("{base}/v2/ingest/relay/spec?have=1")).header("authorization", &bearer).send().await.unwrap().status(), 304);
    let r = c.post(format!("{base}/v2/ingest/relay/spec/ack")).header("authorization", &bearer)
        .json(&json!({"generation": 1, "spec_sha256": spec["spec_sha256"], "status": "applied", "config_sha256_before": "s0", "config_sha256_after": "s1",
            "diff": {"inbounds": {"changed": 1}}, "runtime": {}})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = c.get(format!("{base}/v2/api/v1/ops/{op}")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["status"], "applied");
    assert_eq!(v["data"]["result"]["diff"]["inbounds"]["changed"], 1);
    let v: Value = c.get(format!("{base}/v2/api/v1/relays/line3/entries/20007/share")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    assert!(v["data"]["share_link"].as_str().unwrap().starts_with("vless://"), "{v}");
    // 停用 / 启用
    let v: Value = w("actions").json(&json!({"action": "disable"})).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["generation"], 2);
    let spec: Value = c.get(format!("{base}/v2/ingest/relay/spec?have=1")).header("authorization", &bearer).send().await.unwrap().json().await.unwrap();
    assert_eq!(spec["managed"]["inbounds"].as_array().unwrap().len(), 0);
    assert_eq!(w("actions").json(&json!({"action": "disable"})).send().await.unwrap().status(), 409);
    let v: Value = w("actions").json(&json!({"action": "enable"})).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["generation"], 3);
    // 重启 Xray：命令
    assert_eq!(c.post(format!("{base}/v2/api/v1/relays/线路机3/actions")).header("cookie", good).header("x-fleet-request", "1")
        .json(&json!({"action": "restart_xray", "confirm": "错的"})).send().await.unwrap().status(), 400);
    let v: Value = c.post(format!("{base}/v2/api/v1/relays/线路机3/actions")).header("cookie", good).header("x-fleet-request", "1")
        .json(&json!({"action": "restart_xray", "confirm": "线路机3"})).send().await.unwrap().json().await.unwrap();
    let rop = v["data"]["op_id"].as_str().unwrap().to_string();
    let spec: Value = c.get(format!("{base}/v2/ingest/relay/spec?have=3")).header("authorization", &bearer).send().await.unwrap().json().await.unwrap();
    let cid = spec["commands"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(spec["commands"][0]["kind"], "restart_xray");
    c.post(format!("{base}/v2/ingest/relay/spec/ack")).header("authorization", &bearer)
        .json(&json!({"generation": 3, "status": "applied", "config_sha256_after": "s3", "commands": [{"id": cid, "ok": true}]})).send().await.unwrap();
    let v: Value = c.get(format!("{base}/v2/api/v1/ops/{rop}")).header("cookie", good).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["status"], "applied");
    let v: Value = c.get(format!("{base}/v2/api/v1/system")).send().await.unwrap().json().await.unwrap();
    assert!(v["data"]["features"].as_array().unwrap().iter().any(|f| f == "entries"));
}
