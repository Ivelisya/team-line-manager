//! `fleet-server restore <备份> [--dry-run] [--yes]`：docs/OPS.md §10「从备份恢复」第 2–4 步做成代码（root 执行，fleet-server 停着）。
//!
//! 先准备（不改系统里的任何东西，`--dry-run` 到此为止，打出将要做的）：
//! 1. 备份拷进 0700 的暂存目录（`/root/fleet-restore-XXXXXX`），边拷边算 sha256，和旁边的 `<备份>.sha256` 核对；
//!    之后只用暂存的这一份（核对过的字节不会再被换掉）。
//! 2. `.tar.age`（按 age 文件头认）：`age -d -i <--identity>` 解开。
//! 3. tar 的成员逐个检查：只许普通文件和目录、只许 fleet-backup 打的那几条路径（不许 `..`、绝对路径、符号链接、硬链接），然后才解开。
//!    `fleet.dump`、`etc/fleet/secret.key`（32 字节）、`etc/fleet/ssh/id_ed25519`、`var/lib/fleet/known_hosts` 必须在。
//! 4. `pg_restore -l` 读得了；`_sqlx_migrations` 里的迁移和本程序逐个核对：备份里有本程序没有的、校验和不同的、没跑成功的都拒绝；
//!    本程序比备份多的迁移恢复时补跑（同 §10 第 5 步「启动时跑迁移」，这里在换库之前就跑，跑不过不换）。
//!
//! 真正恢复（`--yes`；fleet-server 在监听就拒绝——§10 第 3 步要先 `systemctl stop fleet-server`，这一步留给人做）：
//! 5. 先做一份安全备份（同 `fleet-server backup`，不轮换）：恢复前的库与密钥。
//! 6. 备份恢复进新库 `<库>_restore_<时间>`（`pg_restore --exit-on-error`，以 postgres 执行、从标准输入读，对象属主恢复为原来的），
//!    以 fleet.toml 里的账号跑迁移。
//! 7. 放回密钥与主机指纹（§10 第 4 步的属主、权限）；被换掉的旧文件以硬链接留成 `<文件>.pre-restore-<时间>`。
//!    fleet.toml 不换（库密码是新装时生成的），备份里那份存成 `/etc/fleet/fleet.toml.from-backup-<时间>`，打出哪些键不同。
//! 8. 一个事务里换库：`<库>` 改名 `<库>_pre_restore_<时间>` 留着，`<库>_restore_<时间>` 改名 `<库>`。
//!
//! 5 之后任何一步失败（含 Ctrl-C / SIGTERM）：已放回的文件换回原来的、新库删掉，系统回到恢复之前；第 8 步是最后一步且是原子的。
//! 重启 ssh / caddy / fleet-server、重建 central-local 等要 systemctl 或要服务在跑的步骤不在这里做，做完打出剩下的手工步骤。
use super::backup::{self, AGE_MAGIC};
use super::{exit_text, sha256_file, PgTools, WorkDir};
use anyhow::{bail, Context};
use sha2::Digest;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use time::{OffsetDateTime, UtcOffset};

/// 谁拥有放回去的文件
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    /// fleet:fleet（fleet-server 要读）
    Fleet,
    /// root:fleet
    RootFleet,
    /// root:root
    Root,
}

impl Who {
    fn label(self) -> &'static str {
        match self {
            Who::Fleet => "fleet:fleet",
            Who::RootFleet => "root:fleet",
            Who::Root => "root:root",
        }
    }
}

/// 放回的属主怎么取。生产查系统里的 fleet 用户；测试不改属主（不是 root）
#[derive(Clone, Copy, Debug)]
pub enum Owners {
    System,
    Keep,
}

impl Owners {
    fn ids(self, who: Who) -> anyhow::Result<Option<(u32, u32)>> {
        match self {
            Owners::Keep => Ok(None),
            Owners::System => {
                #[cfg(unix)]
                {
                    let fleet_gid = super::lookup_group("fleet").context("没有 fleet 组：先按 §10 第 1 步部署一次")?;
                    Ok(Some(match who {
                        Who::Fleet => (super::lookup_user("fleet").context("没有 fleet 用户：先按 §10 第 1 步部署一次")?.0, fleet_gid),
                        Who::RootFleet => (0, fleet_gid),
                        Who::Root => (0, 0),
                    }))
                }
                #[cfg(not(unix))]
                {
                    let _ = who;
                    Ok(None)
                }
            }
        }
    }
}

/// 固定放回的文件：§10 第 4 步（路径相对根，权限，属主，是否必须在备份里）
pub const FILES: [(&str, u32, Who, bool); 9] = [
    ("etc/fleet/secret.key", 0o600, Who::Fleet, true),
    ("etc/fleet/ssh/id_ed25519", 0o600, Who::Fleet, true),
    ("etc/fleet/ssh/id_ed25519.pub", 0o644, Who::Fleet, false),
    ("var/lib/fleet/known_hosts", 0o640, Who::Fleet, true),
    ("etc/fleet/ipintel-overrides.toml", 0o640, Who::RootFleet, false),
    ("etc/fleet/backup.pub", 0o640, Who::RootFleet, false),
    ("etc/fleet/backup-age.recipients", 0o640, Who::RootFleet, false),
    ("etc/ssh/ssh_host_ed25519_key", 0o600, Who::Root, false),
    ("etc/ssh/ssh_host_ed25519_key.pub", 0o644, Who::Root, false),
];
/// Caddy 的带外配置目录：里面的文件按原权限放回（同 `cp -a …/. /etc/caddy/…/`：只覆盖、不删本机多出来的）
pub const CADDY_DIRS: [&str; 2] = ["etc/caddy/global.d", "etc/caddy/sites.d"];

/// 恢复在哪台机器上、用哪些工具。生产用 [`Env::system`]
#[derive(Clone, Debug)]
pub struct Env {
    pub root: PathBuf,
    pub pg: PgTools,
    pub tar: String,
    pub age: String,
    pub owners: Owners,
    /// fleet.toml 的 database_url（库名、属主角色、跑迁移用的账号）
    pub database_url: String,
    /// fleet.toml 的 listen：占着就是 fleet-server 在跑
    pub listen: String,
    /// 恢复前的安全备份（`rotate` 必须是 false）
    pub safety: backup::Plan,
    /// 暂存目录建在这里
    pub work_parent: PathBuf,
}

impl Env {
    pub fn system(cfg: &crate::config::Config) -> anyhow::Result<Env> {
        let (db, _) = super::db_and_user(&cfg.database_url)?;
        Ok(Env {
            root: PathBuf::from("/"),
            pg: PgTools::system(),
            tar: "tar".into(),
            age: "age".into(),
            owners: Owners::System,
            database_url: cfg.database_url.clone(),
            listen: cfg.listen.clone(),
            safety: backup::Plan { rotate: false, ..backup::Plan::system(cfg.selfmon.backup_dir.clone(), db) },
            work_parent: PathBuf::from("/root"),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Opts {
    pub archive: PathBuf,
    /// age 私钥文件（加密备份才要）
    pub identity: Option<PathBuf>,
    /// 没有 .sha256 时也继续（来源可信、校验和丢了才用）
    pub skip_sha256: bool,
}

/// `_sqlx_migrations` 里的一行
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub version: i64,
    pub success: bool,
    pub checksum: Vec<u8>,
}

/// 一个要放回的文件
#[derive(Clone, Debug)]
pub struct Install {
    pub rel: String,
    pub src: PathBuf,
    pub dst: PathBuf,
    pub mode: u32,
    pub who: Who,
    pub change: Change,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    New,
    Replace,
    Same,
}

/// 准备好的恢复：暂存目录（丢弃时删掉，里面有密钥）与将要做的事
pub struct Prepared {
    work: WorkDir,
    pub archive: PathBuf,
    pub sha256: String,
    pub sha256_checked: bool,
    pub encrypted: bool,
    pub bytes: u64,
    /// 解开的树（暂存目录下）
    pub tree: PathBuf,
    pub applied: Vec<Applied>,
    /// 恢复时补跑的迁移（本程序有、备份里没有）
    pub pending: Vec<i64>,
    pub toc_tables: usize,
    pub files: Vec<Install>,
    /// 备份里不放回的成员（fleet.toml 另说）
    pub skipped: Vec<String>,
    /// fleet.toml 里和本机不同的键（不含 database_url）
    pub toml_diff: Vec<String>,
    pub warnings: Vec<String>,
    pub db: String,
    pub db_owner: String,
    /// 现在的库：(表数, 最新迁移)；库不存在为 None
    pub current_db: Option<(i64, Option<i64>)>,
    pub stamp: String,
}

impl Prepared {
    pub fn work_dir(&self) -> &Path { self.work.path() }
    pub fn restore_db(&self) -> String { format!("{}_restore_{}", self.db, self.stamp) }
    pub fn old_db(&self) -> String { format!("{}_pre_restore_{}", self.db, self.stamp) }
    fn file_suffix(&self) -> String { format!("pre-restore-{}", self.stamp.replace('_', "-")) }
    fn toml_copy(&self, root: &Path) -> PathBuf { root.join(format!("etc/fleet/fleet.toml.from-backup-{}", self.stamp.replace('_', "-"))) }

    /// 将要做的事（`--dry-run` 打出来的就是它）
    pub fn describe(&self, env: &Env) -> String {
        let mut o = String::new();
        let mut l = |s: String| {
            o.push_str(&s);
            o.push('\n');
        };
        l(format!(
            "备份：{}（{} 字节，{}，sha256 {}{}）",
            self.archive.display(),
            self.bytes,
            if self.encrypted { "age 加密，已用私钥解开" } else { "明文" },
            self.sha256,
            if self.sha256_checked { "，与 .sha256 一致" } else { "，没有核对（--skip-sha256）" }
        ));
        let bin_max = crate::db::MIGRATOR.migrations.iter().map(|m| m.version).max().unwrap_or(0);
        let bk_max = self.applied.iter().map(|a| a.version).max().unwrap_or(0);
        l(format!("库：fleet.dump 有 {} 张表，迁移到 {bk_max}（本程序到 {bin_max}）", self.toc_tables));
        if !self.pending.is_empty() {
            l(format!("  恢复时补跑 {} 个迁移：{}", self.pending.len(), self.pending.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")));
        }
        match self.current_db {
            Some((t, v)) => l(format!(
                "  现在的库 {}：{t} 张表，迁移到 {}；改名为 {} 留着（不删）",
                self.db,
                v.map(|v| v.to_string()).unwrap_or_else(|| "（没有迁移记录）".into()),
                self.old_db()
            )),
            None => l(format!("  现在没有库 {}", self.db)),
        }
        l(format!("  备份先恢复进 {}、跑迁移，都成功后改名为 {}", self.restore_db(), self.db));
        l("文件：".into());
        for f in &self.files {
            let what = match f.change {
                Change::New => "新建",
                Change::Replace => "替换",
                Change::Same => "不变",
            };
            l(format!("  {what} {}（{:04o} {}）", f.dst.display(), f.mode, f.who.label()));
        }
        if self.files.iter().any(|f| f.change == Change::Replace) {
            l(format!("  被替换的旧文件留成 <文件>.{}", self.file_suffix()));
        }
        let toml = self.toml_copy(&env.root);
        l(format!("  不换 {}（库密码是新装时生成的）；备份里那份存成 {}", env.root.join("etc/fleet/fleet.toml").display(), toml.display()));
        if self.toml_diff.is_empty() {
            l("  fleet.toml 除 database_url 外和备份里的一样".into());
        } else {
            l(format!("  fleet.toml 和备份里不同的键（对照着抄过来）：{}", self.toml_diff.join("、")));
        }
        if !self.skipped.is_empty() {
            l(format!("  不放回：{}", self.skipped.join("、")));
        }
        l(format!("先做一份安全备份（恢复前的库与密钥）到 {}，不轮换", env.safety.dir.display()));
        for w in &self.warnings {
            l(format!("注意：{w}"));
        }
        o
    }
}

/// 北京时间的时间戳，库名、文件后缀用（`20261122_033000`）
pub fn stamp(now: OffsetDateTime) -> String {
    let bj = now.to_offset(UtcOffset::from_hms(8, 0, 0).expect("+08:00"));
    bj.format(&time::macros::format_description!("[year][month][day]_[hour][minute][second]")).expect("格式化时间")
}

/// tar 成员名是否是 fleet-backup 打的（目录带不带结尾的 / 都行）。返回规范化后的名字
pub fn member_ok(raw: &str, is_dir: bool) -> Result<String, String> {
    let name = raw.strip_suffix('/').unwrap_or(raw);
    if name.is_empty() || name.starts_with('/') || name.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(format!("成员名「{raw}」不对（绝对路径、..、空段）"));
    }
    let under = |p: &str| name.strip_prefix(p).is_some_and(|r| r.starts_with('/'));
    let ok = if is_dir {
        matches!(name, "etc" | "etc/fleet" | "var" | "var/lib" | "var/lib/fleet" | "etc/ssh" | "etc/caddy" | "etc/caddy/global.d" | "etc/caddy/sites.d")
            || under("etc/fleet")
            || CADDY_DIRS.iter().any(|d| under(d))
    } else {
        matches!(name, "fleet.dump" | "var/lib/fleet/known_hosts" | "etc/ssh/ssh_host_ed25519_key" | "etc/ssh/ssh_host_ed25519_key.pub")
            || under("etc/fleet")
            || CADDY_DIRS.iter().any(|d| under(d))
    };
    if ok {
        Ok(name.to_string())
    } else {
        Err(format!("备份里有不认识的成员「{raw}」：不是 fleet-server backup / fleet-backup.sh 打的包"))
    }
}

/// `pg_restore --data-only -t _sqlx_migrations -f -` 输出里的 COPY 段
pub fn parse_migrations_copy(sql: &str) -> anyhow::Result<Vec<Applied>> {
    let mut lines = sql.lines();
    let header = lines
        .by_ref()
        .find(|l| l.starts_with("COPY ") && l.contains("_sqlx_migrations ("))
        .context("fleet.dump 里没有 _sqlx_migrations 的数据：不是 fleet 的库，或备份做于第一次迁移之前")?;
    let cols: Vec<&str> = header.split_once('(').and_then(|(_, r)| r.split_once(')')).map(|(c, _)| c.split(',').map(str::trim).collect()).context("COPY 行格式不对")?;
    let idx = |n: &str| cols.iter().position(|c| *c == n).with_context(|| format!("_sqlx_migrations 没有 {n} 列"));
    let (iv, is, ic) = (idx("version")?, idx("success")?, idx("checksum")?);
    let mut out = vec![];
    for l in lines {
        if l == "\\." {
            return Ok(out);
        }
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() != cols.len() {
            bail!("_sqlx_migrations 的一行列数不对：{l}");
        }
        let version: i64 = f[iv].parse().with_context(|| format!("迁移版本不是数字：{}", f[iv]))?;
        let success = match f[is] {
            "t" => true,
            "f" => false,
            x => bail!("迁移 {version} 的 success 是 {x}"),
        };
        // COPY 文本格式里 bytea 的十六进制是 \x…，反斜杠再转义一次成 \\x…
        let h = f[ic].trim_start_matches('\\').trim_start_matches('x');
        let checksum = hex::decode(h).with_context(|| format!("迁移 {version} 的校验和不是十六进制"))?;
        out.push(Applied { version, success, checksum });
    }
    bail!("_sqlx_migrations 的 COPY 段没有结束（\\.）：fleet.dump 不完整")
}

/// 备份里的迁移和本程序的核对：返回恢复时要补跑的版本
pub fn check_migrations(applied: &[Applied], binary: &[(i64, Vec<u8>)]) -> anyhow::Result<Vec<i64>> {
    if let Some(a) = applied.iter().find(|a| !a.success) {
        bail!("备份里的迁移 {} 没有跑成功（success=false）：这份库停在迁移一半，不恢复", a.version);
    }
    let bin: BTreeMap<i64, &Vec<u8>> = binary.iter().map(|(v, c)| (*v, c)).collect();
    let newer: Vec<String> = applied.iter().filter(|a| !bin.contains_key(&a.version)).map(|a| a.version.to_string()).collect();
    if !newer.is_empty() {
        bail!(
            "备份里的库跑过本程序没有的迁移 {}：备份来自更新的 fleet-server，用不早于备份时的版本恢复（/opt/fleet/fleet-server version 看版本）",
            newer.join(", ")
        );
    }
    if let Some(a) = applied.iter().find(|a| bin.get(&a.version).is_some_and(|c| **c != a.checksum)) {
        bail!("备份里迁移 {} 的校验和与本程序的不同（迁移文件被改过）：本程序启动时会拒绝这个库，不恢复", a.version);
    }
    let have: std::collections::HashSet<i64> = applied.iter().map(|a| a.version).collect();
    Ok(bin.keys().filter(|v| !have.contains(v)).copied().collect())
}

fn binary_migrations() -> Vec<(i64, Vec<u8>)> { crate::db::MIGRATOR.migrations.iter().map(|m| (m.version, m.checksum.to_vec())).collect() }

/// 拍平的 TOML 键（`retention.raw_days` → 值）
fn flatten(prefix: &str, v: &toml::Value, out: &mut BTreeMap<String, String>) {
    match v {
        toml::Value::Table(t) => {
            for (k, v) in t {
                flatten(&if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") }, v, out);
            }
        }
        other => {
            out.insert(prefix.to_string(), other.to_string());
        }
    }
}

/// 两份 fleet.toml 不同的键（只给键名，值里可能有令牌）；database_url 不算（恢复后要用新装的密码）
pub fn toml_diff(backup: &str, current: &str) -> Vec<String> {
    let parse = |s: &str| -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        if let Ok(v) = s.parse::<toml::Value>() {
            flatten("", &v, &mut m);
        }
        m
    };
    let (a, b) = (parse(backup), parse(current));
    let mut keys: Vec<String> = a.keys().chain(b.keys()).filter(|k| *k != "database_url" && a.get(*k) != b.get(*k)).cloned().collect();
    keys.sort();
    keys.dedup();
    keys
}

async fn run_ok(mut c: tokio::process::Command, what: &str) -> anyhow::Result<()> {
    let st = c.status().await.with_context(|| format!("执行 {what}"))?;
    if !st.success() {
        bail!("{what} 失败（{}）", exit_text(st));
    }
    Ok(())
}

async fn run_out(mut c: tokio::process::Command, what: &str) -> anyhow::Result<String> {
    let out = c.stdout(Stdio::piped()).output().await.with_context(|| format!("执行 {what}"))?;
    if !out.status.success() {
        bail!("{what} 失败（{}）", exit_text(out.status));
    }
    String::from_utf8(out.stdout).with_context(|| format!("{what} 的输出不是 UTF-8"))
}

/// 拷进暂存目录，边拷边算 sha256
fn copy_hashing(src: &Path, dst: &Path) -> anyhow::Result<(String, u64, bool)> {
    let mut i = std::fs::File::open(src).with_context(|| format!("打开 {}", src.display()))?;
    let mut o = std::fs::OpenOptions::new().write(true).create_new(true).open(dst).with_context(|| format!("建 {}", dst.display()))?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let (mut n_all, mut head) = (0u64, Vec::new());
    loop {
        let n = i.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if head.len() < AGE_MAGIC.len() {
            head.extend_from_slice(&buf[..n.min(AGE_MAGIC.len() - head.len())]);
        }
        h.update(&buf[..n]);
        o.write_all(&buf[..n])?;
        n_all += n as u64;
    }
    o.sync_all()?;
    Ok((hex::encode(h.finalize()), n_all, head == AGE_MAGIC))
}

/// 准备：核对、解开、检查，不改系统里的任何东西
pub async fn prepare(opts: &Opts, env: &Env, now: OffsetDateTime) -> anyhow::Result<Prepared> {
    let (db, db_owner) = super::db_and_user(&env.database_url)?;
    let meta = std::fs::metadata(&opts.archive).with_context(|| format!("读不了备份 {}", opts.archive.display()))?;
    if !meta.is_file() {
        bail!("{} 不是文件", opts.archive.display());
    }
    let work = WorkDir::new(&env.work_parent, "fleet-restore-")?;
    let w = work.path().to_path_buf();
    let mut warnings = vec![];

    // 1. sha256
    let copy = w.join("archive");
    let (sha256, bytes, encrypted) = copy_hashing(&opts.archive, &copy)?;
    let sidecar = PathBuf::from(format!("{}.sha256", opts.archive.display()));
    let sha256_checked = match std::fs::read_to_string(&sidecar) {
        Ok(s) => {
            let want = s.lines().next().and_then(super::parse_sha256_line).with_context(|| format!("{} 第一行不是 sha256sum 格式", sidecar.display()))?;
            if want != sha256 {
                bail!("备份和 {} 对不上：记的 {want}，算出 {sha256}（{bytes} 字节）。传输或磁盘损坏，不恢复", sidecar.display());
            }
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && opts.skip_sha256 => {
            warnings.push(format!("没有 {}，按 --skip-sha256 没核对校验和", sidecar.display()));
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("备份旁边没有 {}：核对不了。从 PVE 拷来的连同 .sha256 一起拷；确认来源可信、校验和确实丢了才加 --skip-sha256", sidecar.display())
        }
        Err(e) => return Err(e).with_context(|| format!("读 {}", sidecar.display())),
    };

    // 2. 解密
    let tarball = w.join("backup.tar");
    if encrypted {
        let id = opts.identity.as_ref().context("这是 age 加密的备份：加 --identity <私钥文件>（离线保存的 fleet-backup-key.txt；用完 shred -u 删掉）")?;
        let mut c = tokio::process::Command::new(&env.age);
        c.kill_on_drop(true).arg("-d").arg("-i").arg(id).arg("-o").arg(&tarball).arg(&copy).stdin(Stdio::null()).stderr(Stdio::inherit());
        run_ok(c, "age 解密（私钥对不上？）").await?;
        std::fs::remove_file(&copy)?;
    } else {
        if opts.identity.is_some() {
            warnings.push("备份不是加密的，--identity 没有用到".into());
        }
        std::fs::rename(&copy, &tarball)?;
    }

    // 3. 成员：名字与类型（-tv 每行第一个字符是类型），全部合格才解开
    let mut c = tokio::process::Command::new(&env.tar);
    c.kill_on_drop(true).arg("-tf").arg(&tarball).stdin(Stdio::null()).stderr(Stdio::inherit());
    let names = run_out(c, "tar -t（备份不是完整的 tar？）").await?;
    let mut c = tokio::process::Command::new(&env.tar);
    c.kill_on_drop(true).arg("-tvf").arg(&tarball).stdin(Stdio::null()).stderr(Stdio::inherit());
    let verbose = run_out(c, "tar -tv").await?;
    let (names, verbose): (Vec<&str>, Vec<&str>) = (names.lines().collect(), verbose.lines().collect());
    if names.len() != verbose.len() {
        bail!("tar 的成员列表对不上（{} / {} 行）：成员名里有换行？不恢复", names.len(), verbose.len());
    }
    let mut members = vec![];
    for (n, v) in names.iter().zip(&verbose) {
        let is_dir = match v.as_bytes().first() {
            Some(b'-') => false,
            Some(b'd') => true,
            _ => bail!("备份里的「{n}」不是普通文件或目录（{}）：不恢复", v.split_whitespace().next().unwrap_or("")),
        };
        members.push((member_ok(n, is_dir).map_err(anyhow::Error::msg)?, is_dir));
    }
    let tree = w.join("tree");
    super::mkdir_mode(&tree, 0o700)?;
    let mut c = tokio::process::Command::new(&env.tar);
    c.kill_on_drop(true).arg("-xf").arg(&tarball).arg("-C").arg(&tree).arg("--no-same-owner").stdin(Stdio::null()).stderr(Stdio::inherit());
    run_ok(c, "tar -x").await?;
    std::fs::remove_file(&tarball)?;
    let is_file = |rel: &str| members.iter().any(|(n, d)| n == rel && !d);
    for need in ["fleet.dump", "etc/fleet/secret.key", "etc/fleet/ssh/id_ed25519", "var/lib/fleet/known_hosts"] {
        if !is_file(need) {
            bail!("备份里没有 {need}：不完整，不恢复");
        }
    }
    let key_len = std::fs::metadata(tree.join("etc/fleet/secret.key"))?.len();
    if key_len != 32 {
        bail!("备份里的 etc/fleet/secret.key 是 {key_len} 字节（应为 32）：不恢复");
    }
    if !is_file("etc/ssh/ssh_host_ed25519_key") {
        warnings.push("备份里没有 etc/ssh/ssh_host_ed25519_key（中控 sshd 主机密钥）：恢复后软路由隧道按旧公钥核对、全连不上，要逐台重新接入；PVE 拉取要按 §10 第 9 步重记指纹".into());
    }

    // 4. 库：TOC 与迁移
    let dump = tree.join("fleet.dump");
    let mut magic = [0u8; 5];
    std::fs::File::open(&dump)?.read_exact(&mut magic).context("fleet.dump 太短")?;
    if &magic != b"PGDMP" {
        bail!("fleet.dump 不是 pg_dump -Fc 的格式");
    }
    let mut c = env.pg.cmd("pg_restore");
    c.arg("-l").stdin(std::fs::File::open(&dump)?);
    let toc = run_out(c, "pg_restore -l（fleet.dump 损坏？）").await?;
    let toc_tables = toc.lines().filter(|l| !l.starts_with(';') && l.contains(" TABLE public ")).count();
    let mut c = env.pg.cmd("pg_restore");
    c.args(["--data-only", "-t", "_sqlx_migrations", "-f", "-"]).stdin(std::fs::File::open(&dump)?);
    let copy_sql = run_out(c, "pg_restore 读迁移记录").await?;
    let applied = parse_migrations_copy(&copy_sql)?;
    let pending = check_migrations(&applied, &binary_migrations())?;

    // 现在的库
    let exists = env.pg.psql(&format!("select count(*) from pg_database where datname = '{db}'")).await? == "1";
    let current_db = if exists {
        let mut c = env.pg.cmd("psql");
        c.args(["-X", "-q", "-tA", "-v", "ON_ERROR_STOP=1", "-d", &db, "-c"]).arg(
            "select (select count(*) from information_schema.tables where table_schema = 'public' and table_type = 'BASE TABLE'), \
             (select max(version) from _sqlx_migrations where to_regclass('_sqlx_migrations') is not null)",
        );
        // 没有 _sqlx_migrations 时第二个子查询报错：退回只数表
        let r = match run_out(c, "psql 读现在的库").await {
            Ok(s) => s,
            Err(_) => env.pg.psql("select 0").await.map(|_| String::new())?,
        };
        let mut it = r.trim().split('|');
        let t = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let v = it.next().and_then(|s| s.parse().ok());
        Some((t, v))
    } else {
        None
    };

    // 要放回的文件
    let stamp = stamp(now);
    let mut files = vec![];
    let mut placed: std::collections::HashSet<String> = ["fleet.dump".to_string(), "etc/fleet/fleet.toml".to_string()].into();
    let change = |src: &Path, dst: &Path| -> anyhow::Result<Change> {
        match std::fs::symlink_metadata(dst) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Change::New),
            Err(e) => Err(e).with_context(|| format!("读 {}", dst.display())),
            Ok(m) if !m.file_type().is_file() => bail!("{} 不是普通文件（符号链接？）：不覆盖，先人工处理", dst.display()),
            Ok(_) => Ok(if sha256_file(src)? == sha256_file(dst)? { Change::Same } else { Change::Replace }),
        }
    };
    for (rel, mode, who, _) in FILES {
        if is_file(rel) {
            let (src, dst) = (tree.join(rel), env.root.join(rel));
            files.push(Install { rel: rel.into(), change: change(&src, &dst)?, src, dst, mode, who });
            placed.insert(rel.into());
        }
    }
    for (n, d) in &members {
        if !d && CADDY_DIRS.iter().any(|c| n.starts_with(&format!("{c}/"))) {
            let (src, dst) = (tree.join(n), env.root.join(n));
            let mode = std::fs::metadata(&src)?.permissions();
            #[cfg(unix)]
            let mode = std::os::unix::fs::PermissionsExt::mode(&mode) & 0o755;
            #[cfg(not(unix))]
            let mode = {
                let _ = mode;
                0o644
            };
            files.push(Install { rel: n.clone(), change: change(&src, &dst)?, src, dst, mode, who: Who::Root });
            placed.insert(n.clone());
        }
    }
    let skipped: Vec<String> = members.iter().filter(|(n, d)| !d && !placed.contains(n)).map(|(n, _)| n.clone()).collect();
    let toml_diff = if is_file("etc/fleet/fleet.toml") {
        let a = std::fs::read_to_string(tree.join("etc/fleet/fleet.toml")).unwrap_or_default();
        let b = std::fs::read_to_string(env.root.join("etc/fleet/fleet.toml")).unwrap_or_default();
        toml_diff(&a, &b)
    } else {
        warnings.push("备份里没有 etc/fleet/fleet.toml".into());
        vec![]
    };
    Ok(Prepared {
        work,
        archive: opts.archive.clone(),
        sha256,
        sha256_checked,
        encrypted,
        bytes,
        tree,
        applied,
        pending,
        toc_tables,
        files,
        skipped,
        toml_diff,
        warnings,
        db,
        db_owner,
        current_db,
        stamp,
    })
}

/// Ctrl-C / SIGTERM：真正恢复期间不让进程被直接杀掉，记下来，在步骤之间检查、走回滚
static STOP: AtomicBool = AtomicBool::new(false);

fn check_stop() -> anyhow::Result<()> {
    if STOP.load(Ordering::SeqCst) {
        bail!("收到退出信号，中止");
    }
    Ok(())
}

/// 放回文件的记录：失败时倒着换回
#[derive(Default)]
struct Journal {
    /// (目标, 旧文件的硬链接)；旧文件不存在的是新建的
    done: Vec<(PathBuf, Option<PathBuf>)>,
    dirs: Vec<PathBuf>,
}

impl Journal {
    fn rollback(&mut self) -> Vec<String> {
        let mut problems = vec![];
        for (dst, old) in self.done.drain(..).rev() {
            let r = match &old {
                Some(o) => std::fs::rename(o, &dst),
                None => std::fs::remove_file(&dst),
            };
            if let Err(e) = r {
                problems.push(format!("{}：{e}", dst.display()));
            }
        }
        for d in self.dirs.drain(..).rev() {
            let _ = std::fs::remove_dir(d);
        }
        problems
    }
}

/// 放回一个文件：同目录下建临时文件（O_EXCL、不跟符号链接）、设属主权限、rename 换上；旧文件先硬链接留着
fn install_file(f: &Install, ids: Option<(u32, u32)>, suffix: &str, j: &mut Journal) -> anyhow::Result<()> {
    let parent = f.dst.parent().context("目标没有上级目录")?;
    if !parent.is_dir() {
        // 只为 Caddy 的目录、/etc/ssh 建上级目录；/etc/fleet 等由部署建（§10 第 1 步）
        if f.who == Who::Root {
            let mut missing = vec![];
            let mut p = parent;
            while !p.is_dir() {
                missing.push(p.to_path_buf());
                p = p.parent().context("上级目录")?;
            }
            for d in missing.into_iter().rev() {
                super::mkdir_mode(&d, 0o755).with_context(|| format!("建目录 {}", d.display()))?;
                j.dirs.push(d);
            }
        } else {
            bail!("{} 不存在：先按 §10 第 1 步部署一次", parent.display());
        }
    }
    let name = f.dst.file_name().context("目标没有文件名")?.to_string_lossy().into_owned();
    let tmp = parent.join(format!(".{name}.restore-tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut out = o.open(&tmp).with_context(|| format!("建 {}", tmp.display()))?;
    let res = (|| -> anyhow::Result<()> {
        std::io::copy(&mut std::fs::File::open(&f.src)?, &mut out)?;
        #[cfg(unix)]
        {
            if let Some((u, g)) = ids {
                std::os::unix::fs::fchown(&out, Some(u), Some(g)).context("改属主")?;
            }
            use std::os::unix::fs::PermissionsExt;
            out.set_permissions(std::fs::Permissions::from_mode(f.mode)).context("改权限")?;
        }
        out.sync_all()?;
        Ok(())
    })();
    drop(out);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.context(format!("写 {}", tmp.display())));
    }
    let old = if f.dst.exists() {
        let o = parent.join(format!("{name}.{suffix}"));
        if let Err(e) = std::fs::hard_link(&f.dst, &o) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("留旧文件 {}", o.display()));
        }
        Some(o)
    } else {
        None
    };
    if let Err(e) = std::fs::rename(&tmp, &f.dst) {
        let _ = std::fs::remove_file(&tmp);
        if let Some(o) = &old {
            let _ = std::fs::remove_file(o);
        }
        return Err(e).with_context(|| format!("换上 {}", f.dst.display()));
    }
    j.done.push((f.dst.clone(), old));
    Ok(())
}

/// 恢复完成的结果
#[derive(Debug)]
pub struct Restored {
    pub safety_backup: PathBuf,
    pub old_db: Option<String>,
    pub replaced: Vec<PathBuf>,
    pub kept_old: Vec<PathBuf>,
    pub toml_copy: PathBuf,
    pub sshd_or_caddy: bool,
    pub backup_pub: bool,
    pub ssh_host_key: bool,
}

/// fleet-server 是否在跑：listen 地址被占着
pub fn serving(listen: &str) -> anyhow::Result<bool> {
    let addr: std::net::SocketAddr = listen.parse().with_context(|| format!("fleet.toml 的 listen 不是地址：{listen}"))?;
    match std::net::TcpListener::bind(addr) {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Ok(true),
        Err(e) => Err(e).with_context(|| format!("检查 {listen} 有没有被占用")),
    }
}

/// 真正恢复（[`prepare`] 之后）。失败时系统回到恢复之前（安全备份留着）
pub async fn apply(p: &Prepared, env: &Env, now: OffsetDateTime) -> anyhow::Result<Restored> {
    if env.safety.rotate {
        bail!("安全备份不能轮换（会删掉旧备份）");
    }
    if p.current_db.is_none() {
        bail!("现在没有库 {}：先按 §10 第 1 步部署一次（install.sh 建库与角色），再恢复", p.db);
    }
    if serving(&env.listen)? {
        bail!("fleet-server 在跑（{} 被占用）：§10 第 3 步先 systemctl stop fleet-server，再重跑这条命令", env.listen);
    }
    let conns = env.pg.psql(&format!("select count(*) from pg_stat_activity where datname = '{}'", p.db)).await?;
    if conns != "0" {
        bail!("还有 {conns} 个连接连着库 {}（fleet-server 没停干净？psql 开着？）：换不了库", p.db);
    }
    {
        // 装上信号处理：从这里起 Ctrl-C / SIGTERM 不直接杀进程，在步骤之间中止并回滚
        STOP.store(false, Ordering::SeqCst);
        let sig = super::stop_signal();
        tokio::spawn(async move {
            let s = sig.await;
            eprintln!("收到 {s}：做完当前这一步就中止并回滚");
            STOP.store(true, Ordering::SeqCst);
        });
    }

    // 5. 安全备份
    println!("== 先做安全备份（恢复前的库与密钥）");
    let safety = backup::run(&env.safety, now).await.context("安全备份没做成，什么都没改")?;
    println!("{}", safety.summary());
    check_stop()?;

    // 6. 新库：恢复 + 迁移
    let tmp_db = p.restore_db();
    super::check_ident("库名", &tmp_db)?;
    println!("== 恢复进新库 {tmp_db}");
    env.pg.psql(&format!("create database {tmp_db} owner {}", p.db_owner)).await.context("建新库")?;
    let mut journal = Journal::default();
    let r = async {
        let mut c = env.pg.cmd("pg_restore");
        c.args(["-d", &tmp_db, "--exit-on-error"]).stdin(std::fs::File::open(p.tree.join("fleet.dump"))?);
        run_ok(c, "pg_restore").await?;
        check_stop()?;
        println!("== 跑迁移");
        let opts: sqlx::postgres::PgConnectOptions = env.database_url.parse().context("database_url")?;
        let pool = sqlx::postgres::PgPoolOptions::new().max_connections(2).connect_with(opts.database(&tmp_db)).await.context("以 fleet.toml 的账号连新库")?;
        let mr = crate::db::MIGRATOR.run(&pool).await.context("在新库上跑迁移");
        pool.close().await;
        mr?;
        check_stop()?;

        // 7. 文件
        println!("== 放回密钥与主机指纹");
        let suffix = p.file_suffix();
        for f in p.files.iter().filter(|f| f.change != Change::Same) {
            install_file(f, env.owners.ids(f.who)?, &suffix, &mut journal)?;
            println!("  {} {}", if f.change == Change::New { "新建" } else { "替换" }, f.dst.display());
        }
        if p.tree.join("etc/fleet/fleet.toml").is_file() {
            let toml = Install {
                rel: "etc/fleet/fleet.toml".into(),
                src: p.tree.join("etc/fleet/fleet.toml"),
                dst: p.toml_copy(&env.root),
                mode: 0o600,
                who: Who::Root,
                change: Change::New,
            };
            if toml.dst.exists() {
                bail!("{} 已存在", toml.dst.display());
            }
            install_file(&toml, env.owners.ids(Who::Root)?, &suffix, &mut journal)?;
        }
        check_stop()?;

        // 8. 换库（最后一步，一个事务）
        println!("== 换库");
        let old = p.current_db.is_some().then(|| p.old_db());
        let sql = match &old {
            Some(o) => format!("alter database {} rename to {o}; alter database {tmp_db} rename to {}", p.db, p.db),
            None => format!("alter database {tmp_db} rename to {}", p.db),
        };
        env.pg.psql(&sql).await.context("换库")?;
        Ok::<_, anyhow::Error>(old)
    }
    .await;
    match r {
        Ok(old_db) => {
            let replaced: Vec<PathBuf> = journal.done.iter().map(|(d, _)| d.clone()).collect();
            let kept_old: Vec<PathBuf> = journal.done.iter().filter_map(|(_, o)| o.clone()).collect();
            let changed = |pre: &str| p.files.iter().any(|f| f.change != Change::Same && f.rel.starts_with(pre));
            Ok(Restored {
                safety_backup: safety.path,
                old_db,
                replaced,
                kept_old,
                toml_copy: p.toml_copy(&env.root),
                sshd_or_caddy: changed("etc/ssh/") || changed("etc/caddy/"),
                backup_pub: p.files.iter().any(|f| f.rel == "etc/fleet/backup.pub"),
                ssh_host_key: p.files.iter().any(|f| f.rel == "etc/ssh/ssh_host_ed25519_key"),
            })
        }
        Err(e) => {
            let problems = journal.rollback();
            let drop_r = env.pg.psql(&format!("drop database if exists {tmp_db}")).await;
            let mut msg = format!("恢复失败，已回到恢复之前（库 {} 与文件都是原来的；安全备份 {} 留着）", p.db, safety.path.display());
            if !problems.is_empty() {
                msg = format!("恢复失败，这些文件没能换回、需要人工处理（旧文件在同目录的 *.{}）：{}", p.file_suffix(), problems.join("；"));
            }
            if let Err(de) = drop_r {
                msg.push_str(&format!("；新库 {tmp_db} 没删掉（{de:#}），手工 drop database"));
            }
            Err(e.context(msg))
        }
    }
}

/// 恢复成功后要人做的步骤（§10 第 3–10 步里这个程序没做的）
pub fn manual_steps(r: &Restored, p: &Prepared) -> String {
    let mut s = vec![];
    if r.sshd_or_caddy {
        s.push(
            "放回了中控 sshd 主机密钥 / Caddy 带外配置，重启它们（不重启，软路由隧道按旧公钥核对全断、经边缘机的连接缺 PROXY 协议配置全断）：\n     \
             sshd -t && systemctl restart ssh && caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null && systemctl restart caddy"
                .to_string(),
        );
    }
    s.push(
        "启动并确认：systemctl start fleet-server；curl -s -o /dev/null -w '%{http_code}\\n' http://127.0.0.1:8080/api/v1/auth/me 应为 401".to_string(),
    );
    s.push("重建 central-local（恢复的库里是备份时的那把）：/opt/fleet/fleet-server local-admin -c /etc/fleet/fleet.toml，再 fleetctl status 看机器".into());
    if !p.toml_diff.is_empty() {
        s.push(format!(
            "fleet.toml 和备份里不同的键：{}。对照 {} 把要保留的设置抄进 /etc/fleet/fleet.toml（database_url 用新装的），改了再 systemctl restart fleet-server",
            p.toml_diff.join("、"),
            r.toml_copy.display()
        ));
    }
    s.push("中控 IP 变了：把域名解析改到这台机器（agent 按域名上报，生效后自动恢复）".into());
    if r.backup_pub {
        s.push("放回了 etc/fleet/backup.pub：再跑一次 deploy/central/deploy.sh，install.sh 才写 PVE 拉取用户的 authorized_keys".into());
    }
    s.push("备份之后升级过 agent 的机器令牌对不上（上报一律 401）：几分钟后 fleetctl ls 里一直离线的逐台 fleetctl upgrade-agent <机器> --yes".into());
    if !r.ssh_host_key {
        s.push("备份里没有 sshd 主机密钥，本机指纹变了：PVE 上 ssh-keygen -R <中控主机>，再 systemctl start fleet-backup-pull 记下新指纹".into());
    }
    let mut cleanup = vec![];
    if let Some(o) = &r.old_db {
        cleanup.push(format!("runuser -u postgres -- psql -c 'drop database {o}'"));
    }
    if !r.kept_old.is_empty() {
        cleanup.push(format!("rm -f {}", r.kept_old.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(" ")));
    }
    cleanup.push(format!("rm -f {}", r.toml_copy.display()));
    s.push(format!("确认都正常后清理恢复前的库与旧文件（里面有旧密钥）：{}", cleanup.join("；")));
    s.push("加密备份的私钥若临时拷到了中控：shred -u <私钥文件>".into());
    s.iter().enumerate().map(|(i, x)| format!("  {}. {x}", i + 1)).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_names() {
        for (n, d) in [
            ("fleet.dump", false),
            ("etc/fleet/", true),
            ("etc/fleet/secret.key", false),
            ("etc/fleet/ssh/", true),
            ("etc/fleet/ssh/id_ed25519", false),
            ("var/lib/fleet/known_hosts", false),
            ("etc/ssh/ssh_host_ed25519_key", false),
            ("etc/ssh/ssh_host_ed25519_key.pub", false),
            ("etc/caddy/global.d/", true),
            ("etc/caddy/sites.d/edge-proxy.caddy", false),
            ("etc/caddy/sites.d/sub/x.caddy", false),
        ] {
            assert!(member_ok(n, d).is_ok(), "{n}");
        }
        for (n, d) in [
            ("/etc/fleet/secret.key", false),
            ("etc/fleet/../../root/.ssh/authorized_keys", false),
            ("./fleet.dump", false),
            ("etc/passwd", false),
            ("etc/ssh/sshd_config", false),
            ("etc/caddy/Caddyfile", false),
            ("var/lib/fleet/ipintel/x", false),
            ("etc", false),
            ("fleet.dump", true),
            ("etc//fleet/x", false),
            ("", false),
        ] {
            assert!(member_ok(n, d).is_err(), "{n}");
        }
    }

    #[test]
    fn migrations_copy_parse_and_check() {
        let sql = "--\n-- PostgreSQL database dump\n--\nSET x = 1;\n\
            COPY public._sqlx_migrations (version, description, installed_on, success, checksum, execution_time) FROM stdin;\n\
            1\tinit\t2026-01-01 00:00:00+00\tt\t\\\\x0a0b\t123\n\
            2\tmore\t2026-01-02 00:00:00+00\tt\t\\\\xff00\t5\n\
            \\.\n\n";
        let a = parse_migrations_copy(sql).unwrap();
        assert_eq!(a, vec![Applied { version: 1, success: true, checksum: vec![10, 11] }, Applied { version: 2, success: true, checksum: vec![255, 0] }]);
        let bin = vec![(1, vec![10, 11]), (2, vec![255, 0]), (3, vec![1])];
        assert_eq!(check_migrations(&a, &bin).unwrap(), vec![3]);
        // 备份比本程序新
        let e = check_migrations(&a, &bin[..1]).unwrap_err().to_string();
        assert!(e.contains("本程序没有的迁移 2"), "{e}");
        // 校验和不同
        let e = check_migrations(&a, &[(1, vec![10, 11]), (2, vec![1])]).unwrap_err().to_string();
        assert!(e.contains("迁移 2 的校验和"), "{e}");
        // 没跑成功
        let bad = vec![Applied { version: 1, success: false, checksum: vec![10, 11] }];
        assert!(check_migrations(&bad, &bin).unwrap_err().to_string().contains("没有跑成功"));
        // 没有 COPY 段 / 没结束
        assert!(parse_migrations_copy("SET x = 1;\n").is_err());
        assert!(parse_migrations_copy("COPY public._sqlx_migrations (version, success, checksum) FROM stdin;\n1\tt\t\\\\x00\n").is_err());
        // 真实迁移：本程序自己的版本与校验和互相一致
        let own: Vec<Applied> = binary_migrations().into_iter().map(|(version, checksum)| Applied { version, success: true, checksum }).collect();
        assert!(check_migrations(&own, &binary_migrations()).unwrap().is_empty());
    }

    #[test]
    fn toml_differences() {
        let a = "listen = \"127.0.0.1:8080\"\ndatabase_url = \"postgres://fleet:old@127.0.0.1/fleet\"\n[retention]\nraw_days = 5\n[selfmon]\nheartbeat_url = \"https://hc/abc\"\n";
        let b = "listen = \"127.0.0.1:8080\"\ndatabase_url = \"postgres://fleet:new@127.0.0.1/fleet\"\n[retention]\nraw_days = 3\naccess_days = 7\n";
        assert_eq!(toml_diff(a, b), vec!["retention.access_days", "retention.raw_days", "selfmon.heartbeat_url"]);
        assert!(toml_diff(b, b).is_empty());
    }

    #[test]
    fn stamps() {
        assert_eq!(stamp(time::macros::datetime!(2026-11-21 19:30:05 UTC)), "20261122_033005");
    }

    #[test]
    fn serving_detects_bound_port() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap().to_string();
        assert!(serving(&a).unwrap());
        drop(l);
        assert!(!serving(&a).unwrap());
        assert!(serving("nonsense").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn install_and_rollback() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let dst_dir = t.path().join("etc/fleet");
        std::fs::create_dir_all(&dst_dir).unwrap();
        std::fs::write(src.join("a"), b"new-a").unwrap();
        std::fs::write(src.join("b"), b"new-b").unwrap();
        std::fs::write(dst_dir.join("a"), b"old-a").unwrap();
        let mk = |n: &str, dst: PathBuf, who| Install { rel: n.into(), src: src.join(n), dst, mode: 0o600, who, change: Change::Replace };
        let mut j = Journal::default();
        install_file(&mk("a", dst_dir.join("a"), Who::Fleet), None, "pre-restore-x", &mut j).unwrap();
        install_file(&mk("b", t.path().join("etc/caddy/sites.d/b"), Who::Root), None, "pre-restore-x", &mut j).unwrap();
        assert_eq!(std::fs::read(dst_dir.join("a")).unwrap(), b"new-a");
        assert_eq!(std::fs::read(dst_dir.join("a.pre-restore-x")).unwrap(), b"old-a");
        assert_eq!(std::fs::read(t.path().join("etc/caddy/sites.d/b")).unwrap(), b"new-b");
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dst_dir.join("a")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // 不是 Root 的文件，上级目录不存在就拒绝（要先部署）
        assert!(install_file(&mk("b", t.path().join("var/lib/fleet/known_hosts"), Who::Fleet), None, "pre-restore-x", &mut j).is_err());
        // 目标是符号链接：建临时文件不跟链接
        let problems = j.rollback();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(std::fs::read(dst_dir.join("a")).unwrap(), b"old-a");
        assert!(!dst_dir.join("a.pre-restore-x").exists());
        assert!(!t.path().join("etc/caddy").exists(), "新建的目录也撤掉");
        assert!(!std::fs::read_dir(&dst_dir).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("restore-tmp")));
    }
}
