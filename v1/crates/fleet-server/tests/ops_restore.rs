//! `fleet-server backup` → `restore` 往返：真的 pg_dump / pg_restore / psql（PostgreSQL 17）、真的 GNU tar。
//!
//! 需要一个可以随便建库删库的 PostgreSQL（超级用户），两个环境变量：
//! - `FLEET_TEST_DB`：超级用户的连接串（`postgres://fleet:fleet@127.0.0.1:<端口>/fleet_test`），跑迁移、读数据用
//! - `FLEET_TEST_PG_WRAP`：调 pg_dump / pg_restore / psql 的前缀（生产是 `runuser -u postgres --`），
//!   如 `docker exec -i -e PGUSER=fleet <容器>`：工具在库那台机器上、经本机 socket 连，和生产一样从标准输入读 dump
//!
//! 没设 `FLEET_TEST_PG_WRAP` 时整组跳过。
use fleet_server::ops::{backup, restore, PgTools};
use sqlx::{Connection, PgConnection};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

struct Pg {
    url: String,
    wrap: Vec<String>,
}

fn pg() -> Option<Pg> {
    let wrap = std::env::var("FLEET_TEST_PG_WRAP").ok()?;
    let url = std::env::var("FLEET_TEST_DB").unwrap_or_else(|_| "postgres://fleet:fleet@127.0.0.1/fleet_test".into());
    Some(Pg { url, wrap: wrap.split_whitespace().map(String::from).collect() })
}

macro_rules! need_pg {
    () => {
        match pg() {
            Some(p) => p,
            None => {
                eprintln!("跳过：没有设置 FLEET_TEST_PG_WRAP（见文件头）");
                return;
            }
        }
    };
}

fn url_for(base: &str, db: &str) -> String {
    let mut u = reqwest::Url::parse(base).unwrap();
    u.set_path(&format!("/{db}"));
    u.to_string()
}

async fn exec(url: &str, sql: &str) {
    let mut c = PgConnection::connect(url).await.unwrap();
    sqlx::raw_sql(sql).execute(&mut c).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    c.close().await.unwrap();
}

/// 库的结构（列、索引、约束）与每张表的行数
async fn snapshot(url: &str) -> (Vec<String>, BTreeMap<String, i64>) {
    let mut c = PgConnection::connect(url).await.unwrap();
    let schema: Vec<String> = sqlx::query_scalar(
        "select * from (
           select 'col ' || table_name || '.' || column_name || ' ' || data_type || ' ' || is_nullable || ' ' || coalesce(column_default, '')
             from information_schema.columns where table_schema = 'public'
           union all select 'idx ' || indexname || ' ' || indexdef from pg_indexes where schemaname = 'public'
           union all select 'con ' || conrelid::regclass::text || ' ' || conname || ' ' || pg_get_constraintdef(oid)
             from pg_constraint where connamespace = 'public'::regnamespace
           union all select 'rel ' || relname || ' ' || relkind::text from pg_class where relnamespace = 'public'::regnamespace and relkind in ('r', 'p', 'v', 'S')
         ) s(x) order by 1",
    )
    .fetch_all(&mut c)
    .await
    .unwrap();
    let tables: Vec<String> = sqlx::query_scalar("select relname::text from pg_class where relnamespace = 'public'::regnamespace and relkind = 'r' order by 1")
        .fetch_all(&mut c)
        .await
        .unwrap();
    let mut counts = BTreeMap::new();
    for t in tables {
        let n: i64 = sqlx::query_scalar(&format!("select count(*) from public.\"{t}\"")).fetch_one(&mut c).await.unwrap();
        counts.insert(t, n);
    }
    c.close().await.unwrap();
    (schema, counts)
}

async fn db_exists(base: &str, db: &str) -> bool {
    let mut c = PgConnection::connect(base).await.unwrap();
    let n: i64 = sqlx::query_scalar("select count(*) from pg_database where datname = $1").bind(db).fetch_one(&mut c).await.unwrap();
    c.close().await.unwrap();
    n == 1
}

struct Fx {
    t: tempfile::TempDir,
    pg: Pg,
    db: String,
}

impl Fx {
    fn root(&self) -> PathBuf { self.t.path().join("root") }
    fn backups(&self) -> PathBuf { self.t.path().join("backups") }
    fn db_url(&self) -> String { url_for(&self.pg.url, &self.db) }
    fn tools(&self) -> PgTools { PgTools { wrap: self.pg.wrap.clone() } }
    fn plan(&self, rotate: bool) -> backup::Plan {
        backup::Plan {
            dir: self.backups(),
            root: self.root(),
            db: self.db.clone(),
            pg: self.tools(),
            tar: "tar".into(),
            age: "age".into(),
            group: None,
            rotate,
        }
    }
    fn env(&self, listen: &str) -> restore::Env {
        restore::Env {
            root: self.root(),
            pg: self.tools(),
            tar: "tar".into(),
            age: "age".into(),
            owners: restore::Owners::Keep,
            database_url: self.db_url(),
            listen: listen.into(),
            safety: self.plan(false),
            work_parent: self.t.path().to_path_buf(),
        }
    }
    fn write(&self, rel: &str, body: &[u8]) {
        let p = self.root().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    fn read(&self, rel: &str) -> Vec<u8> { std::fs::read(self.root().join(rel)).unwrap() }
}

/// 建一个迁移到最新、带数据的库，和一棵像中控的文件树
async fn fixture(pg: Pg) -> Fx {
    let db = format!("rt_{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
    exec(&pg.url, &format!("create database {db} owner fleet")).await;
    let fx = Fx { t: tempfile::tempdir().unwrap(), pg, db };
    let pool = fleet_server::db::connect(&fx.db_url()).await.unwrap();
    fleet_server::db::MIGRATOR.run(&pool).await.unwrap();
    fleet_server::db::maintain_partitions(&pool, 3, 7, 2).await.unwrap();
    fleet_server::auth::upsert_user(&pool, "admin", "correct horse battery").await.unwrap();
    fleet_server::auth::upsert_user(&pool, "ops", "correct horse battery").await.unwrap();
    sqlx::raw_sql(
        "insert into audit_log (actor, action) select 'test', 'a' || g from generate_series(1, 25) g;
         insert into host_metric (machine_id, ts, cpu_pct) select gen_random_uuid(), now() - g * interval '1 minute', 1 from generate_series(1, 30) g;
         insert into access_event (entry_id, ts, dest_host) select gen_random_uuid(), now(), 'x.test' from generate_series(1, 7) g;",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    std::fs::create_dir_all(fx.backups()).unwrap();
    fx.write("etc/fleet/secret.key", &[1u8; 32]);
    fx.write("etc/fleet/fleet.toml", b"listen = \"127.0.0.1:8080\"\ndatabase_url = \"postgres://fleet:old@127.0.0.1/fleet\"\n[retention]\nraw_days = 5\n");
    fx.write("etc/fleet/ssh/id_ed25519", b"central-key-v1");
    fx.write("etc/fleet/ssh/id_ed25519.pub", b"ssh-ed25519 AAAA central");
    fx.write("etc/fleet/backup.pub", b"ssh-ed25519 BBBB pve");
    fx.write("var/lib/fleet/known_hosts", b"m1 ssh-ed25519 CCCC\n");
    fx.write("etc/ssh/ssh_host_ed25519_key", b"sshd-host-v1");
    fx.write("etc/ssh/ssh_host_ed25519_key.pub", b"ssh-ed25519 DDDD host");
    fx.write("etc/caddy/global.d/edge-proxy.caddy", b"servers { listener_wrappers { proxy_protocol } }\n");
    fx
}

/// 「像新装的中控」：库里加了东西、密钥换了、fleet.toml 是新的
async fn mutate(fx: &Fx) {
    exec(&fx.db_url(), "insert into audit_log (actor, action) values ('later', 'b'), ('later', 'c'); delete from app_user where username = 'ops'").await;
    fx.write("etc/fleet/secret.key", &[2u8; 32]);
    fx.write("etc/fleet/ssh/id_ed25519", b"central-key-NEW");
    fx.write("var/lib/fleet/known_hosts", b"");
    fx.write("etc/ssh/ssh_host_ed25519_key", b"sshd-host-NEW");
    fx.write("etc/fleet/fleet.toml", b"listen = \"127.0.0.1:8080\"\ndatabase_url = \"postgres://fleet:new@127.0.0.1/fleet\"\n[retention]\nraw_days = 3\n");
    std::fs::remove_file(fx.root().join("etc/caddy/global.d/edge-proxy.caddy")).unwrap();
}

async fn cleanup(fx: &Fx) {
    let mut c = PgConnection::connect(&fx.pg.url).await.unwrap();
    let dbs: Vec<String> = sqlx::query_scalar("select datname::text from pg_database where datname like $1").bind(format!("{}%", fx.db)).fetch_all(&mut c).await.unwrap();
    for d in dbs {
        sqlx::query(&format!("drop database if exists {d} with (force)")).execute(&mut c).await.unwrap();
    }
    c.close().await.unwrap();
}

fn listed(d: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    v.sort();
    v
}

#[tokio::test]
async fn backup_then_restore_round_trip() {
    let fx = fixture(need_pg!()).await;
    let (schema0, counts0) = snapshot(&fx.db_url()).await;
    assert_eq!(counts0["app_user"], 2);
    assert!(counts0.iter().filter(|(t, _)| t.starts_with("host_metric_")).map(|(_, n)| n).sum::<i64>() == 30, "{counts0:?}");

    // 备份：与 fleet-backup.sh 同样的包
    let now = time::OffsetDateTime::now_utc();
    let done = backup::run(&fx.plan(true), now).await.unwrap();
    println!("{}", done.summary());
    let name = done.path.file_name().unwrap().to_str().unwrap().to_string();
    assert!(backup::rotation_day(&name).is_some(), "{name}");
    assert!(fx.backups().join(format!("{name}.sha256")).exists());

    mutate(&fx).await;
    let (_, counts_mut) = snapshot(&fx.db_url()).await;
    assert_ne!(counts_mut, counts0);

    // 准备 / --dry-run：什么都不改
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let free = listener.local_addr().unwrap().to_string();
    drop(listener);
    let env = fx.env(&free);
    let later = now + time::Duration::seconds(5);
    let opts = restore::Opts { archive: done.path.clone(), ..Default::default() };
    let p = restore::prepare(&opts, &env, later).await.unwrap();
    let plan = p.describe(&env);
    println!("{plan}");
    assert!(p.sha256_checked && !p.encrypted && p.pending.is_empty());
    assert!(p.toc_tables > 50, "{}", p.toc_tables);
    assert!(plan.contains(&format!("替换 {}", fx.root().join("etc/fleet/secret.key").display())), "{plan}");
    assert!(plan.contains(&format!("新建 {}", fx.root().join("etc/caddy/global.d/edge-proxy.caddy").display())), "{plan}");
    assert!(plan.contains(&format!("不变 {}", fx.root().join("etc/fleet/backup.pub").display())), "{plan}");
    assert!(plan.contains("retention.raw_days"), "{plan}");
    let work = p.work_dir().to_path_buf();
    assert!(work.exists());
    assert_eq!(snapshot(&fx.db_url()).await.1, counts_mut, "准备阶段不动库");
    assert_eq!(fx.read("etc/fleet/secret.key"), [2u8; 32]);
    drop(p);
    assert!(!work.exists(), "暂存目录（有密钥）丢弃时删掉");

    // fleet-server 在跑（listen 被占）：拒绝，什么都不改
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let env_busy = fx.env(&busy.local_addr().unwrap().to_string());
    let p = restore::prepare(&opts, &env_busy, later).await.unwrap();
    let e = restore::apply(&p, &env_busy, later).await.unwrap_err();
    assert!(format!("{e:#}").contains("fleet-server 在跑"), "{e:#}");
    drop((p, busy));
    assert_eq!(listed(&fx.backups()).len(), 2, "拒绝时也不做安全备份");

    // 真正恢复
    let p = restore::prepare(&opts, &env, later).await.unwrap();
    let r = restore::apply(&p, &env, later).await.unwrap();
    println!("{}", restore::manual_steps(&r, &p));
    let (schema1, counts1) = snapshot(&fx.db_url()).await;
    assert_eq!(schema1, schema0, "结构和备份时一样");
    let mut want = counts0.clone();
    for (t, n) in want.iter_mut() {
        // 四张原始时序表（及其分区）只备结构
        if ["host_metric", "entry_traffic", "probe_result", "access_event"].iter().any(|x| t == x || t.starts_with(&format!("{x}_"))) && !t.ends_with("_hourly") {
            *n = 0;
        }
    }
    let diff: Vec<_> = want.iter().filter(|(t, n)| counts1.get(*t) != Some(n)).map(|(t, n)| format!("{t}: 应为 {n}，实际 {:?}", counts1.get(t))).collect();
    assert!(diff.is_empty(), "行数不对：{diff:?}");
    assert_eq!(counts1["app_user"], 2);
    assert_eq!(counts1["audit_log"], counts0["audit_log"]);
    // 恢复前的库留着
    let old = r.old_db.clone().unwrap();
    assert_eq!(snapshot(&url_for(&fx.pg.url, &old)).await.1, counts_mut);
    assert!(!db_exists(&fx.pg.url, &p.restore_db()).await);
    // 文件放回；被换掉的旧文件留着
    assert_eq!(fx.read("etc/fleet/secret.key"), [1u8; 32]);
    assert_eq!(fx.read("etc/fleet/ssh/id_ed25519"), b"central-key-v1");
    assert_eq!(fx.read("var/lib/fleet/known_hosts"), b"m1 ssh-ed25519 CCCC\n");
    assert_eq!(fx.read("etc/ssh/ssh_host_ed25519_key"), b"sshd-host-v1");
    assert!(fx.root().join("etc/caddy/global.d/edge-proxy.caddy").exists());
    let suffix = format!("pre-restore-{}", restore::stamp(later).replace('_', "-"));
    assert_eq!(fx.read(&format!("etc/fleet/secret.key.{suffix}")), [2u8; 32]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = |rel: &str| std::fs::metadata(fx.root().join(rel)).unwrap().permissions().mode() & 0o777;
        assert_eq!(m("etc/fleet/secret.key"), 0o600);
        assert_eq!(m("var/lib/fleet/known_hosts"), 0o640);
        assert_eq!(m("etc/ssh/ssh_host_ed25519_key.pub"), 0o644);
    }
    // fleet.toml 不换，备份里那份另存
    assert!(String::from_utf8(fx.read("etc/fleet/fleet.toml")).unwrap().contains("fleet:new@"));
    assert!(String::from_utf8(std::fs::read(&r.toml_copy).unwrap()).unwrap().contains("raw_days = 5"));
    assert!(r.sshd_or_caddy && r.backup_pub && r.ssh_host_key);
    // 安全备份：恢复前的状态，没有轮换掉任何一份
    assert!(r.safety_backup.exists() && r.safety_backup != done.path);
    assert!(done.path.exists());
    // 恢复后的库本程序能直接用（迁移全在、校验和一致）
    let pool = fleet_server::db::connect(&fx.db_url()).await.unwrap();
    fleet_server::db::MIGRATOR.run(&pool).await.unwrap();
    pool.close().await;
    cleanup(&fx).await;
}

/// 放回文件的中途失败：已放回的换回原来的、新库删掉、库与文件都是恢复之前的
#[cfg(unix)]
#[tokio::test]
async fn failure_midway_leaves_prior_state() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture(need_pg!()).await;
    let now = time::OffsetDateTime::now_utc();
    let done = backup::run(&fx.plan(true), now).await.unwrap();
    mutate(&fx).await;
    let (schema_m, counts_m) = snapshot(&fx.db_url()).await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let env = fx.env(&listener.local_addr().unwrap().to_string());
    drop(listener);
    let later = now + time::Duration::seconds(5);
    let p = restore::prepare(&restore::Opts { archive: done.path.clone(), ..Default::default() }, &env, later).await.unwrap();
    // var/lib/fleet 不可写：secret.key、id_ed25519 放回之后，known_hosts 那一步失败
    let vlf = fx.root().join("var/lib/fleet");
    std::fs::set_permissions(&vlf, std::fs::Permissions::from_mode(0o555)).unwrap();
    let e = restore::apply(&p, &env, later).await.unwrap_err();
    std::fs::set_permissions(&vlf, std::fs::Permissions::from_mode(0o755)).unwrap();
    let msg = format!("{e:#}");
    println!("{msg}");
    assert!(msg.contains("恢复失败，已回到恢复之前"), "{msg}");
    assert_eq!(fx.read("etc/fleet/secret.key"), [2u8; 32]);
    assert_eq!(fx.read("etc/fleet/ssh/id_ed25519"), b"central-key-NEW");
    assert_eq!(fx.read("var/lib/fleet/known_hosts"), b"");
    let leftovers: Vec<String> = listed(&fx.root().join("etc/fleet")).into_iter().filter(|n| n.contains("restore")).collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(!db_exists(&fx.pg.url, &p.restore_db()).await, "新库删掉");
    assert!(!db_exists(&fx.pg.url, &p.old_db()).await, "没换库");
    assert_eq!(snapshot(&fx.db_url()).await, (schema_m, counts_m));
    cleanup(&fx).await;
}

/// 加密备份：备份经 `age -R` 管道加密、恢复要 `--identity` 才解得开。这里没有真的 age，用一个按同样参数工作的假 age
/// （加密 = 写 age 文件头再原样接上；解密 = 去掉文件头），测的是参数、管道与流程，不是 age 本身
#[cfg(unix)]
#[tokio::test]
async fn encrypted_backup_needs_identity() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture(need_pg!()).await;
    let age = fx.t.path().join("fake-age");
    std::fs::write(
        &age,
        "#!/bin/sh\nset -e\nH='age-encryption.org/v1'\n\
         if [ \"$1\" = -R ]; then [ -s \"$2\" ]; printf '%s\\n' \"$H\"; exec cat; fi\n\
         [ \"$1\" = -d ] && [ \"$2\" = -i ] && [ \"$(cat \"$3\")\" = AGE-SECRET-KEY-TEST ] && [ \"$4\" = -o ] || { echo 'age: no identity matched' >&2; exit 1; }\n\
         tail -c +23 \"$6\" > \"$5\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&age, std::fs::Permissions::from_mode(0o755)).unwrap();
    fx.write("etc/fleet/backup-age.recipients", b"age1test\n");
    let plan = backup::Plan { age: age.display().to_string(), ..fx.plan(true) };
    let now = time::OffsetDateTime::now_utc();
    let done = backup::run(&plan, now).await.unwrap();
    assert!(done.encrypted && done.path.to_str().unwrap().ends_with(".tar.age"));
    assert!(std::fs::read(&done.path).unwrap().starts_with(b"age-encryption.org/v1"));
    let env = restore::Env { age: age.display().to_string(), ..fx.env("127.0.0.1:1") };
    let e = restore::prepare(&restore::Opts { archive: done.path.clone(), ..Default::default() }, &env, now).await.err().unwrap();
    assert!(format!("{e:#}").contains("--identity"), "{e:#}");
    let wrong = fx.t.path().join("wrong.txt");
    std::fs::write(&wrong, "AGE-SECRET-KEY-OTHER").unwrap();
    let e = restore::prepare(&restore::Opts { archive: done.path.clone(), identity: Some(wrong), ..Default::default() }, &env, now).await.err().unwrap();
    assert!(format!("{e:#}").contains("age 解密"), "{e:#}");
    let key = fx.t.path().join("key.txt");
    std::fs::write(&key, "AGE-SECRET-KEY-TEST").unwrap();
    let p = restore::prepare(&restore::Opts { archive: done.path.clone(), identity: Some(key), ..Default::default() }, &env, now).await.unwrap();
    assert!(p.encrypted && p.sha256_checked);
    assert!(p.files.iter().any(|f| f.rel == "etc/fleet/backup-age.recipients"));
    drop(p);
    cleanup(&fx).await;
}

/// 坏备份在准备阶段就拒绝：校验和对不上、没有校验和、成员越界
#[tokio::test]
async fn bad_archives_rejected_before_any_change() {
    let fx = fixture(need_pg!()).await;
    let now = time::OffsetDateTime::now_utc();
    let done = backup::run(&fx.plan(true), now).await.unwrap();
    let env = fx.env("127.0.0.1:1");
    let opts = |p: &Path| restore::Opts { archive: p.to_path_buf(), ..Default::default() };
    // 改一个字节
    let bad = fx.t.path().join("fleet-20990101-000000.tar");
    let mut b = std::fs::read(&done.path).unwrap();
    let n = b.len();
    b[n / 2] ^= 0xff;
    std::fs::write(&bad, &b).unwrap();
    std::fs::copy(format!("{}.sha256", done.path.display()), format!("{}.sha256", bad.display())).unwrap();
    let e = restore::prepare(&opts(&bad), &env, now).await.err().unwrap();
    assert!(format!("{e:#}").contains("对不上"), "{e:#}");
    // 没有校验和
    std::fs::remove_file(format!("{}.sha256", bad.display())).unwrap();
    let e = restore::prepare(&opts(&bad), &env, now).await.err().unwrap();
    assert!(format!("{e:#}").contains("--skip-sha256"), "{e:#}");
    // 有越界成员的 tar（绝对路径之外的 ../ 与不认识的路径）：用 GNU tar 做一个
    let evil_root = fx.t.path().join("evil");
    std::fs::create_dir_all(evil_root.join("etc")).unwrap();
    std::fs::write(evil_root.join("etc/passwd"), b"x").unwrap();
    std::fs::write(evil_root.join("fleet.dump"), b"PGDMP").unwrap();
    let evil = fx.t.path().join("evil.tar");
    assert!(std::process::Command::new("tar").arg("-cf").arg(&evil).arg("-C").arg(&evil_root).args(["fleet.dump", "etc/passwd"]).status().unwrap().success());
    let e = restore::prepare(&restore::Opts { archive: evil.clone(), skip_sha256: true, ..Default::default() }, &env, now).await.err().unwrap();
    assert!(format!("{e:#}").contains("不认识的成员「etc/passwd」"), "{e:#}");
    // 符号链接成员
    std::fs::remove_file(evil_root.join("etc/passwd")).unwrap();
    std::os::unix::fs::symlink("/root", evil_root.join("etc/fleet")).unwrap();
    assert!(std::process::Command::new("tar").arg("-cf").arg(&evil).arg("-C").arg(&evil_root).args(["fleet.dump", "etc/fleet"]).status().unwrap().success());
    let e = restore::prepare(&restore::Opts { archive: evil, skip_sha256: true, ..Default::default() }, &env, now).await.err().unwrap();
    assert!(format!("{e:#}").contains("不是普通文件或目录"), "{e:#}");
    // 暂存目录都删了
    assert!(!listed(fx.t.path()).iter().any(|n| n.starts_with("fleet-restore-")));
    cleanup(&fx).await;
}
