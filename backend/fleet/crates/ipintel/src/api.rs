//! HTTP 处理函数（axum 0.8，与 fleetd 同一框架）。`router()` 返回一个自带状态的 Router，
//! fleetd 在 `.fallback(...).layer(guard)` 之前 `.merge()` 进去即可复用它的鉴权、CSRF 和安全响应头（见 WIRING.md）。
//!
//! 响应格式与 fleetd 自有接口一致：成功 `{"data": …}`，失败 `{"error": "中文说明"}`，`application/json; charset=utf-8`。
use crate::queries::AnomalyFilter;
use crate::service::IpIntel;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

/// fleetd 的 guard 在鉴权通过后插入 `Extension(ipintel::api::Actor(username))`；没插时记为 "admin"。
#[derive(Clone, Debug)]
pub struct Actor(pub String);

type St = Arc<IpIntel>;

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn json_resp(status: u16, v: &Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(serde_json::to_vec(v).unwrap()))
        .unwrap()
}

fn err(status: u16, msg: &str) -> Response {
    json_resp(status, &json!({"error": msg}))
}

fn data(v: Value) -> Response {
    json_resp(200, &json!({"data": v}))
}

/// 路由表（静态段优先于 `{ip}`，所以 anomalies / status 不会被当成 IP）。
pub fn router<S: Clone + Send + Sync + 'static>(svc: Arc<IpIntel>) -> Router<S> {
    Router::new()
        .route("/v2/api/v1/ipintel/anomalies", get(anomalies))
        .route("/v2/api/v1/ipintel/anomalies/{id}/close", post(close))
        .route("/v2/api/v1/ipintel/status", get(status))
        .route("/v2/api/v1/ipintel", get(lookup_many))
        .route("/v2/api/v1/ipintel/batch", get(lookup_many))
        .route("/v2/api/v1/ipintel/{ip}", get(ip_detail))
        .route("/v2/api/v1/nodes/{id}/sources", get(node_sources))
        .with_state(svc)
}

#[allow(clippy::result_large_err)]
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Response> {
    tokio::task::spawn_blocking(f).await.map_err(|_| err(500, "内部错误"))
}

/// GET /v2/api/v1/ipintel?ips=a,b,c（最多 200 个）→ {ip: 画像}；内部地址带 internal（线路机 / 中控名）
pub async fn lookup_many(State(s): State<St>, Query(q): Query<HashMap<String, String>>) -> Response {
    let ips: Vec<IpAddr> = q.get("ips").map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).take(200).collect()).unwrap_or_default();
    if ips.is_empty() {
        return err(400, "需要 ips=逗号分隔的 IP（最多 200 个）");
    }
    match blocking(move || s.lookup_many(&ips, now())).await {
        Ok(v) => data(v),
        Err(r) => r,
    }
}

/// GET /v2/api/v1/ipintel/{ip}
pub async fn ip_detail(State(s): State<St>, Path(ip): Path<String>) -> Response {
    let Ok(addr) = ip.trim().parse::<IpAddr>() else { return err(400, "IP 地址格式不正确") };
    match blocking(move || s.ip_detail(addr, now())).await {
        Ok(Ok(v)) => data(v),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "ipintel ip_detail");
            err(500, "查询失败")
        }
        Err(r) => r,
    }
}

/// GET /v2/api/v1/nodes/{id}/sources?window=24h|7d|30d|<秒>&limit=50
pub async fn node_sources(State(s): State<St>, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let window = match q.get("window").map(String::as_str) {
        None | Some("24h") => 86400.0,
        Some("1h") => 3600.0,
        Some("7d") => 7.0 * 86400.0,
        Some("14d") => 14.0 * 86400.0,
        Some("30d") => 30.0 * 86400.0,
        Some(x) => match x.parse::<f64>() {
            Ok(v) if v > 0.0 && v <= 35.0 * 86400.0 => v,
            _ => return err(400, "window 只能是 1h / 24h / 7d / 14d / 30d 或不超过 35 天的秒数"),
        },
    };
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(50).clamp(1, 500);
    let id2 = id.clone();
    match blocking(move || s.node_sources(&id, window, limit, now())).await {
        Ok(Ok(Some(v))) => data(v),
        // 还没有访问记录（或 id 对不上）：给空但合法的结构，界面显示“近 24 小时没有来源连接”而不是一直转圈
        Ok(Ok(None)) => data(json!({"node": {"id": id2}, "window_s": window, "total": 0, "distinct_sources": 0, "distribution": {}, "top_sources": [],
            "known": {}, "egress": null, "anomalies": [], "empty": true})),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "ipintel node_sources");
            err(500, "查询失败")
        }
        Err(r) => r,
    }
}

/// GET /v2/api/v1/ipintel/anomalies?status=open|closed|all&kind=&node=&severity=&since=&limit=
pub async fn anomalies(State(s): State<St>, Query(q): Query<HashMap<String, String>>) -> Response {
    let status = q.get("status").cloned();
    if let Some(st) = &status {
        if !matches!(st.as_str(), "open" | "closed" | "all") {
            return err(400, "status 只能是 open / closed / all");
        }
    }
    let f = AnomalyFilter {
        status,
        kind: q.get("kind").cloned().filter(|s| !s.is_empty()),
        node: q.get("node").cloned().filter(|s| !s.is_empty()),
        severity: q.get("severity").cloned().filter(|s| !s.is_empty()),
        since: q.get("since").and_then(|x| x.parse().ok()),
        limit: q.get("limit").and_then(|x| x.parse().ok()),
    };
    match blocking(move || s.anomalies(&f)).await {
        Ok(Ok(v)) => data(v),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "ipintel anomalies");
            err(500, "查询失败")
        }
        Err(r) => r,
    }
}

/// POST /v2/api/v1/ipintel/anomalies/{id}/close（写；fleetd guard 已校验 X-Fleet-Request）  body 可选 {"note": "…"}
pub async fn close(State(s): State<St>, actor: Option<Extension<Actor>>, Path(id): Path<i64>, body: Option<Json<Value>>) -> Response {
    let who = actor.map(|a| a.0 .0).unwrap_or_else(|| "admin".into());
    let note = body.and_then(|b| b.0.get("note").and_then(|n| n.as_str()).map(|n| n.chars().take(500).collect::<String>()));
    match blocking(move || s.close_anomaly(id, &who, note.as_deref(), now())).await {
        Ok(Ok(v)) => data(v),
        Ok(Err(e)) => err(409, &e),
        Err(r) => r,
    }
}

/// GET /v2/api/v1/ipintel/status
pub async fn status(State(s): State<St>) -> Response {
    match blocking(move || s.status()).await {
        Ok(v) => data(v),
        Err(r) => r,
    }
}
