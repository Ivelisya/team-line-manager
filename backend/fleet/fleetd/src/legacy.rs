//! legacy-source：旧 team-ops 后端（127.0.0.1:25800）的只读适配。
//!
//! 硬规则（代码层面保证，不靠约定）：
//! - 只有 GET：这个模块唯一的上游请求出口是 `send_get`，方法写死为 GET；模块里没有任何发 POST 的代码路径；
//! - 路径只允许旧插件 allowedGet 白名单（+ public-status），其余一律 404；
//! - 到旧后端的并发上限 2（共享轮询与按需代理共用同一个信号量）。
use crate::cache::{Cache, Entry};
use crate::clock::Clock;
use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// 旧插件 plugin-live/script.js 的 allowedGet（+ 会话路由 public-status）。`:x` 匹配一段。
pub const ALLOWED_GET: &[&str] = &[
    "overview", "activity", "history", "policies", "policies/preview", "audit", "renewals", "export", "health", "fleet",
    "node", "incidents", "behavior", "panel/overview", "schema", "api-keys", "api-keys/usage", "provision/defaults", "jobs",
    "jobs/:id", "schema.md", "komari/hosts", "komari/hosts/:uuid/metrics", "komari/pings", "komari/pings/:task_id/results",
    "komari/clients", "public-status",
];

pub fn path_allowed(path: &str) -> bool {
    if path.contains("..") || path.contains("//") {
        return false;
    }
    let p = path.trim_start_matches('/');
    if p.is_empty() {
        return false;
    }
    let segs: Vec<&str> = p.split('/').collect();
    ALLOWED_GET.iter().any(|pat| {
        let ps: Vec<&str> = pat.split('/').collect();
        ps.len() == segs.len() && ps.iter().zip(&segs).all(|(a, b)| if a.starts_with(':') { !b.is_empty() } else { a == b })
    })
}

/// 规范化缓存键：路径 + 按键排序的查询串（参数顺序不同的同一请求共用缓存）。
pub fn normalize(path: &str, query: Option<&str>) -> Option<String> {
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    if !path_allowed(&path) {
        return None;
    }
    let mut pairs: Vec<(String, String)> = form_urlencoded::parse(query.unwrap_or("").as_bytes()).into_owned().collect();
    // 与旧后端一致：同名参数取最后一个
    pairs.reverse();
    let mut seen = std::collections::HashSet::new();
    pairs.retain(|(k, _)| seen.insert(k.clone()));
    pairs.sort();
    if pairs.is_empty() {
        return Some(path);
    }
    let q: String = form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs.iter()).finish();
    Some(format!("{path}?{q}"))
}

pub fn split_key(key: &str) -> (&str, &str) {
    key.split_once('?').unwrap_or((key, ""))
}

#[derive(Default, Debug, Clone, serde::Serialize)]
pub struct UpstreamStats {
    pub requests: u64,
    pub errors: u64,
    pub consecutive_errors: u32,
    pub last_ok_at: Option<f64>,
    pub last_error: Option<String>,
    pub last_error_at: Option<f64>,
    #[serde(skip)]
    pub latencies: VecDeque<f64>,
    pub bytes: u64,
}

impl UpstreamStats {
    pub fn record(&mut self, ok: bool, latency_ms: f64, now: f64, err: Option<String>, bytes: usize) {
        self.requests += 1;
        self.bytes += bytes as u64;
        if ok {
            self.consecutive_errors = 0;
            self.last_ok_at = Some(now);
        } else {
            self.errors += 1;
            self.consecutive_errors += 1;
            self.last_error = err;
            self.last_error_at = Some(now);
        }
        self.latencies.push_back(latency_ms);
        while self.latencies.len() > 200 {
            self.latencies.pop_front();
        }
    }

    pub fn percentiles(&self) -> serde_json::Value {
        let mut v: Vec<f64> = self.latencies.iter().copied().collect();
        if v.is_empty() {
            return serde_json::json!({"n": 0});
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
        serde_json::json!({"n": v.len(), "p50_ms": (p(0.5) * 10.0).round() / 10.0, "p95_ms": (p(0.95) * 10.0).round() / 10.0,
            "max_ms": (v[v.len() - 1] * 10.0).round() / 10.0})
    }
}

/// 到旧后端的并发：轮询 1 + 轻请求 2 + 重请求 1 = 4（旧后端 CPUQuota=50%）。v0.3.5 起 legacy.concurrency 不再使用。
pub const LIGHT_SLOTS: usize = 2;
pub const HEAVY_SLOTS: usize = 1;
/// 按需请求的总时限（排队 + 旧后端处理），超时回 503
pub const DEMAND_DEADLINE_S: u64 = 25;

pub fn is_heavy(key: &str) -> bool {
    let path = key.split('?').next().unwrap_or(key);
    matches!(path, "/activity" | "/export" | "/policies/preview")
}

pub struct Legacy {
    client: reqwest::Client,
    base: String,
    key: Option<String>,
    actor: String,
    /// 共享轮询器（overview/health/panel …）独占的槽：按需代理永远拿不到它
    poll_sem: Semaphore,
    /// 按需代理：轻请求（/node、/history、/behavior?asset …）两个槽
    light_sem: Semaphore,
    /// 按需代理：重请求（/activity、/export、/policies/preview）单独一个槽，占不住轻请求
    heavy_sem: Semaphore,
    pub cache: Cache,
    pub stats: Mutex<UpstreamStats>,
    clock: Arc<dyn Clock>,
    /// 下线旧系统后：同一套只读接口改由原生旧库渲染（legacy_source = "native"）
    pub native: std::sync::OnceLock<Arc<crate::legacystore::LegacyStore>>,
    /// 原生渲染后的附加字段（例如给来源 IP 附上画像）：(路径, 响应 JSON) → 是否改动
    pub decorate: std::sync::OnceLock<Arc<dyn Fn(&str, &mut serde_json::Value) -> bool + Send + Sync>>,
}

impl Legacy {
    pub fn new(base: &str, key: Option<String>, actor: &str, _concurrency: usize, timeout_s: u64, clock: Arc<dyn Clock>) -> Legacy {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_s))
            .pool_max_idle_per_host(2)
            .no_proxy()
            .build()
            .expect("http client");
        Legacy {
            client,
            base: base.trim_end_matches('/').to_string(),
            key,
            actor: actor.to_string(),
            poll_sem: Semaphore::new(1),
            light_sem: Semaphore::new(LIGHT_SLOTS),
            heavy_sem: Semaphore::new(HEAVY_SLOTS),
            cache: Cache::new(512),
            stats: Mutex::new(UpstreamStats::default()),
            clock,
            native: std::sync::OnceLock::new(),
            decorate: std::sync::OnceLock::new(),
        }
    }

    pub fn url(&self, key: &str) -> String {
        format!("{}{}", self.base, key)
    }

    /// 唯一的上游出口：方法固定为 GET。
    fn send_get(&self, key: &str) -> reqwest::RequestBuilder {
        let mut rb = self.client.request(reqwest::Method::GET, self.url(key)).header("X-Ops-Actor", &self.actor);
        if let Some(k) = &self.key {
            rb = rb.header("X-Ops-Key", k);
        }
        rb
    }

    /// 直接向上游取一次（不看缓存；受并发上限约束）。
    pub async fn fetch(&self, key: &str) -> Result<Entry, String> {
        self.fetch_lane(key, false).await
    }

    /// `poll = true`：共享轮询器的独占槽、30 s 超时；否则按需槽（轻 / 重分开排队），排队 + 处理总时限 25 s。
    pub async fn fetch_lane(&self, key: &str, poll: bool) -> Result<Entry, String> {
        if poll {
            return self.fetch_inner(key, true).await;
        }
        match tokio::time::timeout(Duration::from_secs(DEMAND_DEADLINE_S), self.fetch_inner(key, false)).await {
            Ok(r) => r,
            Err(_) => {
                let now = self.clock.now();
                self.stats.lock().unwrap().record(false, DEMAND_DEADLINE_S as f64 * 1000.0, now, Some("排队 + 处理超过 25 s".into()), 0);
                Err("旧后端 25 s 内没有返回（排队 + 处理）".into())
            }
        }
    }

    async fn fetch_inner(&self, key: &str, poll: bool) -> Result<Entry, String> {
        if normalize(split_key(key).0, Some(split_key(key).1)).as_deref() != Some(key) {
            return Err("路径不在只读白名单内".into());
        }
        if key.starts_with("/komari/") {
            // 旧后端的 /komari/* 要等一个打开的管理员页面代为执行 RPC，fleetd 改为直接读 Komari。
            return Err("komari/* 不经旧后端".into());
        }
        if let Some(ls) = self.native.get().cloned() {
            let (k, now, t0) = (key.to_string(), self.clock.now(), Instant::now());
            let deco = self.decorate.get().cloned();
            let r = tokio::task::spawn_blocking(move || {
                let _span = crate::cpu::Span::new(format!("render {}", k.split('?').next().unwrap_or(&k)));
                let r = ls.render_cached(&k, now).map(|(st, b)| (st, b.to_string()));
                match (r, deco) {
                    (Some((200, body)), Some(d)) if k.starts_with("/activity") || k.starts_with("/behavior") => {
                        let mut v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                        if d(&k, &mut v) {
                            Some((200, crate::compat::pyjson::dumps(&v)))
                        } else {
                            Some((200, body))
                        }
                    }
                    (r, _) => r,
                }
            })
            .await
            .map_err(|e| e.to_string())?;
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            return match r {
                Some((status, body)) => {
                    self.stats.lock().unwrap().record(status < 500, ms, now, None, body.len());
                    Ok(Entry::new(status, Bytes::from(body), now, ms))
                }
                None => Err("原生旧库不可用（尚未导入？）".into()),
            };
        }
        let sem = if poll { &self.poll_sem } else if is_heavy(key) { &self.heavy_sem } else { &self.light_sem };
        let _permit = sem.acquire().await.map_err(|e| e.to_string())?;
        let t0 = Instant::now();
        let res = async {
            let r = self.send_get(key).timeout(Duration::from_secs(if poll { 30 } else { DEMAND_DEADLINE_S })).send().await.map_err(|e| format!("连接旧后端失败：{}", short_err(&e)))?;
            let status = r.status().as_u16();
            let body: Bytes = r.bytes().await.map_err(|e| format!("读取旧后端响应失败：{}", short_err(&e)))?;
            Ok::<_, String>((status, body))
        }
        .await;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let now = self.clock.now();
        match res {
            Ok((status, body)) => {
                let ok = status < 500;
                self.stats.lock().unwrap().record(ok, ms, now, if ok { None } else { Some(format!("HTTP {status}")) }, body.len());
                Ok(Entry::new(status, body, now, ms))
            }
            Err(e) => {
                self.stats.lock().unwrap().record(false, ms, now, Some(e.clone()), 0);
                Err(e)
            }
        }
    }

    /// 带缓存与请求合并的读取。返回 (结果, 是否命中缓存/合并)。
    pub async fn get(&self, key: &str, max_age: f64) -> (Result<Arc<Entry>, String>, bool) {
        let now = self.clock.now();
        self.cache.get_or_fetch(key, now, max_age, || self.fetch(key)).await
    }

    /// 轮询器用：强制刷新并写入缓存。
    pub async fn refresh(&self, key: &str) -> Result<Arc<Entry>, String> {
        let now = self.clock.now();
        self.cache.get_or_fetch(key, now, -1.0, || self.fetch_lane(key, true)).await.0
    }
}

pub fn short_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "超时".into()
    } else if e.is_connect() {
        "无法连接".into()
    } else {
        let s = e.to_string();
        s.chars().take(160).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist() {
        for p in ["/overview", "/health", "/jobs/abc", "/komari/hosts/u-1/metrics", "/komari/pings/3/results", "/panel/overview", "/public-status"] {
            assert!(path_allowed(p), "{p}");
        }
        for p in ["/", "/sync/hosts", "/sync/pending", "/komari/pending", "/komari/done", "/jobs/a/b", "/fleet/prepare",
                  "/overview/../sync/hosts", "/assets/edit", "/policies/apply", "/api-keys/create", "//overview"] {
            assert!(!path_allowed(p), "{p}");
        }
    }

    #[test]
    fn heavy_lane_classification() {
        assert!(is_heavy("/activity?asset=x&limit=10&seconds=604800"));
        assert!(is_heavy("/activity"));
        assert!(!is_heavy("/node?asset=x"));
        assert!(!is_heavy("/history?asset=x&hours=24"));
        assert!(!is_heavy("/behavior?asset=x&seconds=604800"));
        assert_eq!(LIGHT_SLOTS + HEAVY_SLOTS + 1, 4, "旧后端总并发 ≤ 4");
    }

    #[test]
    fn normalize_sorts_query() {
        assert_eq!(normalize("/history", Some("hours=24&asset=x")).unwrap(), "/history?asset=x&hours=24");
        assert_eq!(normalize("history", Some("asset=x&hours=24")).unwrap(), "/history?asset=x&hours=24");
        assert_eq!(normalize("/overview", None).unwrap(), "/overview");
        assert_eq!(normalize("/overview", Some("")).unwrap(), "/overview");
        assert_eq!(normalize("/activity", Some("limit=1&limit=500")).unwrap(), "/activity?limit=500");
        assert_eq!(normalize("/komari/pings/1/results", Some("uuid=a%20b&hours=24")).unwrap(), "/komari/pings/1/results?hours=24&uuid=a+b");
        assert!(normalize("/sync/hosts", None).is_none());
    }
}
