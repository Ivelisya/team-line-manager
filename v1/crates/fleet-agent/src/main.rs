//! fleet-agent：边缘程序（落地节点 / 线路机 / 探测点共用一个二进制）
//!
//! 用法：fleet-agent [--config PATH] [run|once|render|apply|probe|version] [--desired FILE]
//! `probe`：探测点模式（放在靠近客户的地方测线路机的客户入口，见 probe_point.rs），配置默认 /etc/fleet/probe.toml

mod bridge;
mod collect;
mod config;
mod desired;
mod ping;
mod rtt;
mod host;
mod probe;
mod probe_point;
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
use tokio::task::JoinSet;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

const DEFAULT_CONFIG: &str = "/etc/fleet-agent/agent.toml";
/// 关停时等后台任务自己收尾的上限 / 其后再等进行中的 apply 释放锁的上限
const SHUTDOWN_GRACE: Duration = Duration::from_secs(25);
const APPLY_GRACE: Duration = Duration::from_secs(10);

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
        "fleet-agent {}\n\n用法：fleet-agent [--config PATH] <命令>\n\n命令：\n  run                 常驻运行（默认）\n  once                采集并打印一份上报 JSON（诊断）\n  render --desired F  按期望状态生成 xray.json 并打印\n  apply  --desired F  不经中控，在本机直接应用期望状态（实验室测试）\n  probe               探测点模式：每 30 秒测线路机的客户入口并上报（配置默认 /etc/fleet/probe.toml）\n  version             打印版本\n\n配置默认 {DEFAULT_CONFIG}（也可用环境变量 FLEET_AGENT_CONFIG）。",
        fleet_proto::build_version()
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

/// journald 按行首的 `<N>` 定级（SyslogLevelPrefix 默认开）：不加的话每行都记成 info，`journalctl -p warning` 查不到 WARN / ERROR。
/// `on`：stdout 直连 journald（systemd 单元里运行）；终端里运行不加
struct SyslogPrefix<F> {
    on: bool,
    fmt: F,
}

impl<S, N, F> FormatEvent<S, N> for SyslogPrefix<F>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, mut w: Writer<'_>, ev: &tracing::Event<'_>) -> std::fmt::Result {
        if self.on {
            // syslog 优先级：3 err、4 warning、6 info、7 debug
            let p = match *ev.metadata().level() {
                tracing::Level::ERROR => 3,
                tracing::Level::WARN => 4,
                tracing::Level::INFO => 6,
                _ => 7,
            };
            write!(w, "<{p}>")?;
        }
        self.fmt.format_event(ctx, w, ev)
    }
}

/// stdout 是不是直连 journald：systemd 设的 JOURNAL_STREAM 是那条流的「设备号:inode」，与 stdout 对得上才算
/// （变量会被子进程继承，输出被重定向的进程只看变量会误判）
fn stdout_is_journal() -> bool {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    let Some(v) = std::env::var_os("JOURNAL_STREAM") else { return false };
    let Ok(md) = std::io::stdout().as_fd().try_clone_to_owned().map(std::fs::File::from).and_then(|f| f.metadata()) else { return false };
    v.to_str() == Some(format!("{}:{}", md.dev(), md.ino()).as_str())
}

// 工作线程固定 2 个：默认按核数开，大核数机器上 agent 自己就会占掉单元 TasksMax=128 的一大块
// （阻塞 IO 都在 spawn_blocking 里，实验室单核机一直只有 1 个工作线程）
#[tokio::main(worker_threads = 2)]
async fn main() {
    let journal = stdout_is_journal();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(false)
        .event_format(SyslogPrefix { on: journal, fmt: tracing_subscriber::fmt::format().with_target(false) })
        .init();
    if let Err(e) = real_main().await {
        eprintln!("{}错误：{e:#}", if journal { "<3>" } else { "" });
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let a = parse_args()?;
    match a.cmd.as_str() {
        "help" => usage(),
        "version" => println!("fleet-agent {}", fleet_proto::build_version()),
        "render" => {
            let cfg = load_config(&a, true)?;
            let d = read_desired(&a)?;
            let ctx = xray::render::RenderCtx::of(&cfg);
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
        "probe" => {
            let p = if a.config_given { a.config.clone() } else { PathBuf::from(probe_point::DEFAULT_CONFIG) };
            probe_point::run(&p).await?
        }
        "once" => once(load_config(&a, false)?).await?,
        "run" => run(load_config(&a, false)?).await?,
        other => bail!("未知命令：{other}（用 --help 查看）"),
    }
    Ok(())
}

/// 采集两个主机样本（间隔 1 s），打印一份报告 JSON，不写状态、不上报
async fn once(cfg: Config) -> Result<()> {
    let mut hc = host::HostCollector::new(None, cfg.nic_ifaces.clone());
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut hs = vec![hc.sample()];
    tokio::time::sleep(Duration::from_secs(1)).await;
    hs.push(hc.sample());
    let mut xs = None;
    if cfg.role == Role::Relay {
        let ap = xray::apply::Applier::new(Arc::new(cfg.clone()));
        let up = ap.cli.runtime().await;
        xs = Some(fleet_proto::XrayStatus { running: up.is_ok(), version: ap.cli.version().await.ok(), applied_generation: state::load_applied(&cfg).map(|a| a.generation).unwrap_or(0) });
    }
    let r = Report {
        seq: 0,
        boot_id: host::boot_id(),
        agent_version: fleet_proto::build_version().into(),
        sent_at_ms: util::now_ms(),
        info: Some(host::collect_info(hc.ifaces())),
        host: hs,
        xray: xs,
        ..Default::default()
    };
    println!("{}", serde_json::to_string_pretty(&r)?);
    let tok = std::fs::read_to_string(&cfg.token_file).map(|t| t.trim().len()).unwrap_or(0);
    eprintln!("server_url = {}\ntoken_file = {}（{}）", cfg.server_url, cfg.token_file.display(), if tok > 0 { "可读" } else { "缺失或为空" });
    Ok(())
}

async fn run(cfg: Config) -> Result<()> {
    anyhow::ensure!(!cfg.server_url.is_empty(), "配置缺少 server_url");
    // 明文 http 过公网：令牌与全部遥测可被窃听、劫持。不发，直接退出（实验室用内网 http，照常）
    anyhow::ensure!(
        !server::plain_http_over_public(&cfg.server_url),
        "中控地址 {} 是明文 http:// 且不在内网/回环，拒绝连接：请改用 https://",
        cfg.server_url
    );
    std::fs::create_dir_all(&cfg.data_dir).with_context(|| format!("创建 {}", cfg.data_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&cfg.data_dir, std::fs::Permissions::from_mode(0o700));
    }
    tracing::info!(role = ?cfg.role, machine_id = %cfg.machine_id, version = fleet_proto::build_version(), "fleet-agent 启动");
    let cfg = Arc::new(cfg);
    let sh = state::Shared::new(cfg.clone());
    let (sd_tx, sd_rx) = watch::channel(false);
    // 后台任务全放进 JoinSet：任何一个在收到关停信号之前结束（panic / 意外返回）都当致命错误，退出让 systemd 重启
    let mut set: JoinSet<&'static str> = JoinSet::new();
    spawn_named(&mut set, "host_task", collect::host_task(sh.clone(), sd_rx.clone()));
    let mut applier_handle: Option<Arc<xray::apply::Applier>> = None;
    let client = Arc::new(server::ServerClient::new(&cfg.server_url, cfg.token_file.clone())?);

    if cfg.role == Role::Relay {
        let applier = Arc::new(xray::apply::Applier::new(cfg.clone()));
        applier.ensure_boot().await;
        desired::restore(&sh);
        spawn_named(&mut set, "stats_task", collect::stats_task(sh.clone(), applier.cli.clone(), sd_rx.clone()));
        spawn_named(&mut set, "probe_task", collect::probe_task(sh.clone(), probe::direct_client()?, sd_rx.clone()));
        spawn_named(&mut set, "ping_task", ping::ping_task(sh.clone(), sd_rx.clone()));
        spawn_named(&mut set, "rtt_task", rtt::rtt_task(sh.clone(), client.clone(), sd_rx.clone()));
        spawn_named(&mut set, "desired_task", desired::desired_task(sh.clone(), applier.clone(), client.clone(), sd_rx.clone()));
        applier_handle = Some(applier);
    }

    let reporter = reporter::Reporter::new(sh.clone(), client)?;
    spawn_named(&mut set, "reporter", reporter.run(sd_rx.clone()));

    let mut fatal: Option<String> = None;
    tokio::select! {
        _ = wait_for_signal() => tracing::info!("收到退出信号，落盘缓冲后退出"),
        r = set.join_next() => {
            let m = match r {
                Some(Ok(name)) => format!("后台任务 {name} 意外退出"),
                Some(Err(e)) => format!("后台任务异常终止（panic？）：{e}"),
                None => "没有后台任务".to_string(),
            };
            tracing::error!("{m}，让进程退出以便 systemd 重启");
            fatal = Some(m);
        }
    }
    let _ = sd_tx.send(true);
    // 让各任务自己收尾（上报器落盘缓冲、desired 任务做完手上的 apply）；超时再 abort
    let drained = tokio::time::timeout(SHUTDOWN_GRACE, async { while set.join_next().await.is_some() {} }).await.is_ok();
    if !drained {
        tracing::warn!("{} 秒内仍有后台任务没结束，准备强制终止", SHUTDOWN_GRACE.as_secs());
        if let Some(ap) = &applier_handle {
            if !ap.wait_idle(APPLY_GRACE).await {
                tracing::warn!("等了 {} 秒 apply 仍未结束，强制终止（运行态可能处于中间状态，下次启动会对齐）", APPLY_GRACE.as_secs());
            }
        }
    }
    set.abort_all();
    match fatal {
        Some(m) => bail!(m),
        None => Ok(()),
    }
}

fn spawn_named<F>(set: &mut JoinSet<&'static str>, name: &'static str, f: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    set.spawn(async move {
        f.await;
        name
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 按 main 里的格式各打一条 error / warn / info，返回输出的各行
    fn lines(on: bool) -> Vec<String> {
        let buf = Buf::default();
        let w = buf.clone();
        let sub = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || w.clone())
            .event_format(SyslogPrefix { on, fmt: tracing_subscriber::fmt::format().with_target(false) })
            .finish();
        tracing::subscriber::with_default(sub, || {
            tracing::error!("e");
            tracing::warn!("w");
            tracing::info!("i");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        out.lines().map(String::from).collect()
    }

    #[test]
    fn syslog_priority_prefix_only_under_journald() {
        let on = lines(true);
        assert_eq!(on.iter().map(|l| &l[..3]).collect::<Vec<_>>(), ["<3>", "<4>", "<6>"], "{on:?}");
        assert!(on[1].ends_with(" WARN w"), "{on:?}");
        // 终端里：原样，不带前缀
        let off = lines(false);
        assert!(off.len() == 3 && off.iter().all(|l| !l.starts_with('<')), "{off:?}");
    }
}
