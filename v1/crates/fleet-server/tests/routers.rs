//! 软路由：记录与接入命令、一次性接入脚本、重新接入换密钥与端口、删除撤销、改名、采集一轮（假 ssh 回一份采集结果）、告警、
//! 指标曲线与保留期、一键部署（假 ssh / sshpass / ssh-keyscan）。需要 PostgreSQL（见 common）。
//!
//! 路由是假的：PATH 最前面放假 ssh、sshpass、ssh-keyscan。远端按 `-p <端口>` 分「路由根目录」`<临时目录>/<端口>/`，脚本经 stdin 来，
//! 把 /etc/ 换到根目录里再用本机 sh 跑（/usr/bin/ssh 用本机的 OpenSSH）；采集脚本（含 FLEET_UC）回根目录里的 collect.json，
//! 延迟测试脚本（MODE='delay'）回 delay.json（没有时回「没测成」），取配置文件（含 head -c）回 config.yaml。根目录里有 refuse-key 时中控密钥登录被拒、有 down 时连不上、有 hostkey 时 keyscan 回它。
//! sshpass 核对 SSHPASS（环境变量）是不是根目录里的 password，并记下有没有拿到、参数里有什么。PATH 是整个进程共用的，所以单独一个测试文件。

#![cfg(unix)]
// 采集结果的 json! 字面量层数多
#![recursion_limit = "256"]

#[path = "it/common/mod.rs"]
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{seed_team, T};
use common::tele::Mock;
use fleet_server::routers::{self, alerts, collect};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

const SSH: &str = r#"#!/bin/sh
for a; do
  if [ "${next:-}" = 1 ]; then port=$a; next=; fi
  [ "$a" = -p ] && next=1
  [ "$a" = -O ] && exit 0
  last=$a
done
R="$FAKE_ROOT/${port:-22}"
mkdir -p "$R"
[ -e "$R/down" ] && { echo "ssh: connect to host 127.0.0.1 port $port: Connection refused" >&2; exit 255; }
case " $* " in *PubkeyAuthentication=no*) ;; *) [ -e "$R/refuse-key" ] && { echo "root@127.0.0.1: Permission denied (publickey)." >&2; exit 255; } ;; esac
[ "$last" = "sh -s" ] || exit 2
script=$(cat)
case $script in
  "MODE='delay'"*) cat "$R/delay.json" 2>/dev/null || echo '{"v":1,"now":0,"oc":{"delay":{"ok":false,"error":"没有 delay.json"}}}'; exit 0 ;;
  *FLEET_UC*) cat "$R/collect.json"; exit 0 ;;
  *"head -c"*) cat "$R/config.yaml"; exit 0 ;;
esac
printf '%s\n' "$script" | sed "s#/etc/#$R/etc/#g" | sh -s
"#;

const SSHPASS: &str = r#"#!/bin/sh
[ "$1" = -e ] && shift
for a; do
  if [ "${next:-}" = 1 ]; then port=$a; next=; fi
  [ "$a" = -p ] && next=1
done
R="$FAKE_ROOT/$port"
mkdir -p "$R"
printf 'env=%s args=%s\n' "${SSHPASS:+set}" "$*" >> "$R/sshpass.log"
[ "${SSHPASS:-}" = "$(cat "$R/password" 2>/dev/null)" ] || exit 5
exec "$@"
"#;

/// 回「[主机]:端口 ssh-ed25519 公钥」：根目录里有 hostkey 用它，有 no-sshd 什么都不回，否则用固定的一把
const KEYSCAN: &str = r##"#!/bin/sh
for a; do
  if [ "${next:-}" = 1 ]; then port=$a; next=; fi
  [ "$a" = -p ] && next=1
  host=$a
done
R="$FAKE_ROOT/$port"
[ -e "$R/no-sshd" ] && exit 0
k=$(cat "$R/hostkey" 2>/dev/null || echo "AAAAC3NzaC1lZDI1NTE5AAAAIFakeRouterHostKeyFakeRouterHostKeyFakeRouter1")
echo "# $host:$port SSH-2.0-dropbear_2024.86"
echo "[$host]:$port ssh-ed25519 $k"
"##;

static FAKE: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    exe(&bin.join("ssh"), SSH);
    exe(&bin.join("sshpass"), SSHPASS);
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

/// 某个端口的「路由根目录」
fn root(port: i64) -> PathBuf { FAKE.path().join(port.to_string()) }

fn read(p: &Path) -> String { std::fs::read_to_string(p).unwrap_or_default() }

const HOST_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICentralSshdHostKeyCentralSshdHostKeyCentr";
const LOGIN_PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICentralLoginKeyCentralLoginKeyCentralLogi fleet-central@test";

/// 测试环境：假的路由、中控 sshd 主机公钥与中控登录公钥都备好；通知渠道允许指向本机
async fn setup() -> T {
    LazyLock::force(&FAKE);
    let t = common::tele::setup().await;
    std::fs::create_dir_all(t.dir.path().join("ssh")).unwrap();
    std::fs::write(t.dir.path().join("ssh/id_ed25519.pub"), format!("{LOGIN_PUB}\n")).unwrap();
    std::fs::write(t.dir.path().join("ssh_host_ed25519_key.pub"), format!("{HOST_KEY} root@central\n")).unwrap();
    t
}

/// 不带会话的 GET（路由取接入脚本），返回 (状态码, 正文)
async fn fetch(t: &T, path: &str) -> (StatusCode, String) {
    let res = t.app.clone().oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap();
    let s = res.status();
    let ct = res.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    assert!(ct.starts_with("text/plain"), "{ct}");
    (s, String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
}

fn token_of(cmd: &str) -> String {
    let path = cmd.strip_prefix("curl -fsS http://fleet.test").and_then(|r| r.strip_suffix(" | sh")).unwrap_or_else(|| panic!("{cmd}"));
    path.rsplit('/').next().unwrap().to_string()
}

async fn router_row(t: &T, id: &str) -> (i32, Option<String>, Option<String>) {
    sqlx::query_as("select tunnel_port, tunnel_pubkey, host_key from router where id = $1::uuid").bind(id).fetch_one(&t.st.db).await.unwrap()
}

/// 新建一台（发接入命令的那种），返回 (id, 接入命令)
async fn create(t: &T, name: &str) -> (String, String) {
    let d = t.post("/api/v1/routers", json!({"name": name})).await;
    (d["id"].as_str().unwrap().to_string(), d["bootstrap"]["cmd"].as_str().unwrap().to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_list_and_validate() {
    let t = setup().await;
    let team = seed_team(&t, "团队2").await;
    let d = t.post("/api/v1/routers", json!({"name": " 团队2-路由A ", "team_id": team, "note": "客户现场"})).await;
    let id = d["id"].as_str().unwrap();
    let cmd = d["bootstrap"]["cmd"].as_str().unwrap();
    assert!(cmd.starts_with("curl -fsS http://fleet.test/api/v1/router-bootstrap/") && cmd.ends_with(" | sh"), "{cmd}");
    assert_eq!(token_of(cmd).len(), 43);
    let exp = OffsetDateTime::parse(d["bootstrap"]["expires_at"].as_str().unwrap(), &time::format_description::well_known::Rfc3339).unwrap();
    let left = (exp - OffsetDateTime::now_utc()).whole_minutes();
    assert!((29..=30).contains(&left), "30 分钟有效：{left}");
    let list = t.get("/api/v1/routers").await;
    let r = &list[0];
    assert_eq!((r["name"].as_str(), r["state"].as_str(), r["team"]["name"].as_str(), r["note"].as_str()), (Some("团队2-路由A"), Some("pending"), Some("团队2"), Some("客户现场")));
    assert!(r["summary"].is_null() && r["deploy"].is_null() && r["last_seen_at"].is_null() && r["error"].is_null() && r["bootstrap_expires_at"].is_string());
    assert!((23000..=23999).contains(&r["tunnel_port"].as_i64().unwrap()));
    let one = t.get(&format!("/api/v1/routers/{id}")).await;
    assert!(one["detail"].is_null() && one["id"] == r["id"]);
    // 同名 409，团队不存在 400，名字空 400
    let (s, v) = t.req("POST", "/api/v1/routers", Some(json!({"name": "团队2-路由A"})), None).await;
    assert_eq!((s, v["error"]["message"].as_str()), (StatusCode::CONFLICT, Some("已经有同名的软路由")));
    let (s, _) = t.req("POST", "/api/v1/routers", Some(json!({"name": "x", "team_id": Uuid::now_v7()})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = t.req("POST", "/api/v1/routers", Some(json!({"name": "  "})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = t.req("GET", &format!("/api/v1/routers/{}", Uuid::now_v7()), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // 只读密钥能看不能建
    let k = t.post("/api/v1/api-keys", json!({"name": "ro", "scopes": ["read"], "password": common::ADMIN_PW})).await;
    let tok = k["token"].as_str().unwrap();
    assert_eq!(t.req("GET", "/api/v1/routers", None, Some(tok)).await.0, StatusCode::OK);
    assert_eq!(t.req("POST", "/api/v1/routers", Some(json!({"name": "y"})), Some(tok)).await.0, StatusCode::FORBIDDEN);
    let audit = t.get(&format!("/api/v1/audit?target_id={id}")).await;
    assert_eq!((audit[0]["action"].as_str(), audit[0]["title"].as_str(), audit[0]["summary"].as_str()), (Some("router.create"), Some("新建软路由"), Some("团队2-路由A")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_script_is_one_time() {
    let t = setup().await;
    let (id, cmd) = create(&t, "路由B").await;
    let (port, _, _) = router_row(&t, &id).await;
    let tok = token_of(&cmd);
    let (s, script) = fetch(&t, &format!("/api/v1/router-bootstrap/{tok}")).await;
    assert_eq!(s, StatusCode::OK, "{script}");
    assert!(script.starts_with("#!/bin/sh\n") && script.contains("BEGIN OPENSSH PRIVATE KEY"));
    assert!(script.contains(&format!("CENTRAL_HOSTKEY='{HOST_KEY}'")) && script.contains(&format!("CENTRAL_PUBKEY='{LOGIN_PUB}'")));
    assert!(script.contains("SSH_HOST='fleet.test'\nSSH_PORT='22'\n") && script.contains(&format!("TUNNEL_PORT='{port}'")));
    // 公钥进了库和 sshd 读的清单；私钥不落库
    let (_, pubkey, host_key) = router_row(&t, &id).await;
    let pubkey = pubkey.expect("发了隧道公钥");
    assert!(host_key.is_none());
    assert_eq!(read(&t.dir.path().join("tunnel-keys")), format!("{port} {pubkey}\n"));
    let dump: String = sqlx::query_scalar("select row_to_json(r)::text from router r").fetch_one(&t.st.db).await.unwrap();
    assert!(!dump.contains("PRIVATE KEY"));
    // 只能取一次；过期的、乱写的都 404（回一小段打印原因的脚本）
    let (s, body) = fetch(&t, &format!("/api/v1/router-bootstrap/{tok}")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(body.contains("已过期或已经用过") && body.contains("exit 1"), "{body}");
    let (id2, cmd2) = create(&t, "路由C").await;
    sqlx::query("update router set bootstrap_expires_at = now() - interval '1 minute' where id = $1::uuid").bind(&id2).execute(&t.st.db).await.unwrap();
    assert_eq!(fetch(&t, &format!("/api/v1/router-bootstrap/{}", token_of(&cmd2))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(fetch(&t, "/api/v1/router-bootstrap/not-a-token").await.0, StatusCode::NOT_FOUND);
    // 取过脚本、还没连上：仍是 pending，不再显示接入命令的有效期
    let r = t.get(&format!("/api/v1/routers/{id}")).await;
    assert_eq!((r["state"].as_str(), r["bootstrap_expires_at"].is_null()), (Some("pending"), true));
    let audit = t.get(&format!("/api/v1/audit?target_id={id}")).await;
    assert_eq!((audit[0]["action"].as_str(), audit[0]["actor"].as_str(), audit[0]["title"].as_str()), (Some("router.bootstrap_used"), Some("router-bootstrap"), Some("路由取走接入脚本")));

    // 重新生成接入命令：旧隧道公钥立刻撤掉、端口换一个、马上显示等待接入（哪怕上一轮还连得上）；新令牌能用
    sqlx::query("insert into router_status (router_id, ts, reachable, last_ok_at) values ($1::uuid, now(), true, now())").bind(&id).execute(&t.st.db).await.unwrap();
    assert_eq!(t.get(&format!("/api/v1/routers/{id}")).await["state"], "online");
    let d = t.post(&format!("/api/v1/routers/{id}/bootstrap"), json!({})).await;
    let (port2, pk2, _) = router_row(&t, &id).await;
    assert!(pk2.is_none() && port2 != port);
    assert_eq!(t.get(&format!("/api/v1/routers/{id}")).await["state"], "pending");
    assert_eq!(read(&t.dir.path().join("tunnel-keys")), "");
    let (s, script) = fetch(&t, &format!("/api/v1/router-bootstrap/{}", token_of(d["cmd"].as_str().unwrap()))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(script.contains(&format!("TUNNEL_PORT='{port2}'")));
    let (_, pk3, _) = router_row(&t, &id).await;
    assert_ne!(pk3.as_deref(), Some(pubkey.as_str()), "换了一把");
    // 还没发过密钥的重新生成：端口不换
    let (p_before, _, _) = router_row(&t, &id2).await;
    t.post(&format!("/api/v1/routers/{id2}/bootstrap"), json!({})).await;
    assert_eq!(router_row(&t, &id2).await.0, p_before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_and_delete() {
    let t = setup().await;
    let team = seed_team(&t, "团队1").await;
    let (id, cmd) = create(&t, "旧名字").await;
    create(&t, "别的").await;
    let r = t.req("PATCH", &format!("/api/v1/routers/{id}"), Some(json!({"name": "新名字", "team_id": team, "note": "n"})), None).await.1;
    assert_eq!((r["data"]["name"].as_str(), r["data"]["team"]["name"].as_str(), r["data"]["note"].as_str()), (Some("新名字"), Some("团队1"), Some("n")));
    let r = t.req("PATCH", &format!("/api/v1/routers/{id}"), Some(json!({"team_id": null})), None).await.1;
    assert!(r["data"]["team"].is_null() && r["data"]["name"] == "新名字", "传 null 清掉团队，其余不动");
    assert_eq!(t.req("PATCH", &format!("/api/v1/routers/{id}"), Some(json!({"name": "别的"})), None).await.0, StatusCode::CONFLICT);
    // 取过脚本（清单里有它）、开着一条告警、有指标：删掉后都没了，告警关掉不发恢复
    assert_eq!(fetch(&t, &format!("/api/v1/router-bootstrap/{}", token_of(&cmd))).await.0, StatusCode::OK);
    assert!(!read(&t.dir.path().join("tunnel-keys")).is_empty());
    sqlx::query("insert into incident (id, rule, subject_id, severity, summary) values ($1, 'router_offline', $2::uuid, 'critical', '新名字：连不上')")
        .bind(Uuid::now_v7())
        .bind(&id)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into router_metric (router_id, ts, cpu_pct) values ($1::uuid, now(), 5)").bind(&id).execute(&t.st.db).await.unwrap();
    let (s, v) = t.req("DELETE", &format!("/api/v1/routers/{id}"), None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(v["data"]["ok"] == true && v["data"]["cleanup"].as_str().unwrap().contains("/etc/init.d/fleet-tunnel disable"));
    assert_eq!(read(&t.dir.path().join("tunnel-keys")), "");
    let inc: (String, Value) = sqlx::query_as("select state::text, detail from incident where subject_id = $1::uuid").bind(&id).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((inc.0.as_str(), inc.1["closed_by"].as_str()), ("resolved", Some("router_deleted")));
    assert_eq!(t.count(&format!("select count(*) from router_metric where router_id = '{id}'")).await, 0);
    assert_eq!(t.req("DELETE", &format!("/api/v1/routers/{id}"), None, None).await.0, StatusCode::NOT_FOUND);
    let audit = t.get(&format!("/api/v1/audit?target_id={id}")).await;
    assert_eq!(audit[0]["title"], "删除软路由");
}

// ── 采集一轮 ──────────────────────────────────────────────────────────────

const UUID_OK: &str = "6f1c2c1e-6a9b-4c55-9d3e-8a6f1b2c3d4e";

/// 一台线路机（relay.test，公钥 PBK）+ 团队1-01 挂在它的 20003 端口上（凭据 UUID_OK / abcd）
async fn seed_entry(t: &T) -> Uuid {
    let team = seed_team(t, "团队1").await;
    let (relay, node) = (Uuid::now_v7(), Uuid::now_v7());
    let db = &t.st.db;
    sqlx::query("insert into machine (id, kind, label, primary_ip, lifecycle) values ($1, 'relay', '线路机2', '198.51.100.2', 'active')").bind(relay).execute(db).await.unwrap();
    sqlx::query("insert into relay (machine_id, public_host, reality_private_enc, reality_public_key) values ($1, 'relay.test', '\\x00', 'PBK')").bind(relay).execute(db).await.unwrap();
    sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, lifecycle) values ($1, 'proxy', $2, 1, '203.0.113.10', 'active')").bind(node).bind(team).execute(db).await.unwrap();
    let e = Uuid::now_v7();
    sqlx::query("insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle) values ($1, $2, $3, 20003, $4::uuid, 'abcd', 'active')")
        .bind(e)
        .bind(relay)
        .bind(node)
        .bind(UUID_OK)
        .execute(db)
        .await
        .unwrap();
    e
}

fn collect_json(now: i64, rx: u64) -> Value {
    json!({
        "v": 1, "now": now, "cpu_pct": 12.5, "load": [0.49, 0.5, 0.4], "uptime": 16862, "procs": 120, "cores": 4,
        "cpu_model": "Intel(R) Celeron(R) CPU J1900", "mem_total": 4_000_000_000u64, "mem_avail": 3_000_000_000u64,
        "disk": {"path": "/overlay", "used": 100, "avail": 900}, "temp_c": 38.0, "conntrack": 1887, "conntrack_max": 65535,
        "os": "iStoreOS 24.10.2", "kernel": "6.6.86",
        "ifaces": [{"name": "wan", "up": true, "proto": "dhcp", "device": "eth0", "ip": "192.168.1.2", "uptime": 300},
                   {"name": "lan", "up": true, "proto": "static", "device": "br-lan", "ip": "192.168.124.1", "uptime": 300}],
        "dev": {"eth0": [rx, rx], "br-lan": [5, 5]},
        "oc": {
            "enabled": true, "running": true, "mode": "fake-ip-mix", "config_path": "/etc/openclash/config/c.yaml", "config_mtime": now - 100,
            "config_sha256": "sha-1", "rss": 52428800, "version": "alpha-g99e68e9", "up_total": rx, "down_total": rx * 4, "conns": 3, "conn_dt": 30,
            "exits": {"节点01": {"conns": 2, "up": 3000, "down": 30000, "clients": 1}},
            "clients": {"192.168.124.100": {"conns": 2, "exits": {"节点01": 2}}},
            "proxies": [{"name": "节点01", "type": "Vless", "alive": true, "delay": 88, "at": null},
                        {"name": "节点02", "type": "Vless", "alive": true, "delay": 90, "at": null},
                        {"name": "别人的", "type": "Shadowsocks", "alive": true, "delay": 10, "at": null}],
            "delay": {"ok": true, "results": {"节点01": 80, "节点02": 95}}
        },
        "leases": [["192.168.124.100", "aa:bb:cc:dd:ee:01", "laptop"]],
        "neigh": [["192.168.124.100", "br-lan", "aa:bb:cc:dd:ee:01", "REACHABLE"]]
    })
}

const CONFIG: &str = r#"
secret-should-be-gone: x
proxies:
  - name: 节点01
    type: vless
    server: relay.test
    port: 20003
    uuid: 6f1c2c1e-6a9b-4c55-9d3e-8a6f1b2c3d4e
    flow: xtls-rprx-vision
    servername: www.cloudflare.com
    reality-opts:
      public-key: PBK
      short-id: abcd
  - {name: 节点02, type: vless, server: 198.51.100.2, port: 20003, uuid: 00000000-0000-0000-0000-000000000000, flow: xtls-rprx-vision,
     servername: www.cloudflare.com, reality-opts: {public-key: PBK, short-id: abcd}}
  - {name: 别人的, type: ss, server: 203.0.113.99, port: 8388}
"#;

/// 假装路由一小时前就取走了接入脚本：发了隧道公钥
async fn issue_key(t: &T, id: &str) -> i64 {
    let (port,): (i32,) = sqlx::query_as(
        "update router set tunnel_pubkey = 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeTunnelKeyFakeTunnelKeyFakeTunnelKeyFake', tunnel_key_at = now() - interval '1 hour',
            bootstrap_token_hash = null where id = $1::uuid returning tunnel_port",
    )
    .bind(id)
    .fetch_one(&t.st.db)
    .await
    .unwrap();
    port as i64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_collection_round() {
    let t = setup().await;
    let entry = seed_entry(&t).await;
    let (id, _) = create(&t, "团队1-路由").await;
    let port = issue_key(&t, &id).await;
    let r = root(port);
    std::fs::create_dir_all(&r).unwrap();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    std::fs::write(r.join("collect.json"), collect_json(now, 1_000_000).to_string()).unwrap();
    std::fs::write(r.join("config.yaml"), CONFIG).unwrap();
    std::fs::write(r.join("delay.json"), json!({"v": 1, "now": now, "oc": {"delay": {"ok": true, "results": {"节点01": 80, "节点02": 95}}}}).to_string()).unwrap();
    let mut mem = Default::default();
    collect::round(&t.st, &mut mem).await.unwrap();
    let d = t.get(&format!("/api/v1/routers/{id}")).await;
    assert_eq!(d["state"], "online", "{d}");
    assert!(d["error"].is_null() && d["last_seen_at"].is_string());
    // 首次经隧道记下主机公钥，写进 router_known_hosts
    let (_, _, hk) = router_row(&t, &id).await;
    let hk = hk.unwrap();
    assert!(hk.starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeRouterHostKey"));
    assert_eq!(read(&t.dir.path().join("router_known_hosts")), format!("router-{id} {hk}\n"));
    let s = &d["summary"];
    assert_eq!((s["cpu_pct"].as_f64(), s["cores"].as_i64(), s["mem_used"].as_i64(), s["clients"].as_i64()), (Some(12.5), Some(4), Some(1_000_000_000), Some(1)));
    // 「别人的」这次没测通，但只不通 1 次：同告警，还算可用
    assert_eq!(s["exits"], json!({"total": 3, "alive": 3, "managed": 2, "drift": 1}));
    assert_eq!(s["openclash"]["version"], "alpha-g99e68e9");
    assert!(s["wan"]["rx_bps"].is_null(), "第一次没有速率");
    let ex = d["detail"]["exits"].as_array().unwrap();
    assert_eq!((ex[0]["name"].as_str(), ex[0]["sync"].as_str(), ex[0]["delay_ms"].as_i64()), (Some("节点01"), Some("ok"), Some(80)));
    assert_eq!(ex[0]["entry"]["id"], json!(entry));
    assert_eq!(ex[0]["entry"]["name"], "团队1-01 · 线路机2:20003");
    assert_eq!((ex[1]["sync"].as_str(), ex[2]["sync"].as_str(), ex[2]["entry"].is_null()), (Some("drift"), Some("unmanaged"), true));
    assert_eq!((ex[0]["conns"].as_i64(), ex[0]["up_bps"].as_i64()), (Some(2), Some(800)));
    assert!(ex.iter().all(|e| e.get("uuid").is_none() && !e.to_string().contains(UUID_OK)), "凭据不输出");
    assert_eq!(d["detail"]["clients"][0]["hostname"], "laptop");
    assert_eq!(t.count(&format!("select count(*) from router_metric where router_id = '{id}'")).await, 1);
    assert_eq!(t.count(&format!("select count(*) from router_exit_metric where router_id = '{id}' and delay_ms is not null")).await, 2);
    // 第二轮：有速率了；测延迟每 5 分钟一次，这一轮不测（exit 行 alive 为空）
    std::fs::write(r.join("collect.json"), collect_json(now + 30, 4_750_000).to_string()).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    collect::round(&t.st, &mut mem).await.unwrap();
    let s = t.get(&format!("/api/v1/routers/{id}")).await["summary"].clone();
    assert!(s["wan"]["rx_bps"].as_i64().unwrap() > 0, "{s}");
    // 连不上了：offline，错误写成人话，快照留着
    std::fs::write(r.join("down"), "").unwrap();
    collect::round(&t.st, &mut mem).await.unwrap();
    let d = t.get(&format!("/api/v1/routers/{id}")).await;
    assert_eq!(d["state"], "offline");
    assert!(d["error"].as_str().unwrap().starts_with("经隧道连不上路由："), "{}", d["error"]);
    assert!(d["summary"].is_object());
    std::fs::remove_file(r.join("down")).unwrap();
}

/// 连得上但采集脚本出错：不算离线（在线、带原因、快照留着），告警是「软路由采集出错」（警告、满 60 秒）而不是离线；
/// 延迟测试没测成只影响出口数据；配置解析不了时出口沿用上次对上的入口
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collect_error_is_not_offline() {
    let t = setup().await;
    let entry = seed_entry(&t).await;
    let (id, _) = create(&t, "团队1-路由E").await;
    issue_key(&t, &id).await;
    // 各测试的库是分开的、分到的隧道端口都从段首起，假路由的根目录按端口分：换一个别的测试不用的
    let port = 23950;
    sqlx::query("update router set tunnel_port = $2 where id = $1::uuid").bind(&id).bind(port as i32).execute(&t.st.db).await.unwrap();
    let r = root(port);
    std::fs::create_dir_all(&r).unwrap();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    std::fs::write(r.join("collect.json"), collect_json(now, 1_000_000).to_string()).unwrap();
    std::fs::write(r.join("config.yaml"), CONFIG).unwrap();
    // 延迟测试没测成（没有 delay.json）：出口沿用 mihomo 自己记的，原因记下
    let mut mem = Default::default();
    collect::round(&t.st, &mut mem).await.unwrap();
    let d = t.get(&format!("/api/v1/routers/{id}")).await;
    assert_eq!(d["state"], "online", "{d}");
    assert_eq!(d["detail"]["openclash"]["delay_error"], "没有 delay.json");
    assert_eq!(d["detail"]["exits"][0]["fail_streak"], 0);
    // 配置改坏了（解析不了）：出口照样对上原来的入口，标出沿用
    std::fs::write(r.join("collect.json"), { let mut c = collect_json(now + 30, 2_000_000); c["oc"]["config_sha256"] = json!("sha-2"); c }.to_string()).unwrap();
    std::fs::write(r.join("config.yaml"), "proxies: [ {name: 坏的").unwrap();
    collect::round(&t.st, &mut mem).await.unwrap();
    let d = t.get(&format!("/api/v1/routers/{id}")).await;
    let oc = &d["detail"]["openclash"];
    assert!(oc["config_error"].as_str().unwrap().starts_with("配置文件解析不了") && oc["config_stale"] == true, "{oc}");
    assert_eq!((d["detail"]["exits"][0]["sync"].as_str(), d["detail"]["exits"][0]["entry"]["id"].clone()), (Some("ok"), json!(entry)));
    // 中控重启过（内存里没有上次解析成功的）：沿用上一份快照
    let mut fresh = Default::default();
    std::fs::write(r.join("collect.json"), { let mut c = collect_json(now + 60, 3_000_000); c["oc"]["config_sha256"] = json!("sha-2"); c }.to_string()).unwrap();
    collect::round(&t.st, &mut fresh).await.unwrap();
    let d = t.get(&format!("/api/v1/routers/{id}")).await;
    let x = d["detail"]["exits"].as_array().unwrap().iter().find(|x| x["name"] == "节点01").unwrap();
    assert_eq!((d["detail"]["openclash"]["config_stale"].as_bool(), x["entry"]["id"].clone()), (Some(true), json!(entry)), "{d}");
    // 采集脚本出错：在线、带原因，快照还是上一份
    let collected = d["detail"]["collected_at"].clone();
    std::fs::write(r.join("collect.json"), "Syntax error: unexpected token").unwrap();
    collect::round(&t.st, &mut fresh).await.unwrap();
    let d = t.get(&format!("/api/v1/routers/{id}")).await;
    assert_eq!(d["state"], "online", "{d}");
    assert_eq!(d["error"], "采集脚本出错（退出码 0）：Syntax error: unexpected token");
    assert_eq!(d["detail"]["collected_at"], collected);
    let mut st = alerts::AlertState::default();
    let t0 = OffsetDateTime::now_utc();
    alerts::evaluate(&t.st, &mut st, t0).await.unwrap();
    assert!(incidents(&t, "firing").await.is_empty(), "满 60 秒才开");
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(61)).await.unwrap();
    let inc = incidents(&t, "firing").await;
    assert_eq!(inc.len(), 1, "{inc:?}");
    assert_eq!((inc[0]["rule"].as_str(), inc[0]["severity"].as_str(), inc[0]["rule_name"].as_str()), (Some("router_collect_error"), Some("warning"), Some("软路由采集出错")));
    assert_eq!(inc[0]["summary"], "团队1-路由E：连得上，但采集出错：采集脚本出错（退出码 0）：Syntax error: unexpected token");
    // 好了：满 60 秒恢复
    std::fs::write(r.join("collect.json"), collect_json(now + 90, 4_000_000).to_string()).unwrap();
    collect::round(&t.st, &mut fresh).await.unwrap();
    assert!(t.get(&format!("/api/v1/routers/{id}")).await["error"].is_null());
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(100)).await.unwrap();
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(161)).await.unwrap();
    assert!(incidents(&t, "firing").await.is_empty());
}

// ── 告警 ──────────────────────────────────────────────────────────────────

async fn incidents(t: &T, state: &str) -> Vec<Value> {
    t.get(&format!("/api/v1/incidents?state={state}")).await["items"].as_array().cloned().unwrap_or_default()
}

/// 直接写一份最近采集：`ok_ago` 秒前最后一次连上，此刻 reachable，快照 data
async fn status(t: &T, id: &str, reachable: bool, ok_ago: i64, data: Value) {
    sqlx::query(
        "insert into router_status (router_id, ts, reachable, error, data, last_ok_at) values ($1::uuid, now(), $2, $3, $4, now() - make_interval(secs => $5))
         on conflict (router_id) do update set ts = now(), reachable = $2, error = $3, data = $4, last_ok_at = now() - make_interval(secs => $5)",
    )
    .bind(id)
    .bind(reachable)
    .bind((!reachable).then_some("经隧道连不上路由：Connection refused"))
    .bind(data)
    .bind(ok_ago as f64)
    .execute(&t.st.db)
    .await
    .unwrap();
}

fn snap(running: bool, streaks: [u32; 2], wan_up: bool) -> Value {
    json!({
        "system": {"cpu_pct": 10.0, "mem_used": 1, "mem_total": 4},
        "interfaces": [{"name": "wan", "device": "eth0", "up": wan_up}],
        "openclash": {"enabled": true, "running": running},
        "exits": [{"name": "节点01", "fail_streak": streaks[0], "sync": "ok"}, {"name": "节点02", "fail_streak": streaks[1], "sync": "ok"},
                  {"name": "别人的", "fail_streak": 9, "sync": "unmanaged"}],
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_open_notify_and_recover() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "webhook", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    let (id, _) = create(&t, "团队2-路由A").await;
    issue_key(&t, &id).await;
    let mut st = alerts::AlertState::default();
    let t0 = OffsetDateTime::now_utc();
    // 连不上 90 秒：还不报；5 分钟：报离线（严重），对象是路由
    status(&t, &id, false, 90, snap(true, [0, 0], true)).await;
    alerts::evaluate(&t.st, &mut st, t0).await.unwrap();
    assert!(incidents(&t, "firing").await.is_empty());
    status(&t, &id, false, 300, snap(true, [0, 0], true)).await;
    alerts::evaluate(&t.st, &mut st, t0).await.unwrap();
    let inc = incidents(&t, "firing").await;
    assert_eq!(inc.len(), 1, "{inc:?}");
    assert_eq!((inc[0]["rule"].as_str(), inc[0]["severity"].as_str(), inc[0]["rule_name"].as_str()), (Some("router_offline"), Some("critical"), Some("软路由离线")));
    assert_eq!(inc[0]["subject"], json!({"id": id, "name": "团队2-路由A", "kind": "router"}));
    assert_eq!(inc[0]["subject_name"], "团队2-路由A");
    let summary = inc[0]["summary"].as_str().unwrap();
    assert!(summary.starts_with("团队2-路由A：连不上已 ") && summary.ends_with(" 分钟：经隧道连不上路由：Connection refused"), "{summary}");
    mock.wait("/hook/a", 1).await;
    let b = mock.bodies("/hook/a").pop().unwrap();
    assert_eq!((b["event"].as_str(), b["rule"].as_str(), b["machine"].as_str()), (Some("firing"), Some("router_offline"), Some("团队2-路由A")));
    // 连不上时其余几条无从判断：快照里 OpenClash 没在跑也不开
    status(&t, &id, false, 400, snap(false, [3, 3], false)).await;
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(100)).await.unwrap();
    assert_eq!(incidents(&t, "firing").await.len(), 1);
    // 连上了：离线满 60 秒恢复（发恢复）；出口两个都连续两次不通 → 出口告警立刻开、严重；OpenClash、WAN 要满 60 秒
    status(&t, &id, true, 0, snap(false, [2, 3], false)).await;
    let t1 = t0 + time::Duration::seconds(200);
    alerts::evaluate(&t.st, &mut st, t1).await.unwrap();
    let rules = |v: &[Value]| v.iter().map(|i| i["rule"].as_str().unwrap().to_string()).collect::<std::collections::BTreeSet<_>>();
    let firing = incidents(&t, "firing").await;
    assert_eq!(rules(&firing), ["router_exits_down", "router_offline"].map(String::from).into());
    let ex = firing.iter().find(|i| i["rule"] == "router_exits_down").unwrap();
    assert_eq!(ex["severity"], "critical");
    alerts::evaluate(&t.st, &mut st, t1 + time::Duration::seconds(61)).await.unwrap();
    let firing = incidents(&t, "firing").await;
    assert_eq!(rules(&firing), ["router_exits_down", "router_openclash_down", "router_wan_down"].map(String::from).into());
    mock.wait("/hook/a", 5).await;
    let events: Vec<(String, String)> = mock.bodies("/hook/a").iter().map(|b| (b["event"].as_str().unwrap().into(), b["rule"].as_str().unwrap().into())).collect();
    assert!(events.contains(&("resolved".into(), "router_offline".into())), "{events:?}");
    // 发送记录里对象也是路由
    let log = t.get("/api/v1/notify-log?limit=20").await;
    let l = log.as_array().unwrap().iter().find(|l| l["kind"] == "firing").unwrap();
    assert_eq!((l["subject"]["kind"].as_str(), l["subject_name"].as_str(), l["rule_name"].as_str().is_some()), (Some("router"), Some("团队2-路由A"), true));
    // 好了：满 60 秒全部恢复
    status(&t, &id, true, 0, snap(true, [0, 0], true)).await;
    alerts::evaluate(&t.st, &mut st, t1 + time::Duration::seconds(120)).await.unwrap();
    alerts::evaluate(&t.st, &mut st, t1 + time::Duration::seconds(181)).await.unwrap();
    assert!(incidents(&t, "firing").await.is_empty());
    // 全局静默（只静默这一条规则）：不开
    let now = OffsetDateTime::now_utc();
    let at = |m: i64| (now + time::Duration::minutes(m)).format(&time::format_description::well_known::Rfc3339).unwrap();
    t.post("/api/v1/silences", json!({"rule": "router_wan_down", "starts_at": at(-1), "ends_at": at(60), "comment": "换光猫"})).await;
    status(&t, &id, true, 0, snap(true, [0, 0], false)).await;
    alerts::evaluate(&t.st, &mut st, t1 + time::Duration::seconds(300)).await.unwrap();
    alerts::evaluate(&t.st, &mut st, t1 + time::Duration::seconds(400)).await.unwrap();
    assert!(incidents(&t, "firing").await.is_empty());
    // 删路由：开着的告警关掉、不发恢复
    status(&t, &id, false, 600, snap(true, [0, 0], true)).await;
    alerts::evaluate(&t.st, &mut st, t1 + time::Duration::seconds(500)).await.unwrap();
    assert_eq!(incidents(&t, "firing").await.len(), 1);
    let sent = mock.count("/hook/a");
    assert_eq!(t.req("DELETE", &format!("/api/v1/routers/{id}"), None, None).await.0, StatusCode::OK);
    alerts::evaluate(&t.st, &mut st, t1 + time::Duration::seconds(600)).await.unwrap();
    let all = incidents(&t, "all").await;
    assert!(all.iter().all(|i| i["state"] == "resolved"));
    assert_eq!(all.iter().find(|i| i["rule"] == "router_offline" && i["subject"]["name"] == "（已删除）").map(|i| i["subject"]["kind"].clone()), Some(json!("router")));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock.count("/hook/a"), sent, "删掉的路由不发恢复");
}

/// 重新接入：开着的离线告警悄悄关掉（不发「已恢复」）；接入过的 30 分钟还没连上开离线警告
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebootstrap_closes_offline_quietly_then_warns() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "webhook", "config": {"url": mock.url("/hook/r")}, "min_severity": "info", "enabled": true})).await;
    let (id, _) = create(&t, "团队2-路由R").await;
    issue_key(&t, &id).await;
    let mut st = alerts::AlertState::default();
    let t0 = OffsetDateTime::now_utc();
    status(&t, &id, false, 600, snap(true, [0, 0], true)).await;
    alerts::evaluate(&t.st, &mut st, t0).await.unwrap();
    assert_eq!(incidents(&t, "firing").await.len(), 1);
    mock.wait("/hook/r", 1).await;
    t.post(&format!("/api/v1/routers/{id}/bootstrap"), json!({})).await;
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(30)).await.unwrap();
    assert!(incidents(&t, "firing").await.is_empty());
    let all = incidents(&t, "all").await;
    assert_eq!(all[0]["state"], "resolved");
    let closed_by: Option<String> = sqlx::query_scalar("select detail->>'closed_by' from incident where subject_id = $1::uuid").bind(&id).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(closed_by.as_deref(), Some("router_rebootstrap"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock.count("/hook/r"), 1, "不发恢复");
    // 命令过期没人执行：30 分钟后开离线警告
    sqlx::query("update router set bootstrap_expires_at = now() - interval '1 minute' where id = $1::uuid").bind(&id).execute(&t.st.db).await.unwrap();
    alerts::evaluate(&t.st, &mut st, OffsetDateTime::now_utc()).await.unwrap();
    let inc = incidents(&t, "firing").await;
    assert_eq!(inc.len(), 1, "{inc:?}");
    assert_eq!((inc[0]["rule"].as_str(), inc[0]["severity"].as_str()), (Some("router_offline"), Some("warning")));
    assert!(inc[0]["summary"].as_str().unwrap().starts_with("团队2-路由R：重新接入后还没连上（已 31 分钟"), "{}", inc[0]["summary"]);
}

/// 中控停机的时段不算路由连不上：停之前刚连上过的，恢复后第一轮还没连上不报离线（同 agent）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn central_downtime_is_not_router_silence() {
    let t = setup().await;
    let (id, _) = create(&t, "团队2-路由G").await;
    issue_key(&t, &id).await;
    // 中控 5 分钟前停的、刚刚起来
    sqlx::query("insert into app_setting (key, value, updated_by) values ('central_alive_at', to_jsonb((now() - interval '300 seconds')::text), 't') on conflict (key) do update set value = excluded.value")
        .execute(&t.st.db)
        .await
        .unwrap();
    t.st.report_gaps.started(&t.st.db).await.unwrap();
    status(&t, &id, false, 320, snap(true, [0, 0], true)).await;
    let mut st = alerts::AlertState::default();
    alerts::evaluate(&t.st, &mut st, OffsetDateTime::now_utc()).await.unwrap();
    assert!(incidents(&t, "firing").await.is_empty(), "扣掉停机的 5 分钟，只沉默了 20 秒");
    status(&t, &id, false, 600, snap(true, [0, 0], true)).await;
    alerts::evaluate(&t.st, &mut st, OffsetDateTime::now_utc()).await.unwrap();
    assert_eq!(incidents(&t, "firing").await.len(), 1, "停机之前就连不上的照样离线");
}

/// 删路由：还有没做完的作业改过它的出口、没确认改回时 409，给出作业 id
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_blocked_by_unfinished_router_job() {
    let t = setup().await;
    let (id, _) = create(&t, "团队2-路由D").await;
    let job = Uuid::now_v7();
    sqlx::query("insert into job (id, kind, params, state, created_by) values ($1, 'replace', $2, 'failed', 't')")
        .bind(job)
        .bind(json!({"routers_applied": [{"id": id, "name": "团队2-路由D", "port": 23000, "stamp": "S"}]}))
        .execute(&t.st.db)
        .await
        .unwrap();
    let (s, v) = t.req("DELETE", &format!("/api/v1/routers/{id}"), None, None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains(&job.to_string()), "{v}");
    assert_eq!(v["error"]["detail"]["jobs"], json!([job]));
    sqlx::query("update job set state = 'succeeded' where id = $1").bind(job).execute(&t.st.db).await.unwrap();
    assert_eq!(t.req("DELETE", &format!("/api/v1/routers/{id}"), None, None).await.0, StatusCode::OK);
}

/// 轮换凭据 / 停用入口：列出指向它的软路由出口（确认处与结果里）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn entry_actions_list_router_exits() {
    let t = setup().await;
    let entry = seed_entry(&t).await;
    let (id, _) = create(&t, "团队1-路由X").await;
    let ex = |e: Option<Uuid>| json!({"name": "节点01", "sync": "ok", "entry": e.map(|e| json!({"id": e, "name": "x", "node_id": e, "relay_id": e, "node_name": "", "relay_name": "", "port": 20003, "lifecycle": "active"}))});
    status(&t, &id, true, 0, json!({"exits": [ex(Some(entry)), {"name": "别人的", "sync": "unmanaged"}]})).await;
    let rs = t.get(&format!("/api/v1/entries/{entry}/routers")).await;
    assert_eq!(rs, json!([{"id": id, "name": "团队1-路由X", "state": "online", "exits": ["节点01"]}]));
    let (s, v) = t.req("POST", &format!("/api/v1/entries/{entry}/actions"), Some(json!({"action": "rotate"})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().ends_with("轮换后会连不上、要到路由上换成新凭据：团队1-路由X 的 节点01"), "{v}");
    assert_eq!(v["error"]["detail"]["router_exits"], rs);
    let d = t.post(&format!("/api/v1/entries/{entry}/actions"), json!({"action": "disable"})).await;
    assert_eq!(d["router_exits"], rs);
    assert_eq!(t.req("GET", &format!("/api/v1/entries/{}/routers", Uuid::now_v7()), None, None).await.0, StatusCode::NOT_FOUND);
}

// ── 指标曲线与保留期 ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_rollup_and_retention() {
    let t = setup().await;
    let (id, _) = create(&t, "路由M").await;
    let db = &t.st.db;
    for (ago, cpu) in [(600, 10.0f32), (300, 30.0), (5 * 86400, 99.0)] {
        sqlx::query(
            "insert into router_metric (router_id, ts, cpu_pct, mem_used, mem_total, conntrack, conntrack_max, wan_rx_bps, conns, clients, exits_total, exits_alive)
             values ($1::uuid, now() - make_interval(secs => $2), $3, 50, 100, 10, 100, 8000, 4, 2, 3, 3)",
        )
        .bind(&id)
        .bind(ago as f64)
        .bind(cpu)
        .execute(db)
        .await
        .unwrap();
    }
    for (ago, alive, delay) in [(900, Some(false), None), (600, Some(true), Some(80)), (300, None, None)] {
        sqlx::query("insert into router_exit_metric (router_id, ts, exit, alive, delay_ms, conns) values ($1::uuid, now() - make_interval(secs => $2), '节点01', $3, $4, 2)")
            .bind(&id)
            .bind(ago as f64)
            .bind(alive)
            .bind(delay)
            .execute(db)
            .await
            .unwrap();
    }
    let m = t.get(&format!("/api/v1/routers/{id}/metrics?range=1h")).await;
    assert_eq!(m["step_s"], 30);
    let p = m["points"].as_array().unwrap();
    assert_eq!(p.len(), 2);
    assert_eq!((p[0]["cpu_pct"].as_f64(), p[0]["mem_pct"].as_f64(), p[0]["conntrack_pct"].as_f64(), p[0]["wan_rx_bps"].as_i64()), (Some(10.0), Some(50.0), Some(10.0), Some(8000)));
    for k in ["ts", "load1", "disk_pct", "temp_c", "wan_tx_bps", "proxy_up_bps", "proxy_down_bps", "conns", "clients", "exits_alive"] {
        assert!(p[0].get(k).is_some(), "{k}");
    }
    assert_eq!(t.get(&format!("/api/v1/routers/{id}/metrics?range=24h")).await["step_s"], 300);
    let e = t.get(&format!("/api/v1/routers/{id}/exits/metrics?range=1h")).await;
    assert_eq!((e["step_s"].as_i64(), e["exits"][0]["name"].as_str()), (Some(30), Some("节点01")));
    let pts = e["exits"][0]["points"].as_array().unwrap();
    assert_eq!((pts[1]["delay_ms"].as_i64(), pts[1]["alive"].as_bool(), pts[2]["alive"].is_null()), (Some(80), Some(true), true));
    // 每个点带测了几次、通了几次：原始点没测的是 0 / 0
    let tests = |pts: &Value| -> Vec<(i64, i64)> { pts.as_array().unwrap().iter().map(|p| (p["tests"].as_i64().unwrap(), p["tests_ok"].as_i64().unwrap())).collect() };
    let sum = |v: Vec<(i64, i64)>| v.iter().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    assert_eq!(tests(&e["exits"][0]["points"]), [(1, 0), (1, 1), (0, 0)]);
    let e5 = t.get(&format!("/api/v1/routers/{id}/exits/metrics?range=24h")).await;
    assert_eq!((e5["step_s"].as_i64(), sum(tests(&e5["exits"][0]["points"]))), (Some(300), (2, 1)), "{e5}");
    // 小时汇总
    collect::rollup(db).await.unwrap();
    let h = t.get(&format!("/api/v1/routers/{id}/metrics?range=7d")).await;
    assert_eq!(h["step_s"], 3600);
    assert!(!h["points"].as_array().unwrap().is_empty() && h["points"][0].get("cpu_max").is_some(), "{h}");
    let eh = t.get(&format!("/api/v1/routers/{id}/exits/metrics?range=30d")).await;
    let hp = &eh["exits"][0]["points"];
    assert_eq!(sum(tests(hp)), (2, 1), "{eh}");
    // 两次测试可能落在同一小时（50%）或两个小时（0% 与 100%）：alive_pct 就是这一小时的 tests_ok / tests
    for p in hp.as_array().unwrap() {
        let (n, k) = (p["tests"].as_f64().unwrap(), p["tests_ok"].as_f64().unwrap());
        assert_eq!(p["alive_pct"].as_f64(), (n > 0.0).then(|| 100.0 * k / n), "{p}");
        assert_eq!(p["alive"].as_bool(), (n > 0.0).then_some(k == n), "{p}");
    }
    assert_eq!(t.req("GET", &format!("/api/v1/routers/{id}/metrics?range=2h"), None, None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.req("GET", &format!("/api/v1/routers/{}/metrics", Uuid::now_v7()), None, None).await.0, StatusCode::NOT_FOUND);
    // 保留期：原始点超过 raw_days 的删掉
    collect::cleanup(db, 3).await.unwrap();
    assert_eq!(t.count(&format!("select count(*) from router_metric where router_id = '{id}'")).await, 2);
}

// ── 一键部署 ──────────────────────────────────────────────────────────────

/// 一个「路由」：本机监听一个端口（中控要先连得上 SSH 端口），根目录里备好 dropbear 目录、rc.common 桩、密码
fn fake_router(password: &str) -> (std::net::TcpListener, i64, PathBuf) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port() as i64;
    let r = root(port);
    std::fs::create_dir_all(r.join("etc/dropbear")).unwrap();
    std::fs::create_dir_all(r.join("etc/init.d")).unwrap();
    std::fs::write(r.join("etc/rc.common"), format!("echo \"$2\" >> {}/rc.log\n", r.display())).unwrap();
    std::fs::write(r.join("password"), password).unwrap();
    (l, port, r)
}

async fn wait_deploy(t: &T, id: &str) -> Value {
    for _ in 0..200 {
        let d = t.get(&format!("/api/v1/routers/{id}")).await;
        if d["deploy"]["state"] != "running" {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    panic!("部署 30 秒没有结束");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_click_deploy() {
    let t = setup().await;
    let (_l, sport, r) = fake_router("pw-test-123");
    let d = t.post("/api/v1/routers", json!({"name": "部署的", "ssh": {"host": "127.0.0.1", "port": sport, "user": "root", "password": "pw-test-123"}})).await;
    assert!(d.get("bootstrap").is_none(), "一键部署不发接入命令：{d}");
    let id = d["id"].as_str().unwrap().to_string();
    let d = wait_deploy(&t, &id).await;
    let dep = &d["deploy"];
    assert_eq!((dep["state"].as_str(), dep["step"].is_null(), dep["error"].is_null()), (Some("done"), true, true), "{dep}");
    assert_eq!((dep["host"].as_str(), dep["port"].as_i64(), dep["user"].as_str()), (Some("127.0.0.1"), Some(sport), Some("root")));
    assert!(dep["started_at"].is_string() && dep["finished_at"].is_string());
    let (port, pubkey, hk) = router_row(&t, &id).await;
    assert!(hk.unwrap().contains("FakeRouterHostKey"), "登录时记下主机公钥");
    assert_eq!(read(&t.dir.path().join("tunnel-keys")), format!("{port} {}\n", pubkey.unwrap()));
    // 路由上：密钥、中控主机公钥、中控登录公钥、启动脚本
    assert!(read(&r.join("etc/fleet-tunnel/id_ed25519")).starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
    assert_eq!(read(&r.join("etc/fleet-tunnel/known_hosts")), format!("fleet-central {HOST_KEY}\n"));
    assert_eq!(read(&r.join("etc/dropbear/authorized_keys")), format!("{LOGIN_PUB}\n"));
    assert!(read(&r.join("etc/init.d/fleet-tunnel")).contains(&format!("-p 22 -R 127.0.0.1:{port}:127.0.0.1:22 fleet-tunnel@fleet.test")));
    assert_eq!(read(&r.join("rc.log")), "enable\nrestart\n");
    // 密码经环境变量交给 sshpass，不在参数里；审计不记密码
    let log = read(&r.join("sshpass.log"));
    assert!(log.lines().count() >= 3 && log.lines().all(|l| l.starts_with("env=set ")) && !log.contains("pw-test-123"), "{log}");
    let audit = t.get(&format!("/api/v1/audit?target_id={id}")).await;
    let a = audit.as_array().unwrap().iter().find(|a| a["action"] == "router.deploy").unwrap();
    assert_eq!((a["detail"]["result"].as_str(), a["detail"]["password"].as_bool(), a["detail"].get("started").is_none()), (Some("ok"), Some(true), true));
    assert!(!audit.to_string().contains("pw-test-123"));
    assert_eq!(a["summary"], format!("部署的 · root@127.0.0.1:{sport}（密码）· 成功"));

    // 重新部署、密码不对：失败原因是人话；中控已发过密钥的会换端口——这里登录这一步就失败了，什么都不改
    let (s, v) = t.req("POST", &format!("/api/v1/routers/{id}/deploy"), Some(json!({"host": "127.0.0.1", "port": sport, "password": "wrong"})), None).await;
    assert_eq!((s, &v["data"]), (StatusCode::OK, &json!({"ok": true})));
    let d = wait_deploy(&t, &id).await;
    assert_eq!(d["deploy"]["state"], "failed");
    let err = d["deploy"]["error"].as_str().unwrap();
    assert!(err.starts_with("登录路由失败：登录被拒") && err.contains("用户名或密码不对"), "{err}");
    assert_eq!(router_row(&t, &id).await.0, port);
    // 重新部署、上传密钥失败（路由上没有 /etc/dropbear）：路由上还是旧隧道，库里的隧道公钥与端口写回原来的
    std::fs::rename(r.join("etc/dropbear"), r.join("etc/dropbear.x")).unwrap();
    let before = router_row(&t, &id).await;
    t.post(&format!("/api/v1/routers/{id}/deploy"), json!({"host": "127.0.0.1", "port": sport, "password": "pw-test-123"})).await;
    let d = wait_deploy(&t, &id).await;
    let err = d["deploy"]["error"].as_str().unwrap();
    assert!(err.starts_with("上传密钥失败：没有 ") && err.contains("etc/dropbear") && err.contains("已改回原来的隧道密钥与端口"), "{err}");
    assert_eq!(router_row(&t, &id).await, before);
    assert_eq!(read(&t.dir.path().join("tunnel-keys")), format!("{port} {}\n", before.1.clone().unwrap()));
    std::fs::rename(r.join("etc/dropbear.x"), r.join("etc/dropbear")).unwrap();
    // 主机公钥变了：不登录（不把密码交出去）
    std::fs::write(r.join("hostkey"), "AAAAC3NzaC1lZDI1NTE5AAAAIOtherHostKeyOtherHostKeyOtherHostKeyOtherHo").unwrap();
    t.post(&format!("/api/v1/routers/{id}/deploy"), json!({"host": "127.0.0.1", "port": sport, "password": "pw-test-123"})).await;
    let d = wait_deploy(&t, &id).await;
    let err = d["deploy"]["error"].as_str().unwrap();
    assert!(err.contains("的主机公钥") && err.contains("不一样") && err.contains("重新生成接入命令"), "{err}");
    std::fs::remove_file(r.join("hostkey")).unwrap();
    // 不填密码、中控密钥被拒
    std::fs::write(r.join("refuse-key"), "").unwrap();
    t.post(&format!("/api/v1/routers/{id}/deploy"), json!({"host": "127.0.0.1", "port": sport})).await;
    let d = wait_deploy(&t, &id).await;
    assert!(d["deploy"]["error"].as_str().unwrap().contains("登录被拒"), "{}", d["deploy"]);
    std::fs::remove_file(r.join("refuse-key")).unwrap();
    // 连不上的地址
    let closed = { std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() };
    t.post(&format!("/api/v1/routers/{id}/deploy"), json!({"host": "127.0.0.1", "port": closed})).await;
    let d = wait_deploy(&t, &id).await;
    assert!(d["deploy"]["error"].as_str().unwrap().starts_with(&format!("连不上 127.0.0.1:{closed}")), "{}", d["deploy"]);
    // 正在部署：再部署、重新生成接入命令都 409；参数不对 400
    sqlx::query("update router set deploy_state = 'running' where id = $1::uuid").bind(&id).execute(&t.st.db).await.unwrap();
    assert_eq!(t.req("POST", &format!("/api/v1/routers/{id}/deploy"), Some(json!({"host": "127.0.0.1"})), None).await.0, StatusCode::CONFLICT);
    assert_eq!(t.req("POST", &format!("/api/v1/routers/{id}/bootstrap"), Some(json!({})), None).await.0, StatusCode::CONFLICT);
    assert_eq!(t.req("POST", &format!("/api/v1/routers/{id}/deploy"), Some(json!({"host": "a b"})), None).await.0, StatusCode::BAD_REQUEST);
    // 中控重启：还在部署中的标为失败
    routers::deploy::mark_interrupted(&t.st).await.unwrap();
    let d = t.get(&format!("/api/v1/routers/{id}")).await;
    assert_eq!((d["deploy"]["state"].as_str(), d["deploy"]["error"].as_str()), (Some("failed"), Some("中控重启，部署中断：请重新部署")));
}
