//! 客户批次账与客户到期页：建批次 → 续期改个数 → 汇总字段 → 生成客户页链接 → 公开接口只给有效批次，停用 / 换链接后 404

use crate::common::{self, tele, T};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use fleet_server::batches::{fmt_date, today};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use time::Duration;
use tower::ServiceExt;

/// 不登录取客户页（从 192.0.2.9 来），返回 (状态码, 响应头, 正文)
async fn public(t: &T, link: &str) -> (StatusCode, axum::http::HeaderMap, Value) {
    let path = link.strip_prefix("http://fleet.test/c/").map(|tok| format!("/api/v1/public/customer/{tok}")).unwrap_or_else(|| link.to_string());
    let req = Request::get(path).header("x-forwarded-for", "192.0.2.9").body(Body::empty()).unwrap();
    let res = t.app.clone().oneshot(req).await.unwrap();
    let (s, h) = (res.status(), res.headers().clone());
    let v = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null);
    (s, h, v)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_ledger_and_customer_page() {
    let t = common::boot(false, "trust_forwarded_for = true").await;
    let team = tele::seed_team(&t, "团队2").await;
    let day = |n: i64| fmt_date(today() + Duration::days(n));
    let base = format!("/api/v1/teams/{team}/batches");

    // 建批次：end_date 默认 = 开始日加 1 个日历月，编号默认 1
    let b1 = t.post(&base, json!({"region": "美区", "start_date": day(-5), "months": 1, "count": 5, "note": "另1个归团队1"})).await;
    assert_eq!((b1["seq"].as_i64(), b1["count"].as_i64(), b1["months"].as_i64(), b1["node_months"].as_i64(), b1["state"].as_str()), (Some(1), Some(5), Some(1), Some(5), Some("active")), "{b1}");
    assert_eq!((b1["summary"].as_str(), b1["title"].as_str()), (Some(""), Some("第01批 · 美区 · 当前有效5个")), "{b1}");
    let id1 = b1["id"].as_str().unwrap().to_string();

    // 续期改个数：起点 = 原到期日，5 → 4
    let r = t.post(&format!("/api/v1/batches/{id1}/renew"), json!({"months": 1, "count": 4})).await;
    assert_eq!((r["count"].as_i64(), r["months"].as_i64(), r["node_months"].as_i64(), r["renewals"].as_i64()), (Some(4), Some(2), Some(9), Some(1)), "{r}");
    assert_eq!((r["summary"].as_str(), r["title"].as_str()), (Some("初始5个 → 续期4个"), Some("第01批 · 美区 · 当前有效4个")), "{r}");
    assert_eq!(r["periods"][1]["start_date"], b1["end_date"], "续期段起点是原到期日: {r}");
    assert_eq!(r["start_date"], b1["start_date"]);

    // 第 2 批：已到期的（只管理端看得到）。编号自动 +1；不给月数以外的终点时用 end_date 指定
    let b2 = t.post(&base, json!({"region": "墨西哥", "start_date": day(-90), "end_date": day(-30), "months": 2, "count": 3})).await;
    assert_eq!((b2["seq"].as_i64(), b2["state"].as_str(), b2["title"].as_str()), (Some(2), Some("expired"), Some("第02批 · 墨西哥 · 原3个（已到期）")), "{b2}");
    // 第 3 批：今天到期仍算有效
    let b3 = t.post(&base, json!({"region": "菲律宾", "start_date": day(-28), "end_date": day(0), "months": 1, "count": 1, "seq": 9})).await;
    assert_eq!((b3["seq"].as_i64(), b3["state"].as_str()), (Some(9), Some("today")), "{b3}");

    // 校验：编号冲突 409、只剩一段不能删、段不能重叠
    let (s, v) = t.req("POST", &base, Some(json!({"region": "美区", "start_date": day(-1), "months": 1, "count": 1, "seq": 1})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, v) = t.req("DELETE", &format!("/api/v1/batch-periods/{}", b2["periods"][0]["id"].as_str().unwrap()), None, None).await;
    assert_eq!((s, v["error"]["message"].as_str()), (StatusCode::CONFLICT, Some("只剩这一段，要删就删整个批次")), "{v}");
    let second = r["periods"][1]["id"].as_str().unwrap();
    let (s, v) = t.req("PATCH", &format!("/api/v1/batch-periods/{second}"), Some(json!({"start_date": day(-4)})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, v) = t.req("POST", &base, Some(json!({"region": "美区", "start_date": day(0), "end_date": day(-1), "months": 1, "count": 1})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "到期日早于开始日: {v}");

    // 管理端列表：含已到期的，按编号升序；还没有客户页链接
    let l = t.get(&base).await;
    assert_eq!((l["team"]["name"].as_str(), l["today"].as_str(), l["page"].is_null()), (Some("团队2"), Some(fmt_date(today()).as_str()), true), "{l}");
    let seqs: Vec<i64> = l["batches"].as_array().unwrap().iter().map(|b| b["seq"].as_i64().unwrap()).collect();
    assert_eq!(seqs, [1, 2, 9]);

    // 客户页链接：生成后公开接口只给没到期的批次（第 1、9 批），不带 id / 备注原文
    let page = t.post(&format!("/api/v1/teams/{team}/customer-page"), json!({})).await;
    let link = page["link"].as_str().unwrap().to_string();
    assert!(link.starts_with("http://fleet.test/c/") && page["enabled"] == true, "{page}");
    assert_eq!(t.get(&base).await["page"]["link"], link.as_str(), "管理端随时能再复制同一条链接");
    let (s, h, v) = public(&t, &link).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((h["cache-control"].to_str().unwrap(), h["x-robots-tag"].to_str().unwrap()), ("no-store", "noindex"));
    let d = &v["data"];
    assert_eq!((d["team"].as_str(), d["node_months"].as_i64()), (Some("团队2"), Some(9 + 1)), "{v}");
    let shown: Vec<i64> = d["batches"].as_array().unwrap().iter().map(|b| b["seq"].as_i64().unwrap()).collect();
    assert_eq!(shown, [1, 9], "已到期的第2批不出现: {v}");
    assert_eq!(d["batches"][0]["desc"], "初始5个 → 续期4个；另1个归团队1");
    assert!(d["batches"][0].get("id").is_none());
    assert_eq!(d["regions"], json!([{"region": "美区", "counts": [4], "total": 4, "today_count": 0}, {"region": "菲律宾", "counts": [1], "total": 1, "today_count": 1}]), "{v}");
    // 最近查看：记了时间与来源
    let seen = t.get(&base).await["page"].clone();
    assert!(seen["last_view_at"].is_string() && seen["last_view_ip"] == "192.0.2.9", "{seen}");

    // 令牌不对 404；停用后 404、启用后恢复；换新链接旧的立即 404 且最近查看清空
    assert_eq!(public(&t, "/api/v1/public/customer/not-a-token").await.0, StatusCode::NOT_FOUND);
    let off = t.req("PATCH", &format!("/api/v1/teams/{team}/customer-page"), Some(json!({"enabled": false})), None).await;
    assert_eq!((off.0, off.1["data"]["enabled"].as_bool()), (StatusCode::OK, Some(false)));
    assert_eq!(public(&t, &link).await.0, StatusCode::NOT_FOUND);
    t.req("PATCH", &format!("/api/v1/teams/{team}/customer-page"), Some(json!({"enabled": true})), None).await;
    assert_eq!(public(&t, &link).await.0, StatusCode::OK);
    let new = t.post(&format!("/api/v1/teams/{team}/customer-page"), json!({})).await;
    let new_link = new["link"].as_str().unwrap();
    assert_ne!(new_link, link);
    assert!(new["last_view_at"].is_null(), "{new}");
    assert_eq!(public(&t, &link).await.0, StatusCode::NOT_FOUND);
    assert_eq!(public(&t, new_link).await.0, StatusCode::OK);

    // 删批次、删链接
    let (s, _) = t.req("DELETE", &format!("/api/v1/batches/{id1}"), None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(public(&t, new_link).await.1.get("x-robots-tag").map(|v| v.to_str().unwrap()), Some("noindex"));
    let (s, _) = t.req("DELETE", &format!("/api/v1/teams/{team}/customer-page"), None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(public(&t, new_link).await.0, StatusCode::NOT_FOUND);

    // 写操作记审计，公开拉取不记
    let acts: Vec<String> = sqlx::query_scalar("select action from audit_log where action like 'customer_%' order by id").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(
        acts,
        ["customer_batch.create", "customer_batch.renew", "customer_batch.create", "customer_batch.create", "customer_page.create", "customer_page.disable", "customer_page.enable", "customer_page.rotate", "customer_batch.delete", "customer_page.delete"]
    );
}
