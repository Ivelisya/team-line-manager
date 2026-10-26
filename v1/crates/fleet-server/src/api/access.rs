//! 访问监控接口（只读）。查询逻辑在 `crate::access`。

use super::ok;
use crate::access::{self, Filter, TargetSort, Window};
use crate::auth::Actor;
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::state::AppState;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::future::Future;
use std::sync::Arc;
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
    group: Option<String>,
    target: Option<String>,
    conn: Option<String>,
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

/// 读汇总的窗口不能比汇总的保留期长（面板里设的，见 settings.rs）：更早的汇总已经删了，算出来的数是残缺的
pub(super) fn check_retained(st: &AppState, w: Window) -> ApiResult<Window> {
    let days = st.access_retention().hourly_days;
    if w.retained(days) {
        Ok(w)
    } else {
        Err(ApiError::bad_request(format!("访问记录的汇总只保留 {days} 天，看不了最近 {}；要看更久，先在设置里把保留期调长", w.label())))
    }
}

/// 汇总类视图（概况、目标、来源、新目标、单台机器）的窗口：默认 24h，不能超过汇总的保留期
fn agg_window(st: &AppState, q: &Q) -> ApiResult<Window> { check_retained(st, window_of(q, Window::H24)?) }

/// team / node / search / kind / action 公共筛选
fn filter_of(q: &Q, w: Window) -> ApiResult<Filter> {
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

/// 走 `access_cache`：命中时直接序列化缓存里的那份；没命中就算出来放进去。`key` 为 None 时不缓存；
/// `short`：结果里有不随访问汇总变的部分（单台机器的最近原始记录、来源画像的基线与异常）或读的是原始记录（1h 窗口），只短缓存
pub(super) async fn cached(st: &AppState, key: Option<String>, short: bool, compute: impl Future<Output = ApiResult<Value>>) -> ApiResult<Response> {
    if let Some(v) = key.as_deref().and_then(|k| st.access_cache.get(k, short)) {
        return Ok(Json(&*v).into_response());
    }
    let gen = st.access_cache.gen();
    let v = Arc::new(compute.await?);
    if let Some(k) = key {
        st.access_cache.put(k, gen, v.clone());
    }
    Ok(Json(&*v).into_response())
}

pub async fn summary(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<Q>) -> ApiResult<Response> {
    let f = filter_of(&q, agg_window(&st, &q)?)?;
    let key = Some(format!("summary|{}", f.cache_key()));
    cached(&st, key, f.events, async {
        let (kpi, nodes) = access::summary(&st.access_db, &f).await?;
        Ok(ok(json!({"kpi": kpi, "nodes": nodes, "window_start": access::fmt_ts(f.from)})).0)
    })
    .await
}

/// 域名与 IP。`group=target`：按目标合并各节点（带 `total`）；缺省是「目标 × 节点」行，`target=` 只看一个目标（展开看各节点）
pub async fn targets(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<Q>) -> ApiResult<Response> {
    let mut f = filter_of(&q, agg_window(&st, &q)?)?;
    f.host = nonempty(&q.target).map(String::from);
    let (limit, offset) = page_of(&q, 100)?;
    let sort = match nonempty(&q.sort) {
        None | Some("conns") => TargetSort::Conns,
        Some("last") => TargetSort::Last,
        Some("first") => TargetSort::First,
        Some(_) => return Err(ApiError::bad_request("sort 只能是 conns、last、first")),
    };
    let by_target = match nonempty(&q.group) {
        None | Some("node") => false,
        Some("target") => true,
        Some(_) => return Err(ApiError::bad_request("group 只能是 node、target")),
    };
    // 只缓存第一页：往后翻的页一般只有正在看它的那个页面在轮询，进缓存命中不了几次，
    // 反而按页数把缓存撑满、把第一页这些常用的挤掉
    let key = (offset == 0).then(|| format!("targets|{}|{sort:?}|{limit}|{by_target}", f.cache_key()));
    cached(&st, key, f.events, async {
        let window_start = access::fmt_ts(f.from);
        if by_target {
            let (items, total) = access::target_groups(&st.access_db, &f, sort, limit, offset).await?;
            let next = (offset + (items.len() as i64) < total).then_some(offset + items.len() as i64);
            return Ok(ok(json!({"items": items, "total": total, "next_offset": next, "window_start": window_start})).0);
        }
        let items = access::targets(&st.access_db, &f, sort, limit, offset).await?;
        let next = (items.len() as i64 == limit).then_some(offset + limit);
        Ok(ok(json!({"items": items, "next_offset": next, "window_start": window_start})).0)
    })
    .await
}

/// 按接入方式筛来源时最多取回这么多个，查完画像再筛、再取前 limit 个
const CONN_SCAN: i64 = 5000;

/// `conn=`：只要这种接入方式的来源（unknown 含查不到画像的）。画像缓存里已经查过的在库里就筛掉；
/// 缓存里还没有的（刚出现、库换了版本）要现查，取回后再筛。取回的到了 CONN_SCAN 个、筛完又不够 limit 个时，后面可能还有没看到的：`scan_capped` 为 true
pub async fn sources(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<Q>) -> ApiResult<Response> {
    let f = filter_of(&q, agg_window(&st, &q)?)?;
    let (limit, _) = page_of(&q, 100)?;
    let conn = match nonempty(&q.conn) {
        None | Some("all") => None,
        Some(c @ ("residential" | "mobile" | "datacenter" | "vpn" | "unknown")) => Some(c.to_string()),
        Some(_) => return Err(ApiError::bad_request("conn 只能是 all、residential、mobile、datacenter、vpn、unknown")),
    };
    let key = Some(format!("sources|{}|{limit}|{conn:?}", f.cache_key()));
    cached(&st, key, f.events, async {
        let ver = st.ipintel.version();
        let scan = if conn.is_some() { CONN_SCAN } else { limit };
        let mut items = access::sources(&st.access_db, &f, conn.as_deref().map(|c| (c, ver.as_str())), scan).await?;
        let full = conn.is_some() && items.len() as i64 == CONN_SCAN;
        crate::ipintel::attach(&st, &mut items).await;
        if let Some(c) = &conn {
            items.retain(|s| s.intel.as_ref().map_or(c == "unknown", |i| i.conn_type == *c));
            items.truncate(limit as usize);
        }
        let scan_capped = full && (items.len() as i64) < limit;
        Ok(ok(json!({"items": items, "scan_capped": scan_capped, "window_start": access::fmt_ts(f.from)})).0)
    })
    .await
}

/// 原始记录：按时间倒序，`before` 原样传回上一页的 `next_before`（不透明游标），回看所选窗口（默认 7 天）。
/// 原始记录只留面板里设的天数，窗口比它长时只是更早的已经没有了，不报错（访问页各视图共用一个窗口，汇总看得到的这里照样能选）
pub async fn events(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<Q>) -> ApiResult<Json<Value>> {
    let f = filter_of(&q, window_of(&q, Window::D7)?)?;
    let (limit, _) = page_of(&q, 100)?;
    let before = nonempty(&q.before)
        .map(|s| access::parse_event_cursor(s).ok_or_else(|| ApiError::bad_request("before 不对：原样传上一页返回的 next_before")))
        .transpose()?;
    let items = access::events(&st.access_db, &f, before, limit).await?;
    let next = (items.len() as i64 == limit).then(|| items.last().map(access::event_cursor)).flatten();
    Ok(ok(json!({"items": items, "next_before": next})))
}

/// 窗口内第一次出现的目标（旧系统「新域名」的替代，不告警）
pub async fn new_targets(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<Q>) -> ApiResult<Json<Value>> {
    let w = agg_window(&st, &q)?;
    let mut f = filter_of(&q, w)?;
    // 首次出现时间是精确时间，窗口起点不对齐整点
    f.from = OffsetDateTime::now_utc() - w.duration();
    let (limit, offset) = page_of(&q, 200)?;
    let (items, total) = access::new_targets(&st.access_db, &f, limit, offset).await?;
    let next = (offset + (items.len() as i64) < total).then_some(offset + items.len() as i64);
    Ok(ok(json!({"items": items, "total": total, "next_offset": next, "window_start": access::fmt_ts(f.from)})))
}

pub async fn machine(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<Q>) -> ApiResult<Response> {
    let w = agg_window(&st, &q)?;
    let key = Some(format!("machine|{id}|{w:?}"));
    cached(&st, key, true, async {
        let mut m = access::machine_access(&st.access_db, id, w, st.access_retention().raw_days).await?;
        crate::ipintel::attach(&st, &mut m.top_sources).await;
        Ok(ok(m).0)
    })
    .await
}
