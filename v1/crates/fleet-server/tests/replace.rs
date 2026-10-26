//! 替换节点（replace）：预览（改哪些路由的哪些出口、编号怎么变、旧节点去哪）、校验、作业（改软路由出口 → 交换编号）、
//! 路由离线时一台都不改、一台失败时别的换回原配置、重试、取消、不改路由只换编号、编号压缩。需要 PostgreSQL（见 common）。
//!
//! 路由是假的：PATH 最前面放假 ssh（路由上的脚本本身在 routers::replace 的单元测试里用 sh 真跑过）。按 `-p <端口>` 分路由根目录，
//! 收到读配置的脚本回 config.yaml（`router` 按快照里的出口写好，测试可以改：执行时按它定要改的出口；每读一次往 reads 里记一行），
//! 收到改配置的脚本存成 apply-<n>.sh（根目录里有 hold 就等它删掉再回答），按根目录里的 mode 回答（ok / fail = 测不通、自己改回 / refuse = 认不出、一行没写 /
//! drop = 备份之后连接断了、没有输出 / busy = 等不到路由上的锁、一行没写；之后的取结果有 result 文件就回它，否则「没有结果」），收到回滚脚本把「时间戳 出口 地址 端口」记进 rollback.log（有 rollback-fail 就回答改回失败）。PATH 是整个进程共用的，所以单独一个测试文件。作业引擎不在后台跑（采集循环会改路由状态），测试里手动推进。
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
  "MODE='read'"*)
    [ -e "$R/read-fail" ] && { echo "fleet：读配置失败了" >&2; exit 1; }
    [ -e "$R/no-config" ] && { echo "fleet：OpenClash 的源配置（uci openclash.config.config_path）不存在：没有设置" >&2; exit 3; }
    echo >> "$R/reads"; cat "$R/config.yaml"; exit 0 ;;
  # 连接断了之后取结果（routers::replace::fetch_script）：有 result 就当路由上的脚本跑完了、回它，否则没有结果
  *"WAIT='"*)
    [ -e "$R/result" ] && { cat "$R/result"; exit 0; }
    echo "fleet：路由上没有这次的结果" >&2; exit 5 ;;
  *"MODE='apply'"*) [ -e "$R/down-apply" ] && { echo "Connection timed out during banner exchange" >&2; exit 255; } ;;
  *"MODE='rollback'"*)
    [ -e "$R/rollback-fail" ] && { echo "fleet：写回备份失败" >&2; exit 3; }
    printf '%s\n' "$script" | sed -n "s/^STAMP='\(.*\)'$/\1/p;s/^NAME_1='\(.*\)'$/\1/p;s/^SRV_1='\(.*\)'$/\1/p;s/^PORT_1='\(.*\)'$/\1/p" | paste -sd' ' >> "$R/rollback.log"
    printf '%s\n' "$script" | sed -n "s/^UUID_1='\(.*\)'$/\1/p" >> "$R/rollback-uuid"
    # 路由上这个出口还指向这次的新地址、凭据却被改过：脚本不动它，说明写 stderr、报 skipped_1，整体仍是 rollback=ok
    [ -e "$R/rollback-skip" ] && { echo "fleet：出口「节点01」还指向这次改成的 198.51.100.2:20020，但 uuid、servername、public-key 或 short-id 已经不是这次写的（之后又被改过），没有改回" >&2; echo skipped_1=1; }
    echo rollback=ok; exit 0 ;;
esac
n=$(ls "$R" | grep -c '^apply-' || true)
printf '%s\n' "$script" > "$R/apply-$((n + 1)).sh"
while [ -e "$R/hold" ]; do sleep 0.05; done
case $(cat "$R/mode" 2>/dev/null || echo ok) in
  ok) echo "backup=/etc/fleet-backup/openclash/c.yaml.x"; echo changed=6; echo delay_1=88; echo replace=ok ;;
  fail) echo "backup=/x"; echo changed=6; echo "fleet：出口「节点01」换成新入口后测了 3 次都不通" >&2; echo rolled_back=1; exit 4 ;;
  refuse) echo "fleet：配置的 proxies 里没有出口「节点01」" >&2; echo "fleet：配置没有改动" >&2; exit 2 ;;
  drop) echo "backup=/etc/fleet-backup/openclash/c.yaml.x"; exit 255 ;;
  busy) echo "fleet：路由上上一次改出口的脚本（pid 1）60 秒内还没跑完，这次没有动配置" >&2; exit 7 ;;
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

/// 路由上 OpenClash 的源配置：只有出口的 name / type / server / port（执行时按它定要改的出口）
fn config(port: i32, exits: &[(&str, &str, u16)]) {
    let ps: String = exits.iter().map(|(n, srv, p)| format!("- name: {n}\n  type: vless\n  server: {srv}\n  port: {p}\n")).collect();
    std::fs::write(root(port).join("config.yaml"), format!("proxies:\n{ps}")).unwrap();
}

/// 一台在线的软路由（发过隧道密钥），快照里 `exits`：(出口名, server, port, 对上的入口)；配置里是同样的出口
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
    config(port, &exits.iter().map(|(n, srv, p, _)| (*n, *srv, *p)).collect::<Vec<_>>());
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
    assert_eq!(rs[0]["exits"], json!([{"exit": "节点01", "old_entry_id": oe, "new_entry_id": rs[0]["exits"][0]["new_entry_id"], "from": "198.51.100.2:20003", "to": "198.51.100.2:20020", "error": null}]));
    assert_eq!(rs[1]["exits"][0]["to"], "relay.test:20020");
    // 新节点原来的 02 空出来，团队里没有 03 以后的：不用压缩
    assert_eq!(p["renames"], json!([]));
    assert!(p["warnings"].to_string().contains("旧节点「团队2-01」没有订阅：进备用后没有到期日，不会自动回收"), "{p}");
    // 来源画像：旧节点见过的跟到新节点，新节点自己已有的不动
    sqlx::query(
        "insert into node_source_seen (node_id, dim, value, first_seen, last_seen, conns) values
           ($1, 'isp', '电信', '2026-09-01T00:00:00Z', now(), 50), ($1, 'city', '上海', '2026-09-02T00:00:00Z', now(), 9), ($2, 'isp', '电信', '2026-10-20T00:00:00Z', now(), 3)",
    )
    .bind(w.old)
    .bind(w.new)
    .execute(&t.st.db)
    .await
    .unwrap();

    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    drive(t).await;
    let j = t.job(&job).await;
    assert_eq!(j["state"], "succeeded", "{j} {}", logs(t, &job).await);
    let p: Value = sqlx::query_scalar("select params from job where id = $1").bind(Uuid::parse_str(&job).unwrap()).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(p["routers_applied"], json!([]), "改路由这一步做完就清掉：{p}");
    assert_eq!((j["kind"].as_str(), j["steps"][0]["state"].as_str(), j["steps"][1]["state"].as_str()), (Some("replace"), Some("done"), Some("done")));
    assert_eq!((name_of(t, w.new).await, name_of(t, w.old).await), ("团队2-01".to_string(), "团队2备用-01".to_string()));
    assert_eq!(t.machine_lifecycle(w.old).await, "active", "旧节点不退役，放在备用团队");
    let ov = t.get(&format!("/api/v1/machines/{}", w.old)).await;
    assert_eq!((ov["spare_from"].as_str(), ov["lifecycle_reason"].as_str()), (Some("团队2-01"), Some("替换下来：团队2-02 接替了 团队2-01")), "进备用记下原名和原因");
    let seen: Vec<(String, String, String, i64)> = sqlx::query_as("select dim, value, first_seen::date::text, conns from node_source_seen where node_id = $1 order by dim")
        .bind(w.new)
        .fetch_all(&t.st.db)
        .await
        .unwrap();
    assert_eq!(seen, [("city".to_string(), "上海".to_string(), "2026-09-02".to_string(), 0), ("isp".into(), "电信".into(), "2026-10-20".into(), 3)]);
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
    assert!(err.contains("软路由「路由B」：出口「节点01」换成新入口后测了 3 次都不通；已改回原配置：路由A。软路由都是原样"), "{err}");
    // 路由A 按它的备份时间戳改回这次改过的出口（改成的地址对得上的才改回）；路由B 自己改回了，不再回滚
    let stamp_a = read(root(23121).join("apply-1.sh")).lines().find_map(|l| l.strip_prefix("STAMP='").map(|s| s.trim_end_matches('\'').to_string())).unwrap();
    assert_eq!(read(root(23121).join("rollback.log")), format!("{stamp_a} 节点01 198.51.100.2 20020\n"));
    assert_eq!(read(root(23122).join("rollback.log")), "");
    assert_eq!(name_of(t, w.old).await, "团队2-01", "编号没动");
    // 认不出（一行没写）：同样回滚已改好的
    std::fs::write(root(23122).join("mode"), "refuse").unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    drive(t).await;
    let err = t.job(&job).await["error"].as_str().unwrap().to_string();
    assert!(err.contains("配置的 proxies 里没有出口「节点01」；配置没有改动；已改回原配置：路由A"), "{err}");
    // 读完配置之后路由B 连不上了（改配置时 ssh 没连上，脚本一行没跑）：它什么都没改，不去换回它、不算换回失败；可以直接重试 / 取消
    std::fs::write(root(23122).join("down-apply"), "").unwrap();
    std::fs::write(root(23122).join("rollback-fail"), "").unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    drive(t).await;
    let j = t.job(&job).await;
    let err = j["error"].as_str().unwrap();
    assert!(err.contains("软路由「路由B」：经隧道连不上路由：SSH 握手超时（路由忙，或隧道断了），这台没连上、什么都没改；已改回原配置：路由A。软路由都是原样，编号没改"), "{err}");
    assert!(logs(t, &job).await.contains("软路由「路由B」ssh 的原始输出：Connection timed out during banner exchange"), "英文原文只进日志");
    assert!(!err.contains("人工处理") && j["cancellable"] == true, "{j}");
    assert_eq!(read(root(23122).join("rollback.log")), "", "没连上的不去换回");
    std::fs::remove_file(root(23122).join("down-apply")).unwrap();
    std::fs::remove_file(root(23122).join("rollback-fail")).unwrap();
    std::fs::write(root(23122).join("mode"), "ok").unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    drive(t).await;
    assert_eq!(t.job(&job).await["state"], "succeeded", "{}", logs(t, &job).await);
    assert_eq!((applies(23121).len(), applies(23122).len()), (4, 3));
    assert_eq!(name_of(t, w.new).await, "团队2-01");
}

/// 改回用本作业写进出口的那组参数核对，不从库里现取：第二台还在改的时候新入口被轮换了，第一台改回带的仍是写入时的 uuid
/// （按现取的会对不上，路由上明明是本作业写的却当成别人改过）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollback_uses_params_written_not_current_ones() {
    let w = world().await;
    let oe = old_entry(&w).await;
    router(&w, "路由A", 23231, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    router(&w, "路由B", 23232, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let t = &w.t;
    std::fs::write(root(23232).join("mode"), "fail").unwrap();
    std::fs::write(root(23232).join("hold"), "").unwrap();
    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    let st = t.st.clone();
    let h = tokio::spawn(async move {
        while let Some(j) = jobs::claim(&st, "test").await.unwrap() {
            jobs::advance(&st, j, "test").await.unwrap();
        }
    });
    while applies(23232).is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let rotated = "33333333-3333-4333-8333-333333333333";
    sqlx::query("update entry set client_uuid = $2::uuid where node_id = $1").bind(w.new).bind(rotated).execute(&t.st.db).await.unwrap();
    std::fs::remove_file(root(23232).join("hold")).unwrap();
    h.await.unwrap();
    let j = t.job(&job).await;
    assert!(j["error"].as_str().unwrap().contains("已改回原配置：路由A"), "{j}");
    assert_eq!(read(root(23231).join("rollback-uuid")), format!("{NEW_UUID}\n"), "带的是写入时的 uuid，不是现在库里的");
}

/// 改回时路由上某个出口还指向本作业的新地址、凭据却被改过（脚本不动它、报 skipped_1，整体仍是 rollback=ok）：这台不算改回，作业写明要人工处理的路由与出口，
/// 记进 routers_stuck、不写「已改回原配置」「软路由都是原样」；作业不能取消。处理好后重试照常改回。
/// 「出口本来就是原值」脚本不报 skipped，仍算改回了：failure_rolls_back_and_retry_redoes 里路由A 的改回（没有 skipped_N）、
/// 以及 routers::replace 的 apply_edits_only_the_named_exits 单元测试（已是原样时 rollback=ok 且无 skipped_N）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollback_skipped_exit_is_stuck_not_reverted() {
    let w = world().await;
    let oe = old_entry(&w).await;
    let a = router(&w, "路由A", 23271, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    router(&w, "路由B", 23272, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let t = &w.t;
    std::fs::write(root(23272).join("mode"), "fail").unwrap();
    std::fs::write(root(23271).join("rollback-skip"), "").unwrap();
    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    drive(t).await;
    let j = t.job(&job).await;
    assert_eq!(j["state"], "failed");
    let err = j["error"].as_str().unwrap();
    assert!(err.contains("软路由「路由B」：出口「节点01」换成新入口后测了 3 次都不通；没能改回、需要人工处理：路由A（出口「节点01」没有改回：路由上还指向本作业的新入口，但凭据不是本作业写的"), "{err}");
    assert!(err.contains("还指向这次改成的 198.51.100.2:20020") && err.contains("处理好后点重试接着改完（这时不能取消）"), "{err}");
    assert!(!err.contains("已改回原配置") && !err.contains("软路由都是原样"), "{err}");
    assert_eq!(j["cancellable"], false);
    assert_eq!(j["manual"]["routers"], json!([{"id": a, "name": "路由A", "exits": ["节点01"]}]), "{j}");
    let stuck: Value = sqlx::query_scalar("select params->'routers_stuck' from job where id = $1").bind(Uuid::parse_str(&job).unwrap()).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(stuck.as_array().map(Vec::len), Some(1), "{stuck}");
    // 处理好了（出口不再被别人占着）：重试照常改回，这回都改回了
    std::fs::remove_file(root(23271).join("rollback-skip")).unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    drive(t).await;
    let j = t.job(&job).await;
    let err = j["error"].as_str().unwrap();
    assert!(err.contains("已改回原配置：路由A。软路由都是原样"), "{err}");
    assert_eq!((j["cancellable"].as_bool(), j["manual"].is_null()), (Some(true), true), "{j}");
}

/// 没有路由用旧节点 / 选了不改路由：只交换编号。指定团队与编号；新节点原来的团队压缩编号
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
    // 旧节点进备用后什么时候回收：开着自动续费的会关掉、已过期满 1 天的一小时内就回收
    sqlx::query("insert into subscription (machine_id, price_cents, currency, due_date, billing_tz, auto_renew) values ($1, 500, 'USD', '2099-01-01', 'UTC', true)")
        .bind(w.old)
        .execute(&t.st.db)
        .await
        .unwrap();
    assert!(t.get(&url(w.new)).await["warnings"].to_string().contains("开着自动续费：进备用时会关掉"));
    sqlx::query("update subscription set due_date = '2020-01-01' where machine_id = $1").bind(w.old).execute(&t.st.db).await.unwrap();
    assert!(t.get(&url(w.new)).await["warnings"].to_string().contains("订阅已过期满 1 天：进备用后下一轮维护（一小时内）就自动回收"));
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
    let p = t.get(&format!("{}&team_id={}", url(w.new), w.spare)).await;
    let b = p["blockers"].to_string();
    assert!(b.contains("软路由「路由A」的出口「节点01」") && b.contains("不知道换成哪个"), "{b}");
    // 对不上的出口照样算「指向旧节点的出口」列出来、带原因，不再同时提示「没有软路由的出口指向旧节点」
    let x = &p["routers"][0]["exits"][0];
    assert!(x["exit"] == "节点01" && x["to"].is_null() && x["error"].as_str().unwrap().contains("不知道换成哪个"), "{p}");
    assert!(!p["warnings"].to_string().contains("没有软路由的出口指向旧节点"), "{p}");
    // 新节点没有在用的入口：出口照样列出（原因就是这个），阻断只有总的那一条
    sqlx::query("update entry set lifecycle = 'pending' where node_id = $1").bind(w.new).execute(&t.st.db).await.unwrap();
    let p = t.get(&format!("{}&team_id={}", url(w.new), w.spare)).await;
    assert_eq!(p["blockers"].as_array().map(Vec::len), Some(1), "{p}");
    assert_eq!(p["routers"][0]["exits"][0]["error"], "新节点没有在用的入口", "{p}");
}

// ── 换线路机 / 搬空：摘旧入口之前改软路由出口 ─────────────────────────────

const RELAY_TOKEN: &str = "fa1_test_relay_token";

/// 模拟两台线路机、手动推进作业，直到这个作业结束
async fn run(t: &T, sims: &mut [Sim], job: &str) -> Value {
    for _ in 0..100 {
        one_round(t, sims).await;
        let j = t.job(job).await;
        if matches!(j["state"].as_str(), Some("failed" | "cancelled" | "succeeded")) {
            return j;
        }
    }
    panic!("作业 {job} 没结束：{} {}", t.job(job).await, logs(t, job).await);
}

/// 推进一轮：线路机回报、记探测、推进作业、唤醒等待的
async fn one_round(t: &T, sims: &mut [Sim]) {
    for s in sims.iter_mut() {
        s.round(t).await;
    }
    // 出口探测直接记一份（这里没有后台任务）：验收新入口要它
    sqlx::query("insert into probe_result (entry_id, ts, kind, ok, latency_ms, detail) select id, now(), 'egress', true, 12, '' from entry where lifecycle = 'active'")
        .execute(&t.st.db)
        .await
        .unwrap();
    drive(t).await;
    t.nudge_jobs().await;
    // 应用回报有限流（同 it/lifecycle.rs 的节奏）
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

async fn entry_state(t: &T, id: Uuid) -> String { sqlx::query_scalar("select lifecycle::text from entry where id = $1").bind(id).fetch_one(&t.st.db).await.unwrap() }

/// 出口写的是旧线路机 IP 的改成新线路机 IP，写域名的改成新线路机的 public_host，端口是新入口的；改完才摘旧入口。
/// 要改哪些出口按执行时路由的配置定：发起之后新加的、指向旧入口的出口也改（含快照里没有它的路由），已经不指向旧入口的不改
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_rewrites_router_exits() {
    let w = world().await;
    let oe = old_entry(&w).await;
    router(&w, "路由A", 23151, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    router(&w, "路由B", 23152, &[("节点01", "relay.test", 20003, Some(oe))]).await;
    router(&w, "路由C", 23153, &[]).await;
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
    // 发起之后：路由A 多了一个指向旧入口的出口，路由B 的出口改指别处，路由C（快照里没有出口）加了一个指向旧入口的
    config(23151, &[("节点01", "198.51.100.2", 20003), ("节点01备", "relay.test", 20003), ("别人的", "203.0.113.9", 8388)]);
    config(23152, &[("节点01", "203.0.113.50", 443)]);
    config(23153, &[("x", "198.51.100.2", 20003)]);
    let j = run(t, &mut [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)], &job).await;
    assert_eq!(j["state"], "succeeded", "{j} {}", logs(t, &job).await);
    assert_eq!((j["steps"][4]["key"].as_str(), j["steps"][4]["state"].as_str()), (Some("routers"), Some("done")));
    let (np, uuid): (i32, Uuid) = sqlx::query_as("select port, client_uuid from entry where node_id = $1 and relay_id = $2").bind(w.old).bind(rb).fetch_one(&t.st.db).await.unwrap();
    let a = applies(23151);
    assert_eq!(a.len(), 1);
    assert!(a[0].contains("N='2'\n") &&a[0].contains(&format!("NAME_1='节点01'\nOSRV_1='198.51.100.2'\nOPORT_1='20003'\nSRV_1='198.51.100.3'\nPORT_1='{np}'")), "{}", &a[0][..600]);
    assert!(a[0].contains(&format!("NAME_2='节点01备'\nOSRV_2='relay.test'\nOPORT_2='20003'\nSRV_2='relay3.test'\nPORT_2='{np}'")) && a[0].contains(&format!("UUID_1='{uuid}'")), "{}", &a[0][..600]);
    assert!(applies(23152).is_empty(), "路由B 的出口已经不指向旧入口：不改");
    let c = applies(23153);
    assert!(c.len() == 1 && c[0].contains("N='1'\n") && c[0].contains(&format!("NAME_1='x'\nOSRV_1='198.51.100.2'\nOPORT_1='20003'\nSRV_1='198.51.100.3'\nPORT_1='{np}'")), "{c:?}");
    assert_eq!(entry_state(t, oe).await, "removed");
    let l = logs(t, &job).await;
    assert!(l.contains(&format!("节点01 198.51.100.2:20003 → 198.51.100.3:{np}")), "{l}");
    assert!(l.contains("按软路由现在的配置要改：路由A 的 节点01、节点01备；路由C 的 x（发起时的预览是：路由A 的 节点01；路由B 的 节点01）"), "{l}");
    let out = &j["steps"][4]["output"]["routers"];
    assert_eq!(out.as_array().map(|r| r.iter().map(|x| x["name"].as_str().unwrap()).collect::<Vec<_>>()), Some(vec!["路由A", "路由C"]), "{out}");
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
    assert!(err.contains("软路由「路由B」：出口「节点01」换成新入口后测了 3 次都不通；已改回原配置：路由A。软路由都是原样，旧入口没摘"), "{err}");
    let stamp_a = read(root(23161).join("apply-1.sh")).lines().find_map(|l| l.strip_prefix("STAMP='").map(|s| s.trim_end_matches('\'').to_string())).unwrap();
    assert!(read(root(23161).join("rollback.log")).starts_with(&format!("{stamp_a} 节点01 198.51.100.3 ")), "{}", read(root(23161).join("rollback.log")));
    assert_eq!(entry_state(t, oe).await, "active", "旧入口没摘");
    assert_eq!(j["cancellable"], true, "改路由失败、已换回：能取消");
    // 改回也失败：错误写明要人工处理哪台，作业不能取消（取消会摘掉路由A 还指着的新入口），作业给出 manual；处理好后重试接着做
    std::fs::write(root(23161).join("rollback-fail"), "").unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    let j = run(t, &mut sims, &job).await;
    let err = j["error"].as_str().unwrap();
    assert!(err.contains("没能改回、需要人工处理：路由A（写回备份失败）。处理好后点重试接着改完（这时不能取消）"), "{err}");
    assert_eq!(j["cancellable"], false);
    let ra: Uuid = sqlx::query_scalar("select id from router where name = '路由A'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(j["manual"]["routers"], json!([{"id": ra, "name": "路由A", "exits": ["节点01"]}]), "{j}");
    assert!(j["manual"]["message"].as_str().unwrap().starts_with("软路由「路由A」的出口 节点01（原来指向 198.51.100.2:20003）可能停在改了一半"), "{j}");
    let (s, v) = t.req("POST", &format!("/api/v1/jobs/{job}/cancel"), None, None).await;
    assert!(s == StatusCode::CONFLICT && v.to_string().contains("没能改回原配置"), "{v}");
    std::fs::remove_file(root(23161).join("rollback-fail")).unwrap();
    // 卡住期间路由A 重新接入、换了隧道端口：改回按它现在的端口连
    sqlx::query("update router set tunnel_port = 23163 where id = $1").bind(ra).execute(&t.st.db).await.unwrap();
    let _ = std::fs::remove_dir_all(root(23163));
    std::fs::rename(root(23161), root(23163)).unwrap();
    let rolled = read(root(23163).join("rollback.log")).lines().count();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    let j = run(t, &mut sims, &job).await;
    assert_eq!((j["state"].as_str(), j["cancellable"].as_bool(), j["manual"].is_null()), (Some("failed"), Some(true), true), "{j}");
    assert!(!root(23161).exists() && read(root(23163).join("rollback.log")).lines().count() > rolled, "按现在的端口改回");
    t.post(&format!("/api/v1/jobs/{job}/cancel"), json!({})).await;
    assert_eq!(run(t, &mut sims, &job).await["state"], "cancelled");
    let es: Vec<(Uuid, String)> = sqlx::query_as("select id, lifecycle::text from entry where node_id = $1").bind(w.old).fetch_all(&t.st.db).await.unwrap();
    assert!(es.iter().all(|e| (e.0 == oe) == (e.1 == "active")), "只剩旧入口在用：{es:?}");
}

/// 推进作业引擎：各作业放到后台任务里推进，正在改 `port` 这台路由、卡在假 ssh 的 hold 上的留着（放进 `held`），其余推进完
async fn drive_holding(t: &T, sims: &mut [Sim], port: i32, held: &mut Vec<(String, tokio::task::JoinHandle<()>)>) {
    for s in sims.iter_mut() {
        s.round(t).await;
    }
    sqlx::query("insert into probe_result (entry_id, ts, kind, ok, latency_ms, detail) select id, now(), 'egress', true, 12, '' from entry where lifecycle = 'active'")
        .execute(&t.st.db)
        .await
        .unwrap();
    while let Some(j) = jobs::claim(&t.st, "test").await.unwrap() {
        let (id, n, st) = (j.id.to_string(), applies(port).len(), t.st.clone());
        let h = tokio::spawn(async move { jobs::advance(&st, j, "test").await.unwrap() });
        while !h.is_finished() && applies(port).len() == n {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if h.is_finished() {
            h.await.unwrap();
        } else {
            held.push((id, h));
        }
    }
    t.nudge_jobs().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

/// 搬空：预览列出各节点要改的出口、每台路由只重启 1 次；作业并行建新入口，都验收完后 id 最小的带头一起改：
/// 每台路由一次改完同批全部出口、重启一次。跟着的等它（不动路由、不能取消），带头的改完跟着完成，步骤输出是自己的那几个出口
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evacuate_changes_each_router_once() {
    let w = world().await;
    let oe = old_entry(&w).await;
    let ne: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(&w.t.st.db).await.unwrap();
    let a = router(&w, "路由A", 23171, &[("节点01", "198.51.100.2", 20003, Some(oe)), ("节点02", "198.51.100.2", 20020, Some(ne))]).await;
    let b = router(&w, "路由B", 23172, &[("节点02", "relay.test", 20020, Some(ne))]).await;
    let t = &w.t;
    let (rb, tb) = seed_relay_b(t).await;
    let p = t.get(&format!("/api/v1/relays/{}/evacuate?to_relay_id={rb}", w.relay)).await;
    let exits: Vec<&str> = p["jobs"].as_array().unwrap().iter().map(|j| j["routers"][0]["exits"][0]["exit"].as_str().unwrap()).collect();
    assert_eq!(exits, ["节点01", "节点02"], "{p}");
    assert_eq!(p["router_restarts"], json!([{"id": a, "name": "路由A", "times": 1, "nodes": 2}, {"id": b, "name": "路由B", "times": 1, "nodes": 1}]), "{p}");
    assert_eq!(t.get(&format!("/api/v1/relays/{}/evacuate?to_relay_id={rb}&routers=false", w.relay)).await["router_restarts"], json!([]));
    let d = t.post(&format!("/api/v1/relays/{}/evacuate", w.relay), json!({"to_relay_id": rb})).await;
    let ids: Vec<String> = d["jobs"].as_array().unwrap().iter().map(|j| j["job_id"].as_str().unwrap().to_string()).collect();
    let (lead, follow) = (ids[0].clone(), ids[1].clone());
    // 带头的卡在改路由A 上（假 ssh 等 hold 删掉）
    std::fs::write(root(23171).join("hold"), "").unwrap();
    let mut sims = [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)];
    let mut held = vec![];
    let mut waiting = Value::Null;
    for _ in 0..100 {
        drive_holding(t, &mut sims, 23171, &mut held).await;
        let j = t.job(&follow).await;
        if held.len() == 1 && j["step"] == 4 && j["state"] == "waiting" && logs(t, &follow).await.contains("带头") {
            waiting = j;
            break;
        }
    }
    assert_eq!(held.first().map(|h| h.0.as_str()), Some(lead.as_str()), "{waiting} {}", logs(t, &follow).await);
    assert_eq!(waiting["cancellable"], false, "被带头的认领了：不能取消");
    let (s, v) = t.req("POST", &format!("/api/v1/jobs/{follow}/cancel"), None, None).await;
    assert!(s == StatusCode::CONFLICT && v.to_string().contains("带头一起改软路由出口"), "{v}");
    assert!(logs(t, &follow).await.contains("同批换线路机的「团队2-01」带头一起改软路由出口（本节点的也在里面），等它改完"));
    assert!(logs(t, &lead).await.contains("同批换线路机的 2 个节点（团队2-02）一起改软路由出口：每台路由只改一次、重启一次"), "{}", logs(t, &lead).await);
    std::fs::remove_file(root(23171).join("hold")).unwrap();
    held.pop().unwrap().1.await.unwrap();
    for id in [&lead, &follow] {
        assert_eq!(run(t, &mut sims, id).await["state"], "succeeded", "{}", logs(t, id).await);
    }
    let port_on_b = |n: Uuid| sqlx::query_scalar::<_, i32>("select port from entry where node_id = $1 and relay_id = $2").bind(n).bind(rb).fetch_one(&t.st.db);
    let (pa, pb) = (port_on_b(w.old).await.unwrap(), port_on_b(w.new).await.unwrap());
    let ap = applies(23171);
    assert_eq!(ap.len(), 1, "路由A 只改一次：{ap:?}");
    assert!(ap[0].contains("N='2'\n") && ap[0].contains(&format!("NAME_1='节点01'\nOSRV_1='198.51.100.2'\nOPORT_1='20003'\nSRV_1='198.51.100.3'\nPORT_1='{pa}'")), "{}", &ap[0][..600]);
    assert!(ap[0].contains(&format!("NAME_2='节点02'\nOSRV_2='198.51.100.2'\nOPORT_2='20020'\nSRV_2='198.51.100.3'\nPORT_2='{pb}'")), "{}", &ap[0][..600]);
    let bp = applies(23172);
    assert!(bp.len() == 1 && bp[0].contains(&format!("NAME_1='节点02'\nOSRV_1='relay.test'\nOPORT_1='20020'\nSRV_1='relay3.test'\nPORT_1='{pb}'")), "{bp:?}");
    // 步骤输出：带头的列出全部（标明各出口归哪个作业）和认领的作业，跟着的只有自己的出口
    let (lj, fj) = (t.job(&lead).await, t.job(&follow).await);
    assert_eq!(lj["steps"][4]["output"]["batch"], json!([follow]), "{lj}");
    let fo = &fj["steps"][4]["output"];
    assert_eq!(fo["batch_leader"], json!(lead));
    let mine: Vec<(&str, &str)> = fo["routers"].as_array().unwrap().iter().flat_map(|r| r["exits"].as_array().unwrap().iter().map(move |x| (r["name"].as_str().unwrap(), x["exit"].as_str().unwrap()))).collect();
    assert_eq!(mine, [("路由A", "节点02"), ("路由B", "节点02")], "{fo}");
    assert_eq!(fo["routers"][0]["delay_ms"], json!([null]), "延迟跟着出口取（假 ssh 只回第一个出口的）：{fo}");
    assert_eq!((entry_state(t, oe).await, entry_state(t, ne).await), ("removed".to_string(), "removed".to_string()));
}

/// 搬空三个节点、带头的在排队等路由的锁时被取消（这时能取消）：被它认领的两个重新推一个带头的，一起改、路由只改一次，
/// 不各走各的（认领它们的 routers_batch_leader 还指着被取消的作业，先醒的那个也要把另一个算进来）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evacuate_leader_cancelled_while_queued_repicks_leader() {
    let w = world().await;
    let oe = old_entry(&w).await;
    let ne: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(&w.t.st.db).await.unwrap();
    let n3 = node(&w.t, w.team, 3, "203.0.113.13").await;
    let e3 = entry(&w.t, w.relay, n3, 20021, "33333333-3333-4333-8333-333333333333", "active").await;
    let a = router(&w, "路由A", 23241, &[("节点01", "198.51.100.2", 20003, Some(oe)), ("节点02", "198.51.100.2", 20020, Some(ne)), ("节点03", "198.51.100.2", 20021, Some(e3))]).await;
    let t = &w.t;
    let (rb, tb) = seed_relay_b(t).await;
    // 路由A 的锁被别的作业占着（测试自己拿，PG 会话级 advisory lock）
    let mut lock = t.st.db.acquire().await.unwrap();
    sqlx::query("select pg_advisory_lock(hashtextextended('router:' || $1::text, 0))").bind(a).execute(&mut *lock).await.unwrap();
    let d = t.post(&format!("/api/v1/relays/{}/evacuate", w.relay), json!({"to_relay_id": rb})).await;
    let mut ids: Vec<String> = d["jobs"].as_array().unwrap().iter().map(|j| j["job_id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 3, "{d}");
    ids.sort();
    let (lead, f1, f2) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
    let mut sims = [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)];
    let params = |job: &str| {
        let (db, job): (_, Uuid) = (t.st.db.clone(), job.parse().unwrap());
        async move { sqlx::query_scalar::<_, Value>("select params from job where id = $1").bind(job).fetch_one(&db).await.unwrap() }
    };
    // 带头的认领了另外两个、排队等路由A 的锁
    for _ in 0..100 {
        one_round(t, &mut sims).await;
        if params(&lead).await["routers_batch"].as_array().is_some_and(|b| b.len() == 2) && params(&lead).await["routers_queued_id"] == json!(a) {
            break;
        }
    }
    assert_eq!(params(&lead).await["routers_queued_id"], json!(a), "{}", logs(t, &lead).await);
    // 取消它：被认领的放开，重新推一个带头的（id 小的 f1 带头、认领 f2），路由A 的锁还占着，所以排队
    let (s, v) = t.req("POST", &format!("/api/v1/jobs/{lead}/cancel"), None, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let mut repicked = false;
    for _ in 0..100 {
        one_round(t, &mut sims).await;
        if params(&f1).await["routers_batch"] == json!([f2]) {
            repicked = true;
            break;
        }
    }
    assert!(repicked, "f1 应该重新带头认领 f2：{} / {}", params(&f1).await, logs(t, &f1).await);
    assert_eq!(t.job(&lead).await["state"], "cancelled");
    // 锁放开：两个一起改，路由A 只改一次、两个出口一起
    sqlx::query("select pg_advisory_unlock(hashtextextended('router:' || $1::text, 0))").bind(a).execute(&mut *lock).await.unwrap();
    drop(lock);
    for id in [&f1, &f2] {
        assert_eq!(run(t, &mut sims, id).await["state"], "succeeded", "{}", logs(t, id).await);
    }
    let ap = applies(23241);
    assert!(ap.len() == 1 && ap[0].contains("N='2'\n"), "路由A 只改一次、两个出口一起：{}", ap.iter().map(|a| a.lines().filter(|l| l.starts_with("N=") || l.starts_with("NAME_")).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join(" / "));
}

/// 同批换线路机一起改时一台失败：已改的路由全部换回，几个节点的旧入口都不摘，跟着的一起失败（能取消）；
/// 都重试后照样一起改：每台路由一次
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evacuate_batch_failure_rolls_back_all() {
    let w = world().await;
    let oe = old_entry(&w).await;
    let ne: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(&w.t.st.db).await.unwrap();
    router(&w, "路由A", 23176, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    router(&w, "路由B", 23177, &[("节点01", "198.51.100.2", 20003, Some(oe)), ("节点02", "198.51.100.2", 20020, Some(ne))]).await;
    let t = &w.t;
    let (rb, tb) = seed_relay_b(t).await;
    std::fs::write(root(23177).join("mode"), "fail").unwrap();
    let d = t.post(&format!("/api/v1/relays/{}/evacuate", w.relay), json!({"to_relay_id": rb})).await;
    let ids: Vec<String> = d["jobs"].as_array().unwrap().iter().map(|j| j["job_id"].as_str().unwrap().to_string()).collect();
    let mut sims = [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)];
    let (lj, fj) = (run(t, &mut sims, &ids[0]).await, run(t, &mut sims, &ids[1]).await);
    assert_eq!((lj["state"].as_str(), fj["state"].as_str()), (Some("failed"), Some("failed")), "{lj} {fj}");
    let err = lj["error"].as_str().unwrap();
    assert!(err.contains("软路由「路由B」：出口「节点01」换成新入口后测了 3 次都不通；已改回原配置：路由A。软路由都是原样，旧入口没摘"), "{err}");
    let ferr = fj["error"].as_str().unwrap();
    assert!(ferr.contains("同批换线路机的「团队2-01」带头一起改软路由出口（本节点的也在里面）没成，已一起换回：软路由「路由B」"), "{ferr}");
    assert_eq!((lj["cancellable"].as_bool(), fj["cancellable"].as_bool()), (Some(true), Some(true)), "都已换回：能取消");
    assert!(read(root(23176).join("rollback.log")).contains("节点01 198.51.100.3"), "路由A 改回了");
    let bp = applies(23177);
    assert!(bp.len() == 1 && bp[0].contains("N='2'\n"), "路由B 上两个节点的出口一起改：{bp:?}");
    assert_eq!((entry_state(t, oe).await, entry_state(t, ne).await), ("active".to_string(), "active".to_string()), "旧入口都没摘");
    // 修好后都重试：照样一起改
    std::fs::write(root(23177).join("mode"), "ok").unwrap();
    for id in &ids {
        t.post(&format!("/api/v1/jobs/{id}/retry"), json!({})).await;
    }
    for id in &ids {
        assert_eq!(run(t, &mut sims, id).await["state"], "succeeded", "{}", logs(t, id).await);
    }
    assert_eq!((applies(23176).len(), applies(23177).len()), (2, 2), "重试后每台路由各改一次");
    assert!(applies(23177)[1].contains("N='2'\n"));
    assert_eq!((entry_state(t, oe).await, entry_state(t, ne).await), ("removed".to_string(), "removed".to_string()));
}

/// 批量换线路机（POST /moves）：同一台路由上 3 个节点的出口，预览说只重启 1 次；3 个作业同批，路由只改一次（3 个出口一起）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_move_changes_router_once() {
    let w = world().await;
    let oe = old_entry(&w).await;
    let ne: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(&w.t.st.db).await.unwrap();
    let n3 = node(&w.t, w.team, 3, "203.0.113.13").await;
    let e3 = entry(&w.t, w.relay, n3, 20021, "33333333-3333-4333-8333-333333333333", "active").await;
    let a = router(&w, "路由A", 23251, &[("节点01", "198.51.100.2", 20003, Some(oe)), ("节点02", "198.51.100.2", 20020, Some(ne)), ("节点03", "198.51.100.2", 20021, Some(e3))]).await;
    let t = &w.t;
    let (rb, tb) = seed_relay_b(t).await;
    let nodes = [w.old, w.new, n3];
    let csv = nodes.iter().map(Uuid::to_string).collect::<Vec<_>>().join(",");
    let p = t.get(&format!("/api/v1/moves?machine_ids={csv}&to_relay_id={rb}")).await;
    assert_eq!(p["ok"], true, "{p}");
    assert_eq!(p["router_restarts"], json!([{"id": a, "name": "路由A", "times": 1, "nodes": 3}]), "{p}");
    assert_eq!(p["jobs"][0]["old_entry"], json!({"id": oe, "relay": "线路机2", "port": 20003}), "{p}");
    let d = t.post("/api/v1/moves", json!({"machine_ids": nodes, "to_relay_id": rb})).await;
    let ids: Vec<String> = d["jobs"].as_array().unwrap().iter().map(|j| j["job_id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 3, "{d}");
    let mut sims = [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)];
    for id in &ids {
        assert_eq!(run(t, &mut sims, id).await["state"], "succeeded", "{}", logs(t, id).await);
    }
    let ap = applies(23251);
    assert!(ap.len() == 1 && ap[0].contains("N='3'\n"), "路由A 只改一次、3 个出口一起：{}", ap.len());
    assert!(logs(t, &ids[1]).await.contains("同批换线路机的「团队2-01」带头一起改了软路由出口"), "{}", logs(t, &ids[1]).await);
    for e in [oe, ne, e3] {
        assert_eq!(entry_state(t, e).await, "removed");
    }
}

/// 批量换线路机 / 搬空时软路由不在线：被挡的节点进 skipped（不计入 router_restarts），离线路由按路由合并成 offline_routers
/// （一台路由一条、出口带节点名），blockers 里只留别的原因；提交 409，详情里也带 offline_routers
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_move_merges_offline_routers() {
    let w = world().await;
    let oe = old_entry(&w).await;
    let ne: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(&w.t.st.db).await.unwrap();
    let n3 = node(&w.t, w.team, 3, "203.0.113.13").await;
    let e3 = entry(&w.t, w.relay, n3, 20021, "33333333-3333-4333-8333-333333333333", "active").await;
    let n4 = node(&w.t, w.team, 4, "203.0.113.14").await;
    entry(&w.t, w.relay, n4, 20022, "44444444-4444-4444-8444-444444444444", "active").await;
    let a = router(&w, "路由A", 23261, &[("节点01", "198.51.100.2", 20003, Some(oe)), ("节点02", "198.51.100.2", 20020, Some(ne))]).await;
    let b = router(&w, "路由B", 23262, &[("节点03", "198.51.100.2", 20021, Some(e3))]).await;
    let t = &w.t;
    let (rb, _tb) = seed_relay_b(t).await;
    sqlx::query("update router_status set reachable = false, error = '经隧道连不上路由：refused' where router_id = $1").bind(a).execute(&t.st.db).await.unwrap();
    // 团队2-04 有进行中的作业：别的原因，照旧在 blockers 里
    sqlx::query("insert into job (id, kind, target_id, state, created_by) values ($1, 'upgrade_agent', $2, 'running', 'test')").bind(Uuid::now_v7()).bind(n4).execute(&t.st.db).await.unwrap();
    let csv = [w.old, w.new, n3, n4].iter().map(Uuid::to_string).collect::<Vec<_>>().join(",");
    let p = t.get(&format!("/api/v1/moves?machine_ids={csv}&to_relay_id={rb}")).await;
    assert_eq!(p["ok"], false, "{p}");
    assert_eq!(
        p["offline_routers"],
        json!([{"id": a, "name": "路由A", "state": "offline", "error": "经隧道连不上路由：refused",
                "exits": [{"node": "团队2-01", "exit": "节点01"}, {"node": "团队2-02", "exit": "节点02"}]}]),
        "{p}"
    );
    let bl: Vec<&str> = p["blockers"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    assert!(bl.len() == 1 && bl[0].contains("团队2-04") && !bl[0].contains("软路由"), "{bl:?}");
    assert_eq!(p["skipped"].as_array().unwrap().len(), 3, "团队2-01、02（路由A 离线）和 04（作业）被跳过：{p}");
    // 能换的 03 仍列在 jobs 里，只有它的路由B 会重启；被挡的节点不计入 router_restarts
    assert_eq!(p["router_restarts"], json!([{"id": b, "name": "路由B", "times": 1, "nodes": 1}]), "{p}");
    let (s, v) = t.req("POST", "/api/v1/moves", Some(json!({"machine_ids": [w.old, w.new, n3], "to_relay_id": rb})), None).await;
    let v = v.to_string();
    assert!(s == StatusCode::CONFLICT && v.contains("offline_routers") && v.contains("软路由「路由A」现在离线"), "{s} {v}");
    // 搬空预览共用同一份结构
    let ev = t.get(&format!("/api/v1/relays/{}/evacuate?to_relay_id={rb}", w.relay)).await;
    assert_eq!(ev["offline_routers"][0]["id"], json!(a), "{ev}");
    assert_eq!(ev["offline_routers"][0]["exits"].as_array().unwrap().len(), 2, "{ev}");
    // 不改软路由：没有离线路由要报
    let p = t.get(&format!("/api/v1/moves?machine_ids={},{}&to_relay_id={rb}&routers=false", w.old, w.new)).await;
    assert_eq!((p["ok"].clone(), p["offline_routers"].clone()), (json!(true), json!([])), "{p}");
}

/// 批量换线路机一起改时一台路由失败：同搬空，已改的全部换回、旧入口都不摘，跟着的一起失败；都能取消，取消摘掉新入口、只剩旧入口
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_move_failure_rolls_back_all() {
    let w = world().await;
    let oe = old_entry(&w).await;
    let ne: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(&w.t.st.db).await.unwrap();
    router(&w, "路由A", 23252, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    router(&w, "路由B", 23253, &[("节点02", "198.51.100.2", 20020, Some(ne))]).await;
    let t = &w.t;
    let (rb, tb) = seed_relay_b(t).await;
    std::fs::write(root(23253).join("mode"), "fail").unwrap();
    let d = t.post("/api/v1/moves", json!({"machine_ids": [w.old, w.new], "to_relay_id": rb})).await;
    let ids: Vec<String> = d["jobs"].as_array().unwrap().iter().map(|j| j["job_id"].as_str().unwrap().to_string()).collect();
    let mut sims = [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)];
    let (lj, fj) = (run(t, &mut sims, &ids[0]).await, run(t, &mut sims, &ids[1]).await);
    assert_eq!((lj["state"].as_str(), fj["state"].as_str()), (Some("failed"), Some("failed")), "{lj} {fj}");
    assert!(lj["error"].as_str().unwrap().contains("已改回原配置：路由A。软路由都是原样，旧入口没摘"), "{lj}");
    let ferr = fj["error"].as_str().unwrap();
    assert!(ferr.contains("同批换线路机的「团队2-01」带头一起改软路由出口（本节点的也在里面）没成，已一起换回：软路由「路由B」"), "{ferr}");
    assert_eq!((applies(23252).len(), applies(23253).len()), (1, 1));
    assert_eq!((entry_state(t, oe).await, entry_state(t, ne).await), ("active".to_string(), "active".to_string()), "旧入口都没摘");
    for id in &ids {
        assert_eq!(t.job(id).await["cancellable"], true);
        t.post(&format!("/api/v1/jobs/{id}/cancel"), json!({})).await;
        assert_eq!(run(t, &mut sims, id).await["state"], "cancelled");
    }
    let live: Vec<Uuid> = sqlx::query_scalar("select id from entry where node_id = any($1) and lifecycle = 'active' order by id").bind(vec![w.old, w.new]).fetch_all(&t.st.db).await.unwrap();
    let mut want = vec![oe, ne];
    want.sort();
    assert_eq!(live, want, "只剩旧入口在用");
}

/// 同批换线路机一起改、改回也失败：带头的停在「需要人工处理」，跟着的同样不能取消、给出 manual，单独重试照样跟着它；
/// 带头的重试改完之后，跟着的重试就跟着完成（不再动路由）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evacuate_batch_stuck_holds_followers() {
    let w = world().await;
    let oe = old_entry(&w).await;
    let ne: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(&w.t.st.db).await.unwrap();
    let a = router(&w, "路由A", 23173, &[("节点01", "198.51.100.2", 20003, Some(oe)), ("节点02", "198.51.100.2", 20020, Some(ne))]).await;
    router(&w, "路由B", 23174, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let t = &w.t;
    let (rb, tb) = seed_relay_b(t).await;
    std::fs::write(root(23174).join("mode"), "fail").unwrap();
    std::fs::write(root(23173).join("rollback-fail"), "").unwrap();
    let d = t.post(&format!("/api/v1/relays/{}/evacuate", w.relay), json!({"to_relay_id": rb})).await;
    let ids: Vec<String> = d["jobs"].as_array().unwrap().iter().map(|j| j["job_id"].as_str().unwrap().to_string()).collect();
    let mut sims = [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)];
    let (lj, fj) = (run(t, &mut sims, &ids[0]).await, run(t, &mut sims, &ids[1]).await);
    assert_eq!((lj["state"].as_str(), lj["cancellable"].as_bool()), (Some("failed"), Some(false)), "{lj}");
    assert_eq!((fj["state"].as_str(), fj["cancellable"].as_bool()), (Some("failed"), Some(false)), "{fj}");
    let manual = json!([{"id": a, "name": "路由A", "exits": ["节点01", "节点02"]}]);
    assert_eq!((lj["manual"]["routers"].clone(), fj["manual"]["routers"].clone()), (manual.clone(), manual), "{fj}");
    assert!(fj["error"].as_str().unwrap().contains("改回失败、需要人工处理") && fj["error"].as_str().unwrap().contains("先处理好并重试它那个作业"), "{fj}");
    // 跟着的单独重试：带头的还卡着，照样跟着失败，不自己动路由
    t.post(&format!("/api/v1/jobs/{}/retry", ids[1]), json!({})).await;
    let fj = run(t, &mut sims, &ids[1]).await;
    assert_eq!((fj["state"].as_str(), fj["cancellable"].as_bool()), (Some("failed"), Some(false)), "{fj}");
    assert_eq!(applies(23173).len(), 1);
    // 处理好后重试带头的：接着改完，跟着的重试就完成
    std::fs::remove_file(root(23173).join("rollback-fail")).unwrap();
    std::fs::write(root(23174).join("mode"), "ok").unwrap();
    t.post(&format!("/api/v1/jobs/{}/retry", ids[0]), json!({})).await;
    assert_eq!(run(t, &mut sims, &ids[0]).await["state"], "succeeded", "{}", logs(t, &ids[0]).await);
    t.post(&format!("/api/v1/jobs/{}/retry", ids[1]), json!({})).await;
    let fj = run(t, &mut sims, &ids[1]).await;
    assert_eq!(fj["state"], "succeeded", "{}", logs(t, &ids[1]).await);
    assert_eq!(fj["steps"][4]["output"]["routers"][0]["exits"][0]["exit"], "节点02", "{fj}");
    assert_eq!(applies(23173).len(), 2, "带头的接着改完那一次，跟着的不再动路由");
    assert_eq!((entry_state(t, oe).await, entry_state(t, ne).await), ("removed".to_string(), "removed".to_string()));
}

/// 同批换线路机改回失败（带头的和跟着的都抄着 routers_stuck）后强制删掉那台路由：带头的、跟着的都摘掉它，跟着的也记一条日志，「需要人工处理」消失
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_delete_router_clears_followers_stuck() {
    let w = world().await;
    let oe = old_entry(&w).await;
    let ne: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(&w.t.st.db).await.unwrap();
    let a = router(&w, "路由A", 23183, &[("节点01", "198.51.100.2", 20003, Some(oe)), ("节点02", "198.51.100.2", 20020, Some(ne))]).await;
    router(&w, "路由B", 23184, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let t = &w.t;
    let (rb, tb) = seed_relay_b(t).await;
    std::fs::write(root(23184).join("mode"), "fail").unwrap();
    std::fs::write(root(23183).join("rollback-fail"), "").unwrap();
    let d = t.post(&format!("/api/v1/relays/{}/evacuate", w.relay), json!({"to_relay_id": rb})).await;
    let ids: Vec<String> = d["jobs"].as_array().unwrap().iter().map(|j| j["job_id"].as_str().unwrap().to_string()).collect();
    let mut sims = [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)];
    let (lj, fj) = (run(t, &mut sims, &ids[0]).await, run(t, &mut sims, &ids[1]).await);
    let manual = json!([{"id": a, "name": "路由A", "exits": ["节点01", "节点02"]}]);
    assert_eq!((lj["manual"]["routers"].clone(), fj["manual"]["routers"].clone()), (manual.clone(), manual), "{fj}");
    let stuck = |job: &str| {
        let job: Uuid = job.parse().unwrap();
        let db = t.st.db.clone();
        async move { sqlx::query_scalar::<_, Value>("select params->'routers_stuck' from job where id = $1").bind(job).fetch_one(&db).await.unwrap() }
    };
    assert_eq!(stuck(&ids[1]).await.as_array().map(Vec::len), Some(1), "跟着的抄了带头的 routers_stuck");
    let (s, v) = t.req("DELETE", &format!("/api/v1/routers/{a}?force=true"), None, None).await;
    assert_eq!(s, axum::http::StatusCode::OK, "{v}");
    assert_eq!(v["data"]["jobs"], json!([ids[0]]), "{v}");
    assert_eq!((stuck(&ids[0]).await, stuck(&ids[1]).await), (json!([]), json!([])));
    let (lj, fj) = (t.job(&ids[0]).await, t.job(&ids[1]).await);
    assert_eq!((lj["manual"].clone(), fj["manual"].clone()), (Value::Null, Value::Null), "{lj} {fj}");
    assert!(logs(t, &ids[1]).await.contains("强制删除") && logs(t, &ids[1]).await.contains("路由A"), "{}", logs(t, &ids[1]).await);
    assert!(logs(t, &ids[0]).await.contains("改回放弃"), "{}", logs(t, &ids[0]).await);
    let au: Value = sqlx::query_scalar("select detail from audit_log where action = 'router.delete'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(au["follower_jobs"], json!([ids[1]]), "{au}");
}

/// 不同的作业（各自单独换线路机）改同一台路由时一个一个来：第二个走到改路由这一步时第一个正在改，
/// 它排队（不动路由、不算这一步的超时、能取消），第一个改完才轮到它
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn separate_jobs_take_turns_on_a_router() {
    let w = world().await;
    let oe = old_entry(&w).await;
    let ne: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(&w.t.st.db).await.unwrap();
    router(&w, "路由A", 23178, &[("节点01", "198.51.100.2", 20003, Some(oe)), ("节点02", "198.51.100.2", 20020, Some(ne))]).await;
    let t = &w.t;
    let (rb, tb) = seed_relay_b(t).await;
    let mut ids = vec![];
    for n in [w.old, w.new] {
        ids.push(t.post(&format!("/api/v1/machines/{n}/move"), json!({"to_relay_id": rb})).await["job_id"].as_str().unwrap().to_string());
    }
    // 先改路由A 的那个卡在改配置上（假 ssh 等 hold 删掉）
    std::fs::write(root(23178).join("hold"), "").unwrap();
    let mut sims = [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)];
    let mut held = vec![];
    let mut queued = (String::new(), Value::Null);
    for _ in 0..100 {
        drive_holding(t, &mut sims, 23178, &mut held).await;
        if let [(first, _)] = held.as_slice() {
            let other = ids.iter().find(|i| *i != first).unwrap().clone();
            let j = t.job(&other).await;
            if j["step"] == 4 && j["state"] == "waiting" {
                queued = (other, j);
                break;
            }
        }
    }
    let (second, j) = &queued;
    assert!(held.len() == 1 && j["steps"][4]["state"] == "running", "{j} {}", logs(t, second).await);
    assert!(j["steps"][4]["started_at"].is_null() && j["cancellable"] == true, "排队不算这一步的时间、还能取消：{j}");
    assert_eq!(applies(23178).len(), 1, "后到的没碰路由");
    let reads = read(root(23178).join("reads")).lines().count();
    // 再领几次照样排队，不读路由的配置；日志只写一次
    for _ in 0..2 {
        t.nudge_jobs().await;
        while let Some(j) = jobs::claim(&t.st, "test").await.unwrap() {
            jobs::advance(&t.st, j, "test").await.unwrap();
        }
    }
    assert_eq!(logs(t, second).await.matches("软路由「路由A」正在被别的作业改，排队等它改完").count(), 1);
    assert_eq!(applies(23178).len(), 1);
    assert_eq!(read(root(23178).join("reads")).lines().count(), reads, "排队时只等锁");
    std::fs::remove_file(root(23178).join("hold")).unwrap();
    let (first, h) = held.pop().unwrap();
    h.await.unwrap();
    for id in [&first, second] {
        assert_eq!(run(t, &mut sims, id).await["state"], "succeeded", "{}", logs(t, id).await);
    }
    let name = |id: &str| if id == ids[0] { "节点01" } else { "节点02" };
    let a = applies(23178);
    assert!(a.len() == 2 && a[0].contains(&format!("NAME_1='{}'\n", name(&first))) && a[1].contains(&format!("NAME_1='{}'\n", name(second))), "{a:?}");
    assert_eq!(t.job(second).await["steps"][4]["state"], "done");
}

/// 中控升级前建的作业（步骤是旧定义的）：按步骤 key 对齐现在的定义。第 45 轮之前的换线路机只有 7 步（没有改软路由出口），
/// 还没摘旧入口的接着做时照做新插入的这一步；对不上的（失败停着的那一步已经没有了）作业失败、写清原因，不停在运行中
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_jobs_align_by_step_key() {
    let w = world().await;
    let oe = old_entry(&w).await;
    router(&w, "路由A", 23191, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let t = &w.t;
    let (rb, tb) = seed_relay_b(t).await;
    let d = t.post(&format!("/api/v1/machines/{}/move", w.old), json!({"to_relay_id": rb})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    let id = Uuid::parse_str(&job).unwrap();
    // 改回旧定义：去掉 routers（idx 4），后面的往前挪；参数也同旧作业（没有 routers / plan）
    let db = &t.st.db;
    sqlx::query("delete from job_step where job_id = $1 and key = 'routers'").bind(id).execute(db).await.unwrap();
    // 主键 (job_id, idx)：先挪到负数再挪回来
    sqlx::query("update job_step set idx = -idx where job_id = $1 and idx > 4").bind(id).execute(db).await.unwrap();
    sqlx::query("update job_step set idx = -idx - 1 where job_id = $1 and idx < 0").bind(id).execute(db).await.unwrap();
    sqlx::query("update job set params = params - 'routers' - 'plan' where id = $1").bind(id).execute(db).await.unwrap();
    let j = run(t, &mut [Sim::new(RELAY_TOKEN.into()), Sim::new(tb)], &job).await;
    assert_eq!(j["state"], "succeeded", "{j} {}", logs(t, &job).await);
    let keys: Vec<&str> = j["steps"].as_array().unwrap().iter().map(|s| s["key"].as_str().unwrap()).collect();
    assert_eq!(keys, jobs::defs("move_entry").iter().map(|d| d.key).collect::<Vec<_>>());
    assert_eq!(j["steps"][4]["state"], "done");
    assert!(logs(t, &job).await.contains("按步骤名对齐：新增的「改软路由出口」照做"));
    assert_eq!(applies(23191).len(), 1, "按路由现在的配置改了出口");
    assert_eq!(entry_state(t, oe).await, "removed");

    // 失败停着的那一步在现在的定义里已经没有了：重试后作业失败、写清原因
    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new, "routers": false})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    let id = Uuid::parse_str(&job).unwrap();
    sqlx::query("update job_step set idx = -idx - 1 where job_id = $1").bind(id).execute(db).await.unwrap();
    sqlx::query("update job_step set idx = -idx where job_id = $1").bind(id).execute(db).await.unwrap();
    sqlx::query("insert into job_step (job_id, idx, key, state) values ($1, 0, 'gone', 'failed')").bind(id).execute(db).await.unwrap();
    drive(t).await;
    let j = t.job(&job).await;
    assert_eq!(j["state"], "failed", "{j}");
    assert!(j["error"].as_str().unwrap().contains("作业定义已变：发起时的步骤「gone」已经没有了，而它失败停着：这个作业没法接着做，请重新发起"), "{j}");
    assert_eq!(name_of(t, w.old).await, "团队2-01", "一步都没做");
}

/// 重新编号预览：指向要改名的节点的软路由出口列出来（出口名是路由上自己起的，不跟着改）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renumber_preview_lists_router_exits() {
    let w = world().await;
    let t = &w.t;
    sqlx::query("update machine set number = 3 where id = $1").bind(w.new).execute(&t.st.db).await.unwrap();
    let ne: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(&t.st.db).await.unwrap();
    router(&w, "路由A", 23181, &[("节点03", "198.51.100.2", 20020, Some(ne))]).await;
    let p = t.get(&format!("/api/v1/teams/{}/renumber", w.team)).await;
    assert_eq!(p["changes"][0]["new_name"], "团队2-02", "{p}");
    assert_eq!(p["notes"], json!(["软路由「路由A」的出口「节点03」指向 团队2-03，改名为 团队2-02 后出口名不跟着改"]));
}

/// 改到一半连接断了、改回也失败：作业不能取消、给出 manual（详情与列表）。路由还连不上时重试：没核对过路由，照样不能取消，
/// 也不说「软路由都是原样」。路由恢复后重试：照第一次记下的出口接着改完（不重读配置，备份戳沿用第一次的）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn half_done_router_stays_protected_until_verified() {
    let w = world().await;
    let oe = old_entry(&w).await;
    let a = router(&w, "路由A", 23201, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let t = &w.t;
    std::fs::write(root(23201).join("mode"), "drop").unwrap();
    std::fs::write(root(23201).join("rollback-fail"), "").unwrap();
    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    drive(t).await;
    let j = t.job(&job).await;
    let err = j["error"].as_str().unwrap();
    assert!(
        err.contains("软路由「路由A」：经隧道的连接中途断了（ssh 退出码 255，没有输出），已经备份 /etc/fleet-backup/openclash/c.yaml.x，可能改了一半；没能改回、需要人工处理：路由A（写回备份失败）"),
        "{err}"
    );
    assert!(!err.contains("：；") && !err.contains("都是原样"), "{err}");
    assert_eq!((j["state"].as_str(), j["cancellable"].as_bool()), (Some("failed"), Some(false)), "{j}");
    let manual = json!([{"id": a, "name": "路由A", "exits": ["节点01"]}]);
    assert_eq!(j["manual"]["routers"], manual, "{j}");
    let list = t.get("/api/v1/jobs").await;
    assert_eq!(list["items"][0]["manual"], j["manual"], "列表里同样带上：{list}");
    let stamp1 = read(root(23201).join("apply-1.sh")).lines().find_map(|l| l.strip_prefix("STAMP='").map(|s| s.trim_end_matches('\'').to_string())).unwrap();

    // 路由还连不上时重试：照样不能取消，也不说「软路由都是原样」
    sqlx::query("update router_status set reachable = false, error = '经隧道连不上路由：连接被拒绝（隧道没连上）' where router_id = $1").bind(a).execute(&t.st.db).await.unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    assert_eq!(t.job(&job).await["cancellable"], false, "重试之后、核对路由之前也不能取消");
    drive(t).await;
    let j = t.job(&job).await;
    let err = j["error"].as_str().unwrap();
    assert!(err.contains("软路由 路由A（经隧道连不上路由：连接被拒绝（隧道没连上）） 不在线；没能改回、需要人工处理"), "{err}");
    assert!(!err.contains("都是原样") && !err.contains("一台都没有改"), "{err}");
    assert_eq!((j["state"].as_str(), j["cancellable"].as_bool(), j["manual"]["routers"].clone()), (Some("failed"), Some(false), manual), "{j}");
    assert_eq!(applies(23201).len(), 1, "不在线的不去改");
    let p: Value = sqlx::query_scalar("select params from job where id = $1").bind(Uuid::parse_str(&job).unwrap()).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(p["routers_applied"][0]["stamp"], json!(stamp1), "备份戳沿用第一次的：{p}");

    // 路由恢复后重试：接着改完
    sqlx::query("update router_status set reachable = true, error = null where router_id = $1").bind(a).execute(&t.st.db).await.unwrap();
    std::fs::remove_file(root(23201).join("rollback-fail")).unwrap();
    std::fs::write(root(23201).join("mode"), "ok").unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    drive(t).await;
    let j = t.job(&job).await;
    assert_eq!((j["state"].as_str(), j["manual"].is_null()), (Some("succeeded"), true), "{j} {}", logs(t, &job).await);
    let ap = applies(23201);
    assert!(ap.len() == 2 && ap[1].contains("NAME_1='节点01'\nOSRV_1='198.51.100.2'\nOPORT_1='20003'\nSRV_1='198.51.100.2'\nPORT_1='20020'"), "{ap:?}");
    assert_eq!(read(root(23201).join("reads")).lines().count(), 1, "接着做的不重读配置");
    let p: Value = sqlx::query_scalar("select params from job where id = $1").bind(Uuid::parse_str(&job).unwrap()).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((p["routers_applied"].clone(), p["routers_stuck"].clone()), (json!([]), json!([])), "这一步做完才清掉：{p}");
    assert_eq!(name_of(t, w.new).await, "团队2-01");
}

/// 接着做的那台（上一次改到一半、没改回）重做时新入口已轮换过凭据，这次脚本确定没写进去（等不到路由上的锁、核对没过一行没写）：
/// 记录换回上一次写的那组，改回照它核对（按这次的核对对不上路由上的，会当成别人改过而跳过、作业却报「软路由都是原样」）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redo_not_written_keeps_previous_params() {
    let w = world().await;
    let oe = old_entry(&w).await;
    router(&w, "路由A", 23261, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let t = &w.t;
    std::fs::write(root(23261).join("mode"), "drop").unwrap();
    std::fs::write(root(23261).join("rollback-fail"), "").unwrap();
    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    let jid = Uuid::parse_str(&job).unwrap();
    drive(t).await;
    assert!(t.job(&job).await["error"].as_str().unwrap().contains("需要人工处理"));
    let rotated = "33333333-3333-4333-8333-333333333333";
    sqlx::query("update entry set client_uuid = $2::uuid where node_id = $1").bind(w.new).bind(rotated).execute(&t.st.db).await.unwrap();
    std::fs::remove_file(root(23261).join("rollback-fail")).unwrap();
    let written = || async { sqlx::query_scalar::<_, Option<String>>("select params->'routers_applied'->0->'written'->0->>'uuid' from job where id = $1").bind(jid).fetch_one(&t.st.db).await.unwrap() };
    // 等不到路由上的锁：这次的（轮换后的）没写进去，记录还是上一次的
    std::fs::write(root(23261).join("mode"), "busy").unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    drive(t).await;
    assert_eq!(t.job(&job).await["state"], "waiting");
    assert!(applies(23261)[1].contains(&format!("UUID_1='{rotated}'")));
    assert_eq!(written().await.as_deref(), Some(NEW_UUID));
    // 核对没过、一行没写：改回带的是上一次写的 uuid
    std::fs::write(root(23261).join("mode"), "refuse").unwrap();
    rerun(t, jid).await;
    let j = t.job(&job).await;
    assert!(j["error"].as_str().unwrap().contains("已改回原配置：路由A。软路由都是原样"), "{j}");
    assert_eq!(applies(23261).len(), 3);
    assert_eq!(read(root(23261).join("rollback-uuid")), format!("{NEW_UUID}\n"), "按上一次写的核对");
}

/// 执行时出口对不上新入口（发起之后新节点的入口变了；路由C 发起时的快照里没有出口指向旧节点）：动手之前作业失败，
/// 原因与预览一致（预览里 blocker = 「软路由「名」的出口「名」：」+ 那条出口的 error），一台都没改、能取消
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exit_mismatch_fails_before_any_change() {
    let w = world().await;
    let oe = old_entry(&w).await;
    router(&w, "路由A", 23211, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    router(&w, "路由C", 23212, &[]).await;
    config(23212, &[("x", "198.51.100.2", 20003)]);
    let t = &w.t;
    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    // 发起之后：新节点的入口挪到线路机3，另在线路机4 上加一个——旧入口所在的线路机上没有，不知道换成哪个
    let db = &t.st.db;
    let (r3, r4) = (Uuid::now_v7(), Uuid::now_v7());
    for (id, label, ip, host) in [(r3, "线路机3", "198.51.100.3", "r3.test"), (r4, "线路机4", "198.51.100.4", "r4.test")] {
        sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle) values ($1, 'relay', $2, $3::inet, 'active')").bind(id).bind(label).bind(ip).execute(db).await.unwrap();
        sqlx::query("insert into relay (machine_id, public_host, reality_private_enc, reality_public_key) values ($1, $2, '\\x00', 'PBK')").bind(id).bind(host).execute(db).await.unwrap();
    }
    sqlx::query("update entry set relay_id = $2, port = 20040 where node_id = $1").bind(w.new).bind(r3).execute(db).await.unwrap();
    entry(t, r4, w.new, 20050, "44444444-4444-4444-8444-444444444444", "active").await;
    let p = t.get(&format!("/api/v1/machines/{}/replace?new_machine_id={}", w.old, w.new)).await;
    let x = &p["routers"][0]["exits"][0];
    let blocker = format!("软路由「路由A」的出口「节点01」：{}", x["error"].as_str().unwrap());
    assert!(p["blockers"].as_array().unwrap().iter().any(|b| b == &json!(blocker)), "{p}");
    drive(t).await;
    let j = t.job(&job).await;
    let err = j["error"].as_str().unwrap();
    assert!(err.contains(&blocker) && err.contains("；软路由「路由C」的出口「x」：") && err.contains("不知道换成哪个"), "{err}");
    assert!(err.contains("，一台都没有改。软路由都是原样，编号没改") && !err.contains("配置读不了"), "{err}");
    assert!(applies(23211).is_empty() && applies(23212).is_empty());
    assert_eq!((j["state"].as_str(), j["cancellable"].as_bool(), j["manual"].is_null()), (Some("failed"), Some(true), true), "{j}");
    assert_eq!(name_of(t, w.old).await, "团队2-01");
}

/// 配置读不了的路由：预览不分团队都提醒；执行时哪怕快照里看不出它有出口要改，也一台都不改、作业失败（提示可以不改软路由）。
/// 路由上根本没有源配置的算没有出口，照常做完
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unreadable_router_config_blocks_change() {
    let w = world().await;
    let oe = old_entry(&w).await;
    router(&w, "路由A", 23221, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let b = router(&w, "别队路由", 23222, &[]).await;
    let t = &w.t;
    let other = seed_team(t, "团队9").await;
    sqlx::query("update router set team_id = $2 where id = $1").bind(b).bind(other).execute(&t.st.db).await.unwrap();
    sqlx::query("update router_status set data = $2 where router_id = $1")
        .bind(b)
        .bind(json!({"exits": [], "openclash": {"config_error": "配置文件解析不了：x"}}))
        .execute(&t.st.db)
        .await
        .unwrap();
    let p = t.get(&format!("/api/v1/machines/{}/replace?new_machine_id={}", w.old, w.new)).await;
    assert!(p["warnings"].to_string().contains("软路由「别队路由」的配置读不了（配置文件解析不了：x），看不出它有没有用旧节点；执行时还读不了，作业会失败、一台都不改"), "{p}");
    assert!(!t.get(&format!("/api/v1/machines/{}/replace?new_machine_id={}&routers=false", w.old, w.new)).await["warnings"].to_string().contains("别队路由"));
    std::fs::write(root(23222).join("read-fail"), "").unwrap();
    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    drive(t).await;
    let j = t.job(&job).await;
    let err = j["error"].as_str().unwrap();
    assert!(err.contains("软路由「别队路由」的配置读不了：读配置失败了。看不出它有没有出口指向旧节点") && err.contains("--no-routers") && err.contains("一台都没有改"), "{err}");
    assert!(applies(23221).is_empty(), "一台都不改");
    // 路由上没有源配置：没有出口，照常改完
    std::fs::remove_file(root(23222).join("read-fail")).unwrap();
    std::fs::write(root(23222).join("no-config"), "").unwrap();
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    drive(t).await;
    let j = t.job(&job).await;
    assert_eq!(j["state"], "succeeded", "{j} {}", logs(t, &job).await);
    assert_eq!(applies(23221).len(), 1);
    assert!(logs(t, &job).await.contains("软路由「别队路由」：OpenClash 的源配置（uci openclash.config.config_path）不存在：没有设置，没有出口要改"));
}

/// 改出口指向：直出出口从线路机2 的直出入口换到线路机3 的直出入口，只改选中的路由，两个入口都不动；
/// 改的时候连接断了（重启 OpenClash 掐断了隧道）：等回连后取回路由上的结果，照常算改好了
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repoint_direct_exit_to_other_relay() {
    let w = world().await;
    let t = &w.t;
    let (relay_b, _) = seed_relay_b(t).await;
    let d1 = entry(t, w.relay, w.relay, 20010, "33333333-3333-4333-8333-333333333333", "active").await;
    let d2 = entry(t, relay_b, relay_b, 20030, NEW_UUID, "active").await;
    let ra = router(&w, "团队2-路由A", 23401, &[("节点00-默认", "198.51.100.2", 20010, Some(d1))]).await;
    router(&w, "团队2-路由B", 23402, &[("节点00-默认", "198.51.100.2", 20010, Some(d1))]).await;
    // 不选路由：两台都列出
    let p = t.get(&format!("/api/v1/entries/{d1}/repoint?to_entry_id={d2}")).await;
    assert_eq!((p["ok"].as_bool(), p["routers"].as_array().map(Vec::len)), (Some(true), Some(2)), "{p}");
    assert_eq!((p["old"]["name"].as_str(), p["new"]["name"].as_str()), (Some("线路机2-直出 · 线路机2:20010"), Some("线路机3-直出 · 线路机3:20030")));
    // 只选路由A；出口写的是线路机 IP，换成新线路机的 IP
    let p = t.get(&format!("/api/v1/entries/{d1}/repoint?to_entry_id={d2}&routers={ra}")).await;
    assert_eq!(p["routers"].as_array().map(Vec::len), Some(1), "{p}");
    assert_eq!(p["routers"][0]["exits"][0]["to"], "198.51.100.3:20030");
    // 输入不对
    assert_eq!(t.req("POST", &format!("/api/v1/entries/{d1}/repoint"), Some(json!({"to_entry_id": d1})), None).await.0, StatusCode::BAD_REQUEST);

    std::fs::write(root(23401).join("mode"), "drop").unwrap();
    std::fs::write(root(23401).join("result"), "backup=/etc/fleet-backup/openclash/c.yaml.x\nchanged=6\ndelay_1=77\nreplace=ok\n").unwrap();
    let d = t.post(&format!("/api/v1/entries/{d1}/repoint"), json!({"to_entry_id": d2, "router_ids": [ra]})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    drive(t).await;
    let j = t.job(&job).await;
    let l = logs(t, &job).await;
    assert_eq!((j["state"].as_str(), j["kind"].as_str()), (Some("succeeded"), Some("repoint")), "{j} {l}");
    assert!(l.contains("隧道回连了，取回路由上的结果") && l.contains("77 ms"), "{l}");
    let a = applies(23401);
    assert_eq!(a.len(), 1);
    assert!(a[0].contains("OSRV_1='198.51.100.2'\nOPORT_1='20010'\nSRV_1='198.51.100.3'\nPORT_1='20030'") && a[0].contains(&format!("UUID_1='{NEW_UUID}'")), "{}", &a[0][..400]);
    assert!(applies(23402).is_empty(), "没选的路由不改");
    for e in [d1, d2] {
        assert_eq!(entry_state(t, e).await, "active", "入口都不动");
    }
    let au = t.get(&format!("/api/v1/audit?target_id={}", w.relay)).await;
    let rp = au.as_array().unwrap().iter().find(|a| a["action"] == "entry.repoint").unwrap();
    assert_eq!((rp["title"].as_str(), rp["summary"].as_str()), (Some("改出口指向"), Some("线路机2-直出 · 线路机2:20010 → 线路机3-直出 · 线路机3:20030（改 1 台软路由）")));
}

/// 改出口指向与换线路机同时发起：改出口指向对新入口所在的节点 for update，和换线路机对同一行的 for share 互斥。
/// 顺序模拟：换线路机的事务先锁住节点、建好要摘这个入口的作业但还没提交；改出口指向等它提交，之后看到这个作业被拒
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repoint_waits_for_concurrent_move() {
    let w = world().await;
    let t = &w.t;
    let old = old_entry(&w).await;
    let new: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(&t.st.db).await.unwrap();
    router(&w, "团队2-路由A", 23411, &[("节点01", "198.51.100.2", 20003, Some(old))]).await;
    let mut tx = t.st.db.begin().await.unwrap();
    sqlx::query("select 1 from machine where id = $1 for share").bind(w.new).execute(&mut *tx).await.unwrap();
    let ((code, body, answered), committed) = tokio::join!(
        async {
            let (s, v) = t.req("POST", &format!("/api/v1/entries/{old}/repoint"), Some(json!({"to_entry_id": new})), None).await;
            (s, v, std::time::Instant::now())
        },
        async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            sqlx::query("insert into job (id, kind, target_id, params, created_by) values ($1, 'move_entry', $2, $3, 'test')")
                .bind(Uuid::now_v7())
                .bind(w.new)
                .bind(json!({"old_entry_id": new}))
                .execute(&mut *tx)
                .await
                .unwrap();
            let at = std::time::Instant::now();
            tx.commit().await.unwrap();
            at
        }
    );
    assert!(answered > committed, "改出口指向没有等换线路机的事务提交：{code} {body}");
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("进行中的作业"), "{body}");
    let n: i64 = sqlx::query_scalar("select count(*) from job where kind = 'repoint'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 0);
}

/// 两台路由（隧道端口 `pa`、`pb`）的改出口指向：A 改好、B 设成等不到路由上的锁（BUSY）。返回 (作业 id, A, B)
async fn busy_world(w: &W, pa: i32, pb: i32) -> (String, Uuid, Uuid) {
    let t = &w.t;
    let (relay_b, _) = seed_relay_b(t).await;
    let d1 = entry(t, w.relay, w.relay, 20010, "33333333-3333-4333-8333-333333333333", "active").await;
    let d2 = entry(t, relay_b, relay_b, 20030, NEW_UUID, "active").await;
    let ra = router(w, "团队2-路由A", pa, &[("节点00-默认", "198.51.100.2", 20010, Some(d1))]).await;
    let rb = router(w, "团队2-路由B", pb, &[("节点00-默认", "198.51.100.2", 20010, Some(d1))]).await;
    assert!(ra < rb, "按 id 顺序改：A 在前");
    std::fs::write(root(pb).join("mode"), "busy").unwrap();
    let d = t.post(&format!("/api/v1/entries/{d1}/repoint"), json!({"to_entry_id": d2})).await;
    (d["job_id"].as_str().unwrap().to_string(), ra, rb)
}

/// 等待中的作业马上再领一次
async fn rerun(t: &T, job: Uuid) {
    sqlx::query("update job set run_after = now() where id = $1").bind(job).execute(&t.st.db).await.unwrap();
    drive(t).await;
}

/// 路由上等不到上一份脚本的锁（BUSY）：不改回已改好的、不失败，作业等着隔一分钟再来；改好的那台留在 routers_applied（标了确认改好），
/// 等锁的那台（这次才记进去、一行没写）不留。再来时确认改好的 A 不再改（不再备份、重启 OpenClash），只重试 B；锁放开后接着做完，
/// 步骤输出里两台都在
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn router_busy_waits_and_retries() {
    let w = world().await;
    let t = &w.t;
    let (job, ra, rb) = busy_world(&w, 23421, 23422).await;
    drive(t).await;
    let j = t.job(&job).await;
    let l = logs(t, &job).await;
    assert_eq!(j["state"], "waiting", "{j} {l}");
    assert!(l.contains("60 秒内还没跑完") && l.contains("60 秒后再试"), "{l}");
    assert!(!read(root(23421).join("rollback.log")).contains("节点00-默认"), "改好的 A 不改回");
    let jid = Uuid::parse_str(&job).unwrap();
    let marked: Vec<(String, bool)> = sqlx::query_as("select a->>'id', a->'done' is not null and a->'done' <> 'null' from job j, jsonb_array_elements(j.params->'routers_applied') a where j.id = $1")
        .bind(jid)
        .fetch_all(&t.st.db)
        .await
        .unwrap();
    assert_eq!(marked, vec![(ra.to_string(), true)]);
    // B 还是等不到：A 不再改，只有 B 又试了一次
    rerun(t, jid).await;
    assert_eq!(t.job(&job).await["state"], "waiting");
    assert_eq!((applies(23421).len(), applies(23422).len()), (1, 2));
    // 锁放开了：B 这次改好，A 仍只改过一次
    std::fs::remove_file(root(23422).join("mode")).unwrap();
    rerun(t, jid).await;
    let j = t.job(&job).await;
    assert_eq!(j["state"], "succeeded", "{j} {}", logs(t, &job).await);
    assert_eq!((applies(23421).len(), applies(23422).len()), (1, 3));
    let ids: Vec<&str> = j["steps"][0]["output"]["routers"].as_array().unwrap().iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(ids, vec![ra.to_string(), rb.to_string()], "{j}");
    let left: Value = sqlx::query_scalar("select params->'routers_applied' from job where id = $1").bind(jid).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(left, json!([]));
    let links: i64 = sqlx::query_scalar("select count(*) from router_exit_link where router_id = any($1)").bind(vec![ra, rb]).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(links, 2, "两台都记下改成了指向哪个入口");
}

/// B 一直等不到路由上的锁，再等一轮就超过这一步的超时：不再等，把改好的 A 改回，作业失败、原因写明等不到路由上的锁；路由都是原样，可以取消
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn router_busy_past_timeout_rolls_back() {
    let w = world().await;
    let t = &w.t;
    let (job, _, _) = busy_world(&w, 23431, 23432).await;
    drive(t).await;
    assert_eq!(t.job(&job).await["state"], "waiting");
    let jid = Uuid::parse_str(&job).unwrap();
    // 这一步已经开始了 1750 秒（超时 1800）：再等 60 秒就超了
    sqlx::query("update job_step set started_at = now() - interval '1750 seconds' where job_id = $1 and idx = 0").bind(jid).execute(&t.st.db).await.unwrap();
    rerun(t, jid).await;
    let j = t.job(&job).await;
    assert_eq!(j["state"], "failed", "{j} {}", logs(t, &job).await);
    let err = j["error"].as_str().unwrap();
    assert!(err.contains("软路由「团队2-路由B」：路由上上一次改出口的脚本（pid 1）60 秒内还没跑完，这次没有动配置；这一步等了") && err.contains("还等不到路由上的锁（超时 1800 秒）"), "{err}");
    assert!(err.contains("已改回原配置：团队2-路由A。软路由都是原样"), "{err}");
    assert!(read(root(23431).join("rollback.log")).contains("节点00-默认 198.51.100.3 20030"), "A 改回本作业写的那个出口");
    assert_eq!((applies(23431).len(), applies(23432).len()), (1, 2));
    assert_eq!(j["cancellable"], true, "{j}");
}

/// 出口参数和入口不一致（凭据轮换后没同步）：持续两轮采集（满 30 秒）才开警告，告警带建议操作 fleetctl router sync；
/// 同步作业（目标是路由）把它改成入口现在的参数、server:port 不变，客户自己的出口不动；同步好之后满 60 秒恢复。
/// 路由上有人加的、指向别的团队入口的出口（从没对上过）：不进默认同步，点名同步被拒，要 force
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drift_alert_and_sync() {
    use fleet_server::routers::alerts;
    let w = world().await;
    let t = &w.t;
    let oe = old_entry(&w).await;
    // 团队3 的入口：路由上有人加了一个出口指向它（线路机:端口连号好猜），凭据乱填
    let t3 = seed_team(t, "团队3").await;
    let n3 = node(t, t3, 1, "203.0.113.31").await;
    let e3 = entry(t, w.relay, n3, 20031, "33333333-3333-4333-8333-333333333333", "active").await;
    let ra = router(&w, "团队2-路由S", 23501, &[("节点01", "relay.test", 20003, Some(oe)), ("直连", "203.0.113.99", 8388, None), ("伪造", "relay.test", 20031, Some(e3))]).await;
    // 节点01 是中控作业写过的对应（线路机另一个出口 伪造 没有）
    sqlx::query("insert into router_exit_link (router_id, exit, entry_id, source) values ($1, '节点01', $2, 'replace')").bind(ra).bind(oe).execute(&t.st.db).await.unwrap();
    let mark = |sync: &'static str| {
        sqlx::query("update router_status set data = jsonb_set(jsonb_set(data, '{exits,0,sync}', to_jsonb($2::text)), '{exits,2,sync}', to_jsonb($2::text)) where router_id = $1").bind(ra).bind(sync)
    };
    mark("drift").execute(&t.st.db).await.unwrap();
    sqlx::query("update router_status set data = jsonb_set(data, '{exits,0,linked}', 'true') where router_id = $1").bind(ra).execute(&t.st.db).await.unwrap();
    let firing = || async { t.get("/api/v1/incidents?state=firing").await["items"].as_array().cloned().unwrap_or_default() };
    let mut st = alerts::AlertState::default();
    let t0 = time::OffsetDateTime::now_utc();
    alerts::evaluate(&t.st, &mut st, t0).await.unwrap();
    assert!(firing().await.is_empty(), "第一轮不开");
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(31)).await.unwrap();
    let inc = firing().await;
    assert_eq!((inc.len(), inc[0]["rule"].as_str(), inc[0]["severity"].as_str()), (1, Some("router_exit_drift"), Some("warning")), "{inc:?}");
    assert_eq!(inc[0]["summary"], "团队2-路由S：2 个出口的参数和对应的面板入口不一致：节点01（x，凭据和入口现在的不一样）、伪造（x，凭据和入口现在的不一样；从未对上过，未自动同步）");
    assert_eq!(inc[0]["suggested"], json!(["fleetctl router sync 团队2-路由S"]));
    // 预览：server:port 不变；发起后作业目标是路由
    let p = t.get(&format!("/api/v1/routers/{ra}/sync")).await;
    assert_eq!((p["ok"].as_bool(), &p["routers"][0]["exits"][0]["from"], &p["routers"][0]["exits"][0]["to"]), (Some(true), &json!("relay.test:20003"), &json!("relay.test:20003")), "{p}");
    assert_eq!((p["routers"][0]["exits"].as_array().map(Vec::len), &p["untrusted"]), (Some(1), &json!([{"exit": "伪造", "entry": "x"}])), "伪造出口不进默认同步：{p}");
    // 点名同步别的团队的入口：拒绝，说明原因；force 才放行
    let n = t.get(&format!("/api/v1/routers/{ra}/sync?exits=伪造")).await;
    assert_eq!(n["ok"], false, "{n}");
    assert!(n["blockers"][0].as_str().unwrap().contains("别的团队的入口") && n["blockers"][0].as_str().unwrap().contains("--force"), "{n}");
    assert_eq!(n["need_force"][0]["why"], "别的团队的入口");
    let (c, _) = t.req("POST", &format!("/api/v1/routers/{ra}/sync"), Some(json!({"exits": ["伪造"]})), None).await;
    assert_eq!(c.as_u16(), 409);
    assert_eq!(t.get(&format!("/api/v1/routers/{ra}/sync?exits=伪造&force=true")).await["ok"], true);
    let d = t.post(&format!("/api/v1/routers/{ra}/sync"), json!({})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    drive(t).await;
    let j = t.job(&job).await;
    assert_eq!((j["state"].as_str(), j["kind"].as_str(), j["target"]["name"].as_str()), (Some("succeeded"), Some("router_sync"), Some("团队2-路由S")), "{j} {}", logs(t, &job).await);
    let a = applies(23501);
    assert_eq!(a.len(), 1);
    assert!(a[0].contains("N='1'\n") && a[0].contains("OSRV_1='relay.test'\nOPORT_1='20003'\nSRV_1='relay.test'\nPORT_1='20003'\nUUID_1='11111111-1111-4111-8111-111111111111'"), "{}", &a[0][..400]);
    let au = t.get(&format!("/api/v1/audit?target_id={ra}")).await;
    assert_eq!((au[0]["title"].as_str(), au[0]["summary"].as_str()), (Some("同步软路由出口"), Some("团队2-路由S 的 节点01")));
    let bogus: i64 = sqlx::query_scalar("select count(*) from router_exit_link where router_id = $1 and exit = '伪造'").bind(ra).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(bogus, 0, "没改过它，也不记对应");
    // 一致了：满 60 秒恢复
    mark("ok").execute(&t.st.db).await.unwrap();
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(40)).await.unwrap();
    assert_eq!(firing().await.len(), 1);
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(101)).await.unwrap();
    assert!(firing().await.is_empty());

    // 发起到执行之间入口不在用了：跳过这个出口、不让作业失败；要同步的全被跳过则成功但没改动，原因记在日志里
    mark("drift").execute(&t.st.db).await.unwrap();
    let job = t.post(&format!("/api/v1/routers/{ra}/sync"), json!({"exits": ["节点01"]})).await["job_id"].as_str().unwrap().to_string();
    sqlx::query("update entry set lifecycle = 'disabled' where id = $1").bind(oe).execute(&t.st.db).await.unwrap();
    drive(t).await;
    let j = t.job(&job).await;
    assert_eq!(j["state"].as_str(), Some("succeeded"), "{j} {}", logs(t, &job).await);
    assert!(logs(t, &job).await.contains("跳过"), "{}", logs(t, &job).await);
    assert_eq!(applies(23501).len(), 1, "没有再改路由");
}

/// 交换编号时，旧节点单独设的访问策略（已发布的与草稿）和客户端订阅的「排除 / 另加」跟着编号转给新节点：
/// 预览列出这一条，替换后策略在新节点上（新节点自己原有的被覆盖、日志里写明），旧节点不再有节点级策略，线路机代数往前走
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replace_moves_node_policy_and_sub_settings() {
    let w = world().await;
    let t = &w.t;
    let db = &t.st.db;
    let ins = |id: Uuid, mode: &'static str, domains: Vec<&'static str>, draft: Option<Value>| {
        sqlx::query("insert into policy (scope, scope_id, mode, domains, draft) values ('node', $1, $2, $3::text[], $4)")
            .bind(id)
            .bind(mode)
            .bind(domains.into_iter().map(String::from).collect::<Vec<_>>())
            .bind(draft)
    };
    ins(w.old, "denylist", vec!["bad.com"], Some(json!({"mode": "allowlist", "domains": ["ok.com"], "note": ""}))).execute(db).await.unwrap();
    ins(w.new, "blocked", vec![], None).execute(db).await.unwrap();
    for (name, ex, inc) in [("家里", vec![w.old], vec![]), ("公司", vec![], vec![w.old, w.new]), ("无关", vec![w.new], vec![])] {
        sqlx::query("insert into client_sub (id, name, token_hash, exclude_ids, include_ids) values ($1, $2, decode(md5($2), 'hex'), $3, $4)")
            .bind(Uuid::now_v7())
            .bind(name)
            .bind(ex)
            .bind(inc)
            .execute(db)
            .await
            .unwrap();
    }
    // 替换前的历史版本：v1 还没有任何节点级策略；v2 只有旧节点；v3 旧节点和新节点都有
    let snap = |items: Vec<Value>| Value::Array(items);
    let item = |id: Uuid, mode: &str, domains: Vec<&str>| json!({"scope": "node", "scope_id": id, "mode": mode, "domains": domains, "note": ""});
    for s in [snap(vec![]), snap(vec![item(w.old, "denylist", vec!["bad.com"])]), snap(vec![item(w.old, "denylist", vec!["bad.com"]), item(w.new, "blocked", vec![])])] {
        sqlx::query("insert into policy_version (id, kind, actor, snapshot) values ($1, 'publish', 'test', $2)").bind(Uuid::now_v7()).bind(s).execute(db).await.unwrap();
    }
    // 旧节点见过的目标跟到新节点：首次出现取早、最近出现取晚、次数相加
    sqlx::query(
        "insert into access_target_seen (node_id, dest_host, first_seen, last_seen, total) values
           ($1, 'a.com', '2026-09-01T00:00:00Z', '2026-10-20T00:00:00Z', 10), ($1, 'b.com', '2026-09-05T00:00:00Z', '2026-10-01T00:00:00Z', 4),
           ($2, 'a.com', '2026-10-10T00:00:00Z', '2026-10-25T00:00:00Z', 5)",
    )
    .bind(w.old)
    .bind(w.new)
    .execute(db)
    .await
    .unwrap();
    let p = t.get(&format!("/api/v1/machines/{}/replace?new_machine_id={}", w.old, w.new)).await;
    let ws = p["warnings"].to_string();
    assert!(ws.contains("单独设的访问策略（黑名单 · 1 个域名，另有未发布的草稿）和客户端订阅的排除 / 另加（公司、家里）随编号转给新节点「团队2-02」"), "{ws}");
    assert!(ws.contains("新节点自己单独设的访问策略（暂停新连接）会被覆盖"), "{ws}");
    // 都没有时不提
    let gen = || async { sqlx::query_scalar::<_, i64>("select generation from relay where machine_id = $1").bind(w.relay).fetch_one(db).await.unwrap() };
    let g0 = gen().await;

    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    drive(t).await;
    let j = t.job(&job).await;
    let l = logs(t, &job).await;
    assert_eq!(j["state"], "succeeded", "{j} {l}");
    assert!(l.contains("访问策略随编号转给新节点：黑名单 · 1 个域名，另有未发布的草稿（新节点自己单独设的「暂停新连接」被覆盖）"), "{l}");
    assert!(l.contains("客户端订阅的排除 / 另加改指新节点：公司、家里"), "{l}");
    assert!(gen().await > g0, "线路机期望状态代数要往前走");
    let rows: Vec<(Uuid, String, Option<String>)> = sqlx::query_as("select scope_id, mode, draft->>'mode' from policy where scope = 'node'").fetch_all(db).await.unwrap();
    assert_eq!(rows, [(w.new, "denylist".to_string(), Some("allowlist".to_string()))], "旧节点不再有节点级策略，草稿跟着走");
    // 目标首次出现并到新节点（旧节点的留着）
    let seen: Vec<(String, String, String, i64)> =
        sqlx::query_as("select dest_host, first_seen::date::text, last_seen::date::text, total from access_target_seen where node_id = $1 order by dest_host").bind(w.new).fetch_all(db).await.unwrap();
    assert_eq!(
        seen,
        [("a.com".to_string(), "2026-09-01".to_string(), "2026-10-25".to_string(), 15), ("b.com".into(), "2026-09-05".into(), "2026-10-01".into(), 4)]
    );
    assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from access_target_seen where node_id = $1").bind(w.old).fetch_one(db).await.unwrap(), 2);
    // 版本：多一条 replace，快照 = 当前生效值；替换前的历史快照里旧节点改挂到新节点，回滚到它们不会把策略放回旧节点
    let vs = t.get("/api/v1/policies/versions").await;
    let kinds: Vec<&str> = vs.as_array().unwrap().iter().map(|v| v["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["replace", "publish", "publish", "publish"], "{vs}");
    assert_eq!((vs[0]["is_latest"].as_bool(), vs[0]["is_current"].as_bool(), vs[0]["rollbackable"].as_bool()), (Some(true), Some(true), Some(false)), "{vs}");
    assert_eq!(vs[0]["relays"][0]["relay_id"], json!(w.relay));
    assert_eq!(vs[0]["changes"].as_array().unwrap().len(), 2, "旧节点 → 默认，新节点 → 旧节点的策略：{vs}");
    let snap_now: Value = sqlx::query_scalar("select snapshot from policy_version order by seq desc limit 1").fetch_one(db).await.unwrap();
    assert_eq!(snap_now, json!([{"scope": "node", "scope_id": w.new, "mode": "denylist", "domains": ["bad.com"], "note": ""}]));
    for i in [1, 2] {
        assert_eq!((vs[i]["is_current"].as_bool(), vs[i]["rollbackable"].as_bool()), (Some(true), Some(false)), "替换前的第 {i} 版：策略已在新节点上，和当前相同：{vs}");
    }
    let pre = t.get(&format!("/api/v1/policies/versions/{}/rollback", vs[3]["id"].as_str().unwrap())).await;
    let ids: Vec<&str> = pre["changes"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [w.new.to_string()], "回滚到最早一版只改新节点，不碰旧节点：{pre}");
    assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from audit_log where action = 'policy.replace'").fetch_one(db).await.unwrap(), 1);
    // 审计记下改挂的两个节点、被改写的历史版本（v2、v3）和其中丢掉了新节点原有项的（v3）
    let seqs: Vec<i64> = sqlx::query_scalar("select seq from policy_version where kind = 'publish' order by seq").fetch_all(db).await.unwrap();
    let detail: Value = sqlx::query_scalar("select detail from audit_log where action = 'policy.replace'").fetch_one(db).await.unwrap();
    assert_eq!((&detail["from"], &detail["to"]), (&json!(w.old), &json!(w.new)), "{detail}");
    assert_eq!((&detail["rewritten_seqs"], &detail["dropped_seqs"]), (&json!([seqs[1], seqs[2]]), &json!([seqs[2]])), "{detail}");
    assert!(l.contains(&format!("历史版本的快照里旧节点的访问策略改指新节点：v{}、v{}，其中 v{} 里新节点原有的项被丢掉", seqs[1], seqs[2], seqs[2])), "{l}");
    let subs: Vec<(String, Vec<Uuid>, Vec<Uuid>)> = sqlx::query_as("select name, exclude_ids, include_ids from client_sub order by name").fetch_all(db).await.unwrap();
    assert_eq!(
        subs,
        [("公司".to_string(), vec![], vec![w.new]), ("家里".into(), vec![w.new], vec![]), ("无关".into(), vec![w.new], vec![])],
        "引用旧节点的换成新节点，新节点本来就在名单里的去重"
    );
}

/// 旧节点当前没有节点级策略（设过又清掉了）：当前的策略行不动、不记 replace 版本，但历史快照里旧节点那项照样改挂到新节点，
/// 回滚到那些版本不会把策略放回进了备用的旧节点；再来一遍结果不变
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replace_rewrites_history_even_without_current_node_policy() {
    let w = world().await;
    let t = &w.t;
    let db = &t.st.db;
    let item = |id: Uuid| json!({"scope": "node", "scope_id": id, "mode": "denylist", "domains": ["bad.com"], "note": ""});
    for s in [json!([item(w.old)]), json!([])] {
        sqlx::query("insert into policy_version (id, kind, actor, snapshot) values ($1, 'publish', 'test', $2)").bind(Uuid::now_v7()).bind(s).execute(db).await.unwrap();
    }
    let d = t.post(&format!("/api/v1/machines/{}/replace", w.old), json!({"new_machine_id": w.new})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    drive(t).await;
    let j = t.job(&job).await;
    let l = logs(t, &job).await;
    assert_eq!(j["state"], "succeeded", "{j} {l}");
    assert!(!l.contains("访问策略随编号转给新节点"), "{l}");
    assert!(l.contains("历史版本的快照里旧节点的访问策略改指新节点：v"), "{l}");
    let snaps: Vec<Value> = sqlx::query_scalar("select snapshot from policy_version order by seq").fetch_all(db).await.unwrap();
    assert_eq!(snaps, [json!([item(w.new)]), json!([])], "第一版里旧节点那项改挂到新节点");
    let kinds: Vec<String> = sqlx::query_scalar("select kind from policy_version order by seq").fetch_all(db).await.unwrap();
    assert_eq!(kinds, ["publish", "publish"], "当前策略没有搬：不记 replace 版本");
    assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from audit_log where action = 'policy.replace'").fetch_one(db).await.unwrap(), 0);
    // 幂等：再改写一遍，快照不变
    let mut tx = db.begin().await.unwrap();
    let m = fleet_server::policy::move_node(&mut tx, w.old, w.new, "test").await.unwrap();
    tx.commit().await.unwrap();
    assert!(m.summary.is_none() && m.rewritten_seqs.is_empty() && m.dropped_seqs.is_empty(), "再来一遍：已经没有含旧节点的版本，什么都不动");
    let again: Vec<Value> = sqlx::query_scalar("select snapshot from policy_version order by seq").fetch_all(db).await.unwrap();
    assert_eq!(again, snaps);
}

/// 改出口指向的预览：新旧入口所在节点的生效访问策略不同时提醒（客户实际受的是目标节点 / 团队的策略）；发起时不算这类提醒
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repoint_preview_warns_when_policy_differs() {
    let w = world().await;
    let t = &w.t;
    let db = &t.st.db;
    let oe = old_entry(&w).await;
    let ne: Uuid = sqlx::query_scalar("select id from entry where node_id = $1").bind(w.new).fetch_one(db).await.unwrap();
    router(&w, "团队2-路由A", 23601, &[("节点01", "198.51.100.2", 20003, Some(oe))]).await;
    let url = format!("/api/v1/entries/{oe}/repoint?to_entry_id={ne}");
    let ws = t.get(&url).await["warnings"].to_string();
    assert!(!ws.contains("访问策略"), "两边都是默认：{ws}");
    // 团队策略黑名单；旧节点自己设了暂停新连接 → 新出口（跟随团队）受黑名单
    sqlx::query("insert into policy (scope, scope_id, mode, domains) values ('team', $1, 'denylist', '{bad.com}'), ('node', $2, 'blocked', '{}')").bind(w.team).bind(w.old).execute(db).await.unwrap();
    let p = t.get(&url).await;
    assert!(p["warnings"].to_string().contains("新出口受团队「团队2」的访问策略：黑名单 · 1 个域名，与原来（节点「团队2-01」：暂停新连接）不同"), "{p}");
    // 发起时不算（views::live 重算会拖住提交），其余照常
    let d = t.post(&format!("/api/v1/entries/{oe}/repoint"), json!({"to_entry_id": ne})).await;
    assert!(d["job_id"].is_string() && !d["warnings"].to_string().contains("访问策略"), "{d}");
}
