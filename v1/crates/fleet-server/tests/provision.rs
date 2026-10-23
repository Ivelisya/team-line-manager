//! 上架时的登录试探：「检查」在 SSH 端口通了之后用中控密钥试登录（没填 root 密码又登不进的报错并给出中控公钥，
//! 登得进的读发行版、过期的给警告）；作业第一步「预检与身份」登不进又没密码时同样带上公钥，过期的发行版记一条 warn。
//! 需要 PostgreSQL（见 common）。
//!
//! 机器是假的：本机起一个只回 SSH 版本行的端口（过 `ssh_probe`），PATH 最前面放假 ssh 与假 ssh-keyscan。
//! 假 ssh 按 `-p` 的端口找「机器根目录」`<临时目录>/<端口>/`：里面有 refuse 时密钥登录被拒；否则把脚本里的 /etc/、/run/
//! 改到根目录下再在本机执行（os-release 读根目录里的）。每次调用的参数记进根目录的 ssh.log。
//! PATH 是整个测试进程共用的，所以单独一个测试文件；各测试用各自的端口，根目录互不相干。

#![cfg(unix)]

#[path = "it/common/mod.rs"]
mod common;

use common::{seed_relay, seed_team, setup, T};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tokio::io::AsyncWriteExt;

const SSH: &str = r#"#!/bin/sh
prev=
for a; do
  [ "$prev" = -p ] && port=$a
  case $a in root@*) ip=${a#root@};; esac
  prev=$a
done
R="$FAKE_ROOT/$port"
mkdir -p "$R"
echo "$*" >> "$R/ssh.log"
[ -e "$R/refuse" ] && { echo "root@$ip: Permission denied (publickey,password)." >&2; exit 255; }
sed "s#/etc/#$R/etc/#g; s#/run/#$R/run/#g" | bash -s
"#;

/// 主机公钥固定一把（作业预检记进测试自己的 known_hosts）
const KEYSCAN: &str = "#!/bin/sh\nfor a; do ip=$a; done\necho \"$ip ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostKeyOfTheFakeMachine\"\n";

/// 中控公钥（测试目录的 ssh/id_ed25519.pub）
const PUBKEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIControlKeyForProvisionTests fleet@test";

static FAKE: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    exe(&bin.join("ssh"), SSH);
    exe(&bin.join("ssh-keyscan"), KEYSCAN);
    std::env::set_var("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()));
    std::env::set_var("FAKE_ROOT", dir.path());
    dir
});

fn exe(p: &Path, body: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// 本机出网卡的地址（检查不连回环地址）
fn my_ip() -> String {
    let udp = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    udp.connect("192.0.2.1:9").unwrap();
    udp.local_addr().unwrap().ip().to_string()
}

/// 在 `ip` 上起一个只回 SSH 版本行的端口，返回端口与它的机器根目录。`os` 是根目录里 /etc/os-release 的 (ID, VERSION_ID)，
/// None = 密钥登录被拒
async fn machine(ip: &str, os: Option<(&str, &str)>) -> (u16, PathBuf) {
    LazyLock::force(&FAKE);
    let l = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let _ = s.write_all(b"SSH-2.0-OpenSSH_9.6\r\n").await;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            });
        }
    });
    let root = FAKE.path().join(port.to_string());
    std::fs::create_dir_all(root.join("etc")).unwrap();
    match os {
        Some((id, ver)) => std::fs::write(root.join("etc/os-release"), format!("ID={id}\nVERSION_ID=\"{ver}\"\n")).unwrap(),
        None => std::fs::write(root.join("refuse"), "").unwrap(),
    }
    (port, root)
}

fn write_pubkey(t: &T) {
    let d = t.dir.path().join("ssh");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("id_ed25519.pub"), format!("{PUBKEY}\n")).unwrap();
}

/// 检查：没填密码又登不进的不通过，错误里两条出路都有、公钥全文在最后；填了密码的照旧通过；
/// 登得进的读发行版，过期的给警告但照样通过；试登录不碰中控的 known_hosts。/meta 带中控公钥
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn check_tries_key_login() {
    let t = setup(false).await;
    write_pubkey(&t);
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let me = my_ip();
    let item = |port: u16, pw: Option<&str>| {
        let mut it = json!({"kind": "vps", "ip": me, "ssh_port": port, "team_id": team, "relay_id": relay});
        if let Some(p) = pw {
            it["login"] = json!({"password": p});
        }
        it
    };
    let check = |it: Value| {
        let t = &t;
        async move { t.post("/api/v1/provision/check", json!({"items": [it]})).await["items"][0].clone() }
    };

    let (refused, _) = machine(&me, None).await;
    let r = check(item(refused, None)).await;
    let err = r["errors"][0].as_str().unwrap_or_default();
    assert_eq!(r["ok"], false, "{r}");
    assert!(err.starts_with(&format!("中控密钥登录不了 {me}:{refused}（密钥被拒），也没填 root 密码")), "{err}");
    assert!(err.contains("--ssh-password-env") && err.contains("录入中控公钥") && err.ends_with(PUBKEY), "{err}");
    // 填了 root 密码：照旧通过（作业会用密码装上公钥），也不提醒
    let r = check(item(refused, Some("s3cret-root-password"))).await;
    assert_eq!((&r["ok"], &r["errors"], &r["warnings"]), (&json!(true), &json!([]), &json!([])), "{r}");
    // 空密码等于没填
    assert_eq!(check(item(refused, Some(""))).await["ok"], false);

    let (old, old_root) = machine(&me, Some(("ubuntu", "20.04"))).await;
    let r = check(item(old, None)).await;
    assert_eq!((&r["ok"], &r["errors"]), (&json!(true), &json!([])), "{r}");
    let w = r["warnings"].to_string();
    assert!(w.contains("系统是 Ubuntu 20.04，已过标准支持期") && w.contains("Ubuntu 22.04 起仍在支持"), "{r}");
    let (new, _) = machine(&me, Some(("debian", "12"))).await;
    let r = check(item(new, None)).await;
    assert_eq!((&r["ok"], &r["warnings"]), (&json!(true), &json!([])), "{r}");

    // 试登录只用密钥、不核对也不记录主机公钥：中控的 known_hosts 没有被写
    let log = std::fs::read_to_string(old_root.join("ssh.log")).unwrap();
    for opt in ["BatchMode=yes", "StrictHostKeyChecking=no", "UserKnownHostsFile=/dev/null", "PreferredAuthentications=publickey"] {
        assert!(log.contains(opt), "{opt}: {log}");
    }
    let kh = std::fs::read_to_string(t.dir.path().join("known_hosts")).unwrap_or_default();
    assert!(!kh.contains(&me), "检查不写 known_hosts：{kh}");

    // 家宽代理不 SSH，不试登录
    let r = check(json!({"kind": "proxy", "ip": "203.0.113.10", "team_id": team, "relay_id": relay,
                         "upstream": {"protocol": "socks5", "host": "p.example", "port": 1080}}))
    .await;
    assert_eq!((&r["ok"], &r["warnings"]), (&json!(true), &json!([])), "{r}");

    assert_eq!(t.get("/api/v1/meta").await["ssh_public_key"], PUBKEY);
}

/// 作业第一步：中控密钥登录不了、也没有 root 密码时失败（auth），错误里有中控公钥全文
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preflight_without_key_or_password_shows_pubkey() {
    let t = setup(true).await;
    write_pubkey(&t);
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let (port, _) = machine("127.0.0.1", None).await;
    let created = t.post("/api/v1/provision", json!({"items": [{"kind": "vps", "ip": "127.0.0.1", "ssh_port": port, "team_id": team, "relay_id": relay}]})).await;
    let j = t.wait_job(created["jobs"][0]["job_id"].as_str().unwrap(), "failed").await;
    let s = &j["steps"][0];
    let err = s["error"].as_str().unwrap();
    assert_eq!(s["error_kind"], "auth", "{j}");
    assert!(err.starts_with("中控密钥登录不了，也没有可用的 root 密码（") && err.contains("Permission denied"), "{err}");
    assert!(err.contains("fleetctl retry <作业> --password-env") && err.ends_with(&format!("录入中控公钥：{PUBKEY}")), "{err}");
}

/// 作业第一步：登录成功后发行版已过标准支持期的记一条 warn，不因此失败（这里之后因为假机器没有 systemd 才失败）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preflight_warns_eol_os() {
    let t = setup(true).await;
    let (relay, _) = seed_relay(&t).await;
    let team = seed_team(&t, "团队1").await;
    let (port, _) = machine("127.0.0.1", Some(("ubuntu", "20.04"))).await;
    let created = t.post("/api/v1/provision", json!({"items": [{"kind": "vps", "ip": "127.0.0.1", "ssh_port": port, "team_id": team, "relay_id": relay}]})).await;
    let job = created["jobs"][0]["job_id"].as_str().unwrap().to_string();
    let j = t.wait_job(&job, "failed").await;
    let err = j["steps"][0]["error"].as_str().unwrap_or_default();
    assert!(err.contains("systemd") || err.contains("x86_64"), "失败在发行版之后的检查上，不是因为发行版：{j}");
    let logs = t.get(&format!("/api/v1/jobs/{job}/logs")).await;
    let warn = logs.as_array().unwrap().iter().find(|l| l["msg"].as_str().unwrap_or("").contains("Ubuntu 20.04"));
    assert_eq!(warn.map(|l| &l["level"]), Some(&json!("warn")), "{logs}");
}
