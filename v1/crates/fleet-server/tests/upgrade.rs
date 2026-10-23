//! 升级 agent 的回滚：推送时旧程序留成 .prev，新版本起不来、或重启后 2 分钟没有上报，就换回去并让作业失败；
//! 「全部升级」里哪种失败会让编排停下（要真的执行步骤）。需要 PostgreSQL（见 common）。编排的领取顺序在 tests/it/jobs.rs。
//! 还有系统基线、节点 ssserver 脚本重跑时以运行中的 sshd / 实例为准，系统调优脚本逐键写入读回（不建作业，脚本直接经假 ssh 跑）。
//!
//! 机器是假的：PATH 最前面放一个假 ssh，远端命令在「机器根目录」`<临时目录>/<IP>/` 里真的执行（/run、/usr/local/bin、/etc、/var/lib
//! 改到里面；根目录里有 refuse 时登录被拒），systemctl 换成桩：记下调用；装着的 agent 版本号里带 crash 的，每次查主进程号都不一样（起来就退出、被反复拉起）。
//! 会改测试机本身的命令（建用户、nft、sysctl、apt-get…）也换成桩（见 STUBS）。
//! agent 程序是 shell 脚本，`--version` 打印版本。上报由测试直接写库模拟；线路机的用作业装到机器上的令牌走真的上报接口
//! （Xray 状态要记上报它的版本）。
//! PATH 是整个测试进程共用的，所以单独一个测试文件；各测试的机器 IP 不同，根目录互不相干。

#![cfg(unix)]

#[path = "it/common/mod.rs"]
mod common;

use axum::http::StatusCode;
use common::{seed_node, seed_team, setup, T};
use fleet_server::jobs;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

/// 假 ssh：参数里 `root@` 后面是 IP，最后一个参数是远端命令（`bash -s` 时脚本从 stdin 来，推文件时 stdin 是文件内容）
const SSH: &str = r#"#!/bin/sh
for a; do last=$a; case $a in root@*) ip=${a#root@};; esac; done
export R="$FAKE_ROOT/$ip"
[ -e "$R/refuse" ] && { echo "root@$ip: Permission denied (publickey)." >&2; exit 255; }
mkdir -p "$R/etc/systemd/system" "$R/run"
fix() { sed "s#/run/#$R/run/#g; s#/usr/local/bin/#$R/usr/local/bin/#g; s#/etc/#$R/etc/#g; s#/var/lib/#$R/var/lib/#g; s#^PS=/proc/sys\$#PS=$R/proc/sys#; s#^MI=/proc/meminfo\$#MI=$R/proc/meminfo#"; }
if [ "$last" = "bash -s" ]; then fix | bash -s; else sh -c "$(printf '%s' "$last" | fix)"; fi
"#;

/// systemctl 桩：记下调用。别的单元的主进程号、启动时刻（systemd 的显示格式）读机器根目录里的 pid.<单元>、started.<单元>，
/// 重启时把启动时刻写成现在；有 need-reload.<单元> 时报单元要重新加载，daemon-reload 清掉它
const SYSTEMCTL: &str = r#"#!/bin/sh
echo "$*" >> "$R/systemctl.log"
for u; do :; done
case "$*" in
  *is-active*) echo active ;;
  *MainPID*fleet-agent) if grep -q crash "$R/usr/local/bin/fleet-agent"; then date +%N; else echo 4242; fi ;;
  *MainPID*) cat "$R/pid.$u" 2>/dev/null || echo 0 ;;
  *ExecMainStartTimestamp*) cat "$R/started.$u" 2>/dev/null || true ;;
  *LimitNOFILE*) echo 65536 ;;
  *NeedDaemonReload*) [ -e "$R/need-reload.$u" ] && echo yes || echo no ;;
  daemon-reload) rm -f "$R"/need-reload.* ;;
  restart*) date -u '+%a %Y-%m-%d %H:%M:%S UTC' > "$R/started.$u" ;;
esac
"#;

/// 去掉「-g 组」（测试机上没有 fleet-ss 这个组），其余交给真的 install
const INSTALL: &str = r#"#!/bin/sh
for a; do shift; if [ -n "$skip" ]; then skip=; elif [ "$a" = -g ]; then skip=1; else set -- "$@" "$a"; fi; done
exec /usr/bin/install "$@"
"#;

/// 节点、基线脚本里会改测试机本身的命令换成桩：不建用户和组、不动它的防火墙、内核参数和软件包。
/// sshd -T 报只允许密钥；nft 列出放行集合；ss 报端口在监听
const STUBS: &[(&str, &str)] = &[
    ("id", "#!/bin/sh\n"),
    ("chgrp", "#!/bin/sh\n"),
    ("sysctl", "#!/bin/sh\n"),
    ("modprobe", "#!/bin/sh\n"),
    // 调优脚本：不是容器、时间已同步
    ("systemd-detect-virt", "#!/bin/sh\nexit 1\n"),
    ("timedatectl", "#!/bin/sh\necho yes\n"),
    ("apt-get", "#!/bin/sh\n"),
    ("sshd", "#!/bin/sh\n[ \"$1\" = -T ] && echo 'passwordauthentication no'\nexit 0\n"),
    // 调优脚本采样已建立的连接：机器根目录里的 ss-established；其余是节点脚本查监听
    ("ss", "#!/bin/sh\ncase \"$*\" in *established*) cat \"$R/ss-established\" 2>/dev/null ;; *) echo 'LISTEN 0 4096 0.0.0.0:8388 0.0.0.0:*' ;; esac\n"),
    // 调优脚本的残留检测：默认路由、队列读机器根目录里的 ip-route、tc-qdisc；ip 的其它用法交给真的 ip
    ("ip", "#!/bin/sh\ncase \"$*\" in *route*) cat \"$R/ip-route\" 2>/dev/null; exit 0 ;; esac\nfor d in /sbin /usr/sbin /bin /usr/bin; do [ -x $d/ip ] && exec $d/ip \"$@\"; done\nexit 1\n"),
    ("tc", "#!/bin/sh\ncat \"$R/tc-qdisc\" 2>/dev/null\nexit 0\n"),
    ("nft", r#"#!/bin/sh
[ "$1" = list ] && printf 'table inet ss_guard {\n\tset relays {\n\t\ttype ipv4_addr\n\t\telements = { 198.51.100.2 }\n\t}\n}\n'
exit 0
"#),
    ("install", INSTALL),
];

/// bin/ 下是假 ssh、systemctl 等桩，各机器的根目录按 IP 放在旁边。整个进程只装一次（目录留在 /tmp，进程结束也不删）
static FAKE: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    exe(&bin.join("ssh"), SSH);
    exe(&bin.join("systemctl"), SYSTEMCTL);
    for (name, body) in STUBS {
        exe(&bin.join(name), body);
    }
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

/// 一台在用的 VPS 节点，机器上装着 `old` 版本的 agent，中控 dist 里是 `new`。返回 (机器 id, 机器根目录)
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

/// 领取并推进一轮（在等的先提前到现在），返回作业详情
async fn run_round(t: &T, job: &str) -> Value {
    sqlx::query("update job set run_after = now() where id = $1::uuid").bind(job).execute(&t.st.db).await.unwrap();
    let j = jobs::claim(&t.st, "test").await.unwrap().expect("有可领取的作业");
    assert_eq!(j.id.to_string(), job);
    jobs::advance(&t.st, j, "test").await.unwrap();
    t.job(job).await
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
    let au = t.get("/api/v1/audit?cat=machine").await;
    assert_eq!((&au[0]["title"], &au[0]["summary"]), (&json!("升级 agent"), &json!("团队1-01")), "审计写明升级的是哪台：{au}");
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

/// 线路机的 DNS 是链路本地地址、正在跑的 Xray 还没用上公共解析器的挂载：先装 Xray 单元、重启一次 Xray，再换 agent，步骤里记一条说明。
/// 不是链路本地 DNS 的线路机（上面几个测试）不重启 Xray
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_upgrade_restarts_xray_for_linklocal_dns() {
    let t = setup(false).await;
    let (m, root) = relay(&t, "线路机2", "192.0.2.24", "0.1.0+old", "0.1.0+new", xray_ok("0.1.0+old"), None).await;
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::write(root.join("etc/resolv.conf"), "nameserver 169.254.169.254\n").unwrap();
    std::fs::write(root.join("pid.fleet-xray"), "42\n").unwrap();
    let job = upgrade(&t, m).await;
    let j = run_round(&t, &job).await;
    assert_eq!((j["state"].as_str(), &j["steps"][0]["output"]["xray_restarted"]), (Some("waiting"), &json!(true)), "{j}");
    let log = read(&root.join("systemctl.log"));
    let (xray, agent) = (log.find("restart fleet-xray\n").unwrap(), log.find("restart fleet-agent\n").unwrap());
    assert!(xray < agent && log.matches("restart fleet-xray\n").count() == 1, "{log}");
    assert!(read(&root.join("etc/fleet-xray/resolv.conf")).starts_with("nameserver 1.1.1.1\n"));
    let logs = t.get(&format!("/api/v1/jobs/{job}/logs")).await;
    assert!(logs.as_array().unwrap().iter().any(|l| l["msg"].as_str().unwrap().starts_with("重启了一次 Xray")), "{logs}");
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

// ── 脚本重跑：以运行中的 sshd / ssserver 实例为准（脚本直接经假 ssh 跑，不建作业）─────────────────

fn target(ip: &str) -> fleet_server::ssh::Target {
    fleet_server::ssh::Target { ip: ip.into(), port: 22, key: "/nonexistent".into(), known_hosts: "/nonexistent".into(), password: None }
}

/// 桩里单元 `unit` 的启动时刻（systemctl show 在 TZ=UTC0 下的显示格式）
fn started(root: &Path, unit: &str, at: OffsetDateTime) {
    let f = time::macros::format_description!("[weekday repr:short] [year]-[month]-[day] [hour]:[minute]:[second] UTC");
    std::fs::write(root.join(format!("started.{unit}")), at.format(&f).unwrap()).unwrap();
}

fn set_mtime(p: &Path, at: OffsetDateTime) { std::fs::File::options().write(true).open(p).unwrap().set_modified(at.into()).unwrap() }

/// 系统基线重跑时以运行中的 sshd 为准：换上配置之后没来得及重启就被打断（配置比运行中的 sshd 新），重跑时文件没变也重启，
/// 否则 sshd -T 读文件报 no、运行中的 sshd 却还允许密码登录；sshd 在配置改过之后启动的不重启
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn baseline_restarts_sshd_older_than_its_config() {
    let ip = "192.0.2.41";
    let root = FAKE.path().join(ip);
    for d in ["etc/ssh", "etc/sysctl.d"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(root.join("etc/ssh/sshd_config"), "PasswordAuthentication yes\n").unwrap();
    let conf = [root.join("etc/ssh/sshd_config.d/00-fleet-keyonly.conf"), root.join("etc/ssh/sshd_config")];
    let restarts = || read(&root.join("systemctl.log")).lines().filter(|l| *l == "restart ssh").count();
    let run = move || async move { target(ip).run(jobs::scripts::BASELINE, Duration::from_secs(60)).await.unwrap() };
    let now = OffsetDateTime::now_utc();
    let hours = |h: i64| now - time::Duration::hours(h);

    // 第一次：换上只允许密钥的配置、在主配置第一行引入它，重启 sshd
    started(&root, "ssh", hours(1));
    let o = run().await;
    assert_eq!((o.code, o.kv("sshd_password"), restarts()), (0, Some("no"), 1), "{o:?}");
    assert!(read(&conf[1]).starts_with("Include "), "{}", read(&conf[1]));
    // sshd 在配置改过之后启动：不重启
    for f in &conf {
        set_mtime(f, hours(2));
    }
    started(&root, "ssh", hours(1));
    assert_eq!((run().await.code, restarts()), (0, 1));
    // 换上配置之后没来得及重启就被打断：配置比运行中的 sshd 新，这次文件没变也重启
    set_mtime(&conf[0], now - time::Duration::minutes(10));
    let o = run().await;
    assert_eq!((o.code, o.kv("sshd_password"), restarts()), (0, Some("no"), 2), "{o:?}");
}

/// 节点的 ssserver 重跑时以运行中的实例为准：换上新密码之后没来得及重启就被打断（配置比实例新），重跑时文件没变也重启，
/// 否则 ssserver 一直用旧密码；配置、单元、程序都没变且实例在它们之后启动的不重启（不断客户的连接）；单元换了没重新加载的先加载再重启。
/// /etc/resolv.conf 的 DNS 是链路本地地址时配置里加公共解析器（单元的元数据拦截照旧），报 linklocal_dns=1
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_ss_restarts_an_instance_older_than_its_config() {
    let ip = "192.0.2.42";
    let root = FAKE.path().join(ip);
    // 在跑的 ssserver：磁盘上的程序是指向 sleep 的符号链接，由它起的进程当主进程（「进程跑的是磁盘上的程序」成立）。
    // 不复制：刚写完就执行，别的测试线程同时 fork 出的子进程还拿着写句柄时会 ETXTBSY。
    // argv[0] 固定写 sleep：sleep 可能是按 argv[0] 分派的多合一程序（busybox；Alpine 3.24 的 coreutils 包换成了 uutils），
    // 经符号链接起的进程 argv[0] 是 ssserver，会报 unknown program 立即退出，脚本就看不到「进程跑的是磁盘上的程序」
    let bin = root.join("usr/local/bin/ssserver");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", &bin).unwrap();
    let mut ss = tokio::process::Command::new(&bin).arg0("sleep").arg("600").kill_on_drop(true).spawn().unwrap();
    std::fs::write(root.join("pid.fleet-ssserver"), ss.id().unwrap().to_string()).unwrap();
    let script = |pw: &str| {
        jobs::scripts::vars(&[("SS_PORT", "8388"), ("SS_METHOD", "aes-256-gcm"), ("SS_PASSWORD", pw), ("ALLOW", "198.51.100.2")])
            + &jobs::scripts::node_ss(&jobs::scripts::SsInst::host())
    };
    let run = move |s: String| async move { target(ip).run(&s, Duration::from_secs(60)).await.unwrap() };
    let count = |what: &str| read(&root.join("systemctl.log")).lines().filter(|l| *l == what).count();
    let restarts = || count("restart fleet-ssserver");
    let (conf, unit, acl) = (root.join("etc/shadowsocks-rust/config.json"), root.join("etc/systemd/system/fleet-ssserver.service"), root.join("etc/shadowsocks-rust/fleet.acl"));
    let now = OffsetDateTime::now_utc();

    // 第一次：写配置、单元，重启
    let o = run(script("pw1")).await;
    assert_eq!((o.code, o.kv("ss"), o.kv("nft_relays"), o.kv("linklocal_dns")), (0, Some("listening"), Some("198.51.100.2"), None), "{o:?}");
    assert!(read(&conf).contains(r#""password":"pw1""#) && read(&unit).contains("\nIPAddressDeny=link-local 100.100.100.200/32\n"));
    assert_eq!(restarts(), 1);
    assert!(ss.try_wait().unwrap().is_none(), "假 ssserver 已退出，下面「不重启」的断言没有意义");
    // 都没变、实例在配置和单元之后启动：不重启
    for f in [&conf, &unit, &acl] {
        set_mtime(f, now - time::Duration::hours(2));
    }
    started(&root, "fleet-ssserver", now - time::Duration::hours(1));
    assert_eq!((run(script("pw1")).await.code, restarts()), (0, 1));
    // 换上新密码之后没来得及重启就被打断：配置比运行中的实例新，这次文件没变也重启
    assert_eq!((run(script("pw2")).await.code, restarts()), (0, 2));
    started(&root, "fleet-ssserver", now - time::Duration::hours(1));
    assert_eq!((run(script("pw2")).await.code, restarts()), (0, 3));
    // 单元换了、systemd 还没重新加载：先加载再重启
    std::fs::write(root.join("need-reload.fleet-ssserver"), "").unwrap();
    let reloads = count("daemon-reload");
    assert_eq!((run(script("pw2")).await.code, count("daemon-reload"), restarts()), (0, reloads + 1, 4));
    // DNS 是链路本地地址：配置加公共解析器（配置变了，重启），元数据拦截照旧，报 linklocal_dns=1
    std::fs::write(root.join("etc/resolv.conf"), "nameserver 169.254.169.254\n").unwrap();
    let o = run(script("pw2")).await;
    assert_eq!((o.code, o.kv("linklocal_dns"), restarts()), (0, Some("1"), 5), "{o:?}");
    assert!(read(&conf).contains(r#","dns":"1.1.1.1,8.8.8.8"}"#), "{}", read(&conf));
    assert!(read(&unit).contains("\nIPAddressDeny=link-local 100.100.100.200/32\n"), "{}", read(&unit));
    drop(ss);
}

// ── 系统调优 tune-v2：脚本逐键写入、读回（机器根目录里的 proc/sys/ 是普通文件），推导缓冲区上限，原值快照与撤销，作业 tune ──────────────

/// 假 /proc/sys 里的一个键：`dots` 是 sysctl 键名
fn sysctl_file(root: &Path, key: &str) -> PathBuf { root.join("proc/sys").join(key.replace('.', "/")) }

fn put_sysctl(root: &Path, key: &str, val: &str) {
    let p = sysctl_file(root, key);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, val).unwrap();
}

/// 假 /proc/meminfo
fn put_meminfo(root: &Path, kb: u64) {
    std::fs::create_dir_all(root.join("proc")).unwrap();
    std::fs::write(root.join("proc/meminfo"), format!("MemTotal:       {kb} kB\nMemFree:          100000 kB\nSwapTotal:             0 kB\n")).unwrap();
}

/// 脚本只把读回一致的键写进文件并记 applied（带改前的值）/ ok；现值不小于目标的 `>=` 键保持不动（kept，三元组逐项比）；写不进的（指向 /dev/null）
/// 记 readonly、不进文件、退出码仍为 0；缺失的记 missing。第一次跑先记原值快照（0600），重跑不动快照、文件不变、journald 不再重启；
/// 撤销按快照写回、删掉配置文件。线路机先保留入口端口池再放宽本地端口范围，入口连接的 RTT 取 P90 推导缓冲区上限，残留只报告
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tune_script_writes_reads_back_and_skips() {
    let (ip, rip) = ("192.0.2.45", "192.0.2.46");
    let root = FAKE.path().join(ip);
    put_meminfo(&root, 4_005_000);
    put_sysctl(&root, "net.core.rmem_max", "212992\n");
    put_sysctl(&root, "net.core.somaxconn", "65535\n");
    put_sysctl(&root, "net.ipv4.tcp_slow_start_after_idle", "0\n");
    put_sysctl(&root, "net.ipv4.tcp_rmem", "4096\t131072\t6291456\n");
    put_sysctl(&root, "fs.file-max", "9223372036854775807\n");
    put_sysctl(&root, "net.ipv4.tcp_sack", "0\n");
    put_sysctl(&root, "net.ipv4.tcp_syncookies", "2\n");
    let fin = sysctl_file(&root, "net.ipv4.tcp_fin_timeout");
    std::fs::create_dir_all(fin.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/dev/null", &fin).unwrap();
    std::fs::create_dir_all(root.join("etc/sysctl.d")).unwrap();
    std::fs::write(root.join("etc/sysctl.d/90-fleet-bbr.conf"), "net.core.default_qdisc=fq\n").unwrap();
    let run = |ip: &'static str, script: String| async move { target(ip).run(&script, Duration::from_secs(60)).await.unwrap() };
    let (conf, journald, orig) = (root.join("etc/sysctl.d/99-zz-fleet-tune.conf"), root.join("etc/systemd/journald.conf.d/90-fleet.conf"), root.join("var/lib/fleet/tune-orig.conf"));
    let journald_restarts = || read(&root.join("systemctl.log")).lines().filter(|l| *l == "restart systemd-journald").count();

    // 50M 套餐的节点：RTT 固定 100、不采样，缓冲区上限落在下限 16 MiB
    let o = run(ip, jobs::tune::script(jobs::tune::Role::Node, Some(50))).await;
    assert_eq!((o.code, o.kv("tune_profile"), o.kv("journald"), o.kv("ntp"), o.kv("nofile"), o.kv("container")), (0, Some("tune-v2"), Some("changed"), Some("synced"), Some("65536"), Some("0")), "{o:?}");
    assert_eq!((o.kv("bw"), o.kv("rtt"), o.kv("rtt_source"), o.kv("mem_kb"), o.kv("buf_max"), o.kv("snapshot")), (Some("50"), Some("100"), Some("default"), Some("4005000"), Some("16777216"), Some("created")), "{o:?}");
    let kv: Vec<_> = [
        "net.core.rmem_max", "net.core.somaxconn", "net.ipv4.tcp_slow_start_after_idle", "net.ipv4.tcp_rmem", "net.ipv4.tcp_fin_timeout", "net.netfilter.nf_conntrack_max",
        "fs.file-max", "net.ipv4.tcp_sack", "net.ipv4.tcp_syncookies",
    ]
    .iter()
    .map(|k| o.kv(&format!("tune.{k}")).unwrap_or("-"))
    .collect();
    assert_eq!(kv, ["applied:212992", "kept:65535", "ok", "applied:4096 131072 6291456", "readonly:", "missing", "kept:9223372036854775807", "applied:0", "kept:2"], "{o:?}");
    assert_eq!(read(&sysctl_file(&root, "net.core.rmem_max")), "16777216");
    assert_eq!(read(&sysctl_file(&root, "net.core.somaxconn")), "65535\n", "现值更大：不动");
    assert_eq!(read(&sysctl_file(&root, "fs.file-max")), "9223372036854775807\n", "LONG_MAX 不往下改");
    assert_eq!(read(&sysctl_file(&root, "net.ipv4.tcp_rmem")), "4096 131072 16777216");
    // 只有读回一致的键进文件；旧的 BBR 文件并入后删掉；journald 体积上限写好并重启一次
    assert_eq!(
        read(&conf),
        "# fleet tune-v2（中控生成，勿手改）\nnet.core.rmem_max = 16777216\nnet.ipv4.tcp_rmem = 4096 131072 16777216\nnet.ipv4.tcp_slow_start_after_idle = 0\nnet.ipv4.tcp_sack = 1\n"
    );
    assert!(!root.join("etc/sysctl.d/90-fleet-bbr.conf").exists() && read(&journald).contains("SystemMaxUse=300M\n") && journald_restarts() == 1);
    assert!(!root.join("etc/modules-load.d/90-fleet-conntrack.conf").exists(), "没有 conntrack 键：不写开机载入");
    // 原值快照：改之前的值，只有 root 能读
    let snap = read(&orig);
    assert!(snap.starts_with("# fleet 调优前的原值（tune-v2 首次调优时记下") && snap.contains(" prior=created\n"), "{snap}");
    for l in ["net.core.rmem_max = 212992\n", "net.ipv4.tcp_rmem = 4096 131072 6291456\n", "fs.file-max = 9223372036854775807\n", "net.ipv4.tcp_sack = 0\n", "net.ipv4.tcp_fin_timeout = \n"] {
        assert!(snap.contains(l), "{l} {snap}");
    }
    assert!(!snap.contains("nf_conntrack_max"), "没有的键不记");
    assert_eq!(std::fs::metadata(&orig).unwrap().permissions().mode() & 0o777, 0o600);
    // 步骤结果：逐键凭据、改前的值、推导参数；非容器里写不进的键记警告
    let r = jobs::tune::parse(&o.stdout);
    assert_eq!((&r.result["applied"], &r.result["kept"]), (&json!(["net.core.rmem_max", "net.ipv4.tcp_rmem", "net.ipv4.tcp_sack"]), &json!(["net.core.somaxconn", "fs.file-max", "net.ipv4.tcp_syncookies"])));
    assert_eq!(r.result["before"], json!({"net.core.rmem_max": "212992", "net.ipv4.tcp_rmem": "4096 131072 6291456", "net.ipv4.tcp_sack": "0"}));
    assert_eq!(
        r.result["params"],
        json!({"bw": 50, "bw_plan": 50, "bw_source": "plan", "rtt": 100, "rtt_source": "default", "rtt_samples": 0, "mem_mb": 3911, "buf_max": 16777216})
    );
    assert!(r.result["skipped"].as_array().unwrap().contains(&json!({"key": "net.ipv4.tcp_fin_timeout", "why": "readonly", "got": ""})), "{}", r.result);
    assert!(r.result["skipped"].as_array().unwrap().contains(&json!({"key": "net.netfilter.nf_conntrack_max", "why": "missing", "got": null})));
    assert!(r.warns.iter().any(|w| w.contains("net.ipv4.tcp_fin_timeout")) && !r.warns.iter().any(|w| w.contains("核对")), "{:?}", r.warns);
    assert_eq!((&r.result["route"], &r.result["warnings"]), (&Value::Null, &json!([])));

    // 重跑：快照不动，文件不变，原来 applied 的现在是 ok，journald 不再重启
    let before = std::fs::metadata(&conf).unwrap().modified().unwrap();
    let o = run(ip, jobs::tune::script(jobs::tune::Role::Node, Some(50))).await;
    assert_eq!((o.code, o.kv("journald"), o.kv("tune.net.core.rmem_max"), o.kv("snapshot"), journald_restarts()), (0, Some("same"), Some("ok"), Some("exists"), 1), "{o:?}");
    assert!(std::fs::metadata(&conf).unwrap().modified().unwrap() == before && read(&conf).lines().count() == 5);
    assert_eq!(read(&orig), snap);

    // 撤销：按快照写回（写不进的、本来就是原值的照报），删掉配置文件；快照留着
    let o = run(ip, jobs::tune::rollback_script()).await;
    assert_eq!((o.code, o.kv("tune_rollback"), o.kv("orig.net.core.rmem_max"), o.kv("orig.net.core.somaxconn")), (0, Some("done"), Some("applied:16777216"), Some("ok")), "{o:?}");
    assert_eq!(
        (read(&sysctl_file(&root, "net.core.rmem_max")), read(&sysctl_file(&root, "net.ipv4.tcp_rmem")), read(&sysctl_file(&root, "net.ipv4.tcp_sack"))),
        ("212992\n".into(), "4096 131072 6291456\n".into(), "0\n".into())
    );
    assert!(!conf.exists() && orig.exists());
    let r = jobs::tune::parse_rollback(&o.stdout).unwrap();
    assert!(r.result["applied"].as_array().unwrap().contains(&json!("net.ipv4.tcp_rmem")) && r.result["before"]["net.ipv4.tcp_sack"] == "1", "{}", r.result);

    // 线路机：入口端口池（该线路机自己的）要先保留，再放宽本地端口范围；入口连接的 RTT P90 推导缓冲区上限
    let root = FAKE.path().join(rip);
    put_meminfo(&root, 2_000_000);
    put_sysctl(&root, "net.ipv4.ip_local_reserved_ports", "20001-20999\n");
    put_sysctl(&root, "net.ipv4.ip_local_port_range", "32768\t60999\n");
    put_sysctl(&root, "net.ipv4.tcp_tw_reuse", "2\n");
    put_sysctl(&root, "net.netfilter.nf_conntrack_max", "65536\n");
    put_sysctl(&root, "net.ipv4.tcp_wmem", "4096\t16384\t4194304\n");
    // 入口连接 25 条（20000-29999 内，RTT 150…174 ms；含 v4 映射的 v6 地址），线路机到节点的出站连接 10 条（本地端口在池外，RTT 5 ms，不算）
    let mut ss = String::from("Recv-Q Send-Q Local Address:Port Peer Address:Port Process\n");
    for i in 0..25 {
        let local = if i % 5 == 0 { format!("[::ffff:10.0.0.5]:{}", 20000 + i) } else { format!("10.0.0.5:{}", 20000 + i) };
        ss.push_str(&format!("0 0 {local} 203.0.113.{i}:5{i:04}\n\t bbr wscale:7,7 rto:372 rtt:{}.4/12.3 ato:40 mss:1448 minrtt:120 rcv_rtt:3\n", 150 + i));
    }
    for i in 0..10 {
        ss.push_str(&format!("0 0 10.0.0.5:4{i:04} 198.51.100.{i}:8388\n\t bbr rtt:5/1 mss:1448\n"));
    }
    std::fs::write(root.join("ss-established"), ss).unwrap();
    // 第三方调优残留：tcpfit 的 sysctl 文件、网卡根上的 HTB；默认路由带供应商设的初始窗口
    std::fs::create_dir_all(root.join("etc/sysctl.d")).unwrap();
    std::fs::write(root.join("etc/sysctl.d/99-tcpfit.conf"), "net.core.rmem_max = 31457280\n").unwrap();
    std::fs::write(root.join("tc-qdisc"), "qdisc htb 1: dev eth0 root refcnt 2 r2q 10 default 0x10 direct_packets_stat 0\nqdisc fq 10: dev eth0 parent 1:10 limit 40960p\nqdisc noqueue 0: dev lo root refcnt 2\n").unwrap();
    std::fs::write(root.join("ip-route"), "default via 38.58.57.254 dev ens3 proto static initcwnd 10000 initrwnd 10000\n").unwrap();
    let o = run(rip, jobs::tune::script(jobs::tune::Role::Relay { port_min: 20000, port_max: 29999 }, Some(2560))).await;
    assert_eq!(o.code, 0, "{o:?}");
    // 25 条取第 23 小的 172.4 → 172；1000（2560 封顶）× 172 × 250 + 2 MiB
    assert_eq!((o.kv("bw"), o.kv("rtt"), o.kv("rtt_source"), o.kv("rtt_samples"), o.kv("buf_max")), (Some("1000"), Some("172"), Some("sampled"), Some("25"), Some("45097152")), "{o:?}");
    assert_eq!(read(&sysctl_file(&root, "net.ipv4.tcp_wmem")), "4096 16384 45097152");
    assert_eq!(read(&sysctl_file(&root, "net.ipv4.ip_local_reserved_ports")), "20000-39999,62789");
    assert_eq!((read(&sysctl_file(&root, "net.ipv4.ip_local_port_range")), read(&sysctl_file(&root, "net.ipv4.tcp_tw_reuse"))), ("10240 65535".into(), "1".into()));
    let c = read(&root.join("etc/sysctl.d/99-zz-fleet-tune.conf"));
    assert!(c.find("ip_local_reserved_ports = 20000-39999,62789\n").unwrap() < c.find("ip_local_port_range = 10240 65535").unwrap(), "{c}");
    // conntrack 键存在：设上，并让开机时 systemd-sysctl 之前先载入模块
    assert_eq!((o.kv("tune.net.netfilter.nf_conntrack_max"), read(&sysctl_file(&root, "net.netfilter.nf_conntrack_max"))), (Some("applied:65536"), "262144".into()));
    assert!(c.contains("net.netfilter.nf_conntrack_max = 262144\n"), "{c}");
    assert_eq!(read(&root.join("etc/modules-load.d/90-fleet-conntrack.conf")), "# fleet tune-v2（中控生成，勿手改）\nnf_conntrack\n");
    assert!(read(&root.join("systemctl.log")).contains("show -p LimitNOFILE --value fleet-xray"));
    let r = jobs::tune::parse(&o.stdout);
    assert_eq!((&r.result["params"]["bw_plan"], &r.result["params"]["rtt_samples"], &r.result["route"]), (&json!(2560), &json!(25), &json!({"initcwnd": 10000, "initrwnd": 10000})));
    assert!(r.warns.iter().any(|w| w.contains("99-tcpfit.conf")) && r.warns.iter().any(|w| w.contains("网卡 eth0 的根队列是 HTB")), "{:?}", r.warns);
    assert!(r.infos.iter().any(|i| i.contains("initcwnd 10000")), "{:?}", r.infos);
    assert!(root.join("etc/sysctl.d/99-tcpfit.conf").exists(), "残留只报告，不删");
    // 样本不够 20 条：用默认 200
    std::fs::write(root.join("ss-established"), "0 0 10.0.0.5:20001 203.0.113.1:50001\n\t bbr rtt:180/3 mss:1448\n").unwrap();
    let o = run(rip, jobs::tune::script(jobs::tune::Role::Relay { port_min: 20000, port_max: 29999 }, None)).await;
    assert_eq!((o.kv("bw_source"), o.kv("rtt"), o.kv("rtt_source"), o.kv("rtt_samples"), o.kv("buf_max")), (Some("default"), Some("200"), Some("default"), Some("1"), Some("52097152")), "{o:?}");
}

/// 作业 tune：成功后机器记下调优档与推导参数，视图带 tune；撤销后清掉；没有快照的撤销失败；同机 IP 节点、家宽代理 409；机器上有进行中的作业 409
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tune_job_records_the_profile() {
    let t = setup(false).await;
    let (m, root) = machine(&t, "192.0.2.47", "0.1.0+old", "0.1.0+new").await;
    put_meminfo(&root, 1_000_000);
    put_sysctl(&root, "net.core.rmem_max", "212992\n");
    let v = t.get(&format!("/api/v1/machines/{m}")).await;
    assert_eq!(v["tune"], json!({"profile": null, "at": null, "current": false, "params": null}), "迁移前上线的机器：未调优");

    let tr = &t;
    let post = |id: Uuid, body: Value| async move {
        let path = format!("/api/v1/machines/{id}/tune");
        tr.req("POST", &path, Some(body), None).await
    };
    // 没有快照时撤销：作业失败，什么都不改
    let (s, v) = post(m, json!({"rollback": true})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let j = run_round(&t, v["data"]["job_id"].as_str().unwrap()).await;
    assert!(j["state"] == "failed" && j.to_string().contains("原值快照"), "{j}");

    let (s, v) = post(m, json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let job = v["data"]["job_id"].as_str().unwrap().to_string();
    assert_eq!(post(m, json!({})).await.0, StatusCode::CONFLICT, "已有进行中的作业");
    let au = t.get("/api/v1/audit?cat=machine").await;
    assert_eq!((&au[0]["title"], &au[0]["summary"]), (&json!("系统调优"), &json!("团队1-01 · tune-v2")), "{au}");
    let j = run_round(&t, &job).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    assert_eq!((j["steps"][0]["output"]["profile"].as_str(), &j["steps"][0]["output"]["applied"]), (Some("tune-v2"), &json!(["net.core.rmem_max"])), "{j}");
    // 没有套餐：带宽按 1000，节点 RTT 100：1000 × 100 × 250 + 2 MiB（1 GB 内存的封顶 MemTotal/32 = 32000000 没碰到）
    assert_eq!(read(&sysctl_file(&root, "net.core.rmem_max")), "27097152");
    let v = t.get(&format!("/api/v1/machines/{m}")).await;
    assert_eq!((v["tune"]["profile"].as_str(), v["tune"]["current"].as_bool()), (Some("tune-v2"), Some(true)), "{v}");
    assert_eq!(
        v["tune"]["params"],
        json!({"bw": 1000, "bw_plan": null, "bw_source": "default", "rtt": 100, "rtt_source": "default", "rtt_samples": 0, "mem_mb": 976, "buf_max": 27097152})
    );
    sqlx::query("update machine set tune_profile = 'tune-v1' where id = $1").bind(m).execute(&t.st.db).await.unwrap();
    assert_eq!(t.get(&format!("/api/v1/machines/{m}")).await["tune"]["current"], false, "旧档");

    // 撤销：写回原值，档与参数清掉
    let (s, v) = post(m, json!({"rollback": true})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(t.get("/api/v1/audit?cat=machine").await[0]["summary"], "团队1-01 · 撤销调优");
    let j = run_round(&t, v["data"]["job_id"].as_str().unwrap()).await;
    assert!(j["state"] == "succeeded" && j["steps"][0]["output"]["rollback"] == true, "{j}");
    assert_eq!(read(&sysctl_file(&root, "net.core.rmem_max")), "212992\n");
    assert_eq!(t.get(&format!("/api/v1/machines/{m}")).await["tune"], json!({"profile": null, "at": null, "current": false, "params": null}));

    // 同机 IP 节点：指向宿主机；家宽代理没有我们的机器
    let ipn = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, lifecycle, host_id, ssh_port) select $1, 'vps', team_id, 2, '192.0.2.99'::inet, 'active', id, null from machine where id = $2")
        .bind(ipn)
        .bind(m)
        .execute(&t.st.db)
        .await
        .unwrap();
    let (s, v) = post(ipn, json!({})).await;
    assert!(s == StatusCode::CONFLICT && v.to_string().contains("对宿主机「团队1-01」调优"), "{s} {v}");
    assert_eq!(t.get(&format!("/api/v1/machines/{ipn}")).await["tune"], Value::Null);
    let team: Uuid = sqlx::query_scalar("select team_id from machine where id = $1").bind(m).fetch_one(&t.st.db).await.unwrap();
    let px = seed_node(&t, team, 3, "192.0.2.98", "active").await;
    let (s, v) = post(px, json!({})).await;
    assert!(s == StatusCode::CONFLICT && v.to_string().contains("家宽代理没有我们的机器"), "{s} {v}");
}
