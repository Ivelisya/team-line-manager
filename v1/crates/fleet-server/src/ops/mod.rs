//! 中控上的运维子命令（不起服务、只跑一次）：`fleet-server backup`、`restore`、`ipintel-fetch`。
//! 取代 deploy/backup/fleet-backup.sh、docs/OPS.md §10 的手工恢复步骤与 deploy/ipintel/fetch-dbs.sh。
//!
//! 共同的约定：
//! - 外部工具（pg_dump、pg_restore、psql、tar、age）照旧调用，和脚本一样；周围的逻辑（暂存、校验和、轮换、回滚、退出码）在这里，有单元测试。
//! - 子进程都 `kill_on_drop`：收到 SIGTERM / Ctrl-C 时整个任务被丢弃，子进程跟着被杀，暂存目录由守卫删掉（脚本的 `trap … EXIT`）。
//! - 只在同一个文件系统里 rename 换上新文件：备份先落在备份目录下的 `.work-*`，离线库先落在库目录下的 `.fetch.*`。
pub mod backup;
pub mod ipintel_fetch;
pub mod restore;

use anyhow::{bail, Context};
use sha2::Digest;
use std::io::Read;
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// 调 PostgreSQL 客户端工具的方式。生产：`runuser -u postgres -- <工具>`，以 postgres 身份经本机 socket 连库（脚本与 §10 都这么做）。
/// 测试：换成 `docker exec -i <容器>` 之类，连临时库
#[derive(Clone, Debug)]
pub struct PgTools {
    pub wrap: Vec<String>,
}

impl PgTools {
    pub fn system() -> PgTools { PgTools { wrap: ["runuser", "-u", "postgres", "--"].map(String::from).to_vec() } }

    /// `prog` 加上前缀的命令；标准错误直接给调用方的终端 / journal（pg_dump、pg_restore 的报错原样可见）
    pub fn cmd(&self, prog: &str) -> Command {
        let mut c = match self.wrap.split_first() {
            Some((first, rest)) => {
                let mut c = Command::new(first);
                c.args(rest).arg(prog);
                c
            }
            None => Command::new(prog),
        };
        c.kill_on_drop(true).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::inherit());
        c
    }

    /// 以超级用户在 `postgres` 库里执行一段 SQL（`ON_ERROR_STOP`，多条语句在同一个事务里），返回 `-tA` 的输出
    pub async fn psql(&self, sql: &str) -> anyhow::Result<String> {
        let out = self
            .cmd("psql")
            .args(["-X", "-q", "-tA", "-v", "ON_ERROR_STOP=1", "-d", "postgres", "-c", sql])
            .stdout(std::process::Stdio::piped())
            .output()
            .await
            .context("执行 psql")?;
        if !out.status.success() {
            bail!("psql 失败（{}）：{sql}", exit_text(out.status));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// 只许小写字母、数字、下划线的标识符（库名、角色名会拼进 SQL 与命令行）
pub fn check_ident(what: &str, s: &str) -> anyhow::Result<()> {
    if s.is_empty() || s.len() > 63 || !s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') || s.starts_with(|c: char| c.is_ascii_digit()) {
        bail!("{what}「{s}」不是小写字母、数字、下划线组成的名字：这里不处理");
    }
    Ok(())
}

/// database_url 里的库名与用户名（`postgres://fleet:密码@127.0.0.1/fleet` → ("fleet", "fleet")）
pub fn db_and_user(database_url: &str) -> anyhow::Result<(String, String)> {
    let u = reqwest::Url::parse(database_url).context("database_url 不是 URL")?;
    let db = u.path().trim_start_matches('/').to_string();
    let user = u.username().to_string();
    check_ident("database_url 里的库名", &db)?;
    check_ident("database_url 里的用户名", &user)?;
    Ok((db, user))
}

pub fn exit_text(s: std::process::ExitStatus) -> String {
    match s.code() {
        Some(c) => format!("退出码 {c}"),
        None => "被信号终止".into(),
    }
}

/// 文件的 sha256（十六进制小写）
pub fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// sha256sum 格式的一行（「哈希  文件名」）里的哈希；不是 64 位十六进制返回 None
pub fn parse_sha256_line(line: &str) -> Option<String> {
    let h = line.split_whitespace().next()?.to_ascii_lowercase();
    (h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

/// 同 `du -h` 的写法（1024 进位，小于 10 的保留一位小数）：只用于日志
pub fn human_size(n: u64) -> String {
    let units = ["K", "M", "G", "T"];
    if n < 1024 {
        return format!("{n}");
    }
    let mut v = n as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if v < 10.0 {
        format!("{:.1}{}", (v * 10.0).ceil() / 10.0, units[i])
    } else {
        format!("{}{}", v.ceil() as u64, units[i])
    }
}

/// 暂存目录：建在目标目录里（同一个文件系统，最后一步 rename 是原子的），0700；守卫丢弃时连内容一起删（脚本的 `trap 'rm -rf "$W"' EXIT`）
pub struct WorkDir {
    path: PathBuf,
    keep: bool,
}

impl WorkDir {
    /// `<parent>/<prefix><随机 6 位>`，同 `mktemp -d "$D/.work-XXXXXX"`
    pub fn new(parent: &Path, prefix: &str) -> anyhow::Result<WorkDir> {
        use rand::distributions::{Alphanumeric, DistString};
        for _ in 0..16 {
            let p = parent.join(format!("{prefix}{}", Alphanumeric.sample_string(&mut rand::thread_rng(), 6)));
            match mkdir_mode(&p, 0o700) {
                Ok(()) => return Ok(WorkDir { path: p, keep: false }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("建临时目录 {}", p.display())),
            }
        }
        bail!("在 {} 里建不了临时目录", parent.display())
    }

    pub fn path(&self) -> &Path { &self.path }

    /// 不删（测试里看内容用）
    #[doc(hidden)]
    pub fn keep(&mut self) { self.keep = true; }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

pub fn mkdir_mode(p: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(mode).create(p)?;
        // DirBuilder 的 mode 受 umask 影响：显式再设一次
        set_mode(p, mode)
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::create_dir(p)
    }
}

pub fn set_mode(p: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (p, mode);
        Ok(())
    }
}

/// 用户名 → (uid, gid)；没有这个用户返回 None
#[cfg(unix)]
pub fn lookup_user(name: &str) -> Option<(u32, u32)> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: getpwnam 返回静态缓冲区里的记录或 NULL；只在这个单线程的运维命令里、马上读出两个整数
    let p = unsafe { libc::getpwnam(c.as_ptr()) };
    if p.is_null() {
        return None;
    }
    let p = unsafe { &*p };
    Some((p.pw_uid, p.pw_gid))
}

/// 组名 → gid
#[cfg(unix)]
pub fn lookup_group(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: 同 lookup_user
    let g = unsafe { libc::getgrnam(c.as_ptr()) };
    if g.is_null() {
        return None;
    }
    Some(unsafe { (*g).gr_gid })
}

/// 等 SIGTERM（systemd 停服务、超时）或 SIGINT（Ctrl-C）。调用时立刻装上处理
pub fn stop_signal() -> impl std::future::Future<Output = &'static str> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT");
    async move {
        tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = int.recv() => "SIGINT",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idents_and_url() {
        assert_eq!(db_and_user("postgres://fleet:pw@127.0.0.1/fleet").unwrap(), ("fleet".into(), "fleet".into()));
        assert_eq!(db_and_user("postgres://fleet:fleet@127.0.0.1:55440/fleet_test").unwrap(), ("fleet_test".into(), "fleet".into()));
        assert!(db_and_user("postgres://fleet:pw@127.0.0.1/Fleet").is_err());
        assert!(db_and_user("postgres://fleet:pw@127.0.0.1/a;drop").is_err());
        assert!(db_and_user("postgres://fleet:pw@127.0.0.1/").is_err());
        assert!(check_ident("x", "1abc").is_err());
    }

    #[test]
    fn sha256_lines() {
        let h = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(parse_sha256_line(&format!("{h}  fleet-20261122-033000.tar\n")).as_deref(), Some(h));
        assert_eq!(parse_sha256_line(&h.to_uppercase()).as_deref(), Some(h));
        assert_eq!(parse_sha256_line("abc  x"), None);
        assert_eq!(parse_sha256_line(""), None);
    }

    #[test]
    fn du_style_sizes() {
        assert_eq!(human_size(512), "512");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(15 * 1024 * 1024 + 1), "16M");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0G");
    }

    #[test]
    fn workdir_is_removed_on_drop() {
        let t = tempfile::tempdir().unwrap();
        let p;
        {
            let w = WorkDir::new(t.path(), ".work-").unwrap();
            p = w.path().to_path_buf();
            std::fs::write(p.join("x"), b"1").unwrap();
            assert!(p.file_name().unwrap().to_str().unwrap().starts_with(".work-"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o700);
            }
        }
        assert!(!p.exists());
    }
}
