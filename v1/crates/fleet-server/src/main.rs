use anyhow::{bail, Context};
use axum::serve::ListenerExt;
use fleet_server::{auth, config::Config, crypto::SecretBox, db, jobs, local_admin, ops, state::AppState};
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "用法：
  fleet-server serve    -c <配置>        启动服务（自动迁移数据库）
  fleet-server migrate  -c <配置>        只执行数据库迁移
  fleet-server user-set -c <配置> <用户名>  新建用户或重设密码（密码从标准输入读一行）
  fleet-server user-del -c <配置> [--disable] <用户名>  删除用户（--disable 只停用，user-set 重设密码可恢复）；会话作废，不能删最后一个账号
  fleet-server totp-reset -c <配置> <用户名>  停用该用户的两步验证（手机与恢复码都丢了时用）
  fleet-server local-admin -c <配置> [--rotate] [--fleetctl-config <路径>]
                                         root 在中控上执行：建或修复中控本机 fleetctl 的全权密钥 central-local，
                                         写进 root 的 fleetctl 配置（默认 /root/.config/fleetctl/config.toml）；
                                         仍有效就不换，--rotate 才换（旧的立即失效）
  fleet-server backup -c <配置>           root 在中控上执行：做一份备份到 [selfmon] backup_dir（默认 /var/backups/fleet）并轮换
                                         （fleet-backup.timer 每天跑；docs/OPS.md §10）
  fleet-server restore -c <配置> <备份> [--dry-run] [--yes] [--identity <age 私钥>] [--skip-sha256]
                                         root 在中控上执行：从备份恢复库与密钥（docs/OPS.md §10）。先核对 sha256、解密、检查内容与迁移；
                                         --dry-run 只打出将要做的；真正恢复要 --yes，且 fleet-server 已停；先做安全备份，失败全部换回
  fleet-server ipintel-fetch -c <配置> [目录]  以 fleet 用户执行：下载 / 更新 IP 情报离线库（默认 [ipintel] db_dir），
                                         校验和与加载都通过才原子换上（fleet-ipintel-fetch.timer 每月跑）
  fleet-server gen-key  <路径>            生成 32 字节密钥文件（0600）
  fleet-server version                  打印版本（0.1.0+<提交哈希>，同 fleet-agent version）";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 输出到终端才带颜色码；journald 等重定向下不带（否则日志里满是转义序列）
    tracing_subscriber::fmt()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
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
        // 不读配置：部署脚本装完二进制就用它打出版本，与 fleet-agent version 同格式
        "version" | "-V" | "--version" => println!("fleet-server {}", fleet_proto::build_version()),
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
        "user-del" => {
            let cfg = Config::load(&cfg_path)?;
            let disable = rest.iter().any(|a| a == "--disable");
            let name = rest.iter().find(|a| *a != "--disable").context(USAGE)?;
            let pool = db::connect(&cfg.database_url).await?;
            auth::remove_user(&pool, name, disable).await?;
            println!("已{}用户 {name}（它的会话已失效）", if disable { "停用" } else { "删除" });
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
        "backup" => {
            // 取代 deploy/backup/fleet-backup.sh（fleet-backup.service，root）。收到 SIGTERM / Ctrl-C：子进程被杀、临时目录删掉，以失败退出
            require_root("backup", "pg_dump 经 runuser 以 postgres 身份连库，打包 /etc/fleet 里只有 root 读得了的密钥")?;
            if !rest.is_empty() {
                bail!("{USAGE}");
            }
            let cfg = Config::load(&cfg_path)?;
            let (db, _) = ops::db_and_user(&cfg.database_url)?;
            let plan = ops::backup::Plan::system(cfg.selfmon.backup_dir.clone(), db);
            let done = tokio::select! {
                r = ops::backup::run(&plan, time::OffsetDateTime::now_utc()) => r?,
                s = ops::stop_signal() => bail!("收到 {s}，中止：这次没有备份（临时目录已删，旧备份不动）"),
            };
            println!("{}", done.summary());
        }
        "restore" => {
            require_root("restore", "要以 postgres 身份换库、以 fleet / root 属主放回密钥")?;
            let mut opts = ops::restore::Opts::default();
            let (mut dry, mut yes, mut archive) = (false, false, None);
            let mut it = rest.into_iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--dry-run" => dry = true,
                    "--yes" => yes = true,
                    "--skip-sha256" => opts.skip_sha256 = true,
                    "--identity" => opts.identity = Some(it.next().map(PathBuf::from).context("--identity 后面要跟 age 私钥文件")?),
                    s if s.starts_with('-') => bail!("不认识的参数 {s}\n{USAGE}"),
                    _ if archive.is_none() => archive = Some(PathBuf::from(a)),
                    _ => bail!("{USAGE}"),
                }
            }
            opts.archive = archive.context(USAGE)?;
            let cfg = Config::load(&cfg_path)?;
            let env = ops::restore::Env::system(&cfg)?;
            let now = time::OffsetDateTime::now_utc();
            // 准备阶段被 Ctrl-C：丢弃任务，暂存目录（里面有解开的密钥）由守卫删掉
            let p = tokio::select! {
                r = ops::restore::prepare(&opts, &env, now) => r?,
                s = ops::stop_signal() => bail!("收到 {s}，中止：什么都没改，暂存目录已删"),
            };
            print!("{}", p.describe(&env));
            if dry {
                println!("（--dry-run：到此为止，什么都没改；暂存目录已删）");
                return Ok(());
            }
            if !yes {
                bail!("没有加 --yes：以上是将要做的，什么都还没改。确认后先 systemctl stop fleet-server，再加 --yes 重跑");
            }
            let r = ops::restore::apply(&p, &env, now).await?;
            println!("== 恢复完成：库与密钥是备份时的了（恢复前的安全备份 {}）", r.safety_backup.display());
            println!("剩下的步骤要人做（docs/OPS.md §10）：\n{}", ops::restore::manual_steps(&r, &p));
        }
        "ipintel-fetch" => {
            // 取代 deploy/ipintel/fetch-dbs.sh（fleet-ipintel-fetch.service，User=fleet）
            #[cfg(unix)]
            if unsafe { libc::geteuid() } == 0 {
                bail!("不要以 root 跑 ipintel-fetch：换上的库归 root、fleet-server（fleet 用户）读不了。用 systemctl start fleet-ipintel-fetch，或 runuser -u fleet -- /opt/fleet/fleet-server ipintel-fetch -c /etc/fleet/fleet.toml");
            }
            if rest.len() > 1 {
                bail!("{USAGE}");
            }
            let cfg = Config::load(&cfg_path)?;
            let dest = rest.first().map(PathBuf::from).unwrap_or_else(|| cfg.ipintel.db_dir.clone());
            let (src, lim) = (ops::ipintel_fetch::Sources::default(), ops::ipintel_fetch::Limits::default());
            let rep = tokio::select! {
                r = ops::ipintel_fetch::run(&dest, &src, &lim, time::OffsetDateTime::now_utc()) => r?,
                s = ops::stop_signal() => bail!("收到 {s}，中止：已换上的库不动，没下完的删掉"),
            };
            if !rep.failed.is_empty() {
                bail!("{} 项没更新（旧文件保留，其余照常）：\n  {}", rep.failed.len(), rep.failed.join("\n  "));
            }
            println!(
                "完成：更新 {}，已是最新 {}{}",
                rep.updated.len(),
                rep.current.len(),
                if rep.unverified.is_empty() { String::new() } else { format!("（{} 的来源不公布校验和，只做了格式检查）", rep.unverified.join("、")) }
            );
        }
        "serve" => {
            let cfg = Config::load(&cfg_path)?;
            let secrets = SecretBox::from_key_file(&cfg.secret_key_file)?;
            let listen: SocketAddr = cfg.listen.parse().context("listen 地址")?;
            // 先占端口再碰库：手动多起一个 serve 在这里就因端口被占退出，不会去清在跑实例的作业租约、删分区
            let l = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("监听 {listen}（已经有一个 fleet-server 在跑？）"))?;
            let pool = db::connect(&cfg.database_url).await?;
            db::MIGRATOR.run(&pool).await.context("数据库迁移")?;
            fleet_server::meta::sync_org_tz(&pool, &cfg.org_timezone).await?;
            // 期望状态的生成逻辑改过（DESIRED_FORMAT 升了）：线路机代数统一 +1，重连后重拉
            let n = db::sync_desired_format(&pool).await.context("核对期望状态格式版本")?;
            if n > 0 {
                tracing::info!("期望状态格式版本变为 {}：{n} 台线路机代数 +1，重拉期望状态", db::DESIRED_FORMAT);
            }
            let st = AppState::new(cfg, pool, secrets);
            // 面板里设的访问记录保留期：分区维护、访问汇总的启动补算、上报的接收窗口都按它
            fleet_server::settings::load(&st).await.context("读取访问记录保留期")?;
            db::maintain_partitions(&st.db, st.cfg.retention.raw_days, st.access_retention().raw_days, 3).await?;
            // 停机这段收不到上报：恢复后头两分钟里 agent 的重发还没到，不算它们离线（report_gaps::ReportGaps）
            st.report_gaps.started(&st.db).await.context("读最近的上报时间")?;
            jobs::release_stale_leases(&st).await;
            // 探测点安装的任务在进程内存里：上次进程没跑完的现在已经没人在跑，标为失败；只在启动时做一次
            if let Err(e) = fleet_server::probe_points::install::mark_interrupted(&st).await {
                tracing::warn!("把中断的探测点安装标为失败：{e:#}");
            }
            fleet_server::spawn_background(&st);
            let app = fleet_server::app(st.clone());
            tracing::info!("fleet-server {} 监听 {listen}", fleet_proto::build_version());
            // 开始处理请求之前就装上信号处理：能应答请求时 SIGTERM 一定走优雅退出
            let term = terminated();
            let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
            // 关 Nagle：Caddy 复用到这里的长连接，gzip 响应分几次小写，不关的话每个响应尾巴等对端延迟 ACK 约 40ms
            let l = l.tap_io(|tcp| {
                if let Err(e) = tcp.set_nodelay(true) {
                    tracing::warn!("设置 TCP_NODELAY 失败：{e}");
                }
            });
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
            // 先通知长连接（线路机的期望状态长轮询、实时流 WebSocket）收尾，再停止接新连接：客户端马上重连到新进程，不用干等下面的 10 秒
            st.shutdown.send_replace(true);
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

/// 只许 root 执行的子命令
fn require_root(cmd: &str, why: &str) -> anyhow::Result<()> {
    #[cfg(unix)]
    if unsafe { libc::geteuid() } != 0 {
        bail!("{cmd} 只能由 root 在中控上执行：{why}");
    }
    let _ = (cmd, why);
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
