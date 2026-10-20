//! fleet-agent：边缘程序（落地节点 / 线路机共用一个二进制）
//!
//! 用法：fleet-agent [--config PATH] [run|once|render|apply|version] [--desired FILE]

mod collect;
mod config;
mod desired;
mod host;
mod probe;
mod queue;
mod reporter;
mod server;
mod state;
mod util;
mod xray;

use anyhow::{bail, Context, Result};
use config::{Config, Role};
use fleet_proto::{DesiredState, Report};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const DEFAULT_CONFIG: &str = "/etc/fleet-agent/agent.toml";

struct Args {
    config: PathBuf,
    config_given: bool,
    cmd: String,
    desired: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args { config: std::env::var("FLEET_AGENT_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG.into()).into(), config_given: false, cmd: String::new(), desired: None };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--config" | "-c" => {
                a.config = it.next().context("--config 需要路径")?.into();
                a.config_given = true;
            }
            "--desired" => a.desired = Some(it.next().context("--desired 需要路径")?.into()),
            "-h" | "--help" => a.cmd = "help".into(),
            "-V" | "--version" => a.cmd = "version".into(),
            s if !s.starts_with('-') && a.cmd.is_empty() => a.cmd = s.into(),
            s => bail!("未知参数：{s}"),
        }
    }
    if a.cmd.is_empty() {
        a.cmd = "run".into();
    }
    Ok(a)
}

fn usage() {
    println!(
        "fleet-agent {}\n\n用法：fleet-agent [--config PATH] <命令>\n\n命令：\n  run                 常驻运行（默认）\n  once                采集并打印一份上报 JSON（诊断）\n  render --desired F  按期望状态生成 xray.json 并打印\n  apply  --desired F  不经中控，在本机直接应用期望状态（实验室测试）\n  version             打印版本\n\n配置默认 {DEFAULT_CONFIG}（也可用环境变量 FLEET_AGENT_CONFIG）。",
        env!("CARGO_PKG_VERSION")
    );
}

fn load_config(a: &Args, local_only: bool) -> Result<Config> {
    if local_only && !a.config_given && !a.config.exists() {
        return Ok(Config::relay_default());
    }
    Config::load(&a.config)
}

fn read_desired(a: &Args) -> Result<DesiredState> {
    let p = a.desired.as_ref().context("需要 --desired <文件>")?;
    let s = std::fs::read_to_string(p).with_context(|| format!("读取 {}", p.display()))?;
    serde_json::from_str(&s).with_context(|| format!("解析 {}", p.display()))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .with_ansi(false)
        .init();
    if let Err(e) = real_main().await {
        eprintln!("错误：{e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let a = parse_args()?;
    match a.cmd.as_str() {
        "help" => usage(),
        "version" => println!("fleet-agent {}", env!("CARGO_PKG_VERSION")),
        "render" => {
            let cfg = load_config(&a, true)?;
            let d = read_desired(&a)?;
            let ctx = xray::render::RenderCtx { access_log: cfg.access_log.to_string_lossy().into(), api_addr: cfg.xray_api.clone() };
            println!("{}", serde_json::to_string_pretty(&xray::render::render(&d, &ctx)?.full)?);
        }
        "apply" => {
            let cfg = Arc::new(load_config(&a, true)?);
            let d = read_desired(&a)?;
            std::fs::create_dir_all(&cfg.data_dir)?;
            let ap = xray::apply::Applier::new(cfg.clone());
            let rep = ap.apply(&d).await;
            if rep.ok {
                state::save_applied(&cfg, &d);
            }
            println!("{}", serde_json::to_string_pretty(&rep)?);
            if !rep.ok {
                std::process::exit(2);
            }
        }
        "once" => once(load_config(&a, false)?).await?,
        "run" => run(load_config(&a, false)?).await?,
        other => bail!("未知命令：{other}（用 --help 查看）"),
    }
    Ok(())
}

/// 采集两个主机样本（间隔 1 s），打印一份报告 JSON，不写状态、不上报
async fn once(cfg: Config) -> Result<()> {
    let mut hc = host::HostCollector::new();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut hs = vec![hc.sample()];
    tokio::time::sleep(Duration::from_secs(1)).await;
    hs.push(hc.sample());
    let mut xs = None;
    if cfg.role == Role::Relay {
        let ap = xray::apply::Applier::new(Arc::new(cfg.clone()));
        let up = ap.cli.runtime().await;
        xs = Some(fleet_proto::XrayStatus { running: up.is_ok(), version: ap.cli.version().await.ok(), started_at_ms: None, applied_generation: state::load_applied(&cfg).map(|a| a.generation).unwrap_or(0) });
    }
    let r = Report {
        seq: 0,
        boot_id: host::boot_id(),
        agent_version: env!("CARGO_PKG_VERSION").into(),
        sent_at_ms: util::now_ms(),
        info: Some(host::collect_info()),
        host: hs,
        entries: vec![],
        probes: vec![],
        access: vec![],
        xray: xs,
    };
    println!("{}", serde_json::to_string_pretty(&r)?);
    let tok = std::fs::read_to_string(&cfg.token_file).map(|t| t.trim().len()).unwrap_or(0);
    eprintln!("server_url = {}\ntoken_file = {}（{}）", cfg.server_url, cfg.token_file.display(), if tok > 0 { "可读" } else { "缺失或为空" });
    Ok(())
}

async fn run(cfg: Config) -> Result<()> {
    anyhow::ensure!(!cfg.server_url.is_empty(), "配置缺少 server_url");
    std::fs::create_dir_all(&cfg.data_dir).with_context(|| format!("创建 {}", cfg.data_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&cfg.data_dir, std::fs::Permissions::from_mode(0o700));
    }
    tracing::info!(role = ?cfg.role, machine_id = %cfg.machine_id, version = env!("CARGO_PKG_VERSION"), "fleet-agent 启动");
    let cfg = Arc::new(cfg);
    let sh = state::Shared::new(cfg.clone());
    let (sd_tx, sd_rx) = watch::channel(false);
    let mut tasks = vec![tokio::spawn(collect::host_task(sh.clone(), sd_rx.clone()))];

    if cfg.role == Role::Relay {
        let applier = Arc::new(xray::apply::Applier::new(cfg.clone()));
        applier.ensure_boot().await;
        desired::restore(&sh);
        tasks.push(tokio::spawn(collect::stats_task(sh.clone(), applier.cli.clone(), sd_rx.clone())));
        tasks.push(tokio::spawn(collect::probe_task(sh.clone(), sd_rx.clone())));
        let client = server::ServerClient::new(&cfg.server_url, cfg.token_file.clone(), cfg.ca_file.as_ref())?;
        tasks.push(tokio::spawn(desired::desired_task(sh.clone(), applier, client, sd_rx.clone())));
    }

    let client = server::ServerClient::new(&cfg.server_url, cfg.token_file.clone(), cfg.ca_file.as_ref())?;
    let reporter = reporter::Reporter::new(sh.clone(), client)?;
    let rep_task = tokio::spawn(reporter.run(sd_rx.clone()));

    wait_for_signal().await;
    tracing::info!("收到退出信号，落盘缓冲后退出");
    let _ = sd_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(10), rep_task).await;
    for t in tasks {
        t.abort();
    }
    Ok(())
}

async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}
