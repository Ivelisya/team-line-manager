//! IP 情报测试的公共部分：建库、登录、造机器和入口、塞来源小时汇总、预置画像缓存。
//! 需要 PostgreSQL：环境变量 FLEET_TEST_DB（默认 postgres://fleet:fleet@127.0.0.1/fleet_test）。会清空该库的 public schema。
//! 不起后台循环：来源汇总 / 异常 / 告警评估都由测试手动驱动，并且可以传入「现在」来快进时间。
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use fleet_server::{config::Config, crypto::SecretBox, db, state::AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

pub struct T {
    pub app: axum::Router,
    pub st: AppState,
    pub cookie: String,
    /// 离线库目录（临时目录，随 T 一起删）
    pub dbdir: tempfile::TempDir,
}

impl T {
    pub async fn raw(&self, path: &str) -> (StatusCode, Value) {
        let req = Request::builder().method("GET").uri(path).header("cookie", &self.cookie).body(Body::empty()).unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    pub async fn get(&self, p: &str) -> Value {
        let (s, v) = self.raw(p).await;
        assert_eq!(s, StatusCode::OK, "GET {p}: {v}");
        v["data"].clone()
    }

    /// 预置一条画像缓存，库版本取当前这一代（所以命中）
    pub async fn put_cache(&self, ip: &str, isp: &str, country: &str, province: Option<&str>, city: Option<&str>, conn_type: &str) {
        self.put_cache_v(ip, isp, country, province, city, conn_type, &self.st.ipintel.version()).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn put_cache_v(&self, ip: &str, isp: &str, country: &str, province: Option<&str>, city: Option<&str>, conn_type: &str, version: &str) {
        sqlx::query(
            "insert into ip_intel_cache (ip, isp, asn, country, province, city, conn_type, confidence, reason, db_version)
             values ($1::inet, $2, 64500, $3, $4, $5, $6, 'high', '测试预置', $7)
             on conflict (ip) do update set isp = excluded.isp, country = excluded.country, province = excluded.province, city = excluded.city,
                conn_type = excluded.conn_type, db_version = excluded.db_version",
        )
        .bind(ip)
        .bind(isp)
        .bind(country)
        .bind(province)
        .bind(city)
        .bind(conn_type)
        .bind(version)
        .execute(&self.st.db)
        .await
        .unwrap();
    }

    /// 塞一条来源小时汇总
    pub async fn put_hourly(&self, node: Uuid, hour: OffsetDateTime, ip: &str, conns: i32) {
        sqlx::query("insert into access_source_hourly (node_id, hour, src_ip, conns) values ($1, $2, $3::inet, $4)")
            .bind(node)
            .bind(fleet_server::access::floor_hour(hour))
            .bind(ip)
            .bind(conns)
            .execute(&self.st.db)
            .await
            .unwrap();
    }

    /// 某条规则当前的告警：(state, summary)
    pub async fn incidents(&self, node: Uuid) -> Vec<(String, String, String)> {
        sqlx::query_as("select rule, state::text, summary from incident where subject_id = $1 and rule like 'ipintel!_%' escape '!' order by rule, opened_at")
            .bind(node)
            .fetch_all(&self.st.db)
            .await
            .unwrap()
    }
}

/// `with_dbs`：离线库目录里放一个只有一行的 iptoasn TSV，让服务认为「有库」（来源汇总才会推进）；
/// 否则目录是空的，走降级路径。两种都不会读到真实的 /var/lib/fleet/ipintel。
pub async fn setup(with_dbs: bool) -> T {
    let url = std::env::var("FLEET_TEST_DB").unwrap_or_else(|_| "postgres://fleet:fleet@127.0.0.1/fleet_test".into());
    let pool = db::connect(&url).await.unwrap();
    sqlx::query("drop schema public cascade").execute(&pool).await.unwrap();
    sqlx::query("create schema public").execute(&pool).await.unwrap();
    db::MIGRATOR.run(&pool).await.unwrap();
    db::maintain_partitions(&pool, 14, 7, 2).await.unwrap();
    let dbdir = tempfile::tempdir().unwrap();
    if with_dbs {
        std::fs::write(dbdir.path().join("ip2asn-combined.tsv"), "1.0.0.0\t1.0.0.255\t13335\tAU\tCLOUDFLARENET\n").unwrap();
    }
    let cfg: Config = toml::from_str(&format!(
        r#"database_url = "{url}"
           secret_key_file = "/dev/null"
           public_url = "http://fleet.test"
           ssh_key = "/nonexistent"
           known_hosts = "/tmp/fleet-test-known-hosts"
           dist_dir = "/nonexistent"
           [ipintel]
           db_dir = "{dir}"
           overrides = "/nonexistent/ipintel-overrides.toml"
           internal_ips = ["192.0.2.77"]"#,
        dir = dbdir.path().display().to_string().replace('\\', "/")
    ))
    .unwrap();
    let st = AppState::new(cfg, pool.clone(), SecretBox::from_key(&[9u8; 32]).unwrap());
    fleet_server::auth::upsert_user(&pool, "admin", "correct horse battery").await.unwrap();
    let app = fleet_server::app(st.clone());
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(json!({"username": "admin", "password": "correct horse battery"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
    T { app, st, cookie, dbdir }
}

pub struct Fx {
    pub relay: Uuid,
    /// 团队1-01、团队1-02、团队2-01
    pub n: [Uuid; 3],
}

/// 一台线路机（线路机2）+ 三个节点（团队1-01、团队1-02、团队2-01）+ 各一个入口
pub async fn seed(t: &T) -> Fx {
    let db = &t.st.db;
    let (relay, team1, team2) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for (id, name) in [(team1, "团队1"), (team2, "团队2")] {
        sqlx::query("insert into team (id, name) values ($1, $2)").bind(id).bind(name).execute(db).await.unwrap();
    }
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle) values ($1, 'relay', '线路机2', '198.51.100.2', 'active')")
        .bind(relay)
        .execute(db)
        .await
        .unwrap();
    let mut n = [Uuid::nil(); 3];
    for (i, (team, num)) in [(team1, 1), (team1, 2), (team2, 1)].into_iter().enumerate() {
        n[i] = Uuid::now_v7();
        sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, lifecycle) values ($1, 'vps', $2, $3, $4::inet, 'active')")
            .bind(n[i])
            .bind(team)
            .bind(num)
            .bind(format!("203.0.113.{}", 10 + i))
            .execute(db)
            .await
            .unwrap();
        sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, $4, $5, 'ab', 'active')")
            .bind(Uuid::now_v7())
            .bind(relay)
            .bind(n[i])
            .bind(20001 + i as i32)
            .bind(Uuid::now_v7())
            .execute(db)
            .await
            .unwrap();
    }
    Fx { relay, n }
}
