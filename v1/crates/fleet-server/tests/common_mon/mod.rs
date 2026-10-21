//! 访问监控测试的公共部分：造机器和入口、塞访问记录（建库与登录见 common）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use uuid::Uuid;

pub use crate::common::T;

impl T {
    /// GET，`with_cookie` 为假时不带会话（测未登录）
    pub async fn raw(&self, path: &str, with_cookie: bool) -> (StatusCode, Value) {
        let mut b = Request::builder().method("GET").uri(path);
        if with_cookie {
            b = b.header("cookie", &self.cookie);
        }
        let res = self.app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}

/// 不启动后台任务：汇总由测试自己调用，结果才可预期
pub async fn setup() -> T {
    let t = crate::common::boot(false, "access_cache_secs = 0").await;
    // 测试要塞过去几天的记录：补齐最近 6 天的 access_event 分区（不超过保留期）
    let today = OffsetDateTime::now_utc().date();
    for i in 1..=6 {
        let d = today - Duration::days(i);
        let n = format!("access_event_{:04}{:02}{:02}", d.year(), u8::from(d.month()), d.day());
        let sql = format!("create table if not exists {n} partition of access_event for values from ('{d}') to ('{e}')", e = d + Duration::days(1));
        sqlx::query(&sql).execute(&t.st.db).await.unwrap();
    }
    t
}

pub struct Fx {
    pub relay: Uuid,
    pub team1: Uuid,
    pub team2: Uuid,
    /// 团队1-01、团队1-02、团队2-01
    pub n: [Uuid; 3],
    pub e: [Uuid; 3],
}

/// 一台线路机 + 三个节点（两个团队）+ 各一个入口
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
    Fx { relay, team1, team2, n, e }
}

/// 塞 `count` 条记录，第 i 条的时间是 `at + i 秒`
pub async fn put_events(t: &T, entry: Uuid, at: OffsetDateTime, src: Option<&str>, host: &str, count: usize, blocked: bool) {
    for i in 0..count {
        sqlx::query("insert into access_event (entry_id, ts, src_ip, dest_host, dest_port, network, blocked) values ($1, $2, $3::inet, $4, 443, 'tcp', $5)")
            .bind(entry)
            .bind(at + Duration::seconds(i as i64))
            .bind(src)
            .bind(host)
            .bind(blocked)
            .execute(&t.st.db)
            .await
            .unwrap();
    }
}
