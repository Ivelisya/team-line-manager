//! IP 情报：库缺失时接口降级不报 500；构造 access_source_hourly + 预置 ip_intel_cache 后，
//! 节点来源画像分布、学习期后的新运营商 / 新城市 / 机房来源告警、alerts 循环不误恢复、24 小时后自动恢复。
//! 需要 PostgreSQL（见 common_ipi/mod.rs）。

mod common_ipi;

use axum::http::StatusCode;
use common_ipi::{seed, setup};
use fleet_server::alerts::{self, AlertState};
use fleet_server::ipintel;
use serde_json::Value;
use time::{Duration, OffsetDateTime};

/// 几个测试共用一个库，串行跑
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn find<'a>(list: &'a Value, field: &str, val: &str) -> &'a Value {
    list.as_array().unwrap().iter().find(|x| x[field] == val).unwrap_or_else(|| panic!("没有 {field}={val}：{list}"))
}

fn rules(inc: &[(String, String, String)], state: &str) -> Vec<String> {
    inc.iter().filter(|i| i.1 == state).map(|i| i.0.clone()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn degrades_without_dbs_never_500() {
    let _g = DB_LOCK.lock().await;
    let t = setup(false).await;
    let fx = seed(&t).await;
    let now = OffsetDateTime::now_utc();
    t.put_hourly(fx.n[0], now - Duration::hours(3), "202.102.224.68", 5).await;

    // 状态：一个库都没有，说明缺什么、怎么装
    let s = t.get("/api/v1/ipintel/status").await;
    assert_eq!(s["enabled"], true);
    assert_eq!(s["installed"], false);
    assert_eq!((s["loaded"].as_u64(), s["expected"].as_u64()), (Some(0), Some(5)));
    assert_eq!(s["errors"].as_array().unwrap().len(), 5);
    assert_eq!(s["files"].as_array().unwrap().len(), 5);
    assert!(s["hint"].as_str().unwrap().contains("lab/install-ipintel.sh"), "{s}");
    assert_eq!(s["lists"]["cn_mixed"], 25);

    // 单个 / 批量：200，字段查不到
    let one = t.get("/api/v1/ipintel/8.8.8.8").await;
    assert_eq!(one["degraded"], true);
    assert_eq!(one["type"], "unknown");
    assert!(one["asn"].is_null());
    let b = t.get("/api/v1/ipintel?ips=8.8.8.8,not-an-ip,2001:db8::1").await;
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert!(items[0]["intel"].is_null() && items[0]["error"].is_null());
    assert!(items[1]["error"].as_str().unwrap().contains("不是有效"));
    assert_eq!(b["degraded"], true);
    // 边界
    assert_eq!(t.raw("/api/v1/ipintel/not-an-ip").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.raw("/api/v1/ipintel").await.0, StatusCode::BAD_REQUEST);
    let many = (0..201).map(|i| format!("10.0.{}.{}", i / 250, i % 250 + 1)).collect::<Vec<_>>().join(",");
    assert_eq!(t.raw(&format!("/api/v1/ipintel?ips={many}")).await.0, StatusCode::BAD_REQUEST);
    let ok200 = (0..200).map(|i| format!("10.0.0.{}", i % 250 + 1)).collect::<Vec<_>>().join(",");
    assert_eq!(t.raw(&format!("/api/v1/ipintel?ips={ok200}")).await.0, StatusCode::OK);

    // 节点来源画像：合法的降级结果（有来源，但没有画像）
    let p = t.get(&format!("/api/v1/machines/{}/sources?window=7d", fx.n[0])).await;
    assert_eq!(p["degraded"], true);
    assert_eq!(p["empty"], false);
    assert_eq!(p["unresolved_sources"], 1);
    assert_eq!(p["total_conns"], 0);
    assert!(p["distribution"]["isp"].as_array().unwrap().is_empty());
    assert_eq!(p["top_sources"][0]["src_ip"], "202.102.224.68");
    assert!(p["top_sources"][0]["intel"].is_null());
    assert_eq!(p["learning"]["done"], false);
    // 同参数再取一次走短 TTL 缓存，结果一致；limit 不同是另一份缓存
    assert_eq!(t.get(&format!("/api/v1/machines/{}/sources?window=7d", fx.n[0])).await, p);
    assert_eq!(t.get(&format!("/api/v1/machines/{}/sources?window=7d&limit=1", fx.n[0])).await["top_sources"].as_array().unwrap().len(), 1);
    assert_eq!(t.get(&format!("/api/v1/machines/{}/sources", fx.n[1])).await["empty"], true);
    assert_eq!(t.raw(&format!("/api/v1/machines/{}/sources?window=1h", fx.n[0])).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.raw(&format!("/api/v1/machines/{}/sources?limit=abc", fx.n[0])).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.raw(&format!("/api/v1/machines/{}/sources", uuid::Uuid::now_v7())).await.0, StatusCode::NOT_FOUND);

    // 访问监控的来源条目：intel 为 null，其余字段不变
    let a = t.get("/api/v1/access/sources?window=7d").await;
    let it = &a["items"][0];
    assert_eq!(it["src_ip"], "202.102.224.68");
    assert!(it["intel"].is_null());
    assert_eq!(it["conns"], 5);
    let m = t.get(&format!("/api/v1/machines/{}/access?window=7d", fx.n[0])).await;
    assert!(m["top_sources"][0]["intel"].is_null());

    // 没有库：来源汇总不推进、不产生异常，也不报错
    let r = ipintel::maintain_once(&t.st, now).await.unwrap();
    assert!(r.skipped_no_db);
    assert_eq!(r.pairs, 0);
    let wm: i64 = sqlx::query_scalar("select count(*) from ipintel_state").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(wm, 0, "没库时不推进水位");

    // 缓存按库版本失效：版本不对的行不命中（也没有库可以重算）
    t.put_cache_v("9.9.9.9", "旧版本的结果", "US", None, None, "datacenter", "old-version").await;
    let b = t.get("/api/v1/ipintel?ips=9.9.9.9").await;
    assert!(b["items"][0]["intel"].is_null(), "{b}");
    t.put_cache("9.9.9.9", "当前版本", "US", None, None, "datacenter").await;
    let b = t.get("/api/v1/ipintel?ips=9.9.9.9").await;
    assert_eq!(b["items"][0]["intel"]["isp"], "当前版本");
    assert_eq!(b["items"][0]["intel"]["conn_label"], "机房");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn profile_anomalies_and_alert_lifecycle() {
    let _g = DB_LOCK.lock().await;
    let t = setup(true).await;
    assert_eq!(t.st.ipintel.intel.loaded(), 1);
    let fx = seed(&t).await;
    let now = OffsetDateTime::now_utc();
    let (d, h) = (Duration::days, Duration::hours);

    // 预置画像（不依赖真实库）
    t.put_cache("202.102.224.68", "中国联通", "CN", Some("河南省"), Some("郑州市"), "residential").await;
    t.put_cache("202.96.128.86", "中国联通", "CN", Some("广东省"), Some("广州市"), "residential").await;
    t.put_cache("111.1.1.1", "中国移动", "CN", Some("北京市"), Some("北京市"), "residential").await;
    t.put_cache("8.8.8.8", "Google", "US", Some("California"), Some("Mountain View"), "datacenter").await;
    t.put_cache("223.5.5.5", "阿里", "CN", Some("浙江省"), Some("杭州市"), "datacenter").await;
    t.put_cache("8.8.4.4", "Google", "US", Some("California"), Some("Mountain View"), "datacenter").await;
    t.put_cache("192.0.2.77", "测试网", "CN", None, None, "unknown").await;

    // 节点1：第一条记录在 10 天前（学习期 7 天，到 3 天前结束）
    let n0 = fx.n[0];
    t.put_hourly(n0, now - d(10), "202.102.224.68", 100).await; // 郑州联通
    t.put_hourly(n0, now - d(9), "8.8.8.8", 10).await; // 学习期内的机房来源：只学习
    t.put_hourly(n0, now - d(8), "202.102.224.68", 50).await;
    t.put_hourly(n0, now - h(2), "111.1.1.1", 7).await; // 新运营商 + 新城市
    t.put_hourly(n0, now - h(2), "202.96.128.86", 20).await; // 老运营商、新城市
    t.put_hourly(n0, now - h(2), "223.5.5.5", 3).await; // 新运营商（阿里）+ 机房来源 + 新城市
    // 我方线路机的出口 IP：自动算内部地址，不进画像、不触发异常
    sqlx::query("update machine set primary_ip = '8.8.4.4' where id = $1").bind(fx.relay).execute(&t.st.db).await.unwrap();
    t.put_hourly(n0, now - h(2), "8.8.4.4", 5).await;
    // 节点2：一直是郑州联通，最近多了一个新 IP（不是机房）→ 没有异常
    let n1 = fx.n[1];
    t.put_hourly(n1, now - d(10), "202.102.224.68", 40).await;
    t.put_hourly(n1, now - h(3), "202.102.224.68", 5).await;
    // 节点3：第一条记录才一天前，仍在学习期：出现什么都不报
    let n2 = fx.n[2];
    t.put_hourly(n2, now - d(1), "111.1.1.1", 9).await;
    t.put_hourly(n2, now - d(1), "8.8.8.8", 1).await;
    // 非公网来源也不进画像
    t.put_hourly(n2, now - h(5), "10.1.2.3", 4).await;

    // ── 来源汇总 + 异常 ──
    let r = ipintel::maintain_once(&t.st, now).await.unwrap();
    assert!(!r.skipped_no_db);
    assert_eq!(r.opened, 3, "{r:?}");
    let inc = t.incidents(n0).await;
    assert_eq!(rules(&inc, "firing"), vec!["ipintel_datacenter_source", "ipintel_new_city", "ipintel_new_isp"], "{inc:?}");
    let by_rule = |r: &str| inc.iter().find(|i| i.0 == r).unwrap().2.clone();
    assert!(by_rule("ipintel_new_isp").starts_with("团队1-01：出现新的来源运营商："), "{inc:?}");
    assert!(by_rule("ipintel_new_isp").contains("中国移动") && by_rule("ipintel_new_isp").contains("阿里"));
    assert!(!by_rule("ipintel_new_isp").contains("中国联通"), "老运营商不报");
    assert!(by_rule("ipintel_new_city").contains("广东省广州市") && by_rule("ipintel_new_city").contains("北京市"));
    assert!(by_rule("ipintel_datacenter_source").contains("223.5.5.5") && by_rule("ipintel_datacenter_source").contains("机房"));
    assert!(!by_rule("ipintel_datacenter_source").contains("8.8.8.8"), "学习期内的机房来源不报");
    assert!(!by_rule("ipintel_datacenter_source").contains("8.8.4.4"), "内部地址不报");
    assert!(t.incidents(n1).await.is_empty() && t.incidents(n2).await.is_empty());
    let sev: Vec<String> = sqlx::query_scalar("select severity::text from incident where rule like 'ipintel!_%' escape '!'").fetch_all(&t.st.db).await.unwrap();
    assert!(sev.len() == 3 && sev.iter().all(|s| s == "warning"));

    // 重复执行是幂等的：不重复开告警，连接数不重复累加
    let r2 = ipintel::maintain_once(&t.st, now).await.unwrap();
    assert_eq!((r2.opened, r2.resolved), (0, 0));
    let conns: i64 = sqlx::query_scalar("select conns from node_source_seen where node_id = $1 and dim = 'isp' and value = '中国联通'").bind(n0).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(conns, 100 + 50 + 20);

    // ── 画像分布 ──
    let p = t.get(&format!("/api/v1/machines/{n0}/sources?window=30d")).await;
    assert_eq!((p["degraded"].as_bool(), p["empty"].as_bool()), (Some(false), Some(false)));
    assert_eq!((p["total_conns"].as_i64(), p["total_sources"].as_i64(), p["internal_conns"].as_i64()), (Some(190), Some(5), Some(5)));
    let isp = &p["distribution"]["isp"];
    assert_eq!(isp[0]["value"], "中国联通");
    assert_eq!((isp[0]["conns"].as_i64(), isp[0]["pct"].as_f64()), (Some(170), Some(89.5)));
    assert_eq!(find(isp, "value", "Google")["conns"], 10);
    assert_eq!(isp.as_array().unwrap().len(), 4, "联通 / Google / 中国移动 / 阿里：{isp}");
    let ct = &p["distribution"]["conn_type"];
    let res = find(ct, "value", "residential");
    assert_eq!((res["conns"].as_i64(), res["pct"].as_f64()), (Some(177), Some(93.2)));
    assert_eq!(find(ct, "value", "datacenter")["label"], "机房");
    let city = &p["distribution"]["city"];
    assert_eq!(find(city, "label", "河南省郑州市")["conns"], 150);
    assert_eq!(find(city, "label", "US · Mountain View")["conns"], 10);
    assert_eq!(p["distribution"]["province"][0]["label"], "河南省");
    assert_eq!(find(&p["distribution"]["country"], "value", "CN")["conns"], 180);
    assert_eq!(p["top_sources"][0]["src_ip"], "202.102.224.68");
    assert_eq!(p["top_sources"][0]["intel"]["place"], "河南省郑州市");
    let internal_top = find(&p["top_sources"], "src_ip", "8.8.4.4");
    assert_eq!(internal_top["intel"]["internal"], "线路机2");
    // 基线与学习期
    assert_eq!(find(&p["known"]["isp"], "value", "中国移动")["anomaly"], true);
    assert_eq!(find(&p["known"]["isp"], "value", "中国联通")["anomaly"], false);
    assert_eq!(p["learning"]["done"], true);
    assert_eq!(p["learning"]["learn_days"], 7);
    assert_eq!(p["anomalies"].as_array().unwrap().len(), 3);
    assert!(p["anomalies"][0]["rule_name"].as_str().is_some());
    // 7 天窗口只看最近的
    let p7 = t.get(&format!("/api/v1/machines/{n0}/sources?window=7d&limit=2")).await;
    assert_eq!(p7["total_conns"], 30);
    assert_eq!(p7["top_sources"].as_array().unwrap().len(), 2);
    // 学习期内的节点
    let p2 = t.get(&format!("/api/v1/machines/{n2}/sources?window=7d")).await;
    assert_eq!(p2["learning"]["done"], false);
    assert!(p2["learning"]["remaining_hours"].as_i64().unwrap() > 24);
    assert_eq!(p2["private_conns"], 4);
    assert_eq!(p2["total_conns"], 10);
    // 线路机：汇总它名下所有节点
    let pr = t.get(&format!("/api/v1/machines/{}/sources?window=30d", fx.relay)).await;
    assert_eq!(pr["total_conns"], 190 + 45 + 10);
    // 访问监控的来源条目带画像
    let a = t.get("/api/v1/access/sources?window=7d&limit=20").await;
    let g = a["items"].as_array().unwrap().iter().find(|i| i["src_ip"] == "223.5.5.5").unwrap();
    assert_eq!((g["intel"]["isp"].as_str(), g["intel"]["conn_type"].as_str(), g["intel"]["city"].as_str()), (Some("阿里"), Some("datacenter"), Some("杭州市")));
    let ma = t.get(&format!("/api/v1/machines/{n0}/access?window=7d")).await;
    assert_eq!(find(&ma["top_sources"], "src_ip", "111.1.1.1")["intel"]["isp"], "中国移动");

    // ── alerts 循环不会把 ipintel 告警当成「原因消失」恢复掉 ──
    let mut state = AlertState::default();
    for i in 0..4 {
        alerts::evaluate_once(&t.st, &mut state, now + Duration::seconds(31 * i)).await.unwrap();
    }
    let inc = t.incidents(n0).await;
    assert_eq!(rules(&inc, "firing").len(), 3, "alerts 循环误恢复了 ipintel 告警：{inc:?}");

    // ── 一次性异常：最后一次出现后 24 小时自动恢复，且之后不再重开 ──
    let r = ipintel::maintain_once(&t.st, now + h(20)).await.unwrap();
    assert_eq!(r.resolved, 0, "距最近一次出现不到 24 小时：{r:?}");
    assert_eq!(rules(&t.incidents(n0).await, "firing").len(), 3);
    let r = ipintel::maintain_once(&t.st, now + h(25)).await.unwrap();
    assert_eq!(r.resolved, 3, "{r:?}");
    let inc = t.incidents(n0).await;
    assert_eq!((rules(&inc, "firing").len(), rules(&inc, "resolved").len()), (0, 3), "{inc:?}");
    let closed: Vec<Value> = sqlx::query_scalar("select detail from incident where rule like 'ipintel!_%' escape '!'").fetch_all(&t.st.db).await.unwrap();
    assert!(closed.iter().all(|d| d["closed_by"] == "ipintel_ttl"));
    let r = ipintel::maintain_once(&t.st, now + h(26)).await.unwrap();
    assert_eq!((r.opened, r.resolved), (0, 0), "已结束的值再运行不再告警");
    // 恢复之后 alerts 循环依旧不碰它们
    alerts::evaluate_once(&t.st, &mut state, now + h(26)).await.unwrap();
    assert_eq!(t.incidents(n0).await.len(), 3);
    // 基线里这些值的标记变成 done
    let open: i64 = sqlx::query_scalar("select count(*) from node_source_seen where flag = 'open'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(open, 0);
}

/// 首次扫描分页（40 天前的数据要被扫到）；水位之前的小时被补算后，差值只累加一次；水位之后的差值丢弃（扫描本来就会覆盖）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_hours_are_added_once_and_scan_is_paged() {
    let _g = DB_LOCK.lock().await;
    let t = setup(true).await;
    let fx = seed(&t).await;
    let now = OffsetDateTime::now_utc();
    let n0 = fx.n[0];
    t.put_cache("202.102.224.68", "中国联通", "CN", Some("河南省"), Some("郑州市"), "residential").await;
    t.put_hourly(n0, now - Duration::days(40), "202.102.224.68", 20).await;
    t.put_hourly(n0, now - Duration::days(3), "202.102.224.68", 100).await;
    let conns = || async { sqlx::query_scalar::<_, i64>("select conns from node_source_seen where node_id = $1 and dim = 'isp' and value = '中国联通'").bind(n0).fetch_one(&t.st.db).await.unwrap() };

    ipintel::maintain_once(&t.st, now).await.unwrap();
    assert_eq!(conns().await, 120, "40 天前和 3 天前的都累加了");

    // 3 天前那个小时被补算，多了 30 条；当前小时的差值（水位之后）不该再加
    let late = |hour: OffsetDateTime, delta: i32| {
        sqlx::query("insert into access_source_late (node_id, hour, src_ip, delta) values ($1, $2, '202.102.224.68'::inet, $3)")
            .bind(n0)
            .bind(fleet_server::db::floor_hour(hour))
            .bind(delta)
            .execute(&t.st.db)
    };
    late(now - Duration::days(3), 30).await.unwrap();
    late(now, 5).await.unwrap();
    ipintel::maintain_once(&t.st, now).await.unwrap();
    assert_eq!(conns().await, 150);
    let left: i64 = sqlx::query_scalar("select count(*) from access_source_late").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(left, 0, "处理过的差值（含丢弃的）已清掉");
    ipintel::maintain_once(&t.st, now).await.unwrap();
    assert_eq!(conns().await, 150, "重复执行不重复累加");
}
