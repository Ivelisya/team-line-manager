//! 集成测试的公共搭建，各套件的 common_* 模块在这之上加自己的辅助函数。
//! 需要 PostgreSQL：环境变量 FLEET_TEST_DB（默认 postgres://fleet:fleet@127.0.0.1/fleet_test）。
//!
//! 测试并发安全：每个测试在库里建一个自己的 schema（`t_<建立时的 Unix 秒>_<随机>`），连接池的每条连接都把
//! search_path 设成它，迁移（含 _sqlx_migrations）、分区和数据都落在里面，测试之间互不可见，按 cargo test 默认的并发跑即可。
//! 测试结束（含断言失败）时删掉自己的 schema；进程被杀等原因留下的，下一次运行时删掉建立超过 30 分钟的。
//! 每个测试的连接池最多 5 条连接；cargo test 默认按 CPU 核数并发（6 核的 fleet-dev 最多约 30 条），在 PostgreSQL 默认的
//! 100 条上限之内。核数很多、库又和别的服务共用时，用 `-- --test-threads=N` 压一下并发。
//! 离线 IP 库指向测试自己的临时目录（默认是空的，走降级），不读机器上装好的真实库。
//!
//! 只跑某个套件：`cargo test -p fleet-server --test telemetry`；只跑其中某个测试再加名字过滤。

#![allow(dead_code)]

use axum::body::Body;
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
    /// 本测试的临时目录：known_hosts、离线 IP 库（ipintel/）、推给机器的程序（dist/，默认不存在）
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
struct Schema {
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

/// 建本测试的 schema，跑迁移、建分区，返回 search_path 指向它的连接池
async fn test_db(url: &str) -> (PgPool, Schema) {
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
    // 迁移不拿 sqlx 的咨询锁：它按库名加锁，所有测试的迁移会排成一队；各 schema 互不相干，用不着
    let migrator = sqlx::migrate::Migrator {
        migrations: db::MIGRATOR.migrations.clone(),
        ignore_missing: db::MIGRATOR.ignore_missing,
        locking: false,
        no_tx: db::MIGRATOR.no_tx,
    };
    migrator.run(&pool).await.unwrap();
    db::maintain_partitions(&pool, 14, 7, 2).await.unwrap();
    (pool, schema)
}

/// 起一个全新的测试环境。`background` 为真时启动作业引擎等后台任务；为假则作业不会自己跑（测试手动领取、推进）。
pub async fn setup(background: bool) -> T { boot(background, "job_workers = 4").await }

/// `extra`：追加的配置行（顶层键，或 `ipintel.xxx = ...` 这样的点号键）
pub async fn boot(background: bool, extra: &str) -> T { boot_in(tempfile::tempdir().unwrap(), background, extra).await }

/// 同 `boot`，临时目录由调用方先准备好（比如往 ipintel/ 里放离线库）
pub async fn boot_in(dir: tempfile::TempDir, background: bool, extra: &str) -> T {
    let url = std::env::var("FLEET_TEST_DB").unwrap_or_else(|_| "postgres://fleet:fleet@127.0.0.1/fleet_test".into());
    let (pool, schema) = test_db(&url).await;
    let d = dir.path().display().to_string().replace('\\', "/");
    let cfg: Config = toml::from_str(&format!(
        r#"database_url = "{url}"
           secret_key_file = "/dev/null"
           public_url = "http://fleet.test"
           ssh_key = "/nonexistent"
           known_hosts = "{d}/known_hosts"
           dist_dir = "{d}/dist"
           ipintel.db_dir = "{d}/ipintel"
           ipintel.overrides = "/nonexistent/ipintel-overrides.toml"
           {extra}"#
    ))
    .unwrap();
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

/// 直接在库里放一台在用的线路机（IP 198.51.100.2，名字「线路机2」），并给它签一个 agent 令牌
pub async fn seed_relay(t: &T) -> (Uuid, String) {
    let id = Uuid::now_v7();
    let (sk, pk) = fleet_server::crypto::reality_keypair();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle, activated_at) values ($1, 'relay', '线路机2', '198.51.100.2', 'active', now())")
        .bind(id)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into relay (machine_id, public_host, reality_private_enc, reality_public_key, min_client_ver, generation, applied_generation) values ($1, 'relay.test', $2, $3, '1.8.2', 1, 1)")
        .bind(id)
        .bind(t.st.secrets.seal_str(&sk))
        .bind(pk)
        .execute(&t.st.db)
        .await
        .unwrap();
    let tok = "fa1_test_relay_token";
    sqlx::query("insert into agent_token (id, machine_id, token_hash) values ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(id)
        .bind(fleet_server::crypto::sha256(tok.as_bytes()))
        .execute(&t.st.db)
        .await
        .unwrap();
    (id, tok.to_string())
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
