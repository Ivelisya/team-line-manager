use anyhow::{bail, Context};
use fleet_server::{auth, config::Config, crypto::SecretBox, db, state::AppState};
use std::net::SocketAddr;
use std::path::PathBuf;

const USAGE: &str = "用法：
  fleet-server serve    -c <配置>        启动服务（自动迁移数据库）
  fleet-server migrate  -c <配置>        只执行数据库迁移
  fleet-server user-set -c <配置> <用户名>  新建用户或重设密码（密码从标准输入读一行）
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
        "serve" => {
            let cfg = Config::load(&cfg_path)?;
            let secrets = SecretBox::from_key_file(&cfg.secret_key_file)?;
            let pool = db::connect(&cfg.database_url).await?;
            db::MIGRATOR.run(&pool).await.context("数据库迁移")?;
            db::maintain_partitions(&pool, cfg.retention.raw_days, cfg.retention.access_days, 3).await?;
            let listen: SocketAddr = cfg.listen.parse().context("listen 地址")?;
            let st = AppState::new(cfg, pool, secrets);
            fleet_server::spawn_background(&st);
            let app = fleet_server::app(st);
            let l = tokio::net::TcpListener::bind(listen).await?;
            tracing::info!("fleet-server {} 监听 {listen}", fleet_proto::build_version());
            axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
        }
        _ => bail!("{USAGE}"),
    }
    Ok(())
}
