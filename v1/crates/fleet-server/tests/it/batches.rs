//! 计费台账与客户到期页：初始化（预览、只建没有名额的团队）→ 改起算日 → 手动名额 → 批次 / 月度应收 → 客户页只出有效批次；
//! 对账（传入「现在」）：替换、超时新增、标不计费后超时减少、机器被物理删除；增减记录与审计。
//! 24 小时配对的各种边界在 ledger.rs 的单测里（纯函数），这里只验证读库写库这一层接得上。

use crate::common::{self, seed_node, T};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use fleet_server::batches::{fmt_date, today};
use fleet_server::state::Event;
use fleet_server::{ledger, meta};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use uuid::Uuid;

/// 不登录取客户页（从 192.0.2.9 来），返回 (状态码, 正文)
async fn public(t: &T, link: &str) -> (StatusCode, Value) {
    let path = link.strip_prefix("http://fleet.test/c/").map(|tok| format!("/api/v1/public/customer/{tok}")).unwrap_or_else(|| link.to_string());
    let req = Request::get(path).header("x-forwarded-for", "192.0.2.9").body(Body::empty()).unwrap();
    let res = t.app.clone().oneshot(req).await.unwrap();
    let s = res.status();
    (s, serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null))
}

async fn node(t: &T, team: Uuid, n: i32, country: &str) -> Uuid {
    let id = seed_node(t, team, n, &format!("203.0.113.{}", n + if country == "MX" { 100 } else { 0 }), "active").await;
    sqlx::query("update machine set country = $2 where id = $1").bind(id).bind(country).execute(&t.st.db).await.unwrap();
    id
}

/// 团队的名额：按节点名找
fn slot_of<'a>(v: &'a Value, name: &str) -> &'a Value { v["slots"].as_array().unwrap().iter().find(|s| s["name"] == name).unwrap_or_else(|| panic!("没有 {name}: {v}")) }

async fn texts(t: &T, team: Uuid) -> Vec<String> {
    let v = t.get(&format!("/api/v1/teams/{team}/billing-events?limit=500")).await;
    v.as_array().unwrap().iter().map(|e| e["text"].as_str().unwrap().to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_ledger_and_customer_page() {
    let t = common::boot(false, "trust_forwarded_for = true").await;
    let team1 = common::seed_team(&t, "团队1").await;
    let team2 = common::seed_team(&t, "团队2").await;
    let n1 = node(&t, team1, 1, "US").await;
    let n2 = node(&t, team1, 2, "US").await;
    let n3 = node(&t, team1, 3, "US").await;
    node(&t, team2, 1, "MX").await;
    let base1 = format!("/api/v1/teams/{team1}/batches");
    let now0 = OffsetDateTime::now_utc();

    // 没初始化过：对账什么都不做
    assert_eq!(ledger::reconcile(&t.st, now0).await.unwrap(), 0);
    assert_eq!(t.get(&base1).await["totals"]["pending"], 0);

    // 起算日 = 上个月 1 号；预览不写
    let s = today().replace_day(1).unwrap();
    let s = meta::add_months(s, -1).unwrap();
    let day = |n: i64| fmt_date(s + Duration::days(n));
    let p = t.post("/api/v1/billing/init", json!({"since": day(0), "dry_run": true})).await;
    assert_eq!((p["total"].as_i64(), p["teams"][0]["count"].as_i64()), (Some(4), Some(3)), "{p}");
    assert!(t.get(&base1).await["slots"].as_array().unwrap().is_empty());
    let p = t.post("/api/v1/billing/init", json!({"since": day(0)})).await;
    assert_eq!(p["total"], 4, "{p}");
    // 再跑一次：都已有名额，跳过
    let again = t.post("/api/v1/billing/init", json!({"since": day(0)})).await;
    assert_eq!((again["total"].as_i64(), again["teams"][0]["skipped"].as_bool()), (Some(0), Some(true)), "{again}");
    let v = t.get(&format!("/api/v1/teams/{team2}/batches")).await;
    assert_eq!(v["batches"][0]["region"], "墨西哥", "{v}");

    // 改团队1-03 的起算日；有节点占着的名额不能手动结束、不能删
    let s3 = slot_of(&t.get(&base1).await, "团队1-03")["id"].as_str().unwrap().to_string();
    let r = t.req("PATCH", &format!("/api/v1/billing-slots/{s3}"), Some(json!({"start_date": day(10)})), None).await;
    assert_eq!((r.0, r.1["data"]["start_date"].as_str()), (StatusCode::OK, Some(day(10).as_str())), "{r:?}");
    let (st, v) = t.req("PATCH", &format!("/api/v1/billing-slots/{s3}"), Some(json!({"end_date": day(20)})), None).await;
    assert_eq!((st, v["error"]["message"].as_str()), (StatusCode::CONFLICT, Some("节点还在团队里；不收这个节点的钱请把节点设为不计费")));
    assert_eq!(t.req("DELETE", &format!("/api/v1/billing-slots/{s3}"), None, None).await.0, StatusCode::CONFLICT);

    // 手动名额：一个有效的（墨西哥家宽），一个三个月前就结束了的（只管理端看得到）
    let m = t.post(&format!("/api/v1/teams/{team1}/billing-slots"), json!({"region": "墨西哥", "start_date": day(14), "note": "墨西哥家宽"})).await;
    assert_eq!((m[0]["manual"].as_bool(), m[0]["name"].as_str()), (Some(true), Some("手动 · 墨西哥家宽")), "{m}");
    let old_start = meta::add_months(s, -3).unwrap();
    let old = t.post(&format!("/api/v1/teams/{team1}/billing-slots"), json!({"region": "菲律宾", "start_date": fmt_date(old_start)})).await;
    let old_id = old[0]["id"].as_str().unwrap();
    let r = t.req("PATCH", &format!("/api/v1/billing-slots/{old_id}"), Some(json!({"end_date": fmt_date(old_start + Duration::days(10))})), None).await;
    assert_eq!((r.0, r.1["data"]["state"].as_str()), (StatusCode::OK, Some("ended")), "{r:?}");

    // 批次：按 (起算日, 地区) 编号，第01批（菲律宾）已到期
    let v = t.get(&base1).await;
    let got: Vec<(i64, String, i64, String)> =
        v["batches"].as_array().unwrap().iter().map(|b| (b["seq"].as_i64().unwrap(), b["region"].as_str().unwrap().into(), b["count"].as_i64().unwrap(), b["state"].as_str().unwrap().into())).collect();
    assert_eq!(got, [(1, "菲律宾".into(), 1, "expired".into()), (2, "美区".into(), 2, "active".into()), (3, "美区".into(), 1, "active".into()), (4, "墨西哥".into(), 1, "active".into())], "{v}");
    assert_eq!(v["batches"][1]["key"], format!("{}|美区", day(0)));
    assert_eq!((v["totals"]["active"].as_i64(), slot_of(&v, "团队1-03")["batch_seq"].as_i64()), (Some(4), Some(3)), "{v}");

    // 月度应收（上个月）：每批起点落在这个月的期
    let mon = &day(0)[..7];
    let r = t.get(&format!("/api/v1/teams/{team1}/receivable?month={mon}")).await;
    let lines: Vec<(i64, String, i64)> = r["lines"].as_array().unwrap().iter().map(|l| (l["batch_seq"].as_i64().unwrap(), l["period_start"].as_str().unwrap().into(), l["count"].as_i64().unwrap())).collect();
    assert_eq!(lines, [(2, day(0), 2), (3, day(10), 1), (4, day(14), 1)], "{r}");
    assert_eq!((r["total"].as_i64(), r["start_count"].as_i64(), r["end_count"].as_i64(), r["projected"].as_bool()), (Some(4), Some(2), Some(4), Some(false)), "{r}");
    // 下下个月：整月预估，没有月初 / 月末个数
    let fut = fmt_date(meta::add_months(s, 3).unwrap());
    let r = t.get(&format!("/api/v1/teams/{team1}/receivable?month={}", &fut[..7])).await;
    assert_eq!((r["total"].as_i64(), r["projected"].as_bool(), r["start_count"].is_null()), (Some(4), Some(true), true), "{r}");

    // 客户页：只出有效的批次，不出节点名
    let page = t.post(&format!("/api/v1/teams/{team1}/customer-page"), json!({})).await;
    let (st, pv) = public(&t, page["link"].as_str().unwrap()).await;
    assert_eq!(st, StatusCode::OK, "{pv}");
    let shown: Vec<i64> = pv["data"]["batches"].as_array().unwrap().iter().map(|b| b["seq"].as_i64().unwrap()).collect();
    assert_eq!(shown, [2, 3, 4], "{pv}");
    assert!(!pv.to_string().contains("团队1-0"), "{pv}");

    // ── 对账（传入的「现在」）──
    // 团队1-01 退役、同一轮来了团队1-04：替换，起算日不变
    sqlx::query("update machine set lifecycle = 'retired', retired_at = now() where id = $1").bind(n1).execute(&t.st.db).await.unwrap();
    let n4 = node(&t, team1, 4, "US").await;
    ledger::reconcile(&t.st, now0).await.unwrap();
    let v = t.get(&base1).await;
    let s4 = slot_of(&v, "团队1-04");
    assert_eq!((s4["start_date"].as_str(), s4["holder"]["id"].as_str()), (Some(day(0).as_str()), Some(n4.to_string().as_str())), "{v}");
    // 再加一个：待定，25 小时后成新名额（起算日 = 第一次看到它那天）
    node(&t, team1, 5, "US").await;
    ledger::reconcile(&t.st, now0).await.unwrap();
    let v = t.get(&base1).await;
    assert_eq!((v["pending"][0]["machine"]["name"].as_str(), v["totals"]["pending"].as_i64()), (Some("团队1-05"), Some(1)), "{v}");
    ledger::reconcile(&t.st, now0 + Duration::hours(25)).await.unwrap();
    let v = t.get(&base1).await;
    let seen_day = fmt_date(now0.to_offset(meta::org_offset()).date());
    assert_eq!((slot_of(&v, "团队1-05")["start_date"].as_str(), v["totals"]["pending"].as_i64()), (Some(seen_day.as_str()), Some(0)), "{v}");
    // 团队1-02 设为不计费：机器视图带上；设完马上对账一次，名额立刻空出待定，满 24 小时没人接 → 结束
    let mut ev = t.st.events.subscribe();
    let r = t.req("PUT", &format!("/api/v1/machines/{n2}/billing-exempt"), Some(json!({"exempt": true})), None).await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    assert_eq!(t.get(&format!("/api/v1/machines/{n2}")).await["billing_exempt"], true);
    assert!(std::iter::from_fn(|| ev.try_recv().ok()).any(|e| matches!(e, Event::Billing { id } if id == team1)), "设不计费要发 billing 事件");
    let v = t.get(&base1).await;
    let s2 = slot_of(&v, "团队1-02");
    assert_eq!((s2["state"].as_str(), s2["last_holder_name"].as_str(), v["totals"]["vacated"].as_i64()), (Some("vacated"), Some("团队1-02"), Some(1)), "{v}");
    let t26 = now0 + Duration::hours(26);
    ledger::reconcile(&t.st, t26).await.unwrap();
    let s2 = slot_of(&t.get(&base1).await, "团队1-02").clone();
    // （空出就在刚才，结束日 = 空出那天）
    assert_eq!(s2["end_date"].as_str(), Some(seen_day.as_str()), "{s2}");
    // 机器被物理删除：外键置空，对账照常把名额空出（不报错）
    sqlx::query("delete from machine where id = $1").bind(n3).execute(&t.st.db).await.unwrap();
    ledger::reconcile(&t.st, t26 + Duration::hours(52)).await.unwrap();
    assert_eq!(slot_of(&t.get(&base1).await, "团队1-03")["state"], "vacated");

    // 增减记录：一句话、新的在前、可翻页
    let all = texts(&t, team1).await;
    for want in ["团队1-01 → 团队1-04 替换，计费不变", &format!("团队1-05 加入，新增计费节点（{} 起算）", &seen_day[5..]), "团队1-02 设为不计费", "团队1-01 计入计费（初始化，"] {
        assert!(all.iter().any(|x| x.starts_with(want)), "没有「{want}」: {all:?}");
    }
    assert!(all.iter().any(|x| x.starts_with("团队1-02 离开，计费节点减少（算到 ")), "{all:?}");
    let first = t.get(&format!("/api/v1/teams/{team1}/billing-events?limit=2")).await;
    let next = t.get(&format!("/api/v1/teams/{team1}/billing-events?limit=2&before={}", first[1]["id"].as_str().unwrap())).await;
    assert_eq!(next[0]["text"].as_str(), Some(all[2].as_str()));

    let acts: Vec<String> = sqlx::query_scalar("select action from audit_log where action like 'billing.%' order by id").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(acts, ["billing.init", "billing.init", "billing.slot_update", "billing.slot_add", "billing.slot_add", "billing.slot_update", "billing.exempt"]);
}

/// 整批改起算日；空出的系统名额填结束日
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_start_and_vacated_end() {
    let t = common::boot(false, "").await;
    let team = common::seed_team(&t, "团队1").await;
    let n1 = node(&t, team, 1, "US").await;
    node(&t, team, 2, "US").await;
    let s = meta::add_months(today().replace_day(1).unwrap(), -1).unwrap();
    let day = |n: i64| fmt_date(s + Duration::days(n));
    t.post("/api/v1/billing/init", json!({"since": day(0)})).await;
    // 一个手动名额在 day(20) 结束，同一批（同起算日、同地区）
    let m = t.post(&format!("/api/v1/teams/{team}/billing-slots"), json!({"region": "美区", "start_date": day(0)})).await;
    let m_id = m[0]["id"].as_str().unwrap().to_string();
    let r = t.req("PATCH", &format!("/api/v1/billing-slots/{m_id}"), Some(json!({"end_date": day(20)})), None).await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    let path = format!("/api/v1/teams/{team}/batches/{}%7C%E7%BE%8E%E5%8C%BA", day(0));
    // 新起算日不早于已结束名额的结束日：拒绝，什么都不改
    let (st, v) = t.req("PATCH", &path, Some(json!({"start_date": day(20)})), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("要早于已结束名额的结束日"), "{v}");
    assert_eq!(t.get(&format!("/api/v1/teams/{team}/batches")).await["batches"][0]["start_date"], day(0));
    // 不存在的批次
    let (st, _) = t.req("PATCH", &format!("/api/v1/teams/{team}/batches/{}%7C%E5%A2%A8", day(0)), Some(json!({"start_date": day(5)})), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // 早于结束日：整批一起改
    let mut ev = t.st.events.subscribe();
    let (st, v) = t.req("PATCH", &path, Some(json!({"start_date": day(5)})), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!((v["data"]["key"].as_str(), v["data"]["start_date"].as_str()), (Some(format!("{}|美区", day(5)).as_str()), Some(day(5).as_str())), "{v}");
    assert!(std::iter::from_fn(|| ev.try_recv().ok()).any(|e| matches!(e, Event::Billing { id } if id == team)));
    let d = t.get(&format!("/api/v1/teams/{team}/batches")).await;
    assert!(d["slots"].as_array().unwrap().iter().all(|x| x["start_date"] == day(5)), "{d}");
    assert!(texts(&t, team).await.iter().any(|x| x.contains("起算日") && x.contains("（整批 3 个）")), "{:?}", texts(&t, team).await);
    let acts: Vec<String> = sqlx::query_scalar("select action from audit_log where action = 'billing.batch_update'").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(acts.len(), 1);

    // 节点设不计费：马上空出待定；给空出的系统名额填以后的结束日被拒，填今天结束
    t.req("PUT", &format!("/api/v1/machines/{n1}/billing-exempt"), Some(json!({"exempt": true})), None).await;
    let d = t.get(&format!("/api/v1/teams/{team}/batches")).await;
    let sid = slot_of(&d, "团队1-01")["id"].as_str().unwrap().to_string();
    assert_eq!((slot_of(&d, "团队1-01")["state"].as_str(), d["totals"]["vacated"].as_i64(), d["totals"]["active"].as_i64()), (Some("vacated"), Some(1), Some(1)), "{d}");
    let tomorrow = fmt_date(today() + Duration::days(1));
    let (st, v) = t.req("PATCH", &format!("/api/v1/billing-slots/{sid}"), Some(json!({"end_date": tomorrow})), None).await;
    assert_eq!((st, v["error"]["message"].as_str()), (StatusCode::BAD_REQUEST, Some("空出的计费节点结束日不能晚于今天")), "{v}");
    let (st, v) = t.req("PATCH", &format!("/api/v1/billing-slots/{sid}"), Some(json!({"end_date": fmt_date(today())})), None).await;
    assert_eq!((st, v["data"]["state"].as_str()), (StatusCode::OK, Some("ended")), "{v}");
}
