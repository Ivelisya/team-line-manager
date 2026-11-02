//! 计费台账与客户页：初始化（预览、只建没有名额的团队）→ 改开始收费日 → 手动名额 → 按月看每个节点起点在这个月的那一期（标记已收 / 改回没收、
//! 提前标、没有这一期的报 400）→ 客户页（只有名字、地区、这一期的区间、已付 / 待付）；对账（传入「现在」）：替换（收费状态跟着名额）、超时新增、标不收费后超时减少、
//! 机器被物理删除；增减记录与审计。24 小时配对的各种边界在 ledger.rs 的单测里（纯函数），这里只验证读库写库这一层接得上。

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

/// 某月清单里的 (名字, 已收)，按接口给的顺序
fn listed(v: &Value) -> Vec<(String, bool)> { v["nodes"].as_array().unwrap().iter().map(|n| (n["name"].as_str().unwrap().to_string(), n["paid"] == true)).collect() }

fn named(names: &[(&str, bool)]) -> Vec<(String, bool)> { names.iter().map(|(n, p)| (n.to_string(), *p)).collect() }

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
    // 上个月：月初的两个、月中开始的团队1-03、月中开始的家宽，各有一期从这个月开始（加入那个月就有一期）；默认都没收，按名字排；三个月前停掉的不在
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

    // 标记已收：团队1-01、团队1-02 → 已收的排到后面；再标一次不重复记
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
    assert_eq!(listed(&v["data"]), named(&[("团队1-03", false), ("墨西哥家宽", false), ("团队1-01", true), ("团队1-02", true)]), "{v}");
    assert!(v["data"]["nodes"][2]["paid_at"].is_string(), "{v}");
    assert!(std::iter::from_fn(|| ev.try_recv().ok()).any(|e| matches!(e, Event::Billing { id } if id == team1)), "标记要发 billing 事件");
    assert_eq!(paid(vec![&s1], month(0), true).await.0, StatusCode::OK);
    // 改回没收
    let (_, v) = paid(vec![&s2], month(0), false).await;
    assert_eq!(listed(&v["data"])[..3], named(&[("团队1-02", false), ("团队1-03", false), ("墨西哥家宽", false)])[..], "{v}");
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
    // 别的月份各记各的：本月团队1-01 还是未收
    assert!(listed(&get_month(month(1)).await).contains(&("团队1-01".to_string(), false)));

    // 客户页：这个月每个节点的那一期 名字 · 地区 · 区间 · 已付 / 待付（待付在前）；只有这几项，不出 IP、供应商、价格
    let page = t.post(&format!("/api/v1/teams/{team1}/customer-page"), json!({})).await;
    let link = page["link"].as_str().unwrap();
    let (st, pv) = public(&t, &format!("{link}?month={}", month(0))).await;
    assert_eq!(st, StatusCode::OK, "{pv}");
    let d = &pv["data"];
    assert_eq!(listed(d), named(&[("团队1-02", false), ("团队1-03", false), ("墨西哥家宽", false), ("团队1-01", true)]), "{pv}");
    assert_eq!((d["team"].as_str(), d["months"].as_array().map(|a| a.len())), (Some("团队1"), Some(5)), "{pv}"); // 客户只能翻到本月：没有下个月
    let h = d["nodes"].as_array().unwrap().iter().find(|n| n["name"] == "墨西哥家宽").unwrap();
    let pe = fmt_date(meta::add_months(s + Duration::days(14), 1).unwrap());
    assert_eq!(h, &json!({"name": "墨西哥家宽", "region": "墨西哥", "period_start": day(14), "period_end": pe, "paid": false}), "{pv}");
    let body = pv.to_string();
    assert!(!body.contains("203.0.113") && !body.contains("provider") && !body.contains("price") && !body.contains("slot_id") && !body.contains("菲律宾"), "{pv}");
    // 默认本月；月份写错 400
    assert_eq!(public(&t, link).await.1["data"]["month"], month(1));
    // 晚于本月的 ?month= 当本月
    assert_eq!(public(&t, &format!("{link}?month={}", month(2))).await.1["data"]["month"], month(1));
    assert_eq!(public(&t, &format!("{link}?month=abc")).await.0, StatusCode::BAD_REQUEST);

    // ── 对账（传入的「现在」）──
    // 团队1-01 退役、同一轮来了团队1-04：替换，开始收费日不变，上个月的「已收」跟着名额到团队1-04
    sqlx::query("update machine set lifecycle = 'retired', retired_at = now() where id = $1").bind(n1).execute(&t.st.db).await.unwrap();
    let n4 = node(&t, team1, 4, "US").await;
    ledger::reconcile(&t.st, now0).await.unwrap();
    let v = t.get(&base1).await;
    let s4 = slot_of(&v, "团队1-04");
    assert_eq!((s4["start_date"].as_str(), s4["holder"]["id"].as_str()), (Some(day(0).as_str()), Some(n4.to_string().as_str())), "{v}");
    assert!(listed(&get_month(month(0)).await).contains(&("团队1-04".to_string(), true)));
    // 再加一个：待定（本月的清单里不列，只给 pending），25 小时后成新名额（开始收费日 = 第一次看到它那天）
    node(&t, team1, 5, "US").await;
    ledger::reconcile(&t.st, now0).await.unwrap();
    let v = t.get(&base1).await;
    assert_eq!((v["pending"][0]["machine"]["name"].as_str(), v["totals"]["pending"].as_i64()), (Some("团队1-05"), Some(1)), "{v}");
    let cur = get_month(month(1)).await;
    assert_eq!((cur["pending"][0]["machine"]["name"].as_str(), listed(&cur).iter().any(|n| n.0 == "团队1-05")), (Some("团队1-05"), false), "{cur}");
    assert!(get_month(month(0)).await["pending"].as_array().unwrap().is_empty(), "以前的月份不提待定");
    ledger::reconcile(&t.st, now0 + Duration::hours(25)).await.unwrap();
    let v = t.get(&base1).await;
    let seen = now0.to_offset(meta::org_offset()).date();
    let seen_day = fmt_date(seen);
    assert_eq!((slot_of(&v, "团队1-05")["start_date"].as_str(), v["totals"]["pending"].as_i64()), (Some(seen_day.as_str()), Some(0)), "{v}");
    // 团队1-02 设为不计费：机器视图带上；设完马上对账一次，名额立刻空出待定（清单里还在，标着 vacated），满 24 小时没人接 → 结束
    let mut ev = t.st.events.subscribe();
    let r = t.req("PUT", &format!("/api/v1/machines/{n2}/billing-exempt"), Some(json!({"exempt": true})), None).await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    assert_eq!(t.get(&format!("/api/v1/machines/{n2}")).await["billing_exempt"], true);
    assert!(std::iter::from_fn(|| ev.try_recv().ok()).any(|e| matches!(e, Event::Billing { id } if id == team1)), "设不计费要发 billing 事件");
    let v = t.get(&base1).await;
    let s2v = slot_of(&v, "团队1-02");
    assert_eq!((s2v["state"].as_str(), s2v["last_holder_name"].as_str(), v["totals"]["vacated"].as_i64()), (Some("vacated"), Some("团队1-02"), Some(1)), "{v}");
    let row = get_month(month(1)).await["nodes"].as_array().unwrap().iter().find(|n| n["name"] == "团队1-02").cloned().unwrap();
    assert_eq!((row["vacated"].as_bool(), row["holder_id"].is_null()), (Some(true), true), "{row}");
    let t26 = now0 + Duration::hours(26);
    ledger::reconcile(&t.st, t26).await.unwrap();
    let s2v = slot_of(&t.get(&base1).await, "团队1-02").clone();
    // （空出就在刚才，结束日 = 空出那天）
    assert_eq!(s2v["end_date"].as_str(), Some(seen_day.as_str()), "{s2v}");
    // 空出原因：被设为不计费、节点还在团队里 = exempt；有占用者 = null
    assert_eq!((s2v["left_reason"].as_str(), slot_of(&t.get(&base1).await, "团队1-04")["left_reason"].is_null()), (Some("exempt"), true), "{s2v}");
    // 机器被物理删除：外键置空，对账照常把名额空出（不报错）
    sqlx::query("delete from machine where id = $1").bind(n3).execute(&t.st.db).await.unwrap();
    ledger::reconcile(&t.st, t26 + Duration::hours(52)).await.unwrap();
    let s3v = slot_of(&t.get(&base1).await, "团队1-03").clone();
    assert_eq!((s3v["state"].as_str(), s3v["left_reason"].as_str()), (Some("vacated"), Some("left")), "{s3v}");

    // 增减记录：一句话、新的在前、可翻页
    let all = texts(&t, team1).await;
    let m0 = format!("{} 月", u8::from(s.month()));
    let m2 = format!("{} 月", u8::from(meta::add_months(s, 2).unwrap().month()));
    for want in [
        "团队1-04 补位接替 团队1-01，收费不变".to_string(),
        format!("团队1-05 加入，从 {}开始收费", fmt_cn(seen)),
        "团队1-02 设为不收费".into(),
        "团队1-02 离开，收费到 ".into(),
        "开始记账：3 个节点从 ".into(),
        format!("{m0}已收：团队1-01、团队1-02（共 2 个）"),
        format!("{m0}改回没收：团队1-02（共 1 个）"),
        format!("{}已收：团队1-01（共 1 个）", m2),
    ] {
        assert!(all.iter().any(|x| x.starts_with(&want)), "没有「{want}」: {all:?}");
    }
    // 初始化每个团队只一条，名单在 detail 里
    let inits: Vec<Value> = sqlx::query_scalar("select detail from billing_event where team_id = $1 and kind = 'init'").bind(team1).fetch_all(&t.st.db).await.unwrap();
    assert_eq!((inits.len(), inits[0]["nodes"].as_array().map(|a| a.len())), (1, Some(3)), "{inits:?}");
    let first = t.get(&format!("/api/v1/teams/{team1}/billing-events?limit=2")).await;
    let next = t.get(&format!("/api/v1/teams/{team1}/billing-events?limit=2&before={}", first[1]["id"].as_str().unwrap())).await;
    assert_eq!(next[0]["text"].as_str(), Some(all[2].as_str()));

    let acts: Vec<String> = sqlx::query_scalar("select action from audit_log where action like 'billing.%' order by id").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(acts, ["billing.init", "billing.init", "billing.slot_update", "billing.slot_add", "billing.slot_add", "billing.slot_update", "billing.paid", "billing.unpaid", "billing.paid", "billing.exempt"]);
}

/// 改开始收费日：这个名额已收的期按期序号挪到新开始日的第 k 期（保留已收时刻、旧开始日期序列上没有的删掉）；开始收费日最晚今天加一个月
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
    // 上个月、本月两期都标已收；再造一条不在期序列上的旧行（不该有，改开始收费日时要清掉）
    let (m1, m2) = (s, meta::add_months(s, 1).unwrap());
    assert_eq!((mark(m1).await.0, mark(m2).await.0), (StatusCode::OK, StatusCode::OK));
    let sid_u = Uuid::parse_str(&sid).unwrap();
    sqlx::query("insert into billing_paid (slot_id, period_start) values ($1, $2)").bind(sid_u).bind(s + Duration::days(3)).execute(&t.st.db).await.unwrap();
    let rows = || async { sqlx::query_as::<_, (Date, OffsetDateTime)>("select period_start, paid_at from billing_paid where slot_id = $1 order by period_start").bind(sid_u).fetch_all(&t.st.db).await.unwrap() };
    let before = rows().await;
    assert_eq!(before.len(), 3);
    // 开始收费日改到 6 号：第 0、1 期挪到 s+5 天、下个月的 6 号，已收时刻不变；孤儿行删掉
    let new_start = s + Duration::days(5);
    let (st, v) = t.req("PATCH", &format!("/api/v1/billing-slots/{sid}"), Some(json!({"start_date": fmt_date(new_start)})), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let after = rows().await;
    let want = [new_start, meta::add_months(new_start, 1).unwrap()];
    assert_eq!(after.iter().map(|r| r.0).collect::<Vec<_>>(), want, "{after:?}");
    assert_eq!((after[0].1, after[1].1), (before[0].1, before[2].1), "已收时刻跟着走");
    let cur = t.get(&format!("/api/v1/teams/{team}/billing/month?month={}", fmt_month(m1))).await;
    assert_eq!((cur["nodes"][0]["period_start"].as_str(), cur["nodes"][0]["paid"].as_bool()), (Some(fmt_date(new_start).as_str()), Some(true)), "{cur}");
    let texts = texts(&t, team).await;
    assert!(texts[0].contains("2 期的已收按期序号挪到了新的期") && texts[0].contains("1 条对不上期的已收记录已清掉"), "{texts:?}");

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

/// 空出的系统名额填停止收费日：不能晚于今天，填今天就结束
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vacated_slot_end() {
    let t = common::boot(false, "").await;
    let team = common::seed_team(&t, "团队1").await;
    let n1 = node(&t, team, 1, "US").await;
    node(&t, team, 2, "US").await;
    let s = meta::add_months(today().replace_day(1).unwrap(), -1).unwrap();
    t.post("/api/v1/billing/init", json!({"since": fmt_date(s)})).await;
    // 节点设不计费：马上空出待定；给空出的系统名额填以后的停止收费日被拒，填今天结束
    t.req("PUT", &format!("/api/v1/machines/{n1}/billing-exempt"), Some(json!({"exempt": true})), None).await;
    let d = t.get(&format!("/api/v1/teams/{team}/batches")).await;
    let sid = slot_of(&d, "团队1-01")["id"].as_str().unwrap().to_string();
    assert_eq!((slot_of(&d, "团队1-01")["state"].as_str(), d["totals"]["vacated"].as_i64(), d["totals"]["active"].as_i64()), (Some("vacated"), Some(1), Some(1)), "{d}");
    let tomorrow = fmt_date(today() + Duration::days(1));
    let (st, v) = t.req("PATCH", &format!("/api/v1/billing-slots/{sid}"), Some(json!({"end_date": tomorrow})), None).await;
    assert_eq!((st, v["error"]["message"].as_str()), (StatusCode::BAD_REQUEST, Some("等补位的节点停止收费日不能晚于今天")), "{v}");
    let (st, v) = t.req("PATCH", &format!("/api/v1/billing-slots/{sid}"), Some(json!({"end_date": fmt_date(today())})), None).await;
    assert_eq!((st, v["data"]["state"].as_str(), v["data"]["left_reason"].as_str()), (StatusCode::OK, Some("ended"), Some("exempt")), "手动结束保留空出原因：{v}");
}

/// 空出原因落库：对账空出时按当时是否不计费写 exempt / left，接回清空，之后节点状态变了也不跟着变
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vacate_reason_is_stored() {
    let t = common::boot(false, "").await;
    let team = common::seed_team(&t, "团队1").await;
    let n1 = node(&t, team, 1, "US").await;
    let n2 = node(&t, team, 2, "US").await;
    let s = meta::add_months(today().replace_day(1).unwrap(), -1).unwrap();
    t.post("/api/v1/billing/init", json!({"since": fmt_date(s)})).await;
    let base = format!("/api/v1/teams/{team}/batches");
    async fn reason(t: &T, team: Uuid, name: &str) -> Value { slot_of(&t.get(&format!("/api/v1/teams/{team}/batches")).await, name)["left_reason"].clone() }
    // 设不计费空出 → exempt；恢复计费接回 → null（手动名额与有占用者的也是 null）
    t.req("PUT", &format!("/api/v1/machines/{n1}/billing-exempt"), Some(json!({"exempt": true})), None).await;
    assert_eq!(reason(&t, team, "团队1-01").await, "exempt");
    t.req("PUT", &format!("/api/v1/machines/{n1}/billing-exempt"), Some(json!({"exempt": false})), None).await;
    let v = t.get(&base).await;
    assert_eq!((slot_of(&v, "团队1-01")["state"].as_str(), slot_of(&v, "团队1-01")["left_reason"].is_null()), (Some("active"), true), "{v}");
    // 退役离开 → left；之后又被标成不计费，原因不变（落库的，不按现在的状态推断）
    sqlx::query("update machine set lifecycle = 'retired', retired_at = now() where id = $1").bind(n2).execute(&t.st.db).await.unwrap();
    ledger::reconcile(&t.st, OffsetDateTime::now_utc()).await.unwrap();
    assert_eq!(reason(&t, team, "团队1-02").await, "left");
    t.req("PUT", &format!("/api/v1/machines/{n2}/billing-exempt"), Some(json!({"exempt": true})), None).await;
    assert_eq!(reason(&t, team, "团队1-02").await, "left");
    // 回到团队并恢复计费：接回，清空
    t.req("PUT", &format!("/api/v1/machines/{n2}/billing-exempt"), Some(json!({"exempt": false})), None).await;
    sqlx::query("update machine set lifecycle = 'active', retired_at = null where id = $1").bind(n2).execute(&t.st.db).await.unwrap();
    ledger::reconcile(&t.st, OffsetDateTime::now_utc()).await.unwrap();
    assert!(reason(&t, team, "团队1-02").await.is_null());
    // 手动名额恒为 null
    let m = t.post(&format!("/api/v1/teams/{team}/billing-slots"), json!({"region": "墨西哥", "start_date": fmt_date(s), "note": "家宽"})).await;
    assert!(m[0]["left_reason"].is_null(), "{m}");
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
