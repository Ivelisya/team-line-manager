//! 中控自己的接口耗时与连接池（selfmon 的一部分）：「中控慢不慢、慢在哪」不用登录中控看日志也答得上来。
//!
//! - 请求层（`track`，lib.rs 挂在最外层）：每个请求记方法、匹配到的路由模板（`/api/v1/machines/{id}`，不记原始路径：
//!   路径里的 id、订阅与接入令牌不进日志）、状态码、耗时，agent 的接口另记鉴权认出的机器（`set_machine`）。
//!   慢（超过 `[selfmon] slow_request_ms`，agent 接口 `slow_agent_ms`）或 5xx 的记 INFO，其余记 DEBUG，平常的日志量不变。
//!   期望状态的长轮询本来就挂 30 秒：超过 30 秒 + `slow_agent_ms` 才算慢；WebSocket（实时流、网页终端）只量握手（101），
//!   连着的时长不算慢，打开、关闭各记一条 DEBUG（`StreamGuard`，网页终端另有自己的 INFO）。
//!   耗时量到响应头交出去为止（流式的响应体不算在内）。
//! - 每个路由模板一份耗时分布：进程启动以来、最近 15 分钟（按分钟滚动）。
//! - 连接池（主池、机器视图池、访问监控池）：每 5 秒采样一次大小、空闲、占满没有；主池与视图池另做一次「取连接」计时（拿到马上还），
//!   请求在池子前排队时它跟着变慢。sqlx 不给排队的个数，用「采样时占满了」的比例代替。
//!
//! `GET /api/v1/server/stats`（`stats_json`）、`fleetctl server-stats` 读它；进程重启后从零开始。

use crate::state::AppState;
use crate::{lock, rlock, wlock};
use axum::extract::{MatchedPath, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use serde::Serialize;
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use uuid::Uuid;

/// 耗时分桶的上界（毫秒），最后一桶是更慢的
const BOUNDS_MS: [f64; 13] = [5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10_000.0, 30_000.0, 60_000.0];
/// 「最近」的窗口：这么多个一分钟
const RECENT_MIN: i64 = 15;
/// 连接池采样间隔、留多少份（15 分钟）
const SAMPLE_EVERY: Duration = Duration::from_secs(5);
const SAMPLES_KEEP: usize = 180;
/// 期望状态长轮询的服务端等待（agent_api::desired 的 30 秒）
const LONG_POLL_MS: u64 = 30_000;

tokio::task_local! {
    /// 本请求鉴权认出的机器（agent 接口，`set_machine`）；请求层读它写日志
    static MACHINE: Cell<Option<Uuid>>;
}

/// agent 接口鉴权通过后调：本请求的日志带上机器 id（不在请求层里时什么都不做）
pub fn set_machine(id: Uuid) { let _ = MACHINE.try_with(|c| c.set(Some(id))); }

/// 路由的种类：决定慢的门槛
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Api,
    Agent,
    /// 期望状态长轮询
    LongPoll,
    /// WebSocket 握手
    Stream,
    /// 前端页面、静态文件、订阅拉取等
    Other,
}

fn kind_of(route: &str, status: StatusCode) -> Kind {
    if status == StatusCode::SWITCHING_PROTOCOLS {
        Kind::Stream
    } else if route == "/agent/v1/desired" {
        Kind::LongPoll
    } else if route.starts_with("/agent/") || route == "/echo" {
        Kind::Agent
    } else if route.starts_with("/api/") {
        Kind::Api
    } else {
        Kind::Other
    }
}

#[derive(Clone, Default)]
struct Hist {
    n: u64,
    sum_ms: f64,
    max_ms: f64,
    buckets: [u64; BOUNDS_MS.len() + 1],
    errors: u64,
    slow: u64,
}

impl Hist {
    fn add(&mut self, ms: f64, error: bool, slow: bool) {
        self.n += 1;
        self.sum_ms += ms;
        self.max_ms = self.max_ms.max(ms);
        self.buckets[BOUNDS_MS.iter().position(|b| ms <= *b).unwrap_or(BOUNDS_MS.len())] += 1;
        self.errors += error as u64;
        self.slow += slow as u64;
    }

    fn merge(&mut self, o: &Hist) {
        self.n += o.n;
        self.sum_ms += o.sum_ms;
        self.max_ms = self.max_ms.max(o.max_ms);
        for (a, b) in self.buckets.iter_mut().zip(o.buckets) {
            *a += b;
        }
        self.errors += o.errors;
        self.slow += o.slow;
    }

    /// 第 q 分位落在哪一桶，取那一桶的上界（不超过见过的最大值）：只是量级，不是精确值
    fn quantile(&self, q: f64) -> Option<f64> {
        if self.n == 0 {
            return None;
        }
        let want = ((self.n as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, c) in self.buckets.iter().enumerate() {
            seen += c;
            if seen >= want {
                return Some(BOUNDS_MS.get(i).copied().unwrap_or(f64::INFINITY).min(self.max_ms));
            }
        }
        Some(self.max_ms)
    }

    fn summary(&self) -> Value {
        let r = |v: f64| (v * 10.0).round() / 10.0;
        json!({
            "count": self.n, "errors": self.errors, "slow": self.slow,
            "mean_ms": (self.n > 0).then(|| r(self.sum_ms / self.n as f64)),
            "p50_ms": self.quantile(0.5).map(r), "p90_ms": self.quantile(0.9).map(r), "p99_ms": self.quantile(0.99).map(r),
            "max_ms": (self.n > 0).then(|| r(self.max_ms)), "total_ms": r(self.sum_ms),
        })
    }
}

#[derive(Default)]
struct Route {
    kind: Option<Kind>,
    total: Hist,
    /// (Unix 分钟, 这一分钟的)，旧→新，最多 RECENT_MIN 份
    recent: VecDeque<(i64, Hist)>,
}

impl Route {
    fn recent(&self, now_min: i64) -> Hist {
        let mut h = Hist::default();
        for (_, x) in self.recent.iter().filter(|(m, _)| now_min - m < RECENT_MIN) {
            h.merge(x);
        }
        h
    }
}

/// 一个连接池的一次采样
#[derive(Clone, Copy)]
struct PoolSample {
    size: u32,
    idle: usize,
    /// 采样那一刻占满了（连接数到了上限且没有空闲的）：之后的请求要排队
    full: bool,
    /// 「取连接」计时（毫秒）；这个池子不计时、或 5 秒内没取到为 None（没取到记在 timeouts 里）
    acquire_ms: Option<f64>,
    timeout: bool,
}

/// 请求与连接池的统计（`AppState::http`）
pub struct Stats {
    started: OffsetDateTime,
    routes: Mutex<HashMap<(String, String), Route>>,
    inflight: AtomicI64,
    inflight_max: AtomicI64,
    streams: AtomicI64,
    pools: RwLock<HashMap<&'static str, VecDeque<PoolSample>>>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            started: OffsetDateTime::now_utc(),
            routes: Default::default(),
            inflight: AtomicI64::new(0),
            inflight_max: AtomicI64::new(0),
            streams: AtomicI64::new(0),
            pools: Default::default(),
        }
    }
}

impl Stats {
    fn record(&self, method: &str, route: &str, kind: Kind, ms: f64, error: bool, slow: bool) {
        let now_min = OffsetDateTime::now_utc().unix_timestamp().div_euclid(60);
        let mut m = lock(&self.routes);
        let r = m.entry((method.to_string(), route.to_string())).or_default();
        r.kind = Some(kind);
        r.total.add(ms, error, slow);
        if r.recent.back().is_none_or(|(t, _)| *t != now_min) {
            r.recent.push_back((now_min, Hist::default()));
        }
        while r.recent.front().is_some_and(|(t, _)| now_min - t >= RECENT_MIN) {
            r.recent.pop_front();
        }
        if let Some((_, h)) = r.recent.back_mut() {
            h.add(ms, error, slow);
        }
    }

    fn sample(&self, name: &'static str, s: PoolSample) {
        let mut m = wlock(&self.pools);
        let q = m.entry(name).or_default();
        q.push_back(s);
        while q.len() > SAMPLES_KEEP {
            q.pop_front();
        }
    }
}

/// 连着的 WebSocket（实时流、网页终端）：拿着它的期间算一个，drop 时记一条 DEBUG（连了多久）
pub struct StreamGuard {
    st: AppState,
    route: &'static str,
    since: Instant,
}

impl StreamGuard {
    pub fn new(st: &AppState, route: &'static str) -> Self {
        st.http.streams.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(route, "WebSocket 打开");
        Self { st: st.clone(), route, since: Instant::now() }
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.st.http.streams.fetch_sub(1, Ordering::Relaxed);
        tracing::debug!(route = self.route, secs = self.since.elapsed().as_secs(), "WebSocket 关闭");
    }
}

/// 路由模板：匹配到的（含 nest 的前缀）；没匹配到的按前缀归成几类，不把原始路径当键（扫描器乱打的路径撑不大表，也不进日志）
fn route_of(req: &Request) -> String {
    let path = req.uri().path();
    match req.extensions().get::<MatchedPath>().map(|m| m.as_str()) {
        Some(m) if !m.contains("__private__axum") => m.to_string(),
        _ if path.starts_with("/api/") || path.starts_with("/agent/") => "(没有这个接口)".into(),
        _ if path.starts_with("/assets/") => "/assets/…".into(),
        _ => "(前端页面)".into(),
    }
}

/// 慢的门槛（毫秒）；WebSocket 握手不算慢
fn slow_ms(st: &AppState, kind: Kind) -> Option<u64> {
    let c = &st.cfg.selfmon;
    match kind {
        Kind::Stream => None,
        Kind::LongPoll => Some(LONG_POLL_MS + c.slow_agent_ms),
        Kind::Agent => Some(c.slow_agent_ms),
        Kind::Api | Kind::Other => Some(c.slow_request_ms),
    }
}

/// 请求层（lib.rs）：计时、按路由模板记分布，慢的与 5xx 记 INFO，其余 DEBUG
pub async fn track(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_string();
    let route = route_of(&req);
    let n = st.http.inflight.fetch_add(1, Ordering::Relaxed) + 1;
    st.http.inflight_max.fetch_max(n, Ordering::Relaxed);
    let t0 = Instant::now();
    let (res, machine) = MACHINE
        .scope(Cell::new(None), async {
            let res = next.run(req).await;
            (res, MACHINE.with(|c| c.get()))
        })
        .await;
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    st.http.inflight.fetch_sub(1, Ordering::Relaxed);
    let status = res.status();
    let kind = kind_of(&route, status);
    let slow = slow_ms(&st, kind).is_some_and(|t| ms >= t as f64);
    let error = status.is_server_error();
    st.http.record(&method, &route, kind, ms, error, slow);
    let ms = ms.round() as u64;
    let s = status.as_u16();
    macro_rules! log {
        ($lvl:ident, $msg:literal) => {
            match machine {
                Some(m) => tracing::$lvl!(target: "fleet_server::http", method = %method, route = %route, status = s, ms, machine = %m, $msg),
                None => tracing::$lvl!(target: "fleet_server::http", method = %method, route = %route, status = s, ms, $msg),
            }
        };
    }
    if error {
        log!(info, "请求出错（5xx）");
    } else if slow {
        log!(info, "慢请求");
    } else {
        log!(debug, "请求");
    }
    res
}

/// 每 5 秒采样一次各连接池（后台任务，lib.rs 的 spawn_background）
pub async fn run_forever(st: AppState) {
    let mut t = tokio::time::interval(SAMPLE_EVERY);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        t.tick().await;
        sample_once(&st).await;
    }
}

/// 采样一次（测试直接调）。只给主池和视图池计「取连接」：访问监控池平时没人用，计时会让它一直留着一条连接
pub async fn sample_once(st: &AppState) {
    for (name, pool, timed) in [("main", &st.db, true), ("view", &st.view_db, true), ("access", &st.access_db, false)] {
        let max = pool.options().get_max_connections();
        let (size, idle) = (pool.size(), pool.num_idle());
        let full = size >= max && idle == 0;
        let (mut acquire_ms, mut timeout) = (None, false);
        if timed {
            let t0 = Instant::now();
            match tokio::time::timeout(Duration::from_secs(5), pool.acquire()).await {
                Ok(Ok(c)) => {
                    acquire_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
                    drop(c);
                }
                Ok(Err(_)) | Err(_) => timeout = true,
            }
        }
        st.http.sample(name, PoolSample { size, idle, full, acquire_ms, timeout });
    }
}

/// `GET /api/v1/server/stats` 的数据：见 docs/API.md
pub fn stats_json(st: &AppState) -> Value {
    let s = &st.http;
    let now = OffsetDateTime::now_utc();
    let now_min = now.unix_timestamp().div_euclid(60);
    let c = &st.cfg.selfmon;
    let mut routes: Vec<(f64, f64, Value)> = lock(&s.routes)
        .iter()
        .map(|((m, r), x)| {
            let recent = x.recent(now_min);
            let v = json!({"method": m, "route": r, "kind": x.kind, "recent": recent.summary(), "total": x.total.summary()});
            (recent.sum_ms, x.total.sum_ms, v)
        })
        .collect();
    // 最近 15 分钟占用时间最多的在前（同样多的按启动以来）
    routes.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.total_cmp(&a.1)));
    let pools: Vec<Value> = [("main", &st.db), ("view", &st.view_db), ("access", &st.access_db)]
        .iter()
        .map(|(name, pool)| {
            let q = rlock(&s.pools).get(name).cloned().unwrap_or_default();
            let acq: Vec<f64> = q.iter().filter_map(|x| x.acquire_ms).collect();
            let r = |v: f64| (v * 10.0).round() / 10.0;
            json!({
                "name": name, "max": pool.options().get_max_connections(), "size": pool.size(), "idle": pool.num_idle(),
                "in_use": pool.size().saturating_sub(pool.num_idle() as u32),
                "samples": q.len(), "full_samples": q.iter().filter(|x| x.full).count(),
                "max_in_use": q.iter().map(|x| x.size.saturating_sub(x.idle as u32)).max(),
                "acquire_ms": {
                    "last": q.back().and_then(|x| x.acquire_ms).map(r),
                    "mean": (!acq.is_empty()).then(|| r(acq.iter().sum::<f64>() / acq.len() as f64)),
                    "max": acq.iter().copied().reduce(f64::max).map(r),
                    "timeouts": q.iter().filter(|x| x.timeout).count(),
                },
            })
        })
        .collect();
    json!({
        "started_at": crate::meta::fmt_ts(s.started),
        "uptime_s": (now - s.started).whole_seconds(),
        "recent_minutes": RECENT_MIN,
        "sample_every_s": SAMPLE_EVERY.as_secs(),
        "slow_ms": {"api": c.slow_request_ms, "agent": c.slow_agent_ms, "long_poll": LONG_POLL_MS + c.slow_agent_ms},
        "inflight": s.inflight.load(Ordering::Relaxed),
        "inflight_max": s.inflight_max.load(Ordering::Relaxed),
        "streams": s.streams.load(Ordering::Relaxed),
        "pools": pools,
        "routes": routes.into_iter().map(|x| x.2).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_quantiles_are_bucket_bounds() {
        let mut h = Hist::default();
        for ms in [3.0, 4.0, 7.0, 40.0, 900.0] {
            h.add(ms, false, ms > 500.0);
        }
        h.add(1200.0, true, true);
        assert_eq!((h.n, h.errors, h.slow), (6, 1, 2));
        assert_eq!(h.quantile(0.5), Some(10.0));
        assert_eq!(h.quantile(0.9), Some(1200.0), "最慢一桶的上界不超过见过的最大值");
        assert_eq!(h.quantile(0.99), Some(1200.0));
        assert_eq!(Hist::default().quantile(0.5), None);
        let v = h.summary();
        assert_eq!((v["count"].as_u64(), v["max_ms"].as_f64(), v["mean_ms"].as_f64()), (Some(6), Some(1200.0), Some(359.0)));
    }

    #[test]
    fn kinds_and_thresholds() {
        assert!(kind_of("/agent/v1/desired", StatusCode::NO_CONTENT) == Kind::LongPoll);
        assert!(kind_of("/agent/v1/report", StatusCode::OK) == Kind::Agent);
        assert!(kind_of("/api/v1/stream", StatusCode::SWITCHING_PROTOCOLS) == Kind::Stream);
        assert!(kind_of("/api/v1/stream", StatusCode::UNAUTHORIZED) == Kind::Api, "没升级成 WebSocket 的按普通接口算");
        assert!(kind_of("(前端页面)", StatusCode::OK) == Kind::Other);
    }
}
