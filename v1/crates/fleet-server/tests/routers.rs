//! 软路由：记录与接入命令、一次性接入脚本、重新接入换密钥与端口、删除撤销、改名、采集一轮（假 ssh 回一份采集结果）、告警、
//! 指标曲线与保留期、一键部署（假 ssh / sshpass / ssh-keyscan）。需要 PostgreSQL（见 common）。
//!
//! 路由是假的：PATH 最前面放假 ssh、sshpass、ssh-keyscan。远端按 `-p <端口>` 分「路由根目录」`<临时目录>/<端口>/`，脚本经 stdin 来，
//! 把 /etc/ 换到根目录里再用本机 sh 跑（/usr/bin/ssh 用本机的 OpenSSH）；采集脚本（含 FLEET_UC）回根目录里的 collect.json，
//! 延迟测试脚本（MODE='delay'）回 delay.json（没有时回「没测成」），取配置文件（含 head -c）回 config.yaml，临时切换脚本（MODE='switch'）把组名记进 switch.log、选中的出口写进 group.now（回读的「切之前」是它原来的内容）。根目录里有 refuse-key 时中控密钥登录被拒、有 down 时连不上、有 hostkey 时 keyscan 回它。
//! sshpass 核对 SSHPASS（环境变量）是不是根目录里的 password，并记下有没有拿到、参数里有什么。PATH 是整个进程共用的，所以单独一个测试文件。

#![cfg(unix)]
// 采集结果的 json! 字面量层数多
#![recursion_limit = "256"]

#[path = "it/common/mod.rs"]
mod common;

/// 「不该再发生」的反向断言（没有再发通知）只能等一会儿再看：等到这么久还没有，就当没有
const NOTHING_MORE: Duration = Duration::from_millis(300);

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{seed_team, T};
use common::tele::Mock;
use fleet_server::routers::{self, alerts, collect};
use fleet_server::state::Event;
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
case $script in *"echo fleet_keys=ok"*) [ -e "$R/slow-keys" ] && sleep 2 ;; esac
case $script in
  "MODE='delay'"*) cat "$R/delay.json" 2>/dev/null || echo '{"v":1,"now":0,"oc":{"delay":{"ok":false,"error":"没有 delay.json"}}}'; exit 0 ;;
  "MODE='switch'"*)
    to=$(printf '%s\n' "$script" | sed -n "s/^TO='\(.*\)'$/\1/p")
    printf '%s\n' "$script" | sed -n "s/^GROUP='\(.*\)'$/\1/p" >> "$R/switch.log"
    was=$(cat "$R/group.now" 2>/dev/null)
    printf '%s' "$to" > "$R/group.now"
    echo "{\"v\":1,\"now\":0,\"sw\":{\"ok\":true,\"type\":\"Selector\",\"was\":\"$was\",\"now\":\"$to\"}}"; exit 0 ;;
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
    sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary) values ($1, 'router', 'router_offline', $2::uuid, 'critical', '新名字：连不上')")
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
    assert_eq!(s["exits"], json!({"total": 3, "alive": 3, "dead": [], "managed": 2, "drift": 1, "fleet": {"total": 2, "alive": 2, "dead": []}, "other": 1}));
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
    // 指标点里 fleet 出口（对上了入口的）与全部出口分开记，同摘要的 exits.fleet
    assert_eq!(t.count(&format!("select count(*) from router_metric where router_id = '{id}' and exits_total = 3 and exits_alive = 3 and fleet_total = 2 and fleet_alive = 2")).await, 1);
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
    assert_eq!((inc[0]["rule"].as_str(), inc[0]["severity"].as_str(), inc[0]["rule_name"].as_str()), (Some("router_collect_error"), Some("warning"), Some("软路由读取出错")));
    assert_eq!(inc[0]["summary"], "团队1-路由E：连得上，但没读到状态：采集脚本出错（退出码 0）：Syntax error: unexpected token");
    // 好了：满 60 秒恢复
    std::fs::write(r.join("collect.json"), collect_json(now + 90, 4_000_000).to_string()).unwrap();
    collect::round(&t.st, &mut fresh).await.unwrap();
    assert!(t.get(&format!("/api/v1/routers/{id}")).await["error"].is_null());
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(100)).await.unwrap();
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(161)).await.unwrap();
    // 配置里的 节点02 凭据和入口对不上：读到状态之后另开「出口参数不一致」
    let rules: Vec<Value> = incidents(&t, "firing").await.iter().map(|i| i["rule"].clone()).collect();
    assert_eq!(rules, [json!("router_exit_drift")]);
}

/// 配置变更时间线：同一台路由两次 sha 变化，一次是中控作业改的（作业动手前后在 claims 里留了记录，sha 对得上），一次是路由上改的；
/// 出口差异按出口名比 server:port 与凭据，凭据只记「变了」
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_changes_timeline() {
    let t = setup().await;
    seed_entry(&t).await;
    let (id, _) = create(&t, "团队1-路由C").await;
    issue_key(&t, &id).await;
    let port = 23951;
    sqlx::query("update router set tunnel_port = $2 where id = $1::uuid").bind(&id).bind(port as i32).execute(&t.st.db).await.unwrap();
    let r = root(port);
    std::fs::create_dir_all(&r).unwrap();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let with_sha = |n: i64, sha: &str| { let mut c = collect_json(now + n, 1_000_000 + n as u64); c["oc"]["config_sha256"] = json!(sha); c.to_string() };
    std::fs::write(r.join("collect.json"), with_sha(0, "sha-1")).unwrap();
    std::fs::write(r.join("config.yaml"), CONFIG).unwrap();
    let mut mem = Default::default();
    collect::round(&t.st, &mut mem).await.unwrap();
    let url = format!("/api/v1/routers/{id}/changes");
    assert_eq!(t.get(&url).await, json!([]), "第一次采集没有可比的，不记");
    // 作业改的：节点01 换了端口与凭据、节点02 没了、多了节点03
    let (rid, job) = (id.parse::<Uuid>().unwrap(), Uuid::now_v7());
    t.st.routers_refresh.claims.editing(rid, job, "router_sync");
    t.st.routers_refresh.claims.edited(rid, job, Some("sha-2"));
    std::fs::write(r.join("collect.json"), with_sha(30, "sha-2")).unwrap();
    std::fs::write(
        r.join("config.yaml"),
        "proxies:
  - {name: 节点01, type: vless, server: relay.test, port: 20010, uuid: 11111111-1111-1111-1111-111111111111, flow: xtls-rprx-vision}
  - {name: 别人的, type: ss, server: 203.0.113.99, port: 8388}
  - {name: 节点03, type: vless, server: 198.51.100.2, port: 20005}
",
    )
    .unwrap();
    collect::round(&t.st, &mut mem).await.unwrap();
    // 路由上有人手改：别人的 换了地址（这个 sha 没有作业认领）；中控重启一次不重复记
    std::fs::write(r.join("collect.json"), with_sha(60, "sha-3")).unwrap();
    std::fs::write(r.join("config.yaml"), "proxies:
  - {name: 节点01, type: vless, server: relay.test, port: 20010, uuid: 11111111-1111-1111-1111-111111111111, flow: xtls-rprx-vision}
  - {name: 别人的, type: ss, server: 203.0.113.100, port: 8388}
  - {name: 节点03, type: vless, server: 198.51.100.2, port: 20005}
").unwrap();
    collect::round(&t.st, &mut mem).await.unwrap();
    let mut fresh = Default::default();
    collect::round(&t.st, &mut fresh).await.unwrap();
    let v = t.get(&url).await;
    let c = v.as_array().unwrap();
    assert_eq!(c.len(), 2, "{v}");
    assert_eq!((c[0]["source"].as_str(), c[0]["job_id"].is_null(), c[0]["old_sha"].as_str(), c[0]["new_sha"].as_str()), (Some("router"), true, Some("sha-2"), Some("sha-3")));
    assert_eq!(c[0]["diff"], json!([{"exit": "别人的", "op": "changed", "from": "203.0.113.99:8388", "to": "203.0.113.100:8388"}]));
    assert_eq!((c[1]["source"].as_str(), c[1]["job_id"].as_str(), c[1]["job_kind"].as_str()), (Some("job"), Some(job.to_string().as_str()), Some("router_sync")));
    assert_eq!(
        c[1]["diff"],
        json!([
            {"exit": "节点01", "op": "changed", "from": "relay.test:20003", "to": "relay.test:20010", "creds": true},
            {"exit": "节点03", "op": "added", "to": "198.51.100.2:20005"},
            {"exit": "节点02", "op": "removed", "from": "198.51.100.2:20003"},
        ])
    );
    assert!(!v.to_string().contains(UUID_OK) && !v.to_string().contains("11111111-1111"), "凭据不输出");
    assert_eq!(t.get(&format!("{url}?limit=1")).await.as_array().unwrap().len(), 1);
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

/// 连续不通的出口带上起点（够老，告警当作早已满了）；通着的没有
fn since(streak: u32) -> Value { if streak >= 2 { json!("2000-01-01T00:00:00Z") } else { Value::Null } }

fn snap(running: bool, streaks: [u32; 2], wan_up: bool) -> Value {
    json!({
        "system": {"cpu_pct": 10.0, "mem_used": 1, "mem_total": 4},
        "interfaces": [{"name": "wan", "device": "eth0", "up": wan_up}],
        "openclash": {"enabled": true, "running": running},
        "exits": [{"name": "节点01", "fail_streak": streaks[0], "fail_since": since(streaks[0]), "sync": "ok"}, {"name": "节点02", "fail_streak": streaks[1], "fail_since": since(streaks[1]), "sync": "ok"},
                  {"name": "别人的", "fail_streak": 9, "sync": "unmanaged"}],
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_open_notify_and_recover() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "info", "enabled": true})).await;
    let (id, _) = create(&t, "团队2-路由A").await;
    issue_key(&t, &id).await;
    let mut st = alerts::AlertState::default();
    let t0 = OffsetDateTime::now_utc();
    // 连不上 40 秒：还不报，到满 3 分钟（含 5 秒余量）时再评估一次；5 分钟：报离线（严重），对象是路由
    status(&t, &id, false, 40, snap(true, [0, 0], true)).await;
    let again = alerts::evaluate(&t.st, &mut st, t0).await.unwrap().map(|d| d.as_secs());
    assert!(incidents(&t, "firing").await.is_empty());
    assert!(again.is_some_and(|s| (134..=136).contains(&s)), "{again:?}");
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
    let b = mock.texts("/hook/a").pop().unwrap();
    assert!(b.starts_with("【严重】告警触发\n机器：团队2-路由A\n规则：软路由离线\n"), "{b}");
    // 连不上时其余几条无从判断：快照里 OpenClash 没在跑也不开
    status(&t, &id, false, 400, snap(false, [3, 3], false)).await;
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(100)).await.unwrap();
    assert_eq!(incidents(&t, "firing").await.len(), 1);
    // 连上了：离线当场恢复（发恢复）；出口两个都连续两次不通 → 出口告警立刻开、严重；OpenClash、WAN 要满 60 秒
    status(&t, &id, true, 0, snap(false, [2, 3], false)).await;
    let t1 = t0 + time::Duration::seconds(200);
    alerts::evaluate(&t.st, &mut st, t1).await.unwrap();
    let rules = |v: &[Value]| v.iter().map(|i| i["rule"].as_str().unwrap().to_string()).collect::<std::collections::BTreeSet<_>>();
    let firing = incidents(&t, "firing").await;
    assert_eq!(rules(&firing), ["router_exits_down"].map(String::from).into());
    let ex = firing.iter().find(|i| i["rule"] == "router_exits_down").unwrap();
    assert_eq!(ex["severity"], "critical");
    alerts::evaluate(&t.st, &mut st, t1 + time::Duration::seconds(61)).await.unwrap();
    let firing = incidents(&t, "firing").await;
    assert_eq!(rules(&firing), ["router_exits_down", "router_openclash_down", "router_wan_down"].map(String::from).into());
    mock.wait("/hook/a", 5).await;
    let sent: Vec<(&str, String)> = mock.events("/hook/a").into_iter().zip(mock.texts("/hook/a")).collect();
    assert!(sent.iter().any(|(e, x)| *e == "resolved" && x.contains("规则：软路由离线")), "{sent:?}");
    // 发送记录里对象也是路由
    let log = t.get("/api/v1/notify-log?limit=20").await;
    let l = log.as_array().unwrap().iter().find(|l| l["kind"] == "firing").unwrap();
    assert_eq!((l["subject"]["kind"].as_str(), l["subject_name"].as_str(), l["rule_name"].as_str().is_some()), (Some("router"), Some("团队2-路由A"), true));
    // 好了：出口刚测通一次（还在确认）先不关；再测通一次就关，其余满 60 秒恢复
    let mut s = snap(true, [0, 0], true);
    s["exits"][0]["recovering"] = json!(true);
    status(&t, &id, true, 0, s).await;
    alerts::evaluate(&t.st, &mut st, t1 + time::Duration::seconds(100)).await.unwrap();
    assert!(rules(&incidents(&t, "firing").await).contains("router_exits_down"));
    status(&t, &id, true, 0, snap(true, [0, 0], true)).await;
    alerts::evaluate(&t.st, &mut st, t1 + time::Duration::seconds(120)).await.unwrap();
    assert!(!rules(&incidents(&t, "firing").await).contains("router_exits_down"), "连续两次测通即恢复");
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
    // 等这条重开的离线告警的触发发出去（之前 9 条：4 触发、4 恢复、1 静默变更）
    mock.wait("/hook/a", 10).await;
    let sent = mock.count("/hook/a");
    assert_eq!(t.req("DELETE", &format!("/api/v1/routers/{id}"), None, None).await.0, StatusCode::OK);
    alerts::evaluate(&t.st, &mut st, t1 + time::Duration::seconds(600)).await.unwrap();
    let all = incidents(&t, "all").await;
    assert!(all.iter().all(|i| i["state"] == "resolved"));
    assert_eq!(all.iter().find(|i| i["rule"] == "router_offline" && i["subject"]["name"] == "（已删除）").map(|i| (i["subject"]["kind"].clone(), i["engine"].clone())), Some((json!("router"), json!("router"))));
    tokio::time::sleep(NOTHING_MORE).await;
    assert_eq!(mock.count("/hook/a"), sent, "删掉的路由不发恢复");
}

/// 在告警的 (对象名, 规则)
async fn firing_set(t: &T) -> std::collections::BTreeSet<(String, String)> {
    incidents(t, "firing").await.iter().map(|i| (i["subject"]["name"].as_str().unwrap().to_string(), i["rule"].as_str().unwrap().to_string())).collect()
}

fn pairs(v: &[(&str, &str)]) -> std::collections::BTreeSet<(String, String)> { v.iter().map(|(n, r)| (n.to_string(), r.to_string())).collect() }

/// 单台路由的维护窗口与静默（同机器）：维护中开着的告警保持（不恢复、不发通知）、不开新的，别的路由照常；到期后按那时的状态再判；
/// 只静默一条规则的只挡这一条；全局维护对路由也算维护中；建静默时校验对象与规则；删路由连带删它的静默
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn router_maintenance_and_silence() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/m")}, "min_severity": "info", "enabled": true})).await;
    let (a, _) = create(&t, "团队2-路由M").await;
    let (b, _) = create(&t, "团队2-路由N").await;
    issue_key(&t, &a).await;
    issue_key(&t, &b).await;
    let mut st = alerts::AlertState::default();
    let t0 = OffsetDateTime::now_utc();
    // A 离线：开着严重告警
    status(&t, &a, false, 300, snap(true, [0, 0], true)).await;
    status(&t, &b, true, 0, snap(true, [0, 0], true)).await;
    alerts::evaluate(&t.st, &mut st, t0).await.unwrap();
    assert_eq!(firing_set(&t).await, pairs(&[("团队2-路由M", "router_offline")]));
    mock.wait("/hook/m", 1).await;

    // 校验：对象要存在、规则要是这类对象的、类型要认识
    let now = OffsetDateTime::now_utc();
    let at = |m: i64| (now + time::Duration::minutes(m)).format(&time::format_description::well_known::Rfc3339).unwrap();
    for (extra, want) in [
        (json!({"subject_kind": "router", "subject_id": Uuid::now_v7()}), "subject_id 指向的软路由不存在"),
        (json!({"subject_kind": "router", "subject_id": b, "rule": "agent_offline"}), "「agent 离线」不是软路由的告警规则"),
        (json!({"subject_id": b}), "subject_id 指向的机器不存在"),
        (json!({"subject_kind": "switch", "subject_id": b}), "subject_kind 只能是 machine / router / probe_point"),
        (json!({"subject_kind": "router"}), "给了 subject_kind 就要给 subject_id；全部对象两个都不给"),
    ] {
        let mut body = json!({"starts_at": at(-1), "ends_at": at(60)});
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let (s, v) = t.req("POST", "/api/v1/silences", Some(body), None).await;
        assert_eq!((s, v["error"]["message"].as_str()), (StatusCode::BAD_REQUEST, Some(want)), "{extra}");
    }

    // A 开维护窗口：列表带对象类型与名字，通知、审计写明是软路由，路由视图显示维护中（B 不算）
    let win = t.post("/api/v1/silences", json!({"subject_kind": "router", "subject_id": a, "is_maintenance": true, "starts_at": at(-1), "ends_at": at(60), "comment": "客户现场断电"})).await;
    let win = win["id"].as_str().unwrap().to_string();
    let list = t.get("/api/v1/silences").await;
    assert_eq!((list[0]["subject_kind"].as_str(), list[0]["subject_id"].as_str(), list[0]["subject_name"].as_str()), (Some("router"), Some(a.as_str()), Some("团队2-路由M")), "{list}");
    mock.wait("/hook/m", 2).await;
    let text = mock.texts("/hook/m")[1].clone();
    assert!(text.contains("新建了维护窗口\n对象：软路由「团队2-路由M」\n") && text.contains("期间不开新告警、开着的保持原状、不发通知"), "{text}");
    let log = t.get(&format!("/api/v1/audit?target_id={win}")).await;
    assert!(log[0]["summary"].as_str().unwrap().starts_with("软路由「团队2-路由M」 · 维护窗口 · "), "{log}");
    assert_eq!(t.get(&format!("/api/v1/routers/{a}")).await["maintenance"], true);
    let rs = t.get("/api/v1/routers").await;
    let maint: Vec<(&str, bool)> = rs.as_array().unwrap().iter().map(|r| (r["name"].as_str().unwrap(), r["maintenance"].as_bool().unwrap())).collect();
    assert_eq!(maint, [("团队2-路由M", true), ("团队2-路由N", false)]);

    // 维护中：A 连上了但 WAN 断着——离线告警保持（不恢复、不发恢复），WAN 告警不开；B 照常开
    status(&t, &a, true, 0, snap(true, [0, 0], false)).await;
    status(&t, &b, false, 300, snap(true, [0, 0], true)).await;
    // 这一轮评估发现 A 在维护中的集合变了：发一次 Routers（卡片上的「维护」要跟上），之后没变不再发
    let mut ev = t.st.events.subscribe();
    let mut routers_events = || std::iter::from_fn(|| ev.try_recv().ok()).filter(|e| matches!(e, Event::Routers)).count();
    for s in [100, 200, 300] {
        alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(s)).await.unwrap();
    }
    assert_eq!(routers_events(), 1, "维护窗口生效的那一轮发一次 routers 事件");
    assert_eq!(firing_set(&t).await, pairs(&[("团队2-路由M", "router_offline"), ("团队2-路由N", "router_offline")]));
    mock.wait("/hook/m", 3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent: Vec<(&str, String)> = mock.events("/hook/m").into_iter().zip(mock.texts("/hook/m")).skip(2).collect();
    assert!(sent.len() == 1 && sent[0].0 == "firing" && sent[0].1.contains("机器：团队2-路由N"), "维护中的 A 不发任何通知：{sent:?}");

    // 到期：离线满 60 秒恢复（发恢复），WAN 断开开出来
    sqlx::query("update silence set ends_at = now() - interval '1 second' where id = $1::uuid").bind(&win).execute(&t.st.db).await.unwrap();
    assert_eq!(t.get(&format!("/api/v1/routers/{a}")).await["maintenance"], false);
    for s in [400, 461] {
        alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(s)).await.unwrap();
    }
    assert_eq!(routers_events(), 1, "窗口自己到期的那一轮也发一次 routers 事件，没人点「结束维护」也一样");
    assert_eq!(firing_set(&t).await, pairs(&[("团队2-路由M", "router_wan_down"), ("团队2-路由N", "router_offline")]));
    mock.wait("/hook/m", 5).await;
    let sent: Vec<(&str, String)> = mock.events("/hook/m").into_iter().zip(mock.texts("/hook/m")).collect();
    assert!(sent.iter().any(|(e, x)| *e == "resolved" && x.contains("规则：软路由离线")) && sent.iter().any(|(e, x)| *e == "firing" && x.contains("规则：软路由 WAN 口断开")), "{sent:?}");

    // 只静默 B 的出口告警：出口不通不开，OpenClash 没在跑照开
    t.post("/api/v1/silences", json!({"subject_kind": "router", "subject_id": b, "rule": "router_exits_down", "starts_at": at(-1), "ends_at": at(60)})).await;
    status(&t, &b, true, 0, snap(false, [2, 3], true)).await;
    for s in [500, 561, 622] {
        alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(s)).await.unwrap();
    }
    assert_eq!(firing_set(&t).await, pairs(&[("团队2-路由M", "router_wan_down"), ("团队2-路由N", "router_openclash_down")]));

    // 全局维护：路由也算维护中
    let all = t.post("/api/v1/silences", json!({"is_maintenance": true, "starts_at": at(-1), "ends_at": at(60)})).await;
    assert!(t.get("/api/v1/routers").await.as_array().unwrap().iter().all(|r| r["maintenance"] == true));
    t.req("DELETE", &format!("/api/v1/silences/{}", all["id"].as_str().unwrap()), None, None).await;

    // 删路由：它的静默一起删
    assert_eq!(t.req("DELETE", &format!("/api/v1/routers/{b}"), None, None).await.0, StatusCode::OK);
    let left: i64 = sqlx::query_scalar("select count(*) from silence where router_id = $1::uuid").bind(&b).fetch_one(&t.st.db).await.unwrap();
    assert_eq!(left, 0);
}

/// 重新接入：开着的离线告警悄悄关掉（不发「已恢复」）；接入过的 30 分钟还没连上开离线警告
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebootstrap_closes_offline_quietly_then_warns() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/r")}, "min_severity": "info", "enabled": true})).await;
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
    tokio::time::sleep(NOTHING_MORE).await;
    assert_eq!(mock.count("/hook/r"), 1, "不发恢复");
    // 命令过期没人执行：30 分钟后开离线警告
    sqlx::query("update router set bootstrap_expires_at = now() - interval '1 minute' where id = $1::uuid").bind(&id).execute(&t.st.db).await.unwrap();
    alerts::evaluate(&t.st, &mut st, OffsetDateTime::now_utc()).await.unwrap();
    let inc = incidents(&t, "firing").await;
    assert_eq!(inc.len(), 1, "{inc:?}");
    assert_eq!((inc[0]["rule"].as_str(), inc[0]["severity"].as_str()), (Some("router_offline"), Some("warning")));
    assert!(inc[0]["summary"].as_str().unwrap().starts_with("团队2-路由R：重新接入后还没连上（已 31 分钟"), "{}", inc[0]["summary"]);
}

/// 出口指向的节点在维护：不通的出口都指向它时不开（开着的保持，不发恢复）；指向的节点有机器告警时摘要附上
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exits_down_notes_maintenance_and_alerts() {
    let t = setup().await;
    let team = seed_team(&t, "团队2").await;
    let (relay, _) = common::seed_relay(&t).await;
    let n3 = common::seed_node(&t, team, 3, "203.0.113.13", "active").await;
    let (id, _) = create(&t, "团队2-路由M").await;
    issue_key(&t, &id).await;
    let er = json!({"id": Uuid::now_v7(), "name": "团队2-03 · 线路机2:20003", "node_id": n3, "relay_id": relay, "node_name": "团队2-03", "relay_name": "线路机2", "port": 20003, "lifecycle": "active"});
    let data = json!({
        "system": {"cpu_pct": 10.0, "mem_used": 1, "mem_total": 4},
        "interfaces": [{"name": "wan", "device": "eth0", "up": true}],
        "openclash": {"enabled": true, "running": true},
        "exits": [{"name": "节点03", "fail_streak": 2, "fail_since": since(2), "sync": "ok", "entry": er}, {"name": "节点04", "fail_streak": 0, "sync": "ok"}],
    });
    status(&t, &id, true, 0, data).await;
    let maint = Uuid::now_v7();
    sqlx::query("insert into silence (id, machine_id, is_maintenance, starts_at, ends_at, created_by) values ($1, $2, true, now() - interval '1 minute', now() + interval '1 hour', 'a')")
        .bind(maint)
        .bind(n3)
        .execute(&t.st.db)
        .await
        .unwrap();
    let mut st = alerts::AlertState::default();
    let t0 = OffsetDateTime::now_utc();
    alerts::evaluate(&t.st, &mut st, t0).await.unwrap();
    assert!(incidents(&t, "firing").await.is_empty(), "不通的出口都指向维护中的节点：不开");
    // 维护结束：开；节点有机器告警：摘要附上
    sqlx::query("update silence set deleted_at = now() where id = $1").bind(maint).execute(&t.st.db).await.unwrap();
    sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary) values ($1, 'machine', 'egress_fail', $2, 'critical', 'x')")
        .bind(Uuid::now_v7())
        .bind(n3)
        .execute(&t.st.db)
        .await
        .unwrap();
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(30)).await.unwrap();
    let inc = incidents(&t, "firing").await;
    let ex = inc.iter().find(|i| i["rule"] == "router_exits_down").unwrap();
    assert_eq!(ex["summary"], "团队2-路由M：1 个出口连续两次延迟测试不通：节点03（其中 1 个指向的团队2-03 正在告警）");
    // 又进了维护：开着的保持（不当成恢复），摘要照旧
    sqlx::query("update silence set deleted_at = null where id = $1").bind(maint).execute(&t.st.db).await.unwrap();
    for s in [60, 200] {
        alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(s)).await.unwrap();
    }
    let inc = incidents(&t, "firing").await;
    assert!(inc.iter().any(|i| i["rule"] == "router_exits_down" && i["summary"] == ex["summary"]), "{inc:?}");
}

/// 节点维护刚结束：机器告警的计时被清、要重攒 150 秒才开，路由出口的不通计时早就满了。这段时间路由告警按 quiet 处理（开着、不单独通知），
/// 过了这段时间机器告警还是没来，才在重新采集之后补发「触发」
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exits_down_quiet_right_after_maintenance_ends() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/m")}, "min_severity": "info", "enabled": true})).await;
    let team = seed_team(&t, "团队2").await;
    let (relay, _) = common::seed_relay(&t).await;
    let n3 = common::seed_node(&t, team, 3, "203.0.113.13", "active").await;
    let (id, _) = create(&t, "团队2-路由N").await;
    issue_key(&t, &id).await;
    let t0 = OffsetDateTime::now_utc();
    let at = |s: i64| (t0 + time::Duration::seconds(s)).format(&time::format_description::well_known::Rfc3339).unwrap();
    let er = json!({"id": Uuid::now_v7(), "name": "团队2-03 · 线路机2:20003", "node_id": n3, "relay_id": relay, "node_name": "团队2-03", "relay_name": "线路机2", "port": 20003, "lifecycle": "active"});
    let data = |collected: String| {
        json!({
            "system": {"cpu_pct": 10.0, "mem_used": 1, "mem_total": 4},
            "interfaces": [{"name": "wan", "device": "eth0", "up": true}],
            "openclash": {"enabled": true, "running": true},
            "exits": [{"name": "节点03", "fail_streak": 2, "fail_since": since(2), "sync": "ok", "entry": er}, {"name": "节点04", "fail_streak": 0, "sync": "ok"}],
            "collected_at": collected,
        })
    };
    status(&t, &id, true, 0, data(at(0))).await;
    let maint = Uuid::now_v7();
    sqlx::query("insert into silence (id, machine_id, is_maintenance, starts_at, ends_at, created_by) values ($1, $2, true, now() - interval '10 minutes', now() + interval '1 hour', 'a')")
        .bind(maint)
        .bind(n3)
        .execute(&t.st.db)
        .await
        .unwrap();
    let mut st = alerts::AlertState::default();
    alerts::evaluate(&t.st, &mut st, t0).await.unwrap();
    assert!(incidents(&t, "firing").await.is_empty(), "维护中：不开");
    // 维护刚结束，机器告警还没来得及开：路由告警开着，但不单独通知
    sqlx::query("update silence set deleted_at = now() where id = $1").bind(maint).execute(&t.st.db).await.unwrap();
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(30)).await.unwrap();
    let inc = incidents(&t, "firing").await;
    assert_eq!(inc.iter().filter(|i| i["rule"] == "router_exits_down").count(), 1, "{inc:?}");
    assert!(inc.iter().any(|i| i["summary"].as_str().is_some_and(|s| s.contains("1 个指向的团队2-03 刚结束维护"))), "{inc:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock.count("/hook/m"), 0, "机器告警还没来：先别抢着通知");
    // 过了这段时间（维护结束已超过 180 秒）机器告警还是没开：解除 quiet，等重新采集后补发「触发」
    sqlx::query("update silence set starts_at = now() - interval '20 minutes', deleted_at = now() - interval '4 minutes' where id = $1").bind(maint).execute(&t.st.db).await.unwrap();
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(60)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock.count("/hook/m"), 0, "快照还是解除之前的");
    status(&t, &id, true, 0, data(at(70))).await;
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(90)).await.unwrap();
    mock.wait("/hook/m", 1).await;
    assert_eq!(mock.events("/hook/m"), ["firing"]);
}

/// 出口不通开着但 quiet（指向的节点有机器告警）：机器告警先恢复后，路由告警等这台路由重新采集（快照晚于解除的时刻），出口还不通才补发「触发」；
/// 没有新快照的评估（补采别的、到点复查）不算数；解除之后出口就好了的，既不发触发也不发恢复（不会出现「触发」随后又「恢复」）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quiet_exits_down_waits_for_fresh_snapshot_after_machine_alert_resolves() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/q")}, "min_severity": "info", "enabled": true})).await;
    let team = seed_team(&t, "团队2").await;
    let (relay, _) = common::seed_relay(&t).await;
    let t0 = OffsetDateTime::now_utc();
    let at = |s: i64| (t0 + time::Duration::seconds(s)).format(&time::format_description::well_known::Rfc3339).unwrap();
    let mut routers = vec![];
    for (name, number) in [("团队2-路由A", 3), ("团队2-路由B", 4)] {
        let node = common::seed_node(&t, team, number, &format!("203.0.113.{}", 10 + number), "active").await;
        let (id, _) = create(&t, name).await;
        issue_key(&t, &id).await;
        let er = json!({"id": Uuid::now_v7(), "name": "入口", "node_id": node, "relay_id": relay, "node_name": format!("团队2-0{number}"), "relay_name": "线路机2", "port": 20003, "lifecycle": "active"});
        let data = move |streak: u32, collected: String| {
            json!({
                "system": {"cpu_pct": 10.0, "mem_used": 1, "mem_total": 4},
                "interfaces": [{"name": "wan", "device": "eth0", "up": true}],
                "openclash": {"enabled": true, "running": true},
                "exits": [{"name": "节点01", "fail_streak": streak, "fail_since": since(streak), "sync": "ok", "entry": er}, {"name": "节点02", "fail_streak": 0, "sync": "ok"}],
                "collected_at": collected,
            })
        };
        status(&t, &id, true, 0, data(2, at(0))).await;
        let inc = Uuid::now_v7();
        sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary) values ($1, 'machine', 'egress_fail', $2, 'critical', 'x')").bind(inc).bind(node).execute(&t.st.db).await.unwrap();
        routers.push((id, data));
    }
    let mut st = alerts::AlertState::default();
    // 机器告警在报：路由的出口告警开着、不通知
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(0)).await.unwrap();
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(30)).await.unwrap();
    assert_eq!(incidents(&t, "firing").await.iter().filter(|i| i["rule"] == "router_exits_down").count(), 2);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock.count("/hook/q"), 0);
    // 机器告警恢复，出口还不通：解除 quiet 的这一轮、之后没有新快照的评估（补采别的、到点复查）都还是不通知
    sqlx::query("update incident set state = 'resolved', resolved_at = now() where engine = 'machine'").execute(&t.st.db).await.unwrap();
    for s in [60, 75] {
        alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(s)).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock.count("/hook/q"), 0, "快照还是解除之前的：不算重新测过");
    // 两台都重新采集了：B 的出口好了，不发触发也不发恢复；A 还不通：补发触发
    status(&t, &routers[1].0, true, 0, (routers[1].1)(0, at(80))).await;
    status(&t, &routers[0].0, true, 0, (routers[0].1)(2, at(80))).await;
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(90)).await.unwrap();
    mock.wait("/hook/q", 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock.events("/hook/q"), ["firing"]);
    assert!(mock.texts("/hook/q")[0].contains("机器：团队2-路由A"), "{:?}", mock.texts("/hook/q"));
    let firing = incidents(&t, "firing").await;
    assert_eq!(firing.iter().filter(|i| i["rule"] == "router_exits_down").count(), 1, "B 的已恢复");
    // A 也好了：报恢复
    status(&t, &routers[0].0, true, 0, (routers[0].1)(0, at(120))).await;
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(120)).await.unwrap();
    mock.wait("/hook/q", 2).await;
    assert_eq!(mock.events("/hook/q"), ["firing", "resolved"]);
}

/// 中控重启后（AlertState 重建）：重启前就开着的 quiet 告警（还在告警、没通知过、没被静默）从库里补回 quiet，
/// 机器告警已经恢复时同样等重新采集再补发；已通知过的、被静默的不当 quiet
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alert_state_load_restores_quiet_exits_down() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/l")}, "min_severity": "info", "enabled": true})).await;
    let team = seed_team(&t, "团队2").await;
    let (relay, _) = common::seed_relay(&t).await;
    let t0 = OffsetDateTime::now_utc();
    let at = |s: i64| (t0 + time::Duration::seconds(s)).format(&time::format_description::well_known::Rfc3339).unwrap();
    let node = common::seed_node(&t, team, 3, "203.0.113.13", "active").await;
    let (id, _) = create(&t, "团队2-路由A").await;
    issue_key(&t, &id).await;
    let er = json!({"id": Uuid::now_v7(), "name": "入口", "node_id": node, "relay_id": relay, "node_name": "团队2-03", "relay_name": "线路机2", "port": 20003, "lifecycle": "active"});
    let data = |collected: String| {
        json!({
            "system": {"cpu_pct": 10.0, "mem_used": 1, "mem_total": 4},
            "interfaces": [{"name": "wan", "device": "eth0", "up": true}],
            "openclash": {"enabled": true, "running": true},
            "exits": [{"name": "节点01", "fail_streak": 2, "sync": "ok", "entry": er.clone(), "fail_since": at(-300)}, {"name": "节点02", "fail_streak": 0, "sync": "ok"}],
            "collected_at": collected,
        })
    };
    status(&t, &id, true, 0, data(at(0))).await;
    // 重启前开的 quiet 告警：开的时候没通知过
    sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary) values ($1, 'router', 'router_exits_down', $2::uuid, 'warning', '团队2-路由A：1 个出口连续两次延迟测试不通：节点01')")
        .bind(Uuid::now_v7())
        .bind(&id)
        .execute(&t.st.db)
        .await
        .unwrap();
    // 机器告警早已恢复（没有在报的），出口还不通：不能一上来就当「新的」通知
    let mut st = alerts::AlertState::load(&t.st.db).await.unwrap();
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(10)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock.count("/hook/l"), 0, "补回了 quiet：解除后先等重新采集");
    status(&t, &id, true, 0, data(at(20))).await;
    alerts::evaluate(&t.st, &mut st, t0 + time::Duration::seconds(40)).await.unwrap();
    mock.wait("/hook/l", 1).await;
    assert_eq!(mock.events("/hook/l"), ["firing"]);

    // 判 quiet 看「上次恢复之后有没有报过触发」：从没通知过的、通知时刻不晚于最近一条恢复通知的（恢复后重开、重开时 quiet）是 quiet；
    // 恢复之后又报过触发的、没恢复过但通知过的不是
    let open = |notified: &str| {
        let (db, id, notified) = (t.st.db.clone(), Uuid::now_v7(), notified.to_string());
        async move {
            let inc = Uuid::now_v7();
            sqlx::query(&format!(
                "insert into incident (id, engine, rule, subject_id, severity, summary, notified_at) values ($1, 'router', 'router_exits_down', $2, 'warning', 's', {notified})"
            ))
            .bind(inc)
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
            (inc, id)
        }
    };
    let log = |inc: Uuid, kind: &'static str, ago: i64| {
        let db = t.st.db.clone();
        async move {
            sqlx::query("insert into notify_log (incident_id, ok, kind, ts) values ($1, true, $2, now() - make_interval(secs => $3))")
                .bind(inc)
                .bind(kind)
                .bind(ago as f64)
                .execute(&db)
                .await
                .unwrap();
        }
    };
    let (reopened, reopened_id) = open("now() - interval '300 seconds'").await;
    log(reopened, "firing", 300).await;
    log(reopened, "resolved", 200).await;
    let (told_again, told_again_id) = open("now() - interval '30 seconds'").await;
    log(told_again, "resolved", 200).await;
    let (told, told_id) = open("now() - interval '30 seconds'").await;
    log(told, "firing", 30).await;
    let st = alerts::AlertState::load(&t.st.db).await.unwrap();
    assert_eq!(
        (st.is_quiet(reopened_id), st.is_quiet(told_again_id), st.is_quiet(told_id), st.is_quiet(id.parse().unwrap())),
        (true, false, false, false),
        "恢复后重开且没再报过触发的补回 quiet；上面已经单独通知过的（没恢复过）不是"
    );
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

/// 强制删路由（路由回不来了）：把它从没做完的作业的 routers_applied / routers_stuck 里摘掉、作业日志记一条，作业不再被它卡住
/// （没有别的路由要改回时能取消，「需要人工处理」消失）；作业正在执行时不摘，409
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_delete_releases_unfinished_router_job() {
    let t = setup().await;
    let (id, _) = create(&t, "团队2-路由D").await;
    let id: Uuid = id.parse().unwrap();
    let other = Uuid::now_v7();
    let job = Uuid::now_v7();
    let applied = |r: Uuid, n: &str| json!({"id": r, "name": n, "port": 23000, "stamp": "S"});
    let stuck = |r: Uuid, n: &str| json!({"id": r, "name": n, "exits": ["节点01"], "was": ["1.2.3.4:20001"]});
    sqlx::query("insert into job (id, kind, params, state, step, created_by) values ($1, 'replace', $2, 'running', 0, 't')")
        .bind(job)
        .bind(json!({"routers_applied": [applied(id, "团队2-路由D"), applied(other, "路由E")], "routers_stuck": [stuck(id, "团队2-路由D")]}))
        .execute(&t.st.db)
        .await
        .unwrap();
    let (plain, forced) = (format!("/api/v1/routers/{id}"), format!("/api/v1/routers/{id}?force=true"));
    let del = |force: bool| t.req("DELETE", if force { &forced } else { &plain }, None, None);
    let (s, v) = del(false).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("强制删除"), "{v}");
    let (s, v) = del(true).await;
    assert_eq!(s, StatusCode::CONFLICT, "作业正在执行：{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("正在执行"), "{v}");
    sqlx::query("update job set state = 'failed' where id = $1").bind(job).execute(&t.st.db).await.unwrap();
    let (s, v) = del(true).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["jobs"], json!([job]));
    let p: Value = sqlx::query_scalar("select params from job where id = $1").bind(job).fetch_one(&t.st.db).await.unwrap();
    assert_eq!((p["routers_applied"].clone(), p["routers_stuck"].clone()), (json!([applied(other, "路由E")]), json!([])), "{p}");
    let log: String = sqlx::query_scalar("select msg from job_log where job_id = $1").bind(job).fetch_one(&t.st.db).await.unwrap();
    assert!(log.contains("团队2-路由D") && log.contains("改回放弃"), "{log}");
    let a: Value = sqlx::query_scalar("select detail from audit_log where action = 'router.delete'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!((a["force"].clone(), a["jobs"].clone()), (json!(true), json!([job])), "{a}");
    let j = t.get(&format!("/api/v1/jobs/{job}")).await;
    assert_eq!((j["manual"].clone(), j["cancellable"].clone()), (Value::Null, json!(false)), "路由E 还没改回，照旧不能取消：{j}");
    // 路由E 也摘掉（同样强制删了）之后能取消
    sqlx::query("update job set params = params || '{\"routers_applied\": []}' where id = $1").bind(job).execute(&t.st.db).await.unwrap();
    assert_eq!(t.get(&format!("/api/v1/jobs/{job}")).await["cancellable"], json!(true));
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
    // 全部出口 3 个都可用；第一点另记 fleet 出口 2 个、可用 1 个，其余是迁移 0091 之前的点（fleet 两列为空）
    for (ago, cpu, fleet) in [(600, 10.0f32, Some((2, 1))), (300, 30.0, None), (5 * 86400, 99.0, None)] {
        sqlx::query(
            "insert into router_metric (router_id, ts, cpu_pct, mem_used, mem_total, conntrack, conntrack_max, wan_rx_bps, conns, clients, exits_total, exits_alive,
                fleet_total, fleet_alive)
             values ($1::uuid, now() - make_interval(secs => $2), $3, 50, 100, 10, 100, 8000, 4, 2, 3, 3, $4, $5)",
        )
        .bind(&id)
        .bind(ago as f64)
        .bind(cpu)
        .bind(fleet.map(|f: (i32, i32)| f.0))
        .bind(fleet.map(|f| f.1))
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
    for k in ["ts", "load1", "disk_pct", "temp_c", "wan_tx_bps", "proxy_up_bps", "proxy_down_bps", "conns", "clients", "exits_alive", "fleet_alive"] {
        assert!(p[0].get(k).is_some(), "{k}");
    }
    // 可用出口按 fleet 出口算；没有 fleet 读数的旧点退回全部出口
    assert_eq!((p[0]["exits_alive"].as_i64(), p[0]["fleet_alive"].as_i64(), p[1]["fleet_alive"].as_i64()), (Some(3), Some(1), Some(3)));
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
    // 小时点：1 与 3（旧点退回 exits_alive）的平均不会是 null；小时里全是旧点时就是 3
    assert!(h["points"].as_array().unwrap().iter().all(|p| p["fleet_alive"].as_f64().is_some_and(|v| (1.0..=3.0).contains(&v))), "{h}");
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
    common::eventually("部署结束", Duration::from_secs(30), || async {
        let d = t.get(&format!("/api/v1/routers/{id}")).await;
        (d["deploy"]["state"] != "running").then_some(d)
    })
    .await
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

/// 重新部署失败要写回旧隧道端口时，那个端口已经分给了别的路由（换走之后空出来，同时有别的部署 / 重新接入）：不写回、明说
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redeploy_restore_skips_taken_port() {
    let t = setup().await;
    let (_l, sport, r) = fake_router("pw");
    let (id, _) = create(&t, "部署A").await;
    let (other, _) = create(&t, "路由B").await;
    let old = issue_key(&t, &id).await;
    // 上传密钥失败（没有 /etc/dropbear），而且慢一点：失败之前把 A 原来的端口分给 B
    std::fs::remove_dir_all(r.join("etc/dropbear")).unwrap();
    std::fs::write(r.join("slow-keys"), "").unwrap();
    t.post(&format!("/api/v1/routers/{id}/deploy"), json!({"host": "127.0.0.1", "port": sport, "password": "pw"})).await;
    common::eventually("上传密钥之前换了端口", Duration::from_secs(2), || async { (i64::from(router_row(&t, &id).await.0) != old).then_some(()) }).await;
    sqlx::query("update router set tunnel_port = $2 where id = $1::uuid").bind(&other).bind(old as i32).execute(&t.st.db).await.unwrap();
    let d = wait_deploy(&t, &id).await;
    let err = d["deploy"]["error"].as_str().unwrap();
    assert!(err.contains(&format!("改回原来的隧道密钥也失败了（原来的隧道端口 {old} 已经分给了软路由「路由B」）")), "{err}");
    assert_ne!(i64::from(router_row(&t, &id).await.0), old);
}

/// 临时切换：只能切 select 组；切到组里另一个出口、到时自动切回（操作者「系统」）；
/// 路由自己改回了（重启、OpenClash 重载后组回到默认选择）：采集核对时记录结束、待切回取消、记审计（切换生效之前开始的采集不算）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_and_auto_back() {
    let t = setup().await;
    let (id, _) = create(&t, "团队2-路由W").await;
    issue_key(&t, &id).await;
    let port = 23940;
    sqlx::query("update router set tunnel_port = $2 where id = $1::uuid").bind(&id).bind(port).execute(&t.st.db).await.unwrap();
    let r = root(port.into());
    let _ = std::fs::remove_dir_all(&r);
    std::fs::create_dir_all(&r).unwrap();
    std::fs::write(r.join("group.now"), "节点01").unwrap();
    let groups = |now: &str| json!([{"name": "手动选择", "type": "Selector", "now": now, "all": ["节点01", "节点02"]}, {"name": "自动", "type": "URLTest", "now": "节点01", "all": ["节点01", "节点02"]}]);
    let mut s = snap(true, [0, 0], true);
    s["groups"] = groups("节点01");
    status(&t, &id, true, 0, s.clone()).await;
    let path = format!("/api/v1/routers/{id}/switch");
    let (code, v) = t.req("POST", &path, Some(json!({"group": "自动", "to": "节点02"})), None).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().contains("只有 select 组可以"), "{v}");
    for (group, to, msg) in [("手动选择", "节点09", "只能切到组里已有的出口（节点01、节点02）"), ("没有", "节点01", "没有策略组「没有」")] {
        let (code, v) = t.req("POST", &path, Some(json!({"group": group, "to": to})), None).await;
        assert!(code == StatusCode::BAD_REQUEST && v["error"]["message"].as_str().unwrap().contains(msg), "{v}");
    }
    assert!(read(&r.join("switch.log")).is_empty(), "没去动路由");
    // 切到 节点02，1 小时后自动切回
    let d = t.post(&path, json!({"group": "手动选择", "to": "节点02", "back_after_s": 3600})).await;
    assert_eq!((d["from"].as_str(), d["to"].as_str(), d["back_at"].is_string()), (Some("节点01"), Some("节点02"), true), "{d}");
    assert_eq!((read(&r.join("switch.log")), read(&r.join("group.now"))), ("手动选择\n".to_string(), "节点02".to_string()));
    let sw = t.get(&format!("/api/v1/routers/{id}")).await["switches"].clone();
    assert_eq!((sw[0]["group"].as_str(), sw[0]["from"].as_str(), sw[0]["to"].as_str()), (Some("手动选择"), Some("节点01"), Some("节点02")), "{sw}");
    let audit = |_: &T| async { t.get(&format!("/api/v1/audit?target_id={id}")).await[0].clone() };
    assert_eq!(audit(&t).await["title"], "临时切换出口");
    // 没到时间不动；到时间切回 节点01，操作者是「系统」
    let rid = Uuid::parse_str(&id).unwrap();
    routers::switch::due(&t.st).await;
    assert_eq!(read(&r.join("group.now")), "节点02");
    sqlx::query("update router_switch set back_at = now() - interval '1 second'").execute(&t.st.db).await.unwrap();
    routers::switch::due(&t.st).await;
    assert_eq!(read(&r.join("group.now")), "节点01");
    assert_eq!(t.get(&format!("/api/v1/routers/{id}")).await["switches"], json!([]));
    let a = audit(&t).await;
    assert_eq!((a["title"].as_str(), a["actor"].as_str(), a["summary"].as_str()), (Some("切回出口"), Some("系统"), Some("团队2-路由W · 手动选择：节点02 → 节点01（到时自动切回）")), "{a}");
    // 再切一次；之后路由重启、组回到 节点01
    t.post(&path, json!({"group": "手动选择", "to": "节点02", "back_after_s": 3600})).await;
    let back: collect::Snapshot = serde_json::from_value(s).unwrap();
    let now = OffsetDateTime::now_utc();
    routers::switch::reconcile(&t.st, rid, &back, now - time::Duration::seconds(60)).await.unwrap();
    assert_eq!(t.get(&format!("/api/v1/routers/{id}")).await["switches"].as_array().map(Vec::len), Some(1), "切换之前开始的采集不算");
    routers::switch::reconcile(&t.st, rid, &back, now + time::Duration::seconds(1)).await.unwrap();
    assert_eq!(t.get(&format!("/api/v1/routers/{id}")).await["switches"], json!([]));
    let a = audit(&t).await;
    assert_eq!((a["title"].as_str(), a["summary"].as_str()), (Some("临时切换被路由改回"), Some("团队2-路由W · 手动选择：已被路由自己改回（现在选中的是「节点01」），待切回已取消")), "{a}");
    assert_eq!(t.req("POST", &format!("{path}/back"), Some(json!({"group": "手动选择"})), None).await.0, StatusCode::NOT_FOUND);
}

/// 到时自动切回用的是拿锁之前读出的旧记录（前面每条切回最多 25 秒）：等锁期间人又切了一次、或手动切回了，
/// 拿到锁后按 id 重读，不再到时的就跳过，不去撤销人的操作
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn due_skips_record_changed_while_waiting() {
    let t = setup().await;
    let (id, _) = create(&t, "团队2-路由V").await;
    issue_key(&t, &id).await;
    let port = 23942;
    sqlx::query("update router set tunnel_port = $2 where id = $1::uuid").bind(&id).bind(port).execute(&t.st.db).await.unwrap();
    let r = root(port.into());
    let _ = std::fs::remove_dir_all(&r);
    std::fs::create_dir_all(&r).unwrap();
    std::fs::write(r.join("group.now"), "节点01").unwrap();
    let mut s = snap(true, [0, 0], true);
    s["groups"] = json!([{"name": "手动选择", "type": "Selector", "now": "节点01", "all": ["节点01", "节点02", "节点03"]}]);
    status(&t, &id, true, 0, s).await;
    let rid = Uuid::parse_str(&id).unwrap();
    let path = format!("/api/v1/routers/{id}/switch");
    // 切到 节点02 并到时，due 读到这条；等锁期间人又切到 节点03（不再自动切回）
    t.post(&path, json!({"group": "手动选择", "to": "节点02", "back_after_s": 3600})).await;
    sqlx::query("update router_switch set back_at = now() - interval '1 second'").execute(&t.st.db).await.unwrap();
    let stale = routers::switch::active(&t.st.db, Some(rid)).await.unwrap();
    assert_eq!(stale.len(), 1);
    t.post(&path, json!({"group": "手动选择", "to": "节点03"})).await;
    let log = read(&r.join("switch.log"));
    routers::switch::due_each(&t.st, stale).await;
    assert_eq!((read(&r.join("group.now")), read(&r.join("switch.log"))), ("节点03".to_string(), log.clone()), "人又切了一次：不撤销");
    let sw = t.get(&format!("/api/v1/routers/{id}")).await["switches"][0].clone();
    assert_eq!((sw["to"].as_str(), sw["back_at"].is_null()), (Some("节点03"), true), "{sw}");
    // 又到时，due 读到后人手动切回了：记录已结束，同样不动路由
    sqlx::query("update router_switch set back_at = now() - interval '1 second'").execute(&t.st.db).await.unwrap();
    let stale = routers::switch::active(&t.st.db, Some(rid)).await.unwrap();
    t.post(&format!("{path}/back"), json!({"group": "手动选择"})).await;
    std::fs::write(r.join("group.now"), "节点03").unwrap();
    let log = read(&r.join("switch.log"));
    routers::switch::due_each(&t.st, stale).await;
    assert_eq!((read(&r.join("group.now")), read(&r.join("switch.log"))), ("节点03".to_string(), log), "已手动切回：不再动路由");
    let n: i64 = sqlx::query_scalar("select count(*) from audit_log where action = 'router.switch_back'").fetch_one(&t.st.db).await.unwrap();
    assert_eq!(n, 1, "只有手动切回那一条");
}

/// 没有规则用的组（快照 used = false）切了不影响流量：409、不去动路由；used 为 true 或没有这个字段（旧快照，未知）照常能切。
/// 到时自动切回时原出口还不通、或它指向的节点有进行中的严重机器告警：不切回，往后推 1 小时、记原因、审计一条、通知渠道只说一次；都恢复后才切回
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_unused_group_and_deferred_back() {
    let t = setup().await;
    let mock = Mock::start().await;
    t.post("/api/v1/notify-channels", json!({"name": "wh", "kind": "feishu", "config": {"url": mock.url("/hook/a")}, "min_severity": "critical", "enabled": true})).await;
    let (id, _) = create(&t, "团队2-路由D").await;
    issue_key(&t, &id).await;
    let port = 23941;
    sqlx::query("update router set tunnel_port = $2 where id = $1::uuid").bind(&id).bind(port).execute(&t.st.db).await.unwrap();
    let r = root(port.into());
    let _ = std::fs::remove_dir_all(&r);
    std::fs::create_dir_all(&r).unwrap();
    std::fs::write(r.join("group.now"), "节点01").unwrap();
    let (node, relay) = (Uuid::now_v7(), Uuid::now_v7());
    let entry = json!({"id": Uuid::now_v7(), "name": "团队2-03 · 线路机1:20003", "node_id": node, "relay_id": relay, "node_name": "团队2-03", "relay_name": "线路机1", "port": 20003, "lifecycle": "active"});
    // 原出口 节点01：streak 是它连续不通的次数，带着对应的入口
    let snapshot = |streak: u32, now: &str| {
        let mut s = snap(true, [streak, 0], true);
        s["exits"][0]["alive"] = json!(streak == 0);
        s["exits"][0]["entry"] = entry.clone();
        s["groups"] = json!([
            {"name": "GLOBAL", "type": "Selector", "now": "DIRECT", "all": ["DIRECT", "节点01", "节点02"], "used": false},
            {"name": "手动选择", "type": "Selector", "now": now, "all": ["节点01", "节点02"], "used": true},
            {"name": "旧快照", "type": "Selector", "now": "节点01", "all": ["节点01", "节点02"]},
        ]);
        s
    };
    status(&t, &id, true, 0, snapshot(0, "节点01")).await;
    let path = format!("/api/v1/routers/{id}/switch");
    let (code, v) = t.req("POST", &path, Some(json!({"group": "GLOBAL", "to": "节点02"})), None).await;
    assert!(code == StatusCode::CONFLICT && v["error"]["message"].as_str().unwrap().contains("没有规则用策略组「GLOBAL」，切了不影响流量"), "{code} {v}");
    assert!(read(&r.join("switch.log")).is_empty(), "没去动路由");
    // 未知的（旧快照）照常能切；这里不留记录，免得影响下面
    let (code, _) = t.req("POST", &path, Some(json!({"group": "旧快照", "to": "节点02"})), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(read(&r.join("switch.log")), "旧快照\n");
    t.post(&format!("{path}/back"), json!({"group": "旧快照"})).await;
    std::fs::write(r.join("switch.log"), "").unwrap();
    // 手动选择 切到 节点02，马上到时间；此时 节点01 连续 3 次不通
    t.post(&path, json!({"group": "手动选择", "to": "节点02", "back_after_s": 3600})).await;
    let past = || async { sqlx::query("update router_switch set back_at = now() - interval '1 second' where state = 'active'").execute(&t.st.db).await.unwrap(); };
    let sw = || async { t.get(&format!("/api/v1/routers/{id}")).await["switches"][0].clone() };
    past().await;
    status(&t, &id, true, 0, snapshot(3, "节点02")).await;
    routers::switch::due(&t.st).await;
    assert_eq!(read(&r.join("group.now")), "节点02", "原出口不通：不切回");
    let v = sw().await;
    assert_eq!(v["defer_why"], "原出口「节点01」现在不通（连续 3 次测延迟不通）", "{v}");
    assert!(v["deferred_at"].is_string(), "{v}");
    let later: bool = sqlx::query_scalar("select back_at > now() + interval '59 minutes' and back_at < now() + interval '61 minutes' from router_switch where state = 'active'").fetch_one(&t.st.db).await.unwrap();
    assert!(later, "推迟 1 小时：{v}");
    let a = t.get(&format!("/api/v1/audit?target_id={id}")).await[0].clone();
    assert_eq!((a["title"].as_str(), a["actor"].as_str()), (Some("推迟自动切回"), Some("系统")), "{a}");
    mock.wait("/hook/a", 1).await;
    let n = mock.texts("/hook/a").pop().unwrap();
    assert!(n.starts_with("【出口自动切回推迟】团队2-路由D\n策略组「手动选择」临时切到「节点02」，到时要切回「节点01」，但原出口「节点01」现在不通"), "{n}");
    // 出口通了，但它指向的节点有严重的出口不通告警：还是不切回，这次不再通知
    let inc = Uuid::now_v7();
    sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary) values ($1, 'machine', 'egress_fail', $2, 'critical', 's')").bind(inc).bind(node).execute(&t.st.db).await.unwrap();
    past().await;
    status(&t, &id, true, 0, snapshot(0, "节点02")).await;
    routers::switch::due(&t.st).await;
    assert_eq!(read(&r.join("group.now")), "节点02");
    let why = sw().await["defer_why"].as_str().unwrap().to_string();
    assert!(why.starts_with("原出口「节点01」指向的 团队2-03 有进行中的严重告警（"), "{why}");
    // 同机 IP 节点：agent 离线只由宿主机报，宿主机在告警也拦，文案写宿主机名
    sqlx::query("update incident set state = 'resolved', resolved_at = now() where id = $1").bind(inc).execute(&t.st.db).await.unwrap();
    let team = seed_team(&t, "团队9").await;
    let host = Uuid::now_v7();
    for (id, number, host_id) in [(host, 1, None), (node, 2, Some(host))] {
        sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, ssh_port, lifecycle, host_id, activated_at) values ($1, 'vps', $2, $3, $4::inet, $6, 'active', $5, now())")
            .bind(id)
            .bind(team)
            .bind(number)
            .bind(format!("203.0.113.{}", 40 + number))
            .bind(host_id)
            .bind(host_id.is_none().then_some(22))
            .execute(&t.st.db)
            .await
            .unwrap();
    }
    let inc = Uuid::now_v7();
    sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary) values ($1, 'machine', 'agent_offline', $2, 'critical', 's')").bind(inc).bind(host).execute(&t.st.db).await.unwrap();
    past().await;
    routers::switch::due(&t.st).await;
    assert_eq!(read(&r.join("group.now")), "节点02");
    let why = sw().await["defer_why"].as_str().unwrap().to_string();
    assert_eq!(why, "原出口「节点01」指向的 团队9-01 有进行中的严重告警（agent 离线）", "{why}");
    // 宿主机自己的出口不通 / TCP 不通不代表这个 IP 节点（它有自己的 IP 和探测）：不拦。都恢复了：切回，操作者是「系统」；通知一共只有最初那一条
    sqlx::query("update incident set state = 'resolved', resolved_at = now() where id = $1").bind(inc).execute(&t.st.db).await.unwrap();
    for rule in ["egress_fail", "tcp_fail"] {
        sqlx::query("insert into incident (id, engine, rule, subject_id, severity, summary) values ($1, 'machine', $2, $3, 'critical', 's')").bind(Uuid::now_v7()).bind(rule).bind(host).execute(&t.st.db).await.unwrap();
    }
    past().await;
    routers::switch::due(&t.st).await;
    assert_eq!((read(&r.join("group.now")), t.get(&format!("/api/v1/routers/{id}")).await["switches"].clone()), ("节点01".to_string(), json!([])));
    assert_eq!(mock.texts("/hook/a").len(), 1);
}
