//! 访问监控：构造 access_event → 跑汇总 → 各接口的聚合结果。
//! 需要 PostgreSQL（见 common_mon.rs）。

mod common_mon;

use axum::http::StatusCode;
use common_mon::{put_events, seed, setup, T};
use fleet_server::access;
use serde_json::Value;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// 两个测试共用一个库，串行跑
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn row<'a>(d: &'a Value, key: &str, field: &str, val: &str) -> &'a Value {
    d[key].as_array().unwrap().iter().find(|n| n[field] == val).unwrap_or_else(|| panic!("{key} 里没有 {field}={val}：{d}"))
}

fn ts(v: &Value) -> OffsetDateTime { OffsetDateTime::parse(v.as_str().unwrap(), &time::format_description::well_known::Rfc3339).unwrap() }

fn hosts(d: &Value) -> Vec<String> { d["items"].as_array().unwrap().iter().map(|i| i["dest_host"].as_str().unwrap().to_string()).collect() }

fn item<'a>(d: &'a Value, host: &str, node: &str) -> &'a Value {
    d["items"].as_array().unwrap().iter().find(|i| i["dest_host"] == host && i["name"] == node).unwrap_or_else(|| panic!("没有 {node} 的 {host}：{d}"))
}

/// 节点1 = 团队1-01，节点2 = 团队1-02，节点3 = 团队2-01
async fn fixture(t: &T) -> common_mon::Fx {
    let fx = seed(t).await;
    let now = OffsetDateTime::now_utc();
    let m = Duration::minutes;
    let (e1, e2, e3) = (fx.e[0], fx.e[1], fx.e[2]);
    // 节点1
    put_events(t, e1, now - m(20), Some("1.1.1.1"), "google.com", 3, false).await;
    put_events(t, e1, now - m(19), Some("1.1.1.2"), "google.com", 2, false).await;
    put_events(t, e1, now - m(18), Some("1.1.1.1"), "a1b2c3d4.example.com", 1, false).await;
    put_events(t, e1, now - m(17), Some("1.1.1.1"), "DEADBEEF12.Example.com.", 1, false).await;
    put_events(t, e1, now - m(16), Some("1.1.1.1"), "550e8400-e29b-41d4-a716-446655440000.example.com", 1, false).await;
    put_events(t, e1, now - m(15), Some("1.1.1.3"), "www.example.com", 1, false).await;
    put_events(t, e1, now - m(14), Some("1.1.1.1"), "ads.tracker.net", 4, true).await;
    put_events(t, e1, now - m(13), Some("1.1.1.4"), "203.0.113.9", 2, false).await;
    put_events(t, e1, now - m(150), Some("1.1.1.5"), "old.example.org", 2, false).await;
    put_events(t, e1, now - Duration::hours(30), Some("1.1.1.6"), "ancient.example.org", 1, false).await;
    put_events(t, e1, now - Duration::days(5), Some("1.1.1.7"), "week.example.org", 1, false).await;
    // 节点2
    put_events(t, e2, now - m(10), Some("2.2.2.2"), "google.com", 2, false).await;
    put_events(t, e2, now - m(9), Some("2.2.2.2"), "abcdef12ab.foo.co.uk", 2, false).await;
    // 节点3
    put_events(t, e3, now - m(5), Some("3.3.3.3"), "solo.example.net", 1, false).await;
    access::rollup_since(&t.st.db, now - Duration::days(6)).await.unwrap();
    fx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collapse_and_kind_rules() {
    let _g = DB_LOCK.lock().await;
    let t = setup().await;
    let cases = [
        ("www.google.com", "www.google.com"),
        ("example.com", "example.com"),
        ("a1b2c3d4.example.com", "*.example.com"),
        ("x.y.a1b2c3d4.example.com", "*.example.com"),
        ("deadbeef.example.com", "deadbeef.example.com"),
        ("abcdefgh.example.com", "abcdefgh.example.com"),
        ("abcdef12.example.com", "*.example.com"),
        ("abcdefghij0123456789abcd.example.com", "*.example.com"),
        ("abcdefghijklmnopqrst.example.com", "abcdefghijklmnopqrst.example.com"),
        ("abcdefghijklmnopqr12.example.com", "abcdefghijklmnopqr12.example.com"),
        ("550e8400-e29b-41d4-a716-446655440000.example.com", "*.example.com"),
        ("abcdef12ab.foo.co.uk", "*.foo.co.uk"),
        ("x.abcdef12ab.co.uk", "x.abcdef12ab.co.uk"),
        ("a.b.c.d.example.com", "a.b.c.d.example.com"),
        ("1.2.3.4", "1.2.3.4"),
        ("2001:db8::1", "2001:db8::1"),
    ];
    for (input, want) in cases {
        let got: String = sqlx::query_scalar("select access_collapse($1)").bind(input).fetch_one(&t.st.db).await.unwrap();
        assert_eq!(got, want, "access_collapse({input})");
    }
    for (input, want) in [("1.2.3.4", "ip"), ("2001:db8::1", "ip"), ("example.com", "domain"), ("*.example.com", "domain")] {
        let got: String = sqlx::query_scalar("select access_kind($1)").bind(input).fetch_one(&t.st.db).await.unwrap();
        assert_eq!(got, want, "access_kind({input})");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn summary_targets_sources_events() {
    let _g = DB_LOCK.lock().await;
    let t = setup().await;
    let (st, _) = t.raw("/api/v1/access/summary", false).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let fx = fixture(&t).await;
    let (n1, n2, n3) = (fx.n[0], fx.n[1], fx.n[2]);

    // ── 24h 汇总（小时表） ──
    let d = t.get("/api/v1/access/summary?window=24h").await;
    assert_eq!(d["kpi"]["active_nodes"], 3);
    assert_eq!(d["kpi"]["conns"], 22);
    assert_eq!(d["kpi"]["targets"], 9);
    assert_eq!(d["kpi"]["blocked"], 4);
    let a = row(&d, "nodes", "name", "团队1-01");
    assert_eq!((a["conns"].as_i64(), a["targets"].as_i64(), a["domains"].as_i64()), (Some(17), Some(6), Some(5)));
    assert_eq!((a["blocked"].as_i64(), a["sources"].as_i64()), (Some(4), Some(5)));
    assert_eq!(a["team"], "团队1");
    assert!(a["last_ts"].is_string());
    assert_eq!(row(&d, "nodes", "name", "团队1-02")["conns"], 4);
    assert_eq!(row(&d, "nodes", "name", "团队2-01")["team"], "团队2");
    assert_eq!(d["nodes"][0]["name"], "团队1-01", "按连接数降序");

    // ── 1h 汇总（原始记录）：不含 150 分钟前的 old.example.org，结果口径一致 ──
    let d = t.get("/api/v1/access/summary?window=1h").await;
    let a = row(&d, "nodes", "name", "团队1-01");
    assert_eq!((a["conns"].as_i64(), a["targets"].as_i64(), a["sources"].as_i64()), (Some(15), Some(5), Some(4)));
    assert_eq!(d["kpi"]["conns"], 20);

    // ── 7d / 30d 窗口；30d 里再塞一条 20 天前的小时汇总 ──
    let d = t.get("/api/v1/access/summary?window=7d").await;
    assert_eq!(row(&d, "nodes", "name", "团队1-01")["conns"], 19);
    sqlx::query("insert into access_hourly (node_id, relay_id, hour, dest_host, kind, conns) values ($1, $2, $3, 'seeded.example.com', 'domain', 7)")
        .bind(n1)
        .bind(fx.relay)
        .bind(fleet_server::db::floor_hour(OffsetDateTime::now_utc() - Duration::days(20)))
        .execute(&t.st.db)
        .await
        .unwrap();
    assert_eq!(row(&t.get("/api/v1/access/summary?window=7d").await, "nodes", "name", "团队1-01")["conns"], 19);
    assert_eq!(row(&t.get("/api/v1/access/summary?window=30d").await, "nodes", "name", "团队1-01")["conns"], 26);

    // ── 筛选 ──
    let d = t.get("/api/v1/access/summary?window=24h&action=blocked").await;
    assert_eq!((d["kpi"]["active_nodes"].as_i64(), d["kpi"]["conns"].as_i64(), d["kpi"]["blocked"].as_i64(), d["kpi"]["targets"].as_i64()), (Some(1), Some(4), Some(4), Some(1)));
    let d = t.get("/api/v1/access/summary?window=24h&action=routed").await;
    assert_eq!(row(&d, "nodes", "name", "团队1-01")["conns"], 13);
    assert_eq!(d["kpi"]["blocked"], 0);
    let d = t.get("/api/v1/access/summary?window=24h&kind=ip").await;
    assert_eq!((d["kpi"]["targets"].as_i64(), d["kpi"]["conns"].as_i64()), (Some(1), Some(2)));
    let d = t.get(&format!("/api/v1/access/summary?window=24h&team={}", fx.team1)).await;
    assert_eq!((d["nodes"].as_array().unwrap().len(), d["kpi"]["conns"].as_i64()), (2, Some(21)));
    let d = t.get(&format!("/api/v1/access/summary?window=24h&team={}", fx.team2)).await;
    assert_eq!(d["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(d["nodes"][0]["node_id"], n3.to_string());
    let d = t.get(&format!("/api/v1/access/summary?window=24h&node={n2}&team=&search=&kind=all&action=all")).await;
    assert_eq!((d["nodes"].as_array().unwrap().len(), d["kpi"]["conns"].as_i64()), (1, Some(4)));
    let d = t.get("/api/v1/access/summary?window=24h&search=GOOGLE").await;
    assert_eq!((d["nodes"].as_array().unwrap().len(), d["kpi"]["conns"].as_i64(), d["kpi"]["targets"].as_i64()), (2, Some(7), Some(2)));
    // 搜索里的 % 和 _ 按字面匹配
    let d = t.get("/api/v1/access/summary?window=24h&search=%25").await;
    assert_eq!(d["kpi"]["conns"], 0);

    // ── 目标列表：随机子域名合并、阻止数、排序、分页 ──
    let d = t.get(&format!("/api/v1/access/targets?window=24h&node={n1}")).await;
    let hs = hosts(&d);
    assert_eq!(hs.len(), 6);
    assert!(hs.contains(&"*.example.com".to_string()) && !hs.iter().any(|h| h.contains("deadbeef") || h.contains("550e8400") || h.contains("a1b2c3d4")), "{hs:?}");
    assert_eq!(hs[0], "google.com");
    assert_eq!(item(&d, "*.example.com", "团队1-01")["conns"], 3);
    assert_eq!(item(&d, "*.example.com", "团队1-01")["sources"], 1);
    let ads = item(&d, "ads.tracker.net", "团队1-01");
    assert_eq!((ads["conns"].as_i64(), ads["blocked"].as_i64(), ads["kind"].as_str()), (Some(4), Some(4), Some("domain")));
    assert!(ads["first_seen"].is_string() && ads["last_seen"].is_string());
    assert_eq!(item(&d, "203.0.113.9", "团队1-01")["kind"], "ip");
    assert!(d["next_offset"].is_null());

    let p1 = t.get(&format!("/api/v1/access/targets?window=24h&node={n1}&limit=4&offset=0")).await;
    let p2 = t.get(&format!("/api/v1/access/targets?window=24h&node={n1}&limit=4&offset=4")).await;
    assert_eq!((p1["items"].as_array().unwrap().len(), p1["next_offset"].as_i64()), (4, Some(4)));
    assert_eq!((p2["items"].as_array().unwrap().len(), p2["next_offset"].is_null()), (2, true));
    let mut all = hosts(&p1);
    all.extend(hosts(&p2));
    assert_eq!(all, hs, "分页拼起来和整页一致");
    // 排序
    let d = t.get(&format!("/api/v1/access/targets?window=24h&node={n1}&sort=first")).await;
    assert_eq!(hosts(&d).last().unwrap(), "old.example.org", "首次出现最早的在最后");
    let d = t.get(&format!("/api/v1/access/targets?window=24h&node={n1}&sort=last&limit=1")).await;
    assert_eq!(hosts(&d)[0], "203.0.113.9", "最近出现的在最前");
    // limit 上限 500，非法参数 400
    let d = t.get("/api/v1/access/targets?window=24h&limit=100000").await;
    assert!(d["items"].as_array().unwrap().len() <= 500);
    for bad in ["window=2d", "node=abc", "limit=x", "kind=x", "action=x", "sort=x", "team=zzz"] {
        let (s, _) = t.raw(&format!("/api/v1/access/targets?{bad}"), true).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    }

    // 1h 窗口走原始记录，合并规则和来源数口径一致（原始记录里来源数是精确去重）
    let d = t.get(&format!("/api/v1/access/targets?window=1h&node={n1}")).await;
    assert_eq!(d["items"].as_array().unwrap().len(), 5);
    assert_eq!(item(&d, "google.com", "团队1-01")["sources"], 2);
    assert_eq!(item(&d, "*.example.com", "团队1-01")["conns"], 3);
    assert!(!hosts(&d).contains(&"old.example.org".to_string()));
    assert!(item(&d, "google.com", "团队1-01")["first_seen"].is_string());
    let d = t.get("/api/v1/access/targets?window=1h&action=blocked").await;
    assert_eq!((hosts(&d), d["items"][0]["conns"].as_i64()), (vec!["ads.tracker.net".to_string()], Some(4)));
    // 合并后的 *.foo.co.uk
    assert!(hosts(&t.get(&format!("/api/v1/access/targets?window=24h&node={n2}")).await).contains(&"*.foo.co.uk".to_string()));

    // ── 来源 top ──
    let d = t.get(&format!("/api/v1/access/sources?window=24h&node={n1}")).await;
    let items = d["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    assert_eq!((items[0]["src_ip"].as_str(), items[0]["conns"].as_i64()), (Some("1.1.1.1"), Some(10)));
    let d = t.get(&format!("/api/v1/access/sources?window=7d&node={n1}&limit=2")).await;
    assert_eq!(d["items"].as_array().unwrap().len(), 2);
    let d = t.get("/api/v1/access/sources?window=1h&search=2.2.2").await;
    assert_eq!((d["items"].as_array().unwrap().len(), d["items"][0]["conns"].as_i64()), (1, Some(4)));
    assert_eq!(d["items"][0]["name"], "团队1-02");

    // ── 原始记录：倒序分页、过滤 ──
    let d = t.get(&format!("/api/v1/access/events?node={n1}&limit=7")).await;
    assert_eq!(d["items"].as_array().unwrap().len(), 7);
    assert_eq!(d["items"][0]["dest_host"], "203.0.113.9");
    let mut seen = d["items"].as_array().unwrap().len();
    let mut next = d["next_before"].as_str().map(String::from);
    let mut last_ts = ts(&d["items"][6]["ts"]);
    while let Some(b) = next {
        let q = b.replace('+', "%2B").replace('|', "%7C");
        let d = t.get(&format!("/api/v1/access/events?node={n1}&limit=7&before={q}")).await;
        for e in d["items"].as_array().unwrap() {
            assert!(ts(&e["ts"]) <= last_ts, "时间递减");
        }
        seen += d["items"].as_array().unwrap().len();
        if let Some(l) = d["items"].as_array().unwrap().last() {
            last_ts = ts(&l["ts"]);
        }
        next = d["next_before"].as_str().map(String::from);
    }
    assert_eq!(seen, 19, "节点1 七天内 19 条");
    let d = t.get(&format!("/api/v1/access/events?node={n1}&search=203.0.113")).await;
    assert_eq!(d["items"].as_array().unwrap().len(), 2);
    assert_eq!(d["items"][0]["dest_port"], 443);
    assert_eq!(d["items"][0]["network"], "tcp");
    assert_eq!(d["items"][0]["name"], "团队1-01");
    let d = t.get("/api/v1/access/events?search=1.1.1.7").await;
    assert_eq!(d["items"][0]["dest_host"], "week.example.org");
    let d = t.get(&format!("/api/v1/access/events?node={n1}&action=blocked")).await;
    assert_eq!(d["items"].as_array().unwrap().len(), 4);
    assert!(d["items"].as_array().unwrap().iter().all(|e| e["blocked"] == true));
    let d = t.get(&format!("/api/v1/access/events?node={n1}&window=24h")).await;
    assert_eq!(d["items"].as_array().unwrap().len(), 17, "24h 内不含 30 小时前和 5 天前");
    assert_eq!(t.get("/api/v1/access/events?limit=3").await["items"].as_array().unwrap().len(), 3);
    let (s, _) = t.raw("/api/v1/access/events?before=yesterday", true).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // 同一时刻的多条记录：按 (ts, id) 游标翻页，一条不漏、不重复
    let same = OffsetDateTime::now_utc() - Duration::minutes(3);
    for _ in 0..8 {
        sqlx::query("insert into access_event (entry_id, ts, src_ip, dest_host, dest_port, network, blocked) values ($1, $2, '3.3.3.9', 'same.example.net', 443, 'tcp', false)")
            .bind(fx.e[2])
            .bind(same)
            .execute(&t.st.db)
            .await
            .unwrap();
    }
    let (mut got, mut next) = (0, None::<String>);
    loop {
        let q = next.as_deref().map(|b| format!("&before={}", b.replace('+', "%2B").replace('|', "%7C"))).unwrap_or_default();
        let d = t.get(&format!("/api/v1/access/events?node={n3}&limit=3{q}")).await;
        got += d["items"].as_array().unwrap().len();
        next = d["next_before"].as_str().map(String::from);
        if next.is_none() {
            break;
        }
    }
    assert_eq!(got, 9, "8 条同一时刻的 + 原来 1 条");

    // 按线路机筛选（逐入口取再合并）：多个入口同一时刻的记录交错，按 (ts, id) 倒序翻页一条不漏、不重复
    for e in [fx.e[0], fx.e[1], fx.e[0], fx.e[1]] {
        sqlx::query("insert into access_event (entry_id, ts, src_ip, dest_host, dest_port, network, blocked) values ($1, $2, '3.3.3.8', 'mix.example.net', 443, 'tcp', false)")
            .bind(e)
            .bind(same)
            .execute(&t.st.db)
            .await
            .unwrap();
    }
    let mut f = access::Filter::new(access::Window::D7, OffsetDateTime::now_utc());
    f.relay = Some(fx.relay);
    let (mut keys, mut before) = (vec![], None);
    loop {
        let page = access::events(&t.st.db, &f, before, 4).await.unwrap();
        keys.extend(page.iter().map(|e| (e.ts, e.id)));
        if page.len() < 4 {
            break;
        }
        before = page.last().map(|e| (e.ts, e.id));
    }
    let want: Vec<(OffsetDateTime, i64)> = sqlx::query_as(
        "select ts, id from access_event where entry_id in (select id from entry where relay_id = $1) and ts >= $2 order by ts desc, id desc",
    )
    .bind(fx.relay)
    .bind(f.from)
    .fetch_all(&t.st.db)
    .await
    .unwrap();
    assert_eq!(keys, want, "逐页拼起来等于一次按 (ts, id) 倒序取全");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_targets_machine_and_idempotent_rollup() {
    let _g = DB_LOCK.lock().await;
    let t = setup().await;
    let fx = fixture(&t).await;
    let (n1, e1) = (fx.n[0], fx.e[0]);

    // 新出现的目标：24h 不含 30 小时前、5 天前首次出现的
    let d = t.get(&format!("/api/v1/access/new-targets?window=24h&node={n1}")).await;
    let hs = hosts(&d);
    assert_eq!(hs.len(), 6, "{hs:?}");
    assert!(!hs.contains(&"ancient.example.org".to_string()) && !hs.contains(&"week.example.org".to_string()));
    assert_eq!(hs[0], "203.0.113.9", "最晚出现的在最前");
    assert_eq!(item(&d, "google.com", "团队1-01")["total"], 5);
    assert_eq!(item(&d, "*.example.com", "团队1-01")["total"], 3);
    let d7 = t.get(&format!("/api/v1/access/new-targets?window=7d&node={n1}")).await;
    assert_eq!(d7["items"].as_array().unwrap().len(), 8);
    assert!(hosts(&d7).contains(&"week.example.org".to_string()));
    let d = t.get(&format!("/api/v1/access/new-targets?window=7d&node={n1}&kind=ip")).await;
    assert_eq!(hosts(&d), vec!["203.0.113.9".to_string()]);
    let d = t.get("/api/v1/access/new-targets?window=24h&search=solo").await;
    assert_eq!(hosts(&d), vec!["solo.example.net".to_string()]);
    let d = t.get(&format!("/api/v1/access/new-targets?window=7d&node={n1}&limit=3")).await;
    assert_eq!((d["items"].as_array().unwrap().len(), d["next_offset"].as_i64(), d["total"].as_i64()), (3, Some(3), Some(8)));
    let d = t.get(&format!("/api/v1/access/new-targets?window=7d&node={n1}&limit=3&offset=6")).await;
    assert_eq!((d["items"].as_array().unwrap().len(), d["next_offset"].is_null(), d["total"].as_i64()), (2, true, Some(8)), "最后一页不满，不给 next_offset");
    let d = t.get(&format!("/api/v1/access/new-targets?window=7d&node={n1}&kind=ip")).await;
    assert_eq!(d["total"].as_i64(), Some(1), "总数跟着筛选走");

    // 重复汇总不改变结果（含 target_seen.total）
    let before = t.get("/api/v1/access/summary?window=7d").await;
    access::rollup_recent(&t.st.db).await.unwrap();
    access::rollup_since(&t.st.db, OffsetDateTime::now_utc() - Duration::days(6)).await.unwrap();
    assert_eq!(t.get("/api/v1/access/summary?window=7d").await["kpi"], before["kpi"]);
    let d = t.get(&format!("/api/v1/access/new-targets?window=24h&node={n1}")).await;
    assert_eq!(item(&d, "google.com", "团队1-01")["total"], 5);

    // 增量：新来一条，重算后只加 1，first_seen 不变
    let first = item(&d, "google.com", "团队1-01")["first_seen"].clone();
    put_events(&t, e1, OffsetDateTime::now_utc() - Duration::seconds(30), Some("1.1.1.9"), "google.com", 1, false).await;
    access::rollup_recent(&t.st.db).await.unwrap();
    access::rollup_recent(&t.st.db).await.unwrap();
    let d = t.get(&format!("/api/v1/access/new-targets?window=24h&node={n1}")).await;
    assert_eq!(item(&d, "google.com", "团队1-01")["total"], 6);
    assert_eq!(item(&d, "google.com", "团队1-01")["first_seen"], first);
    let d = t.get(&format!("/api/v1/access/targets?window=24h&node={n1}")).await;
    assert_eq!(item(&d, "google.com", "团队1-01")["conns"], 6);

    // 单节点：top 域名、top 来源、最近记录、小时序列
    let d = t.get(&format!("/api/v1/machines/{n1}/access?window=24h")).await;
    assert_eq!(d["top_domains"][0]["dest_host"], "google.com");
    assert_eq!(d["top_domains"][0]["conns"], 6);
    assert_eq!(d["top_domains"].as_array().unwrap().len(), 6);
    assert_eq!(d["top_sources"][0]["src_ip"], "1.1.1.1");
    assert_eq!(d["recent"].as_array().unwrap().len(), 20);
    assert_eq!(d["recent"][0]["dest_host"], "google.com", "最近一条是刚塞的");
    let hourly = d["hourly"].as_array().unwrap();
    assert_eq!(hourly.len(), 25, "从 24 小时前的整点到当前整点");
    assert_eq!(hourly.iter().map(|h| h["conns"].as_i64().unwrap()).sum::<i64>(), 18);
    assert_eq!(hourly.iter().map(|h| h["blocked"].as_i64().unwrap()).sum::<i64>(), 4);
    let d = t.get(&format!("/api/v1/machines/{n1}/access?window=1h")).await;
    assert_eq!(d["top_domains"][0]["conns"], 6);
    assert!(d["hourly"].as_array().unwrap().len() >= 24, "1h 窗口的序列也给 24 小时");
    // 线路机：汇总它名下所有节点
    let d = t.get(&format!("/api/v1/machines/{}/access?window=24h", fx.relay)).await;
    assert_eq!(d["top_domains"][0]["dest_host"], "google.com");
    assert_eq!(d["top_domains"][0]["conns"], 8);
    // 节点1 后来也经另一台线路机走过流量（入口已移除）：只算到那台线路机和节点1 上，不算到原线路机
    let relay2 = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle) values ($1, 'relay', '线路机3', '198.51.100.3', 'active')").bind(relay2).execute(&t.st.db).await.unwrap();
    let e_via2 = Uuid::now_v7();
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, 21000, $4, 'cd', 'removed')")
        .bind(e_via2)
        .bind(relay2)
        .bind(n1)
        .bind(Uuid::now_v7())
        .execute(&t.st.db)
        .await
        .unwrap();
    put_events(&t, e_via2, OffsetDateTime::now_utc() - Duration::minutes(2), Some("9.9.9.9"), "via2.example.com", 3, false).await;
    access::rollup_recent(&t.st.db).await.unwrap();
    for w in ["1h", "24h"] {
        let d = t.get(&format!("/api/v1/machines/{}/access?window={w}", fx.relay)).await;
        assert!(!d["top_domains"].to_string().contains("via2"), "{w}：{d}");
        assert!(!d["top_sources"].to_string().contains("9.9.9.9"), "{w}：{d}");
        assert!(!d["recent"].to_string().contains("via2"), "{w}：{d}");
        let d = t.get(&format!("/api/v1/machines/{relay2}/access?window={w}")).await;
        assert_eq!(d["top_domains"].as_array().unwrap().len(), 1, "{w}：{d}");
        assert_eq!((d["top_domains"][0]["dest_host"].as_str(), d["top_domains"][0]["conns"].as_i64()), (Some("via2.example.com"), Some(3)), "{w}");
        assert_eq!(d["top_sources"].as_array().unwrap().len(), 1, "{w}：{d}");
        assert_eq!(d["top_sources"][0]["src_ip"], "9.9.9.9");
        assert_eq!(d["recent"].as_array().unwrap().len(), 3);
        assert!(t.get(&format!("/api/v1/machines/{n1}/access?window={w}")).await["top_domains"].to_string().contains("via2.example.com"));
    }
    let d = t.get(&format!("/api/v1/machines/{relay2}/access?window=24h")).await;
    assert_eq!(d["hourly"].as_array().unwrap().iter().map(|h| h["conns"].as_i64().unwrap()).sum::<i64>(), 3);
    let (s, _) = t.raw(&format!("/api/v1/machines/{}/access", Uuid::now_v7()), true).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // 没有任何记录的节点：全是空的，不报错
    let d = t.get(&format!("/api/v1/machines/{}/access?window=7d", fx.n[2])).await;
    assert_eq!(d["top_domains"].as_array().unwrap().len(), 1);

    // 清理：90 天前的汇总被删
    sqlx::query("insert into access_hourly (node_id, relay_id, hour, dest_host, kind, conns) values ($1, $2, now() - interval '100 days', 'gone.example.com', 'domain', 1)")
        .bind(n1)
        .bind(fx.relay)
        .execute(&t.st.db)
        .await
        .unwrap();
    access::prune(&t.st.db).await.unwrap();
    let n: i64 = sqlx::query_scalar("select count(*) from access_hourly where dest_host = 'gone.example.com'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 0);
}
