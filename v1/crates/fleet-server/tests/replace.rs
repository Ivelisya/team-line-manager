//! 替换节点（replace）：预览（改哪些路由的哪些出口、编号怎么变、旧节点去哪）、校验、作业（改软路由出口 → 交换编号）、
//! 路由离线时一台都不改、一台失败时别的换回原配置、重试、取消、不改路由只换编号、编号压缩。需要 PostgreSQL（见 common）。
//!
//! 路由是假的：PATH 最前面放假 ssh（路由上的脚本本身在 routers::replace 的单元测试里用 sh 真跑过）。按 `-p <端口>` 分路由根目录，
//! 收到改配置的脚本存成 apply-<n>.sh，按根目录里的 mode 回答（ok / fail = 测不通、自己换回 / refuse = 认不出、一行没写），
//! 收到回滚脚本记进 rollback.log。PATH 是整个进程共用的，所以单独一个测试文件。作业引擎不在后台跑（采集循环会改路由状态），测试里手动推进。
//! 换线路机 / 搬空改软路由出口用的是同一步，也在这里测（线路机用 common::Sim 模拟）。

#![cfg(unix)]

#[path = "it/common/mod.rs"]
mod common;

use axum::http::StatusCode;
use common::{seed_relay, seed_relay_b, seed_team, setup, Sim, T};
use fleet_server::jobs;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::LazyLock;
use uuid::Uuid;

const SSH: &str = r#"#!/bin/sh
for a; do
  if [ "${next:-}" = 1 ]; then port=$a; next=; fi
  [ "$a" = -p ] && next=1
  [ "$a" = -O ] && exit 0
done
R="$FAKE_ROOT/${port:-22}"
mkdir -p "$R"
[ -e "$R/down" ] && { echo "ssh: connect to host 127.0.0.1 port $port: Connection refused" >&2; exit 255; }
script=$(cat)
case $script in
  *"MODE='rollback'"*) printf '%s\n' "$script" | sed -n "s/^STAMP='\(.*\)'$/\1/p" >> "$R/rollback.log"; echo rollback=ok; exit 0 ;;
esac
n=$(ls "$R" | grep -c '^apply-' || true)
printf '%s\n' "$script" > "$R/apply-$((n + 1)).sh"
case $(cat "$R/mode" 2>/dev/null || echo ok) in
  ok) echo "backup=/etc/fleet-backup/openclash/c.yaml.x"; echo changed=6; echo delay_1=88; echo replace=ok ;;
  fail) echo "backup=/x"; echo changed=6; echo "fleet：出口「节点01」换成新入口后测了 3 次都不通" >&2; echo rolled_back=1; exit 4 ;;
  refuse) echo "fleet：配置的 proxies 里没有出口「节点01」" >&2; echo "fleet：配置没有改动" >&2; exit 2 ;;
esac
"#;

static FAKE: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("bin/ssh");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, SSH).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("PATH", format!("{}:{}", dir.path().join("bin").display(), std::env::var("PATH").unwrap_or_default()));
    std::env::set_var("FAKE_ROOT", dir.path());
    dir
});

fn root(port: i32) -> PathBuf { FAKE.path().join(port.to_string()) }
fn read(p: PathBuf) -> String { std::fs::read_to_string(p).unwrap_or_default() }
fn applies(port: i32) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(root(port)).map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("apply-")).collect()).unwrap_or_default();
    v.sort();
    v.iter().map(|n| read(root(port).join(n))).collect()
}

const NEW_UUID: &str = "22222222-2222-4222-8222-222222222222";

struct W {
    t: T,
    team: Uuid,
    spare: Uuid,
    relay: Uuid,
    old: Uuid,
    new: Uuid,
}

async fn node(t: &T, team: Uuid, number: i32, ip: &str) -> Uuid { common::seed_node(t, team, number, ip, "active").await }

async fn entry(t: &T, relay: Uuid, node: Uuid, port: i32, uuid: &str, lifecycle: &str) -> Uuid {
    let e = Uuid::now_v7();
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, $4, $5::uuid, 'abcd', $6::entry_lifecycle)")
        .bind(e)
        .bind(relay)
        .bind(node)
        .bind(port)
        .bind(uuid)
        .bind(lifecycle)
        .execute(&t.st.db)
        .await
        .unwrap();
    e
}

/// 团队2：01 是旧节点（线路机2:20003），02 是刚上架的新节点（线路机2:20020）；备用团队是迁移建好的那个，改名为「团队2备用」
async fn world() -> W {
    LazyLock::force(&FAKE);
    let t = setup(false).await;
    let team = seed_team(&t, "团队2").await;
    let spare: Uuid = sqlx::query_scalar("select id from team where spare").fetch_one(&t.st.db).await.unwrap();
    t.req("PATCH", &format!("/api/v1/teams/{spare}"), Some(json!({"name": "团队2备用"})), None).await;
    let (relay, _) = seed_relay(&t).await;
    let old = node(&t, team, 1, "203.0.113.11").await;
    let new = node(&t, team, 2, "203.0.113.12").await;
    entry(&t, relay, old, 20003, "11111111-1111-4111-8111-111111111111", "active").await;
    entry(&t, relay, new, 20020, NEW_UUID, "active").await;
    W { t, team, spare, relay, old, new }
}

/// 一台在线的软路由（发过隧道密钥），快照里 `exits`：(出口名, server, port, 对上的入口)
async fn router(w: &W, name: &str, port: i32, exits: &[(&str, &str, u16, Option<Uuid>)]) -> Uuid {
    let id = Uuid::now_v7();
    let db = &w.t.st.db;
    sqlx::query("insert into router (id, name, team_id, tunnel_port, tunnel_pubkey, tunnel_key_at) values ($1, $2, $3, $4, 'ssh-ed25519 AAAA', now() - interval '1 hour')")
        .bind(id)
        .bind(name)
        .bind(w.team)
        .bind(port)
        .execute(db)
        .await
        .unwrap();
    let mut xs = vec![];
    for (n, srv, p, e) in exits {
        let entry = match e {
            Some(e) => {
                let (node, relay, lc): (Uuid, Uuid, String) = sqlx::query_as("select node_id, relay_id, lifecycle::text from entry where id = $1").bind(e).fetch_one(db).await.unwrap();
                json!({"id": e, "name": "x", "node_id": node, "relay_id": relay, "node_name": "", "relay_name": "线路机2", "port": p, "lifecycle": lc})
            }
            None => Value::Null,
        };
        xs.push(json!({"name": n, "type": "Vless", "server": srv, "port": p, "entry": entry, "sync": if e.is_some() { "ok" } else { "unmanaged" }}));
    }
    sqlx::query("insert into router_status (router_id, ts, reachable, data, last_ok_at) values ($1, now(), true, $2, now())")
        .bind(id)
        .bind(json!({"exits": xs}))
        .execute(db)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(root(port));
    std::fs::create_dir_all(root(port)).unwrap();
    id
}

async fn old_entry(w: &W) -> Uuid { sqlx::query_scalar("select id from entry where node_id = $1").bind(w.old).fetch_one(&w.t.st.db).await.unwrap() }

/// 手动推进作业引擎，直到没有可领的
async fn drive(t: &T) {
    while let Some(j) = jobs::claim(&t.st, "test").await.unwrap() {
        jobs::advance(&t.st, j, "test").await.unwrap();
    }
}

async fn name_of(t: &T, id: Uuid) -> String {
    sqlx::query_scalar("select display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = $1").bind(id).fetch_one(&t.st.db).await.unwrap()
}

async fn logs(t: &T, job: &str) -> String { t.get(&format!("/api/v1/jobs/{job}/logs")).await.to_string() }

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preview_and_full_run() {
    let w = world().await;
    let oe = old_entry(&w).await;
    router(&w, "团队2-路由A", 23101, &[("节点01", "198.51.100.2", 20003, Some(oe)), ("直连", "203.0.113.99", 8388, None)]).await;
    router(&w, "团队2-路由B", 23102, &[("节点01", "relay.test", 20003, Some(oe))]).await;
    let t = &w.t;
    let p = t.get(&format!("/api/v1/machines/{}/replace?new_machine_id={}", w.old, w.new)).await;
    assert_eq!(p["ok"], true, "{p}");
    assert_eq!((p["old"]["name"].as_str(), p["new"]["name"].as_str(), p["new_name"].as_str()), (Some("团队2-01"), Some("团队2-02"), Some("团队2-01")));
    assert_eq!(p["old_to"], json!({"team": {"id": w.spare, "name": "团队2备用", "spare": true}, "number": 1, "name": "团队2备用-01"}));
    let rs = p["routers"].as_array().unwrap();
    assert_eq!(rs.len(), 2, "{p}");
    // 出口写的是线路机 IP 就换成 IP，写的是域名就换成 public_host
    assert_eq!(rs[0]["exits"], json!([{"exit": "节点01", "old_entry_id": oe, "new_entry_id": rs[0]["exits"][0]["new_entry_id"], "from": "198.51.100.2:20003", "to": "198.51.100.2:20020"}]));
    assert_eq!(rs[1]["exits"][0]["to"], "relay.test:20020");
    // 新节点原来的 02 空出来，团队里没有 03 以后的：不用压缩
    assert_eq!(p["renames"], json!([]));

    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    drive(t).await;
    let j = t.job(&job).await;
    assert_eq!(j["state"], "succeeded", "{j} {}", logs(t, &job).await);
    assert_eq!((j["kind"].as_str(), j["steps"][0]["state"].as_str(), j["steps"][1]["state"].as_str()), (Some("replace"), Some("done"), Some("done")));
    assert_eq!((name_of(t, w.new).await, name_of(t, w.old).await), ("团队2-01".to_string(), "团队2备用-01".to_string()));
    assert_eq!(t.machine_lifecycle(w.old).await, "active", "旧节点不退役，放在备用团队");
    assert_eq!(t.get(&format!("/api/v1/machines/{}", w.old)).await["spare_from"], "团队2-01", "进备用记下原名");
    // 两台路由各收到一份改配置的脚本：新入口的参数在 stdin 里；作业日志、步骤输出、审计里没有凭据
    for (port, srv) in [(23101, "198.51.100.2"), (23102, "relay.test")] {
        let a = applies(port);
        assert_eq!(a.len(), 1, "{port}");
        assert!(a[0].contains("MODE='apply'") && a[0].contains("NAME_1='节点01'") && a[0].contains(&format!("UUID_1='{NEW_UUID}'")), "{}", &a[0][..400]);
        assert!(a[0].contains(&format!("OSRV_1='{srv}'\nOPORT_1='20003'\nSRV_1='{srv}'\nPORT_1='20020'")), "{}", &a[0][..400]);
    }
    let l = logs(t, &job).await;
    assert!(l.contains("团队2-路由A") && l.contains("节点01 198.51.100.2:20003 → 198.51.100.2:20020") && l.contains("88 ms"), "{l}");
    let dump: String = sqlx::query_scalar("select (select coalesce(string_agg(msg, ' '), '') from job_log) || (select params::text from job) || (select string_agg(coalesce(output::text, ''), ' ') from job_step) || (select string_agg(detail::text, ' ') from audit_log)")
        .fetch_one(&t.st.db)
        .await
        .unwrap();
    assert!(!dump.contains(NEW_UUID) && !dump.contains("abcd'"), "凭据不进日志、参数、审计");
    // 审计：替换一条，改名两条（同 PATCH），摘要看得懂
    let au = t.get(&format!("/api/v1/audit?target_id={}", w.old)).await;
    let acts: Vec<&str> = au.as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap()).collect();
    assert!(acts.contains(&"machine.replace") && acts.contains(&"machine.rename"), "{acts:?}");
    let rep = au.as_array().unwrap().iter().find(|a| a["action"] == "machine.replace").unwrap();
    assert_eq!((rep["title"].as_str(), rep["summary"].as_str()), (Some("替换节点"), Some("团队2-01 由 团队2-02 接替，旧节点改为 团队2备用-01（改 2 台软路由）")));
    let au = t.get(&format!("/api/v1/audit?target_id={}", w.new)).await;
    assert_eq!(au[0]["summary"], "团队2-02 → 团队2-01");
    // 作业已结束：不能取消
    assert_eq!(t.req("POST", &format!("/api/v1/jobs/{job}/cancel"), None, None).await.0, StatusCode::CONFLICT);
}

/// 有一台路由不在线：预览列出原因、提交 409；作业排队后路由才掉线：一台都不改，作业失败，可以取消
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_router_blocks_everything() {
    let w = world().await;
    let oe = old_entry(&w).await;
    router(&w, "路由A", 23111, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let b = router(&w, "路由B", 23112, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let t = &w.t;
    let off = |id: Uuid| async move {
        sqlx::query("update router_status set reachable = false, error = '经隧道连不上路由：refused' where router_id = $1").bind(id).execute(&t.st.db).await.unwrap();
    };
    off(b).await;
    let p = t.get(&format!("/api/v1/machines/{}/replace?new_machine_id={}", w.old, w.new)).await;
    assert_eq!(p["ok"], false);
    let bl = p["blockers"][0].as_str().unwrap();
    assert!(bl.contains("软路由「路由B」现在离线：经隧道连不上路由：refused") && bl.contains("节点01"), "{bl}");
    let (s, v) = t.req("POST", &format!("/api/v1/machines/{}/replace", w.old), Some(json!({"new_machine_id": w.new})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    // 不改路由：不看路由在不在线
    let p = t.get(&format!("/api/v1/machines/{}/replace?new_machine_id={}&routers=false", w.old, w.new)).await;
    assert_eq!(p["ok"], true, "{p}");

    sqlx::query("update router_status set reachable = true where router_id = $1").bind(b).execute(&t.st.db).await.unwrap();
    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    off(b).await;
    drive(t).await;
    let j = t.job(&job).await;
    assert_eq!(j["state"], "failed");
    let err = j["error"].as_str().unwrap();
    assert!(err.contains("软路由 路由B（经隧道连不上路由：refused） 不在线，一台都没有改"), "{err}");
    assert!(applies(23111).is_empty() && applies(23112).is_empty(), "一台都没碰");
    assert_eq!(name_of(t, w.old).await, "团队2-01");
    assert_eq!(j["cancellable"], true, "改路由失败（没改动）停在第一步：能取消");
    t.post(&format!("/api/v1/jobs/{job}/cancel"), json!({})).await;
    drive(t).await;
    assert_eq!(t.job(&job).await["state"], "cancelled");
    assert_eq!((name_of(t, w.old).await, name_of(t, w.new).await), ("团队2-01".to_string(), "团队2-02".to_string()));
}

/// 第二台测不通（它自己换回了）：第一台按备份换回，作业失败、编号不动；修好后重试，两台都重新改
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failure_rolls_back_and_retry_redoes() {
    let w = world().await;
    let oe = old_entry(&w).await;
    router(&w, "路由A", 23121, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    router(&w, "路由B", 23122, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let (ra, rb) = sqlx::query_as::<_, (String, String)>("select (select name from router order by tunnel_port limit 1), (select name from router order by tunnel_port desc limit 1)").fetch_one(&w.t.st.db).await.unwrap();
    assert_eq!((ra.as_str(), rb.as_str()), ("路由A", "路由B"));
    let t = &w.t;
    std::fs::write(root(23122).join("mode"), "fail").unwrap();
    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    drive(t).await;
    let j = t.job(&job).await;
    assert_eq!(j["state"], "failed");
    let err = j["error"].as_str().unwrap();
    assert!(err.contains("软路由「路由B」：出口「节点01」换成新入口后测了 3 次都不通；已换回原配置：路由A。软路由都是原样"), "{err}");
    // 路由A 按它的备份时间戳回滚；路由B 自己换回了，不再回滚
    let stamp_a = read(root(23121).join("apply-1.sh")).lines().find_map(|l| l.strip_prefix("STAMP='").map(|s| s.trim_end_matches('\'').to_string())).unwrap();
    assert_eq!(read(root(23121).join("rollback.log")), format!("{stamp_a}\n"));
    assert_eq!(read(root(23122).join("rollback.log")), "");
    assert_eq!(name_of(t, w.old).await, "团队2-01", "编号没动");
    // 认不出（一行没写）：同样回滚已改好的
    std::fs::write(root(23122).join("mode"), "refuse").unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    drive(t).await;
    let err = t.job(&job).await["error"].as_str().unwrap().to_string();
    assert!(err.contains("配置的 proxies 里没有出口「节点01」；配置没有改动；已换回原配置：路由A"), "{err}");
    std::fs::write(root(23122).join("mode"), "ok").unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    drive(t).await;
    assert_eq!(t.job(&job).await["state"], "succeeded", "{}", logs(t, &job).await);
    assert_eq!((applies(23121).len(), applies(23122).len()), (3, 3));
    assert_eq!(name_of(t, w.new).await, "团队2-01");
}

/// 没有路由用旧节点 / 选了不改路由：只交换编号。指定团队与编号；auto_compact 压缩新节点原来的团队
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renumber_only_and_compaction() {
    let w = world().await;
    let t = &w.t;
    // 团队2：01 旧、02 新、03、04 → 新节点接 01，旧节点去团队2 的 07，02 空出来：03→02、04→03
    let n3 = node(t, w.team, 3, "203.0.113.13").await;
    let n4 = node(t, w.team, 4, "203.0.113.14").await;
    let q = format!("/api/v1/machines/{}/replace?new_machine_id={}&team_id={}&number=7", w.old, w.new, w.team);
    let p = t.get(&q).await;
    assert_eq!(p["ok"], true, "{p}");
    assert_eq!((p["old_to"]["name"].as_str(), p["old_to"]["team"]["spare"].as_bool()), (Some("团队2-07"), Some(false)));
    assert_eq!(p["routers"], json!([]));
    let ws = p["warnings"].to_string();
    assert!(ws.contains("不是备用团队") && ws.contains("没有软路由的出口指向旧节点"), "{ws}");
    let ren: Vec<(String, String)> = p["renames"].as_array().unwrap().iter().map(|r| (r["old_name"].as_str().unwrap().into(), r["new_name"].as_str().unwrap().into())).collect();
    assert_eq!(ren, [("团队2-03".to_string(), "团队2-02".to_string()), ("团队2-04".into(), "团队2-03".into()), ("团队2-07".into(), "团队2-04".into())]);
    // 编号被占、超范围
    assert!(t.get(&format!("/api/v1/machines/{}/replace?new_machine_id={}&team_id={}&number=3", w.old, w.new, w.team)).await["blockers"].to_string().contains("编号 3 已被占用"));
    let (s, _) = t.req("POST", &format!("/api/v1/machines/{}/replace", w.old), Some(json!({"new_machine_id": w.new, "old_to": {"team_id": w.team, "number": 1000}})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new, "old_to": {"team_id": w.team, "number": 7}})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    drive(t).await;
    let j = t.job(&job).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    assert_eq!(j["steps"][0]["state"], "skipped");
    let names = (name_of(t, w.new).await, name_of(t, n3).await, name_of(t, n4).await, name_of(t, w.old).await);
    assert_eq!(names, ("团队2-01".into(), "团队2-02".into(), "团队2-03".into(), "团队2-04".into()));

    // 选了不改路由：有路由用着旧节点也不碰
    let w = world().await;
    let oe = old_entry(&w).await;
    router(&w, "路由A", 23131, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let t = &w.t;
    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new, "routers": false})).await;
    drive(t).await;
    let j = t.job(d["job_id"].as_str().unwrap()).await;
    assert_eq!((j["state"].as_str(), j["steps"][0]["state"].as_str()), (Some("succeeded"), Some("skipped")));
    assert!(applies(23131).is_empty());
    assert_eq!(name_of(t, w.old).await, "团队2备用-01");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn validation() {
    let w = world().await;
    let t = &w.t;
    let url = |new: Uuid| format!("/api/v1/machines/{}/replace?new_machine_id={new}", w.old);
    // 自己替换自己、新节点不存在：400；旧节点不存在：404
    assert_eq!(t.req("GET", &url(w.old), None, None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.req("GET", &url(Uuid::now_v7()), None, None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.req("GET", &format!("/api/v1/machines/{}/replace?new_machine_id={}", Uuid::now_v7(), w.new), None, None).await.0, StatusCode::NOT_FOUND);
    // 新节点没有在用的入口；新节点是线路机；旧节点有进行中的作业
    sqlx::query("update entry set lifecycle = 'pending' where node_id = $1").bind(w.new).execute(&t.st.db).await.unwrap();
    assert!(t.get(&url(w.new)).await["blockers"].to_string().contains("新节点「团队2-02」没有在用的入口"));
    sqlx::query("update entry set lifecycle = 'active' where node_id = $1").bind(w.new).execute(&t.st.db).await.unwrap();
    assert!(t.get(&url(w.relay)).await["blockers"].to_string().contains("是线路机"));
    sqlx::query("insert into job (id, kind, target_id, state, created_by) values ($1, 'upgrade_agent', $2, 'queued', 't')").bind(Uuid::now_v7()).bind(w.old).execute(&t.st.db).await.unwrap();
    assert!(t.get(&url(w.new)).await["blockers"].to_string().contains("「团队2-01」还有进行中的作业（升级 agent）"));
    sqlx::query("delete from job").execute(&t.st.db).await.unwrap();
    // 新节点在别的团队：替换时改到旧节点的团队（提示一下）
    let other = seed_team(t, "待分配").await;
    let x = node(t, other, 1, "203.0.113.30").await;
    entry(t, w.relay, x, 20030, "33333333-3333-4333-8333-333333333333", "active").await;
    let p = t.get(&url(x)).await;
    assert_eq!((p["ok"].as_bool(), p["new_name"].as_str()), (Some(true), Some("团队2-01")));
    assert!(p["warnings"].to_string().contains("不在旧节点的团队（团队2）"));
    // 新节点在两台线路机上都有入口、都不在旧入口的线路机上：说明对不上
    let oe = old_entry(&w).await;
    router(&w, "路由A", 23141, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let other_relay = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle) values ($1, 'relay', '线路机3', '198.51.100.3', 'active')").bind(other_relay).execute(&t.st.db).await.unwrap();
    sqlx::query("insert into relay (machine_id, public_host, reality_private_enc, reality_public_key) values ($1, 'r3.test', '\\x00', 'PBK3')").bind(other_relay).execute(&t.st.db).await.unwrap();
    let third = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle) values ($1, 'relay', '线路机4', '198.51.100.4', 'active')").bind(third).execute(&t.st.db).await.unwrap();
    sqlx::query("insert into relay (machine_id, public_host, reality_private_enc, reality_public_key) values ($1, 'r4.test', '\\x00', 'PBK4')").bind(third).execute(&t.st.db).await.unwrap();
    sqlx::query("update entry set relay_id = $2, port = 20040 where node_id = $1").bind(w.new).bind(other_relay).execute(&t.st.db).await.unwrap();
    entry(t, third, w.new, 20050, "44444444-4444-4444-8444-444444444444", "active").await;
    let b = t.get(&format!("{}&team_id={}", url(w.new), w.spare)).await["blockers"].to_string();
    assert!(b.contains("软路由「路由A」的出口「节点01」") && b.contains("不知道换成哪个"), "{b}");
}

// ── 换线路机 / 搬空：摘旧入口之前改软路由出口 ─────────────────────────────

const RELAY_TOKEN: &str = "fa1_test_relay_token";

/// 模拟两台线路机、手动推进作业，直到这个作业结束
async fn run(t: &T, sims: &mut [Sim], job: &str) -> Value {
    for _ in 0..100 {
        for s in sims.iter_mut() {
            s.round(t).await;
        }
        // 出口探测直接记一份（这里没有后台任务）：验收新入口要它
        sqlx::query("insert into probe_result (entry_id, ts, kind, ok, latency_ms, detail) select id, now(), 'egress', true, 12, '' from entry where lifecycle = 'active'")
            .execute(&t.st.db)
            .await
            .unwrap();
        drive(t).await;
        let j = t.job(job).await;
        if matches!(j["state"].as_str(), Some("failed" | "cancelled" | "succeeded")) {
            return j;
        }
        t.nudge_jobs().await;
        // 应用回报有限流（同 it/lifecycle.rs 的节奏）
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("作业 {job} 没结束：{} {}", t.job(job).await, logs(t, job).await);
}

async fn entry_state(t: &T, id: Uuid) -> String { sqlx::query_scalar("select lifecycle::text from entry where id = $1").bind(id).fetch_one(&t.st.db).await.unwrap() }

/// 出口写的是旧线路机 IP 的改成新线路机 IP，写域名的改成新线路机的 public_host，端口是新入口的；改完才摘旧入口
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_rewrites_router_exits() {
    let w = world().await;
    let oe = old_entry(&w).await;
    router(&w, "路由A", 23151, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    router(&w, "路由B", 23152, &[("节点01", "relay.test", 20003, Some(oe))]).await;
    let t = &w.t;
    let (rb, tb) = seed_relay_b(t).await;
    let p = t.get(&format!("/api/v1/machines/{}/move?to_relay_id={rb}", w.old)).await;
    assert_eq!((p["ok"].as_bool(), p["routers_enabled"].as_bool()), (Some(true), Some(true)), "{p}");
    let rs = p["routers"].as_array().unwrap();
    assert_eq!((rs[0]["exits"][0]["from"].as_str(), rs[0]["exits"][0]["to"].as_str()), (Some("198.51.100.2:20003"), Some("198.51.100.3:新端口")));
    assert_eq!(rs[1]["exits"][0]["to"], "relay3.test:新端口");
    let d = t.post(&format!("/api/v1/machines/{}/move", w.old), json!({"to_relay_id": rb})).await;
    assert_eq!(d["routers"].as_array().unwrap().len(), 2, "{d}");
    let job = d["job_id"].as_str().unwrap().to_string();
    let j = run(t, &mut [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)], &job).await;
    assert_eq!(j["state"], "succeeded", "{j} {}", logs(t, &job).await);
    assert_eq!((j["steps"][4]["key"].as_str(), j["steps"][4]["state"].as_str()), (Some("routers"), Some("done")));
    let (np, uuid): (i32, Uuid) = sqlx::query_as("select port, client_uuid from entry where node_id = $1 and relay_id = $2").bind(w.old).bind(rb).fetch_one(&t.st.db).await.unwrap();
    for (port, osrv, srv) in [(23151, "198.51.100.2", "198.51.100.3"), (23152, "relay.test", "relay3.test")] {
        let a = applies(port);
        assert_eq!(a.len(), 1, "{port}");
        assert!(a[0].contains(&format!("OSRV_1='{osrv}'\nOPORT_1='20003'\nSRV_1='{srv}'\nPORT_1='{np}'")) && a[0].contains(&format!("UUID_1='{uuid}'")), "{}", &a[0][..400]);
    }
    assert_eq!(entry_state(t, oe).await, "removed");
    let l = logs(t, &job).await;
    assert!(l.contains(&format!("节点01 198.51.100.2:20003 → 198.51.100.3:{np}")), "{l}");
}

/// 路由不在线时预览阻断（不改路由的不阻断）；一台改失败：已改好的换回，旧入口不摘，作业失败、可以取消（取消摘掉新入口）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_router_failure_keeps_old_entry() {
    let w = world().await;
    let oe = old_entry(&w).await;
    router(&w, "路由A", 23161, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let b = router(&w, "路由B", 23162, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let t = &w.t;
    let (rb, tb) = seed_relay_b(t).await;
    sqlx::query("update router_status set reachable = false, error = '经隧道连不上路由：refused' where router_id = $1").bind(b).execute(&t.st.db).await.unwrap();
    let p = t.get(&format!("/api/v1/machines/{}/move?to_relay_id={rb}", w.old)).await;
    let bl = p["blockers"].to_string();
    assert!(p["ok"] == false && bl.contains("软路由「路由B」现在离线") && bl.contains("要等它在线再换线路机"), "{p}");
    assert_eq!(t.req("POST", &format!("/api/v1/machines/{}/move", w.old), Some(json!({"to_relay_id": rb})), None).await.0, StatusCode::CONFLICT);
    assert_eq!(t.get(&format!("/api/v1/machines/{}/move?to_relay_id={rb}&routers=false", w.old)).await["ok"], true);
    sqlx::query("update router_status set reachable = true where router_id = $1").bind(b).execute(&t.st.db).await.unwrap();

    std::fs::write(root(23162).join("mode"), "fail").unwrap();
    let d = t.post(&format!("/api/v1/machines/{}/move", w.old), json!({"to_relay_id": rb})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    let mut sims = [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)];
    let j = run(t, &mut sims, &job).await;
    assert_eq!((j["state"].as_str(), j["steps"][4]["state"].as_str()), (Some("failed"), Some("failed")), "{j}");
    let err = j["error"].as_str().unwrap();
    assert!(err.contains("软路由「路由B」：出口「节点01」换成新入口后测了 3 次都不通；已换回原配置：路由A。软路由都是原样，旧入口没摘"), "{err}");
    let stamp_a = read(root(23161).join("apply-1.sh")).lines().find_map(|l| l.strip_prefix("STAMP='").map(|s| s.trim_end_matches('\'').to_string())).unwrap();
    assert_eq!(read(root(23161).join("rollback.log")), format!("{stamp_a}\n"));
    assert_eq!(entry_state(t, oe).await, "active", "旧入口没摘");
    assert_eq!(j["cancellable"], true, "改路由失败、已换回：能取消");
    t.post(&format!("/api/v1/jobs/{job}/cancel"), json!({})).await;
    assert_eq!(run(t, &mut sims, &job).await["state"], "cancelled");
    let es: Vec<(Uuid, String)> = sqlx::query_as("select id, lifecycle::text from entry where node_id = $1").bind(w.old).fetch_all(&t.st.db).await.unwrap();
    assert!(es.iter().all(|e| (e.0 == oe) == (e.1 == "active")), "只剩旧入口在用：{es:?}");
}

/// 搬空：预览列出各节点要改的出口；改同一台路由的作业一个接一个跑（后建的排在前一个之后）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evacuate_rewrites_exits_one_job_at_a_time() {
    let w = world().await;
    let oe = old_entry(&w).await;
    let ne: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(&w.t.st.db).await.unwrap();
    router(&w, "路由A", 23171, &[("节点01", "198.51.100.2", 20003, Some(oe)), ("节点02", "198.51.100.2", 20020, Some(ne))]).await;
    let t = &w.t;
    let (rb, tb) = seed_relay_b(t).await;
    let p = t.get(&format!("/api/v1/relays/{}/evacuate?to_relay_id={rb}", w.relay)).await;
    let exits: Vec<&str> = p["jobs"].as_array().unwrap().iter().map(|j| j["routers"][0]["exits"][0]["exit"].as_str().unwrap()).collect();
    assert_eq!(exits, ["节点01", "节点02"], "{p}");
    let d = t.post(&format!("/api/v1/relays/{}/evacuate", w.relay), json!({"to_relay_id": rb})).await;
    let ids: Vec<String> = d["jobs"].as_array().unwrap().iter().map(|j| j["job_id"].as_str().unwrap().to_string()).collect();
    let after: Vec<Option<String>> = sqlx::query_scalar("select params->>'routers_after' from job order by created_at, id").fetch_all(&t.st.db).await.unwrap();
    assert_eq!(after, [None, Some(ids[0].clone())]);
    assert!(logs(t, &ids[1]).await.contains(&format!("排在作业 {}（团队2-01）之后", ids[0])));
    // 第一个在跑时第二个领不到
    let c = jobs::claim(&t.st, "test").await.unwrap().unwrap();
    assert_eq!(c.id.to_string(), ids[0]);
    assert!(jobs::claim(&t.st, "test").await.unwrap().is_none());
    jobs::advance(&t.st, c, "test").await.unwrap();
    let mut sims = [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)];
    for id in &ids {
        assert_eq!(run(t, &mut sims, id).await["state"], "succeeded", "{}", logs(t, id).await);
    }
    let a = applies(23171);
    assert!(a.len() == 2 && a[0].contains("NAME_1='节点01'") && a[1].contains("NAME_1='节点02'"), "{a:?}");
}
