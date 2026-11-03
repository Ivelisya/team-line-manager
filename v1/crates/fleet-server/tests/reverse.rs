//! 穿透开关的作业（jobs/steps/bridge.rs）：开启 = 推 bridge → 切换并验收；关闭 = 切换并验收 → 收起 bridge（还有别的穿透入口时只删隧道、不重启）；
//! 验收不过自动切回原来的模式、作业失败；取消补偿切回并重推 bridge。需要 PostgreSQL（见 common）。
//!
//! 机器是假的（同 tests/upgrade.rs）：PATH 最前面放一个假 ssh，远端命令在「机器根目录」`<临时目录>/<IP>/` 里真的执行
//! （/usr/local/bin、/etc、/run 改到里面），systemctl 换成桩、记下调用。dist 里的 xray 是个 shell 脚本（-test 一律通过）。
//! 线路机用 common 的 Sim：拉期望状态、回报应用成功、给每个入口报一个成功的出口探测；失败的那个测试自己报出口不通。
//! PATH 是整个测试进程共用的，所以单独一个测试文件。

#![cfg(unix)]

#[path = "it/common/mod.rs"]
mod common;

use axum::http::StatusCode;
use common::{now_ms, seed_relay, seed_relay_b, seed_team, setup, Sim, T};
use fleet_proto::{ApplyReport, DesiredState, EntryApply, Outbound, ProbeKind, ProbeSample, Report};
use fleet_server::jobs;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use uuid::Uuid;

const SSH: &str = r#"#!/bin/sh
for a; do last=$a; case $a in root@*) ip=${a#root@};; esac; done
export R="$FAKE_ROOT/$ip"
mkdir -p "$R/etc/systemd/system" "$R/run"
fix() { sed "s#/run/#$R/run/#g; s#/usr/local/bin/#$R/usr/local/bin/#g; s#/etc/#$R/etc/#g; s#/var/lib/#$R/var/lib/#g"; }
if [ "$last" = "bash -s" ]; then fix | bash -s; else sh -c "$(printf '%s' "$last" | fix)"; fi
"#;

/// systemctl 桩：记下调用；单元都算在跑、主进程号不变（没有起来就退出的）
const SYSTEMCTL: &str = r#"#!/bin/sh
echo "$*" >> "$R/systemctl.log"
case "$*" in
  *is-active*) echo active ;;
  *MainPID*) echo 4242 ;;
esac
exit 0
"#;

/// 去掉「-g 组」，其余交给真的 install
const INSTALL: &str = r#"#!/bin/sh
for a; do shift; if [ -n "$skip" ]; then skip=; elif [ "$a" = -g ]; then skip=1; else set -- "$@" "$a"; fi; done
exec /usr/bin/install "$@"
"#;

/// dist 里的 xray：version 打印生产同款版本行；-test 同真 xray 按 -c 的扩展名认格式，认不出又没给 -format 就失败（报错在标准输出），
/// 其余一律通过
const XRAY: &str = r#"#!/bin/sh
[ "$1" = version ] && { echo 'Xray 26.3.27 (Xray, Penetrates Everything.) d2758a0'; exit 0; }
fmt=; cfg=
while [ $# -gt 0 ]; do case "$1" in -format) fmt=$2; shift;; -c) cfg=$2; shift;; esac; shift; done
case "$fmt:$cfg" in :|:*.json|:*.yaml|:*.yml|:*.toml|?*:*) exit 0;; esac
echo "Failed to start: main: failed to load config files: [$cfg] > core: Failed to get format of $cfg"
exit 23
"#;

static FAKE: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    exe(&bin.join("ssh"), SSH);
    exe(&bin.join("systemctl"), SYSTEMCTL);
    exe(&bin.join("install"), INSTALL);
    std::env::set_var("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()));
    std::env::set_var("FAKE_ROOT", dir.path());
    dir
});

fn exe(p: &Path, body: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn read(p: &Path) -> String { std::fs::read_to_string(p).unwrap_or_default() }

/// 一台在用的 VPS 节点（agent 报过会穿透，有 SS 出站参数），机器根目录在假 ssh 的目录下。中控 dist 里放好 xray
async fn node(t: &T, ip: &str) -> (Uuid, PathBuf) {
    LazyLock::force(&FAKE);
    exe(&t.dir.path().join("dist/xray"), XRAY);
    let team = seed_team(t, "团队1").await;
    let id = Uuid::now_v7();
    sqlx::query("insert into machine (id, kind, team_id, number, primary_ip, ssh_port, lifecycle, activated_at) values ($1, 'vps', $2, 1, $3::inet, 22, 'active', now())")
        .bind(id)
        .bind(team)
        .bind(ip)
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into node_outbound (machine_id, protocol, host, port, method, secret_enc) values ($1, 'shadowsocks', $2, 8388, 'aes-256-gcm', $3)")
        .bind(id)
        .bind(ip)
        .bind(t.st.secrets.seal_str("sspw"))
        .execute(&t.st.db)
        .await
        .unwrap();
    sqlx::query("insert into agent_state (machine_id, agent_version, last_report_at, info) values ($1, 'v', now(), '{\"caps\": [\"reverse\"]}')").bind(id).execute(&t.st.db).await.unwrap();
    (id, FAKE.path().join(ip))
}

async fn entry(t: &T, relay: Uuid, node: Uuid, mode: &str) -> Uuid {
    let mut tx = t.st.db.begin().await.unwrap();
    let (e, _, _) = fleet_server::entries::create_with(&mut tx, relay, node, mode).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("update entry set lifecycle = 'active' where id = $1").bind(e).execute(&t.st.db).await.unwrap();
    e
}

async fn mode_of(t: &T, e: Uuid) -> String { sqlx::query_scalar("select mode from entry where id = $1").bind(e).fetch_one(&t.st.db).await.unwrap() }

async fn switch(t: &T, e: Uuid, on: bool) -> String {
    t.post(&format!("/api/v1/entries/{e}/reverse"), json!({"on": on})).await["job_id"].as_str().unwrap().to_string()
}

/// 一轮：各线路机拉期望状态、回报、报出口探测（`manual` 的那台由测试自己报：穿透入口的出口探测通不通由它定）；再领取并推进一次作业
async fn round(t: &T, sims: &mut [&mut Sim], manual: Option<(&str, &mut u64, bool)>, job: &str) -> Value {
    for s in sims.iter_mut() {
        s.round(t).await;
    }
    if let Some((tok, seq, reverse_ok)) = manual {
        relay_round(t, tok, seq, reverse_ok).await;
    }
    sqlx::query("update job set run_after = now() where id = $1::uuid").bind(job).execute(&t.st.db).await.unwrap();
    if let Some(j) = jobs::claim(&t.st, "test").await.unwrap() {
        jobs::advance(&t.st, j, "test").await.unwrap();
    }
    t.job(job).await
}

/// 推到作业结束（成功、失败、取消）
async fn drive(t: &T, sims: &mut [&mut Sim], job: &str) -> Value {
    for _ in 0..100 {
        let j = round(t, sims, None, job).await;
        if matches!(j["state"].as_str(), Some("succeeded" | "failed" | "cancelled")) {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("作业 {job} 没结束：{}", t.job(job).await);
}

/// 线路机回报应用成功；`reverse_ok` 为假时穿透入口的出口探测不通（隧道挂不上）
async fn relay_round(t: &T, tok: &str, seq: &mut u64, reverse_ok: bool) {
    let (s, v) = t.req("GET", "/agent/v1/desired?after=0", None, Some(tok)).await;
    assert_eq!(s, StatusCode::OK);
    let ds: DesiredState = serde_json::from_value(v).unwrap();
    let rep = ApplyReport { generation: ds.generation, ok: true, error: None, entries: ds.entries.iter().map(|e| EntryApply { id: e.id, ok: true, error: None }).collect() };
    t.req("POST", "/agent/v1/applied", Some(serde_json::to_value(&rep).unwrap()), Some(tok)).await;
    *seq += 1;
    let probes = ds
        .entries
        .iter()
        .map(|e| {
            let ok = reverse_ok || !matches!(e.outbound, Outbound::Reverse { .. });
            ProbeSample { entry_id: e.id, ts_ms: now_ms(), kind: ProbeKind::Egress, ok, latency_ms: None, detail: Some(if ok { e.expect_egress_ip.to_string() } else { "连接被关闭".into() }), retrans: None }
        })
        .collect();
    let r = Report { seq: *seq, boot_id: "b".into(), agent_version: "v".into(), sent_at_ms: now_ms(), probes, caps: vec!["reverse".into()], ..Default::default() };
    let (s, v) = t.req("POST", "/agent/v1/report", Some(serde_json::to_value(&r).unwrap()), Some(tok)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

fn bridge_restarts(root: &Path) -> usize { read(&root.join("systemctl.log")).lines().filter(|l| *l == "restart fleet-xray-bridge").count() }

/// 开：推 xray、装单元、写配置（两条隧道）、重启 bridge，切到穿透、经入口验收；关掉其中一条：只删隧道不重启；
/// 关掉最后一条：bridge 停掉删掉。线路机拿到的期望状态跟着变
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_on_then_off() {
    let t = setup(false).await;
    let ((ra, ta), (rb, tb)) = (seed_relay(&t).await, seed_relay_b(&t).await);
    let (mut a, mut b) = (Sim::new(ta), Sim::new(tb));
    let (n, root) = node(&t, "203.0.113.51").await;
    let e1 = entry(&t, ra, n, "normal").await;
    // 另一台线路机上已经有一条穿透入口（bridge 还没装：开 e1 时一起装上）
    let e2 = entry(&t, rb, n, "reverse").await;
    // 线路机先报一轮（带 caps：会穿透）
    a.round(&t).await;
    b.round(&t).await;
    let job = switch(&t, e1, true).await;
    let j = drive(&t, &mut [&mut a, &mut b], &job).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    assert_eq!(mode_of(&t, e1).await, "reverse");
    let keys: Vec<(&str, &str)> = j["steps"].as_array().unwrap().iter().map(|s| (s["key"].as_str().unwrap(), s["state"].as_str().unwrap())).collect();
    assert_eq!(keys, [("bridge_up", "done"), ("switch", "done"), ("bridge_down", "skipped")]);
    assert_eq!(j["steps"][0]["output"]["tunnels"], 2);
    assert_eq!(j["steps"][1]["output"]["egress_ip"], "203.0.113.51");
    // 节点上：xray 程序、单元、配置（0600 目录里），两条隧道连各自线路机的入口端口，bridge 重启过一次
    assert!(root.join("usr/local/bin/xray").exists());
    assert!(read(&root.join("etc/systemd/system/fleet-xray-bridge.service")).contains("fleet-bridge/xray.json"), "单元里的路径被假 ssh 改到了机器根目录下");
    let cfg: Value = serde_json::from_str(&read(&root.join("etc/fleet-bridge/xray.json"))).unwrap();
    let addrs: Vec<String> = cfg["outbounds"].as_array().unwrap().iter().filter(|o| o["protocol"] == "vless").map(|o| format!("{}:{}", o["settings"]["address"].as_str().unwrap(), o["settings"]["port"])).collect();
    let ports: Vec<(Uuid, i32)> = sqlx::query_as("select id, port from entry where id = any($1)").bind(vec![e1, e2]).fetch_all(&t.st.db).await.unwrap();
    let port = |e: Uuid| ports.iter().find(|p| p.0 == e).unwrap().1;
    let (p1, p2) = (port(e1), port(e2));
    let mut want = vec![format!("198.51.100.2:{p1}"), format!("198.51.100.3:{p2}")];
    want.sort();
    let mut got = addrs.clone();
    got.sort();
    assert_eq!(got, want);
    assert_eq!(bridge_restarts(&root), 1, "{}", read(&root.join("systemctl.log")));
    // 线路机拿到的是穿透出站
    let ds = a.round(&t).await;
    assert!(matches!(ds.entries.iter().find(|e| e.id == e1).unwrap().outbound, Outbound::Reverse { .. }));

    // 关掉 e1：切回 SS、验收，bridge 只去掉这条隧道、不重启
    let job = switch(&t, e1, false).await;
    let j = drive(&t, &mut [&mut a, &mut b], &job).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    assert_eq!(mode_of(&t, e1).await, "normal");
    assert_eq!((j["steps"][0]["state"].as_str(), j["steps"][2]["output"]["action"].as_str()), (Some("skipped"), Some("quiet")), "{j}");
    let cfg: Value = serde_json::from_str(&read(&root.join("etc/fleet-bridge/xray.json"))).unwrap();
    assert_eq!(cfg["outbounds"].as_array().unwrap().iter().filter(|o| o["protocol"] == "vless").count(), 1);
    assert_eq!(bridge_restarts(&root), 1, "只删隧道不重启");
    assert!(matches!(a.round(&t).await.entries.iter().find(|e| e.id == e1).unwrap().outbound, Outbound::Shadowsocks { .. }));

    // 关掉最后一条：bridge 停掉、单元和配置删掉
    let job = switch(&t, e2, false).await;
    let j = drive(&t, &mut [&mut a, &mut b], &job).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    assert_eq!(j["steps"][2]["output"]["action"], "remove");
    assert!(!root.join("etc/fleet-bridge/xray.json").exists() && !root.join("etc/systemd/system/fleet-xray-bridge.service").exists());
    assert!(read(&root.join("systemctl.log")).contains("disable --now fleet-xray-bridge"));
}

/// 推到作业结束，线路机由测试自己报（见 relay_round）
async fn drive_manual(t: &T, tok: &str, seq: &mut u64, reverse_ok: bool, job: &str) -> Value {
    for _ in 0..100 {
        let j = round(t, &mut [], Some((tok, &mut *seq, reverse_ok)), job).await;
        if matches!(j["state"].as_str(), Some("succeeded" | "failed" | "cancelled")) {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("作业 {job} 没结束：{}", t.job(job).await);
}

/// 开启后出口验收一直不过：自动切回普通模式、作业失败（错误写明已切回），这条隧道从 bridge 上撤掉（节点上没有别的穿透入口：整个收起）；
/// 修好后重试：从切换那一步重来，先把隧道推回去，验收通过。另一次开启在推完 bridge 后取消：补偿收起 bridge，模式不变
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_verify_reverts_then_retry_and_cancel() {
    let t = setup(false).await;
    let (ra, ta) = seed_relay(&t).await;
    let (n, root) = node(&t, "203.0.113.52").await;
    let e = entry(&t, ra, n, "normal").await;
    let mut seq = 0u64;
    relay_round(&t, &ta, &mut seq, true).await;
    let job = switch(&t, e, true).await;
    let mut j = Value::Null;
    for i in 0..60 {
        j = round(&t, &mut [], Some((&ta, &mut seq, false)), &job).await;
        if !matches!(j["state"].as_str(), Some("running" | "waiting" | "queued")) {
            break;
        }
        // 切换之后线路机已应用、出口一直不通：把这一步的开始时间往前挪到超时
        if i == 5 {
            sqlx::query("update job_step set started_at = now() - interval '500 seconds' where job_id = $1::uuid and key = 'switch'").bind(&job).execute(&t.st.db).await.unwrap();
        }
    }
    assert_eq!(j["state"], "failed", "{j}");
    let err = j["error"].as_str().unwrap();
    assert!(err.contains("出口探测一直没通过") && err.contains("已自动切回普通模式"), "{err}");
    assert_eq!(mode_of(&t, e).await, "normal", "切回去了");
    assert!(!root.join("etc/fleet-bridge/xray.json").exists(), "隧道从 bridge 上撤掉：节点上没有别的穿透入口，整个收起");
    // 修好之后重试：先把隧道推回去，再切、验收
    t.post(&format!("/api/v1/jobs/{job}/retry"), json!({})).await;
    let j = drive_manual(&t, &ta, &mut seq, true, &job).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    assert_eq!(mode_of(&t, e).await, "reverse");
    assert!(root.join("etc/fleet-bridge/xray.json").exists());

    // 关掉之后再开、推完 bridge 就取消：补偿收起 bridge，入口还是普通
    let off = switch(&t, e, false).await;
    assert_eq!(drive_manual(&t, &ta, &mut seq, true, &off).await["state"], "succeeded");
    let on = switch(&t, e, true).await;
    let j = round(&t, &mut [], Some((&ta, &mut seq, true)), &on).await;
    assert_eq!(j["steps"][0]["state"], "done", "{j}");
    assert!(root.join("etc/fleet-bridge/xray.json").exists());
    t.post(&format!("/api/v1/jobs/{on}/cancel"), json!({})).await;
    let j = drive_manual(&t, &ta, &mut seq, true, &on).await;
    assert_eq!(j["state"], "cancelled", "{j}");
    assert!(!root.join("etc/fleet-bridge/xray.json").exists(), "取消后 bridge 收起");
    assert_eq!(mode_of(&t, e).await, "normal");
}

/// 带 bridge 端口开穿透：线路机 agent 不认 bridge_port（旧）/ 端口不可用 / 被另一个入口占着 都被拒；
/// 成功后节点 bridge 配置连 bridge 端口、线路机期望状态里带 bridge_port（普通入口不带）、entries 视图有它；关掉后清掉
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_port_on_then_off() {
    let t = setup(false).await;
    let (ra, ta) = seed_relay(&t).await;
    let mut a = Sim::new(ta);
    let (n, root) = node(&t, "203.0.113.53").await;
    let e = entry(&t, ra, n, "normal").await;
    a.round(&t).await;
    let url = format!("/api/v1/entries/{e}/reverse");
    let bp_of = |t: &T| {
        let db = t.st.db.clone();
        async move { sqlx::query_scalar::<_, Option<i32>>("select bridge_port from entry where id = $1").bind(e).fetch_one(&db).await.unwrap() }
    };
    // 线路机 agent 旧（只报 reverse）：拒绝并提示升级；不带 bridge 端口照旧能开（预览）
    sqlx::query("update agent_state set info = info || jsonb_build_object('caps', $2::jsonb) where machine_id = $1").bind(ra).bind(json!(["reverse"])).execute(&t.st.db).await.unwrap();
    let (s, v) = t.req("POST", &url, Some(json!({"on": true, "bridge_port": 8443})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("不认 bridge 端口") && v.to_string().contains("upgrade-agent"), "{v}");
    assert_eq!(bp_of(&t).await, None, "被拒不写库");
    assert_eq!(t.get(&format!("{url}?on=true")).await["ok"], true);
    a.round(&t).await; // 回报新能力
    // 端口不可用：0、22、超出临时端口以下的范围、Xray API 端口；别的入口占着
    for bad in [0, 22, 10240, 65535, 62789] {
        let (s, v) = t.req("POST", &url, Some(json!({"on": true, "bridge_port": bad})), None).await;
        assert_eq!(s, StatusCode::CONFLICT, "{bad}: {v}");
        assert!(v.to_string().contains("不可用"), "{bad}: {v}");
    }
    let team: Uuid = sqlx::query_scalar("select team_id from machine where id = $1").bind(n).fetch_one(&t.st.db).await.unwrap();
    let other = common::seed_node(&t, team, 2, "203.0.113.54", "active").await;
    let e2 = entry(&t, ra, other, "normal").await;
    sqlx::query("update entry set bridge_port = 8443 where id = $1").bind(e2).execute(&t.st.db).await.unwrap();
    let (s, v) = t.req("POST", &url, Some(json!({"on": true, "bridge_port": 8443})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("另一个入口"), "{v}");
    sqlx::query("update entry set bridge_port = null where id = $1").bind(e2).execute(&t.st.db).await.unwrap();
    // 关闭时不能带 bridge 端口
    assert_eq!(t.req("POST", &url, Some(json!({"on": false, "bridge_port": 8443})), None).await.0, StatusCode::BAD_REQUEST);

    // 成功
    let pv = t.get(&format!("{url}?on=true&bridge_port=8443")).await;
    assert_eq!(pv["ok"], true, "{pv}");
    assert!(pv["impacts"].to_string().contains("198.51.100.2:8443") && pv["impacts"].to_string().contains("节点出站只开少数端口"), "{pv}");
    let d = t.post(&url, json!({"on": true, "bridge_port": 8443})).await;
    let job = d["job_id"].as_str().unwrap().to_string();
    let j = drive(&t, &mut [&mut a], &job).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    assert_eq!(bp_of(&t).await, Some(8443));
    // 节点 bridge 连线路机 IP:8443，不是入口端口
    let cfg: Value = serde_json::from_str(&read(&root.join("etc/fleet-bridge/xray.json"))).unwrap();
    let tunnel = cfg["outbounds"].as_array().unwrap().iter().find(|o| o["protocol"] == "vless").unwrap();
    assert_eq!((tunnel["settings"]["address"].as_str(), tunnel["settings"]["port"].as_i64()), (Some("198.51.100.2"), Some(8443)));
    // 线路机期望状态：穿透入口带 bridge_port；视图里也有
    let ds = a.round(&t).await;
    assert_eq!(ds.entries.iter().find(|x| x.id == e).unwrap().bridge_port, Some(8443));
    let view = t.get(&format!("/api/v1/machines/{ra}")).await;
    assert_eq!(view["entries"].as_array().unwrap().iter().find(|x| x["id"] == json!(e)).unwrap()["bridge_port"], 8443, "{view}");

    // 关掉：切回普通，bridge 端口清掉，期望状态里没有
    let off = switch(&t, e, false).await;
    let j = drive(&t, &mut [&mut a], &off).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    assert_eq!(bp_of(&t).await, None);
    assert_eq!(a.round(&t).await.entries.iter().find(|x| x.id == e).unwrap().bridge_port, None);
}
