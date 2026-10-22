//! 访问策略集成测试：草稿 → 预览 → 发布 → 期望状态 → agent 回报 → 版本状态 → 回滚；冲突与校验。
//! 需要 PostgreSQL（见 common）。只跑这一套：`cargo test -p fleet-server --test policy`。

mod common;
mod common_pol;

use axum::http::StatusCode;
use common_pol::{put_hourly, seed, setup};
use fleet_proto::PolicyMode;
use fleet_server::state::Event;
use serde_json::{json, Value};

fn ids(v: &Value, key: &str) -> Vec<String> {
    v.as_array().unwrap().iter().map(|x| x[key].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn draft_preview_publish_apply_rollback() {
    let t = setup().await;
    let fx = seed(&t).await;
    let mut events = t.st.events.subscribe();

    // 初始：没有任何策略、没有草稿
    let list = t.get("/api/v1/policies").await;
    assert_eq!(list["draft_hash"], Value::Null);
    assert_eq!(list["draft_count"], 0);
    let teams = list["teams"].as_array().unwrap();
    assert_eq!(ids(&list["teams"], "name"), vec!["团队1", "团队2"]);
    assert_eq!(teams[0]["policy"]["revision"], 0);
    assert_eq!(teams[0]["policy"]["mode"], "observe");
    let nodes = teams[0]["nodes"].as_array().unwrap();
    assert_eq!(ids(&teams[0]["nodes"], "name"), vec!["团队1-01", "团队1-02"]);
    assert_eq!(nodes[0]["policy"]["mode"], "inherit");
    assert_eq!(nodes[0]["effective"]["mode"], "observe");
    assert_eq!(nodes[0]["effective"]["source"], "default");

    // 草稿：团队1 白名单（字符串形式，含中文逗号、大写、精确匹配）；团队1-02 自己黑名单（数组形式）
    let r = t
        .put_policy("team", fx.team1, json!({"mode": "allowlist", "domains": "Example.com，=exact.org\n", "note": "只放行业务域名", "expected_revision": 0}))
        .await;
    assert_eq!(r["policy"]["revision"], 1);
    assert_eq!(r["policy"]["mode"], "observe", "生效值不变，草稿单独存");
    assert_eq!(r["policy"]["draft"]["mode"], "allowlist");
    assert_eq!(r["policy"]["draft"]["domains"], json!(["=exact.org", "example.com"]));
    assert!(matches!(events.try_recv(), Ok(Event::Policies)));
    t.put_policy("node", fx.n[1], json!({"mode": "denylist", "domains": ["bad.com"], "note": "", "expected_revision": 0})).await;

    // 此时下发的期望状态完全不变
    let g0 = t.relay_generation(fx.relay).await;
    assert!(t.agent_desired(&fx.token).await.entries.iter().all(|e| e.policy.is_none()));

    // 访问汇总（最近 24 小时内）：团队1-01 白名单只放行 example.com / exact.org
    for (host, c, b) in [("example.com", 10, 0), ("a.example.com", 5, 0), ("other.com", 7, 0), ("1.2.3.4", 3, 0), ("www.exact.org", 2, 0)] {
        put_hourly(&t, &fx, fx.n[0], 2, host, c, b).await;
    }
    for (host, c, b) in [("bad.com", 4, 0), ("x.bad.com", 6, 0), ("good.com", 20, 0)] {
        put_hourly(&t, &fx, fx.n[1], 3, host, c, b).await;
    }
    put_hourly(&t, &fx, fx.n[2], 3, "whatever.com", 100, 0).await;
    // 25 小时前的不算
    put_hourly(&t, &fx, fx.n[0], 25, "old.com", 999, 0).await;

    let pv = t.post("/api/v1/policies/preview", json!({})).await;
    let hash = pv["draft_hash"].as_str().unwrap().to_string();
    assert_eq!(pv["draft_count"], 2);
    assert_eq!(pv["window_hours"], 24);
    assert_eq!(pv["affected_relays"], 1);
    assert_eq!(pv["changes"].as_array().unwrap().len(), 2);
    let pn = pv["nodes"].as_array().unwrap();
    // 团队1-01 继承团队 allowlist：other.com 7 + IP 3 + www.exact.org 2（= 精确匹配不含子域）= 12
    let n1 = pn.iter().find(|n| n["name"] == "团队1-01").unwrap();
    assert_eq!(n1["mode"], "allowlist");
    assert_eq!(n1["source"], "team");
    assert_eq!(n1["total_conns"], 27);
    assert_eq!(n1["would_block"], 12);
    assert_eq!(n1["domains"], json!([{"host": "other.com", "conns": 7}, {"host": "1.2.3.4", "conns": 3}, {"host": "www.exact.org", "conns": 2}]));
    // 团队1-02 自己的黑名单：bad.com 4 + x.bad.com 6
    let n2 = pn.iter().find(|n| n["name"] == "团队1-02").unwrap();
    assert_eq!((n2["mode"].as_str(), n2["source"].as_str()), (Some("denylist"), Some("node")));
    assert_eq!(n2["would_block"], 10);
    assert_eq!(n2["domains"][0], json!({"host": "x.bad.com", "conns": 6}));
    // 团队2 没有策略，不出现在预览里
    assert!(pn.iter().all(|n| n["name"] != "团队2-01"));
    // 预览之后草稿又被改了：旧的 draft_hash 不能再发布
    let changed = t
        .put_policy("team", fx.team2, json!({"mode": "blocked", "domains": [], "note": "暂停", "expected_revision": 0}))
        .await;
    assert_ne!(changed["draft_hash"].as_str().unwrap(), hash);
    let m = t.fail("POST", "/api/v1/policies/publish", Some(json!({"draft_hash": hash})), StatusCode::CONFLICT).await;
    assert!(m.contains("已变化"), "{m}");
    let m = t.fail("POST", "/api/v1/policies/discard", Some(json!({"draft_hash": hash})), StatusCode::CONFLICT).await;
    assert!(m.contains("已变化"), "{m}");
    // 重新预览：3 份草稿；团队2 暂停 → 团队2-01 全部阻止
    let pv = t.post("/api/v1/policies/preview", json!({})).await;
    assert_eq!(pv["draft_count"], 3);
    let n3 = pv["nodes"].as_array().unwrap().iter().find(|n| n["name"] == "团队2-01").unwrap().clone();
    assert_eq!((n3["mode"].as_str(), n3["would_block"].as_i64()), (Some("blocked"), Some(100)));
    let hash = pv["draft_hash"].as_str().unwrap().to_string();

    // 发布
    let v1 = t.post("/api/v1/policies/publish", json!({"draft_hash": hash})).await;
    assert_eq!(v1["seq"], 1);
    assert_eq!(v1["kind"], "publish");
    assert_eq!(v1["status"], "applying");
    assert_eq!(v1["relays"].as_array().unwrap().len(), 1, "三份草稿落在同一台线路机上，只 +1 一次");
    assert_eq!(t.relay_generation(fx.relay).await, g0 + 1);
    assert_eq!(v1["relays"][0]["generation"], g0 + 1);
    assert!(matches!(events.try_recv(), Ok(Event::Policies)));
    // 草稿清空，生效值更新，revision 递增
    let list = t.get("/api/v1/policies").await;
    assert_eq!(list["draft_count"], 0);
    assert_eq!(list["draft_hash"], Value::Null);
    assert_eq!(list["teams"][0]["policy"]["mode"], "allowlist");
    assert_eq!(list["teams"][0]["policy"]["draft"], Value::Null);
    assert_eq!(list["teams"][0]["policy"]["revision"], 2);
    assert_eq!(list["teams"][0]["nodes"][0]["effective"]["source"], "team");
    assert_eq!(list["teams"][0]["nodes"][1]["effective"]["mode"], "denylist");
    // 没有草稿时再发布 / 丢弃
    t.fail("POST", "/api/v1/policies/publish", Some(json!({"draft_hash": hash})), StatusCode::CONFLICT).await;
    t.fail("POST", "/api/v1/policies/discard", Some(json!({"draft_hash": hash})), StatusCode::CONFLICT).await;

    // 期望状态带上了策略
    let ds = t.agent_desired(&fx.token).await;
    assert_eq!(ds.generation as i64, g0 + 1);
    let p0 = ds.entries[0].policy.as_ref().unwrap();
    assert_eq!(p0.mode, PolicyMode::Allowlist);
    assert_eq!(p0.domains, vec!["full:exact.org", "domain:example.com"]);
    let p1 = ds.entries[1].policy.as_ref().unwrap();
    assert_eq!((p1.mode, p1.domains.clone()), (PolicyMode::Denylist, vec!["domain:bad.com".to_string()]));
    let p2 = ds.entries[2].policy.as_ref().unwrap();
    assert_eq!((p2.mode, p2.domains.len()), (PolicyMode::Blocked, 0));
    // 线上 JSON 形状
    let raw = t.req("GET", "/agent/v1/desired?after=0", None, Some(&fx.token)).await.1;
    assert_eq!(raw["entries"][0]["policy"], json!({"mode": "allowlist", "domains": ["full:exact.org", "domain:example.com"]}));

    // 节点健康：团队2-01 暂停了新连接，给一条 info 提示，但不算故障
    let m = t.get(&format!("/api/v1/machines/{}", fx.n[2])).await;
    let keys: Vec<&str> = m["health"]["reasons"].as_array().unwrap().iter().map(|r| r["key"].as_str().unwrap()).collect();
    assert!(keys.contains(&"policy_blocked"), "{keys:?}");
    let r = m["health"]["reasons"].as_array().unwrap().iter().find(|r| r["key"] == "policy_blocked").unwrap();
    assert_eq!((r["severity"].as_str(), r["rule_name"].as_str()), (Some("info"), Some("访问策略暂停")));
    assert!(r["text"].as_str().unwrap().contains("已暂停新连接"));

    // 线路机还没回报：版本进行中；回报失败 → failed；再回报成功 → applied
    let vs = t.get("/api/v1/policies/versions").await;
    assert_eq!(vs[0]["status"], "applying");
    assert_eq!(vs[0]["relays"][0]["status"], "applying");
    t.agent_applied(&fx.token, ds.generation, false, &ds).await;
    let vs = t.get("/api/v1/policies/versions").await;
    assert_eq!(vs[0]["status"], "failed");
    assert!(vs[0]["relays"][0]["error"].as_str().unwrap().contains("模拟"));
    t.agent_applied(&fx.token, ds.generation, true, &ds).await;
    let vs = t.get("/api/v1/policies/versions").await;
    assert_eq!(vs[0]["status"], "applied");
    assert_eq!(vs[0]["relays"][0]["status"], "applied");
    assert_eq!(vs[0]["is_current"], true);
    assert_eq!(vs[0]["rollbackable"], false);

    // 第二个版本：团队2 恢复 observe，团队1-02 跟随团队（inherit）
    t.put_policy("team", fx.team2, json!({"mode": "observe", "domains": [], "note": "", "expected_revision": 2})).await;
    let list = t.get("/api/v1/policies").await;
    let rev = list["teams"][0]["nodes"][1]["policy"]["revision"].as_i64().unwrap();
    t.put_policy("node", fx.n[1], json!({"mode": "inherit", "domains": [], "note": "", "expected_revision": rev})).await;
    let pv = t.post("/api/v1/policies/preview", json!({})).await;
    let v2 = t.post("/api/v1/policies/publish", json!({"draft_hash": pv["draft_hash"]})).await;
    assert_eq!(v2["seq"], 2);
    assert_eq!(t.relay_generation(fx.relay).await, g0 + 2);
    let ds2 = t.agent_desired(&fx.token).await;
    assert_eq!(ds2.entries[1].policy.as_ref().unwrap().mode, PolicyMode::Allowlist, "团队1-02 回到跟随团队");
    assert!(ds2.entries[2].policy.is_none());
    t.agent_applied(&fx.token, ds2.generation, true, &ds2).await;
    // 团队2-01 不再暂停：健康提示消失
    let m = t.get(&format!("/api/v1/machines/{}", fx.n[2])).await;
    assert!(m["health"]["reasons"].as_array().unwrap().iter().all(|r| r["key"] != "policy_blocked"));

    // 版本列表：最新在前；v1 可回滚，v2 是当前
    let vs = t.get("/api/v1/policies/versions").await;
    assert_eq!(vs.as_array().unwrap().len(), 2);
    assert_eq!((vs[0]["seq"].as_i64(), vs[0]["status"].as_str(), vs[0]["is_current"].as_bool()), (Some(2), Some("applied"), Some(true)));
    assert_eq!((vs[1]["seq"].as_i64(), vs[1]["rollbackable"].as_bool()), (Some(1), Some(true)));
    let v1_id = vs[1]["id"].as_str().unwrap().to_string();
    let v2_id = vs[0]["id"].as_str().unwrap().to_string();

    // 回滚到 v1：生成新版本 v3，线路机代数 +1，期望状态回到 v1 的样子
    t.fail("POST", &format!("/api/v1/policies/versions/{v2_id}/rollback"), None, StatusCode::CONFLICT).await;
    let v3 = t.post(&format!("/api/v1/policies/versions/{v1_id}/rollback"), json!({})).await;
    assert_eq!((v3["seq"].as_i64(), v3["kind"].as_str(), v3["rollback_of_seq"].as_i64()), (Some(3), Some("rollback"), Some(1)));
    assert_eq!(v3["status"], "applying");
    assert_eq!(t.relay_generation(fx.relay).await, g0 + 3);
    let ds3 = t.agent_desired(&fx.token).await;
    assert_eq!(ds3.entries[1].policy.as_ref().unwrap().mode, PolicyMode::Denylist);
    assert_eq!(ds3.entries[2].policy.as_ref().unwrap().mode, PolicyMode::Blocked);
    t.agent_applied(&fx.token, ds3.generation, true, &ds3).await;
    let vs = t.get("/api/v1/policies/versions").await;
    assert_eq!(vs.as_array().unwrap().len(), 3);
    assert_eq!((vs[0]["status"].as_str(), vs[0]["is_current"].as_bool()), (Some("applied"), Some(true)));
    // v1 与现在一致，不能再回滚；v2 可以
    assert_eq!(vs[2]["rollbackable"], false);
    assert_eq!(vs[1]["rollbackable"], true);

    // 审计留痕
    for (action, want) in [("policy.draft", 5i64), ("policy.publish", 2), ("policy.rollback", 1)] {
        let n: i64 = sqlx::query_scalar("select count(*) from audit_log where action = $1").bind(action).fetch_one(&t.st.db).await.unwrap();
        assert_eq!(n, want, "{action}");
    }
}

#[tokio::test]
async fn revision_conflicts_and_stale_hash() {
    let t = setup().await;
    let fx = seed(&t).await;
    let body = |rev: i64, d: &str| json!({"mode": "denylist", "domains": d, "note": "", "expected_revision": rev});

    // 记录还不存在：expected_revision 必须是 0
    let m = t.fail("PUT", &format!("/api/v1/policies/team/{}", fx.team1), Some(body(3, "a.com")), StatusCode::CONFLICT).await;
    assert!(m.contains("请重新加载"), "{m}");
    let r = t.put_policy("team", fx.team1, body(0, "a.com")).await;
    assert_eq!(r["policy"]["revision"], 1);
    // 两个人同时基于 revision 1 编辑：先到的成功，后到的 409
    let r = t.put_policy("team", fx.team1, body(1, "a.com, b.com")).await;
    assert_eq!(r["policy"]["revision"], 2);
    let m = t.fail("PUT", &format!("/api/v1/policies/team/{}", fx.team1), Some(body(1, "c.com")), StatusCode::CONFLICT).await;
    assert!(m.contains("草稿已被修改"), "{m}");
    // 已有记录用 0 也不行
    t.fail("PUT", &format!("/api/v1/policies/team/{}", fx.team1), Some(body(0, "c.com")), StatusCode::CONFLICT).await;
    // 草稿没丢
    let list = t.get("/api/v1/policies").await;
    assert_eq!(list["teams"][0]["policy"]["draft"]["domains"], json!(["a.com", "b.com"]));

    // 草稿被改回与生效值相同 = 草稿消失（observe 且无域名）
    let r = t.put_policy("team", fx.team1, json!({"mode": "observe", "domains": [], "note": "", "expected_revision": 2})).await;
    assert_eq!(r["policy"]["draft"], Value::Null);
    assert_eq!(r["draft_hash"], Value::Null);
    assert_eq!(r["draft_count"], 0);

    // 预览后草稿变化 → 旧 hash 发布 409；丢弃用对的 hash 才行，丢弃后生效值与期望状态都没变
    let g0 = t.relay_generation(fx.relay).await;
    t.put_policy("team", fx.team1, body(3, "a.com")).await;
    let h1 = t.post("/api/v1/policies/preview", json!({})).await["draft_hash"].as_str().unwrap().to_string();
    t.put_policy("node", fx.n[0], json!({"mode": "blocked", "domains": [], "note": "", "expected_revision": 0})).await;
    t.fail("POST", "/api/v1/policies/publish", Some(json!({"draft_hash": h1})), StatusCode::CONFLICT).await;
    t.fail("POST", "/api/v1/policies/publish", Some(json!({"draft_hash": "deadbeef"})), StatusCode::CONFLICT).await;
    let h2 = t.post("/api/v1/policies/preview", json!({})).await["draft_hash"].as_str().unwrap().to_string();
    let d = t.post("/api/v1/policies/discard", json!({"draft_hash": h2})).await;
    assert_eq!(d["discarded"], 2);
    let list = t.get("/api/v1/policies").await;
    assert_eq!(list["draft_count"], 0);
    assert_eq!(list["teams"][0]["policy"]["mode"], "observe");
    assert_eq!(t.relay_generation(fx.relay).await, g0, "丢弃不动线路机");
    assert!(t.agent_desired(&fx.token).await.entries.iter().all(|e| e.policy.is_none()));
    assert!(t.get("/api/v1/policies/versions").await.as_array().unwrap().is_empty());
    // 丢弃使 revision 前进：基于丢弃前版本的编辑 409
    let rev = list["teams"][0]["policy"]["revision"].as_i64().unwrap();
    assert!(rev > 3);
    t.fail("PUT", &format!("/api/v1/policies/team/{}", fx.team1), Some(body(3, "z.com")), StatusCode::CONFLICT).await;
}

/// 预览之后有人回滚：草稿没变，但生效版本变了，旧的 draft_hash 不能直接发布
#[tokio::test]
async fn rollback_invalidates_old_preview() {
    let t = setup().await;
    let fx = seed(&t).await;
    t.put_policy("team", fx.team1, json!({"mode": "denylist", "domains": ["a.com"], "note": "", "expected_revision": 0})).await;
    let pv = t.post("/api/v1/policies/preview", json!({})).await;
    let v1 = t.post("/api/v1/policies/publish", json!({"draft_hash": pv["draft_hash"]})).await;
    t.put_policy("team", fx.team1, json!({"mode": "observe", "domains": [], "note": "", "expected_revision": 2})).await;
    let pv = t.post("/api/v1/policies/preview", json!({})).await;
    t.post("/api/v1/policies/publish", json!({"draft_hash": pv["draft_hash"]})).await;

    t.put_policy("node", fx.n[2], json!({"mode": "blocked", "domains": [], "note": "", "expected_revision": 0})).await;
    let old = t.post("/api/v1/policies/preview", json!({})).await["draft_hash"].as_str().unwrap().to_string();
    t.post(&format!("/api/v1/policies/versions/{}/rollback", v1["id"].as_str().unwrap()), json!({})).await;
    let m = t.fail("POST", "/api/v1/policies/publish", Some(json!({"draft_hash": old})), StatusCode::CONFLICT).await;
    assert!(m.contains("生效版本已变化"), "{m}");
    let new = t.post("/api/v1/policies/preview", json!({})).await["draft_hash"].as_str().unwrap().to_string();
    assert_ne!(new, old);
    let v = t.post("/api/v1/policies/publish", json!({"draft_hash": new})).await;
    assert_eq!(v["seq"], 4);
}

/// 与生效值相同的草稿（读时当作没有）：回滚正好回到草稿的值时清掉；库里残留的不算丢弃、不动 revision
#[tokio::test]
async fn drafts_equal_to_current_are_not_drafts() {
    let t = setup().await;
    let fx = seed(&t).await;
    let deny = |rev: i64| json!({"mode": "denylist", "domains": ["a.com"], "note": "", "expected_revision": rev});
    let publish = || async {
        let pv = t.post("/api/v1/policies/preview", json!({})).await;
        t.post("/api/v1/policies/publish", json!({"draft_hash": pv["draft_hash"]})).await
    };
    t.put_policy("team", fx.team1, deny(0)).await;
    let v1 = publish().await;
    t.put_policy("team", fx.team1, json!({"mode": "observe", "domains": [], "note": "", "expected_revision": 2})).await;
    publish().await;
    // 草稿改回黑名单，再回滚到 v1（也是这个黑名单）：草稿正好是回滚后的值，一起清掉
    t.put_policy("team", fx.team1, deny(4)).await;
    t.post(&format!("/api/v1/policies/versions/{}/rollback", v1["id"].as_str().unwrap()), json!({})).await;
    let draft = || async {
        let (d, rev): (Option<Value>, i32) =
            sqlx::query_as("select draft, revision from policy where scope = 'team' and scope_id = $1").bind(fx.team1).fetch_one(&t.st.db).await.unwrap();
        (d.is_some(), rev)
    };
    let (has, rev) = draft().await;
    assert!(!has, "回滚后不留与生效值相同的草稿");
    // 库里残留一份（迁移 0035 之前的数据）：丢弃只算真正的草稿，这一行 revision 不动
    sqlx::query("update policy set draft = $2 where scope = 'team' and scope_id = $1")
        .bind(fx.team1)
        .bind(json!({"mode": "denylist", "domains": ["a.com"], "note": ""}))
        .execute(&t.st.db)
        .await
        .unwrap();
    t.put_policy("node", fx.n[2], json!({"mode": "blocked", "domains": [], "note": "", "expected_revision": 0})).await;
    let pv = t.post("/api/v1/policies/preview", json!({})).await;
    assert_eq!(pv["draft_count"], 1);
    let d = t.post("/api/v1/policies/discard", json!({"draft_hash": pv["draft_hash"]})).await;
    assert_eq!(d["discarded"], 1);
    assert_eq!(draft().await, (true, rev));
    // 迁移 0035 把残留的清掉，同样不动 revision
    sqlx::query(include_str!("../../../migrations/0035_policy_ghost_drafts.sql")).execute(&t.st.db).await.unwrap();
    assert_eq!(draft().await, (false, rev));
}

/// 删团队不绕开发布版本：访问规则不是默认、或有草稿时 409；改回默认并发布后才能删。
/// 回滚到删团队之前的版本时跳过这个团队（不建出没有主人的规则行），别的范围照常回滚
#[tokio::test]
async fn team_delete_respects_policy_versions() {
    let t = setup().await;
    let fx = seed(&t).await;
    let team = common::seed_team(&t, "团队9").await;
    let path = format!("/api/v1/teams/{team}");
    let publish = || async {
        let pv = t.post("/api/v1/policies/preview", json!({})).await;
        t.post("/api/v1/policies/publish", json!({"draft_hash": pv["draft_hash"]})).await
    };
    t.put_policy("team", team, json!({"mode": "denylist", "domains": ["a.com"], "note": "", "expected_revision": 0})).await;
    t.put_policy("team", fx.team1, json!({"mode": "denylist", "domains": ["a.com"], "note": "", "expected_revision": 0})).await;
    assert!(t.fail("DELETE", &path, None, StatusCode::CONFLICT).await.contains("访问规则"), "只有草稿也不能删");
    let v1 = publish().await;
    assert!(t.fail("DELETE", &path, None, StatusCode::CONFLICT).await.contains("访问规则"), "生效的规则不是默认");
    // 改回仅观察时客户端仍带着隐藏的旧域名：服务端存成空的，删团队不再因此被拒
    let r = t.put_policy("team", team, json!({"mode": "observe", "domains": ["a.com"], "note": "", "expected_revision": 2})).await;
    assert_eq!(r["policy"]["draft"]["domains"], json!([]));
    t.put_policy("team", fx.team1, json!({"mode": "observe", "domains": ["a.com"], "note": "", "expected_revision": 2})).await;
    assert!(t.fail("DELETE", &path, None, StatusCode::CONFLICT).await.contains("草稿"), "改回默认的草稿还没发布");
    let pv = t.post("/api/v1/policies/preview", json!({})).await;
    assert!(pv["changes"].as_array().unwrap().iter().all(|c| c["to"] == json!({"mode": "observe", "domains": 0})), "{pv}");
    publish().await;
    // 「当前」= 最新一版；v1 可回滚，回滚会改什么（当前 → v1）按版本单独取
    let vs = t.get("/api/v1/policies/versions").await;
    assert_eq!((vs[0]["is_latest"].as_bool(), vs[0]["is_current"].as_bool(), vs[0]["rollbackable"].as_bool()), (Some(true), Some(true), Some(false)));
    assert!(vs[1].get("rollback_changes").is_none(), "列表不带回滚预览");
    let rp = t.get(&format!("/api/v1/policies/versions/{}/rollback", vs[1]["id"].as_str().unwrap())).await;
    assert_eq!((rp["seq"].as_i64(), rp["rollbackable"].as_bool()), (Some(1), Some(true)), "{rp}");
    let rc = rp["changes"].as_array().unwrap();
    assert_eq!(rc.len(), 2);
    assert!(rc.iter().all(|c| c["from"]["mode"] == "observe" && c["to"] == json!({"mode": "denylist", "domains": 1})), "{rc:?}");
    let cur = t.get(&format!("/api/v1/policies/versions/{}/rollback", vs[0]["id"].as_str().unwrap())).await;
    assert_eq!((cur["rollbackable"].as_bool(), cur["changes"].clone()), (Some(false), json!([])));
    let (s, v) = t.req("DELETE", &path, None, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    t.post(&format!("/api/v1/policies/versions/{}/rollback", v1["id"].as_str().unwrap()), json!({})).await;
    let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as("select scope_id, mode from policy where scope = 'team'").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(rows, vec![(fx.team1, "denylist".to_string())]);
    // v1 里被删掉的团队不算：v1 与当前相同（不可回滚，和回滚接口的判断一致），但「当前」只标最新的 v3
    let vs = t.get("/api/v1/policies/versions").await;
    assert_eq!((vs[0]["seq"].as_i64(), vs[0]["is_latest"].as_bool()), (Some(3), Some(true)));
    assert_eq!((vs[2]["seq"].as_i64(), vs[2]["is_latest"].as_bool(), vs[2]["is_current"].as_bool(), vs[2]["rollbackable"].as_bool()), (Some(1), Some(false), Some(true), Some(false)));
    t.fail("POST", &format!("/api/v1/policies/versions/{}/rollback", v1["id"].as_str().unwrap()), None, StatusCode::CONFLICT).await;
    // 操作记录写出回滚到哪一版、改了什么
    let s: String = sqlx::query_scalar("select search from audit_log where action = 'policy.rollback'").fetch_one(&t.st.db).await.unwrap();
    assert!(s.contains("v3 · 回滚到 v1 · 团队1：仅观察 → 黑名单（1 个域名）"), "{s}");
}

#[tokio::test]
async fn note_only_change_and_node_exemption() {
    let t = setup().await;
    let fx = seed(&t).await;
    let g0 = t.relay_generation(fx.relay).await;
    // 只改备注：不需要线路机做任何事，版本直接是已应用
    t.put_policy("team", fx.team1, json!({"mode": "observe", "domains": [], "note": "先观察一周", "expected_revision": 0})).await;
    let pv = t.post("/api/v1/policies/preview", json!({})).await;
    assert_eq!(pv["affected_relays"], 0);
    assert!(pv["nodes"].as_array().unwrap().is_empty());
    assert_eq!((pv["changes"][0]["note_changed"].as_bool(), pv["changes"][0]["domains_changed"].as_bool()), (Some(true), Some(false)), "{pv}");
    let v = t.post("/api/v1/policies/publish", json!({"draft_hash": pv["draft_hash"]})).await;
    assert_eq!(v["status"], "applied");
    assert!(v["relays"].as_array().unwrap().is_empty());
    assert_eq!(t.relay_generation(fx.relay).await, g0);

    // 节点自己选 observe，可以豁免团队的黑名单
    t.put_policy("team", fx.team1, json!({"mode": "denylist", "domains": ["bad.com"], "note": "", "expected_revision": 2})).await;
    t.put_policy("node", fx.n[0], json!({"mode": "observe", "domains": [], "note": "豁免", "expected_revision": 0})).await;
    let pv = t.post("/api/v1/policies/preview", json!({})).await;
    t.post("/api/v1/policies/publish", json!({"draft_hash": pv["draft_hash"]})).await;
    let ds = t.agent_desired(&fx.token).await;
    assert!(ds.entries[0].policy.is_none(), "节点显式 observe 优先于团队");
    assert_eq!(ds.entries[1].policy.as_ref().unwrap().mode, PolicyMode::Denylist);
    assert!(ds.entries[2].policy.is_none());
}

#[tokio::test]
async fn validation_errors() {
    let t = setup().await;
    let fx = seed(&t).await;
    let put = |scope: &'static str, id: uuid::Uuid, mode: &'static str, domains: Value| {
        let t = &t;
        async move {
            t.req("PUT", &format!("/api/v1/policies/{scope}/{id}"), Some(json!({"mode": mode, "domains": domains, "note": "", "expected_revision": 0})), None).await
        }
    };
    // 非法域名：400，信息中文且点名
    for (input, want) in [("good.com, 1.2.3.4", "1.2.3.4"), ("https://a.com/x", "https://a.com/x"), ("中文.com", "中文.com"), ("nodot", "nodot")] {
        let (s, v) = put("team", fx.team1, "denylist", json!(input)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{input}");
        let m = v["error"]["message"].as_str().unwrap();
        assert!(m.contains(want), "{input} -> {m}");
    }
    // allowlist / denylist 必须有域名
    let (s, v) = put("team", fx.team1, "allowlist", json!([])).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().contains("必须填写域名"));
    // 团队不能 inherit；未知模式
    assert_eq!(put("team", fx.team1, "inherit", json!([])).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(put("team", fx.team1, "pause", json!([])).await.0, StatusCode::BAD_REQUEST);
    // 未知范围 / 不存在的对象 / 线路机
    assert_eq!(put("group", fx.team1, "observe", json!([])).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(put("team", uuid::Uuid::now_v7(), "observe", json!([])).await.0, StatusCode::NOT_FOUND);
    assert_eq!(put("node", fx.relay, "observe", json!([])).await.0, StatusCode::BAD_REQUEST);
    // 300 个上限
    let many: Vec<String> = (0..301).map(|i| format!("h{i}.example.com")).collect();
    let (s, _) = put("team", fx.team1, "denylist", json!(many)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // 全部失败后没有留下任何记录
    let n: i64 = sqlx::query_scalar("select count(*) from policy").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 0);
}
