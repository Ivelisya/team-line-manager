//! HTTP 路由。两组都是泛型 `Router<S>`，可以直接 merge 进 fleetd 的 `Router<AppState>`：
//! - `ingest_routes`：边缘上报，Bearer 令牌；**放在 fleetd 管理员 guard 之外**；
//! - `read_routes`：只读查询；**放在 fleetd 管理员 guard 之内**（本模块自己不做管理员鉴权）。
use crate::{now, Accepted, Shared};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde::Deserialize;
use serde_json::{json, Value};

fn resp(status: u16, v: &Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(serde_json::to_vec(v).unwrap()))
        .unwrap()
}

fn err(status: u16, msg: &str) -> Response {
    resp(status, &json!({"error": msg}))
}

pub fn ingest_routes<S: Clone + Send + Sync + 'static>(ing: Shared) -> Router<S> {
    let limit = ing.cfg.body_max;
    Router::new()
        .route("/v2/ingest/node", post(ingest_node))
        .route("/v2/ingest/relay", post(ingest_relay))
        .layer(DefaultBodyLimit::max(limit))
        .layer(Extension(ing))
}

pub fn read_routes<S: Clone + Send + Sync + 'static>(ing: Shared) -> Router<S> {
    Router::new()
        .route("/v2/api/v1/hosts", get(hosts))
        .route("/v2/api/v1/hosts/{id}/series", get(series))
        .route("/v2/api/v1/relays/{id}/xray", get(relay_xray))
        .route("/v2/api/v1/relays/{id}/probes", get(relay_probes))
        .route("/v2/api/v1/ingest/alerts", get(alerts))
        .layer(Extension(ing))
}

async fn ingest_node(Extension(ing): Extension<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    ingest(ing, "node", headers, body).await
}

async fn ingest_relay(Extension(ing): Extension<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    ingest(ing, "relay", headers, body).await
}

fn bearer(h: &HeaderMap) -> Option<String> {
    let v = h.get(header::AUTHORIZATION)?.to_str().ok()?;
    let t = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))?.trim();
    (t.len() >= 20 && t.len() <= 256).then(|| t.to_string())
}

fn thread_ms() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime 只写入 ts
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 * 1000.0 + ts.tv_nsec as f64 / 1e6
}

async fn ingest(ing: Shared, kind: &'static str, headers: HeaderMap, body: Bytes) -> Response {
    let t = now();
    let Some(tok) = bearer(&headers) else {
        if !ing.authfail_allow(t) {
            return too_many(5);
        }
        return err(401, "缺少令牌");
    };
    let ing_notify = ing.clone();
    let r = tokio::task::spawn_blocking(move || -> Result<(Accepted, String), crate::Reject> {
        struct CpuGuard<'a>(&'a crate::Ingest, f64);
        impl Drop for CpuGuard<'_> {
            fn drop(&mut self) {
                let mut g = self.0.cpu.lock().unwrap();
                g.0 += 1;
                g.1 += thread_ms() - self.1;
            }
        }
        let _cpu = CpuGuard(&ing, thread_ms());
        let Some(host) = ing.auth(&tok) else {
            if !ing.authfail_allow(t) {
                return Err(crate::Reject { status: 429, msg: "鉴权失败过多".into(), retry_after: Some(5) });
            }
            return Err(crate::Reject { status: 401, msg: "令牌无效或已撤销".into(), retry_after: None });
        };
        if let Err(after) = ing.rate_check(&host, t) {
            return Err(crate::Reject { status: 429, msg: "上报过于频繁".into(), retry_after: Some(after) });
        }
        let v: Value = serde_json::from_slice(&body).map_err(|e| crate::Reject { status: 400, msg: format!("JSON 无效：{e}"), retry_after: None })?;
        let a = ing.accept(&host, kind, &v, t)?;
        Ok((a, host))
    })
    .await;
    match r {
        Ok(Ok((Accepted::Ok { samples, skipped }, host))) => {
            tracing::debug!(host = %host, kind, samples, skipped, "ingest ok");
            ing_notify.accepted.notify_one();
            let mut body = json!({"status": "ok", "samples": samples, "skipped": skipped});
            if ing_notify.live.load(std::sync::atomic::Ordering::Relaxed) {
                body["live"] = json!(true);
            }
            resp(200, &body)
        }
        Ok(Ok((Accepted::Duplicate, _))) => resp(200, &json!({"status": "duplicate"})),
        Ok(Err(rj)) => {
            if rj.status >= 500 {
                tracing::warn!(status = rj.status, error = %rj.msg, "ingest failed");
            }
            match rj.retry_after {
                Some(a) if rj.status == 429 => too_many(a),
                _ => err(rj.status, &rj.msg),
            }
        }
        Err(e) => err(500, &format!("内部错误：{e}")),
    }
}

fn too_many(after: u64) -> Response {
    let mut r = err(429, "请求过于频繁");
    r.headers_mut().insert(header::RETRY_AFTER, after.to_string().parse().unwrap());
    *r.status_mut() = StatusCode::TOO_MANY_REQUESTS;
    r
}

/// "90s" "15m" "24h" "7d" 或纯秒数
pub fn parse_range(s: Option<&str>, default: i64) -> Result<i64, String> {
    let Some(s) = s.map(|x| x.trim()).filter(|x| !x.is_empty()) else { return Ok(default) };
    let (num, mul) = match s.chars().last().unwrap() {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        _ => (s, 1),
    };
    num.parse::<i64>().ok().filter(|n| *n > 0).map(|n| n * mul).ok_or_else(|| format!("range 无效：{s}"))
}

async fn hosts(Extension(ing): Extension<Shared>) -> Response {
    let v = tokio::task::spawn_blocking(move || ing.hosts(now())).await.unwrap_or(Value::Null);
    resp(200, &json!({"data": v}))
}

#[derive(Deserialize)]
struct SeriesQ {
    metric: Option<String>,
    range: Option<String>,
    step: Option<i64>,
}

async fn series(Extension(ing): Extension<Shared>, Path(id): Path<String>, Query(q): Query<SeriesQ>) -> Response {
    let range = match parse_range(q.range.as_deref(), 3600) {
        Ok(r) => r,
        Err(e) => return err(400, &e),
    };
    let metric = q.metric.unwrap_or_else(|| "cpu".into());
    let r = tokio::task::spawn_blocking(move || ing.series(&id, &metric, range, q.step, now())).await;
    match r {
        Ok(Ok(v)) => resp(200, &json!({"data": v})),
        Ok(Err(e)) => err(400, &e),
        Err(e) => err(500, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct XrayQ {
    range: Option<String>,
    kind: Option<String>,
    tag: Option<String>,
}

async fn relay_xray(Extension(ing): Extension<Shared>, Path(id): Path<String>, Query(q): Query<XrayQ>) -> Response {
    let range = match parse_range(q.range.as_deref(), 3600) {
        Ok(r) => r,
        Err(e) => return err(400, &e),
    };
    let r = tokio::task::spawn_blocking(move || ing.relay_xray(&id, range, q.kind.as_deref(), q.tag.as_deref(), now())).await;
    match r {
        Ok(Some(v)) => resp(200, &json!({"data": v})),
        Ok(None) => err(404, "没有这台线路机的上报"),
        Err(e) => err(500, &e.to_string()),
    }
}

async fn alerts(Extension(ing): Extension<Shared>) -> Response {
    let v = tokio::task::spawn_blocking(move || ing.evaluate(now())).await.unwrap_or_default();
    resp(200, &json!({"data": v}))
}

#[derive(Deserialize)]
struct RangeQ {
    range: Option<String>,
}

async fn relay_probes(Extension(ing): Extension<Shared>, Path(id): Path<String>, Query(q): Query<RangeQ>) -> Response {
    let range = match parse_range(q.range.as_deref(), 900) {
        Ok(r) => r,
        Err(e) => return err(400, &e),
    };
    match tokio::task::spawn_blocking(move || ing.relay_probes(&id, range, now())).await {
        Ok(Some(v)) => resp(200, &json!({"data": v})),
        Ok(None) => err(404, "没有这台线路机的探测上报"),
        Err(e) => err(500, &e.to_string()),
    }
}
