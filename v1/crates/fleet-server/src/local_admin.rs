//! 中控本机的全权 fleetctl：`fleet-server local-admin` 建或修复名为 central-local 的 API 密钥（全部 scope、不过期），
//! 把中控本机地址和它写进 root 的 fleetctl 配置。用户经常换设备、笔记本上不放令牌：ssh 进中控（root）后，
//! agent 直接用本机 fleetctl 做面板能做的一切，`fleetctl ssh` 再登录任意机器 / 软路由。
//!
//! 部署（install.sh、lab/deploy-server.sh）每次都跑：配置里的令牌还对得上一把有效、权限齐全的 central-local 就不换，
//! 不让正在干活的 agent 半路失效；`--rotate` 才换。新建时撤销其它有效的 central-local（轮换时就是旧的那把）。
//! 同别的密钥：库里只存 sha256 与前缀，令牌只进那个 0600 的配置文件，不打印。审计的操作者记 cli（同 totp-reset）。

use crate::auth::{ALL_SCOPES, REVOKED_CHANNEL};
use crate::crypto::sha256;
use crate::db::{audit, new_id};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// 中控本机 fleetctl 用的密钥名。留给 local-admin：网页与接口不能建同名的，审计里的 apikey:central-local 只可能是它
pub const KEY_NAME: &str = "central-local";
/// root 的 fleetctl 配置（fleetctl 在 Linux 上读 ~/.config/fleetctl/config.toml）
pub const DEFAULT_CONFIG: &str = "/root/.config/fleetctl/config.toml";

/// 同 fleetctl 的配置文件（fleetctl config.rs 的 FileCfg）
#[derive(Default, Serialize, Deserialize)]
struct FleetctlCfg {
    url: Option<String>,
    token: Option<String>,
}

/// fleetctl 连中控本机的地址：fleet.toml 的 listen 是 0.0.0.0 / [::] 时换成同族的回环地址，其余照写（只听某个内网地址时回环连不上）
pub fn local_url(listen: &str) -> anyhow::Result<String> {
    let mut a: SocketAddr = listen.parse().with_context(|| format!("fleet.toml 的 listen「{listen}」不是「地址:端口」"))?;
    if a.ip().is_unspecified() {
        a.set_ip(if a.is_ipv4() { Ipv4Addr::LOCALHOST.into() } else { Ipv6Addr::LOCALHOST.into() });
    }
    Ok(format!("http://{a}"))
}

pub struct Outcome {
    /// 新建了（第一次、配置里的令牌失效了，或 --rotate）；false = 原来那把仍有效，没换
    pub created: bool,
    pub prefix: String,
    /// 新建时撤销的旧 central-local 把数
    pub revoked: usize,
    pub url: String,
    pub config: PathBuf,
}

impl Outcome {
    /// 命令行输出的一句话（不含令牌）
    pub fn summary(&self) -> String {
        let at = format!("fleetctl 配置 {}（中控 {}）", self.config.display(), self.url);
        if self.created {
            let old = if self.revoked > 0 { format!("，撤销了旧的 {} 把", self.revoked) } else { String::new() };
            format!("已新建 API 密钥 {KEY_NAME}（前缀 {}，权限 {}，不过期{old}），写入 {at}", self.prefix, ALL_SCOPES.join(","))
        } else {
            format!("API 密钥 {KEY_NAME}（前缀 {}）仍有效，没有更换；{at}", self.prefix)
        }
    }
}

/// 建或修复 central-local，写 `config`（root 的 fleetctl 配置）。`url` 是 fleetctl 连中控本机的地址（`local_url`）
pub async fn ensure(db: &PgPool, url: &str, config: &Path, rotate: bool) -> anyhow::Result<Outcome> {
    let all: Vec<String> = ALL_SCOPES.iter().map(|s| s.to_string()).collect();
    let cur = read_cfg(config)?;
    if let (false, Some(tok)) = (rotate, cur.token.as_deref()) {
        // 权限要齐：以后加了新的 scope，部署时这里对不上，换一把全的
        let prefix: Option<String> = sqlx::query_scalar(
            "select prefix from api_key where key_hash = $1 and name = $2 and revoked_at is null
               and (expires_at is null or expires_at > now()) and scopes @> $3",
        )
        .bind(sha256(tok.as_bytes()))
        .bind(KEY_NAME)
        .bind(&all)
        .fetch_optional(db)
        .await?;
        if let Some(prefix) = prefix {
            // listen 的端口改过：照原令牌改写地址
            if cur.url.as_deref() != Some(url) {
                write_cfg(config, url, tok)?;
            }
            return Ok(Outcome { created: false, prefix, revoked: 0, url: url.into(), config: config.into() });
        }
    }
    let (prefix, token) = crate::api::keys::new_token();
    let id = new_id();
    let mut tx = db.begin().await?;
    let old: Vec<(Uuid, String)> = sqlx::query_as("update api_key set revoked_at = now() where name = $1 and revoked_at is null returning id, prefix")
        .bind(KEY_NAME)
        .fetch_all(&mut *tx)
        .await?;
    for (oid, p) in &old {
        audit(&mut tx, "cli", "apikey.revoke", "api_key", Some(*oid), json!({"name": KEY_NAME, "prefix": p, "via": "local-admin"})).await?;
    }
    sqlx::query("insert into api_key (id, name, prefix, key_hash, scopes) values ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(KEY_NAME)
        .bind(&prefix)
        .bind(sha256(token.as_bytes()))
        .bind(&all)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, "cli", "apikey.create", "api_key", Some(id), json!({"name": KEY_NAME, "prefix": prefix, "scopes": all, "expires_days": null, "via": "local-admin", "rotate": rotate}))
        .await?;
    tx.commit().await?;
    if !old.is_empty() {
        // 跑着的中控里用旧密钥连着的实时流马上断开（同 user-set）
        sqlx::query("select pg_notify($1, '')").bind(REVOKED_CHANNEL).execute(db).await?;
    }
    // 先提交再写文件：写失败时库里多一把没人拿着的密钥，下次运行发现配置不对，撤销它再建
    write_cfg(config, url, &token)?;
    Ok(Outcome { created: true, prefix, revoked: old.len(), url: url.into(), config: config.into() })
}

/// 现有的 fleetctl 配置；没有或解析不了算空（照常新建，整份重写）
fn read_cfg(path: &Path) -> anyhow::Result<FleetctlCfg> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(toml::from_str(&s).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FleetctlCfg::default()),
        Err(e) => Err(e).with_context(|| format!("读取 {}", path.display())),
    }
}

/// 写 0600 的配置（含令牌）：同目录的临时文件写好再改名，不会留下半个文件；目录不存在就建（0700）
fn write_cfg(path: &Path, url: &str, token: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir).with_context(|| format!("创建目录 {}", dir.display()))?;
    let body = toml::to_string(&FleetctlCfg { url: Some(url.into()), token: Some(token.into()) })?;
    let tmp = dir.join(format!(".fleetctl-config.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let r = o.open(&tmp).and_then(|mut f| f.write_all(body.as_bytes()).and_then(|_| f.sync_all())).and_then(|_| std::fs::rename(&tmp, path));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.with_context(|| format!("写入 {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_url_uses_loopback_for_wildcard() {
        assert_eq!(local_url("127.0.0.1:8080").unwrap(), "http://127.0.0.1:8080");
        assert_eq!(local_url("0.0.0.0:8080").unwrap(), "http://127.0.0.1:8080");
        assert_eq!(local_url("[::]:9000").unwrap(), "http://[::1]:9000");
        // 只听某个地址：回环连不上，照写
        assert_eq!(local_url("192.168.31.60:8080").unwrap(), "http://192.168.31.60:8080");
        assert!(local_url("localhost:8080").is_err());
    }

    #[test]
    fn config_matches_fleetctl_format() {
        let d = std::env::temp_dir().join(format!("fleet-local-admin-{}", Uuid::new_v4().simple()));
        let p = d.join("fleetctl/config.toml");
        write_cfg(&p, "http://127.0.0.1:8080", "flk_aa_bb").unwrap();
        let c = read_cfg(&p).unwrap();
        assert_eq!((c.url.as_deref(), c.token.as_deref()), (Some("http://127.0.0.1:8080"), Some("flk_aa_bb")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!((mode(&p), mode(p.parent().unwrap())), (0o600, 0o700));
        }
        // 解析不了的旧文件算空，不报错（照常新建、整份重写）
        std::fs::write(&p, "这不是 toml =").unwrap();
        assert!(read_cfg(&p).unwrap().token.is_none());
        assert!(read_cfg(&d.join("nope.toml")).unwrap().token.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
