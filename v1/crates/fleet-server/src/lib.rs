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
pub mod jobs;
pub mod names;
pub mod notify;
pub mod ssh;
pub mod state;
pub mod views;

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
        // 前端是单页应用：未知路径回 index.html
        let spa = ServeDir::new(&dir).fallback(ServeFile::new(dir.join("index.html")));
        r = r.fallback_service(spa);
    }
    r.layer(CompressionLayer::new()).layer(TraceLayer::new_for_http()).with_state(st)
}

/// 后台任务：作业、告警、分区维护、小时汇总
pub fn spawn_background(st: &AppState) {
    tokio::spawn(jobs::run_forever(st.clone()));
    tokio::spawn(alerts::run_forever(st.clone()));
    let s = st.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            t.tick().await;
            if let Err(e) = db::maintain_partitions(&s.db, s.cfg.retention.raw_days, s.cfg.retention.access_days, 3).await {
                tracing::error!("分区维护失败：{e:#}");
            }
            if let Err(e) = db::cleanup_retention(&s.db).await {
                tracing::error!("保留期清理失败：{e:#}");
            }
        }
    });
    let s = st.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            t.tick().await;
            if let Err(e) = db::rollup_hours(&s.db).await {
                tracing::error!("小时汇总失败：{e:#}");
            }
        }
    });
}
