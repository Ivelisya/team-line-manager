//! HTTP：/v2/api/legacy/*（旧接口字节兼容）、/v2/api/stream（SSE）、/v2/api/v1/*（fleetd 自有）。
use crate::alerts::Matcher;
use crate::app::{alert_from_row, alert_json, get_alert_row, persist_alert, App};
use crate::auth::{direct_local, AuthError, Identity};
use crate::catalog;
use crate::komari::MeResult;
use crate::legacy::normalize;
use crate::sse::{encode_event, parse_last_event_id};
use crate::store;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path, Query, RawQuery, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Router};
use chrono::TimeZone;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use rusqlite::OptionalExtension;

pub type AppState = Arc<App>;

pub fn json_resp(status: u16, v: &Value) -> Response {
    let body = serde_json::to_vec(v).unwrap();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

pub fn err(status: u16, msg: &str) -> Response {
    json_resp(status, &json!({"error": msg}))
}

fn data(v: Value) -> Response {
    json_resp(200, &json!({"data": v}))
}

pub fn router(app: AppState) -> Router {
    let mut r = Router::new()
        .route("/v2/api/legacy/{*path}", get(legacy_proxy))
        .route("/v2/api/stream", get(stream))
        .route("/v2/api/v1/plans", get(plans))
        .route("/v2/api/v1/assets", get(assets))
        .route("/v2/api/v1/quota", get(quota))
        .route("/v2/api/v1/alerts", get(alerts_list))
        .route("/v2/api/v1/alerts/{id}", get(alert_get))
        .route("/v2/api/v1/alerts/{id}/ack", post(alert_ack))
        .route("/v2/api/v1/silences", get(silences_list).post(silence_create))
        .route("/v2/api/v1/silences/{id}", delete(silence_delete))
        .route("/v2/api/v1/maintenance", get(maint_list).post(maint_create))
        .route("/v2/api/v1/maintenance/{id}", delete(silence_delete))
        .route("/v2/api/v1/notify-log", get(notify_log))
        .route("/v2/api/v1/channels", get(channels))
        .route("/v2/api/v1/system", get(system))
        .route("/v2/api/v1/audit", get(audit_list))
        .route("/v2/api/v1/probes", get(probes_series))
        .route("/v2/api/v1/probes/latest", get(probes_latest))
        .route("/v2/api/v1/ipintel/backfill", get(ipintel_backfill_status))
        .route("/v2/api/v1/health/shadow", get(health_shadow))
        .route("/v2/api/v1/onboard/{*rest}", axum::routing::any(onboard_proxy))
        .route("/v2/api/v1/shadow/legacy", get(shadow_legacy))
        .route("/v2/api/v1/debug/profile", get(debug_profile))
        .route("/v2/api/v1/debug/mem", get(debug_mem))
        .route("/v2/api/v1/labels", get(labels))
        .route("/v2/api/v1/activity/delta", get(activity_delta))
        .route("/v2/api/v1/delta/{res}", get(resource_delta))
        .route("/v2/api/v1/legacy-write/{*rest}", post(legacy_write))
        .route("/v2/api/v1/ops", get(ops_list))
        .route("/v2/api/v1/ops/{id}", get(op_get))
        .route("/v2/api/v1/relays/{id}/spec", get(relay_spec_view))
        .route("/v2/api/v1/relays/{id}/actions", post(relay_action))
        .route("/v2/api/v1/relays/{id}/entries/{port}/actions", post(entry_action))
        .route("/v2/api/v1/relays/{id}/entries/{port}/share", get(entry_share))
        .route("/v2/api/v1/auth/login", post(auth_login))
        .route("/v2/api/v1/auth/logout", post(auth_logout))
        .route("/v2/api/v1/auth/me", get(auth_me))
        .route("/v2/api/v1/auth/password", post(auth_password))
        .route("/v2/api/v1/auth/totp/setup", post(auth_totp_setup))
        .route("/v2/api/v1/auth/totp/enable", post(auth_totp_enable))
        .route("/v2/api/v1/auth/totp/disable", post(auth_totp_disable))
        .route("/v2/api/v1/auth/sessions", get(auth_sessions))
        .route("/v2/api/v1/auth/sessions/{id}", delete(auth_session_revoke))
        .merge(fleetd_ingest::api::read_routes::<AppState>(app.ingest.clone()));
    if let Some(ii) = &app.ipintel {
        r = r.merge(ipintel::api::router::<AppState>(ii.clone()));
    }
    r.fallback(not_found)
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

async fn not_found() -> Response {
    err(404, "接口不存在")
}

/// 会话鉴权：先认 fleetd 自己的会话（Cookie fleet_session），再（Komari 下线前）认 Komari 管理员会话。
pub async fn authenticate(app: &App, headers: &HeaderMap, peer: SocketAddr) -> Result<Identity, AuthError> {
    let now = app.clock.now();
    if let Some(tok) = crate::auth::cookie_value(headers, crate::accounts::COOKIE) {
        if let Some(u) = app.fleet_session(&tok, now).await {
            return Ok(Identity { username: u, session: None, via: "fleetd", fleet_token: Some(tok) });
        }
    }
    let komari_off = !app.cfg.auth.komari_fallback || !app.cfg.komari.enabled;
    if app.auth.mode == crate::config::AuthMode::Fleetd || (app.auth.mode == crate::config::AuthMode::Komari && komari_off) {
        return Err(AuthError::Unauthorized);
    }
    let k = &app.komari;
    app.auth
        .check(headers, peer, now, |tok| async move {
            match k.me(Some(&tok)).await {
                Ok(MeResult::Admin(u)) => Ok(Some(u)),
                Ok(MeResult::NotLoggedIn) => Ok(None),
                Err(e) => Err(e),
            }
        })
        .await
}

/// 鉴权 + CSRF + 统一安全响应头。
async fn guard(State(app): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if !path.starts_with("/v2/api/") {
        return secure(err(404, "接口不存在"));
    }
    let method = req.method().clone();
    let headers = req.headers().clone();
    let now = app.clock.now();
    let _ = now;
    let local_diag = path == "/v2/api/v1/system" || path == "/v2/api/v1/health/shadow" || path == "/v2/api/v1/shadow/legacy" || path == "/v2/api/v1/debug/profile" || path == "/v2/api/v1/debug/mem"
        // 本机运维观察 SSE（只有资源名 / 版本 / 路径，不含数据）
        || path == "/v2/api/stream";
    let is_login = path == "/v2/api/v1/auth/login" && method == Method::POST;
    let ident = if local_diag && method == Method::GET && direct_local(&headers, peer) {
        // 本机直接探活 / 排障（install 脚本、运维在中控上的 curl）；经 Caddy 转发的请求一定带 X-Forwarded-For，走不到这里
        Ok(Identity { username: "local-ops".into(), session: None, via: "local", fleet_token: None })
    } else if is_login {
        Ok(Identity { username: "anonymous".into(), session: None, via: "anonymous", fleet_token: None })
    } else {
        authenticate(&app, &headers, peer).await
    };
    let ident = match ident {
        Ok(i) => i,
        Err(AuthError::Unauthorized) => return secure(err(401, "请先登录管理员账户")),
        Err(AuthError::Unavailable(e)) => {
            tracing::warn!(error = %e, "session check unavailable");
            return secure(err(503, "无法验证登录状态，请稍后重试"));
        }
    };
    if method != Method::GET && method != Method::HEAD {
        if path.starts_with("/v2/api/legacy/") {
            return secure(err(405, "旧系统接口只读"));
        }
        if headers.get("x-fleet-request").and_then(|v| v.to_str().ok()) != Some("1") {
            return secure(err(403, "请求来源校验失败"));
        }
        if let Some(s) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
            if s != "same-origin" && s != "none" {
                return secure(err(403, "请求来源校验失败"));
            }
        }
    }
    let close_anomaly = method == Method::POST && path.starts_with("/v2/api/v1/ipintel/anomalies/") && path.ends_with("/close");
    let actor = ident.username.clone();
    req.extensions_mut().insert(ipintel::api::Actor(ident.username.clone()));
    req.extensions_mut().insert(ident);
    let t_req = std::time::Instant::now();
    let q_part = req.uri().query().map(|q| q.split('&').filter_map(|kv| kv.split('=').next()).collect::<Vec<_>>().join(",")).unwrap_or_default();
    let resp = next.run(req).await;
    {
        // 按路径（数字、uuid 归一）统计请求数与耗时：/system 的 requests
        let norm: String = path.split('/').map(|seg| if seg.len() >= 8 && seg.chars().any(|c| c.is_ascii_digit()) && seg.contains('-') { ":id" } else { seg }).collect::<Vec<_>>().join("/");
        let key = if q_part.is_empty() { norm } else { format!("{norm}?{q_part}") };
        let ms = t_req.elapsed().as_secs_f64() * 1000.0;
        let mut g = app.req_stats.lock().unwrap();
        if g.len() < 400 || g.contains_key(&key) {
            let e = g.entry(key).or_insert((0, 0.0, Default::default()));
            e.0 += 1;
            e.1 += ms;
            e.2.push_back(ms);
            if e.2.len() > 200 {
                e.2.pop_front();
            }
        }
    }
    if close_anomaly && resp.status().is_success() {
        // ipintel 的“关闭异常”写审计（出口 IP 变更只能这样结束）
        let id = path.trim_start_matches("/v2/api/v1/ipintel/anomalies/").trim_end_matches("/close").to_string();
        let now = app.clock.now();
        let _ = app.store.call(move |c| store::audit(c, now, &actor, "ipintel.anomaly.close", &format!("anomaly:{id}"), &json!({}))).await;
        app.hub.publish("ipintel", Some("/v2/api/v1/ipintel/anomalies".into()), now);
    }
    secure(resp)
}

fn secure(mut r: Response) -> Response {
    let h = r.headers_mut();
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    r
}

// ─── /v2/api/legacy/* ──────────────────────────────────────────────────────────────────────────────

/// 按需代理 /activity 的时间窗上限（14 d 的查询在旧后端上要 16–20 s）
pub const ACTIVITY_MAX_SECONDS: i64 = 7 * 86400;

fn etag_of(e: &crate::cache::Entry) -> String {
    format!("\"{}\"", hex::encode(&e.hash[..12]))
}

fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).map(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*")).unwrap_or(false)
}

async fn legacy_proxy(State(app): State<AppState>, Extension(id): Extension<Identity>, Path(path): Path<String>, RawQuery(q): RawQuery,
    req_headers: HeaderMap) -> Response {
    let Some(key) = normalize(&path, q.as_deref()) else {
        return err(404, "接口不存在");
    };
    if key.starts_with("/activity?") || key == "/activity" {
        let secs = form_urlencoded::parse(key.split_once('?').map(|x| x.1).unwrap_or("").as_bytes())
            .find(|(k, _)| k == "seconds")
            .and_then(|(_, v)| v.parse::<f64>().ok());
        if secs.map(|s| s > ACTIVITY_MAX_SECONDS as f64).unwrap_or(false) {
            return err(400, "seconds 不能超过 604800（7 天）：更长的时间窗在旧后端上太慢，请分段查询");
        }
    }
    app.proxy_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ttl = app.ttl_for(&key);
    let (res, hit) = if key.starts_with("/komari/") && (id.session.is_none() || !app.cfg.komari.enabled) {
        // 没有 Komari 会话（fleetd 账户）：原生等价（nodeagent / relayd 探测 / 原生旧库）
        let (a2, k2, now) = (app.clone(), key.clone(), app.clock.now());
        let r = tokio::task::spawn_blocking(move || {
            let (p, q) = crate::legacy::split_key(&k2);
            let params: std::collections::BTreeMap<String, String> = form_urlencoded::parse(q.as_bytes()).into_owned().collect();
            crate::komarinative::render(&a2, p, &params, now)
        })
        .await
        .ok()
        .flatten();
        match r {
            Some((st, v)) => (Ok(std::sync::Arc::new(crate::cache::Entry::new(st, bytes::Bytes::from(crate::compat::pyjson::dumps(&v)), now, 0.0))), false),
            None => (Err("接口不存在".to_string()), false),
        }
    } else if key.starts_with("/komari/") {
        app.komari.legacy_shape(&key, id.session.as_deref(), ttl).await
    } else {
        app.legacy.get(&key, ttl).await
    };
    if hit {
        app.proxy_hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    match res {
        Ok(e) => {
            let age = (app.clock.now() - e.fetched_at).max(0.0);
            if key == "/schema.md" && e.status == 200 {
                let md = serde_json::from_slice::<Value>(&e.body)
                    .ok()
                    .and_then(|v| v.pointer("/data/markdown").and_then(|m| m.as_str()).map(String::from))
                    .unwrap_or_default();
                return Response::builder()
                    .status(200)
                    .header(header::CONTENT_TYPE, "text/markdown; charset=utf-8")
                    .body(Body::from(md))
                    .unwrap();
            }
            let tag = etag_of(&e);
            if e.status == 200 && not_modified(&req_headers, &tag) {
                return Response::builder().status(304).header(header::ETAG, tag).header("x-fleet-age", format!("{age:.1}"))
                    .body(Body::empty()).unwrap();
            }
            Response::builder()
                .status(e.status)
                .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                .header(header::ETAG, tag)
                .header("x-fleet-cache", if hit { "hit" } else { "miss" })
                .header("x-fleet-age", format!("{age:.1}"))
                .body(Body::from(e.body.clone()))
                .unwrap()
        }
        Err(e) => {
            tracing::warn!(key = %key, error = %e, "legacy proxy failed");
            err(503, "运营服务暂不可用，请稍后重试")
        }
    }
}

// ─── SSE ───────────────────────────────────────────────────────────────────────────────────────────

async fn stream(State(app): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let epoch = app.hub.epoch;
    let last = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .or_else(|| q.get("last_event_id").cloned())
        .and_then(|s| parse_last_event_id(&s, epoch));
    // 本机直连（guard 已按本机运维放行）：不跟会话走，心跳时不做会话复核
    let local = direct_local(&headers, peer);
    let was_idle = app.hub.clients() == 0;
    let guard = app.hub.client_guard();
    if was_idle {
        app.poll_wake.notify_one();
    }
    let sub = app.hub.subscribe(last);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(64);
    let resources: serde_json::Map<String, Value> =
        app.polls.iter().map(|p| (p.name.to_string(), json!(format!("/v2/api/legacy{}", p.key)))).collect();
    let app2 = app.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let mut sub = sub;
        let hello = json!({"epoch": epoch, "last_id": format!("{}-{}", epoch, sub.last_id), "versions": sub.versions,
            "resources": resources, "extra": {"alerts": "/v2/api/v1/alerts", "notify": "/v2/api/v1/notify-log", "silences": "/v2/api/v1/silences",
                "probes": "/v2/api/v1/probes/latest", "ipintel": "/v2/api/v1/ipintel/anomalies", "policies": "/v2/api/legacy/policies"},
            "heartbeat_s": 15});
        let mut first = format!("retry: 3000\n\nevent: hello\ndata: {hello}\n\n");
        if sub.reset {
            first.push_str(&format!("event: reset\ndata: {}\n\n", json!({"reason": "last_event_id_too_old"})));
        }
        for ev in &sub.replay {
            first.push_str(&encode_event(ev, epoch));
        }
        if tx.send(Ok(Bytes::from(first))).await.is_err() {
            return;
        }
        let mut hb = tokio::time::interval(Duration::from_secs(15));
        hb.tick().await;
        loop {
            let chunk = tokio::select! {
                // 客户端断开（响应体被丢弃）→ 立刻结束，客户端计数随之减一
                _ = tx.closed() => break,
                r = sub.rx.recv() => match r {
                    Ok(ev) => encode_event(&ev, epoch),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => format!("event: reset\ndata: {}\n\n", json!({"reason": "lagged"})),
                    Err(_) => break,
                },
                _ = hb.tick() => {
                    // 长连接也要跟着会话走：会话失效就断开
                    let now = app2.clock.now();
                    let ok = if local { Ok(()) } else { authenticate(&app2, &headers, peer).await.map(|_| ()) };
                    if matches!(ok, Err(AuthError::Unauthorized)) {
                        let _ = tx.send(Ok(Bytes::from(format!("event: auth\ndata: {}\n\n", json!({"error": "请先登录管理员账户"}))))).await;
                        break;
                    }
                    format!("event: heartbeat\ndata: {}\n\n", json!({"ts": now, "clients": app2.hub.clients()}))
                }
            };
            if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                break;
            }
        }
    });
    let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        // no-transform：Caddy 的 encode 不压缩这条流（逐帧即时送达）
        .header(header::CACHE_CONTROL, "no-store, no-transform")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap()
}

// ─── /v1：套餐、资产、配额 ────────────────────────────────────────────────────────────────────────

async fn plans(State(app): State<AppState>) -> Response {
    let c = app.catalog.read().unwrap().clone();
    data(json!({"plans": c.plans, "count": c.plans.len()}))
}

async fn assets(State(app): State<AppState>) -> Response {
    let ov = app.overview_data().await;
    let c = app.catalog.read().unwrap().clone();
    data(catalog::assets_view(&c, ov.as_ref(), app.clock.now()))
}

async fn quota(State(app): State<AppState>) -> Response {
    let ov = app.overview_data().await;
    let kc = if app.cfg.komari.enabled {
        app.komari.cache.peek("/komari/clients").and_then(|e| serde_json::from_slice::<Value>(&e.body).ok()).and_then(|v| v.get("data").cloned())
    } else {
        let (a2, now) = (app.clone(), app.clock.now());
        tokio::task::spawn_blocking(move || crate::komarinative::render(&a2, "/komari/clients", &Default::default(), now))
            .await
            .ok()
            .flatten()
            .and_then(|(_, v)| v.get("data").cloned())
    };
    let c = app.catalog.read().unwrap().clone();
    data(catalog::quota_view(&c, ov.as_ref(), kc.as_ref(), app.clock.now()))
}

// ─── /v1：事项（告警） ────────────────────────────────────────────────────────────────────────────

async fn alerts_list(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let state = q.get("state").cloned().unwrap_or_else(|| "open".into());
    let cond = match state.as_str() {
        "open" => "state IN ('pending','firing')",
        "firing" => "state='firing'",
        "pending" => "state='pending'",
        "acked" => "state='firing' AND ack_at IS NOT NULL",
        "unacked" => "state='firing' AND ack_at IS NULL",
        "resolved" => "state='resolved' AND fired_at IS NOT NULL",
        "all" => "1=1",
        _ => return err(400, "state 取值：open|firing|pending|acked|unacked|resolved|all"),
    };
    let limit: i64 = q.get("limit").and_then(|x| x.parse().ok()).unwrap_or(200).clamp(1, 1000);
    let severity = q.get("severity").cloned();
    let rule = q.get("rule").cloned();
    let relay = q.get("relay").cloned();
    let node = q.get("node").cloned();
    let ui = app.cfg.ui_base_url.clone();
    let sql = format!(
        "SELECT * FROM alerts WHERE {cond} AND (?1 IS NULL OR severity=?1) AND (?2 IS NULL OR rule=?2)
         AND (?3 IS NULL OR json_extract(labels,'$.relay')=?3) AND (?4 IS NULL OR json_extract(labels,'$.node')=?4)
         ORDER BY CASE state WHEN 'firing' THEN 0 WHEN 'pending' THEN 1 ELSE 2 END,
                  CASE severity WHEN 'critical' THEN 0 WHEN 'warning' THEN 1 ELSE 2 END, coalesce(fired_at,pending_since) DESC, id DESC LIMIT ?5"
    );
    let (rows, counts) = app
        .store
        .call(move |c| {
            let mut st = c.prepare(&sql)?;
            let rows: Vec<Value> = st
                .query_map(rusqlite::params![severity, rule, relay, node, limit], alert_from_row)?
                .filter_map(Result::ok)
                .map(|a| alert_json(&a, &ui))
                .collect();
            let counts: Value = c.query_row(
                "SELECT sum(state='pending'), sum(state='firing'), sum(state='firing' AND ack_at IS NOT NULL),
                        sum(state='firing' AND suppressed IS NOT NULL), sum(state='firing' AND severity='critical'),
                        sum(state='firing' AND severity='warning'), sum(state='firing' AND severity='info') FROM alerts",
                [],
                |r| {
                    Ok(json!({"pending": r.get::<_, Option<i64>>(0)?.unwrap_or(0), "firing": r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        "acked": r.get::<_, Option<i64>>(2)?.unwrap_or(0), "suppressed": r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        "critical": r.get::<_, Option<i64>>(4)?.unwrap_or(0), "warning": r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                        "info": r.get::<_, Option<i64>>(6)?.unwrap_or(0)}))
                },
            )?;
            Ok::<_, rusqlite::Error>((rows, counts))
        })
        .await
        .unwrap_or_default();
    data(json!({"alerts": rows, "counts": counts, "state": state, "shadow": app.notifier.is_shadow()}))
}

async fn alert_get(State(app): State<AppState>, Path(id): Path<i64>) -> Response {
    let ui = app.cfg.ui_base_url.clone();
    match app.store.call(move |c| get_alert_row(c, id)).await {
        Some(a) => {
            let aid = a.id;
            let notes = app
                .store
                .call(move |c| {
                    let mut st = c.prepare("SELECT id,ts,channel,status,kind,title FROM notify_log WHERE EXISTS (SELECT 1 FROM json_each(alert_ids) WHERE value=?) ORDER BY id DESC LIMIT 50")?;
                    let v: Vec<Value> = st
                        .query_map([aid], |r| Ok(json!({"id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?, "channel": r.get::<_, String>(2)?,
                            "status": r.get::<_, String>(3)?, "kind": r.get::<_, String>(4)?, "title": r.get::<_, String>(5)?})))?
                        .filter_map(Result::ok)
                        .collect();
                    Ok::<_, rusqlite::Error>(v)
                })
                .await
                .unwrap_or_default();
            let mut v = alert_json(&a, &ui);
            v["notifications"] = json!(notes);
            data(v)
        }
        None => err(404, "告警不存在"),
    }
}

fn parse_body(body: &Bytes) -> Result<Value, Response> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    if body.len() > 65536 {
        return Err(err(413, "请求过大"));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(v @ Value::Object(_)) => Ok(v),
        _ => Err(err(400, "请求必须是 JSON 对象")),
    }
}

async fn alert_ack(State(app): State<AppState>, Extension(id): Extension<Identity>, Path(aid): Path<i64>, body: Bytes) -> Response {
    let b = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let note: String = b.get("note").and_then(|x| x.as_str()).unwrap_or("").chars().take(500).collect();
    let now = app.clock.now();
    let r = app.engine.lock().unwrap().ack(aid, &id.username, now);
    match r {
        Ok(a) => {
            let user = id.username.clone();
            let a2 = a.clone();
            let res = app
                .store
                .call(move |c| {
                    let tx = c.transaction()?;
                    persist_alert(&tx, &a2)?;
                    store::audit(&tx, now, &user, "alert.ack", &format!("alert:{}", a2.id),
                        &json!({"note": note, "rule": a2.rule, "labels": a2.labels, "severity": a2.severity}))?;
                    tx.commit()
                })
                .await;
            if let Err(e) = res {
                return err(500, &format!("保存失败：{e}"));
            }
            app.hub.publish("alerts", Some("/v2/api/v1/alerts".into()), now);
            data(alert_json(&a, &app.cfg.ui_base_url))
        }
        Err(e) => err(409, &e),
    }
}

// ─── /v1：静默与维护窗口 ──────────────────────────────────────────────────────────────────────────

const MATCHER_LABELS: &[&str] = &["node", "relay", "relay_id", "provider", "team", "cause", "kind", "asset", "check", "channel", "entry", "threshold", "scope"];
const MAX_WINDOW_S: f64 = 7.0 * 86400.0;

fn parse_when(v: Option<&Value>, tz: chrono_tz::Tz, default: Option<f64>) -> Result<f64, String> {
    match v {
        None | Some(Value::Null) => default.ok_or_else(|| "缺少时间".to_string()),
        Some(Value::Number(n)) => n.as_f64().ok_or_else(|| "时间无效".into()),
        Some(Value::String(s)) => {
            if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
                return Ok(d.timestamp() as f64);
            }
            for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
                if let Ok(n) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
                    return tz.from_local_datetime(&n).earliest().map(|d| d.timestamp() as f64).ok_or_else(|| "该时区里不存在这个时刻".into());
                }
            }
            Err(format!("无法解析时间：{s}（用 \"YYYY-MM-DD HH:MM\" 加 tz，或 Unix 秒）"))
        }
        _ => Err("时间无效".into()),
    }
}

fn parse_matchers(v: Option<&Value>) -> Result<Vec<Matcher>, String> {
    let mut out = vec![];
    match v {
        Some(Value::Object(m)) => {
            for (k, x) in m {
                out.push(Matcher { name: k.clone(), value: x.as_str().ok_or("匹配值必须是字符串")?.to_string(), negate: false });
            }
        }
        Some(Value::Array(xs)) => {
            for x in xs {
                let m: Matcher = serde_json::from_value(x.clone()).map_err(|_| "匹配条件格式：{name, value, negate?}")?;
                out.push(m);
            }
        }
        _ => return Err("缺少 matchers（例如 {\"relay\": \"线路机1\"}）".into()),
    }
    if out.is_empty() || out.len() > 8 {
        return Err("matchers 须有 1–8 条".into());
    }
    for m in &out {
        if !MATCHER_LABELS.contains(&m.name.as_str()) {
            return Err(format!("不支持的标签 {}（可用：{}）", m.name, MATCHER_LABELS.join(", ")));
        }
        if m.value.is_empty() || m.value.len() > 200 {
            return Err("匹配值不能为空".into());
        }
    }
    if out.iter().all(|m| m.negate) {
        return Err("至少要有一条正向匹配".into());
    }
    Ok(out)
}

async fn create_window(app: AppState, id: Identity, body: Bytes, kind: &str) -> Response {
    let b = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let kind = b.get("kind").and_then(|x| x.as_str()).unwrap_or(kind).to_string();
    if kind != "silence" && kind != "maintenance" {
        return err(400, "kind 取值：silence|maintenance");
    }
    let tzname = b.get("tz").and_then(|x| x.as_str()).unwrap_or("Asia/Shanghai").to_string();
    let Ok(tz) = tzname.parse::<chrono_tz::Tz>() else {
        return err(400, &format!("时区无效：{tzname}"));
    };
    let now = app.clock.now();
    let matchers = match parse_matchers(b.get("matchers")) {
        Ok(m) => m,
        Err(e) => return err(400, &e),
    };
    let starts = match parse_when(b.get("starts_at"), tz, Some(now)) {
        Ok(t) => t,
        Err(e) => return err(400, &e),
    };
    let ends = match (b.get("ends_at"), b.get("duration_minutes").and_then(|x| x.as_f64())) {
        (Some(v), _) if !v.is_null() => match parse_when(Some(v), tz, None) {
            Ok(t) => t,
            Err(e) => return err(400, &e),
        },
        (_, Some(m)) => starts + m * 60.0,
        _ => return err(400, "缺少 ends_at 或 duration_minutes"),
    };
    if ends <= starts || ends <= now {
        return err(400, "结束时间必须晚于开始时间和当前时间");
    }
    if ends - starts > MAX_WINDOW_S {
        return err(400, "时间窗不能超过 7 天");
    }
    let comment: String = b.get("comment").and_then(|x| x.as_str()).unwrap_or("").chars().take(500).collect();
    let user = id.username.clone();
    let mjson = serde_json::to_string(&matchers).unwrap();
    let k2 = kind.clone();
    let res = app
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO silences(kind,matchers,starts_at,ends_at,tz,comment,created_by,created_at) VALUES(?,?,?,?,?,?,?,?)",
                rusqlite::params![k2, mjson, starts, ends, tzname, comment, user, now],
            )?;
            let sid = tx.last_insert_rowid();
            store::audit(&tx, now, &user, &format!("{k2}.create"), &format!("{k2}:{sid}"),
                &json!({"matchers": serde_json::from_str::<Value>(&mjson).unwrap(), "starts_at": starts, "ends_at": ends, "tz": tzname, "comment": comment}))?;
            tx.commit()?;
            Ok::<_, rusqlite::Error>(sid)
        })
        .await;
    match res {
        Ok(sid) => {
            app.reload_silences().await;
            app.hub.publish("silences", Some("/v2/api/v1/silences".into()), now);
            let list = windows(&app, Some(&kind), true).await;
            let row = list.into_iter().find(|v| v["id"] == json!(sid)).unwrap_or(Value::Null);
            json_resp(201, &json!({"data": row}))
        }
        Err(e) => err(500, &format!("保存失败：{e}")),
    }
}

async fn windows(app: &AppState, kind: Option<&str>, all: bool) -> Vec<Value> {
    let kind = kind.map(String::from);
    let now = app.clock.now();
    app.store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT id,kind,matchers,starts_at,ends_at,tz,comment,created_by,created_at,deleted_at,deleted_by FROM silences
                 WHERE (?1 IS NULL OR kind=?1) AND (?2 OR (deleted_at IS NULL AND ends_at > ?3)) ORDER BY id DESC LIMIT 500",
            )?;
            let v: Vec<Value> = st
                .query_map(rusqlite::params![kind, all, now], |r| {
                    let m: String = r.get(2)?;
                    let (s, e): (f64, f64) = (r.get(3)?, r.get(4)?);
                    let deleted: Option<f64> = r.get(9)?;
                    let status = if deleted.is_some() { "deleted" } else if now < s { "scheduled" } else if now < e { "active" } else { "expired" };
                    Ok(json!({"id": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?, "matchers": serde_json::from_str::<Value>(&m).unwrap_or(Value::Null),
                        "starts_at": s, "ends_at": e, "tz": r.get::<_, String>(5)?, "comment": r.get::<_, String>(6)?,
                        "created_by": r.get::<_, String>(7)?, "created_at": r.get::<_, f64>(8)?, "deleted_at": deleted,
                        "deleted_by": r.get::<_, Option<String>>(10)?, "status": status}))
                })?
                .filter_map(Result::ok)
                .collect();
            Ok::<_, rusqlite::Error>(v)
        })
        .await
        .unwrap_or_default()
}

async fn silences_list(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let all = q.get("all").map(|x| x == "1").unwrap_or(false);
    let kind = q.get("kind").cloned();
    data(json!({"silences": windows(&app, kind.as_deref(), all).await}))
}

async fn maint_list(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let all = q.get("all").map(|x| x == "1").unwrap_or(false);
    data(json!({"maintenance": windows(&app, Some("maintenance"), all).await}))
}

async fn silence_create(State(app): State<AppState>, Extension(id): Extension<Identity>, body: Bytes) -> Response {
    create_window(app, id, body, "silence").await
}

async fn maint_create(State(app): State<AppState>, Extension(id): Extension<Identity>, body: Bytes) -> Response {
    create_window(app, id, body, "maintenance").await
}

async fn silence_delete(State(app): State<AppState>, Extension(id): Extension<Identity>, Path(sid): Path<i64>) -> Response {
    let now = app.clock.now();
    let user = id.username.clone();
    let res = app
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            let kind: Option<String> = tx
                .query_row("SELECT kind FROM silences WHERE id=? AND deleted_at IS NULL", [sid], |r| r.get(0))
                .ok();
            let Some(kind) = kind else { return Ok(false) };
            tx.execute("UPDATE silences SET deleted_at=?, deleted_by=? WHERE id=?", rusqlite::params![now, user, sid])?;
            store::audit(&tx, now, &user, &format!("{kind}.delete"), &format!("{kind}:{sid}"), &json!({}))?;
            tx.commit()?;
            Ok::<_, rusqlite::Error>(true)
        })
        .await;
    match res {
        Ok(true) => {
            app.reload_silences().await;
            app.hub.publish("silences", Some("/v2/api/v1/silences".into()), now);
            data(json!({"id": sid, "deleted": true}))
        }
        Ok(false) => err(404, "静默或维护窗口不存在"),
        Err(e) => err(500, &format!("保存失败：{e}")),
    }
}

// ─── /v1：探测历史 ────────────────────────────────────────────────────────────────────────────────

async fn probes_latest(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let since = q.get("since").and_then(|s| s.parse::<f64>().ok());
    let g = app.probes_gen.load(std::sync::atomic::Ordering::Relaxed);
    if since.is_none() {
        // 同一探测代内共用一次渲染
        if let Some((cg, body, tag)) = app.probes_cache.lock().unwrap().clone() {
            if cg == g {
                if not_modified(&headers, &tag) {
                    return Response::builder().status(304).header(header::ETAG, tag).body(Body::empty()).unwrap();
                }
                return Response::builder().status(200).header(header::CONTENT_TYPE, "application/json; charset=utf-8").header(header::ETAG, tag).body(Body::from(body)).unwrap();
            }
        }
    }
    let ov = app.cached_data("/overview", 3600.0);
    match app.store.call(move |c| crate::probes::latest_since(c, ov.as_ref(), since)).await {
        Ok(v) => {
            let body = serde_json::to_vec(&json!({"data": v})).unwrap();
            let tag = format!("\"{}\"", hex::encode(&<sha2::Sha256 as sha2::Digest>::digest(&body)[..12]));
            let body = bytes::Bytes::from(body);
            if since.is_none() {
                *app.probes_cache.lock().unwrap() = Some((g, body.clone(), tag.clone()));
            }
            if not_modified(&headers, &tag) {
                return Response::builder().status(304).header(header::ETAG, tag).body(Body::empty()).unwrap();
            }
            Response::builder().status(200).header(header::CONTENT_TYPE, "application/json; charset=utf-8").header(header::ETAG, tag)
                .body(Body::from(body)).unwrap()
        }
        Err(e) => err(500, &format!("读取失败：{e}")),
    }
}

async fn probes_series(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(node) = q.get("node").cloned() else { return err(400, "缺少 node（短名如 t1-01，或资产 uuid）") };
    // uuid → 短名
    let node = if node.len() > 20 && node.contains('-') {
        app.cached_data("/overview", 3600.0)
            .and_then(|ov| {
                ov.get("assets")?.as_array()?.iter().find(|a| a.get("uuid").and_then(|u| u.as_str()) == Some(node.as_str()))
                    .and_then(|a| a.get("name").and_then(|n| n.as_str()).map(crate::rules::short_id))
            })
            .unwrap_or(node)
    } else {
        node
    };
    let range = q.get("range").map(String::as_str).unwrap_or("24h");
    let secs = match range {
        "1h" => 3600.0,
        "4h" => 4.0 * 3600.0,
        "6h" => 6.0 * 3600.0,
        "12h" => 12.0 * 3600.0,
        "24h" | "1d" => 86400.0,
        _ => return err(400, "range 取值：1h|4h|6h|12h|24h"),
    };
    let kinds: Vec<&'static str> = match q.get("kind").map(String::as_str) {
        None | Some("all") => crate::probes::KINDS.to_vec(),
        Some(k) => match crate::probes::KINDS.iter().find(|x| **x == k) {
            Some(x) => vec![*x],
            None => return err(400, "kind 取值：tcp|egress|path|all"),
        },
    };
    let raw = q.get("raw").map(|x| x == "1").unwrap_or(false);
    let since = app.clock.now() - secs;
    let n2 = node.clone();
    match app.store.call(move |c| crate::probes::series(c, &n2, &kinds, since, raw)).await {
        Ok(v) => data(json!({"node": node, "range": range, "since": since, "kinds": v})),
        Err(e) => err(500, &format!("读取失败：{e}")),
    }
}

// ─── 账户与会话（替代 Komari 登录） ─────────────────────────────────────────────────────────────────

fn session_cookie(app: &App, token: &str, max_age: i64) -> String {
    format!("{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}", crate::accounts::COOKIE, token, max_age,
        if app.cfg.auth.cookie_secure { "; Secure" } else { "" })
}

fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()).map(|s| s.trim().to_string())
        .unwrap_or_else(|| peer.ip().to_string())
}

async fn audit_async(app: &App, actor: &str, action: &str, target: &str, detail: Value) {
    let now = app.clock.now();
    let (a, b, t) = (actor.to_string(), action.to_string(), target.to_string());
    let _ = app.store.call(move |c| store::audit(c, now, &a, &b, &t, &detail)).await;
}

async fn auth_login(State(app): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, body: Bytes) -> Response {
    let b = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let user = b.get("username").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let pw = b.get("password").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let code = b.get("totp").and_then(|x| x.as_str()).map(String::from);
    if user.is_empty() || pw.is_empty() || user.len() > 64 || pw.len() > 256 {
        return err(400, "请输入用户名和密码");
    }
    let now = app.clock.now();
    let ip = client_ip(&headers, peer);
    let keys = vec![format!("ip:{ip}"), format!("user:{user}")];
    if let Some(wait) = app.login_limit.blocked(&keys, now) {
        return json_resp(429, &json!({"error": format!("尝试次数过多，请 {} 分钟后再试", (wait / 60.0).ceil() as i64)}));
    }
    let u2 = user.clone();
    let found = app.store.call(move |c| crate::accounts::user_by_name(c, &u2)).await;
    let ok = match &found {
        Some(u) => {
            let (scheme, hash, pw2) = (u.pw_scheme.clone(), u.pw_hash.clone(), pw.clone());
            tokio::task::spawn_blocking(move || crate::accounts::verify_password(&scheme, &hash, &pw2)).await.unwrap_or(false)
        }
        None => {
            // 用户不存在也做一次同等耗时的计算，避免按耗时猜用户名
            let pw2 = pw.clone();
            let _ = tokio::task::spawn_blocking(move || crate::accounts::hash_password(&pw2)).await;
            false
        }
    };
    let Some(u) = found.filter(|_| ok) else {
        app.login_limit.fail(&keys, now);
        audit_async(&app, &user, "auth.login_failed", &format!("user:{user}"), json!({"ip": ip})).await;
        return err(401, "用户名或密码错误");
    };
    if let Some(secret) = &u.totp_secret {
        match &code {
            None => return json_resp(401, &json!({"error": "需要两步验证码", "totp_required": true})),
            Some(c) if !crate::accounts::totp_check(secret, c, now) => {
                app.login_limit.fail(&keys, now);
                audit_async(&app, &user, "auth.login_failed", &format!("user:{user}"), json!({"ip": ip, "reason": "totp"})).await;
                return json_resp(401, &json!({"error": "两步验证码不正确", "totp_required": true}));
            }
            _ => {}
        }
    }
    app.login_limit.success(&keys);
    // Komari 导入的旧哈希：登录成功后升级为 PBKDF2
    if u.pw_scheme != "pbkdf2-sha256" {
        let pw2 = pw.clone();
        let h = tokio::task::spawn_blocking(move || crate::accounts::hash_password(&pw2)).await.unwrap();
        let id = u.id;
        let _ = app.store.call(move |c| c.execute("UPDATE users SET pw_scheme='pbkdf2-sha256', pw_hash=?, updated_at=? WHERE id=?",
            rusqlite::params![h, now, id])).await;
    }
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let (uid, ip2) = (u.id, ip.clone());
    let token = match app.store.call(move |c| crate::accounts::create_session(c, uid, &ip2, &ua, now)).await {
        Ok(t) => t,
        Err(e) => return err(500, &format!("创建会话失败：{e}")),
    };
    audit_async(&app, &u.username, "auth.login", &format!("user:{}", u.username), json!({"ip": ip, "totp": u.totp_secret.is_some()})).await;
    let mut r = json_resp(200, &json!({"data": {"username": u.username, "expires_at": now + crate::accounts::SESSION_ABS_S,
        "idle_timeout_s": crate::accounts::SESSION_IDLE_S, "totp_enabled": u.totp_secret.is_some()}}));
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&session_cookie(&app, &token, crate::accounts::SESSION_ABS_S as i64)).unwrap());
    r
}

async fn auth_logout(State(app): State<AppState>, Extension(id): Extension<Identity>) -> Response {
    if let Some(tok) = &id.fleet_token {
        let (t, now) = (tok.clone(), app.clock.now());
        let _ = app.store.call(move |c| crate::accounts::revoke_session_token(c, &t, now)).await;
        app.forget_session(tok);
        audit_async(&app, &id.username, "auth.logout", &format!("user:{}", id.username), json!({})).await;
    }
    let mut r = data(json!({"logged_out": true}));
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&session_cookie(&app, "", 0)).unwrap());
    r
}

async fn auth_me(State(app): State<AppState>, Extension(id): Extension<Identity>) -> Response {
    let name = id.username.clone();
    let totp = app.store.call(move |c| crate::accounts::user_by_name(c, &name).map(|u| u.totp_secret.is_some())).await;
    data(json!({"username": id.username, "via": id.via, "fleetd_account": totp.is_some(), "totp_enabled": totp.unwrap_or(false),
        "komari_fallback": app.cfg.auth.komari_fallback && app.cfg.komari.enabled}))
}

fn need_fleet_account(id: &Identity) -> Option<Response> {
    if id.via != "fleetd" {
        return Some(err(403, "请用 fleetd 账户登录后再修改账户设置"));
    }
    None
}

async fn auth_password(State(app): State<AppState>, Extension(id): Extension<Identity>, body: Bytes) -> Response {
    if let Some(r) = need_fleet_account(&id) {
        return r;
    }
    let b = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let old = b.get("old_password").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let new = b.get("new_password").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if new.chars().count() < 10 || new.len() > 256 {
        return err(400, "新密码至少 10 个字符");
    }
    let name = id.username.clone();
    let Some(u) = app.store.call(move |c| crate::accounts::user_by_name(c, &name)).await else { return err(404, "账户不存在") };
    let (s, h, o) = (u.pw_scheme.clone(), u.pw_hash.clone(), old.clone());
    if !tokio::task::spawn_blocking(move || crate::accounts::verify_password(&s, &h, &o)).await.unwrap_or(false) {
        return err(403, "当前密码不正确");
    }
    let hash = tokio::task::spawn_blocking(move || crate::accounts::hash_password(&new)).await.unwrap();
    let now = app.clock.now();
    let (uid, keep) = (u.id, id.fleet_token.clone().map(|t| crate::accounts::sha256_hex(&t)).unwrap_or_default());
    let _ = app.store.call(move |c| {
        c.execute("UPDATE users SET pw_scheme='pbkdf2-sha256', pw_hash=?, updated_at=? WHERE id=?", rusqlite::params![hash, now, uid])?;
        // 其他会话一律作废
        c.execute("UPDATE sessions SET revoked_at=? WHERE user_id=? AND revoked_at IS NULL AND token_sha256<>?", rusqlite::params![now, uid, keep])
    }).await;
    app.forget_all_sessions();
    audit_async(&app, &id.username, "auth.password_changed", &format!("user:{}", id.username), json!({})).await;
    data(json!({"changed": true}))
}

async fn auth_totp_setup(State(app): State<AppState>, Extension(id): Extension<Identity>) -> Response {
    if let Some(r) = need_fleet_account(&id) {
        return r;
    }
    let secret = crate::accounts::base32_encode(&crate::accounts::random_bytes(20));
    let (name, s2, now) = (id.username.clone(), secret.clone(), app.clock.now());
    let _ = app.store.call(move |c| c.execute("UPDATE users SET totp_pending=?, updated_at=? WHERE username=?", rusqlite::params![s2, now, name])).await;
    let uri = format!("otpauth://totp/{}:{}?secret={}&issuer={}&digits=6&period=30",
        form_urlencoded::byte_serialize("团队线路管理".as_bytes()).collect::<String>(), id.username, secret,
        form_urlencoded::byte_serialize("团队线路管理".as_bytes()).collect::<String>());
    data(json!({"secret": secret, "otpauth_uri": uri, "note": "用验证器扫码或手动输入密钥，再调 /auth/totp/enable 提交 6 位码启用"}))
}

async fn auth_totp_enable(State(app): State<AppState>, Extension(id): Extension<Identity>, body: Bytes) -> Response {
    if let Some(r) = need_fleet_account(&id) {
        return r;
    }
    let code = parse_body(&body).ok().and_then(|b| b.get("code").and_then(|x| x.as_str()).map(String::from)).unwrap_or_default();
    let name = id.username.clone();
    let pending: Option<String> = app.store.call(move |c| c.query_row("SELECT totp_pending FROM users WHERE username=?", [name], |r| r.get(0)).ok().flatten()).await;
    let Some(p) = pending else { return err(409, "请先调用 /auth/totp/setup") };
    let now = app.clock.now();
    if !crate::accounts::totp_check(&p, &code, now) {
        return err(400, "验证码不正确");
    }
    let name = id.username.clone();
    let _ = app.store.call(move |c| c.execute("UPDATE users SET totp_secret=totp_pending, totp_pending=NULL, updated_at=? WHERE username=?",
        rusqlite::params![now, name])).await;
    audit_async(&app, &id.username, "auth.totp_enabled", &format!("user:{}", id.username), json!({})).await;
    data(json!({"totp_enabled": true}))
}

async fn auth_totp_disable(State(app): State<AppState>, Extension(id): Extension<Identity>, body: Bytes) -> Response {
    if let Some(r) = need_fleet_account(&id) {
        return r;
    }
    let code = parse_body(&body).ok().and_then(|b| b.get("code").and_then(|x| x.as_str()).map(String::from)).unwrap_or_default();
    let name = id.username.clone();
    let Some(u) = app.store.call(move |c| crate::accounts::user_by_name(c, &name)).await else { return err(404, "账户不存在") };
    let Some(secret) = u.totp_secret else { return err(409, "没有启用两步验证") };
    let now = app.clock.now();
    if !crate::accounts::totp_check(&secret, &code, now) {
        return err(400, "验证码不正确");
    }
    let uid = u.id;
    let _ = app.store.call(move |c| c.execute("UPDATE users SET totp_secret=NULL, totp_pending=NULL, updated_at=? WHERE id=?", rusqlite::params![now, uid])).await;
    audit_async(&app, &id.username, "auth.totp_disabled", &format!("user:{}", id.username), json!({})).await;
    data(json!({"totp_enabled": false}))
}

async fn auth_sessions(State(app): State<AppState>, Extension(id): Extension<Identity>) -> Response {
    let (name, now) = (id.username.clone(), app.clock.now());
    let cur = id.fleet_token.as_ref().map(|t| crate::accounts::sha256_hex(t));
    let mut list = app.store.call(move |c| crate::accounts::list_sessions(c, &name, now)).await;
    if let Some(h) = cur {
        let cur_id: Option<i64> = {
            let h2 = h.clone();
            app.store.call(move |c| c.query_row("SELECT id FROM sessions WHERE token_sha256=?", [h2], |r| r.get(0)).ok()).await
        };
        for s in list.iter_mut() {
            s["current"] = json!(Some(s["id"].as_i64().unwrap_or(-1)) == cur_id);
        }
    }
    data(json!({"sessions": list}))
}

async fn auth_session_revoke(State(app): State<AppState>, Extension(id): Extension<Identity>, Path(sid): Path<i64>) -> Response {
    let (name, now) = (id.username.clone(), app.clock.now());
    let n = app.store.call(move |c| c.execute("UPDATE sessions SET revoked_at=? WHERE id=? AND revoked_at IS NULL
        AND user_id=(SELECT id FROM users WHERE username=?)", rusqlite::params![now, sid, name]).unwrap_or(0)).await;
    if n == 0 {
        return err(404, "会话不存在");
    }
    app.forget_all_sessions();
    audit_async(&app, &id.username, "auth.session_revoked", &format!("session:{sid}"), json!({})).await;
    data(json!({"revoked": sid}))
}

async fn onboard_proxy(State(app): State<AppState>, Extension(id): Extension<Identity>, method: Method, RawQuery(q): RawQuery,
    uri: axum::http::Uri, body: Bytes) -> Response {
    let Some(ob) = &app.onboard else { return err(404, "上架流水线未启用（fleetd.toml 未配置 onboard_config）") };
    let path = uri.path().to_string();
    let (code, v) = ob.handle(method.as_str(), &path, q.as_deref().unwrap_or(""), &body, &id.username).await;
    if method != Method::GET && (200..300).contains(&code) {
        let now = app.clock.now();
        let action = path.rsplit('/').next().unwrap_or("").to_string();
        let user = id.username.clone();
        let detail = json!({"path": path, "status": code});
        let _ = app.store.call(move |c| store::audit(c, now, &user, &format!("onboard.{action}"), "onboard", &detail)).await;
        app.hub.publish("onboard", Some("/v2/api/v1/onboard/batches".into()), now);
    }
    json_resp(code, &v)
}

async fn health_shadow(State(app): State<AppState>) -> Response {
    let native = {
        let n = app.native.lock().unwrap();
        json!({"mode": app.cfg.edge.native_health, "native_after_s": app.cfg.edge.native_after_s, "rows_loaded": n.rows_loaded,
            "last_eval_ms": (n.last_eval_ms * 100.0).round() / 100.0, "relays": n.coverage.values().cloned().collect::<Vec<_>>()})
    };
    let sh = app.health_shadow.lock().unwrap();
    data(json!({"mode": "shadow：旧 /overview → fleet-health 移植版 monitor，对照旧 /health；告警仍来自旧 /health",
        "evaluations": sh.evaluations, "compared": sh.compared, "agree": sh.agree,
        "agree_pct": if sh.compared > 0 { json!((sh.agree as f64 * 10000.0 / sh.compared as f64).round() / 100.0) } else { Value::Null },
        "last_at": sh.last_at, "last_ms": (sh.last_ms * 100.0).round() / 100.0,
        "last_disagree": sh.last_disagree, "disagree_by_reason": sh.disagree_by_reason, "native": native}))
}

async fn ipintel_backfill_status(State(app): State<AppState>) -> Response {
    let st = app.ipintel_state.lock().unwrap().clone();
    data(json!({"enabled": app.ipintel.is_some(), "state": st}))
}

// ─── /v1：通知、渠道、系统、审计 ─────────────────────────────────────────────────────────────────

async fn notify_log(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let limit: i64 = q.get("limit").and_then(|x| x.parse().ok()).unwrap_or(100).clamp(1, 1000);
    let status = q.get("status").cloned();
    let channel = q.get("channel").cloned();
    let rows = app
        .store
        .call(move |c| store::notify_list(c, limit, status.as_deref(), channel.as_deref()))
        .await
        .unwrap_or_default();
    data(json!({"notifications": rows, "shadow": app.notifier.is_shadow()}))
}

async fn channels(State(app): State<AppState>) -> Response {
    let list = app.notifier.channels_json();
    let note = if app.notifier.channels.is_empty() {
        "未配置任何通知渠道：影子模式，所有通知只写入通知记录（status = shadow）"
    } else if app.notifier.is_shadow() {
        "已配置渠道，但 notify.shadow = true：仍只写通知记录"
    } else {
        "通知正常投递"
    };
    data(json!({"shadow": app.notifier.is_shadow(), "channels": list, "note": note,
        "healthchecks": app.cfg.notify.healthchecks_url.is_some()}))
}

async fn audit_list(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let limit: i64 = q.get("limit").and_then(|x| x.parse().ok()).unwrap_or(100).clamp(1, 1000);
    let (rows, broken) = app
        .store
        .call(move |c| Ok::<_, rusqlite::Error>((store::audit_list(c, limit)?, store::audit_verify(c)?)))
        .await
        .unwrap_or_default();
    data(json!({"audit": rows, "chain_ok": broken.is_none(), "chain_broken_at": broken}))
}

async fn system(State(app): State<AppState>) -> Response {
    let now = app.clock.now();
    let active = app.active();
    let (cpu, avg, rss, threads, total_cpu, anon, file) = {
        let s = app.sys.lock().unwrap();
        (s.cpu_pct, s.avg(), s.rss_kb, s.threads, s.total_cpu_s, s.rss_anon_kb, s.rss_file_kb)
    };
    let polls: Vec<Value> = app
        .polls
        .iter()
        .map(|p| {
            let st = p.st.lock().unwrap().clone();
            let iv = if active { Some(p.active_s) } else { p.idle_s };
            json!({"resource": p.name, "path": p.key, "source": format!("{:?}", p.source).to_lowercase(), "interval_s": iv,
                "active_s": p.active_s, "idle_s": p.idle_s, "version": st.version, "polls": st.polls,
                "age_s": if st.last_ok > 0.0 { json!(((now - st.last_ok) * 10.0).round() / 10.0) } else { Value::Null },
                "last_latency_ms": (st.last_latency_ms * 10.0).round() / 10.0, "bytes": st.last_bytes, "errors": st.errors,
                "consecutive_errors": st.consecutive_errors, "last_error": st.last_error})
        })
        .collect();
    let (legacy_stats, legacy_p) = {
        let s = app.legacy.stats.lock().unwrap();
        (serde_json::to_value(&*s).unwrap(), s.percentiles())
    };
    let (komari_stats, komari_p) = {
        let s = app.komari.stats.lock().unwrap();
        (serde_json::to_value(&*s).unwrap(), s.percentiles())
    };
    let (in_mem, pending, firing, suppressed, next_digest, last_digest) = {
        let e = app.engine.lock().unwrap();
        let act: Vec<_> = e.active().collect();
        (
            e.all_in_memory().count(),
            act.iter().filter(|a| a.state == crate::alerts::State::Pending).count(),
            act.iter().filter(|a| a.state == crate::alerts::State::Firing).count(),
            act.iter().filter(|a| a.suppressed.is_some()).count(),
            e.next_digest_at,
            e.last_digest_at,
        )
    };
    let eval = app.eval.lock().unwrap().clone();
    let kh = app.komari_health.lock().unwrap().clone();
    data(json!({
        "requests": req_stats_json(&app),
        "render_cache": render_cache_json(&app),
        "cpu_by": crate::cpu::snapshot(),
        "cpu_ingest": cpu_ingest_json(&app),
        "service": "fleetd", "version": crate::app::VERSION, "pid": std::process::id(), "legacy_source": app.cfg.legacy_source,
        "features": app.features(),
        "started_at": app.started_at, "uptime_s": (now - app.started_at).round(), "now": now,
        "cpu_pct": (cpu * 100.0).round() / 100.0, "cpu_avg_5m_pct": avg.map(|a| (a * 100.0).round() / 100.0), "cpu_total_s": total_cpu,
        "rss_mb": (rss as f64 / 1024.0 * 10.0).round() / 10.0, "rss_anon_mb": (anon as f64 / 1024.0 * 10.0).round() / 10.0,
        "rss_file_mb": (file as f64 / 1024.0 * 10.0).round() / 10.0, "threads": threads,
        "cadence": if active { "active" } else { "idle" }, "sse_clients": app.hub.clients(), "sse_versions": app.hub.versions(),
        "legacy": {"base_url": app.cfg.legacy.base_url, "key": app.cfg.legacy.key_file.is_some(), "stats": legacy_stats, "latency": legacy_p,
            "lanes": {"poll": 1, "light": crate::legacy::LIGHT_SLOTS, "heavy": crate::legacy::HEAVY_SLOTS, "demand_deadline_s": crate::legacy::DEMAND_DEADLINE_S}},
        "komari": {"enabled": app.cfg.komari.enabled, "base_url": app.cfg.komari.base_url, "api_key": app.komari.has_api_key(), "stats": komari_stats, "latency": komari_p,
            "reachable": kh.consecutive_failures < 3, "health": kh},
        "polls": polls,
        "proxy": {"requests": app.proxy_requests.load(std::sync::atomic::Ordering::Relaxed), "cache_hits": app.proxy_hits.load(std::sync::atomic::Ordering::Relaxed),
            "legacy_cache_entries": app.legacy.cache.len(), "legacy_cache_bytes": app.legacy.cache.bytes(), "komari_cache_entries": app.komari.cache.len()},
        "auth": {"mode": format!("{:?}", app.auth.mode), "fleetd_accounts": true, "komari_fallback": app.auth.mode == crate::config::AuthMode::Komari && app.cfg.auth.komari_fallback && app.cfg.komari.enabled,
            "login": "/v2/api/v1/auth/login", "cached_sessions": app.auth.cached_sessions(),
            "upstream_checks": app.auth.checks_upstream.load(std::sync::atomic::Ordering::Relaxed)},
        "alerts": {"in_memory": in_mem, "pending": pending, "firing": firing, "suppressed": suppressed,
            "last_digest_at": last_digest, "next_digest_at": next_digest, "evaluation": eval},
        "notify": {"shadow": app.notifier.is_shadow(), "channels": app.notifier.channels.len()},
        "db": {"path": app.store.path.display().to_string(), "size_bytes": app.store.size_bytes()},
        "edge": {"hosts": app.ingest.hosts(now).as_array().map(|a| a.len()).unwrap_or(0),
            "native_relays": app.native.lock().unwrap().coverage.values().filter(|c| c.source == "native").map(|c| c.relay.clone()).collect::<Vec<_>>(),
            "managed_relays": app.managed_relays(),
            "onboard": app.onboard.is_some()},
        "ipintel": {"enabled": app.ipintel.is_some(), "state": app.ipintel_state.lock().unwrap().clone(),
            "loaded": app.ipintel.as_ref().map(|ii| ii.status().get("loaded").cloned().unwrap_or(Value::Null))},
    }))
}

pub fn into_service(app: AppState) -> axum::extract::connect_info::IntoMakeServiceWithConnectInfo<Router, SocketAddr> {
    // 边缘上报在 guard 之外（agent 没有管理员会话，用自己的令牌）
    fleetd_ingest::api::ingest_routes::<()>(app.ingest.clone())
        .merge(relay_agent_routes(app.clone()))
        .merge(crate::legacyhttp::routes(app.clone()))
        .merge(router(app))
        .into_make_service_with_connect_info::<SocketAddr>()
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>(_: T) {}
    let _ = |r: Response| is_send(r.into_response());
}

// ─── relayd 管理模式（fleet.relayspec.v1，API.md §10）：边缘侧（agent 令牌）与管理侧（会话） ─────────

pub(crate) fn relay_host_pub(app: &App, id: &str) -> Option<(String, String)> {
    relay_host(app, id)
}

fn relay_host(app: &App, id: &str) -> Option<(String, String)> {
    // 接受 host_id（line3）或线路机名（线路机3）；返回 (host_id, 线路机名)
    let hosts = &app.cfg.edge.relay_hosts;
    if let Some(n) = hosts.get(id) {
        return Some((id.to_string(), n.clone()));
    }
    hosts.iter().find(|(_, n)| n.as_str() == id).map(|(h, n)| (h.clone(), n.clone()))
}

fn agent_relay(app: &App, headers: &HeaderMap) -> Result<String, Response> {
    let tok = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::trim);
    let Some(tok) = tok.filter(|t| !t.is_empty()) else { return Err(err(401, "缺少 agent 令牌")) };
    match app.ingest.auth(tok) {
        Some(h) if app.cfg.edge.relay_hosts.contains_key(&h) => Ok(h),
        Some(_) => Err(err(403, "这个令牌不属于线路机")),
        None => Err(err(401, "agent 令牌无效")),
    }
}

async fn relay_observed(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let host = match agent_relay(&app, &headers) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let doc = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if doc["schema"] != "fleet.relayobs.v1" || doc["config_sha256"].as_str().is_none() || !doc["managed"].is_object() {
        return err(400, "需要 schema=fleet.relayobs.v1、config_sha256、managed");
    }
    let now = app.clock.now();
    let h2 = host.clone();
    match app.store.call(move |c| crate::relayspec::record_observed(c, &h2, &doc, now)).await {
        Ok((g, adopted)) => {
            if adopted {
                tracing::info!(relay = %host, generation = g, "relay config changed outside fleetd; adopted as new generation");
                app.hub.publish("relays", Some(format!("/v2/api/v1/relays/{host}/spec")), now);
            }
            json_resp(200, &json!({"ok": true, "generation": g, "adopted": adopted}))
        }
        Err(e) => err(500, &format!("保存失败：{e}")),
    }
}

async fn relay_spec_get(State(app): State<AppState>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let host = match agent_relay(&app, &headers) {
        Ok(h) => h,
        Err(r) => return r,
    };
    app.relay_polls.lock().unwrap().insert(host.clone(), app.clock.now());
    let have: i64 = q.get("have").and_then(|x| x.parse().ok()).unwrap_or(0);
    let wait: u64 = q.get("wait").and_then(|x| x.parse().ok()).unwrap_or(0).min(25);
    let deadline = std::time::Instant::now() + Duration::from_secs(wait);
    loop {
        let (h2, now) = (host.clone(), app.clock.now());
        if let Some(doc) = app.store.call(move |c| crate::relayspec::spec_doc(c, &h2, have, now)).await {
            return json_resp(200, &doc);
        }
        if std::time::Instant::now() >= deadline {
            return Response::builder().status(304).body(Body::empty()).unwrap();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn relay_spec_ack(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let host = match agent_relay(&app, &headers) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let doc = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if doc["generation"].as_i64().is_none() || doc["status"].as_str().is_none() {
        return err(400, "需要 generation、status");
    }
    let now = app.clock.now();
    let (h2, d2) = (host.clone(), doc.clone());
    match app.store.call(move |c| crate::relayspec::record_ack(c, &h2, &d2, now)).await {
        Ok(ops) => {
            let st = doc["status"].as_str().unwrap_or("");
            tracing::info!(relay = %host, generation = doc["generation"].as_i64().unwrap_or(-1), status = st, ops = ops.len(), "relay spec ack");
            let (h3, d3, ops2) = (host.clone(), doc.clone(), ops.clone());
            let _ = app.store.call(move |c| store::audit(c, now, &format!("relayd:{h3}"), "relay.spec_ack", &format!("relay:{h3}"),
                &json!({"generation": d3["generation"], "status": d3["status"], "error": d3["error"], "ops": ops2}))).await;
            for op in ops {
                app.hub.publish("ops", Some(format!("/v2/api/v1/ops/{op}")), now);
            }
            json_resp(200, &json!({"ok": true}))
        }
        Err(e) => err(500, &format!("保存失败：{e}")),
    }
}

pub fn relay_agent_routes(app: AppState) -> Router {
    Router::new()
        .route("/v2/ingest/relay/observed", post(relay_observed))
        .route("/v2/ingest/relay/spec", get(relay_spec_get))
        .route("/v2/ingest/relay/spec/ack", post(relay_spec_ack))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
        .with_state(app)
}

async fn ops_list(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let relay = q.get("relay").and_then(|r| relay_host(&app, r)).map(|x| x.0);
    let status = q.get("status").cloned();
    let limit: i64 = q.get("limit").and_then(|x| x.parse().ok()).unwrap_or(100).clamp(1, 1000);
    let rows = app.store.call(move |c| {
        let mut st = c.prepare("SELECT id, kind, relay, generation, target, detail, status, created_by, created_at, acked_at, result, error, expires_at
            FROM ops WHERE (?1 IS NULL OR relay=?1) AND (?2 IS NULL OR status=?2) ORDER BY created_at DESC LIMIT ?3").unwrap();
        st.query_map(rusqlite::params![relay, status, limit], crate::relayspec::op_row).unwrap().filter_map(Result::ok).collect::<Vec<_>>()
    }).await;
    data(json!({"ops": rows}))
}

async fn op_get(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    match app.store.call(move |c| crate::relayspec::op_json(c, &id)).await {
        Some(v) => data(v),
        None => err(404, "操作不存在"),
    }
}

async fn relay_spec_view(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    let Some((host, name)) = relay_host(&app, &id) else { return err(404, "没有这台线路机") };
    let h2 = host.clone();
    let v = app.store.call(move |c| {
        let spec = crate::relayspec::current_spec(c, &h2);
        let obs = crate::relayspec::observed(c, &h2);
        let ack = crate::relayspec::last_applied(c, &h2);
        json!({"generation": spec.as_ref().map(|s| s.generation).unwrap_or(0), "spec_sha256": spec.as_ref().map(|s| s.spec_sha256.clone()),
            "managed_mode": spec.is_some(), "observed": obs.as_ref().map(|o| json!({"config_sha256": o.0, "generation": o.1, "observed_at": o.3,
                "inbounds": o.2["inbounds"].as_array().map(|a| a.len()), "outbounds": o.2["outbounds"].as_array().map(|a| a.len())})),
            "last_applied": ack.map(|(g, s)| json!({"generation": g, "config_sha256_after": s}))})
    }).await;
    let mut v = v;
    v["management_live"] = json!(app.managed_relays().contains(&host));
    v["relay"] = json!(name);
    v["host_id"] = json!(host);
    data(v)
}

/// 入口操作：enable / disable / reset_traffic / rename / rotate_uuid（API.md §9.1）
pub(crate) async fn entry_action(State(app): State<AppState>, Extension(id): Extension<Identity>, Path((relay, port)): Path<(String, i64)>, body: Bytes) -> Response {
    let Some((host, name)) = relay_host(&app, &relay) else { return err(404, "没有这台线路机") };
    let b = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let action = b["action"].as_str().unwrap_or("").to_string();
    let now = app.clock.now();
    let user = id.username.clone();
    if action != "rename" && !app.managed_relays().contains(&host) {
        return err(409, &format!("{name} 的 relayd 还没开启管理模式（最近 60 秒没有来取期望状态），现在不能下发"));
    }
    match action.as_str() {
        "rename" => {
            let remark: String = b["remark"].as_str().unwrap_or("").trim().chars().take(64).collect();
            let (h2, u2) = (host.clone(), user.clone());
            let r = app.store.call(move |c| {
                c.execute("INSERT INTO entry_meta(relay, port, remark, updated_at) VALUES(?1,?2,?3,?4)
                    ON CONFLICT(relay, port) DO UPDATE SET remark=excluded.remark, updated_at=excluded.updated_at", rusqlite::params![h2, port, remark, now])?;
                store::audit(c, now, &u2, "entry.rename", &format!("{h2}:{port}"), &json!({"remark": remark}))
            }).await;
            return match r {
                Ok(_) => data(json!({"relay": name, "port": port, "remark": b["remark"]})),
                Err(e) => err(500, &e.to_string()),
            };
        }
        "rotate_uuid" => {
            let want = format!("{name}:{port}");
            if b["confirm"].as_str() != Some(want.as_str()) {
                return err(400, &format!("轮换 UUID 会让这个入口的客户端订阅失效；确认请传 confirm = \"{want}\""));
            }
        }
        "enable" | "disable" | "reset_traffic" => {}
        _ => return err(400, "action 取值：enable | disable | reset_traffic | rename | rotate_uuid"),
    }
    let (h2, u2, act) = (host.clone(), user.clone(), action.clone());
    let r = app.store.call(move |c| -> Result<Value, String> {
        let tx = c.transaction().map_err(|e| e.to_string())?;
        let e2 = |e: rusqlite::Error| e.to_string();
        if act == "reset_traffic" {
            let tag = format!("inbound-{port}");
            let op = crate::relayspec::create_op(&tx, "reset_traffic", &h2, None, &port.to_string(), &json!({"tag": tag}), &u2, now).map_err(e2)?;
            crate::relayspec::create_command(&tx, &h2, "reset_traffic", &json!({"tag": tag, "users": true}), &op, now).map_err(e2)?;
            store::audit(&tx, now, &u2, "entry.reset_traffic", &format!("{h2}:{port}"), &json!({"op": op})).map_err(e2)?;
            tx.commit().map_err(e2)?;
            return Ok(json!({"op_id": op, "generation": Value::Null}));
        }
        let (mut m, base) = crate::relayspec::base_for_write(&tx, &h2)?;
        let idx = crate::relayspec::find_inbound(&m, port);
        let mut detail = json!({"port": port});
        match act.as_str() {
            "disable" => {
                let i = idx.ok_or("这个端口没有入站（可能已停用）")?;
                let ib = m["inbounds"].as_array_mut().unwrap().remove(i);
                // 记下原位置：启用时插回原处，配置文件回到停用前的样子（不只是内容相同）
                let saved = json!({"fleet_saved_inbound": 1, "index": i, "inbound": ib});
                tx.execute("INSERT INTO entry_meta(relay, port, disabled_inbound, updated_at) VALUES(?1,?2,?3,?4)
                    ON CONFLICT(relay, port) DO UPDATE SET disabled_inbound=excluded.disabled_inbound, updated_at=excluded.updated_at",
                    rusqlite::params![h2, port, saved.to_string(), now]).map_err(e2)?;
            }
            "enable" => {
                if idx.is_some() {
                    return Err("入口已是启用状态".into());
                }
                let saved: Option<String> = tx.query_row("SELECT disabled_inbound FROM entry_meta WHERE relay=? AND port=?", rusqlite::params![h2, port], |r| r.get(0))
                    .optional().map_err(e2)?.flatten();
                let sv: Value = saved.and_then(|s| serde_json::from_str(&s).ok()).ok_or("没有保存的入站配置（不是经 fleetd 停用的）")?;
                let (ib, at) = if sv.get("fleet_saved_inbound").is_some() {
                    (sv["inbound"].clone(), sv["index"].as_u64().map(|x| x as usize))
                } else {
                    (sv, None)
                };
                let arr = m["inbounds"].as_array_mut().ok_or("受管集合无效")?;
                let at = at.unwrap_or(arr.len()).min(arr.len());
                arr.insert(at, ib);
            }
            "rotate_uuid" => {
                let i = idx.ok_or("这个端口没有入站")?;
                let rot = crate::relayspec::rotate_clients(&mut m["inbounds"][i]);
                if rot.is_empty() {
                    return Err("这个入站没有可轮换的客户端".into());
                }
                detail["rotated"] = json!(rot.iter().map(|(o, n)| json!({"old_prefix": o, "new_prefix": n})).collect::<Vec<_>>());
            }
            _ => unreachable!(),
        }
        let g = crate::relayspec::insert_spec(&tx, &h2, &m, &base, &act, &u2, now).map_err(e2)?;
        let op = crate::relayspec::create_op(&tx, &act, &h2, Some(g), &port.to_string(), &detail, &u2, now).map_err(e2)?;
        store::audit(&tx, now, &u2, &format!("entry.{act}"), &format!("{h2}:{port}"), &json!({"op": op, "generation": g, "detail": detail})).map_err(e2)?;
        if act == "enable" {
            tx.execute("UPDATE entry_meta SET disabled_inbound=NULL WHERE relay=? AND port=?", rusqlite::params![h2, port]).map_err(e2)?;
        }
        tx.commit().map_err(e2)?;
        Ok(json!({"op_id": op, "generation": g}))
    }).await;
    match r {
        Ok(mut v) => {
            app.hub.publish("ops", Some(format!("/v2/api/v1/ops/{}", v["op_id"].as_str().unwrap_or(""))), now);
            v["relay"] = json!(name);
            v["status"] = json!("pending");
            json_resp(202, &json!({"data": v}))
        }
        Err(e) => err(409, &e),
    }
}

/// 只调整受管入站在配置里的先后顺序（内容一字不改）：`{"action":"reorder_inbounds","order":[端口…],"confirm":"<线路机名>"}`，
/// order 必须恰好是现有受管入站端口的一个排列。用于把早期“启用时追加到末尾”造成的顺序变化恢复原样。
async fn reorder_inbounds(app: AppState, id: Identity, host: String, name: String, b: Value) -> Response {
    if b["confirm"].as_str() != Some(name.as_str()) {
        return err(400, &format!("调整入站顺序会让 relayd 重写这台线路机的配置；确认请传 confirm = \"{name}\""));
    }
    if !app.managed_relays().contains(&host) {
        return err(409, &format!("{name} 的 relayd 还没开启管理模式（最近 60 秒没有来取期望状态），现在不能下发"));
    }
    let Some(order) = b["order"].as_array().map(|a| a.iter().filter_map(|x| x.as_i64()).collect::<Vec<i64>>()) else {
        return err(400, "order 必须是端口数组");
    };
    let (now, user, h2) = (app.clock.now(), id.username.clone(), host.clone());
    let r = app.store.call(move |c| -> Result<(String, i64), String> {
        let tx = c.transaction().map_err(|e| e.to_string())?;
        let e2 = |e: rusqlite::Error| e.to_string();
        let (mut m, base) = crate::relayspec::base_for_write(&tx, &h2)?;
        let ibs = m["inbounds"].as_array().cloned().ok_or("受管集合无效")?;
        let port_of = |v: &Value| v["port"].as_i64();
        let mut cur: Vec<i64> = ibs.iter().filter_map(port_of).collect();
        let mut want = order.clone();
        cur.sort();
        want.sort();
        if cur != want || ibs.len() != order.len() {
            return Err(format!("order 必须恰好是现有受管入站端口的排列（现有 {} 个）", ibs.len()));
        }
        let before: Vec<i64> = ibs.iter().filter_map(port_of).collect();
        // 以线路机实际观测到的顺序为准（期望状态可能已是目标顺序，但 relayd 旧版本当作无变化没有落盘）
        let seen: Vec<i64> = crate::relayspec::observed(&tx, &h2)
            .and_then(|o| o.2["inbounds"].as_array().map(|a| a.iter().filter_map(port_of).collect()))
            .unwrap_or_else(|| before.clone());
        if before == order && seen == order {
            return Err("顺序没有变化".into());
        }
        let before = seen;
        let re: Vec<Value> = order.iter().map(|p| ibs.iter().find(|v| port_of(v) == Some(*p)).cloned().unwrap()).collect();
        m["inbounds"] = Value::Array(re);
        let detail = json!({"before": before, "after": order});
        let g = crate::relayspec::insert_spec(&tx, &h2, &m, &base, "reorder_inbounds", &user, now).map_err(e2)?;
        let op = crate::relayspec::create_op(&tx, "reorder_inbounds", &h2, Some(g), "", &detail, &user, now).map_err(e2)?;
        store::audit(&tx, now, &user, "relay.reorder_inbounds", &format!("relay:{h2}"), &json!({"op": op, "generation": g, "detail": detail})).map_err(e2)?;
        tx.commit().map_err(e2)?;
        Ok((op, g))
    }).await;
    match r {
        Ok((op, g)) => {
            app.hub.publish("ops", Some(format!("/v2/api/v1/ops/{op}")), now);
            json_resp(202, &json!({"data": {"op_id": op, "relay": name, "generation": g, "status": "pending"}}))
        }
        Err(e) => err(409, &e),
    }
}

/// Xray 日志（只读）：`{"action":"xray_logs","which":"error"|"access","lines":100}`。下发一条 generation-0 命令，
/// relayd 下一次取期望状态时读日志尾部（有上限）并随回执带回；这里最多等 25 s。
async fn xray_logs(app: AppState, id: Identity, host: String, name: String, b: Value) -> Response {
    if !app.managed_relays().contains(&host) {
        return err(409, &format!("{name} 的 relayd 还没开启管理模式（最近 60 秒没有来取期望状态），现在不能读取日志"));
    }
    let which = match b["which"].as_str().unwrap_or("error") {
        w @ ("error" | "access") => w.to_string(),
        _ => return err(400, "which 取值：error | access"),
    };
    let lines = b["lines"].as_u64().unwrap_or(100).clamp(1, 500);
    let (now, user, h2) = (app.clock.now(), id.username.clone(), host.clone());
    let args = json!({"which": which.clone(), "lines": lines});
    let r = app.store.call(move |c| -> rusqlite::Result<String> {
        let tx = c.transaction()?;
        let op = crate::relayspec::create_op(&tx, "xray_logs", &h2, None, &which, &args, &user, now)?;
        crate::relayspec::create_command(&tx, &h2, "xray_logs", &args, &op, now)?;
        tx.commit()?;
        Ok(op)
    }).await;
    let op = match r {
        Ok(op) => op,
        Err(e) => return err(500, &e.to_string()),
    };
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let o2 = op.clone();
        let row: Option<(String, Option<String>, Option<String>)> = app.store.call(move |c| {
            c.query_row("SELECT status, result, error FROM ops WHERE id=?", [&o2], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).ok()
        }).await;
        if let Some((status, result, error)) = row {
            if status != "pending" {
                let res: Value = result.and_then(|x| serde_json::from_str(&x).ok()).unwrap_or(Value::Null);
                let cmd = &res["command"];
                return data(json!({"op_id": op, "relay": name, "status": status, "error": error,
                    "which": b["which"].as_str().unwrap_or("error"), "lines": cmd["result"]["lines"], "truncated": cmd["result"]["truncated"]}));
            }
        }
    }
    json_resp(202, &json!({"data": {"op_id": op, "relay": name, "status": "pending", "message": "relayd 还没回执；稍后用 GET /v2/api/v1/ops/{op_id} 查看"}}))
}

pub(crate) async fn relay_action(State(app): State<AppState>, Extension(id): Extension<Identity>, Path(relay): Path<String>, body: Bytes) -> Response {
    let Some((host, name)) = relay_host(&app, &relay) else { return err(404, "没有这台线路机") };
    let b = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if b["action"] == "reorder_inbounds" {
        return reorder_inbounds(app, id, host, name, b).await;
    }
    if b["action"] == "xray_logs" {
        return xray_logs(app, id, host, name, b).await;
    }
    if b["action"] != "restart_xray" {
        return err(400, "action 取值：restart_xray | reorder_inbounds | xray_logs");
    }
    if b["confirm"].as_str() != Some(name.as_str()) {
        return err(400, &format!("重启 Xray 会断开这台线路机上的所有连接；确认请传 confirm = \"{name}\""));
    }
    if !app.managed_relays().contains(&host) {
        return err(409, &format!("{name} 的 relayd 还没开启管理模式（最近 60 秒没有来取期望状态），现在不能下发"));
    }
    let (now, user, h2) = (app.clock.now(), id.username.clone(), host.clone());
    let r = app.store.call(move |c| -> rusqlite::Result<String> {
        let tx = c.transaction()?;
        let op = crate::relayspec::create_op(&tx, "restart_xray", &h2, None, "", &json!({}), &user, now)?;
        crate::relayspec::create_command(&tx, &h2, "restart_xray", &json!({}), &op, now)?;
        store::audit(&tx, now, &user, "relay.restart_xray", &format!("relay:{h2}"), &json!({"op": op}))?;
        tx.commit()?;
        Ok(op)
    }).await;
    match r {
        Ok(op) => {
            app.hub.publish("ops", Some(format!("/v2/api/v1/ops/{op}")), now);
            json_resp(202, &json!({"data": {"op_id": op, "relay": name, "generation": Value::Null, "status": "pending"}}))
        }
        Err(e) => err(500, &e.to_string()),
    }
}

async fn entry_share(State(app): State<AppState>, Extension(id): Extension<Identity>, Path((relay, port)): Path<(String, i64)>) -> Response {
    let Some((host, name)) = relay_host(&app, &relay) else { return err(404, "没有这台线路机") };
    let public = match name.as_str() {
        n => app.cached_data("/overview", 3600.0)
            .and_then(|ov| ov["assets"].as_array().and_then(|a| a.iter().find(|x| x["name"] == n).and_then(|x| x["ip"].as_str().map(String::from))))
            .unwrap_or_default(),
    };
    let (h2, now, user) = (host.clone(), app.clock.now(), id.username.clone());
    let r = app.store.call(move |c| -> Result<String, String> {
        let (m, _) = crate::relayspec::base_for_write(c, &h2)?;
        let i = crate::relayspec::find_inbound(&m, port).ok_or("这个端口没有入站")?;
        let remark: Option<String> = c.query_row("SELECT remark FROM entry_meta WHERE relay=? AND port=?", rusqlite::params![h2, port], |r| r.get(0)).ok().flatten();
        let label = remark.unwrap_or_else(|| format!("{name}-{port}"));
        let link = crate::relayspec::share_link(&m["inbounds"][i], &public, &label).ok_or("入站缺少 REALITY 参数，生成不了分享链接")?;
        let _ = store::audit(c, now, &user, "secret.reveal", &format!("{h2}:{port}"), &json!({"what": "share_link"}));
        Ok(link)
    }).await;
    match r {
        Ok(link) => data(json!({"relay": relay, "port": port, "share_link": link})),
        Err(e) => err(409, &e),
    }
}

async fn legacy_write(State(app): State<AppState>, Extension(id): Extension<Identity>, Path(rest): Path<String>, body: Bytes) -> Response {
    crate::legacyhttp::v2_write(&app, &format!("/{rest}"), &body, &id.username).await
}

/// 访问记录增量：`?after=<cursor>&limit=500`（不带 after：最近 limit 条 + 当前游标）。
/// 行 = 旧 /activity rows 的字段 + `rid`（游标）+ `source_intel`；按 rid 升序。SSE 资源 `activity` 变化时调用。
async fn activity_delta(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let after: Option<i64> = q.get("after").and_then(|x| x.parse().ok());
    let limit: i64 = q.get("limit").and_then(|x| x.parse().ok()).unwrap_or(500).clamp(1, 2000);
    let (ls, a2) = (app.legacy_store.clone(), app.clone());
    let r = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let c = ls.reader().ok_or("原生旧库不可用")?;
        let sql = match after {
            Some(_) => "SELECT rowid AS rid, * FROM activity WHERE rowid>?1 ORDER BY rowid LIMIT ?2",
            None => "SELECT * FROM (SELECT rowid AS rid, * FROM activity WHERE rowid>?1 ORDER BY rowid DESC LIMIT ?2) ORDER BY rid",
        };
        let rows = crate::compat::py::rows(&c, sql, &[rusqlite::types::Value::Integer(after.unwrap_or(0)), rusqlite::types::Value::Integer(limit + 1)])
            .map_err(|e| format!("{e:?}"))?;
        let more = rows.len() as i64 > limit;
        let mut rows: Vec<Value> = rows.into_iter().take(limit as usize).map(Value::Object).collect();
        let cursor = rows.last().and_then(|r| r["rid"].as_i64()).or(after).unwrap_or_else(|| c.query_row("SELECT coalesce(max(rowid),0) FROM activity", [], |r| r.get(0)).unwrap_or(0));
        if let (Some(ii), false) = (a2.ipintel.clone(), rows.is_empty()) {
            let ips: Vec<std::net::IpAddr> = rows.iter().filter_map(|r| r["source_ip"].as_str().and_then(|s| s.parse().ok())).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
            let intel = ii.lookup_many(&ips, a2.clock.now());
            for r in rows.iter_mut() {
                let x = r["source_ip"].as_str().map(|ip| intel[ip].clone()).unwrap_or(Value::Null);
                if x.is_object() {
                    r["source_intel"] = json!({"isp": x["isp"], "asn": x["asn"], "country": x["country"], "region": x["region"], "city": x["city"],
                        "place": x["place"], "type": x["type"], "type_word": x["type_word"], "internal": x["internal"]});
                }
            }
        }
        Ok(json!({"cursor": cursor, "more": more, "rows": rows}))
    })
    .await;
    match r {
        Ok(Ok(v)) => data(v),
        Ok(Err(e)) => err(503, &e),
        Err(e) => err(500, &e.to_string()),
    }
}

/// overview / health 增量：`?since=<SSE 版本>`（见 crate::delta 与 API.md §12.5）
async fn resource_delta(State(app): State<AppState>, Path(res): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    if res != "overview" && res != "health" {
        return err(404, "只有 overview、health 提供增量");
    }
    let since = q.get("since").and_then(|x| x.parse::<u64>().ok());
    match app.snapshots.delta(&res, since) {
        Some(v) => data(v),
        None => err(503, "还没有快照，请稍后重试或读整份"),
    }
}

/// 代码 → 中文标签（规则、原因、严重级别），界面统一用这里的文字
async fn labels() -> Response {
    let causes: serde_json::Map<String, Value> = crate::alerts::CAUSE_LABELS.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    let rules: serde_json::Map<String, Value> = crate::alerts::CAUSE_LABELS.iter().map(|(k, _)| (k.to_string(), json!(crate::alerts::rule_title(k)))).collect();
    data(json!({"causes": causes, "rules": rules, "severity": {"critical": "严重", "warning": "警告", "info": "信息"}}))
}

/// 内存构成（本机直连免鉴权）：各缓存的条目 / 字节、SQLite 堆、分配器提交量。
/// `?collect=1` 先让分配器归还空闲页；profile 构建另有 `?drop=rcache,snapshots,window,rollup,memo,conns`（丢弃后比较匿名内存）。
async fn debug_mem(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let before = crate::system::rss_anon_kb();
    #[cfg(feature = "profile")]
    if let Some(d) = q.get("drop") {
        for w in d.split(',') {
            match w {
                "snapshots" => app.snapshots.clear(),
                other => app.legacy_store.drop_caches(other),
            }
        }
    }
    if q.contains_key("collect") || q.contains_key("drop") {
        tokio::task::spawn_blocking(crate::system::allocator_collect).await.ok();
    }
    let after = crate::system::rss_anon_kb();
    let (sq, sq_peak) = crate::system::sqlite_memory();
    let (commit, commit_peak) = crate::system::allocator_commit();
    let ring = app.hub.ring_len();
    data(json!({"rss_anon_kb_before": before, "rss_anon_kb": after,
        "allocator": {"commit_mb": commit as f64 / 1048576.0, "peak_commit_mb": commit_peak as f64 / 1048576.0},
        "sqlite": {"used_mb": sq as f64 / 1048576.0, "peak_mb": sq_peak as f64 / 1048576.0},
        "snapshots": app.snapshots.mem_stats(), "legacy_store": app.legacy_store.mem_stats(),
        "legacy_cache": {"entries": app.legacy.cache.len(), "bytes": app.legacy.cache.bytes()}, "sse_ring": ring,
        "req_stats": app.req_stats.lock().unwrap().len(), "alerts_in_memory": app.engine.lock().unwrap().all_in_memory().count()}))
}

#[cfg(feature = "profile")]
async fn debug_profile(Query(q): Query<HashMap<String, String>>) -> Response {
    let secs: u64 = q.get("seconds").and_then(|x| x.parse().ok()).unwrap_or(30).clamp(5, 300);
    let guard = match pprof::ProfilerGuardBuilder::default().frequency(199).build() {
        Ok(g) => g,
        Err(e) => return err(500, &e.to_string()),
    };
    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
    let report = match guard.report().build() {
        Ok(r) => r,
        Err(e) => return err(500, &e.to_string()),
    };
    let mut lines: Vec<(String, isize)> = report
        .data
        .iter()
        .map(|(frames, n)| {
            let mut stack: Vec<String> = frames.frames.iter().rev().flat_map(|f| f.iter().map(|s| s.name())).collect();
            stack.insert(0, frames.thread_name.clone());
            (stack.join(";"), *n)
        })
        .collect();
    lines.sort_by(|a, b| b.1.cmp(&a.1));
    let body: String = lines.iter().map(|(s, n)| format!("{s} {n}\n")).collect();
    Response::builder().status(200).header(header::CONTENT_TYPE, "text/plain; charset=utf-8").body(Body::from(body)).unwrap()
}

#[cfg(not(feature = "profile"))]
async fn debug_profile() -> Response {
    err(404, "这个构建没有带 profile 特性")
}

fn cpu_ingest_json(app: &App) -> Value {
    let g = *app.ingest.cpu.lock().unwrap();
    json!({"batches": g.0, "cpu_ms": g.1.round()})
}

fn render_cache_json(app: &App) -> Value {
    let (h, m) = *app.legacy_store.rcache_stats.lock().unwrap();
    json!({"hits": h, "renders": m, "gen": app.legacy_store.gen.load(std::sync::atomic::Ordering::Relaxed)})
}

fn req_stats_json(app: &App) -> Value {
    let g = app.req_stats.lock().unwrap();
    let mut v: Vec<(String, u64, f64, Vec<f64>)> = g.iter().map(|(k, (n, ms, r))| (k.clone(), *n, *ms, r.iter().copied().collect())).collect();
    v.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    // p50/p95 取最近 200 次
    Value::Array(
        v.into_iter()
            .take(40)
            .map(|(k, n, ms, mut r)| {
                r.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let p = |q: f64| r.get(((r.len() as f64 - 1.0) * q).round() as usize).map(|x| (x * 10.0).round() / 10.0);
                json!({"path": k, "count": n, "total_ms": ms.round(), "avg_ms": (ms / n as f64 * 10.0).round() / 10.0, "p50_ms": p(0.5), "p95_ms": p(0.95)})
            })
            .collect(),
    )
}

async fn shadow_legacy(State(app): State<AppState>) -> Response {
    let now = app.clock.now();
    if let Some((t, v)) = app.shadow_cache.lock().unwrap().clone() {
        if now - t < 60.0 {
            return data(v);
        }
    }
    let ledger = crate::shadow::ledger_compare(&app).await;
    let feed = app.legacy_store.status.lock().unwrap().clone();
    let v = json!({"generated_at": now, "ledger": ledger, "native_store": {"path": app.legacy_store.path, "feed": feed},
        "paths": crate::shadowcmp::summary(&app.shadow_cmp, now)});
    *app.shadow_cache.lock().unwrap() = Some((now, v.clone()));
    data(v)
}
