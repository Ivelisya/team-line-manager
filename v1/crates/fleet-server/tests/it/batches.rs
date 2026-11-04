//! 计费台账与客户页：初始化（预览、只建没有名额的团队）→ 改开始收费日 → 手动名额 → 按月看每个节点起点在这个月的那一期（标记已支付 / 改回待支付、
//! 提前标、没有这一期的报 400）→ 客户页（只有名字、地区、这一期的区间、已付 / 待付）；对账（传入「现在」）：加入即建名额（开始收费日 = 购买日）、
//! 离开即结束、不猜补位、回来的节点接回、机器被物理删除；替换节点接名额（take_over）；增减记录与审计；迁移 0104。规则的各种边界在 ledger.rs 的单测里（纯函数）。

use crate::common::{self, seed_node, T};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use fleet_server::batches::{fmt_cn, fmt_date, fmt_month, today};
use fleet_server::state::Event;
use fleet_server::{ledger, meta};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use time::{Date, Duration, OffsetDateTime};
use tower::ServiceExt;
use uuid::Uuid;

/// 不登录取客户页（从 192.0.2.9 来），返回 (状态码, 正文)
async fn public(t: &T, link: &str, password: &str) -> (StatusCode, Value) {
    let path = link.strip_prefix("http://fleet.test/c/").map(|tok| format!("/api/v1/public/customer/{tok}")).unwrap_or_else(|| link.to_string());
    let req = Request::post(path).header("content-type", "application/json").header("x-fleet-request", "1").header("x-forwarded-for", "192.0.2.9").body(Body::from(json!({"password": password}).to_string())).unwrap();
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

/// 某月清单里的 (名字, 已支付)，按接口给的顺序
fn listed(v: &Value) -> Vec<(String, bool)> { v["nodes"].as_array().unwrap().iter().map(|n| (n["name"].as_str().unwrap().to_string(), n["paid"] == true)).collect() }

fn named(names: &[(&str, bool)]) -> Vec<(String, bool)> { names.iter().map(|(n, p)| (n.to_string(), *p)).collect() }

#[tokio::test]
async fn month_order_uses_effective_date_and_survives_payment_changes() {
    let t = common::boot(false, "trust_forwarded_for = true").await;
    let team = common::seed_team(&t, "排序测试").await;
    for number in 1..=3 { node(&t, team, number, "US").await; }
    let month = today().replace_day(1).unwrap();
    let start = meta::add_months(month, -3).unwrap().replace_day(5).unwrap();
    t.post("/api/v1/billing/init", json!({"since": fmt_date(start)})).await;
    let slots = t.get(&format!("/api/v1/teams/{team}/batches")).await;
    let mut ids = Vec::new();
    // Initial activation dates and node numbers deliberately disagree with this month's dates.
    // Nodes 02 and 03 both take effect on the fifth, even though 03 originally started earlier.
    for (number, months_ago, day) in [(1, 2, 20), (2, 1, 5), (3, 3, 5)] {
        let name = format!("排序测试-{number:02}");
        let id = Uuid::parse_str(slot_of(&slots, &name)["id"].as_str().unwrap()).unwrap();
        let date = meta::add_months(month, -months_ago).unwrap().replace_day(day).unwrap();
        sqlx::query("update billing_slot set start_date=$2 where id=$1").bind(id).bind(date).execute(&t.st.db).await.unwrap();
        ids.push(id);
    }
    let month_text = fmt_month(month);
    let path = format!("/api/v1/teams/{team}/billing/month?month={month_text}");
    let before = t.get(&path).await;
    assert_eq!(listed(&before), named(&[("排序测试-02", false), ("排序测试-03", false), ("排序测试-01", false)]));
    let after = t.post(&format!("/api/v1/teams/{team}/billing/paid"), json!({"month": month_text, "slot_ids": [ids[1]], "paid": true})).await;
    let expected = named(&[("排序测试-02", true), ("排序测试-03", false), ("排序测试-01", false)]);
    assert_eq!(listed(&after), expected);
    let page = t.post(&format!("/api/v1/teams/{team}/customer-page"), json!({})).await;
    let link = format!("{}?month={month_text}", page["link"].as_str().unwrap());
    let (status, customer) = public(&t, &link, page["password"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed(&customer["data"]), expected, "客户页与管理端的顺序必须一致");
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
    assert!(t.get(&base1).await["slots"].as_array().unwrap().is_empty());

    // 开始收费日 = 上个月 1 号；预览不写
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
    assert_eq!(v["slots"][0]["region"], "墨西哥", "{v}");

    // 团队1-03 月中开始收费（day 10）；有节点占着的名额不能手动停止、不能删
    let s3 = slot_of(&t.get(&base1).await, "团队1-03")["id"].as_str().unwrap().to_string();
    let r = t.req("PATCH", &format!("/api/v1/billing-slots/{s3}"), Some(json!({"start_date": day(10)})), None).await;
    assert_eq!((r.0, r.1["data"]["start_date"].as_str()), (StatusCode::OK, Some(day(10).as_str())), "{r:?}");
    let (st, v) = t.req("PATCH", &format!("/api/v1/billing-slots/{s3}"), Some(json!({"end_date": day(20)})), None).await;
    assert_eq!((st, v["error"]["message"].as_str()), (StatusCode::CONFLICT, Some("节点还在团队里；不收这个节点的钱请把节点设为不计费")));
    assert_eq!(t.req("DELETE", &format!("/api/v1/billing-slots/{s3}"), None, None).await.0, StatusCode::CONFLICT);

    // 手动名额：一个在收费的（墨西哥家宽），一个三个月前月中开始、10 天后就停了的（没写备注）
    let m = t.post(&format!("/api/v1/teams/{team1}/billing-slots"), json!({"region": "墨西哥", "start_date": day(14), "note": "墨西哥家宽"})).await;
    assert_eq!((m[0]["manual"].as_bool(), m[0]["name"].as_str()), (Some(true), Some("墨西哥家宽")), "{m}");
    let old_start = meta::add_months(s, -3).unwrap() + Duration::days(5);
    let old = t.post(&format!("/api/v1/teams/{team1}/billing-slots"), json!({"region": "菲律宾", "start_date": fmt_date(old_start)})).await;
    let old_id = old[0]["id"].as_str().unwrap().to_string();
    let r = t.req("PATCH", &format!("/api/v1/billing-slots/{old_id}"), Some(json!({"end_date": fmt_date(old_start + Duration::days(10))})), None).await;
    assert_eq!((r.0, r.1["data"]["state"].as_str()), (StatusCode::OK, Some("ended")), "{r:?}");

    // ── 按月 ──
    let month = |m: i64| fmt_month(meta::add_months(s, m).unwrap());
    let get_month = |m: String| {
        let t = &t;
        async move { t.get(&format!("/api/v1/teams/{team1}/billing/month?month={m}")).await }
    };
    // 上个月：月初的两个、月中开始的团队1-03、月中开始的家宽，各有一期从这个月开始（加入那个月就有一期）；默认都待支付，按本期生效日排；三个月前停掉的不在
    let v = get_month(month(0)).await;
    assert_eq!(listed(&v), named(&[("团队1-01", false), ("团队1-02", false), ("团队1-03", false), ("墨西哥家宽", false)]), "{v}");
    assert_eq!(v["month"].as_str(), Some(month(0).as_str()), "{v}");
    // 每行是这个节点自己的一期：团队1-03 从 day 10 起满一个月
    let n03 = v["nodes"].as_array().unwrap().iter().find(|n| n["name"] == "团队1-03").unwrap();
    let pe03 = fmt_date(meta::add_months(s + Duration::days(10), 1).unwrap());
    assert_eq!((n03["period_start"].as_str(), n03["period_end"].as_str(), n03["upcoming"].as_bool()), (Some(day(10).as_str()), Some(pe03.as_str()), Some(false)), "{v}");
    // 可以翻看的月份：最早开始收费的那个月 → 下个月
    let months: Vec<&str> = v["months"].as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect();
    assert_eq!(months, [month(-3), month(-2), month(-1), month(0), month(1), month(2)], "{v}");
    // 开始 10 天就停了的只有开始那个月的一期；之后的月份没有
    assert_eq!(listed(&get_month(month(-3)).await), named(&[("手动添加的节点", false)]));
    assert!(listed(&get_month(month(-2)).await).is_empty());
    // 默认本月；月份写错 400
    assert_eq!(t.get(&format!("/api/v1/teams/{team1}/billing/month")).await["month"], month(1));
    assert_eq!(t.req("GET", &format!("/api/v1/teams/{team1}/billing/month?month=2026-13"), None, None).await.0, StatusCode::BAD_REQUEST);

    // 标记已支付：团队1-01、团队1-02 → 支付状态不改变生效日顺序；再标一次不重复记
    let id_of = |v: &Value, name: &str| v["nodes"].as_array().unwrap().iter().find(|n| n["name"] == name).unwrap()["slot_id"].as_str().unwrap().to_string();
    let (s1, s2) = (id_of(&v, "团队1-01"), id_of(&v, "团队1-02"));
    let paid = |ids: Vec<&String>, m: String, on: bool| {
        let t = &t;
        let ids: Vec<String> = ids.into_iter().cloned().collect();
        async move { t.req("POST", &format!("/api/v1/teams/{team1}/billing/paid"), Some(json!({"month": m, "slot_ids": ids, "paid": on})), None).await }
    };
    let mut ev = t.st.events.subscribe();
    let (st, v) = paid(vec![&s1, &s2], month(0), true).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(listed(&v["data"]), named(&[("团队1-01", true), ("团队1-02", true), ("团队1-03", false), ("墨西哥家宽", false)]), "{v}");
    assert!(v["data"]["nodes"][0]["paid_at"].is_string(), "{v}");
    assert!(std::iter::from_fn(|| ev.try_recv().ok()).any(|e| matches!(e, Event::Billing { id } if id == team1)), "标记要发 billing 事件");
    assert_eq!(paid(vec![&s1], month(0), true).await.0, StatusCode::OK);
    // 改回待支付
    let (_, v) = paid(vec![&s2], month(0), false).await;
    assert_eq!(listed(&v["data"]), named(&[("团队1-01", true), ("团队1-02", false), ("团队1-03", false), ("墨西哥家宽", false)]), "{v}");
    // 还没到的期也能先标（客户预付）
    let (st, v) = paid(vec![&s1], month(2), true).await;
    let row = v["data"]["nodes"].as_array().unwrap().iter().find(|n| n["slot_id"] == s1.as_str()).cloned().unwrap();
    assert_eq!((st, row["paid"].as_bool(), row["upcoming"].as_bool()), (StatusCode::OK, Some(true), Some(true)), "{v}");
    // 不该收的月份 / 不是这个团队的节点：400，什么都不改
    let (st, v) = paid(vec![&old_id], month(0), true).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("没有要收这些节点的钱"), "{v}");
    let other = t.get(&format!("/api/v1/teams/{team2}/batches")).await["slots"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(paid(vec![&s1, &other], month(0), false).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(paid(vec![&s1], month(-2), true).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(paid(vec![&s1], "2026/11".into(), true).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(listed(&get_month(month(0)).await).iter().filter(|n| n.1).count(), 1);
    // 别的月份各记各的：本月团队1-01 还是待支付
    assert!(listed(&get_month(month(1)).await).contains(&("团队1-01".to_string(), false)));

    // 跨月待支付：与逐月查看时所有已经开始且待支付的期次一致（含同一名额在不同月份的期）。
    let due = t.get(&format!("/api/v1/teams/{team1}/billing/outstanding")).await;
    let mut expected = Vec::new();
    for m in get_month(month(0)).await["months"].as_array().unwrap() {
        let view = get_month(m.as_str().unwrap().to_string()).await;
        for n in view["nodes"].as_array().unwrap() {
            if n["upcoming"] == false && n["paid"] == false {
                expected.push(n.clone());
            }
        }
    }
    let actual = due["nodes"].as_array().unwrap();
    assert!(!expected.is_empty());
    assert_eq!(actual.len(), expected.len());
    assert!(expected.iter().all(|n| actual.contains(n)));

    // 客户页：这个月每个节点的那一期 名字 · 地区 · 区间 · 已付 / 待付（按本期生效日排列）；只有这几项，不出 IP、供应商、价格
    let page = t.post(&format!("/api/v1/teams/{team1}/customer-page"), json!({})).await;
    let link = page["link"].as_str().unwrap();
    let password = page["password"].as_str().unwrap();
    assert_eq!(public(&t, link, "").await.0, StatusCode::FORBIDDEN);
    assert_eq!(public(&t, link, "wrong").await.0, StatusCode::FORBIDDEN);
    let (st, pv) = public(&t, &format!("{link}?month={}", month(0)), password).await;
    assert_eq!(st, StatusCode::OK, "{pv}");
    let d = &pv["data"];
    assert_eq!(listed(d), named(&[("团队1-01", true), ("团队1-02", false), ("团队1-03", false), ("墨西哥家宽", false)]), "{pv}");
    assert_eq!((d["team"].as_str(), d["months"].as_array().map(|a| a.len())), (Some("团队1"), Some(6)), "{pv}"); // 与管理端一致，可以翻到下个月
    let h = d["nodes"].as_array().unwrap().iter().find(|n| n["name"] == "墨西哥家宽").unwrap();
    let pe = fmt_date(meta::add_months(s + Duration::days(14), 1).unwrap());
    assert_eq!(h, &json!({"name": "墨西哥家宽", "region": "墨西哥", "period_start": day(14), "period_end": pe, "paid": false, "manual": true, "ended": false}), "{pv}");
    let expected_count = expected.len();
    let actual_count: u64 = d["outstanding"].as_array().unwrap().iter().map(|item| item["count"].as_u64().unwrap()).sum();
    assert_eq!(actual_count, expected_count as u64);
    let body = pv.to_string();
    assert!(!body.contains("203.0.113") && !body.contains("provider") && !body.contains("price") && !body.contains("slot_id") && !body.contains("菲律宾"), "{pv}");
    // 默认本月；月份写错 400
    assert_eq!(public(&t, link, password).await.1["data"]["month"], month(1));
    // 与管理端一样查看未来期次
    assert_eq!(public(&t, &format!("{link}?month={}", month(2)), password).await.1["data"]["month"], month(2));
    assert_eq!(public(&t, &format!("{link}?month=abc"), password).await.0, StatusCode::BAD_REQUEST);

    sqlx::query("update customer_page set token_at = now() - interval '1 hour' where team_id = $1")
        .bind(team1).execute(&t.st.db).await.unwrap();
    assert_eq!(public(&t, link, password).await.0, StatusCode::NOT_FOUND);
    fleet_server::api::batches::refresh_customer_pages(&t.st).await.unwrap();
    let automatic = t.get(&base1).await["page"].clone();
    assert_eq!(automatic["enabled"], true);
    assert_ne!(automatic["link"], page["link"]);
    assert_ne!(automatic["password"], page["password"]);
    let once = automatic.clone();
    fleet_server::api::batches::refresh_customer_pages(&t.st).await.unwrap();
    assert_eq!(t.get(&base1).await["page"], once);
    assert_eq!(public(&t, link, password).await.0, StatusCode::NOT_FOUND);
    assert_eq!(public(&t, automatic["link"].as_str().unwrap(), automatic["password"].as_str().unwrap()).await.0, StatusCode::OK);
    sqlx::query("update customer_page set enabled = false, token_at = now() - interval '2 hours' where team_id = $1")
        .bind(team1).execute(&t.st.db).await.unwrap();
    fleet_server::api::batches::refresh_customer_pages(&t.st).await.unwrap();
    assert_eq!(t.get(&base1).await["page"]["link"], automatic["link"]);
    sqlx::query("update customer_page set enabled = true where team_id = $1").bind(team1).execute(&t.st.db).await.unwrap();

    let fresh = t.post(&format!("/api/v1/teams/{team1}/customer-page"), json!({})).await;
    assert_eq!(public(&t, link, password).await.0, StatusCode::NOT_FOUND);
    let new_link = fresh["link"].as_str().unwrap();
    assert_eq!(public(&t, new_link, password).await.0, StatusCode::FORBIDDEN);
    assert_eq!(public(&t, new_link, fresh["password"].as_str().unwrap()).await.0, StatusCode::OK);

    // ── 对账（传入的「现在」）──
    let today_s = fmt_date(today());
    // 不猜补位：团队1-01 退役、同一轮来了团队1-04 → 团队1-01 今天结束（上个月标的已支付留在它的名额上）、团队1-04 新增（没有订阅 = 今天开始收费）
    sqlx::query("update machine set lifecycle = 'retired', retired_at = now() where id = $1").bind(n1).execute(&t.st.db).await.unwrap();
    let n4 = node(&t, team1, 4, "US").await;
    ledger::reconcile(&t.st, now0).await.unwrap();
    let v = t.get(&base1).await;
    let (s1v, s4) = (slot_of(&v, "团队1-01"), slot_of(&v, "团队1-04"));
    assert_eq!((s1v["state"].as_str(), s1v["end_date"].as_str(), s1v["left_reason"].as_str()), (Some("ended"), Some(today_s.as_str()), Some("left")), "{v}");
    assert_eq!((s4["start_date"].as_str(), s4["holder"]["id"].as_str()), (Some(today_s.as_str()), Some(n4.to_string().as_str())), "{v}");
    assert!(listed(&get_month(month(0)).await).contains(&("团队1-01".to_string(), true)));
    assert!(v.get("pending").is_none() && get_month(month(1)).await.get("pending").is_none(), "没有待定了：{v}");
    // 有订阅的从购买日开始收费：到期日往前推一个周期（季付推 3 个月）；同机 IP 节点没有自己的订阅看宿主机的
    let host = node(&t, team1, 5, "US").await;
    let due = today() + Duration::days(20);
    sqlx::query("insert into subscription (machine_id, price_cents, cycle, due_date) values ($1, 500, 'quarterly', $2)").bind(host).bind(due).execute(&t.st.db).await.unwrap();
    sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, ssh_port, lifecycle, host_id, country, activated_at) values ($1, 'vps', $2, 6, '203.0.113.66', null, 'active', $3, 'US', now())")
        .bind(Uuid::now_v7())
        .bind(team1)
        .bind(host)
        .execute(&t.st.db)
        .await
        .unwrap();
    ledger::reconcile(&t.st, now0).await.unwrap();
    let v = t.get(&base1).await;
    let bought = meta::add_months(due, -3).unwrap();
    assert_eq!((slot_of(&v, "团队1-05")["start_date"].as_str(), slot_of(&v, "团队1-06")["start_date"].as_str()), (Some(fmt_date(bought).as_str()), Some(fmt_date(bought).as_str())), "{v}");
    // 团队1-02 设为不计费：机器视图带上；设完马上对账一次，名额今天结束（离开原因 exempt，已经开始的那一期照收）
    let mut ev = t.st.events.subscribe();
    let r = t.req("PUT", &format!("/api/v1/machines/{n2}/billing-exempt"), Some(json!({"exempt": true})), None).await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    assert_eq!(t.get(&format!("/api/v1/machines/{n2}")).await["billing_exempt"], true);
    assert!(std::iter::from_fn(|| ev.try_recv().ok()).any(|e| matches!(e, Event::Billing { id } if id == team1)), "设不计费要发 billing 事件");
    let v = t.get(&base1).await;
    let s2v = slot_of(&v, "团队1-02");
    assert_eq!((s2v["state"].as_str(), s2v["end_date"].as_str(), s2v["left_reason"].as_str()), (Some("ended"), Some(today_s.as_str()), Some("exempt")), "{v}");
    // 有占用者的离开原因是 null
    assert!(slot_of(&v, "团队1-04")["left_reason"].is_null(), "{v}");
    // 机器被物理删除：外键置空，对账照常结束名额（不报错）
    sqlx::query("delete from machine where id = $1").bind(n3).execute(&t.st.db).await.unwrap();
    ledger::reconcile(&t.st, now0).await.unwrap();
    let s3v = slot_of(&t.get(&base1).await, "团队1-03").clone();
    assert_eq!((s3v["state"].as_str(), s3v["left_reason"].as_str()), (Some("ended"), Some("left")), "{s3v}");

    // 增减记录：一句话、新的在前、可翻页
    let all = texts(&t, team1).await;
    let m0 = format!("{} 月", u8::from(s.month()));
    let m2 = format!("{} 月", u8::from(meta::add_months(s, 2).unwrap().month()));
    for want in [
        "团队1-01 离开，收费到 ".to_string(),
        format!("团队1-04 加入，从 {}开始收费", fmt_cn(today())),
        format!("团队1-05 加入，从 {}开始收费", fmt_cn(bought)),
        "团队1-02 设为不收费".into(),
        "团队1-02 离开，收费到 ".into(),
        "团队1-03 离开，".into(),
        "开始记账：3 个节点从 ".into(),
        format!("{m0}已支付：团队1-01、团队1-02（共 2 个）"),
        format!("{m0}改回待支付：团队1-02（共 1 个）"),
        format!("{}已支付：团队1-01（共 1 个）", m2),
    ] {
        assert!(all.iter().any(|x| x.starts_with(&want)), "没有「{want}」: {all:?}");
    }
    assert!(!all.iter().any(|x| x.contains("补位")), "对账不猜补位：{all:?}");
    // 初始化每个团队只一条，名单在 detail 里
    let inits: Vec<Value> = sqlx::query_scalar("select detail from billing_event where team_id = $1 and kind = 'init'").bind(team1).fetch_all(&t.st.db).await.unwrap();
    assert_eq!((inits.len(), inits[0]["nodes"].as_array().map(|a| a.len())), (1, Some(3)), "{inits:?}");
    let first = t.get(&format!("/api/v1/teams/{team1}/billing-events?limit=2")).await;
    let next = t.get(&format!("/api/v1/teams/{team1}/billing-events?limit=2&before={}", first[1]["id"].as_str().unwrap())).await;
    assert_eq!(next[0]["text"].as_str(), Some(all[2].as_str()));

    let acts: Vec<String> = sqlx::query_scalar("select action from audit_log where action like 'billing.%' order by id").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(acts, ["billing.init", "billing.init", "billing.slot_update", "billing.slot_add", "billing.slot_add", "billing.slot_update", "billing.paid", "billing.unpaid", "billing.paid", "billing.exempt"]);
}

/// 改开始收费日：这个名额已支付的期按期序号挪到新开始日的第 k 期（保留已支付时刻、旧开始日期序列上没有的删掉）；开始收费日最晚今天加一个月
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_date_change_moves_paid() {
    let t = common::boot(false, "").await;
    let team = common::seed_team(&t, "团队1").await;
    node(&t, team, 1, "US").await;
    let s = meta::add_months(today().replace_day(1).unwrap(), -1).unwrap();
    t.post("/api/v1/billing/init", json!({"since": fmt_date(s)})).await;
    let sid = slot_of(&t.get(&format!("/api/v1/teams/{team}/batches")).await, "团队1-01")["id"].as_str().unwrap().to_string();
    let mark = |m: Date| {
        let (t, sid) = (&t, sid.clone());
        async move { t.req("POST", &format!("/api/v1/teams/{team}/billing/paid"), Some(json!({"month": fmt_month(m), "slot_ids": [sid], "paid": true})), None).await }
    };
    // 上个月、本月两期都标已支付；再造一条不在期序列上的旧行（不该有，改开始收费日时要清掉）
    let (m1, m2) = (s, meta::add_months(s, 1).unwrap());
    assert_eq!((mark(m1).await.0, mark(m2).await.0), (StatusCode::OK, StatusCode::OK));
    let sid_u = Uuid::parse_str(&sid).unwrap();
    sqlx::query("insert into billing_paid (slot_id, period_start) values ($1, $2)").bind(sid_u).bind(s + Duration::days(3)).execute(&t.st.db).await.unwrap();
    let rows = || async { sqlx::query_as::<_, (Date, OffsetDateTime)>("select period_start, paid_at from billing_paid where slot_id = $1 order by period_start").bind(sid_u).fetch_all(&t.st.db).await.unwrap() };
    let before = rows().await;
    assert_eq!(before.len(), 3);
    // 开始收费日改到 6 号：第 0、1 期挪到 s+5 天、下个月的 6 号，已支付时刻不变；孤儿行删掉
    let new_start = s + Duration::days(5);
    let (st, v) = t.req("PATCH", &format!("/api/v1/billing-slots/{sid}"), Some(json!({"start_date": fmt_date(new_start)})), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let after = rows().await;
    let want = [new_start, meta::add_months(new_start, 1).unwrap()];
    assert_eq!(after.iter().map(|r| r.0).collect::<Vec<_>>(), want, "{after:?}");
    assert_eq!((after[0].1, after[1].1), (before[0].1, before[2].1), "已支付时刻跟着走");
    let cur = t.get(&format!("/api/v1/teams/{team}/billing/month?month={}", fmt_month(m1))).await;
    assert_eq!((cur["nodes"][0]["period_start"].as_str(), cur["nodes"][0]["paid"].as_bool()), (Some(fmt_date(new_start).as_str()), Some(true)), "{cur}");
    let texts = texts(&t, team).await;
    assert!(texts[0].contains("2 期的已支付按期序号挪到了新的期") && texts[0].contains("1 条对不上期的已支付记录已清掉"), "{texts:?}");

    // 开始收费日最晚今天加一个月：新增、修改、初始化都一样；正好是上限可以
    let max = meta::add_months(today(), 1).unwrap();
    let late = fmt_date(max + Duration::days(1));
    let (st, v) = t.req("POST", &format!("/api/v1/teams/{team}/billing-slots"), Some(json!({"region": "墨西哥", "start_date": late})), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains(&fmt_date(max)), "{v}");
    assert_eq!(t.req("PATCH", &format!("/api/v1/billing-slots/{sid}"), Some(json!({"start_date": late})), None).await.0, StatusCode::BAD_REQUEST);
    let (st, v) = t.req("POST", &format!("/api/v1/teams/{team}/billing-slots"), Some(json!({"region": "墨西哥", "start_date": fmt_date(max), "note": "家宽"})), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // 能翻到那个月：最后一个月就是下个月
    let months = t.get(&format!("/api/v1/teams/{team}/billing/month")).await["months"].clone();
    assert_eq!(months.as_array().unwrap().last().and_then(|m| m.as_str()), Some(fmt_month(meta::add_months(today().replace_day(1).unwrap(), 1).unwrap()).as_str()), "{months}");
}

/// 迁移 0098：旧的逐节点 init 按 (团队, 起算日) 合并成一条新格式，可重复执行；回填空出名额的原因
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0098_merges_old_init_events() {
    let (pool, _schema) = common::db_until(97).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    for (id, name) in [(a, "团队A"), (b, "团队B")] {
        sqlx::query("insert into team (id, name) values ($1, $2)").bind(id).bind(name).execute(&pool).await.unwrap();
    }
    // 旧格式：A 有两个起算日（10-01 两个节点、10-05 一个），B 一个；A 另有一条已经是新格式的 init 与一条别的事件
    let old = [(a, "团队A-01", "美区", "2026-10-01", 10), (a, "团队A-02", "墨西哥", "2026-10-01", 11), (a, "团队A-03", "美区", "2026-10-05", 12), (b, "团队B-01", "美区", "2026-10-01", 13)];
    for (team, name, region, start, min) in old {
        sqlx::query("insert into billing_event (id, team_id, at, kind, machine_name, detail) values ($1, $2, $3, 'init', $4, $5)")
            .bind(Uuid::now_v7())
            .bind(team)
            .bind(OffsetDateTime::UNIX_EPOCH + Duration::minutes(min))
            .bind(name)
            .bind(json!({"region": region, "start_date": start}))
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("insert into billing_event (id, team_id, at, kind, detail) values ($1, $2, now(), 'init', $3), ($4, $2, now(), 'add', '{}')")
        .bind(Uuid::now_v7())
        .bind(b)
        .bind(json!({"start_date": "2026-10-02", "count": 1, "nodes": [{"name": "团队B-09", "region": "美区"}]}))
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .unwrap();
    // 存量名额：一个空出的系统名额（回填成 left，exempt 的回填走同一条 SQL，要机器行才能标不计费，这里不造）、一个手动的（保持 null）
    for manual in [false, true] {
        sqlx::query("insert into billing_slot (id, team_id, region, start_date, vacated_at, prev_holder_id, manual) values ($1, $2, '美区', '2026-10-01', $3, $4, $5)")
            .bind(Uuid::now_v7())
            .bind(a)
            .bind((!manual).then(OffsetDateTime::now_utc))
            .bind((!manual).then(Uuid::now_v7))
            .bind(manual)
            .execute(&pool)
            .await
            .unwrap();
    }
    common::migrator(None).run(&pool).await.unwrap();
    let merged = |team: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (OffsetDateTime, Value)>("select at, detail from billing_event where team_id = $1 and kind = 'init' order by detail ->> 'start_date', at")
                .bind(team)
                .fetch_all(&pool)
                .await
                .unwrap()
        }
    };
    for _ in 0..2 {
        let ra = merged(a).await;
        assert_eq!(ra.len(), 2, "{ra:?}");
        assert_eq!(ra[0].0, OffsetDateTime::UNIX_EPOCH + Duration::minutes(10));
        assert_eq!(ra[0].1, json!({"start_date": "2026-10-01", "count": 2, "nodes": [{"name": "团队A-01", "region": "美区"}, {"name": "团队A-02", "region": "墨西哥"}]}));
        assert_eq!(ra[1].1, json!({"start_date": "2026-10-05", "count": 1, "nodes": [{"name": "团队A-03", "region": "美区"}]}));
        // B：旧的合成一条，原本就是新格式的不动，别的 kind 不动
        let rb = merged(b).await;
        assert_eq!(rb.iter().map(|r| r.1["count"].as_i64()).collect::<Vec<_>>(), [Some(1), Some(1)], "{rb:?}");
        assert_eq!(rb[0].1["nodes"][0]["name"], "团队B-01");
        assert_eq!(rb[1].1["nodes"][0]["name"], "团队B-09");
        let adds: i64 = sqlx::query_scalar("select count(*) from billing_event where kind = 'add'").fetch_one(&pool).await.unwrap();
        assert_eq!(adds, 1);
        let reasons: Vec<(bool, Option<String>)> = sqlx::query_as("select manual, vacate_reason from billing_slot order by manual").fetch_all(&pool).await.unwrap();
        assert_eq!(reasons, [(false, Some("left".to_string())), (true, None)]);
        // 再执行一遍：没有旧行，无操作
        sqlx::raw_sql(include_str!("../../../../migrations/0098_billing_vacate_reason.sql")).execute(&pool).await.unwrap();
    }
}

/// 系统名额离开即结束：它的停止收费日只能改到不晚于今天；不能手动恢复（节点回来由对账接回）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ended_system_slot_edit() {
    let t = common::boot(false, "").await;
    let team = common::seed_team(&t, "团队1").await;
    let n1 = node(&t, team, 1, "US").await;
    node(&t, team, 2, "US").await;
    let s = meta::add_months(today().replace_day(1).unwrap(), -1).unwrap();
    t.post("/api/v1/billing/init", json!({"since": fmt_date(s)})).await;
    t.req("PUT", &format!("/api/v1/machines/{n1}/billing-exempt"), Some(json!({"exempt": true})), None).await;
    let d = t.get(&format!("/api/v1/teams/{team}/batches")).await;
    let sid = slot_of(&d, "团队1-01")["id"].as_str().unwrap().to_string();
    assert_eq!((slot_of(&d, "团队1-01")["state"].as_str(), d["totals"]["ended"].as_i64(), d["totals"]["active"].as_i64()), (Some("ended"), Some(1), Some(1)), "{d}");
    let tomorrow = fmt_date(today() + Duration::days(1));
    let (st, v) = t.req("PATCH", &format!("/api/v1/billing-slots/{sid}"), Some(json!({"end_date": tomorrow})), None).await;
    assert_eq!((st, v["error"]["message"].as_str()), (StatusCode::BAD_REQUEST, Some("系统节点的停止收费日不能晚于今天")), "{v}");
    assert_eq!(t.req("PATCH", &format!("/api/v1/billing-slots/{sid}"), Some(json!({"end_date": null})), None).await.0, StatusCode::CONFLICT);
    let yesterday = fmt_date(today() - Duration::days(1));
    let (st, v) = t.req("PATCH", &format!("/api/v1/billing-slots/{sid}"), Some(json!({"end_date": yesterday})), None).await;
    assert_eq!((st, v["data"]["state"].as_str(), v["data"]["left_reason"].as_str()), (StatusCode::OK, Some("ended"), Some("exempt")), "改结束日保留离开原因：{v}");
}

/// 离开原因落库（结束时按当时是否不计费写 exempt / left，之后节点状态变了也不跟着变）；回来的节点在收费期内原样接回，不记事件
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leave_and_come_back() {
    let t = common::boot(false, "").await;
    let team = common::seed_team(&t, "团队1").await;
    let n1 = node(&t, team, 1, "US").await;
    let n2 = node(&t, team, 2, "US").await;
    // 10 天前开始收费：今天在第一期中间，结束后「收费到」一定不早于今天
    let s = today() - Duration::days(10);
    t.post("/api/v1/billing/init", json!({"since": fmt_date(s)})).await;
    let base = format!("/api/v1/teams/{team}/batches");
    let id_of = |v: &Value, name: &str| slot_of(v, name)["id"].clone();
    let before = t.get(&base).await;
    async fn reason(t: &T, team: Uuid, name: &str) -> Value { slot_of(&t.get(&format!("/api/v1/teams/{team}/batches")).await, name)["left_reason"].clone() }
    // 设不计费 → 结束，exempt；恢复计费 → 同一个名额接回，没结束、原因清空
    t.req("PUT", &format!("/api/v1/machines/{n1}/billing-exempt"), Some(json!({"exempt": true})), None).await;
    assert_eq!(reason(&t, team, "团队1-01").await, "exempt");
    t.req("PUT", &format!("/api/v1/machines/{n1}/billing-exempt"), Some(json!({"exempt": false})), None).await;
    let v = t.get(&base).await;
    let s1 = slot_of(&v, "团队1-01");
    assert_eq!((id_of(&v, "团队1-01"), s1["state"].as_str(), s1["end_date"].is_null(), s1["left_reason"].is_null()), (id_of(&before, "团队1-01"), Some("active"), true, true), "{v}");
    // 退役离开 → left；之后又被标成不计费，原因不变（落库的，不按现在的状态推断）
    sqlx::query("update machine set lifecycle = 'retired', retired_at = now() where id = $1").bind(n2).execute(&t.st.db).await.unwrap();
    ledger::reconcile(&t.st, OffsetDateTime::now_utc()).await.unwrap();
    assert_eq!(reason(&t, team, "团队1-02").await, "left");
    t.req("PUT", &format!("/api/v1/machines/{n2}/billing-exempt"), Some(json!({"exempt": true})), None).await;
    assert_eq!(reason(&t, team, "团队1-02").await, "left");
    // 撤销退役并恢复计费：接回同一个名额
    t.req("PUT", &format!("/api/v1/machines/{n2}/billing-exempt"), Some(json!({"exempt": false})), None).await;
    sqlx::query("update machine set lifecycle = 'active', retired_at = null where id = $1").bind(n2).execute(&t.st.db).await.unwrap();
    ledger::reconcile(&t.st, OffsetDateTime::now_utc()).await.unwrap();
    let v = t.get(&base).await;
    assert_eq!((id_of(&v, "团队1-02"), reason(&t, team, "团队1-02").await), (id_of(&before, "团队1-02"), Value::Null), "{v}");
    assert_eq!(v["slots"].as_array().unwrap().len(), 2, "没有新建名额：{v}");
    let adds: i64 = sqlx::query_scalar("select count(*) from billing_event where kind = 'add'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(adds, 0, "接回不记 add");
    // 手动名额恒为 null
    let m = t.post(&format!("/api/v1/teams/{team}/billing-slots"), json!({"region": "墨西哥", "start_date": fmt_date(s), "note": "家宽"})).await;
    assert!(m[0]["left_reason"].is_null(), "{m}");
}

/// 替换节点接名额（ledger::take_over，替换作业交换编号的同一个事务里调）：新节点接过旧名额，开始收费日、地区、已支付都不变，记「补位接替」；
/// 新节点上架后对账给它建的名额连同那条 add 删掉，它已支付的期挪到接过的名额同一个月那一期；旧节点进了备用不另记离开
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replace_takes_over_slot() {
    let t = common::boot(false, "").await;
    let team = common::seed_team(&t, "团队2").await;
    let spare: Uuid = sqlx::query_scalar("select id from team where spare").fetch_one(&t.st.db).await.unwrap();
    let old = node(&t, team, 7, "US").await;
    let s = meta::add_months(today().replace_day(1).unwrap(), -1).unwrap();
    t.post("/api/v1/billing/init", json!({"since": fmt_date(s)})).await;
    // 新节点上架进团队：对账马上给它建名额（今天开始收费）；本月先标了已支付
    let new = node(&t, team, 20, "US").await;
    let now = OffsetDateTime::now_utc();
    ledger::reconcile(&t.st, now).await.unwrap();
    let v = t.get(&format!("/api/v1/teams/{team}/batches")).await;
    let (old_slot, new_slot) = (slot_of(&v, "团队2-07")["id"].as_str().unwrap().to_string(), slot_of(&v, "团队2-20")["id"].as_str().unwrap().to_string());
    let this = fmt_month(today());
    let r = t.req("POST", &format!("/api/v1/teams/{team}/billing/paid"), Some(json!({"month": this, "slot_ids": [new_slot], "paid": true})), None).await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    // 作业里交换编号的那一步：旧节点去备用、新节点接旧编号，同一个事务里接名额
    let mut tx = t.st.db.begin().await.unwrap();
    ledger::lock_team(&mut tx, team).await.unwrap();
    sqlx::query("update machine set team_id = $2, number = 1 where id = $1").bind(old).bind(spare).execute(&mut *tx).await.unwrap();
    sqlx::query("update machine set number = 7 where id = $1").bind(new).execute(&mut *tx).await.unwrap();
    assert!(ledger::take_over(&mut tx, team, old, new, ("团队2-07", "团队2-20"), now).await.unwrap());
    tx.commit().await.unwrap();
    // 之后对账什么都不变
    assert_eq!(ledger::reconcile(&t.st, now).await.unwrap(), 0);
    let v = t.get(&format!("/api/v1/teams/{team}/batches")).await;
    let slots = v["slots"].as_array().unwrap();
    assert_eq!(slots.len(), 1, "新节点自己的名额删掉了：{v}");
    let s7 = &slots[0];
    assert_eq!((s7["id"].as_str(), s7["name"].as_str(), s7["start_date"].as_str(), s7["holder"]["id"].as_str()), (Some(old_slot.as_str()), Some("团队2-07"), Some(fmt_date(s).as_str()), Some(new.to_string().as_str())), "{v}");
    let cur = t.get(&format!("/api/v1/teams/{team}/billing/month?month={this}")).await;
    assert_eq!(listed(&cur), named(&[("团队2-07", true)]), "已支付挪到接过的名额本月那一期：{cur}");
    let all = texts(&t, team).await;
    assert!(all.iter().any(|x| x == "团队2-20 补位接替 团队2-07，收费不变"), "{all:?}");
    assert!(!all.iter().any(|x| x.contains("加入") || x.contains("离开")), "新节点的 add 删掉、旧节点不记离开：{all:?}");
    // 旧节点没有名额（已经接过了）：再调什么都不做
    let mut tx = t.st.db.begin().await.unwrap();
    assert!(!ledger::take_over(&mut tx, team, old, new, ("团队2-07", "团队2-20"), now).await.unwrap());
}

/// 迁移 0104：现有待定行按「加入即计费」转成名额（开始收费日 = 购买日，没有订阅的是第一次看到那天；记 add），不可计费的丢掉，删表；可重复执行
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0104_converts_pending() {
    let (pool, _schema) = common::db_until(101).await;
    let team = Uuid::now_v7();
    sqlx::query("insert into team (id, name) values ($1, '团队A')").bind(team).execute(&pool).await.unwrap();
    let mut ids = vec![];
    for (n, country, lifecycle) in [(1, Some("US"), "active"), (2, Some("MX"), "active"), (3, None, "retired")] {
        let id = Uuid::now_v7();
        sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, country, lifecycle, activated_at) values ($1, 'proxy', $2, $3, $4::inet, $5, $6::machine_lifecycle, now())")
            .bind(id)
            .bind(team)
            .bind(n)
            .bind(format!("203.0.113.{n}"))
            .bind(country)
            .bind(lifecycle)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("insert into billing_pending (machine_id, team_id, seen_at) values ($1, $2, '2026-10-30 20:00+00')").bind(id).bind(team).execute(&pool).await.unwrap();
        ids.push(id);
    }
    // 1 号有月付订阅（到期 10-20 → 9-20 买的）；2 号没有订阅（北京时间 10-31 第一次看到）；3 号已退役，丢掉
    sqlx::query("insert into subscription (machine_id, price_cents, due_date) values ($1, 500, '2026-10-20')").bind(ids[0]).execute(&pool).await.unwrap();
    common::migrator(None).run(&pool).await.unwrap();
    for _ in 0..2 {
        let slots: Vec<(Uuid, String, Date)> = sqlx::query_as("select holder_id, region, start_date from billing_slot order by holder_name").fetch_all(&pool).await.unwrap();
        let d = |s: &str| fleet_server::batches::parse_date(s).unwrap();
        assert_eq!(slots, [(ids[0], "美区".to_string(), d("2026-09-20")), (ids[1], "MX".to_string(), d("2026-10-31"))]);
        let adds: Vec<(Option<String>, Value)> = sqlx::query_as("select machine_name, detail from billing_event where kind = 'add' order by machine_name").fetch_all(&pool).await.unwrap();
        assert_eq!(adds[1], (Some("团队A-02".to_string()), json!({"region": "MX", "start_date": "2026-10-31"})), "{adds:?}");
        assert_eq!(adds.len(), 2);
        let gone: Option<String> = sqlx::query_scalar("select to_regclass('billing_pending')::text").fetch_one(&pool).await.unwrap();
        assert_eq!(gone, None);
        // 再执行一遍：表已经删了，无操作
        sqlx::raw_sql(include_str!("../../../../migrations/0104_billing_no_pending.sql")).execute(&pool).await.unwrap();
    }
}
