//! 测试服专用：独立运行接收模块（a395 验收用）。生产由 fleetd 挂载 `api::ingest_routes` / `api::read_routes`。
//! ingest-testd serve --db ./ingest.db --listen 127.0.0.1:25910
//! ingest-testd agent-token issue <host_id> --db ./ingest.db
use fleetd_ingest::{api, Ingest, IngestCfg};
use std::path::PathBuf;

fn main() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_env("INGEST_LOG").unwrap_or_else(|_| "info".into())).with_ansi(false).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut db = PathBuf::from("./ingest.db");
    let mut listen = "127.0.0.1:25910".to_string();
    let mut rest = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--db" => db = PathBuf::from(it.next().expect("--db")),
            "--listen" => listen = it.next().expect("--listen").clone(),
            _ => rest.push(a.clone()),
        }
    }
    let cfg = IngestCfg { db_path: db, ..Default::default() };
    match rest.first().map(|s| s.as_str()) {
        Some("agent-token") => std::process::exit(fleetd_ingest::tokens::cli(cfg, &rest[1..])),
        Some("serve") => {}
        _ => {
            eprintln!("用法：ingest-testd serve|agent-token … [--db F] [--listen A]");
            std::process::exit(2)
        }
    }
    assert!(listen.starts_with("127.0.0.1:"), "测试服只监听回环");
    let ing = Ingest::open(cfg).unwrap_or_else(|e| panic!("{e}"));
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    rt.block_on(async move {
        ing.spawn_maintenance();
        let app: axum::Router = api::ingest_routes(ing.clone()).merge(api::read_routes(ing.clone()));
        let l = tokio::net::TcpListener::bind(&listen).await.unwrap();
        tracing::info!(%listen, "ingest-testd listening");
        axum::serve(l, app)
            .with_graceful_shutdown(async {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
                tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
            })
            .await
            .unwrap();
    });
}
