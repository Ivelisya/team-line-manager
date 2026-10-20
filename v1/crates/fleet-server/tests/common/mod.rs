//! 集成测试的公共搭建。需要 PostgreSQL：环境变量 FLEET_TEST_DB（默认 postgres://fleet:fleet@127.0.0.1/fleet_test）。
//! 每次 `setup` 都会清空该库的 public schema，所以测试要串行跑（`--test-threads=1`）。

#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use fleet_server::{config::Config, crypto::SecretBox, db, state::AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

pub struct T {
    pub app: axum::Router,
    pub st: AppState,
    pub cookie: String,
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
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("作业 {id} 没到 {want}：{}", self.job(id).await);
    }

    pub async fn machine_lifecycle(&self, id: Uuid) -> String {
        sqlx::query_scalar("select lifecycle::text from machine where id = $1").bind(id).fetch_one(&self.st.db).await.unwrap()
    }
}

/// 起一个全新的测试环境。`background` 为真时启动作业引擎等后台任务；为假则作业不会自己跑（测试手动领取、推进）。
pub async fn setup(background: bool) -> T {
    let url = std::env::var("FLEET_TEST_DB").unwrap_or_else(|_| "postgres://fleet:fleet@127.0.0.1/fleet_test".into());
    let pool = db::connect(&url).await.unwrap();
    sqlx::query("drop schema public cascade").execute(&pool).await.unwrap();
    sqlx::query("create schema public").execute(&pool).await.unwrap();
    db::MIGRATOR.run(&pool).await.unwrap();
    db::maintain_partitions(&pool, 14, 7, 2).await.unwrap();
    let cfg: Config = toml::from_str(&format!(
        r#"database_url = "{url}"
           secret_key_file = "/dev/null"
           public_url = "http://fleet.test"
           ssh_key = "/nonexistent"
           known_hosts = "/tmp/fleet-test-known-hosts"
           dist_dir = "/nonexistent"
           job_workers = 4"#
    ))
    .unwrap();
    let st = AppState::new(cfg, pool.clone(), SecretBox::from_key(&[9u8; 32]).unwrap());
    if background {
        fleet_server::spawn_background(&st);
    }
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
    T { app, st, cookie }
}

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
