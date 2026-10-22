//! 订阅跟随模板：价格、币种、周期、流量上限跟随套餐，账单时区、提前提醒、流量重置跟随供应商；订阅只存到期日和单独设置的值。
//! 需要 PostgreSQL（见 common）。

mod common;

use axum::http::StatusCode;
use common::{seed_relay, seed_team, setup, T};
use serde_json::{json, Value};
use uuid::Uuid;

fn upstream() -> Value { json!({"protocol": "socks5", "host": "p.example", "port": 1080}) }

async fn machine(t: &T, id: &str) -> Value { t.get(&format!("/api/v1/machines/{id}")).await }

async fn put_sub(t: &T, id: &str, body: Value) -> (StatusCode, Value) { t.req("PUT", &format!("/api/v1/machines/{id}/subscription"), Some(body), None).await }

/// 订阅表里存的（不是生效值）：(价格, 币种, 周期, 时区, 提醒, 流量, 重置日)
async fn stored(t: &T, id: &str) -> (Option<i32>, Option<String>, Option<String>, Option<String>, Option<i32>, Option<i32>, Option<i32>) {
    sqlx::query_as("select price_cents, currency, cycle::text, billing_tz, remind_days, traffic_gb, traffic_reset_day from subscription where machine_id = $1::uuid")
        .bind(id)
        .fetch_one(&t.st.db)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscription_follows_plan_and_provider() {
    let t = setup(false).await;
    let pv = t.post("/api/v1/providers", json!({"name": "甲", "code": "A", "timezone": "America/Los_Angeles", "traffic_reset": "billing", "remind_days": 5})).await["id"].clone();
    let plan = t.post("/api/v1/plans", json!({"provider_id": pv, "name": "季付", "price_cents": 3000, "currency": "CNY", "cycle": "quarterly", "traffic_gb": 2000})).await["id"].clone();
    let plan_path = format!("/api/v1/plans/{}", plan.as_str().unwrap());
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let item = |ip: &str, sub: Value| json!({"kind": "proxy", "ip": ip, "team_id": team, "relay_id": relay, "plan_id": plan, "upstream": upstream(), "subscription": sub});

    // 选了套餐就只要到期日：不填到期日报错；供应商对不上报错
    let chk = t.post("/api/v1/provision/check", json!({"items": [
        {"kind": "proxy", "ip": "203.0.113.9", "team_id": team, "relay_id": relay, "plan_id": plan, "upstream": upstream()},
        {"kind": "proxy", "ip": "203.0.113.8", "team_id": team, "relay_id": relay, "upstream": upstream(), "subscription": {"due_date": "2027-01-31"}},
        item("203.0.113.7", json!({"due_date": "2027-01-31"})),
    ]})).await;
    let errs = |i: usize| chk["items"][i]["errors"].to_string();
    assert!(errs(0).contains("要填到期日") && errs(1).contains("没有套餐") && chk["items"][2]["ok"] == true, "{chk}");
    let other = t.post("/api/v1/providers", json!({"name": "乙", "code": "B"})).await["id"].clone();
    let mut bad = item("203.0.113.7", json!({"due_date": "2027-01-31"}));
    bad["provider_id"] = other.clone();
    assert!(t.post("/api/v1/provision/check", json!({"items": [bad]})).await["items"][0]["errors"].to_string().contains("套餐不属于"));

    // 只给到期日：订阅里什么都不抄，生效值全来自模板；1 月 31 日到期、按账单日重置 → 每月 31 号
    let created = t.post("/api/v1/provision", json!({"items": [item("203.0.113.7", json!({"due_date": "2027-01-31"}))]})).await;
    let id = created["jobs"][0]["machine_id"].as_str().unwrap().to_string();
    sqlx::query("update machine set lifecycle = 'active' where id = $1::uuid").bind(&id).execute(&t.st.db).await.unwrap();
    assert_eq!(stored(&t, &id).await, (None, None, None, None, None, None, None));
    let m = machine(&t, &id).await;
    let s = &m["subscription"];
    assert_eq!(
        (s["price_cents"].as_i64(), s["currency"].as_str(), s["cycle"].as_str(), s["billing_tz"].as_str(), s["remind_days"].as_i64(), s["traffic_gb"].as_i64(), s["traffic_reset_day"].as_i64()),
        (Some(3000), Some("CNY"), Some("quarterly"), Some("America/Los_Angeles"), Some(5), Some(2000), Some(31)),
        "{s}"
    );
    assert_eq!((s["overrides"].clone(), m["provider"]["id"].clone()), (json!([]), pv.clone()), "供应商取套餐的");
    assert_eq!((m["sub_template"]["traffic_reset"].as_str(), m["quota"]["limit_gb"].as_i64(), m["quota"]["limit_source"].as_str()), (Some("billing"), Some(2000), Some("plan")));
    // 月费按生效值折算（季付 3000 → 每月 1000）
    let cost = t.get("/api/v1/overview").await["monthly_cost"].clone();
    assert_eq!(cost, json!([{"currency": "CNY", "cents": 1000}]));

    // 改套餐：没单独设置的机器立即跟着变
    t.req("PATCH", &plan_path, Some(json!({"price_cents": 3600, "traffic_gb": 3000})), None).await;
    let s = machine(&t, &id).await["subscription"].clone();
    assert_eq!((s["price_cents"].as_i64(), s["traffic_gb"].as_i64()), (Some(3600), Some(3000)));

    // 单独设置价格：之后改套餐不影响它；和套餐相同的值按跟随存（旧客户端整份带上生效值也一样）
    let full = |price: i64| json!({"due_date": "2027-01-31", "price_cents": price, "currency": "CNY", "cycle": "quarterly", "billing_tz": "America/Los_Angeles",
        "remind_days": 5, "traffic_gb": 3000, "traffic_reset_day": 31, "auto_renew": true});
    let (st, v) = put_sub(&t, &id, full(900)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(stored(&t, &id).await, (Some(900), None, None, None, None, None, None));
    t.req("PATCH", &plan_path, Some(json!({"price_cents": 4000})), None).await;
    let s = machine(&t, &id).await["subscription"].clone();
    assert_eq!((s["price_cents"].as_i64(), s["overrides"].clone(), s["auto_renew"].clone()), (Some(900), json!(["price_cents"]), json!(true)));
    // 清掉单独设置（null）= 重新跟随；没传 auto_renew = 不改
    let (st, v) = put_sub(&t, &id, json!({"due_date": "2027-01-31", "price_cents": null, "traffic_gb": 0})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let m = machine(&t, &id).await;
    let s = &m["subscription"];
    assert_eq!((s["price_cents"].as_i64(), s["traffic_gb"].as_i64(), s["overrides"].clone(), s["auto_renew"].clone()), (Some(4000), Some(0), json!(["traffic_gb"]), json!(true)));
    assert_eq!((m["quota"]["limit_gb"].clone(), m["quota"]["limit_source"].as_str()), (Value::Null, Some("none")), "单独设成 0 = 不限");

    // 供应商改成自然月、改提醒天数：跟随的重置日变 1 号
    t.req("PATCH", &format!("/api/v1/providers/{}", pv.as_str().unwrap()), Some(json!({"traffic_reset": "month", "remind_days": 9})), None).await;
    let s = machine(&t, &id).await["subscription"].clone();
    assert_eq!((s["traffic_reset_day"].as_i64(), s["remind_days"].as_i64()), (Some(1), Some(9)));
    assert_eq!(t.req("PATCH", &format!("/api/v1/providers/{}", pv.as_str().unwrap()), Some(json!({"traffic_reset": "weekly"})), None).await.0, StatusCode::BAD_REQUEST);

    // 续费按生效的周期推（客户端算新到期日），续费后单独设置照旧
    let (st, v) = t.req("POST", &format!("/api/v1/machines/{id}/renewals"), Some(json!({"new_due": "2027-04-30", "amount_cents": 4000})), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let s = machine(&t, &id).await["subscription"].clone();
    assert_eq!((s["due_date"].as_str(), s["cycle"].as_str(), s["overrides"].clone()), (Some("2027-04-30"), Some("quarterly"), json!(["traffic_gb"])));

    // 套餐有机器用着时不能换供应商
    let (st, v) = t.req("PATCH", &plan_path, Some(json!({"provider_id": other})), None).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn machine_without_plan_must_set_price() {
    let t = setup(false).await;
    let (relay, _) = seed_relay(&t).await;
    let rid = relay.to_string();
    let (st, v) = put_sub(&t, &rid, json!({"due_date": "2027-01-01"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("没有套餐"), "{v}");
    for bad in [json!({"cycle": "weekly"}), json!({"remind_days": 61}), json!({"currency": "US$"}), json!({"traffic_reset_day": 32}), json!({"price_cents": -1})] {
        let mut b = json!({"due_date": "2027-01-01", "price_cents": 100});
        b.as_object_mut().unwrap().extend(bad.as_object().unwrap().clone());
        assert_eq!(put_sub(&t, &rid, b.clone()).await.0, StatusCode::BAD_REQUEST, "{b}");
    }
    // 没有供应商：时区、提醒、币种、周期是默认值（北京时间、7 天、USD、按月），重置日按自然月
    let (st, v) = put_sub(&t, &rid, json!({"due_date": "2027-01-15", "price_cents": 500})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let s = machine(&t, &rid).await["subscription"].clone();
    assert_eq!(
        (s["currency"].as_str(), s["cycle"].as_str(), s["billing_tz"].as_str(), s["remind_days"].as_i64(), s["traffic_reset_day"].as_i64(), s["overrides"].clone()),
        (Some("USD"), Some("monthly"), Some("Asia/Shanghai"), Some(7), Some(1), json!(["price_cents"]))
    );
}

/// 按账单日重置：周期从最近一个「到期日几号」起算，月份没有这一天按月末（31 号到期在 2 月是 28 / 29 号）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_day_reset_period() {
    let t = setup(false).await;
    let pv = t.post("/api/v1/providers", json!({"name": "甲", "code": "A", "timezone": "Asia/Tokyo", "traffic_reset": "billing"})).await["id"].clone();
    let (relay, _) = seed_relay(&t).await;
    sqlx::query("update machine set provider_id = $2::uuid where id = $1").bind(relay).bind(pv.as_str().unwrap()).execute(&t.st.db).await.unwrap();
    let (st, v) = put_sub(&t, &relay.to_string(), json!({"due_date": "2030-01-31", "price_cents": 100})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let today: time::Date = sqlx::query_scalar("select (now() at time zone 'Asia/Tokyo')::date").fetch_one(&t.st.db).await.unwrap();
    let m = machine(&t, &relay.to_string()).await;
    assert_eq!(m["subscription"]["traffic_reset_day"], 31);
    assert_eq!(m["quota"]["period_start"], fleet_server::views::period_start(today, 31).to_string());
}

/// 迁移 0026 对已有数据：账单日重置的供应商按数据识别；和模板相同的值改回跟随，不同的留作单独设置；机器的供应商按套餐修正。
/// 迁移可重复执行：在旧写法的数据上再跑一遍
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migration_normalizes_hard_coded_subscriptions() {
    let t = setup(false).await;
    let db = &t.st.db;
    let gm = t.post("/api/v1/providers", json!({"name": "GOMAMI", "code": "GM", "timezone": "America/Los_Angeles"})).await["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let att = t.post("/api/v1/providers", json!({"name": "ATTVPS", "code": "ATT", "timezone": "America/New_York"})).await["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let gm_plan = t.post("/api/v1/plans", json!({"provider_id": gm, "name": "Mini", "price_cents": 3946, "traffic_gb": 2000})).await["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let att_plan = t.post("/api/v1/plans", json!({"provider_id": att, "name": "Base", "price_cents": 1000})).await["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    // (供应商, 套餐, 价格, 时区, 流量, 重置日, 到期日)：上架时抄的价格 / 时区、手填的套餐流量与账单日
    let rows: [(Uuid, Uuid, i32, &str, Option<i32>, Option<i32>, &str); 4] = [
        (gm, gm_plan, 3946, "America/Los_Angeles", Some(2000), Some(13), "2026-11-13"),
        (gm, gm_plan, 4200, "America/Los_Angeles", Some(2000), Some(25), "2026-11-25"),
        (att, att_plan, 1000, "Asia/Shanghai", None, Some(1), "2026-11-05"),
        (att, gm_plan, 1000, "America/New_York", Some(0), None, "2026-11-05"),
    ];
    let mut ids = vec![];
    for (i, (p, plan, price, tz, gb, day, due)) in rows.iter().enumerate() {
        let id = Uuid::now_v7();
        sqlx::query("insert into machine (id, kind, label, primary_ip, provider_id, plan_id, lifecycle) values ($1, 'relay', $2, $3::inet, $4, $5, 'active')")
            .bind(id)
            .bind(format!("线路机{i}"))
            .bind(format!("198.51.100.{}", 10 + i))
            .bind(p)
            .bind(plan)
            .execute(db)
            .await
            .unwrap();
        sqlx::query("insert into subscription (machine_id, price_cents, currency, cycle, due_date, billing_tz, remind_days, traffic_gb, traffic_reset_day) values ($1, $2, 'USD', 'monthly', $3::date, $4, 7, $5, $6)")
            .bind(id)
            .bind(price)
            .bind(due)
            .bind(tz)
            .bind(gb)
            .bind(day)
            .execute(db)
            .await
            .unwrap();
        ids.push(id);
    }
    sqlx::raw_sql(include_str!("../../../migrations/0026_subscription_inherit.sql")).execute(db).await.unwrap();

    let reset: Vec<(String, String)> = sqlx::query_as("select code, traffic_reset from provider order by code").fetch_all(db).await.unwrap();
    assert_eq!(reset, [("ATT".into(), "month".into()), ("GM".into(), "billing".into())], "GM 的重置日都是到期日；ATT 只有 1 号的不算证据");
    let got = |id: Uuid| async move {
        let s: (Option<i32>, Option<String>, Option<String>, Option<String>, Option<i32>, Option<i32>, Option<i32>) = sqlx::query_as(
            "select price_cents, currency, cycle::text, billing_tz, remind_days, traffic_gb, traffic_reset_day from subscription where machine_id = $1",
        )
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap();
        s
    };
    assert_eq!(got(ids[0]).await, (None, None, None, None, None, None, None), "全和模板相同：只剩到期日");
    assert_eq!(got(ids[1]).await, (Some(4200), None, None, None, None, None, None), "价格不同，留作单独设置");
    assert_eq!(got(ids[2]).await, (None, None, None, Some("Asia/Shanghai".into()), None, None, None), "时区和供应商不同；1 号 = 自然月");
    // 第 4 台：机器的供应商（ATT）和套餐的（GM）不一致 → 按套餐修正；流量 0 = 不限，和套餐 2000 不同
    let pv: Uuid = sqlx::query_scalar("select provider_id from machine where id = $1").bind(ids[3]).fetch_one(db).await.unwrap();
    assert_eq!(pv, gm);
    assert_eq!(got(ids[3]).await, (Some(1000), None, None, Some("America/New_York".into()), None, Some(0), None));
    // 生效值不变（价格、流量），重置日按 GM 的账单日
    let s = machine(&t, &ids[3].to_string()).await["subscription"].clone();
    assert_eq!((s["price_cents"].as_i64(), s["traffic_gb"].as_i64(), s["traffic_reset_day"].as_i64()), (Some(1000), Some(0), Some(5)));
}
