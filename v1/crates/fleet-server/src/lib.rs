pub mod access;
pub mod agent_api;
pub mod alerts;
pub mod api;
pub mod auth;
pub mod config;
pub mod crypto;
pub mod db;
pub mod entries;
pub mod error;
pub mod health;
pub mod ipintel;
pub mod jobs;
pub mod live;
pub mod names;
pub mod notify;
pub mod policy;
pub mod silence;
pub mod ssh;
pub mod state;
pub mod totp;
pub mod views;

use axum::extract::State;
use axum::routing::get;
use axum::Router;
use state::AppState;
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

pub fn app(st: AppState) -> Router {
    let no_api = || async { error::ApiError::not_found("没有这个接口") };
    let mut r = Router::new()
        .nest("/api/v1", api::router().fallback(no_api))
        .nest("/agent/v1", agent_api::router().fallback(no_api))
        .route("/echo", get(agent_api::echo));
    if let Some(dir) = st.cfg.web_dir.clone().filter(|d| d.join("index.html").exists()) {
        // 前端是单页应用：未知路径回 index.html。/assets/ 下的除外，缺的文件回 404：发布新版后旧页面懒加载的旧 chunk
        // 拿到 index.html 会当脚本解析失败，还会带着下面的一年缓存头留在浏览器里
        let spa = ServeDir::new(&dir).fallback(ServeFile::new(dir.join("index.html")));
        r = r.nest_service("/assets", ServeDir::new(dir.join("assets"))).fallback_service(spa);
    }
    let https = st.cfg.public_url.starts_with("https://");
    r.layer(axum::middleware::from_fn_with_state(https, response_headers))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(st)
}

/// 前端页面的 CSP：只许本站的脚本、样式、图片（图标是 data: URI）、连接；样式放开 'unsafe-inline' 是给 Radix 的内联 style
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; font-src 'self'; \
                   object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// 缓存头与安全头。
/// 缓存：/assets/ 下的文件名带内容哈希，可以永久缓存；index.html 等其余页面每次都要重新验证，否则发布新版后浏览器还拿着旧的入口文件；
/// 接口没自己设 Cache-Control 的一律 no-store（数据里有 IP、令牌提示、审计，不留在浏览器和中间缓存里）。
/// 安全：所有响应带 nosniff、同源才带 Referer、不许被嵌进框架；前端页面（/api、/agent、/echo 以外）再带 CSP；`https`（public_url 是 https）时带 HSTS
async fn response_headers(State(https): State<bool>, req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, REFERRER_POLICY, STRICT_TRANSPORT_SECURITY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS};
    use axum::http::HeaderValue;
    let path = req.uri().path();
    let api = path.starts_with("/api/");
    let page = !api && !path.starts_with("/agent/") && path != "/echo";
    let cache = if path.starts_with("/assets/") {
        Some("public, max-age=31536000, immutable")
    } else if page {
        Some("no-cache")
    } else {
        None
    };
    let mut res = next.run(req).await;
    let ok = res.status().is_success();
    let h = res.headers_mut();
    match cache {
        Some(v) if ok && !h.contains_key(CACHE_CONTROL) => {
            h.insert(CACHE_CONTROL, HeaderValue::from_static(v));
        }
        None if api && !h.contains_key(CACHE_CONTROL) => {
            h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
        _ => {}
    }
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    h.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    if page {
        h.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    }
    if https {
        h.insert(STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000"));
    }
    res
}

/// 后台任务：作业、告警、实时指标、分区维护、小时汇总
pub fn spawn_background(st: &AppState) {
    tokio::spawn(jobs::run_forever(st.clone()));
    tokio::spawn(alerts::run_forever(st.clone()));
    tokio::spawn(live::run_forever(st.clone()));
    tokio::spawn(access::run_forever(st.clone()));
    tokio::spawn(ipintel::run_forever(st.clone()));
    let s = st.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            t.tick().await;
            if let Err(e) = db::maintain_partitions(&s.db, s.cfg.retention.raw_days, s.cfg.retention.access_days, 3).await {
                tracing::error!("分区维护失败：{e:#}");
            }
            match jobs::prune(&s.db).await {
                Ok(p) if p.passwords_cleared + p.jobs_deleted > 0 => {
                    tracing::info!("作业清理：清掉 {} 份引导密码，删除 {} 个旧作业", p.passwords_cleared, p.jobs_deleted)
                }
                Ok(_) => {}
                Err(e) => tracing::error!("作业清理失败：{e:#}"),
            }
            // 来源 IP 画像：不短于学习期，也不短于访问小时汇总的保留期（来源同步首次扫描回看这么久）
            let source_ip_days = (s.cfg.ipintel.learn_days as i64).max(access::HOURLY_DAYS) + 1;
            if let Err(e) = db::cleanup_retention(&s.db, source_ip_days).await {
                tracing::error!("保留期清理失败：{e:#}");
            }
        }
    });
    let s = st.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(std::time::Duration::from_secs(300));
        // 一轮跑得比间隔长时顺延，不连着补跑
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            if let Err(e) = db::rollup_hours(&s.db).await {
                tracing::error!("小时汇总失败：{e:#}");
            }
            // 失败也可能已写进一部分小时：照样作废流量缓存
            s.live.rolled_up();
        }
    });
}
