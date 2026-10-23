//! 集成测试的公共搭建，子模块（ipi、mon、ops、pol、tele）在这之上加各套件自己的辅助函数。
//! 需要 PostgreSQL：环境变量 FLEET_TEST_DB（默认 postgres://fleet:fleet@127.0.0.1/fleet_test）。
//!
//! 测试并发安全：每个测试在库里建一个自己的 schema（`t_<建立时的 Unix 秒>_<随机>`），连接池的每条连接都把
//! search_path 设成它，迁移（含 _sqlx_migrations）、分区和数据都落在里面，测试之间互不可见，按 cargo test 默认的并发跑即可。
//! 测试结束（含断言失败）时删掉自己的 schema；进程被杀等原因留下的，下一次运行时删掉建立超过 30 分钟的。
//! 每个测试的连接池最多 5 条连接；cargo test 默认按 CPU 核数并发（6 核的 fleet-dev 最多约 30 条），在 PostgreSQL 默认的
//! 100 条上限之内。核数很多、库又和别的服务共用时，用 `-- --test-threads=N` 压一下并发。
//! 离线 IP 库指向测试自己的临时目录（默认是空的，走降级），不读机器上装好的真实库。
//!
//! 集成测试都在 tests/it 这一个测试目标里（只链接一次）。只跑某个套件：`cargo test -p fleet-server --test it telemetry::`；
//! 只跑其中某个测试再加名字。改了进程 PATH（放假 ssh）的 replace、routers、upgrade、provision 各自单独一个目标（`--test replace`），也用这里的搭建。

#![allow(dead_code)]

pub mod ipi;
pub mod mon;
pub mod ops;
pub mod pol;
pub mod tele;

use axum::body::Body;
use fleet_proto::{ApplyReport, DesiredState, EntryApply, ProbeKind, ProbeSample, Report};
use axum::http::{Request, StatusCode};
use fleet_server::{config::Config, crypto::SecretBox, db, state::AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

pub struct T {
    pub app: axum::Router,
    pub st: AppState,
    pub cookie: String,
    /// 本测试的临时目录：known_hosts、离线 IP 库（ipintel/）、推给机器的程序（dist/，默认不存在）、中控 SSH 密钥（ssh/，默认不存在）、
    /// 软路由的隧道公钥清单与 router_known_hosts（不碰本机 /var/lib/fleet 下真的那几个）
    pub dir: tempfile::TempDir,
    _schema: Schema,
}

impl T {
    pub async fn req(&self, method: &str, path: &str, body: Option<Value>, auth: Option<&str>) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path).header("content-type", "application/json").header("x-fleet-request", "1");
        match auth {
            Some(tok) => b = b.header("authorization", format!("Bearer {tok}")),
            None => b = b.header("cookie", &self.cookie),
        }
        let req = b.body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty)).unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    pub async fn get(&self, p: &str) -> Value {
        let (s, v) = self.req("GET", p, None, None).await;
        assert_eq!(s, StatusCode::OK, "GET {p}: {v}");
        v["data"].clone()
    }

    pub async fn post(&self, p: &str, b: Value) -> Value {
        let (s, v) = self.req("POST", p, Some(b), None).await;
        assert!(s.is_success(), "POST {p}: {s} {v}");
        v["data"].clone()
    }

    pub async fn job(&self, id: &str) -> Value { self.get(&format!("/api/v1/jobs/{id}")).await }

    /// 把在等待的作业提前到现在并唤醒作业引擎：步骤的 Wait(5~10 秒) 是给真实线路机留的兜底，测试里模拟的线路机每轮都即时回报，不必干等
    pub async fn nudge_jobs(&self) {
        sqlx::query("update job set run_after = now() where state = 'waiting' and run_after > now()").execute(&self.st.db).await.unwrap();
        self.st.wake_jobs();
    }

    /// 等作业到某个状态；中途变成了别的终态（尤其是想要的不是 failed 却 failed 了）直接 panic 并带上日志
    pub async fn wait_job(&self, id: &str, want: &str) -> Value {
        for _ in 0..400 {
            let j = self.job(id).await;
            if j["state"] == want {
                return j;
            }
            let terminal = matches!(j["state"].as_str(), Some("failed" | "cancelled" | "succeeded"));
            if terminal {
                let logs = self.get(&format!("/api/v1/jobs/{id}/logs")).await;
                panic!("作业 {id} 想要 {want}，实际 {}：{} / {logs}", j["state"], j["error"]);
            }
            self.nudge_jobs().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("作业 {id} 没到 {want}：{}", self.job(id).await);
    }

    pub async fn machine_lifecycle(&self, id: Uuid) -> String {
        sqlx::query_scalar("select lifecycle::text from machine where id = $1").bind(id).fetch_one(&self.st.db).await.unwrap()
    }
}

/// 本测试的 schema，drop 时删掉。测试的运行时里不能等异步操作：另起一个线程和运行时，用一条新连接去删
pub struct Schema {
    url: String,
    name: String,
}

impl Drop for Schema {
    fn drop(&mut self) {
        let (url, name) = (self.url.clone(), self.name.clone());
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            rt.block_on(async {
                let Ok(mut c) = PgConnection::connect(&url).await else { return };
                // 先断开本测试连接池的连接（application_name 是 schema 名）：单线程运行时的测试此刻被这里阻塞着，
                // 接口出错时丢下的事务还没来得及回滚、拿着锁（含跨 schema 的咨询锁，会挡住别的测试），不断开就删不动
                let _ = sqlx::query("select pg_terminate_backend(pid) from pg_stat_activity where application_name = $1 and pid <> pg_backend_pid()")
                    .bind(&name)
                    .execute(&mut c)
                    .await;
                // 后台任务可能又连上来（锁等待、死锁被选中回滚）：最多试 3 次，每次最多等 5 秒锁，还删不掉就留给下一次运行清理
                let _ = sqlx::query("set lock_timeout = '5s'").execute(&mut c).await;
                for _ in 0..3 {
                    if sqlx::query(&format!("drop schema if exists {name} cascade")).execute(&mut c).await.is_ok() {
                        break;
                    }
                }
                let _ = c.close().await;
            });
        })
        .join();
    }
}

/// 每个进程只清一次残留的 schema
static SWEPT: AtomicBool = AtomicBool::new(false);

/// 迁移不拿 sqlx 的咨询锁：它按库名加锁，所有测试的迁移会排成一队；各 schema 互不相干，用不着。
/// `upto`：只跑到这个版本（测迁移本身：先放存量数据再跑后面的）
pub fn migrator(upto: Option<i64>) -> sqlx::migrate::Migrator {
    sqlx::migrate::Migrator {
        migrations: db::MIGRATOR.migrations.iter().filter(|m| !matches!(upto, Some(v) if m.version > v)).cloned().collect::<Vec<_>>().into(),
        ignore_missing: db::MIGRATOR.ignore_missing,
        locking: false,
        no_tx: db::MIGRATOR.no_tx,
    }
}

/// 只迁移到 `version` 的空 schema（不建分区、不起服务），测迁移用；之后 `migrator(None).run(&pool)` 跑完其余的
pub async fn db_until(version: i64) -> (PgPool, Schema) {
    let url = std::env::var("FLEET_TEST_DB").unwrap_or_else(|_| "postgres://fleet:fleet@127.0.0.1/fleet_test".into());
    test_db(&url, Some(version)).await
}

/// 建本测试的 schema，跑迁移（`upto` 见 [`migrator`]；跑全了才建分区），返回 search_path 指向它的连接池
async fn test_db(url: &str, upto: Option<i64>) -> (PgPool, Schema) {
    let mut c = PgConnection::connect(url).await.unwrap();
    if !SWEPT.swap(true, Ordering::Relaxed) {
        let old: Vec<String> = sqlx::query_scalar(
            "select nspname::text from pg_namespace
             where nspname ~ '^t_[0-9]+_[0-9a-f]+$' and split_part(nspname, '_', 2)::bigint < extract(epoch from now())::bigint - 1800",
        )
        .fetch_all(&mut c)
        .await
        .unwrap();
        sqlx::query("set lock_timeout = '5s'").execute(&mut c).await.unwrap();
        for s in old {
            let _ = sqlx::query(&format!("drop schema if exists {s} cascade")).execute(&mut c).await;
        }
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let name = format!("t_{secs}_{}", &Uuid::new_v4().simple().to_string()[..8]);
    sqlx::query(&format!("create schema {name}")).execute(&mut c).await.unwrap();
    let _ = c.close().await;
    let schema = Schema { url: url.to_string(), name: name.clone() };

    let opts: PgConnectOptions = url.parse().unwrap();
    // 等空闲连接的超时与 db::connect 相同（5 秒）
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(opts.options([("search_path", name.as_str())]).application_name(&name))
        .await
        .unwrap();
    migrator(upto).run(&pool).await.unwrap();
    if upto.is_none() {
        db::maintain_partitions(&pool, 14, 7, 2).await.unwrap();
    }
    (pool, schema)
}

/// 起一个全新的测试环境。`background` 为真时启动作业引擎等后台任务；为假则作业不会自己跑（测试手动领取、推进）。
pub async fn setup(background: bool) -> T { boot(background, "job_workers = 4").await }

/// `extra`：追加的配置行（顶层键，或 `ipintel.xxx = ...` 这样的点号键）
pub async fn boot(background: bool, extra: &str) -> T { boot_in(tempfile::tempdir().unwrap(), background, extra).await }

/// 同 `boot`，临时目录由调用方先准备好（比如往 ipintel/ 里放离线库）
pub async fn boot_in(dir: tempfile::TempDir, background: bool, extra: &str) -> T {
    let url = std::env::var("FLEET_TEST_DB").unwrap_or_else(|_| "postgres://fleet:fleet@127.0.0.1/fleet_test".into());
    let (pool, schema) = test_db(&url, None).await;
    let d = dir.path().display().to_string().replace('\\', "/");
    let mut cfg: Config = toml::from_str(&format!(
        r#"database_url = "{url}"
           secret_key_file = "/dev/null"
           public_url = "http://fleet.test"
           ssh_key = "{d}/ssh/id_ed25519"
           known_hosts = "{d}/known_hosts"
           dist_dir = "{d}/dist"
           ipintel.db_dir = "{d}/ipintel"
           ipintel.overrides = "/nonexistent/ipintel-overrides.toml"
           routers.tunnel_keys_file = "{d}/tunnel-keys"
           routers.known_hosts = "{d}/router_known_hosts"
           routers.control_dir = "{d}/ssh-ctl"
           routers.sshd_host_key = "{d}/ssh_host_ed25519_key.pub"
           fx.auto = false
           {extra}"#
    ))
    .unwrap();
    // 通知不等合并（只合并已经排着的）：各用例不用每条多等 20 秒
    cfg.notify.group_wait_s = 0;
    // 通知渠道指向本机假服务：extra 里写 `notify.allow_private = true`（不从 fleet.toml 读，只在这里认）
    cfg.notify.allow_private = extra.contains("notify.allow_private = true");
    let st = AppState::new(cfg, pool.clone(), SecretBox::from_key(&[9u8; 32]).unwrap());
    if background {
        fleet_server::spawn_background(&st);
    }
    fleet_server::auth::upsert_user(&pool, "admin", ADMIN_PW).await.unwrap();
    let app = fleet_server::app(st.clone());
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(json!({"username": "admin", "password": ADMIN_PW}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
    T { app, st, cookie, dir, _schema: schema }
}

pub const ADMIN_PW: &str = "correct horse battery";

/// 直接在库里放一台在用的线路机，并给它签一个 agent 令牌。返回 (机器 id, 令牌)
async fn seed_relay_row(t: &T, label: &str, ip: &str, public_host: &str, tok: &str) -> (Uuid, String) {
    let id = Uuid::now_v7();
    let (sk, pk) = fleet_server::crypto::reality_keypair();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle, activated_at) values ($1, 'relay', $2, $3::inet, 'active', now())")
        .bind(id)
        .bind(label)
        .bind(ip)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into relay (machine_id, public_host, reality_private_enc, reality_public_key, min_client_ver, generation, applied_generation) values ($1, $2, $3, $4, '1.8.2', 1, 1)")
        .bind(id)
        .bind(public_host)
        .bind(t.st.secrets.seal_str(&sk))
        .bind(pk)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into agent_token (id, machine_id, token_hash) values ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(id)
        .bind(fleet_server::crypto::sha256(tok.as_bytes()))
        .execute(&t.st.db)
        .await
        .unwrap();
    (id, tok.to_string())
}

/// 线路机「线路机2」（198.51.100.2）
pub async fn seed_relay(t: &T) -> (Uuid, String) { seed_relay_row(t, "线路机2", "198.51.100.2", "relay.test", "fa1_test_relay_token").await }

/// 第二台线路机「线路机3」（198.51.100.3）
pub async fn seed_relay_b(t: &T) -> (Uuid, String) { seed_relay_row(t, "线路机3", "198.51.100.3", "relay3.test", "fa1_test_relay_token_b").await }

/// 中控 dist 里放一个假的 agent 程序（shell 脚本，`--version` 打印版本）：「全部升级」要记下它的 sha256 与版本号
pub fn dist_agent(t: &T, version: &str) {
    let p = t.dir.path().join("dist/fleet-agent");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, format!("#!/bin/sh\necho 'fleet-agent {version}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// 每 100 毫秒试一次 `f`，返回 `Some` 就结束并交回它的值；`timeout` 内一直是 `None` 就 panic，说明等的是什么
pub async fn eventually<R, F, Fut>(what: &str, timeout: Duration, f: F) -> R
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<R>>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(r) = f().await {
            return r;
        }
        assert!(std::time::Instant::now() < deadline, "{} 秒内没等到：{what}", timeout.as_secs());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn now_ms() -> i64 { (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64 }

/// 模拟线路机：拉期望状态、回报全部应用成功、给每个入口报一个成功的出口探测
pub struct Sim {
    tok: String,
    seq: u64,
}

impl Sim {
    pub fn new(tok: String) -> Self { Sim { tok, seq: 0 } }

    /// 一轮：拉期望状态 → 回报已应用 → 上报出口探测
    pub async fn round(&mut self, t: &T) -> DesiredState {
        let (s, v) = t.req("GET", "/agent/v1/desired?after=0", None, Some(&self.tok)).await;
        assert_eq!(s, StatusCode::OK, "desired: {v}");
        let ds: DesiredState = serde_json::from_value(v).unwrap();
        let rep = ApplyReport {
            generation: ds.generation,
            ok: true,
            error: None,
            entries: ds.entries.iter().map(|e| EntryApply { id: e.id, ok: true, error: None }).collect(),
        };
        let (s, _) = t.req("POST", "/agent/v1/applied", Some(serde_json::to_value(&rep).unwrap()), Some(&self.tok)).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        self.seq += 1;
        let r = Report {
            seq: self.seq,
            boot_id: "b1".into(),
            agent_version: "test".into(),
            sent_at_ms: now_ms(),
            probes: ds
                .entries
                .iter()
                .map(|e| ProbeSample { entry_id: e.id, ts_ms: now_ms(), kind: ProbeKind::Egress, ok: true, latency_ms: Some(12.0), detail: Some(e.expect_egress_ip.to_string()), retrans: None })
                .collect(),
            ..Default::default()
        };
        let (s, v) = t.req("POST", "/agent/v1/report", Some(serde_json::to_value(&r).unwrap()), Some(&self.tok)).await;
        assert_eq!(s, StatusCode::OK, "report: {v}");
        ds
    }
}

pub async fn seed_team(t: &T, name: &str) -> Uuid {
    t.post("/api/v1/teams", json!({"name": name})).await["id"].as_str().unwrap().parse().unwrap()
}

/// 直接在库里放一台节点（kind: proxy / vps）。返回机器 id。
pub async fn seed_node(t: &T, team: Uuid, number: i32, ip: &str, lifecycle: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into machine (id, kind, team_id, number, primary_ip, lifecycle, activated_at, retired_at)
         values ($1, 'proxy', $2, $3, $4::inet, $5::machine_lifecycle, now(), case when $5 = 'retired' then now() end)",
    )
    .bind(id)
    .bind(team)
    .bind(number)
    .bind(ip)
    .bind(lifecycle)
    .execute(&t.st.db)
    .await
    .unwrap();
    id
}
