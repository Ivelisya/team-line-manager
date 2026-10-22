//! 升级 agent 的回滚：推送时旧程序留成 .prev，新版本起不来、或重启后 2 分钟没有上报，就换回去并让作业失败。
//! 需要 PostgreSQL（见 common）。「全部升级」的编排（金丝雀、线路机逐台）不碰机器，在 tests/jobs.rs。
//!
//! 机器是假的：PATH 最前面放一个假 ssh，远端命令在「机器根目录」`<临时目录>/<IP>/` 里真的执行（/usr/local/bin、/etc、/var/lib
//! 改到里面），systemctl 换成桩：记下调用；装着的 agent 版本号里带 crash 的，每次查主进程号都不一样（起来就退出、被反复拉起）。
//! agent 程序是 shell 脚本，`--version` 打印版本。上报由测试直接写库模拟。
//! PATH 是整个测试进程共用的，所以单独一个测试文件；各测试的机器 IP 不同，根目录互不相干。

#![cfg(unix)]

mod common;

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
}
