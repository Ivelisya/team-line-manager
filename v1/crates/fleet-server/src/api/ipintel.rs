//! 来源 IP 画像接口（只读）。逻辑在 `crate::ipintel`。离线库缺失时不报错：画像字段查不到，`/ipintel/status` 说明缺什么。

use super::ok;
use crate::access::Window;
use crate::auth::Actor;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::Json;
use fleet_ipintel::EXPECTED_DBS;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::IpAddr;
use time::OffsetDateTime;
use uuid::Uuid;

/// 一次最多查多少个地址
const BATCH_MAX: usize = 200;
const NOT_INSTALLED: &str = "IP 库未安装：在中控运行 lab/install-ipintel.sh";

fn parse_ip(s: &str) -> ApiResult<IpAddr> {
    s.trim().parse::<IpAddr>().map_err(|_| ApiError::bad_request(format!("「{}」不是有效的 IP 地址", s.trim())))
}

/// `GET /ipintel/status`：各库是否加载、版本 / 日期、条目数、缺什么
pub async fn status(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut s = st.ipintel.intel.status();
    let cache: i64 = sqlx::query_scalar("select count(*) from ip_intel_cache").fetch_one(&st.db).await?;
    s["enabled"] = json!(st.ipintel.enabled);
    s["cache_rows"] = json!(cache);
    if !st.ipintel.enabled {
        s["hint"] = json!("IP 情报已在配置里关闭（[ipintel] enabled = false）");
    } else if s["installed"] != json!(true) {
        s["hint"] = json!(NOT_INSTALLED);
    } else if s["loaded"].as_u64().unwrap_or(0) < EXPECTED_DBS as u64 {
        s["hint"] = json!("IP 库不全，部分字段查不到：在中控重新运行 lab/install-ipintel.sh 或检查 /var/lib/fleet/ipintel");
    } else {
        s["hint"] = Value::Null;
    }
    Ok(ok(s))
}

/// `GET /ipintel/{ip}`：单个地址的完整画像（含各库原始结果）
pub async fn one(_a: Actor, State(st): State<AppState>, Path(ip): Path<String>) -> ApiResult<Json<Value>> {
    let ip = parse_ip(&ip)?;
    st.ipintel.ensure_internal(&st.db).await;
    let internal = st.ipintel.internal_name(ip);
    let intel = st.ipintel.intel.lookup(ip, internal.as_deref());
    let mut v = serde_json::to_value(&intel).map_err(|e| ApiError::internal(e.to_string()))?;
    let loaded = st.ipintel.intel.loaded();
    v["degraded"] = json!(loaded < EXPECTED_DBS);
    v["loaded"] = json!(loaded);
    Ok(ok(v))
}

#[derive(Deserialize, Default)]
pub struct BatchQ {
    ips: Option<String>,
}

/// `GET /ipintel?ips=a,b,c`（≤200）：批量取摘要画像，查不到的 intel 为 null
pub async fn batch(_a: Actor, State(st): State<AppState>, Query(q): Query<BatchQ>) -> ApiResult<Json<Value>> {
    let list: Vec<&str> = q.ips.as_deref().unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if list.is_empty() {
        return Err(ApiError::bad_request("ips 不能为空，写成 ips=1.2.3.4,5.6.7.8"));
    }
    if list.len() > BATCH_MAX {
        return Err(ApiError::bad_request(format!("一次最多查 {BATCH_MAX} 个地址")));
    }
    st.ipintel.ensure_internal(&st.db).await;
    let parsed: Vec<(&str, Option<IpAddr>)> = list.iter().map(|s| (*s, s.parse::<IpAddr>().ok())).collect();
    let ips: Vec<IpAddr> = parsed.iter().filter_map(|p| p.1).collect();
    // 任意地址不写缓存，免得被拿来灌表
    let map = st.ipintel.enrich(&st.db, &ips, false).await;
    let items: Vec<Value> = parsed
        .into_iter()
        .map(|(raw, ip)| match ip {
            None => json!({"ip": raw, "intel": null, "error": "不是有效的 IP 地址"}),
            Some(ip) => json!({"ip": raw, "intel": map.get(&ip)}),
        })
        .collect();
    Ok(ok(json!({"items": items, "loaded": st.ipintel.intel.loaded(), "degraded": st.ipintel.intel.loaded() == 0})))
}

#[derive(Deserialize, Default)]
pub struct SourcesQ {
    window: Option<String>,
    limit: Option<String>,
}

/// `GET /machines/{id}/sources?window=24h|7d|30d&limit=50`：来源画像分布、top 来源（带画像）、已学基线、学习期
pub async fn machine_sources(_a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>, Query(q): Query<SourcesQ>) -> ApiResult<Json<Value>> {
    let w = match q.window.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Window::H24,
        Some("24h") => Window::H24,
        Some("7d") => Window::D7,
        Some("30d") => Window::D30,
        Some(_) => return Err(ApiError::bad_request("window 只能是 24h、7d、30d")),
    };
    let limit = match q.limit.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => 50,
        Some(s) => s.parse::<i64>().map_err(|_| ApiError::bad_request("limit 不是整数"))?.clamp(1, 500),
    };
    Ok(ok(crate::ipintel::sources_profile(&st, id, w, limit, OffsetDateTime::now_utc()).await?))
}
