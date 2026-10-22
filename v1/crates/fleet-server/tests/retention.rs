//! 访问记录保留期（面板里设的，见 settings.rs）：默认值、改值与边界、权限、审计与实时事件；改短后各表过期的删掉、保留期内的不删，
//! 改长后不误删，学习期比汇总长时来源画像以学习期为准；访问页的窗口不能超过汇总的保留期；上报只收原始记录保留期以内的。
//! 需要 PostgreSQL（见 common）。不起后台循环：改了之后的清理由接口自己在后台跑，测试等它删完。

mod common;
mod common_mon;
mod common_tele;

use axum::http::StatusCode;
use common_mon::{put_events, seed, setup, T};
use fleet_server::db::floor_hour;
use fleet_server::settings::{self, AccessRetention};
use fleet_server::state::Event;
use serde_json::{json, Value};
use std::time::Duration as StdDuration;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const URL: &str = "/api/v1/settings/access-retention";

async fn count(t: &T, sql: &str) -> i64 { sqlx::query_scalar::<_, i64>(sql).fetch_one(&t.st.db).await.unwrap() }

/// PUT 保留期（会话），返回 (状态码, 响应体)
async fn put(t: &T, raw: i64, hourly: i64) -> (StatusCode, Value) { t.req("PUT", URL, Some(json!({"raw_days": raw, "hourly_days": hourly})), None).await }

/// 等后台清理删到 `sql` 数出来是 `want`（接口改完保留期在后台清，测试没法直接等它）；20 秒还不到就失败
async fn wait_count(t: &T, sql: &str, want: i64) {
    for _ in 0..200 {
        if count(t, sql).await == want {
            return;
        }
        tokio::time::sleep(StdDuration::from_millis(100)).await;
    }
    panic!("20 秒内没等到 {sql} = {want}（现在是 {}）", count(t, sql).await);
}

/// 某天的 access_event 分区还在不在
async fn partition(t: &T, ago_days: i64) -> bool {
    let d = OffsetDateTime::now_utc().date() - Duration::days(ago_days);
    let n = format!("access_event_{:04}{:02}{:02}", d.year(), u8::from(d.month()), d.day());
    count(t, &format!("select count(*) from pg_class where relname = '{n}' and relnamespace = (select oid from pg_namespace where nspname = current_schema())")).await == 1
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn defaults_bounds_permissions_audit_and_event() {
    let t = setup().await;
    // 没设过：原始记录按配置文件的 access_days（默认 7），汇总 31 天；来源画像不短于学习期（默认 7 天），这里就是 31
    assert_eq!(t.get(URL).await, json!({"raw_days": 7, "hourly_days": 31, "min": 1, "max": 90, "profile_days": 31}));

    // 边界：0、负数、91、汇总比原始记录短都 400，什么都不写
    for (raw, hourly, why) in [(0, 31, "1–90 天"), (-3, 31, "1–90 天"), (7, 91, "1–90 天"), (91, 91, "1–90 天"), (7, 0, "1–90 天"), (7, 6, "至少 7 天")] {
        let (s, v) = put(&t, raw, hourly).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{raw} {hourly}: {v}");
        assert!(v["error"]["message"].as_str().unwrap().contains(why), "{raw} {hourly}: {v}");
    }
    let (s, _) = t.req("PUT", URL, Some(json!({"raw_days": 3})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "缺 hourly_days");
    assert_eq!(count(&t, "select count(*) from app_setting").await, 0);

    // 只读密钥能看不能改
    let key = |name: &'static str, scopes: Value| {
        let t = &t;
        async move { t.post("/api/v1/api-keys", json!({"name": name, "scopes": scopes, "password": common::ADMIN_PW})).await["token"].as_str().unwrap().to_string() }
    };
    let ro = key("ro", json!(["read"])).await;
    let (s, v) = t.req("GET", URL, None, Some(&ro)).await;
    assert_eq!((s, v["data"]["hourly_days"].as_i64()), (StatusCode::OK, Some(31)));
    let (s, v) = t.req("PUT", URL, Some(json!({"raw_days": 3, "hourly_days": 14})), Some(&ro)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");

    // 改（带上 GET 的 min / max 也行）：返回新值、发实时事件、写审计（从几天改到几天）、库里一行、内存里的值跟着变
    let mut rx = t.st.events.subscribe();
    let (s, v) = t.req("PUT", URL, Some(json!({"raw_days": 3, "hourly_days": 14, "min": 1, "max": 90})), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"], json!({"raw_days": 3, "hourly_days": 14, "min": 1, "max": 90, "profile_days": 14}));
    let ev = tokio::time::timeout(StdDuration::from_secs(5), async {
        loop {
            let e = rx.recv().await.unwrap();
            if matches!(e, Event::Settings) {
                return e;
            }
        }
    })
    .await
    .expect("5 秒内没收到 settings 事件");
    assert_eq!(ev.to_text().as_str(), r#"{"type":"settings"}"#);
    assert_eq!(t.st.access_retention(), AccessRetention { raw_days: 3, hourly_days: 14 });
    let (value, by): (Value, String) = sqlx::query_as("select value, updated_by from app_setting where key = 'access_retention'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!((value, by.as_str()), (json!({"raw_days": 3, "hourly_days": 14}), "admin"));
    let audit = t.get("/api/v1/audit?limit=5").await;
    let a = audit.as_array().unwrap().iter().find(|a| a["action"] == "settings.access_retention").unwrap_or_else(|| panic!("没有审计：{audit}"));
    assert_eq!((a["title"].as_str(), a["summary"].as_str(), a["actor"].as_str()), (Some("修改访问记录保留期"), Some("原始记录 7 → 3 天，汇总 31 → 14 天"), Some("admin")));
    assert_eq!(a["detail"], json!({"from": {"raw_days": 7, "hourly_days": 31}, "to": {"raw_days": 3, "hourly_days": 14}}));

    // 一样的值再提交：照样 200，不写审计
    let (s, _) = put(&t, 3, 14).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(count(&t, "select count(*) from audit_log where action = 'settings.access_retention'").await, 1);

    // write 密钥能改，审计记密钥名；两头的边界值本身能存
    let rw = key("rw", json!(["read", "write"])).await;
    let (s, v) = t.req("PUT", URL, Some(json!({"raw_days": 1, "hourly_days": 1})), Some(&rw)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // 学习期 7 天比汇总长：来源画像按学习期留
    assert_eq!(v["data"]["profile_days"], 7);
    let actor: String = sqlx::query_scalar("select actor from audit_log where action = 'settings.access_retention' order by id desc limit 1").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(actor, "apikey:rw");
    let (s, _) = put(&t, 90, 90).await;
    assert_eq!(s, StatusCode::OK);

    // 重启时从库里读回来；库里的值被手工改坏了明确报错，不悄悄换成默认的
    sqlx::query("update app_setting set value = '{\"raw_days\": 5, \"hourly_days\": 20}' where key = 'access_retention'").execute(&t.st.db).await.unwrap();
    assert_eq!(settings::load(&t.st).await.unwrap(), AccessRetention { raw_days: 5, hourly_days: 20 });
    assert_eq!(t.get(URL).await["hourly_days"], 20);
    sqlx::query("update app_setting set value = '{\"raw_days\": 9, \"hourly_days\": 5}' where key = 'access_retention'").execute(&t.st.db).await.unwrap();
    assert!(settings::load(&t.st).await.unwrap_err().to_string().contains("不对"));
}

/// 各表都放「保留期内」和「过期」的行，先改长（不误删），再改短（过期的删掉、保留期内的留着），再短到学习期以内（来源画像按学习期）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shortening_deletes_expired_rows_everywhere_lengthening_keeps_them() {
    let t = setup().await;
    let fx = seed(&t).await;
    let (n1, e1) = (fx.n[0], fx.e[0]);
    let db = &t.st.db;
    let now = OffsetDateTime::now_utc();
    let ago = |d: i64| now - Duration::days(d);
    let hour_ago = |d: i64| floor_hour(now) - Duration::days(d);

    // 原始记录：现在、2 天前、5 天前（分区在 common_mon::setup 里建好了）。要有现在的：删过期分区前先拿表里最新的数据核对本机时钟
    // （db::drop_before），最新的离现在超过一天就不删
    put_events(&t, e1, now, Some("1.1.1.1"), "now.example.com", 1, false).await;
    put_events(&t, e1, ago(2), Some("1.1.1.2"), "two.example.com", 1, false).await;
    put_events(&t, e1, ago(5), Some("1.1.1.5"), "five.example.com", 1, false).await;
    // 小时汇总、来源小时汇总：2 / 5 / 10 / 20 / 40 天前（40 天的超出现在默认的 31 天，用来看改长后不误删）
    for d in [2, 5, 10, 20, 40] {
        sqlx::query("insert into access_hourly (node_id, relay_id, hour, dest_host, kind, conns) values ($1, $2, $3, $4, 'domain', 1)")
            .bind(n1)
            .bind(fx.relay)
            .bind(hour_ago(d))
            .bind(format!("h{d}.example.com"))
            .execute(db)
            .await
            .unwrap();
        sqlx::query("insert into access_source_hourly (node_id, relay_id, hour, src_ip, conns) values ($1, $2, $3, $4::inet, 1)")
            .bind(n1)
            .bind(fx.relay)
            .bind(hour_ago(d))
            .bind(format!("203.0.113.{d}"))
            .execute(db)
            .await
            .unwrap();
    }
    for d in [5, 10, 20, 40] {
        let ip = format!("198.51.100.{d}");
        sqlx::query("insert into access_source_late (node_id, hour, src_ip, delta) values ($1, $2, $3::inet, 1)").bind(n1).bind(hour_ago(d)).bind(&ip).execute(db).await.unwrap();
        sqlx::query("insert into access_dirty (hour) values ($1)").bind(hour_ago(d)).execute(db).await.unwrap();
        sqlx::query("insert into access_target_seen (node_id, dest_host, first_seen, last_seen, total) values ($1, $2, $3, $3, 1)")
            .bind(n1)
            .bind(format!("seen{d}.example.com"))
            .bind(ago(d))
            .execute(db)
            .await
            .unwrap();
        sqlx::query("insert into ip_intel_cache (ip, isp, db_version, updated_at) values ($1::inet, '某运营商', 'v', $2)").bind(&ip).bind(ago(d)).execute(db).await.unwrap();
        // 来源画像四个维度都有；另有一条在告警中的（open）
        for dim in ["isp", "city", "conn_type", "ip"] {
            sqlx::query("insert into node_source_seen (node_id, dim, value, first_seen, last_seen, conns) values ($1, $2, $3, $4, $4, 1)")
                .bind(n1)
                .bind(dim)
                .bind(format!("{dim}-{d}"))
                .bind(ago(d))
                .execute(db)
                .await
                .unwrap();
        }
        // 画像异常告警：已恢复的按保留期删；别的规则的已恢复告警不归它管
        for rule in ["ipintel_new_city", "agent_offline"] {
            sqlx::query("insert into incident (id, rule, subject_id, severity, state, summary, opened_at, resolved_at) values ($1, $2, $3, 'warning', 'resolved', 's', $4, $4)")
                .bind(Uuid::now_v7())
                .bind(rule)
                .bind(n1)
                .bind(ago(d))
                .execute(db)
                .await
                .unwrap();
        }
    }
    sqlx::query("insert into node_source_seen (node_id, dim, value, first_seen, last_seen, flag) values ($1, 'ip', '192.0.2.200', $2, $2, 'open')").bind(n1).bind(ago(40)).execute(db).await.unwrap();
    sqlx::query("insert into incident (id, rule, subject_id, severity, summary, opened_at) values ($1, 'ipintel_new_isp', $2, 'warning', 's', $3)")
        .bind(Uuid::now_v7())
        .bind(n1)
        .bind(ago(40))
        .execute(db)
        .await
        .unwrap();

    let hourly = |table: &str| format!("select count(*) from {table} where node_id = '{n1}'");
    let counts = || async {
        let mut v = vec![];
        for sql in [
            "select count(*) from access_event".to_string(),
            hourly("access_hourly"),
            hourly("access_source_hourly"),
            hourly("access_source_late"),
            "select count(*) from access_dirty".to_string(),
            hourly("access_target_seen"),
            "select count(*) from ip_intel_cache".to_string(),
            "select count(*) from node_source_seen where flag is null".to_string(),
            "select count(*) from node_source_seen where flag = 'open'".to_string(),
            "select count(*) from incident where rule like 'ipintel!_%' escape '!' and state = 'resolved'".to_string(),
            "select count(*) from incident where state = 'firing'".to_string(),
            "select count(*) from incident where rule = 'agent_offline'".to_string(),
        ] {
            v.push(count(&t, &sql).await);
        }
        v
    };
    let all = vec![3, 5, 5, 4, 4, 4, 4, 16, 1, 4, 1, 4];
    assert_eq!(counts().await, all);

    // 改长（14 天原始记录、60 天汇总）：一条都不删，包括超出原来 31 天、在新期限内的
    let (s, v) = put(&t, 14, 60).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    settings::enforce(&t.st).await.unwrap();
    assert_eq!(counts().await, all, "改长后不误删");
    assert!(partition(&t, 5).await);

    // 改短（3 天、14 天）：接口在后台马上清，不等每小时的维护。等最后一步（画像异常告警）删完
    let (s, _) = put(&t, 3, 14).await;
    assert_eq!(s, StatusCode::OK);
    wait_count(&t, "select count(*) from incident where rule like 'ipintel!_%' escape '!' and state = 'resolved'", 2).await;
    // 原始记录：5 天前的整天分区删了；汇总：只剩 14 天以内的；来源画像各维度同（学习期 7 天，比汇总短）；在告警中的、别的告警不动
    assert_eq!(counts().await, vec![2, 3, 3, 2, 2, 2, 2, 8, 1, 2, 1, 4]);
    assert!(partition(&t, 2).await && !partition(&t, 5).await, "5 天前的分区删掉，2 天前的留着");
    let left: Vec<String> = sqlx::query_scalar("select dest_host from access_target_seen order by 1").fetch_all(db).await.unwrap();
    assert_eq!(left, ["seen10.example.com", "seen5.example.com"]);
    let left: Vec<String> = sqlx::query_scalar("select distinct dim from node_source_seen where value like '%-10' order by 1").fetch_all(db).await.unwrap();
    assert_eq!(left, ["city", "conn_type", "ip", "isp"], "运营商 / 城市 / 接入方式也按期删，不再永久保留");

    // 再短到学习期以内（1 天、3 天）：汇总只剩 3 天内的，来源画像与画像异常告警按学习期留 7 天
    let (s, v) = put(&t, 1, 3).await;
    assert_eq!((s, v["data"]["profile_days"].as_i64()), (StatusCode::OK, Some(7)));
    wait_count(&t, "select count(*) from incident where rule like 'ipintel!_%' escape '!' and state = 'resolved'", 1).await;
    assert_eq!(counts().await, vec![1, 1, 1, 0, 0, 0, 0, 4, 1, 1, 1, 4]);
    assert!(!partition(&t, 2).await);
    let left: Vec<String> = sqlx::query_scalar("select value from node_source_seen where flag is null order by 1").fetch_all(db).await.unwrap();
    assert_eq!(left, ["city-5", "conn_type-5", "ip-5", "isp-5"], "5 天前的来源画像还在学习期以内");
}

/// 访问页各视图的窗口不能超过汇总的保留期；原始记录列表照旧按窗口回看
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_cannot_exceed_hourly_retention() {
    let t = setup().await;
    let fx = seed(&t).await;
    let n1 = fx.n[0];
    let views = [
        "/api/v1/access/summary".to_string(),
        "/api/v1/access/targets".to_string(),
        "/api/v1/access/sources".to_string(),
        "/api/v1/access/new-targets".to_string(),
        format!("/api/v1/machines/{n1}/access"),
        format!("/api/v1/machines/{n1}/sources"),
    ];
    for v in &views {
        let (s, _) = t.raw(&format!("{v}?window=30d"), true).await;
        assert_eq!(s, StatusCode::OK, "默认 31 天，30d 能看：{v}");
    }
    let (s, _) = put(&t, 2, 5).await;
    assert_eq!(s, StatusCode::OK);
    for v in &views {
        for w in ["7d", "30d"] {
            let (s, body) = t.raw(&format!("{v}?window={w}"), true).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{v} {w}");
            let msg = body["error"]["message"].as_str().unwrap();
            assert!(msg.contains("只保留 5 天") && msg.contains(if w == "7d" { "最近 7 天" } else { "最近 30 天" }), "{msg}");
        }
        let (s, _) = t.raw(&format!("{v}?window=24h"), true).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    let (s, _) = t.raw("/api/v1/access/summary?window=1h", true).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = t.raw("/api/v1/access/events?window=30d", true).await;
    assert_eq!(s, StatusCode::OK, "原始记录列表不拦，更早的只是已经删了");
    // 汇总留 7 天：7d 又能看了
    let (s, _) = put(&t, 2, 7).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = t.raw("/api/v1/access/summary?window=7d", true).await;
    assert_eq!(s, StatusCode::OK);
}

/// 上报只收原始记录保留期以内的访问记录：设成 1 天后，2 天前的不再入库
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_accepts_access_events_only_within_raw_retention() {
    use common_tele::{access, blank_report, now_ms, seed_node, seed_relay, seed_team};
    let t = common_tele::setup().await;
    let relay = seed_relay(&t, "线路机1", "198.51.100.1", "tok1").await;
    let team = seed_team(&t, "团队1").await;
    let (_node, entry) = seed_node(&t, relay, team, 1, "203.0.113.5", 20000).await;
    let (s, _) = put(&t, 1, 3).await;
    assert_eq!(s, StatusCode::OK);
    let now = now_ms();
    let mut r = blank_report(1);
    r.access = vec![access(entry, now - 5_000), access(entry, now - 2 * 86_400_000)];
    let (s, v) = t.report("tok1", &r).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(t.count("select count(*) from access_event").await, 1, "2 天前的超出 1 天的保留期，丢掉");
}
