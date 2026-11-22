//! `fleet-server backup`：中控每天的备份（fleet-backup.timer 触发，root 执行；也可手动 systemctl start fleet-backup）。
//! 取代 deploy/backup/fleet-backup.sh，产物逐项相同，家里 PVE 的拉取（fleet-backup-latest.sh / fleet-backup-pull.sh）与
//! 中控自己的 `central_backup_stale` 告警（selfmon.rs 看备份目录里最新一份 fleet-*.tar / .tar.age 的 mtime）照旧：
//!
//! - 一份备份是一个 tar：`fleet.dump`（`pg_dump -Fc`，四张原始时序表只备结构）+ `etc/fleet` + `var/lib/fleet/known_hosts`，
//!   再加存在才打的 `etc/ssh/ssh_host_ed25519_key`(.pub)、`etc/caddy/global.d`、`etc/caddy/sites.d`。tar 照旧由 GNU tar 打（成员顺序、属主、权限与脚本一致）。
//! - `/etc/fleet/backup-age.recipients` 存在且非空时 tar 经管道交给 `age -R` 加密后才落盘，存成 `.tar.age`。
//! - 名字 `fleet-<北京时间 YYYYmmdd-HHMMSS>.tar[.age]`，旁边 `<名字>.sha256`（sha256sum 格式）；两个文件 0640 root:fleet-backup。
//!   先在备份目录里的 `.work-XXXXXX` 做好，校验和先 rename 就位、再 rename 备份：拉取时看得到的备份一定有它的校验和。
//! - 轮换（[`rotation_victims`]）：每个北京日期只留最后一份，只留最近 14 个日期；另外总是保留最新 3 份；别的名字不动。
//! - 任何一步失败：退出码非 0，`.work-*` 连同里面的半成品删掉，备份目录里已有的不动。
use super::{exit_text, human_size, sha256_file, PgTools, WorkDir};
use anyhow::{bail, Context};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use time::{OffsetDateTime, UtcOffset};

/// 保留最近多少个北京日期（每个日期最后一份）
pub const KEEP_DAYS: usize = 14;
/// 另外总是保留最新几份
pub const KEEP_LATEST: usize = 3;
/// 只备结构不备数据的表（按天分区、只保留几天、量最大）：`--exclude-table-data-and-children`
pub const EXCLUDE_DATA: [&str; 4] = ["host_metric", "entry_traffic", "probe_result", "access_event"];
/// 一定要打进包的路径（相对根目录；不存在时 tar 失败、备份失败，同脚本）
pub const REQUIRED: [&str; 2] = ["etc/fleet", "var/lib/fleet/known_hosts"];
/// 存在才打的路径（tar 遇到不存在的路径会失败）
pub const OPTIONAL: [&str; 4] = ["etc/ssh/ssh_host_ed25519_key", "etc/ssh/ssh_host_ed25519_key.pub", "etc/caddy/global.d", "etc/caddy/sites.d"];
/// age 公钥清单（相对根目录）
pub const RECIPIENTS: &str = "etc/fleet/backup-age.recipients";
/// 备份文件的属组：PVE 拉取用的 fleet-backup 用户要能读
pub const GROUP: &str = "fleet-backup";
/// age 文件头（PVE 拉取按它判断是不是密文）
pub const AGE_MAGIC: &[u8] = b"age-encryption.org/v1";

/// 一次备份怎么做。生产用 [`Plan::system`]；测试换根目录、工具和属组
#[derive(Clone, Debug)]
pub struct Plan {
    /// 备份目录（/var/backups/fleet；与告警检查的 `[selfmon] backup_dir` 是同一个）
    pub dir: PathBuf,
    /// 要打包的文件所在的根（生产 /）
    pub root: PathBuf,
    /// 库名（database_url 里的，生产 fleet）
    pub db: String,
    pub pg: PgTools,
    pub tar: String,
    pub age: String,
    /// 备份文件改成这个属组；None 不改（测试）
    pub group: Option<String>,
    /// 做完是否轮换（恢复前的安全备份不轮换：不删任何旧备份，包括正要恢复的那份）
    pub rotate: bool,
}

impl Plan {
    pub fn system(dir: PathBuf, db: String) -> Plan {
        Plan { dir, root: PathBuf::from("/"), db, pg: PgTools::system(), tar: "tar".into(), age: "age".into(), group: Some(GROUP.into()), rotate: true }
    }
}

#[derive(Debug)]
pub struct Done {
    pub path: PathBuf,
    pub bytes: u64,
    pub encrypted: bool,
    /// 轮换删掉的（文件名）
    pub removed: Vec<String>,
    /// 做完后目录里的备份份数（同脚本最后一行的「共 N 份」）
    pub total: usize,
}

impl Done {
    /// 同脚本的最后一行
    pub fn summary(&self) -> String {
        format!("已备份 {}（{}），共 {} 份", self.path.display(), human_size(self.bytes), self.total)
    }
}

/// 备份名：`fleet-<北京时间>.tar`（加密的再加 `.age`）
pub fn backup_name(now: OffsetDateTime, encrypted: bool) -> String {
    let bj = now.to_offset(UtcOffset::from_hms(8, 0, 0).expect("+08:00"));
    let f = time::macros::format_description!("[year][month][day]-[hour][minute][second]");
    format!("fleet-{}.tar{}", bj.format(&f).expect("格式化时间"), if encrypted { ".age" } else { "" })
}

/// 轮换认的名字 `fleet-<8 位日期>-<6 位时间>.tar[.age]`：返回日期部分
pub fn rotation_day(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("fleet-")?;
    let b = rest.as_bytes();
    if b.len() < 15 || !b[..8].iter().all(u8::is_ascii_digit) || b[8] != b'-' || !b[9..15].iter().all(u8::is_ascii_digit) {
        return None;
    }
    matches!(&rest[15..], ".tar" | ".tar.age").then(|| &rest[..8])
}

/// 脚本里 `list` 列出的名字：`fleet-*.tar*` 且不以 `.sha256` 结尾（份数就按它数）
pub fn is_listed(name: &str) -> bool {
    name.strip_prefix("fleet-").is_some_and(|r| r.contains(".tar")) && !name.ends_with(".sha256")
}

/// 轮换要删的备份（文件名；调用方连同 `<名字>.sha256` 一起删）。与 fleet-backup.sh 文末逐条相同：
/// 按名字排序；每个日期只认排在最后的一份，留最近 `keep_days` 个日期的这一份；另外留排在最后的 `keep_latest` 份；
/// 不是 `fleet-<日期>-<时间>.tar[.age]` 的名字不动
pub fn rotation_victims(names: &[String], keep_days: usize, keep_latest: usize) -> Vec<String> {
    let mut cand: Vec<&str> = names.iter().map(String::as_str).filter(|n| is_listed(n) && rotation_day(n).is_some()).collect();
    cand.sort_unstable();
    cand.dedup();
    let mut last_of_day: std::collections::BTreeMap<&str, &str> = Default::default();
    for n in &cand {
        last_of_day.insert(rotation_day(n).unwrap(), n);
    }
    let mut keep: std::collections::HashSet<&str> = last_of_day.values().rev().take(keep_days).copied().collect();
    keep.extend(cand.iter().rev().take(keep_latest).copied());
    cand.into_iter().filter(|n| !keep.contains(n)).map(String::from).collect()
}

/// 做一份备份
pub async fn run(p: &Plan, now: OffsetDateTime) -> anyhow::Result<Done> {
    super::check_ident("库名", &p.db)?;
    if !p.dir.is_dir() {
        bail!("备份目录 {} 不存在（install.sh 建它：install -d -m 755 -o root -g fleet-backup）", p.dir.display());
    }
    let gid = match &p.group {
        Some(g) => Some(super::lookup_group(g).with_context(|| format!("没有 {g} 组（install.sh 建 fleet-backup 用户时一起建）"))?),
        None => None,
    };
    let rec = p.root.join(RECIPIENTS);
    let encrypted = std::fs::metadata(&rec).map(|m| m.len() > 0).unwrap_or(false);
    let name = backup_name(now, encrypted);
    let work = WorkDir::new(&p.dir, ".work-")?;
    let w = work.path();

    // 1. 库：pg_dump 以 postgres 身份写到标准输出，落在临时目录里
    let dump = std::fs::File::create(w.join("fleet.dump")).context("建 fleet.dump")?;
    let mut args = vec!["-Fc".to_string(), "-d".into(), p.db.clone()];
    args.extend(EXCLUDE_DATA.iter().map(|t| format!("--exclude-table-data-and-children={t}")));
    let st = p.pg.cmd("pg_dump").args(&args).stdout(Stdio::from(dump)).status().await.context("执行 pg_dump")?;
    if !st.success() {
        bail!("pg_dump 失败（{}），没有备份", exit_text(st));
    }

    // 2. tar：路径与顺序同脚本；可选路径存在才打
    let mut members: Vec<String> = REQUIRED.iter().map(|s| s.to_string()).collect();
    members.extend(OPTIONAL.iter().filter(|o| p.root.join(o).exists()).map(|s| s.to_string()));
    let out = w.join(&name);
    let mut tar = tokio::process::Command::new(&p.tar);
    tar.kill_on_drop(true).stdin(Stdio::null()).stderr(Stdio::inherit());
    tar.arg("-cf").arg(if encrypted { "-".into() } else { out.clone() });
    tar.arg("-C").arg(w).arg("fleet.dump").arg("-C").arg(&p.root).args(&members);
    if encrypted {
        // tar | age -R <清单> > 备份：明文不落盘；两边任何一个失败都算失败（脚本的 pipefail）
        let f = std::fs::File::create(&out).context("建加密备份文件")?;
        let mut t = tar.stdout(Stdio::piped()).spawn().context("执行 tar")?;
        let pipe: Stdio = t.stdout.take().expect("tar stdout").try_into().context("tar 的管道")?;
        let mut a = tokio::process::Command::new(&p.age);
        a.kill_on_drop(true).arg("-R").arg(&rec).stdin(pipe).stdout(Stdio::from(f)).stderr(Stdio::inherit());
        let mut a = a.spawn().context("执行 age（中控要装 age 包）")?;
        let (ts, as_) = tokio::join!(t.wait(), a.wait());
        let (ts, as_) = (ts?, as_?);
        if !ts.success() {
            bail!("tar 失败（{}），没有备份", exit_text(ts));
        }
        if !as_.success() {
            bail!("age 加密失败（{}），没有备份：检查 {}", exit_text(as_), rec.display());
        }
    } else {
        let st = tar.status().await.context("执行 tar")?;
        if !st.success() {
            bail!("tar 失败（{}），没有备份", exit_text(st));
        }
    }

    // 3. 校验和、属组、权限（fleet-backup 用户要能读）
    let sum = sha256_file(&out).context("算 sha256")?;
    let sum_file = w.join(format!("{name}.sha256"));
    std::fs::write(&sum_file, format!("{sum}  {name}\n")).context("写 .sha256")?;
    for f in [&out, &sum_file] {
        #[cfg(unix)]
        if let Some(gid) = gid {
            std::os::unix::fs::chown(f, None, Some(gid)).with_context(|| format!("改属组 {}", f.display()))?;
        }
        super::set_mode(f, 0o640).with_context(|| format!("改权限 {}", f.display()))?;
    }
    let bytes = std::fs::metadata(&out)?.len();

    // 4. 就位：校验和先，备份后。备份 rename 失败时把刚放上去的校验和撤掉（不留孤儿）
    let final_sum = p.dir.join(format!("{name}.sha256"));
    let final_out = p.dir.join(&name);
    std::fs::rename(&sum_file, &final_sum).with_context(|| format!("放上 {}", final_sum.display()))?;
    if let Err(e) = std::fs::rename(&out, &final_out) {
        let _ = std::fs::remove_file(&final_sum);
        return Err(e).with_context(|| format!("放上 {}", final_out.display()));
    }
    drop(work);

    // 5. 轮换
    let mut removed = vec![];
    let names = list_names(&p.dir)?;
    if p.rotate {
        for v in rotation_victims(&names, KEEP_DAYS, KEEP_LATEST) {
            for f in [p.dir.join(&v), p.dir.join(format!("{v}.sha256"))] {
                match std::fs::remove_file(&f) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("轮换时删 {}", f.display())),
                }
            }
            removed.push(v);
        }
    }
    let total = list_names(&p.dir)?.iter().filter(|n| is_listed(n)).count();
    Ok(Done { path: final_out, bytes, encrypted, removed, total })
}

fn list_names(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut v = vec![];
    for e in std::fs::read_dir(dir).with_context(|| format!("读备份目录 {}", dir.display()))? {
        if let Some(n) = e?.file_name().to_str() {
            v.push(n.to_string());
        }
    }
    v.sort();
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> { v.iter().map(|x| x.to_string()).collect() }

    #[test]
    fn names_are_beijing_time() {
        let t = time::macros::datetime!(2026-11-21 19:30:05 UTC);
        assert_eq!(backup_name(t, false), "fleet-20261122-033005.tar");
        assert_eq!(backup_name(t, true), "fleet-20261122-033005.tar.age");
    }

    #[test]
    fn rotation_name_pattern() {
        assert_eq!(rotation_day("fleet-20261122-033005.tar"), Some("20261122"));
        assert_eq!(rotation_day("fleet-20261122-033005.tar.age"), Some("20261122"));
        for bad in [
            "fleet-20261122-033005.tar.sha256",
            "fleet-20261122-033005.tar.bak",
            "fleet-2026112-033005.tar",
            "fleet-20261122-03300.tar",
            "fleet-prod-20261122-033005.tar",
            "fleet-20261122_033005.tar",
            "xfleet-20261122-033005.tar",
            "fleet-20261122-033005.tgz",
        ] {
            assert_eq!(rotation_day(bad), None, "{bad}");
        }
        assert!(is_listed("fleet-x.tar.bak") && is_listed("fleet-prod-1.tar") && !is_listed("fleet-1.tar.sha256") && !is_listed(".work-abc"));
    }

    /// 连续 20 天每天 03:30 一份：留最近 14 天
    #[test]
    fn keeps_last_14_days() {
        let names: Vec<String> = (1..=20).flat_map(|d| [format!("fleet-202611{d:02}-033000.tar"), format!("fleet-202611{d:02}-033000.tar.sha256")]).collect();
        let v = rotation_victims(&names, KEEP_DAYS, KEEP_LATEST);
        assert_eq!(v, (1..=6).map(|d| format!("fleet-202611{d:02}-033000.tar")).collect::<Vec<_>>());
    }

    /// 同一天几份：只留这一天最后一份；但最新 3 份总是留着（部署前的同步备份不会被当天后来的定时备份马上顶掉）
    #[test]
    fn same_day_keeps_last_but_latest_three_survive() {
        let names = s(&[
            "fleet-20261120-033000.tar",
            "fleet-20261121-033000.tar",
            "fleet-20261121-101500.tar",
            "fleet-20261121-120000.tar.age",
            "fleet-20261121-180000.tar.age",
            "fleet-20261122-033000.tar.age",
        ]);
        let v = rotation_victims(&names, KEEP_DAYS, KEEP_LATEST);
        // 21 日最后一份 18:00；最新 3 份 = 21 日 12:00、18:00、22 日；20 日是那天最后一份，在 14 天里
        assert_eq!(v, s(&["fleet-20261121-033000.tar", "fleet-20261121-101500.tar"]));
    }

    /// 日期按「有备份的日期」数，不按日历：中间停了几周，旧的照样留够 14 个日期
    #[test]
    fn counts_days_with_backups_not_calendar() {
        let mut names: Vec<String> = (1..=10).map(|d| format!("fleet-202609{d:02}-033000.tar")).collect();
        names.extend((1..=6).map(|d| format!("fleet-202611{d:02}-033000.tar")));
        let v = rotation_victims(&names, KEEP_DAYS, KEEP_LATEST);
        assert_eq!(v, s(&["fleet-20260901-033000.tar", "fleet-20260902-033000.tar"]));
    }

    /// 别的名字不动：PVE 的 fleet-prod-*、手工改名的 .bak、孤儿 .sha256、临时目录
    #[test]
    fn foreign_names_untouched() {
        let mut names: Vec<String> = (1..=20).map(|d| format!("fleet-202611{d:02}-033000.tar")).collect();
        names.extend(s(&["fleet-20261101-033000.tar.bak", "fleet-prod-20261101-033000.tar", "fleet-20261101-000000.tar.sha256", ".work-abc123", "README"]));
        let v = rotation_victims(&names, KEEP_DAYS, KEEP_LATEST);
        assert_eq!(v.len(), 6);
        assert!(v.iter().all(|n| rotation_day(n).is_some()));
    }

    /// 同一时刻的明文与密文（开加密那天手工各做了一份）：按名字排序 .tar 在 .tar.age 前，那天留 .tar.age
    #[test]
    fn plain_and_age_same_second() {
        let mut names: Vec<String> = (1..=16).map(|d| format!("fleet-202611{d:02}-033000.tar")).collect();
        names.push("fleet-20261116-033000.tar.age".into());
        let v = rotation_victims(&names, KEEP_DAYS, KEEP_LATEST);
        // 16 日最后一份是 .tar.age；最新 3 份 = 15 日 .tar、16 日 .tar、16 日 .tar.age（都留）；14 个日期 = 3..16 日
        assert_eq!(v, s(&["fleet-20261101-033000.tar", "fleet-20261102-033000.tar"]));
    }

    #[test]
    fn few_backups_nothing_removed() {
        assert!(rotation_victims(&s(&["fleet-20261122-033000.tar"]), KEEP_DAYS, KEEP_LATEST).is_empty());
        assert!(rotation_victims(&[], KEEP_DAYS, KEEP_LATEST).is_empty());
    }

    // ── 用假的 pg_dump / age 跑整条流程（GNU tar 是真的）。生产路径用真的 pg_dump：见 tests/it/ops.rs 的往返测试 ──

    struct Fx {
        _t: tempfile::TempDir,
        root: PathBuf,
        dir: PathBuf,
        bin: PathBuf,
    }

    #[cfg(unix)]
    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        set_mode_x(path);
    }
    #[cfg(unix)]
    fn set_mode_x(p: &Path) { super::super::set_mode(p, 0o755).unwrap(); }

    #[cfg(unix)]
    fn fx(dump: &str) -> Fx {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("root");
        let dir = t.path().join("backups");
        let bin = t.path().join("bin");
        for d in [root.join("etc/fleet/ssh"), root.join("var/lib/fleet"), root.join("etc/caddy/sites.d"), dir.clone(), bin.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(root.join("etc/fleet/secret.key"), [7u8; 32]).unwrap();
        std::fs::write(root.join("etc/fleet/fleet.toml"), "listen = \"127.0.0.1:8080\"\n").unwrap();
        std::fs::write(root.join("etc/fleet/ssh/id_ed25519"), "key").unwrap();
        std::fs::write(root.join("var/lib/fleet/known_hosts"), "h1 ssh-ed25519 AAAA\n").unwrap();
        std::fs::write(root.join("etc/caddy/sites.d/x.caddy"), "x\n").unwrap();
        // 假 pg_dump：记下参数，输出给定内容（或失败）
        script(&bin.join("pg_dump"), &format!("echo \"$@\" > \"{}/args\"\n{dump}", t.path().display()));
        // 假 age：写 age 文件头，后面原样接上明文
        script(&bin.join("age"), "[ \"$1\" = -R ] || exit 9\nprintf 'age-encryption.org/v1\\n'; cat");
        Fx { root, dir, bin, _t: t }
    }

    #[cfg(unix)]
    fn plan(f: &Fx) -> Plan {
        Plan {
            dir: f.dir.clone(),
            root: f.root.clone(),
            db: "fleet".into(),
            // 假 pg_dump 经 env 改过的 PATH 找到：只对这个子进程生效，不动测试进程的环境
            pg: PgTools { wrap: vec!["env".into(), format!("PATH={}:{}", f.bin.display(), std::env::var("PATH").unwrap_or_default())] },
            tar: "tar".into(),
            age: f.bin.join("age").display().to_string(),
            group: None,
            rotate: true,
        }
    }

    #[cfg(unix)]
    fn with_path<T>(_f: &Fx, run: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(run)
    }

    #[cfg(unix)]
    fn tar_list(p: &Path) -> Vec<String> {
        let o = std::process::Command::new("tar").arg("-tf").arg(p).output().unwrap();
        assert!(o.status.success());
        String::from_utf8(o.stdout).unwrap().lines().map(String::from).collect()
    }

    #[cfg(unix)]
    #[test]
    fn plain_backup_layout_checksum_and_rotation() {
        let f = fx("printf 'PGDMP-fake'");
        // 已有 16 天的旧备份（带校验和）与一个别人的文件
        for d in 1..=16 {
            std::fs::write(f.dir.join(format!("fleet-202610{d:02}-033000.tar")), b"old").unwrap();
            std::fs::write(f.dir.join(format!("fleet-202610{d:02}-033000.tar.sha256")), b"x").unwrap();
        }
        std::fs::write(f.dir.join("notes.txt"), b"keep").unwrap();
        let now = time::macros::datetime!(2026-11-21 19:30:00 UTC);
        let done = with_path(&f, run(&plan(&f), now)).unwrap();
        assert_eq!(done.path, f.dir.join("fleet-20261122-033000.tar"));
        assert!(!done.encrypted);
        let args = std::fs::read_to_string(f._t.path().join("args")).unwrap();
        assert_eq!(
            args.trim(),
            "-Fc -d fleet --exclude-table-data-and-children=host_metric --exclude-table-data-and-children=entry_traffic \
             --exclude-table-data-and-children=probe_result --exclude-table-data-and-children=access_event"
        );
        let m = tar_list(&done.path);
        assert_eq!(m[0], "fleet.dump");
        for want in ["etc/fleet/", "etc/fleet/secret.key", "etc/fleet/ssh/id_ed25519", "var/lib/fleet/known_hosts", "etc/caddy/sites.d/", "etc/caddy/sites.d/x.caddy"] {
            assert!(m.iter().any(|x| x == want), "缺 {want}：{m:?}");
        }
        assert!(!m.iter().any(|x| x.starts_with("etc/ssh") || x.starts_with("etc/caddy/global.d")), "不存在的可选路径不打：{m:?}");
        // 校验和：sha256sum 格式，按备份名
        let sum = std::fs::read_to_string(f.dir.join("fleet-20261122-033000.tar.sha256")).unwrap();
        assert_eq!(sum, format!("{}  fleet-20261122-033000.tar\n", sha256_file(&done.path).unwrap()));
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&done.path).unwrap().permissions().mode() & 0o777, 0o640);
        }
        // 轮换：17 个日期留 14 个，删 10/01–10/03（连同 .sha256），别的文件不动，临时目录不留
        assert_eq!(done.removed, s(&["fleet-20261001-033000.tar", "fleet-20261002-033000.tar", "fleet-20261003-033000.tar"]));
        assert!(!f.dir.join("fleet-20261001-033000.tar.sha256").exists());
        assert!(f.dir.join("fleet-20261004-033000.tar.sha256").exists() && f.dir.join("notes.txt").exists());
        assert_eq!(done.total, 14);
        assert!(!std::fs::read_dir(&f.dir).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".work-")));
        assert_eq!(done.summary(), format!("已备份 {}（{}），共 14 份", done.path.display(), human_size(done.bytes)));
    }

    #[cfg(unix)]
    #[test]
    fn encrypted_when_recipients_nonempty() {
        let f = fx("printf 'PGDMP-fake'");
        std::fs::write(f.root.join(RECIPIENTS), "").unwrap();
        let now = time::macros::datetime!(2026-11-21 19:30:00 UTC);
        let d = with_path(&f, run(&plan(&f), now)).unwrap();
        assert!(!d.encrypted, "空清单不加密（同 [ -s ]）");
        std::fs::write(f.root.join(RECIPIENTS), "age1example\n").unwrap();
        let now = time::macros::datetime!(2026-11-21 19:31:00 UTC);
        let d = with_path(&f, run(&plan(&f), now)).unwrap();
        assert!(d.encrypted);
        assert_eq!(d.path.file_name().unwrap(), "fleet-20261122-033100.tar.age");
        let b = std::fs::read(&d.path).unwrap();
        assert!(b.starts_with(AGE_MAGIC));
        assert!(f.dir.join("fleet-20261122-033100.tar.age.sha256").exists());
    }

    /// 中途失败：退出码非 0，备份目录里不多任何东西（没有半个 tar、没有孤儿校验和、没有 .work-*），旧备份不被轮换
    #[cfg(unix)]
    #[test]
    fn failures_leave_nothing_behind() {
        let now = time::macros::datetime!(2026-11-21 19:30:00 UTC);
        let snapshot = |d: &Path| {
            let mut v: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            v.sort();
            v
        };
        let seed = |f: &Fx| {
            for d in 1..=16 {
                std::fs::write(f.dir.join(format!("fleet-202610{d:02}-033000.tar")), b"old").unwrap();
            }
        };
        // pg_dump 失败
        let f = fx("printf 'partial'; exit 1");
        seed(&f);
        let before = snapshot(&f.dir);
        let e = with_path(&f, run(&plan(&f), now)).unwrap_err();
        assert!(format!("{e:#}").contains("pg_dump 失败（退出码 1）"), "{e:#}");
        assert_eq!(snapshot(&f.dir), before);
        // tar 失败：必须的路径不在（known_hosts 没了）
        let f = fx("printf 'PGDMP'");
        seed(&f);
        std::fs::remove_file(f.root.join("var/lib/fleet/known_hosts")).unwrap();
        let before = snapshot(&f.dir);
        let e = with_path(&f, run(&plan(&f), now)).unwrap_err();
        assert!(format!("{e:#}").contains("tar 失败"), "{e:#}");
        assert_eq!(snapshot(&f.dir), before);
        // age 失败（管道后半段）：同样什么都不留
        let f = fx("printf 'PGDMP'");
        seed(&f);
        std::fs::write(f.root.join(RECIPIENTS), "age1example\n").unwrap();
        script(&f.bin.join("age"), "cat > /dev/null; exit 3");
        let before = snapshot(&f.dir);
        let e = with_path(&f, run(&plan(&f), now)).unwrap_err();
        assert!(format!("{e:#}").contains("age 加密失败（退出码 3）"), "{e:#}");
        assert_eq!(snapshot(&f.dir), before);
        // 属组不存在：同脚本的 chgrp 失败
        let f = fx("printf 'PGDMP'");
        let before = snapshot(&f.dir);
        let p = Plan { group: Some("no-such-group-fleet-test".into()), ..plan(&f) };
        let e = with_path(&f, run(&p, now)).unwrap_err();
        assert!(format!("{e:#}").contains("没有 no-such-group-fleet-test 组"), "{e:#}");
        assert_eq!(snapshot(&f.dir), before);
    }

    #[cfg(unix)]
    #[test]
    fn no_rotation_for_safety_backup() {
        let f = fx("printf 'PGDMP'");
        for d in 1..=20 {
            std::fs::write(f.dir.join(format!("fleet-202610{d:02}-033000.tar")), b"old").unwrap();
        }
        let p = Plan { rotate: false, ..plan(&f) };
        let d = with_path(&f, run(&p, time::macros::datetime!(2026-11-21 19:30:00 UTC))).unwrap();
        assert!(d.removed.is_empty());
        assert_eq!(d.total, 21);
    }
}
