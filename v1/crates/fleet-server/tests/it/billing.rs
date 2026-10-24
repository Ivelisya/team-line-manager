//! 订阅跟随模板：价格、币种、周期、流量上限跟随套餐，账单时区、提前提醒、流量重置跟随供应商；订阅只存到期日和单独设置的值。
//! 需要 PostgreSQL（见 common）。

use crate::common;

use axum::http::StatusCode;
use common::{seed_relay, seed_team, setup, T};
use serde_json::{json, Value};
use uuid::Uuid;

/// (供应商, 套餐, 价格, 时区, 流量, 重置日, 到期日)
type Row<'a> = (Uuid, Uuid, i32, &'a str, Option<i32>, Option<i32>, &'a str);
/// 订阅里存下来的单独设置：价格、币种、周期、时区、提醒天数、流量、重置日
type StoredSub = (Option<i32>, Option<String>, Option<String>, Option<String>, Option<i32>, Option<i32>, Option<i32>);

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
    let cost = t.get("/api/v1/overview").await["monthly_usd"]["unconverted"].clone();
    assert_eq!(cost, json!([{"currency": "CNY", "cents": 1000}]), "测试里没有汇率，CNY 单列未折算");

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
    // 预览（POST …/subscription/preview，独立路由）：返回规整后的生效值与 overrides，库里什么都不变、不记审计
    let audits = t.count("select count(*) from audit_log where action = 'subscription.set'").await;
    let dry = format!("/api/v1/machines/{id}/subscription/preview");
    let (st, v) = t.req("POST", &dry, Some(json!({"due_date": "2027-02-28", "price_cents": 1234, "traffic_gb": 3000})), None).await;
    let e = &v["data"]["effective"];
    assert_eq!(
        (st, v["data"]["dry_run"].clone(), e["price_cents"].as_i64(), e["due_date"].as_str(), e["overrides"].clone()),
        (StatusCode::OK, json!(true), Some(1234), Some("2027-02-28"), json!(["price_cents"])),
        "和套餐相同的流量上限按跟随，不算单独设置：{v}"
    );
    assert_eq!(stored(&t, &id).await, (Some(900), None, None, None, None, None, None), "预览不写库");
    assert_eq!(machine(&t, &id).await["subscription"]["due_date"], "2027-01-31");
    assert_eq!(t.count("select count(*) from audit_log where action = 'subscription.set'").await, audits);
    assert_eq!(t.req("POST", &dry, Some(json!({"due_date": "2027-02-28", "cycle": "weekly"})), None).await.0, StatusCode::BAD_REQUEST, "预览同样校验");
    assert_eq!(t.count("select count(*) from audit_log where action = 'subscription.set'").await, audits, "校验失败的预览同样不留审计");
    let nobody = format!("/api/v1/machines/{}/subscription/preview", Uuid::nil());
    assert_eq!(t.req("POST", &nobody, Some(json!({"due_date": "2027-02-28", "price_cents": 1})), None).await.0, StatusCode::NOT_FOUND, "没有这台机器");
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

    // 续费：直接给新到期日；续费后单独设置照旧
    let renew = format!("/api/v1/machines/{id}/renewals");
    let (st, v) = t.req("POST", &renew, Some(json!({"new_due": "2027-04-30", "amount_cents": 4000})), None).await;
    assert_eq!((st, v["data"]["old_due"].as_str(), v["data"]["new_due"].as_str()), (StatusCode::OK, Some("2027-01-31"), Some("2027-04-30")), "{v}");
    let s = machine(&t, &id).await["subscription"].clone();
    assert_eq!((s["due_date"].as_str(), s["cycle"].as_str(), s["overrides"].clone()), (Some("2027-04-30"), Some("quarterly"), json!(["traffic_gb"])));
    // 或给周期数：中控按生效的周期从当前到期日一次推够（小月按月末）；两个都给、都不给、0 期都是 400
    for bad in [json!({}), json!({"cycles": 0}), json!({"cycles": 1, "new_due": "2028-01-01"})] {
        assert_eq!(t.req("POST", &renew, Some(bad.clone()), None).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (st, v) = t.req("POST", &renew, Some(json!({"cycles": 2, "amount_cents": 8000})), None).await;
    assert_eq!((st, v["data"]["old_due"].as_str(), v["data"]["new_due"].as_str()), (StatusCode::OK, Some("2027-04-30"), Some("2027-10-31")), "{v}");
    // 视图带出：下次续费默认推到哪天、这一期几天（2027-07-31 → 10-31）、折成每月（季付 4000 → 1333）、到期状态
    let s = machine(&t, &id).await["subscription"].clone();
    assert_eq!(
        (s["next_due"].as_str(), s["period_days"].as_i64(), s["monthly_cents"].as_i64(), s["due_state"].as_str()),
        (Some("2028-01-31"), Some(92), Some(1333), Some("ok")),
        "{s}"
    );

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
    // 没有供应商：时区、提醒、币种、周期是默认值（组织时区、7 天、USD、按月），重置日按自然月
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

    // 按周期续费落在账单日（31 号），不从落到月末的 2-28 接着漂成 3-28；重置日跟着账单日
    let rid = relay.to_string();
    let renew = format!("/api/v1/machines/{rid}/renewals");
    let (st, v) = t.req("POST", &renew, Some(json!({"cycles": 1})), None).await;
    assert_eq!((st, v["data"]["new_due"].as_str()), (StatusCode::OK, Some("2030-02-28")), "{v}");
    let s = machine(&t, &rid).await["subscription"].clone();
    assert_eq!((s["billing_day"].as_i64(), s["traffic_reset_day"].as_i64(), s["next_due"].as_str()), (Some(31), Some(31), Some("2030-03-31")), "{s}");
    // 改订阅别的项、到期日不变：账单日不动
    assert_eq!(put_sub(&t, &rid, json!({"due_date": "2030-02-28", "price_cents": 200})).await.0, StatusCode::OK);
    let (st, v) = t.req("POST", &renew, Some(json!({"cycles": 1})), None).await;
    assert_eq!((st, v["data"]["new_due"].as_str()), (StatusCode::OK, Some("2030-03-31")), "{v}");
    // 直接给新到期日：落在账单日上的（网页默认填的 next_due，小月的月末）账单日不变
    let (st, v) = t.req("POST", &renew, Some(json!({"new_due": "2030-04-30"})), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let s = machine(&t, &rid).await["subscription"].clone();
    assert_eq!((s["billing_day"].as_i64(), s["next_due"].as_str()), (Some(31), Some("2030-05-31")), "{s}");
    // 别的日子：账单日改成它几号，重置日跟着
    let (st, v) = t.req("POST", &renew, Some(json!({"new_due": "2030-06-15"})), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let s = machine(&t, &rid).await["subscription"].clone();
    assert_eq!((s["billing_day"].as_i64(), s["traffic_reset_day"].as_i64(), s["next_due"].as_str()), (Some(15), Some(15), Some("2030-07-15")), "{s}");
    // 改订阅改了到期日：同样改账单日
    assert_eq!(put_sub(&t, &rid, json!({"due_date": "2030-06-29", "price_cents": 200})).await.0, StatusCode::OK);
    assert_eq!(machine(&t, &rid).await["subscription"]["billing_day"].as_i64(), Some(29));
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
    let rows: [Row; 4] = [
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
    // 0084 给两个视图末尾加了列，旧迁移的 create or replace view 不能删列：先删掉视图（只有它们互相依赖），旧迁移重放后再跑 0084 补回
    sqlx::raw_sql("drop view subscription_eff; drop view sub_template").execute(db).await.unwrap();
    sqlx::raw_sql(include_str!("../../../../migrations/0026_subscription_inherit.sql")).execute(db).await.unwrap();
    // 0026 换回了按到期日的重置日：再跑 0032（同样可重复执行）回到按账单日
    sqlx::raw_sql(include_str!("../../../../migrations/0032_billing_day.sql")).execute(db).await.unwrap();
    sqlx::raw_sql(include_str!("../../../../migrations/0084_traffic_rule_override.sql")).execute(db).await.unwrap();

    let reset: Vec<(String, String)> = sqlx::query_as("select code, traffic_reset from provider order by code").fetch_all(db).await.unwrap();
    assert_eq!(reset, [("ATT".into(), "month".into()), ("GM".into(), "billing".into())], "GM 的重置日都是到期日；ATT 只有 1 号的不算证据");
    let got = |id: Uuid| async move {
        let s: StoredSub = sqlx::query_as(
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

/// GET /meta：枚举的中文名、阈值、默认值与组织时区只在中控一处；到期状态按生效的提前提醒天数，总览照它计数
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn meta_and_due_state() {
    let t = setup(false).await;
    let m = t.get("/api/v1/meta").await;
    assert_eq!(
        (m["org_timezone"].as_str(), m["kinds"]["vps"].as_str(), m["job_kinds"]["restore_vps"].as_str(), m["healths"]["maintenance"].as_str()),
        (Some("Asia/Shanghai"), Some("VPS 节点"), Some("恢复 VPS 节点"), Some("维护"))
    );
    assert_eq!((m["cycles"].as_array().map(Vec::len), m["thresholds"]["traffic_quota_warn_pct"].as_f64()), (Some(4), Some(90.0)));
    assert_eq!((m["rules"]["renewal_due"].as_str(), m["severities"]["critical"]["label"].as_str()), (Some("即将到期"), Some("严重")));
    assert!(!m.to_string().contains("落地机"), "{m}");

    let (relay, _) = seed_relay(&t).await;
    let rid = relay.to_string();
    let today: time::Date = sqlx::query_scalar("select local_date(org_tz())").fetch_one(&t.st.db).await.unwrap();
    for (days, remind, want, soon, overdue) in [(3, 7, "soon", 1, 0), (3, 2, "ok", 0, 0), (-1, 7, "overdue", 0, 1)] {
        let due = (today + time::Duration::days(days)).to_string();
        let (st, v) = put_sub(&t, &rid, json!({"due_date": due, "price_cents": 100, "remind_days": remind})).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let s = machine(&t, &rid).await["subscription"].clone();
        assert_eq!(s["due_state"].as_str(), Some(want), "{s}");
        let ov = t.get("/api/v1/overview").await;
        assert_eq!((ov["due_soon"].as_i64(), ov["overdue"].as_i64()), (Some(soon), Some(overdue)), "{ov}");
    }
    // 币种只认 3 个字母（套餐、订阅同一个校验）
    for bad in ["USDT", "US", "U$D"] {
        assert_eq!(put_sub(&t, &rid, json!({"due_date": "2027-01-01", "price_cents": 1, "currency": bad})).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
}

async fn del(t: &T, p: &str) -> (StatusCode, Value) { t.req("DELETE", p, None, None).await }

/// 删团队 / 供应商 / 套餐：还有机器（含已退役的）、套餐用着时 409 说清楚还有什么；没人用了才删，记审计
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_delete_refuses_while_in_use() {
    let t = setup(false).await;
    let pv = t.post("/api/v1/providers", json!({"name": "甲", "code": "A"})).await["id"].as_str().unwrap().to_string();
    let plan = t.post("/api/v1/plans", json!({"provider_id": pv, "name": "季付", "price_cents": 3000})).await["id"].as_str().unwrap().to_string();
    let team = seed_team(&t, "团队1").await;
    let node = common::seed_node(&t, team, 1, "203.0.113.20", "retired").await;
    sqlx::query("update machine set plan_id = $2::uuid, provider_id = $3::uuid where id = $1").bind(node).bind(&plan).bind(&pv).execute(&t.st.db).await.unwrap();

    let (s, v) = del(&t, &format!("/api/v1/teams/{team}")).await;
    assert!(s == StatusCode::CONFLICT && v.to_string().contains("1 台已退役的机器"), "{s} {v}");
    let (s, v) = del(&t, &format!("/api/v1/providers/{pv}")).await;
    assert!(s == StatusCode::CONFLICT && v.to_string().contains("1 个套餐") && v.to_string().contains("1 台机器"), "{s} {v}");
    let (s, v) = del(&t, &format!("/api/v1/plans/{plan}")).await;
    assert!(s == StatusCode::CONFLICT && v.to_string().contains("归档"), "{s} {v}");

    // 机器没了（彻底删除）之后依次都能删
    sqlx::query("delete from machine where id = $1").bind(node).execute(&t.st.db).await.unwrap();
    for p in [format!("/api/v1/plans/{plan}"), format!("/api/v1/providers/{pv}"), format!("/api/v1/teams/{team}")] {
        let (s, v) = del(&t, &p).await;
        assert_eq!(s, StatusCode::OK, "{p}: {v}");
    }
    let left: Vec<Value> = t.get("/api/v1/teams").await.as_array().unwrap().iter().map(|x| x["name"].clone()).collect();
    assert_eq!(left, [json!("备用")], "只剩迁移建的备用团队");
    let audit = t.get("/api/v1/audit").await;
    let titles: Vec<&str> = audit.as_array().unwrap().iter().filter_map(|a| a["title"].as_str()).collect();
    assert!(titles.starts_with(&["删除团队", "删除供应商", "删除套餐"]), "{titles:?}");
}

/// 在库里放一台带订阅的在用节点：到期日 = 今天 + `days`，价格 `price`（分）+ `cur`；`team` 是备用团队时就是备用机器
async fn due_node(t: &T, team: Uuid, number: i32, provider: Option<&str>, days: i32, price: i32, cur: &str, auto: bool) -> Uuid {
    let id = common::seed_node(t, team, number, &format!("203.0.113.{}", 100 + number), "active").await;
    sqlx::query("update machine set provider_id = $2::uuid where id = $1").bind(id).bind(provider).execute(&t.st.db).await.unwrap();
    sqlx::query("insert into subscription (machine_id, price_cents, currency, due_date, auto_renew) values ($1, $2, $3, current_date + $4::int, $5)")
        .bind(id)
        .bind(price)
        .bind(cur)
        .bind(days)
        .bind(auto)
        .execute(&t.st.db)
        .await
        .unwrap();
    id
}

/// 待付清单：按 供应商 × 到期日 分组，每组原币种小计与 USD 合计（用设置里的汇率）；窗口外、备用团队、自动续费的不进待付（备用要 include_spare，自动续费单列）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn due_groups_by_provider_and_date_with_usd_total() {
    let t = setup(false).await;
    let pa = t.post("/api/v1/providers", json!({"name": "甲", "code": "A", "panel_url": "https://a.example/panel"})).await["id"].as_str().unwrap().to_string();
    let pb = t.post("/api/v1/providers", json!({"name": "乙", "code": "B"})).await["id"].as_str().unwrap().to_string();
    let (st, v) = t.req("PUT", "/api/v1/settings/fx-rates/CNY", Some(json!({"usd_per_unit": 0.125})), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let team = seed_team(&t, "团队1").await;
    let spare: Uuid = sqlx::query_scalar("select id from team where spare").fetch_one(&t.st.db).await.unwrap();
    let a1 = due_node(&t, team, 1, Some(&pa), 5, 1000, "USD", false).await;
    let a2 = due_node(&t, team, 2, Some(&pa), 5, 8000, "CNY", false).await;
    let b1 = due_node(&t, team, 3, Some(&pb), 9, 500, "USD", false).await;
    due_node(&t, team, 4, Some(&pa), 40, 1000, "USD", false).await; // 窗口外
    let auto = due_node(&t, team, 5, Some(&pa), 5, 1000, "USD", true).await; // 自动续费
    let sp = due_node(&t, spare, 6, Some(&pa), 5, 1000, "USD", false).await; // 备用团队
    let d5: String = sqlx::query_scalar("select (current_date + 5)::text").fetch_one(&t.st.db).await.unwrap();

    let v = t.get("/api/v1/billing/due?days=30").await;
    let groups = v["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "{v}");
    let g = &groups[0];
    let ids = |g: &Value| g["machines"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!((g["provider"]["name"].as_str(), g["provider"]["panel_url"].as_str(), g["due_date"].as_str()), (Some("甲"), Some("https://a.example/panel"), Some(d5.as_str())), "{g}");
    assert_eq!(ids(g), [a1.to_string(), a2.to_string()]);
    // 原币种小计 + USD 合计：10.00 USD + 80.00 CNY × 0.125 = 20.00 USD
    assert_eq!(g["total"]["cents"], 2000, "{g}");
    assert_eq!(g["total"]["by_currency"].as_array().unwrap().iter().map(|p| (p["currency"].as_str().unwrap(), p["cents"].as_i64().unwrap(), p["usd_cents"].as_i64().unwrap())).collect::<Vec<_>>(), [("CNY", 8000, 1000), ("USD", 1000, 1000)]);
    assert_eq!((groups[1]["provider"]["name"].as_str(), ids(&groups[1])), (Some("乙"), vec![b1.to_string()]));
    assert_eq!(v["total"]["cents"], 2500);
    assert_eq!(v["auto"].as_array().unwrap().iter().map(|a| a["machine"]["id"].as_str().unwrap()).collect::<Vec<_>>(), [auto.to_string()]);
    // 带上备用团队 / 按日期截止
    let v = t.get("/api/v1/billing/due?days=30&include_spare=true").await;
    assert!(v["groups"][0]["machines"].as_array().unwrap().iter().any(|m| m["id"] == sp.to_string()), "{v}");
    let until: String = sqlx::query_scalar("select (current_date + 6)::text").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(t.get(&format!("/api/v1/billing/due?until={until}")).await["groups"].as_array().unwrap().len(), 1, "只有 5 天后到期的甲");
}

async fn due_of(t: &T, id: Uuid) -> String { sqlx::query_scalar("select due_date::text from subscription where machine_id = $1").bind(id).fetch_one(&t.st.db).await.unwrap() }

/// 批量续费：一个事务里按订阅周期顺延每台、金额默认订阅价，审计一条（列出机器）；dry_run 只预览什么都不写；有一台不行整批都不登记
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renew_batch_is_atomic_with_one_audit() {
    let t = setup(false).await;
    let pa = t.post("/api/v1/providers", json!({"name": "甲", "code": "A"})).await["id"].as_str().unwrap().to_string();
    let team = seed_team(&t, "团队1").await;
    let a1 = due_node(&t, team, 1, Some(&pa), 5, 1000, "USD", false).await;
    let a2 = due_node(&t, team, 2, Some(&pa), 5, 2500, "USD", false).await;
    let bare = common::seed_node(&t, team, 3, "203.0.113.150", "active").await; // 没有订阅
    let (d1, d2) = (due_of(&t, a1).await, due_of(&t, a2).await);
    let body = |dry: bool, ids: &[Uuid]| json!({"items": ids.iter().map(|i| json!({"machine_id": i})).collect::<Vec<_>>(), "ref": "#2041", "dry_run": dry});

    let (st, v) = t.req("POST", "/api/v1/renewals/batch", Some(body(true, &[a1, a2])), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!((v["data"]["dry_run"].clone(), v["data"]["total"]["cents"].clone()), (json!(true), json!(3500)), "{v}");
    assert_eq!((due_of(&t, a1).await, due_of(&t, a2).await), (d1.clone(), d2.clone()), "预览不写库");
    assert_eq!(t.count("select count(*) from renewal").await + t.count("select count(*) from audit_log where action like 'renewal%'").await, 0);

    // 带一台没有订阅的：整批 409，谁都没登记
    let (st, v) = t.req("POST", "/api/v1/renewals/batch", Some(body(false, &[a1, a2, bare])), None).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("团队1-03"), "报错带机器名：{v}");
    assert_eq!((due_of(&t, a1).await, t.count("select count(*) from renewal").await), (d1.clone(), 0));

    let (st, v) = t.req("POST", "/api/v1/renewals/batch", Some(body(false, &[a1, a2])), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (n1, n2) = (due_of(&t, a1).await, due_of(&t, a2).await);
    assert!(n1 > d1 && n2 > d2, "到期日顺延了一个周期：{d1} → {n1}");
    let rs: Vec<(i32, String)> = sqlx::query_as("select amount_cents, note from renewal order by amount_cents").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(rs, [(1000, "账单 #2041".to_string()), (2500, "账单 #2041".to_string())], "金额默认订阅价，账单号进备注");
    assert_eq!((t.count("select count(*) from audit_log where action = 'renewal.batch'").await, t.count("select count(*) from audit_log where action = 'renewal'").await), (1, 0), "只记一条审计");
    let audit = t.get("/api/v1/audit").await;
    assert_eq!(audit[0]["summary"].as_str(), Some("2 台：团队1-01、团队1-02（账单 #2041）"), "{audit}");
}
