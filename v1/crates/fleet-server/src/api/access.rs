//! 访问监控接口（只读）。查询逻辑在 `crate::access`。

use super::ok;
use crate::access::{self, Filter, TargetSort, Window};
use crate::auth::Actor;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

/// 所有参数都按字符串收：前端会带 `team=` 这样的空值，空串一律当作没给
#[derive(Deserialize, Default)]
pub struct Q {
    window: Option<String>,
    team: Option<String>,
    node: Option<String>,
    search: Option<String>,
    action: Option<String>,
    kind: Option<String>,
    limit: Option<String>,
    offset: Option<String>,
    sort: Option<String>,
    before: Option<String>,
}

fn nonempty(s: &Option<String>) -> Option<&str> { s.as_deref().map(str::trim).filter(|s| !s.is_empty()) }

fn uuid_of(s: &Option<String>, what: &str) -> ApiResult<Option<Uuid>> {
    nonempty(s).map(|v| v.parse::<Uuid>().map_err(|_| ApiError::bad_request(format!("{what} 不是有效的 ID")))).transpose()
}

fn int_of(s: &Option<String>, what: &str) -> ApiResult<Option<i64>> {
    nonempty(s).map(|v| v.parse::<i64>().map_err(|_| ApiError::bad_request(format!("{what} 不是整数")))).transpose()
}

fn window_of(q: &Q, default: Window) -> ApiResult<Window> {
    match nonempty(&q.window) {
        None => Ok(default),
        Some(w) => Window::parse(w).ok_or_else(|| ApiError::bad_request("window 只能是 1h、24h、7d、30d")),
    }
}

/// team / node / search / kind / action 公共筛选
fn filter_of(q: &Q, default_window: Window) -> ApiResult<Filter> {
    let w = window_of(q, default_window)?;
    let mut f = Filter::new(w, OffsetDateTime::now_utc());
    f.team = uuid_of(&q.team, "team")?;
    f.nodes = uuid_of(&q.node, "node")?.map(|n| vec![n]);
    f.like = nonempty(&q.search).and_then(access::like_pattern);
    f.kind = match nonempty(&q.kind) {
        None | Some("all") => None,
        Some(k @ ("domain" | "ip")) => Some(k.to_string()),
        Some(_) => return Err(ApiError::bad_request("kind 只能是 all、domain、ip")),
    };
    f.action = match nonempty(&q.action) {
        None | Some("all") => "all",
        Some("blocked") => "blocked",
        Some("routed") => "routed",
        Some(_) => return Err(ApiError::bad_request("action 只能是 all、blocked、routed")),
    };
    Ok(f)
}

fn page_of(q: &Q, default_limit: i64) -> ApiResult<(i64, i64)> {
    let limit = int_of(&q.limit, "limit")?.unwrap_or(default_limit).clamp(1, 500);
    let offset = int_of(&q.offset, "offset")?.unwrap_or(0).clamp(0, 100_000);
    Ok((limit, offset))
}

pub async fn summary(_a: Actor, State(st): State<AppState>, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    let f = filter_of(&q, Window::H24)?;
    let key = (!f.events).then(|| format!("summary|{f:?}"));
    if let Some(v) = key.as_deref().and_then(|k| st.access_cache.get(k)) {
        return Ok(Json(v));
    }
    let (kpi, nodes) = access::summary(&st.db, &f).await?;
    let out = ok(json!({"kpi": kpi, "nodes": nodes, "window_start": access::fmt_ts(f.from)}));
    if let Some(k) = key {
        st.access_cache.put(k, out.0.clone());
    }
    Ok(out)
}

pub async fn targets(_a: Actor, State(st): State<AppState>, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    let f = filter_of(&q, Window::H24)?;
    let (limit, offset) = page_of(&q, 100)?;
    let sort = match nonempty(&q.sort) {
        None | Some("conns") => TargetSort::Conns,
        Some("last") => TargetSort::Last,
        Some("first") => TargetSort::First,
        Some(_) => return Err(ApiError::bad_request("sort 只能是 conns、last、first")),
    };
    let key = (!f.events).then(|| format!("targets|{f:?}|{sort:?}|{limit}|{offset}"));
    if let Some(v) = key.as_deref().and_then(|k| st.access_cache.get(k)) {
        return Ok(Json(v));
    }
    let items = access::targets(&st.db, &f, sort, limit, offset).await?;
    let next = (items.len() as i64 == limit).then_some(offset + limit);
    let out = ok(json!({"items": items, "next_offset": next, "window_start": access::fmt_ts(f.from)}));
    if let Some(k) = key {
        st.access_cache.put(k, out.0.clone());
    }
    Ok(out)
}

pub async fn sources(_a: Actor, State(st): State<AppState>, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    let f = filter_of(&q, Window::H24)?;
    let (limit, _) = page_of(&q, 100)?;
    let mut items = access::sources(&st.db, &f, limit).await?;
    crate::ipintel::attach(&st, &mut items).await;
    Ok(ok(json!({"items": items, "window_start": access::fmt_ts(f.from)})))
}

/// 原始记录：按时间倒序，`before` 是上一页最后一条的 ts（RFC 3339），最多回看 7 天
pub async fn events(_a: Actor, State(st): State<AppState>, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    let f = filter_of(&q, Window::D7)?;
    let (limit, _) = page_of(&q, 100)?;
    let before = nonempty(&q.before)
        .map(|s| OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).map_err(|_| ApiError::bad_request("before 要写成 RFC 3339 时间")))
        .transpose()?;
    let items = access::events(&st.db, &f, before, limit).await?;
    let next = (items.len() as i64 == limit).then(|| items.last().map(|e| access::fmt_ts(e.ts))).flatten();
    Ok(ok(json!({"items": items, "next_before": next})))
}

/// 窗口内第一次出现的目标（旧系统「新域名」的替代，不告警）
pub async fn new_targets(_a: Actor, State(st): State<AppState>, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    let mut f = filter_of(&q, Window::H24)?;
    // 首次出现时间是精确时间，窗口起点不对齐整点
    f.from = OffsetDateTime::now_utc() - window_of(&q, Window::H24)?.duration();
    let (limit, offset) = page_of(&q, 200)?;
    let (items, total) = access::new_targets(&st.db, &f, limit, offset).await?;
    let next = (offset + (items.len() as i64) < total).then_some(offset + items.len() as i64);
    Ok(ok(json!({"items": items, "total": total, "next_offset": next, "window_start": access::fmt_ts(f.from)})))
}

pub async fn machine(_a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    let w = window_of(&q, Window::H24)?;
    let key = (w != Window::H1).then(|| format!("machine|{id}|{w:?}"));
    if let Some(v) = key.as_deref().and_then(|k| st.access_cache.get(k)) {
        return Ok(Json(v));
    }
    let mut m = access::machine_access(&st.db, id, w).await?;
    crate::ipintel::attach(&st, &mut m.top_sources).await;
    let out = ok(m);
    if let Some(k) = key {
        st.access_cache.put(k, out.0.clone());
    }
    Ok(out)
}
