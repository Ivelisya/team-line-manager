use anyhow::{bail, Context};
use fleet_server::{auth, config::Config, crypto::SecretBox, db, jobs, local_admin, state::AppState};
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "用法：
  fleet-server serve    -c <配置>        启动服务（自动迁移数据库）
  fleet-server migrate  -c <配置>        只执行数据库迁移
  fleet-server user-set -c <配置> <用户名>  新建用户或重设密码（密码从标准输入读一行）
  fleet-server totp-reset -c <配置> <用户名>  停用该用户的两步验证（手机与恢复码都丢了时用）
  fleet-server local-admin -c <配置> [--rotate] [--fleetctl-config <路径>]
                                         root 在中控上执行：建或修复中控本机 fleetctl 的全权密钥 central-local，
                                         写进 root 的 fleetctl 配置（默认 /root/.config/fleetctl/config.toml）；
                                         仍有效就不换，--rotate 才换（旧的立即失效）
  fleet-server gen-key  <路径>            生成 32 字节密钥文件（0600）";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn,tower_http=warn".into()))
        .init();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = if args.is_empty() { bail!("{USAGE}") } else { args.remove(0) };
    let mut cfg_path = PathBuf::from("/etc/fleet/fleet.toml");
    let mut rest = vec![];
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "-c" || a == "--config" {
            cfg_path = it.next().map(PathBuf::from).context("-c 后面要跟配置路径")?;
        } else {
            rest.push(a);
        }
    }
    match cmd.as_str() {
        "gen-key" => {
            let p = rest.first().context(USAGE)?;
            if std::path::Path::new(p).exists() {
                bail!("{p} 已存在，不覆盖");
            }
            let mut k = [0u8; 32];
            rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut k);
            std::fs::write(p, k)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))?;
            }
            println!("已生成 {p}");
        }
        "migrate" => {
            let cfg = Config::load(&cfg_path)?;
            let pool = db::connect(&cfg.database_url).await?;
            db::MIGRATOR.run(&pool).await?;
            println!("迁移完成");
        }
        "user-set" => {
            let cfg = Config::load(&cfg_path)?;
            let name = rest.first().context(USAGE)?;
            let mut pw = String::new();
            std::io::stdin().read_line(&mut pw)?;
            let pw = pw.trim_end_matches(['\r', '\n']);
            if pw.chars().count() < 10 {
                bail!("密码至少 10 个字符");
            }
            let pool = db::connect(&cfg.database_url).await?;
            auth::upsert_user(&pool, name, pw).await?;
            println!("已设置用户 {name}（该用户的旧会话已失效，登录失败记录已清空）");
        }
        "totp-reset" => {
            let cfg = Config::load(&cfg_path)?;
            let name = rest.first().context(USAGE)?;
            let pool = db::connect(&cfg.database_url).await?;
            auth::totp_reset(&pool, name).await?;
            println!("已停用用户 {name} 的两步验证（恢复码已作废；登录后可在「设置 → 账户安全」重新开启）");
        }
        "local-admin" => {
            // 全权令牌写进 root 的家目录：只给中控上的 root（fleet 用户跑 fleet-server 时也写不了 /root）
            #[cfg(unix)]
            {
                if unsafe { libc::geteuid() } != 0 {
                    bail!("local-admin 只能由 root 在中控上执行：它把全权 API 密钥写进 root 的 fleetctl 配置");
                }
            }
            let (mut rotate, mut out) = (false, PathBuf::from(local_admin::DEFAULT_CONFIG));
            let mut it = rest.into_iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--rotate" => rotate = true,
                    "--fleetctl-config" => out = it.next().map(PathBuf::from).context("--fleetctl-config 后面要跟路径")?,
                    _ => bail!("{USAGE}"),
                }
            }
            let cfg = Config::load(&cfg_path)?;
            let url = local_admin::local_url(&cfg.listen)?;
            let pool = db::connect(&cfg.database_url).await?;
            println!("{}", local_admin::ensure(&pool, &url, &out, rotate).await?.summary());
        }
        "serve" => {
            let cfg = Config::load(&cfg_path)?;
            let secrets = SecretBox::from_key_file(&cfg.secret_key_file)?;
            let listen: SocketAddr = cfg.listen.parse().context("listen 地址")?;
            // 先占端口再碰库：手动多起一个 serve 在这里就因端口被占退出，不会去清在跑实例的作业租约、删分区
            let l = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("监听 {listen}（已经有一个 fleet-server 在跑？）"))?;
            let pool = db::connect(&cfg.database_url).await?;
            db::MIGRATOR.run(&pool).await.context("数据库迁移")?;
            // 0034 之前的操作记录没有搜索文本：回填（只第一次启动时有活）
            let n = db::backfill_audit_search(&pool).await.context("回填操作记录的搜索文本")?;
            if n > 0 {
                tracing::info!("回填了 {n} 条操作记录的搜索文本");
            }
            fleet_server::meta::sync_org_tz(&pool, &cfg.org_timezone).await?;
            let st = AppState::new(cfg, pool, secrets);
            // 面板里设的访问记录保留期：分区维护、访问汇总的启动补算、上报的接收窗口都按它
            fleet_server::settings::load(&st).await.context("读取访问记录保留期")?;
            db::maintain_partitions(&st.db, st.cfg.retention.raw_days, st.access_retention().raw_days, 3).await?;
            // 停机这段收不到上报：恢复后头两分钟里 agent 的重发还没到，不算它们离线（report_gaps::ReportGaps）
            st.report_gaps.started(&st.db).await.context("读最近的上报时间")?;
            jobs::release_stale_leases(&st).await;
            fleet_server::spawn_background(&st);
            let app = fleet_server::app(st.clone());
            tracing::info!("fleet-server {} 监听 {listen}", fleet_proto::build_version());
            // 开始处理请求之前就装上信号处理：能应答请求时 SIGTERM 一定走优雅退出
            let term = terminated();
            let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
            let mut http = tokio::spawn(
                axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>())
                    .with_graceful_shutdown(async move {
                        let _ = stopped.await;
                    })
                    .into_future(),
            );
            tokio::select! {
                r = &mut http => {
                    r??;
                    bail!("HTTP 服务意外结束");
                }
                _ = term => {}
            }
            // 优雅退出：先不领新作业、等进行中的步骤做完（最多 60 秒），这期间接口照常；再停止接新连接，进行中的请求最多再等 10 秒
            tracing::info!("收到退出信号：不再领新作业，等进行中的步骤做完（最多 60 秒）");
            // 网页终端先断（浏览器收到「中控正在重启」、写完「关闭」审计，最多等 2 秒），之后不再开新的：WS 任务在进程退出时会被直接丢掉
            fleet_server::terminal::shutdown(&st, Duration::from_secs(2)).await;
            let left = jobs::drain(&st, Duration::from_secs(60)).await;
            if left > 0 {
                tracing::warn!("{left} 个作业的这一步没做完：重启后从这一步重做");
            }
            let _ = stop.send(());
            if tokio::time::timeout(Duration::from_secs(10), http).await.is_err() {
                tracing::warn!("还有请求没处理完（如线路机的长轮询），不再等");
            }
            tracing::info!("fleet-server 已退出");
        }
        _ => bail!("{USAGE}"),
    }
    Ok(())
}

/// 退出信号：systemd 停服务发 SIGTERM，前台运行时 Ctrl-C 是 SIGINT。调用时立刻装上处理（之后到的信号不再直接杀掉进程），
/// 返回等信号到来的 future
fn terminated() -> impl std::future::Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT");
    async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
}
