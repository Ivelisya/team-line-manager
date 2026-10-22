//! 软路由（OpenWrt / iStoreOS + OpenClash，mihomo 内核）纳入面板：记录、接入、隧道、采集、告警、一键部署。
//!
//! 连接：
//! - 路由 → 中控：路由上新增的 /etc/init.d/fleet-tunnel（procd，与原有的 router-tunnel 并存）用
//!   `ssh -N -R 127.0.0.1:<tunnel_port>:127.0.0.1:22 fleet-tunnel@<中控>` 连中控 sshd。账号 fleet-tunnel 只认
//!   AuthorizedKeysCommand（/usr/local/lib/fleet/tunnel-keys）给出的公钥，每把只许在 127.0.0.1:<自己的端口> 监听，不能开会话。
//!   公钥清单是中控写的 `[routers] tunnel_keys_file`（[`sync_files`] 由库整份原子重写）。
//! - 隧道密钥由中控生成：私钥只写进一次性的接入脚本（或一键部署时经 SSH 直接送过去），不落库；重新接入换一对，旧的立刻从清单里撤掉
//!   （已经连着的旧连接 sshd 不会断，直到它自己断开；端口分配因此跳过正在监听的端口，见 [`alloc_port`]）。
//! - 中控 → 路由：`ssh -p <tunnel_port> root@127.0.0.1`，用中控现有的 SSH 密钥（接入脚本把它的公钥加进路由的 dropbear authorized_keys），
//!   主机公钥按 router.host_key 严格核对（HostKeyAlias=router-<id>，known_hosts 由库整份重写）。第一次经隧道连上时记录
//!   （只有拿到那把一次性隧道私钥的人能占住这个端口，经隧道的首次信任可以接受），一键部署在登录那一刻记录；重新生成接入命令时清掉重记。
//! - 接入只新增、采集只读：不动路由的 root 密码、dropbear / LuCI 监听、OpenClash 配置与密钥、防火墙、原有的 router-tunnel。

pub mod alerts;
pub mod clash;
pub mod collect;
pub mod deploy;
pub mod replace;
pub mod scripts;
pub mod tunnel;

use crate::config::Config;
use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

/// 接入命令（一次性令牌）的有效期
pub const BOOTSTRAP_MINUTES: i64 = 30;
/// 分隧道端口的咨询锁（两个同时新建的不会分到同一个端口；唯一约束兜底）
const PORT_LOCK: i64 = 0x0072_6f75_7465_7230;

/// 隧道公钥清单与 router_known_hosts 的读-改-写串行
static FILES_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 路由的状态：最近一次采集连上了 = online；没连上、且换了这把隧道密钥之后连上过 = offline；其余（还没接入、刚重新生成接入命令）= pending
pub fn state_of(reachable: Option<bool>, last_ok_at: Option<OffsetDateTime>, tunnel_key_at: Option<OffsetDateTime>) -> &'static str {
    if reachable == Some(true) {
        "online"
    } else if last_ok_at.is_some_and(|ok| tunnel_key_at.is_some_and(|k| ok >= k)) {
        "offline"
    } else {
        "pending"
    }
}

/// 接入命令：`curl -fsS <public_url>/api/v1/router-bootstrap/<令牌> | sh`
pub fn bootstrap_cmd(public_url: &str, token: &str) -> String {
    format!("curl -fsS {}/api/v1/router-bootstrap/{token} | sh", public_url.trim_end_matches('/'))
}

/// 中控 sshd 的主机公钥（写进路由的 known_hosts）与中控登录公钥（写进路由的 authorized_keys）：「类型 base64」/ 整行
pub struct CentralKeys {
    pub host_key: String,
    pub login_pub: String,
}

pub async fn central_keys(cfg: &Config) -> anyhow::Result<CentralKeys> {
    use anyhow::Context;
    let host = tokio::fs::read_to_string(&cfg.routers.sshd_host_key)
        .await
        .with_context(|| format!("读中控 sshd 的主机公钥 {}（[routers] sshd_host_key）", cfg.routers.sshd_host_key.display()))?;
    let host_key = key_of(&host).with_context(|| format!("{} 不是 ssh-ed25519 公钥", cfg.routers.sshd_host_key.display()))?;
    let p = cfg.ssh_key.with_extension("pub");
    let login = tokio::fs::read_to_string(&p).await.with_context(|| format!("读中控登录公钥 {}", p.display()))?;
    let login_pub = login.lines().next().unwrap_or("").trim().to_string();
    if !login_pub.starts_with("ssh-") || login_pub.split_whitespace().count() < 2 || login_pub.contains('\'') {
        anyhow::bail!("{} 不是一行 OpenSSH 公钥", p.display());
    }
    Ok(CentralKeys { host_key, login_pub })
}

/// 一行公钥（`ssh-ed25519 <base64> [注释]`）里的「类型 base64」；只收 ed25519
pub fn key_of(line: &str) -> Option<String> {
    let mut it = line.split_whitespace();
    let (t, b) = (it.next()?, it.next()?);
    (t == "ssh-ed25519" && valid_b64(b)).then(|| format!("{t} {b}"))
}

fn valid_b64(s: &str) -> bool { !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')) }

/// 隧道密钥：OpenSSH 格式的 ed25519 私钥（不加密，接入脚本写到路由的 /etc/fleet-tunnel/id_ed25519）与「ssh-ed25519 base64」公钥
pub struct TunnelKey {
    pub private_pem: String,
    pub public: String,
}

/// 生成一对隧道密钥。私钥只给接入脚本（或一键部署）用，不落库、不进日志
pub fn tunnel_keypair(comment: &str) -> TunnelKey {
    use rand::RngCore;
    let mut seed = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut seed);
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    let pk = sk.verifying_key().to_bytes();
    fn put(b: &mut Vec<u8>, s: &[u8]) {
        b.extend_from_slice(&(s.len() as u32).to_be_bytes());
        b.extend_from_slice(s);
    }
    let mut public = Vec::new();
    put(&mut public, b"ssh-ed25519");
    put(&mut public, &pk);
    // 私钥段：两个相同的校验数、密钥、注释，按 8 字节补 1 2 3 …（PROTOCOL.key）
    let check = rand::thread_rng().next_u32().to_be_bytes();
    let mut private = Vec::new();
    private.extend_from_slice(&check);
    private.extend_from_slice(&check);
    put(&mut private, b"ssh-ed25519");
    put(&mut private, &pk);
    let mut both = seed.to_vec();
    both.extend_from_slice(&pk);
    put(&mut private, &both);
    put(&mut private, comment.as_bytes());
    let mut pad = 1u8;
    while private.len() % 8 != 0 {
        private.push(pad);
        pad += 1;
    }
    let mut out = b"openssh-key-v1\0".to_vec();
    put(&mut out, b"none");
    put(&mut out, b"none");
    put(&mut out, b"");
    out.extend_from_slice(&1u32.to_be_bytes());
    put(&mut out, &public);
    put(&mut out, &private);
    let b64 = base64::engine::general_purpose::STANDARD;
    let body = b64.encode(&out);
    let lines: Vec<&str> = body.as_bytes().chunks(70).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect();
    // 中间缓冲里的私钥抹掉（SigningKey 自己 drop 时会抹）
    seed.fill(0);
    both.fill(0);
    private.fill(0);
    out.fill(0);
    TunnelKey {
        private_pem: format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n", lines.join("\n")),
        public: format!("ssh-ed25519 {}", b64.encode(&public)),
    }
}

/// 中控本机这个端口上有程序在监听（隧道端口有人占着）
async fn listening(port: u16) -> bool { crate::ssh::tcp_ok("127.0.0.1", port, Duration::from_millis(300)).await }

/// 分一个隧道端口：段内最小的、库里没人用、此刻也没人在监听的。删掉的路由的旧隧道可能还连着、占着它原来的端口
/// （sshd 不会因为公钥撤了就断开已有连接），分出去的话新路由的隧道建不起来，中控却会经它连到那台旧路由
pub async fn alloc_port(tx: &mut Tx<'_>, cfg: &Config) -> ApiResult<i32> {
    sqlx::query("select pg_advisory_xact_lock($1)").bind(PORT_LOCK).execute(&mut **tx).await?;
    let taken: HashSet<i32> = sqlx::query_scalar("select tunnel_port from router").fetch_all(&mut **tx).await?.into_iter().collect();
    for p in cfg.routers.port_min..=cfg.routers.port_max {
        if !taken.contains(&(p as i32)) && !listening(p).await {
            return Ok(p as i32);
        }
    }
    Err(ApiError::conflict(format!("隧道端口 {}–{} 都分完了（或被别的程序占着）", cfg.routers.port_min, cfg.routers.port_max)))
}

/// 换掉一台已经发过隧道密钥的路由的密钥时（重新生成接入命令、一键部署），隧道端口也换一个：旧隧道就算还连着（sshd 不因公钥撤了就断开），
/// 中控也不再经它连（旧隧道立刻失效）；旧端口要等那条连接断了才会再分出去（见 [`alloc_port`]）。还没发过密钥的保留原端口
pub async fn rotate_port(tx: &mut Tx<'_>, cfg: &Config, id: Uuid) -> ApiResult<i32> {
    let (port, issued): (i32, bool) = sqlx::query_as("select tunnel_port, tunnel_pubkey is not null or tunnel_key_at is not null from router where id = $1 for update")
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
    if !issued {
        return Ok(port);
    }
    let p = alloc_port(tx, cfg).await?;
    sqlx::query("update router set tunnel_port = $2 where id = $1").bind(id).bind(p).execute(&mut **tx).await?;
    Ok(p)
}

/// 新的一次性接入令牌：返回 (令牌, sha256, 到期时间)
pub fn new_token() -> (String, Vec<u8>, OffsetDateTime) {
    let t = crate::crypto::random_token(32);
    let h = crate::crypto::sha256(t.as_bytes());
    (t, h, OffsetDateTime::now_utc() + time::Duration::minutes(BOOTSTRAP_MINUTES))
}

/// 令牌的样子（random_token(32)：43 个 URL 安全 base64 字符）；不像的不查库
pub fn token_shape_ok(t: &str) -> bool { t.len() == 43 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') }

/// 由库整份重写两个文件（内容没变不写）：
/// - 隧道公钥清单 `tunnel_keys_file`：每行「端口 ssh-ed25519 <base64>」，sshd 的 AuthorizedKeysCommand 读它；
/// - 经隧道登录路由用的 known_hosts：每行「router-<id> ssh-ed25519 <base64>」（HostKeyAlias）。
///
/// tmp + rename 原子替换：sshd 随时在读，不能读到写了一半的。路由增删、重新接入、记下主机公钥之后调；采集每轮也调一次，文件被删、被改能自己恢复
pub async fn sync_files(st: &AppState) -> anyhow::Result<()> {
    let _g = FILES_LOCK.lock().await;
    let rows: Vec<(Uuid, i32, Option<String>, Option<String>)> =
        sqlx::query_as("select id, tunnel_port, tunnel_pubkey, host_key from router order by tunnel_port").fetch_all(&st.db).await?;
    let (keys, hosts) = file_contents(&rows);
    write_if_changed(&st.cfg.routers.tunnel_keys_file, &keys).await?;
    write_if_changed(&st.cfg.routers.known_hosts, &hosts).await?;
    Ok(())
}

/// (隧道公钥清单, known_hosts) 的内容；格式不对的公钥不写（库里不会有，防万一）
pub fn file_contents(rows: &[(Uuid, i32, Option<String>, Option<String>)]) -> (String, String) {
    let keys = rows.iter().filter_map(|(_, p, k, _)| k.as_deref().and_then(key_of).map(|k| format!("{p} {k}\n"))).collect();
    let hosts = rows.iter().filter_map(|(id, _, _, h)| h.as_deref().and_then(key_of).map(|h| format!("router-{id} {h}\n"))).collect();
    (keys, hosts)
}

async fn write_if_changed(path: &Path, content: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    if tokio::fs::read_to_string(path).await.ok().as_deref() == Some(content) {
        return Ok(());
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, content).await.with_context(|| format!("写 {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path).await.with_context(|| format!("替换 {}", path.display()))?;
    Ok(())
}

/// 改了公钥清单、主机公钥之后：重写两个文件，写不进去回 500（库已经改了；采集循环每轮还会再对齐一次）
pub async fn sync_files_or_500(st: &AppState) -> ApiResult<()> {
    sync_files(st).await.map_err(|e| ApiError::internal(format!("写隧道公钥清单 / router_known_hosts 失败：{e:#}")))
}

/// 列表与详情的一行：路由 + 团队 + 最近一次采集
#[derive(sqlx::FromRow)]
pub struct Row {
    pub id: Uuid,
    pub name: String,
    pub team_id: Option<Uuid>,
    pub team_name: Option<String>,
    pub note: String,
    pub tunnel_port: i32,
    pub tunnel_key_at: Option<OffsetDateTime>,
    pub bootstrap_expires_at: Option<OffsetDateTime>,
    pub deploy_state: Option<String>,
    pub deploy_step: Option<String>,
    pub deploy_error: Option<String>,
    pub deploy_host: Option<String>,
    pub deploy_port: Option<i32>,
    pub deploy_user: Option<String>,
    pub deploy_started_at: Option<OffsetDateTime>,
    pub deploy_finished_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub reachable: Option<bool>,
    pub error: Option<String>,
    pub data: Option<Value>,
    pub last_ok_at: Option<OffsetDateTime>,
}

pub const SELECT: &str = "select r.id, r.name, r.team_id, t.name team_name, r.note, r.tunnel_port, r.tunnel_key_at,
        case when r.bootstrap_expires_at > now() then r.bootstrap_expires_at end bootstrap_expires_at,
        r.deploy_state, r.deploy_step, r.deploy_error, r.deploy_host, r.deploy_port, r.deploy_user, r.deploy_started_at, r.deploy_finished_at,
        r.created_at, s.reachable, s.error, s.data, s.last_ok_at
    from router r left join team t on t.id = r.team_id left join router_status s on s.router_id = r.id";

fn rfc(t: Option<OffsetDateTime>) -> Value {
    t.and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).map(Value::String).unwrap_or(Value::Null)
}

impl Row {
    pub fn state(&self) -> &'static str { state_of(self.reachable, self.last_ok_at, self.tunnel_key_at) }

    pub fn snapshot(&self) -> Option<collect::Snapshot> { self.data.clone().and_then(|d| serde_json::from_value(d).ok()) }

    /// 接口里的 Router；`detail` 为真时加上完整快照
    pub fn view(&self, detail: bool) -> Value {
        let snap = self.snapshot();
        let deploy = self.deploy_state.as_ref().map(|s| {
            json!({
                "state": s, "step": self.deploy_step, "error": self.deploy_error,
                "host": self.deploy_host, "port": self.deploy_port, "user": self.deploy_user,
                "started_at": rfc(self.deploy_started_at), "finished_at": rfc(self.deploy_finished_at),
            })
        });
        let mut v = json!({
            "id": self.id,
            "name": self.name,
            "team": self.team_id.map(|id| json!({"id": id, "name": self.team_name})),
            "note": self.note,
            "tunnel_port": self.tunnel_port,
            "created_at": rfc(Some(self.created_at)),
            "state": self.state(),
            "last_seen_at": rfc(self.last_ok_at),
            "error": if self.reachable == Some(true) { None } else { self.error.clone() },
            "summary": snap.as_ref().map(collect::summary),
            "deploy": deploy,
            "bootstrap_expires_at": rfc(self.bootstrap_expires_at),
        });
        if detail {
            v["detail"] = snap.map(|s| serde_json::to_value(s).unwrap_or_default()).unwrap_or(Value::Null);
        }
        v
    }
}

pub async fn load(db: &sqlx::PgPool, id: Option<Uuid>) -> sqlx::Result<Vec<Row>> {
    match id {
        Some(id) => sqlx::query_as(&format!("{SELECT} where r.id = $1")).bind(id).fetch_all(db).await,
        None => sqlx::query_as(&format!("{SELECT} order by t.sort_order nulls first, t.name nulls first, r.name")).fetch_all(db).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states() {
        let t = OffsetDateTime::now_utc();
        let ago = |s| t - time::Duration::seconds(s);
        assert_eq!(state_of(None, None, None), "pending", "建了记录、还没采集过");
        assert_eq!(state_of(Some(false), None, Some(ago(60))), "pending", "发了密钥、还没连上过");
        assert_eq!(state_of(Some(true), Some(t), Some(ago(60))), "online");
        assert_eq!(state_of(Some(false), Some(ago(30)), Some(ago(60))), "offline");
        assert_eq!(state_of(Some(false), Some(ago(90)), Some(ago(60))), "pending", "重新接入之后还没连上过");
        assert_eq!(state_of(Some(true), Some(t), None), "online", "重新生成了接入命令，旧隧道还连着");
        assert_eq!(state_of(Some(false), Some(ago(30)), None), "pending");
    }

    #[test]
    fn keys_and_files() {
        assert_eq!(key_of("ssh-ed25519 AAAAC3Nz+/= root@x").as_deref(), Some("ssh-ed25519 AAAAC3Nz+/="));
        assert!(key_of("ssh-rsa AAAA").is_none() && key_of("ssh-ed25519").is_none() && key_of("ssh-ed25519 AA'A").is_none());
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let rows = vec![
            (a, 23000, Some("ssh-ed25519 AAAA1".to_string()), Some("ssh-ed25519 HOST1".to_string())),
            (b, 23001, None, None),
            (Uuid::now_v7(), 23002, Some("ssh-ed25519 AA;reboot".to_string()), None),
        ];
        let (keys, hosts) = file_contents(&rows);
        assert_eq!(keys, "23000 ssh-ed25519 AAAA1\n");
        assert_eq!(hosts, format!("router-{a} ssh-ed25519 HOST1\n"));
        assert!(token_shape_ok(&new_token().0));
        assert!(!token_shape_ok("abc") && !token_shape_ok(&format!("{}/", "a".repeat(42))));
        assert_eq!(bootstrap_cmd("https://f.test/", "TOK"), "curl -fsS https://f.test/api/v1/router-bootstrap/TOK | sh");
    }

    /// 生成的私钥 ssh-keygen 认得，推出来的公钥就是我们记的那把
    #[cfg(unix)]
    #[test]
    fn tunnel_key_is_valid_openssh() {
        use std::os::unix::fs::PermissionsExt;
        let k = tunnel_keypair("fleet-tunnel test");
        assert!(k.private_pem.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----\n") && k.private_pem.ends_with("-----END OPENSSH PRIVATE KEY-----\n"));
        assert!(key_of(&k.public).is_some());
        let dir = std::env::temp_dir().join(format!("fleet-tk-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("id_ed25519");
        std::fs::write(&p, &k.private_pem).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        let out = std::process::Command::new("ssh-keygen").arg("-y").arg("-f").arg(&p).output().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let derived = String::from_utf8_lossy(&out.stdout);
        assert_eq!(key_of(&derived).as_deref(), Some(k.public.as_str()));
        assert_ne!(tunnel_keypair("x").public, k.public);
    }
}
