//! 升级 agent 的回滚：推送时旧程序留成 .prev，新版本起不来、或重启后 2 分钟没有上报，就换回去并让作业失败；
//! 「全部升级」里哪种失败会让编排停下（要真的执行步骤）。需要 PostgreSQL（见 common）。编排的领取顺序在 tests/jobs.rs。
//!
//! 机器是假的：PATH 最前面放一个假 ssh，远端命令在「机器根目录」`<临时目录>/<IP>/` 里真的执行（/usr/local/bin、/etc、/var/lib
//! 改到里面；根目录里有 refuse 时登录被拒），systemctl 换成桩：记下调用；装着的 agent 版本号里带 crash 的，每次查主进程号都不一样（起来就退出、被反复拉起）。
//! agent 程序是 shell 脚本，`--version` 打印版本。上报由测试直接写库模拟；线路机的用作业装到机器上的令牌走真的上报接口
//! （Xray 状态要记上报它的版本）。
//! PATH 是整个测试进程共用的，所以单独一个测试文件；各测试的机器 IP 不同，根目录互不相干。

#![cfg(unix)]

mod common;

use axum::http::StatusCode;
use common::{seed_team, setup, T};
use fleet_server::jobs;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use uuid::Uuid;

/// 假 ssh：参数里 `root@` 后面是 IP，最后一个参数是远端命令（`bash -s` 时脚本从 stdin 来，推文件时 stdin 是文件内容）
const SSH: &str = r#"#!/bin/sh
for a; do last=$a; case $a in root@*) ip=${a#root@};; esac; done
export R="$FAKE_ROOT/$ip"
[ -e "$R/refuse" ] && { echo "root@$ip: Permission denied (publickey)." >&2; exit 255; }
mkdir -p "$R/etc/systemd/system"
fix() { sed "s#/usr/local/bin/#$R/usr/local/bin/#g; s#/etc/#$R/etc/#g; s#/var/lib/#$R/var/lib/#g"; }
if [ "$last" = "bash -s" ]; then fix | bash -s; else sh -c "$(printf '%s' "$last" | fix)"; fi
"#;

const SYSTEMCTL: &str = r#"#!/bin/sh
echo "$*" >> "$R/systemctl.log"
case "$*" in
  *is-active*) echo active ;;
  *MainPID*) if grep -q crash "$R/usr/local/bin/fleet-agent"; then date +%N; else echo 4242; fi ;;
esac
"#;

/// bin/ 下是假 ssh 与 systemctl，各机器的根目录按 IP 放在旁边。整个进程只装一次（目录留在 /tmp，进程结束也不删）
static FAKE: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    exe(&bin.join("ssh"), SSH);
    exe(&bin.join("systemctl"), SYSTEMCTL);
    std::env::set_var("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()));
    std::env::set_var("FAKE_ROOT", dir.path());
    dir
});

fn exe(p: &Path, body: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// agent 程序：`--version` 打印版本
fn agent_bin(p: &Path, version: &str) { exe(p, &format!("#!/bin/sh\necho 'fleet-agent {version}'\n")); }

fn read(p: &Path) -> String { std::fs::read_to_string(p).unwrap_or_default() }

/// 一台在用的落地机，机器上装着 `old` 版本的 agent，中控 dist 里是 `new`。返回 (机器 id, 机器根目录)
async fn machine(t: &T, ip: &str, old: &str, new: &str) -> (Uuid, PathBuf) {
    let root = FAKE.path().join(ip);
    agent_bin(&root.join("usr/local/bin/fleet-agent"), old);
    agent_bin(&t.dir.path().join("dist/fleet-agent"), new);
    let team = seed_team(t, "团队1").await;
    let id = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, lifecycle) values ($1, 'vps', $2, 1, $3::inet, 'active')")
        .bind(id)
        .bind(team)
        .bind(ip)
        .execute(&t.st.db)
        .await
        .unwrap();
    (id, root)
}

async fn upgrade(t: &T, m: Uuid) -> String {
    t.post(&format!("/api/v1/machines/{m}/upgrade-agent"), json!({})).await["job_id"].as_str().unwrap().to_string()
}

/// 领取并推进一轮（在等的先提前到现在），返回作业详情。
/// 推进放在堆上：调试构建不复用栈槽，测试函数里每处调用的临时 future 各占一份栈，几轮下来会撑爆测试线程 2 MB 的栈
async fn run_round(t: &T, job: &str) -> Value {
    Box::pin(async {
        sqlx::query("update job set run_after = now() where id = $1::uuid").bind(job).execute(&t.st.db).await.unwrap();
        let j = jobs::claim(&t.st, "test").await.unwrap().expect("有可领取的作业");
        assert_eq!(j.id.to_string(), job);
        jobs::advance(&t.st, j, "test").await.unwrap();
        t.job(job).await
    })
    .await
}

/// agent 用这个作业签发的新令牌报了一次，版本是 `version`
async fn report(t: &T, machine: Uuid, job: &str, version: &str) {
    sqlx::query("update agent_token set last_used_at = now() where id = (select (params->>'agent_token_id')::uuid from job where id = $1::uuid)")
        .bind(job)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query(
        "insert into agent_state (machine_id, agent_version, last_report_at) values ($1, $2, now())
         on conflict (machine_id) do update set agent_version = excluded.agent_version, last_report_at = now()",
    )
    .bind(machine)
    .bind(version)
    .execute(&t.st.db)
    .await
    .unwrap();
}

/// 一台在用的线路机 `label`：配置代数 3，机器上装着 `old` 版本的 agent，中控 dist 里是 `new`。
/// 升级前旧 agent 最近报的 Xray 状态是 `xray`，中控记的应用错误是 `apply_error`。返回 (机器 id, 机器根目录)
async fn relay(t: &T, label: &str, ip: &str, old: &str, new: &str, xray: Value, apply_error: Option<&str>) -> (Uuid, PathBuf) {
    let root = FAKE.path().join(ip);
    agent_bin(&root.join("usr/local/bin/fleet-agent"), old);
    agent_bin(&t.dir.path().join("dist/fleet-agent"), new);
    let id = Uuid::now_v7();
    let (sk, pk) = fleet_server::crypto::reality_keypair();
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle) values ($1, 'relay', $2, $3::inet, 'active')")
        .bind(id)
        .bind(label)
        .bind(ip)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query(
        "insert into relay (machine_id, public_host, reality_private_enc, reality_public_key, generation, applied_generation, apply_error)
         values ($1, 'relay.test', $2, $3, 3, 3, $4)",
    )
    .bind(id)
    .bind(t.st.secrets.seal_str(&sk))
    .bind(pk)
    .bind(apply_error)
    .execute(&t.st.db)
    .await
    .unwrap();
    sqlx::query("insert into agent_state (machine_id, agent_version, last_report_at, info) values ($1, $2, now(), $3)")
        .bind(id)
        .bind(old)
        .bind(json!({"xray": xray}))
        .execute(&t.st.db)
        .await
        .unwrap();
    (id, root)
}

/// 机器上的 agent 用作业装上的令牌经上报接口报一份：版本 `version`，`xray` 为 null 时不带 Xray 状态
async fn agent_report(t: &T, root: &Path, seq: u64, version: &str, xray: Value, apply_error: Option<&str>) {
    let tok = read(&root.join("etc/fleet-agent/token"));
    let mut r = json!({"seq": seq, "boot_id": "b1", "agent_version": version, "sent_at_ms": 0, "agent_health": {"last_apply_error": apply_error}});
    if !xray.is_null() {
        r["xray"] = xray;
    }
    let (s, v) = t.req("POST", "/agent/v1/report", Some(r), Some(tok.trim())).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// systemctl 桩记下的 agent 重启次数
fn restarts(root: &Path) -> usize { read(&root.join("systemctl.log")).lines().filter(|l| *l == "restart fleet-agent").count() }

/// 「等待新版本上报」已经等满 2 分钟：把这一步的开始时间往前挪
async fn wait_out(t: &T, job: &str) {
    sqlx::query("update job_step set started_at = now() - interval '121 seconds' where job_id = $1::uuid and key = 'wait_report'")
        .bind(job)
        .execute(&t.st.db)
        .await
        .unwrap();
}

/// 成功：新程序换上，旧的留成 .prev（下次升级覆盖），agent 只重启一次；用新令牌的上报要是新版本才算
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgrade_keeps_prev_and_waits_for_the_new_version() {
    let t = setup(false).await;
    let (m, root) = machine(&t, "192.0.2.11", "0.1.0+old", "0.1.0+new").await;
    let (bin, prev) = (root.join("usr/local/bin/fleet-agent"), root.join("usr/local/bin/fleet-agent.prev"));
    let job = upgrade(&t, m).await;
    let j = run_round(&t, &job).await;
    assert_eq!((j["state"].as_str(), j["step"].as_i64()), (Some("waiting"), Some(1)), "{j}");
    let out = &j["steps"][0]["output"];
    assert_eq!([&out["binary_updated"], &out["agent_version"], &out["prev_version"]], [&json!(true), &json!("0.1.0+new"), &json!("0.1.0+old")], "{j}");
    assert!(read(&bin).contains("0.1.0+new") && read(&prev).contains("0.1.0+old"));

    // 用新令牌报的却还是旧版本：不算，接着等
    report(&t, m, &job, "0.1.0+old").await;
    assert_eq!(run_round(&t, &job).await["state"], "waiting");
    report(&t, m, &job, "0.1.0+new").await;
    let j = run_round(&t, &job).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    assert_eq!(j["steps"][1]["output"]["agent_version"], "0.1.0+new");
    assert_eq!(restarts(&root), 1, "没有回滚");
    assert!(read(&bin).contains("0.1.0+new") && read(&prev).contains("0.1.0+old"), ".prev 留着，下次升级覆盖");
}

/// 新版本 2 分钟没有上报：换回 .prev 并重启，作业失败、写明回滚到哪个版本；重试从推送重来；
/// 机器上没有 .prev（换不回去）时写明要人工处理
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn silent_new_version_is_rolled_back() {
    let t = setup(false).await;
    let (m, root) = machine(&t, "192.0.2.12", "0.1.0+old", "0.1.0+new").await;
    let (bin, prev) = (root.join("usr/local/bin/fleet-agent"), root.join("usr/local/bin/fleet-agent.prev"));
    let job = upgrade(&t, m).await;
    assert_eq!(run_round(&t, &job).await["state"], "waiting");
    // 还没等满 2 分钟：接着等，不回滚
    assert_eq!(run_round(&t, &job).await["state"], "waiting");
    wait_out(&t, &job).await;
    let j = run_round(&t, &job).await;
    assert_eq!(j["state"], "failed", "{j}");
    assert_eq!(j["error"], "等待新版本上报：新版本 0.1.0+new 重启后 120 秒没有上报，已回滚到 0.1.0+old");
    assert_eq!(j["steps"][1]["error_kind"], "timeout");
    assert!(read(&bin).contains("0.1.0+old") && read(&prev).contains("0.1.0+old"));
    assert_eq!(restarts(&root), 2, "装新版本一次、换回去一次");

    // 重试从推送重来：又换上新版本，接着等它上报
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    let j = run_round(&t, &job).await;
    assert_eq!((j["state"].as_str(), j["step"].as_i64()), (Some("waiting"), Some(1)), "{j}");
    assert!(read(&bin).contains("0.1.0+new") && read(&prev).contains("0.1.0+old"));

    // 这回机器上的 .prev 没了：换不回去
    std::fs::remove_file(&prev).unwrap();
    wait_out(&t, &job).await;
    let j = run_round(&t, &job).await;
    let err = j["error"].as_str().unwrap();
    assert!(
        err.starts_with("等待新版本上报：新版本 0.1.0+new 重启后 120 秒没有上报，机器上没有 /usr/local/bin/fleet-agent.prev，没能回滚：需要人工处理"),
        "{err}"
    );
    assert_eq!((j["state"].as_str(), restarts(&root)), (Some("failed"), 3));
}

/// 新版本起来就退出（主进程号一直在变）：推送那一步当场换回去
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crashing_new_version_is_rolled_back_at_once() {
    let t = setup(false).await;
    let (m, root) = machine(&t, "192.0.2.13", "0.1.0+old", "0.1.0+crash").await;
    let job = upgrade(&t, m).await;
    let j = run_round(&t, &job).await;
    assert_eq!(j["state"], "failed", "{j}");
    assert_eq!(j["error"], "推送并重启 agent：新版本 0.1.0+crash 起不来（主进程起来就退出），已回滚到 0.1.0+old");
    assert_eq!(j["steps"][0]["error_kind"], "remote");
    assert!(read(&root.join("usr/local/bin/fleet-agent")).contains("0.1.0+old"));
    assert_eq!(restarts(&root), 2);

    // 回滚脚本再跑一遍（换完之后连接断了、ssh 重试）：结果相同
    let ssh = fleet_server::ssh::Target { ip: "192.0.2.13".into(), port: 22, key: "/nonexistent".into(), known_hosts: t.dir.path().join("known_hosts"), password: None };
    let o = ssh.run(&jobs::scripts::agent_rollback(), std::time::Duration::from_secs(30)).await.unwrap();
    assert_eq!((o.code, o.kv("agent"), o.kv("version")), (0, Some("active"), Some("0.1.0+old")), "{o:?}");
    assert!(read(&root.join("usr/local/bin/fleet-agent")).contains("0.1.0+old") && read(&root.join("usr/local/bin/fleet-agent.prev")).contains("0.1.0+old"));
}

/// `by` 版本的 agent 报的正常的 Xray 状态（在跑、已应用到代数 3）
fn xray_ok(by: &str) -> Value { json!({"running": true, "applied_generation": 3, "agent_version": by}) }

/// 线路机：新版本上报了还不算，要它自己报的 Xray 状态做到升级前做到的几项。重启后新进程最先补发的是旧进程退出时落盘的报告
/// （旧版本号、旧进程的 Xray 状态），之后几份不带 Xray 状态：这些都不能算数
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_upgrade_waits_for_xray_from_the_new_version() {
    let t = setup(false).await;
    let (m, root) = relay(&t, "线路机2", "192.0.2.21", "0.1.0+old", "0.1.0+new", xray_ok("0.1.0+old"), None).await;
    let job = upgrade(&t, m).await;
    let j = run_round(&t, &job).await;
    assert_eq!((j["state"].as_str(), j["step"].as_i64()), (Some("waiting"), Some(1)), "{j}");
    assert_eq!(j["steps"][0]["output"]["xray_checks"], "Xray 在跑、配置应用到代数 3、没有应用错误", "{j}");

    agent_report(&t, &root, 1, "0.1.0+old", json!({"running": true, "applied_generation": 3}), None).await;
    assert_eq!(run_round(&t, &job).await["state"], "waiting", "旧进程落盘的报告");
    agent_report(&t, &root, 2, "0.1.0+new", Value::Null, None).await;
    assert_eq!(run_round(&t, &job).await["state"], "waiting", "新版本还没报 Xray 状态：库里的是旧进程报的");
    agent_report(&t, &root, 3, "0.1.0+new", json!({"running": true, "applied_generation": 3}), None).await;
    let j = run_round(&t, &job).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    assert_eq!(restarts(&root), 1, "没有回滚");
}

/// 新版本照常上报、Xray 却不正常（没在跑、配置没应用上、应用出错）：2 分钟后换回旧版本，错误逐项写明；
/// 升级前就不正常的项不查（升级前 Xray 就坏着，不能因此永远升级不了）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_upgrade_rolls_back_when_xray_breaks() {
    let t = setup(false).await;
    let (m, root) = relay(&t, "线路机2", "192.0.2.22", "0.1.0+old", "0.1.0+new", xray_ok("0.1.0+old"), None).await;
    let job = upgrade(&t, m).await;
    assert_eq!(run_round(&t, &job).await["state"], "waiting");
    agent_report(&t, &root, 1, "0.1.0+new", json!({"running": false, "applied_generation": 2}), Some("xray -test 失败")).await;
    assert_eq!(run_round(&t, &job).await["state"], "waiting", "还没到 2 分钟");
    wait_out(&t, &job).await;
    let j = run_round(&t, &job).await;
    assert_eq!(j["state"], "failed", "{j}");
    assert_eq!(
        j["error"],
        "等待新版本上报：新版本 0.1.0+new 重启后 120 秒线路机检查没通过（Xray 没在跑；已应用的配置代数 2，没到 3；应用配置出错：xray -test 失败），已回滚到 0.1.0+old"
    );
    assert_eq!(j["steps"][1]["error_kind"], "remote");
    assert!(read(&root.join("usr/local/bin/fleet-agent")).contains("0.1.0+old"));
    assert_eq!(restarts(&root), 2);

    // 升级前 Xray 就没在跑、配置停在旧代数、有应用错误：这几项都不查，新版本上报了就算成功
    let t = setup(false).await;
    let before = json!({"running": false, "applied_generation": 2, "agent_version": "0.1.0+old"});
    let (m, root) = relay(&t, "线路机2", "192.0.2.23", "0.1.0+old", "0.1.0+new", before, Some("xray -test 失败")).await;
    let job = upgrade(&t, m).await;
    let j = run_round(&t, &job).await;
    assert_eq!(j["steps"][0]["output"]["xray_checks"], "不查（升级前 Xray 就不正常）", "{j}");
    agent_report(&t, &root, 1, "0.1.0+new", Value::Null, Some("xray -test 失败")).await;
    assert_eq!(run_round(&t, &job).await["state"], "succeeded");
}

/// 「全部升级」，返回按执行顺序的作业 id
async fn upgrade_all(t: &T) -> Vec<String> {
    let up = t.post("/api/v1/agents/upgrade", json!({})).await;
    up["jobs"].as_array().unwrap().iter().map(|j| j["job_id"].as_str().unwrap().to_string()).collect()
}

/// 「全部升级」：新版本在金丝雀上没上报、换回了旧版本，其余作业一步都不做，直接取消，原因写明是哪台（沿着编排一路传下去）；机器随即空出来
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bulk_upgrade_stops_when_the_new_version_fails() {
    let t = setup(false).await;
    let (_, root) = relay(&t, "线路机2", "192.0.2.31", "0.1.0+old", "0.1.0+new", xray_ok("0.1.0+old"), None).await;
    relay(&t, "线路机3", "192.0.2.32", "0.1.0+old", "0.1.0+new", xray_ok("0.1.0+old"), None).await;
    let (vps, _) = machine(&t, "192.0.2.33", "0.1.0+old", "0.1.0+new").await;
    let ids = upgrade_all(&t).await;
    assert_eq!(run_round(&t, &ids[0]).await["state"], "waiting");
    wait_out(&t, &ids[0]).await;
    let j = run_round(&t, &ids[0]).await;
    assert!(j["error"].as_str().unwrap().ends_with("没有上报，已回滚到 0.1.0+old"), "{j}");
    assert_eq!(restarts(&root), 2);
    for id in &ids[1..] {
        let j = run_round(&t, id).await;
        assert_eq!(j["state"], "cancelled", "{j}");
        assert_eq!(j["error"], "金丝雀「线路机2」升级失败，其余机器不再升级");
        assert!(j["started_at"].is_null() && j["steps"].as_array().unwrap().iter().all(|s| s["state"] == "pending"), "{j}");
        let logs = t.get(&format!("/api/v1/jobs/{id}/logs")).await;
        assert_eq!(logs[0]["msg"], "没有开始：金丝雀「线路机2」升级失败，其余机器不再升级", "{logs}");
    }
    assert!(t.get(&format!("/api/v1/machines/{vps}")).await["active_job"].is_null());
}

/// 「全部升级」：金丝雀推送之前就失败（登录被拒，机器上的程序没换过）与新版本无关，只算它自己，后面的照常升级
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bulk_upgrade_goes_on_after_a_failure_before_push() {
    let t = setup(false).await;
    let (_, r2) = relay(&t, "线路机2", "192.0.2.34", "0.1.0+old", "0.1.0+new", xray_ok("0.1.0+old"), None).await;
    std::fs::write(r2.join("refuse"), "").unwrap();
    let (_, r3) = relay(&t, "线路机3", "192.0.2.35", "0.1.0+old", "0.1.0+new", xray_ok("0.1.0+old"), None).await;
    let ids = upgrade_all(&t).await;
    let j = run_round(&t, &ids[0]).await;
    assert_eq!((j["state"].as_str(), j["steps"][0]["error_kind"].as_str()), (Some("failed"), Some("auth")), "{j}");
    assert!(read(&r2.join("usr/local/bin/fleet-agent")).contains("0.1.0+old"));
    assert_eq!(run_round(&t, &ids[1]).await["state"], "waiting", "线路机3 照常开始");
    agent_report(&t, &r3, 1, "0.1.0+new", json!({"running": true, "applied_generation": 3}), None).await;
    assert_eq!(run_round(&t, &ids[1]).await["state"], "succeeded");
}
