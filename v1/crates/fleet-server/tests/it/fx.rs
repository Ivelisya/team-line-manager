//! 汇率与 USD 折算（fx.rs）：从本地假汇率源拉取、手动覆盖优先、取消覆盖、拉取失败保留上次的值；
//! 机器视图、总览、团队、套餐里的 USD 字段。不访问公网。需要 PostgreSQL（见 common）。

use crate::common;

use axum::http::StatusCode;
use common::{boot, seed_node, seed_team, T};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// 假汇率源：回 frankfurter 的形状，CNY 取共享的值；None 时回 500
async fn fake_source(cny: Arc<Mutex<Option<f64>>>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/latest",
        axum::routing::get(move || {
            let cny = cny.clone();
            async move {
                match *cny.lock().unwrap() {
                    Some(v) => (StatusCode::OK, axum::Json(json!({"amount": 1.0, "base": "USD", "date": "2026-10-22", "rates": {"CNY": v, "EUR": 0.8}}))),
                    None => (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"message": "boom"}))),
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}/latest?base=USD")
}

async fn sub(t: &T, id: &str) -> Value { t.get(&format!("/api/v1/machines/{id}")).await["subscription"].clone() }

fn cny_row(rates: &Value) -> Value { rates["rates"].as_array().unwrap().iter().find(|r| r["currency"] == "CNY").cloned().unwrap_or(Value::Null) }

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usd_conversion_auto_manual_and_failure() {
    let cny = Arc::new(Mutex::new(Some(8.0)));
    let url = fake_source(cny.clone()).await;
    let t = boot(false, &format!("fx.url = \"{url}\"")).await;
    let team = seed_team(&t, "团队1").await;
    let node = seed_node(&t, team, 1, "203.0.113.20", "active").await.to_string();
    let usd_node = seed_node(&t, team, 2, "203.0.113.21", "active").await.to_string();
    for (id, body) in [
        (&node, json!({"due_date": "2027-01-31", "price_cents": 29999, "currency": "CNY"})),
        (&usd_node, json!({"due_date": "2027-01-31", "price_cents": 30000, "currency": "USD", "cycle": "quarterly"})),
    ] {
        let (st, v) = t.req("PUT", &format!("/api/v1/machines/{id}/subscription"), Some(body), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
    }
    let pv = t.post("/api/v1/providers", json!({"name": "甲", "code": "A"})).await["id"].clone();
    t.post("/api/v1/plans", json!({"provider_id": pv, "name": "线路", "price_cents": 89997, "currency": "CNY", "cycle": "quarterly"})).await;

    // 还没有汇率：CNY 未折算（不当 1:1），USD 照常；总览单列未折算的
    let s = sub(&t, &node).await;
    assert_eq!((s["monthly_usd_cents"].clone(), s["usd_per_unit"].clone(), s["monthly_cents"].as_i64()), (Value::Null, Value::Null, Some(29999)), "{s}");
    let s = sub(&t, &usd_node).await;
    assert_eq!((s["monthly_usd_cents"].as_i64(), s["price_usd_cents"].as_i64(), s["usd_per_unit"].as_f64()), (Some(10000), Some(30000), Some(1.0)));
    let ov = t.get("/api/v1/overview").await;
    assert_eq!(ov["monthly_usd"]["cents"], 10000);
    assert_eq!(ov["monthly_usd"]["unconverted"], json!([{"currency": "CNY", "cents": 29999}]));
    assert_eq!(ov["monthly_cost"], json!([{"currency": "CNY", "cents": 29999}, {"currency": "USD", "cents": 10000}]), "旧字段保留");
    let r = t.get("/api/v1/settings/fx-rates").await;
    assert_eq!((r["missing"].clone(), r["fetched_at"].clone(), r["auto"].clone()), (json!(["CNY"]), Value::Null, json!(false)), "{r}");

    // 立即更新：1 USD = 8 CNY → ¥299.99 = $37.49875 → 3750（四舍五入到分）
    let r = t.post("/api/v1/settings/fx-rates/refresh", json!({})).await;
    assert_eq!((r["rates_date"].as_str(), r["source"].as_str(), r["last_error"].clone(), r["missing"].clone()), (Some("2026-10-22"), Some(url.as_str()), Value::Null, json!([])), "{r}");
    let c = cny_row(&r);
    assert_eq!((c["usd_per_unit"].as_f64(), c["per_usd"].as_f64(), c["source"].as_str(), c["in_use"].clone()), (Some(0.125), Some(8.0), Some("auto"), json!(true)), "{c}");
    let s = sub(&t, &node).await;
    assert_eq!((s["monthly_usd_cents"].as_i64(), s["price_usd_cents"].as_i64(), s["usd_per_unit"].as_f64()), (Some(3750), Some(3750), Some(0.125)));
    let ov = t.get("/api/v1/overview").await;
    assert_eq!(ov["monthly_usd"], json!({"cents": 13750, "unconverted": [],
        "by_currency": [{"currency": "CNY", "cents": 29999, "usd_cents": 3750, "usd_per_unit": 0.125}, {"currency": "USD", "cents": 10000, "usd_cents": 10000, "usd_per_unit": 1.0}]}));
    let teams = t.get("/api/v1/teams").await;
    let t1 = teams.as_array().unwrap().iter().find(|x| x["name"] == "团队1").unwrap();
    assert_eq!(t1["monthly_usd"]["cents"], 13750, "{t1}");
    let plans = t.get("/api/v1/plans").await;
    // 季付 ¥899.97 → $112.49625 → 11250；月均 ¥299.99 → 3750
    assert_eq!((plans[0]["price_usd_cents"].as_i64(), plans[0]["monthly_usd_cents"].as_i64()), (Some(11250), Some(3750)), "{plans}");

    // 手动覆盖（按 1 USD = 10 CNY 给）：优先于自动，写审计
    let (st, v) = t.req("PUT", "/api/v1/settings/fx-rates/cny", Some(json!({"per_usd": 10})), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let c = cny_row(&v["data"]);
    assert_eq!((c["usd_per_unit"].as_f64(), c["source"].as_str(), c["auto_per_usd"].as_f64(), c["manual"]["by"].as_str()), (Some(0.1), Some("manual"), Some(8.0), Some("admin")), "{c}");
    assert_eq!(sub(&t, &node).await["monthly_usd_cents"], 3000);
    let audit = t.get("/api/v1/audit?limit=20").await;
    let a = audit["items"].as_array().or(audit.as_array()).unwrap().iter().find(|a| a["action"] == "settings.fx_rate").cloned().unwrap_or_else(|| panic!("没有审计：{audit}"));
    assert_eq!(a["detail"]["from"], json!({"usd_per_unit": 0.125, "source": "auto"}), "{a}");

    // 自动更新不改手动覆盖的；自动的值照样更新
    *cny.lock().unwrap() = Some(7.0);
    let r = t.post("/api/v1/settings/fx-rates/refresh", json!({})).await;
    assert_eq!((cny_row(&r)["usd_per_unit"].as_f64(), cny_row(&r)["auto_per_usd"].as_f64()), (Some(0.1), Some(7.0)));
    assert_eq!(sub(&t, &node).await["monthly_usd_cents"], 3000);

    // 取消覆盖：回到自动的 1/7 → 4286
    let (st, v) = t.req("DELETE", "/api/v1/settings/fx-rates/CNY", None, None).await;
    assert_eq!((st, cny_row(&v["data"])["source"].as_str()), (StatusCode::OK, Some("auto")), "{v}");
    assert_eq!(sub(&t, &node).await["monthly_usd_cents"], 4286);

    // 汇率源失败：502，保留上次的值，记下原因
    *cny.lock().unwrap() = None;
    let (st, v) = t.req("POST", "/api/v1/settings/fx-rates/refresh", Some(json!({})), None).await;
    assert_eq!(st, StatusCode::BAD_GATEWAY, "{v}");
    let r = t.get("/api/v1/settings/fx-rates").await;
    assert!(r["last_error"].as_str().is_some_and(|e| e.contains("500")), "{r}");
    assert_eq!(cny_row(&r)["auto_per_usd"].as_f64(), Some(7.0));
    assert_eq!(sub(&t, &node).await["monthly_usd_cents"], 4286);

    // 不合规的输入
    for (path, body) in [
        ("/api/v1/settings/fx-rates/USD", json!({"usd_per_unit": 1.1})),
        ("/api/v1/settings/fx-rates/CNY", json!({"usd_per_unit": 0.1, "per_usd": 10})),
        ("/api/v1/settings/fx-rates/CNY", json!({})),
        ("/api/v1/settings/fx-rates/CNY", json!({"usd_per_unit": -1})),
        ("/api/v1/settings/fx-rates/C1", json!({"usd_per_unit": 0.1})),
    ] {
        let (st, v) = t.req("PUT", path, Some(body.clone()), None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{path} {body}: {v}");
    }
    // 手动给一个源里没有的币种：也能折算
    let (st, _) = t.req("PUT", "/api/v1/settings/fx-rates/XYZ", Some(json!({"usd_per_unit": 2})), None).await;
    assert_eq!(st, StatusCode::OK);
    let r = t.get("/api/v1/settings/fx-rates").await;
    let x = r["rates"].as_array().unwrap().iter().find(|r| r["currency"] == "XYZ").unwrap().clone();
    assert_eq!((x["usd_per_unit"].as_f64(), x["in_use"].clone()), (Some(2.0), json!(false)));
}
