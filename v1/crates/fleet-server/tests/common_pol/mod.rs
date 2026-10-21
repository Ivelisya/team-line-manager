//! 访问策略集成测试的公共部分：造线路机 / 节点 / 入口、模拟 agent、塞访问汇总（建库与登录见 common）。

#![allow(dead_code)]

use axum::http::StatusCode;
use fleet_proto::{ApplyReport, DesiredState, EntryApply};
use serde_json::Value;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

pub use crate::common::T;

impl T {
    /// 期望一个特定状态码，返回错误信息
    pub async fn fail(&self, method: &str, p: &str, b: Option<Value>, want: StatusCode) -> String {
        let (s, v) = self.req(method, p, b, None).await;
        assert_eq!(s, want, "{method} {p}: {v}");
        v["error"]["message"].as_str().unwrap_or("").to_string()
    }

    /// PUT /policies/{scope}/{id}，成功返回 data
    pub async fn put_policy(&self, scope: &str, id: Uuid, body: Value) -> Value {
        let (s, v) = self.req("PUT", &format!("/api/v1/policies/{scope}/{id}"), Some(body), None).await;
        assert_eq!(s, StatusCode::OK, "PUT policy: {v}");
        v["data"].clone()
    }

    pub async fn relay_generation(&self, relay: Uuid) -> i64 {
        sqlx::query_scalar("select generation from relay where machine_id = $1").bind(relay).fetch_one(&self.st.db).await.unwrap()
    }

    // ── 模拟 agent ──

    pub async fn agent_desired(&self, tok: &str) -> DesiredState {
        let (s, v) = self.req("GET", "/agent/v1/desired?after=0", None, Some(tok)).await;
        assert_eq!(s, StatusCode::OK, "desired: {v}");
        serde_json::from_value(v).unwrap()
    }

    /// 回报某个代数的应用结果
    pub async fn agent_applied(&self, tok: &str, generation: u64, ok: bool, ds: &DesiredState) {
        let rep = ApplyReport {
            generation,
            ok,
            error: (!ok).then(|| "xray -test 失败：模拟".to_string()),
            entries: ds.entries.iter().map(|e| EntryApply { id: e.id, ok, error: None }).collect(),
        };
        let (s, v) = self.req("POST", "/agent/v1/applied", Some(serde_json::to_value(&rep).unwrap()), Some(tok)).await;
        assert_eq!(s, StatusCode::NO_CONTENT, "applied: {v}");
    }
}

/// 不启动后台任务：汇总、告警都由测试自己驱动
pub async fn setup() -> T { crate::common::boot(false, "").await }

pub struct Fx {
    pub relay: Uuid,
    pub token: String,
    pub team1: Uuid,
    pub team2: Uuid,
    /// 团队1-01、团队1-02、团队2-01
    pub n: [Uuid; 3],
    pub e: [Uuid; 3],
}

/// 一台线路机（代数 1，已应用）+ 三个节点（两个团队）+ 各一个在用入口 + 节点出站
pub async fn seed(t: &T) -> Fx {
    let db = &t.st.db;
    let (relay, team1, team2) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for (id, name) in [(team1, "团队1"), (team2, "团队2")] {
        sqlx::query("insert into team (id, name) values ($1, $2)").bind(id).bind(name).execute(db).await.unwrap();
    }
    let (sk, pk) = fleet_server::crypto::reality_keypair();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle, activated_at) values ($1, 'relay', '线路机2', '198.51.100.2', 'active', now())")
        .bind(relay)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("insert into relay (machine_id, public_host, reality_private_enc, reality_public_key, min_client_ver, generation, applied_generation) values ($1, 'relay.test', $2, $3, '1.8.2', 1, 1)")
        .bind(relay)
        .bind(t.st.secrets.seal_str(&sk))
        .bind(pk)
        .execute(db)
        .await
        .unwrap();
    let token = "fa1_test_policy_token".to_string();
    sqlx::query("insert into agent_token (id, machine_id, token_hash) values ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(relay)
        .bind(fleet_server::crypto::sha256(token.as_bytes()))
        .execute(db)
        .await
        .unwrap();
    let mut n = [Uuid::nil(); 3];
    let mut e = [Uuid::nil(); 3];
    for (i, (team, num)) in [(team1, 1), (team1, 2), (team2, 1)].into_iter().enumerate() {
        n[i] = Uuid::now_v7();
        e[i] = Uuid::now_v7();
        sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, lifecycle) values ($1, 'vps', $2, $3, $4::inet, 'active')")
            .bind(n[i])
            .bind(team)
            .bind(num)
            .bind(format!("203.0.113.{}", 10 + i))
            .execute(db)
            .await
            .unwrap();
        sqlx::query("insert into node_outbound (machine_id, protocol, host, port, method, secret_enc) values ($1, 'shadowsocks', $2, 8388, 'aes-256-gcm', $3)")
            .bind(n[i])
            .bind(format!("203.0.113.{}", 10 + i))
            .bind(t.st.secrets.seal_str("pw"))
            .execute(db)
            .await
            .unwrap();
        sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, $4, $5, 'ab', 'active')")
            .bind(e[i])
            .bind(relay)
            .bind(n[i])
            .bind(20001 + i as i32)
            .bind(Uuid::now_v7())
            .execute(db)
            .await
            .unwrap();
    }
    Fx { relay, token, team1, team2, n, e }
}

/// 往访问小时汇总里塞一行（`hours_ago` 小时前，整点）
pub async fn put_hourly(t: &T, fx: &Fx, node: Uuid, hours_ago: i64, host: &str, conns: i32, blocked: i32) {
    let hour = OffsetDateTime::now_utc() - Duration::hours(hours_ago);
    let hour = OffsetDateTime::from_unix_timestamp(hour.unix_timestamp() - hour.unix_timestamp().rem_euclid(3600)).unwrap();
    let kind = if host.parse::<std::net::IpAddr>().is_ok() { "ip" } else { "domain" };
    sqlx::query("insert into access_hourly (node_id, relay_id, hour, dest_host, kind, conns, blocked, sources) values ($1, $2, $3, $4, $5, $6, $7, 1)")
        .bind(node)
        .bind(fx.relay)
        .bind(hour)
        .bind(host)
        .bind(kind)
        .bind(conns)
        .bind(blocked)
        .execute(&t.st.db)
        .await
        .unwrap();
}
