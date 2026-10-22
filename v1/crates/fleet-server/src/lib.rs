pub mod access;
pub mod agent_api;
pub mod alerts;
pub mod api;
pub mod auth;
pub mod config;
pub mod country;
pub mod crypto;
pub mod db;
pub mod entries;
pub mod error;
pub mod health;
pub mod ipintel;
pub mod jobs;
pub mod live;
pub mod local_admin;
pub mod meta;
pub mod names;
pub mod notify;
pub mod policy;
pub mod pty;
pub mod selfmon;
pub mod settings;
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

/// 后台任务：作业、告警、实时指标、访问汇总、来源画像、通知发送、分区维护、小时汇总。
/// 都在 `supervise` 下跑：panic 或意外返回时记 error、10 秒后原地重启（这些循环的状态在库里，内存里的去抖计数、缓存重启后重新累计）。
/// 作业引擎例外：一退出就结束整个进程，交给 systemd 重启（单元里 Restart=always），重启时 main 清掉旧租约、作业从各自这一步重做。
/// （清旧租约已挪到 main、本进程还在推进的作业不会被再领——jobs::Running——原地重启其实也不会两个执行者同时推进）
pub fn spawn_background(st: &AppState) {
    supervise(st, "作业引擎", true, jobs::run_forever);
    supervise(st, "告警", false, alerts::run_forever);
    supervise(st, "实时指标", false, live::run_forever);
    supervise(st, "访问汇总", false, access::run_forever);
    // 配置里关掉时它直接返回，不算意外退出，不起
    if st.ipintel.enabled {
        supervise(st, "来源画像", false, ipintel::run_forever);
    } else {
        tracing::info!("ipintel 已在配置里关闭");
    }
    notify::start(st);
    supervise(st, "分区与保留期维护", false, housekeeping);
    supervise(st, "小时汇总", false, rollup_loop);
    supervise(st, "会话撤销通知", false, revoked_listener);
}

/// 命令行（user-set 重设密码）在别的进程里删会话：经 NOTIFY 转来，实时流的连接马上复核。
/// 断线时 recv 自己重连；连不上或 listen 失败才返回，由 supervise 10 秒后重启
async fn revoked_listener(s: AppState) {
    match sqlx::postgres::PgListener::connect_with(&s.db).await {
        Ok(mut l) => match l.listen(auth::REVOKED_CHANNEL).await {
            Ok(()) => {
                while l.recv().await.is_ok() {
                    s.revoked();
                }
            }
            Err(e) => tracing::warn!("监听会话撤销通知失败：{e}"),
        },
        Err(e) => tracing::warn!("监听会话撤销通知失败：{e}"),
    }
}

/// 跑一个不该结束的后台循环：panic 或返回了记 error，10 秒后重启；`exit` 为真时改为结束整个进程，交给 systemd 重启
pub(crate) fn supervise<F, Fut>(st: &AppState, name: &'static str, exit: bool, f: F)
where
    F: Fn(AppState) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let st = st.clone();
    tokio::spawn(async move {
        loop {
            let why = match tokio::spawn(f(st.clone())).await {
                Ok(()) => "意外退出".to_string(),
                Err(e) if e.is_panic() => format!("panic（{e}）"),
                // 运行时正在关（进程退出、测试结束）
                Err(_) => return,
            };
            if exit {
                tracing::error!("后台任务「{name}」{why}，结束进程交给 systemd 重启");
                std::process::exit(1);
            }
            tracing::error!("后台任务「{name}」{why}，10 秒后重启");
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });
}

/// 每小时：组织时区偏移、分区维护（原始访问记录按面板里设的保留期，见 settings.rs）、作业清理、保留期清理。
/// 访问汇总与来源画像的保留期清理在访问汇总的循环里（`access::prune`）
async fn housekeeping(s: AppState) {
    let mut t = tokio::time::interval(std::time::Duration::from_secs(3600));
    loop {
        t.tick().await;
        // 组织时区的 UTC 偏移（夏令时）
        if let Err(e) = meta::sync_org_tz(&s.db, &s.cfg.org_timezone).await {
            tracing::error!("刷新组织时区失败：{e:#}");
        }
        if let Err(e) = db::maintain_partitions(&s.db, s.cfg.retention.raw_days, s.access_retention().raw_days, 3).await {
            tracing::error!("分区维护失败：{e:#}");
        }
        match jobs::prune(&s.db).await {
            Ok(p) if p.passwords_cleared + p.jobs_deleted > 0 => {
                tracing::info!("作业清理：清掉 {} 份引导密码，删除 {} 个旧作业", p.passwords_cleared, p.jobs_deleted)
            }
            Ok(_) => {}
            Err(e) => tracing::error!("作业清理失败：{e:#}"),
        }
        if let Err(e) = db::cleanup_retention(&s.db).await {
            tracing::error!("保留期清理失败：{e:#}");
        }
    }
}

/// 每 5 分钟：小时汇总
async fn rollup_loop(s: AppState) {
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
}
