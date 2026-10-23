//! 装探测点：两种方式共用一份安装脚本（[`INSTALL`]，POSIX sh：Alpine 是 busybox ash，没有 bash）。
//! - 中控经 SSH 装（`fleetctl probe add <名> --host <IP>`）：用中控自己的密钥登录 root，主机指纹首次记录、之后严格校验（同上架，ssh.rs），
//!   推 dist 里的 fleet-agent 到 /opt/fleet/，跑安装脚本，等首次上报。接口立即返回，进度写在 probe_point 的 install_* 列（同软路由的一键部署）；
//!   登不进时报错里给出中控公钥全文，让用户加进探测点的 authorized_keys。
//! - 一次性安装命令（不给 --host）：30 分钟内有效、只能用一次（同软路由的接入命令）。用户在探测点上以 root 执行
//!   `curl … | sh`，中控这时才发上报令牌，脚本用它下载 `/agent/v1/probe/binary`。
//!
//! 每次安装都换一把新令牌（旧的随之作废）。安装脚本只新增：系统用户 fleet-probe、/opt/fleet/、/etc/fleet/probe.toml（0600，含令牌）、
//! 服务（systemd：/etc/systemd/system/fleet-probe.service；OpenRC：/etc/init.d/fleet-probe，supervise-daemon 守护、开机启动、
//! 日志 /var/log/fleet-probe.log）。服务以 fleet-probe 身份跑（只建连、不监听端口），/opt/fleet 归它：升级时它自己换程序。
//! 删掉探测点后机器上的东西中控不去动，打印清理命令（[`CLEANUP`]）。
//!
//! 升级（`fleetctl probe upgrade`）：记下 dist 里程序的 sha256（upgrade_sha256），探测点下次取目标时看到、自己下载、核对 sha256、
//! 试跑 `--version`，把旧的留成 fleet-agent.prev 再换上，退出让服务管理器拉起新的。不需要中控能 SSH 到它。

use crate::db::audit;
use crate::error::{ApiError, ApiResult};
use crate::jobs::scripts::vars;
use crate::ssh::{ErrKind, Target};
use crate::state::AppState;
use futures::FutureExt;
use serde_json::json;
use std::panic::AssertUnwindSafe;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

/// 一次性安装命令的有效期（同软路由的接入命令）
pub const BOOTSTRAP_MINUTES: i64 = 30;
/// 装好后等首次上报最多这么久（探测点启动先取目标、测一轮，十几秒）
const FIRST_REPORT_WAIT: Duration = Duration::from_secs(90);

/// 安装脚本。需要 SERVER_URL、TOKEN、NAME；FETCH=1 时从中控下载程序（一次性安装命令），否则程序已经推到 /opt/fleet/fleet-agent（SSH 安装）
pub const INSTALL: &str = r#"
set -eu
[ "$(id -u)" = 0 ] || { echo "fleet：要用 root 执行" >&2; exit 2; }
case "$(uname -m)" in x86_64|amd64) ;; *) echo "fleet：只支持 x86_64（这台是 $(uname -m)）" >&2; exit 2 ;; esac
if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then INIT=systemd
elif [ -x /sbin/openrc-run ] && command -v rc-update >/dev/null 2>&1; then INIT=openrc
else echo "fleet：认不出 init 系统（只支持 systemd 与 OpenRC）" >&2; exit 2; fi
mkdir -p /opt/fleet /etc/fleet
if [ "${FETCH:-0}" = 1 ]; then
  T=/opt/fleet/.fleet-agent.$$
  if command -v curl >/dev/null 2>&1; then
    curl -fsS --retry 3 -H "Authorization: Bearer $TOKEN" -o "$T" "$SERVER_URL/agent/v1/probe/binary"
  else
    wget -q --header "Authorization: Bearer $TOKEN" -O "$T" "$SERVER_URL/agent/v1/probe/binary"
  fi
  chmod 755 "$T"
  "$T" --version >/dev/null || { rm -f "$T"; echo "fleet：下载的 fleet-agent 跑不起来" >&2; exit 2; }
  mv -f "$T" /opt/fleet/fleet-agent
fi
[ -x /opt/fleet/fleet-agent ] || { echo "fleet：没有 /opt/fleet/fleet-agent" >&2; exit 2; }
# 重装：上次升级留下的试运行 / 坏版本标记作废（留着的话，新装的程序一启动就被当成试运行到时、换回 fleet-agent.prev）
rm -f /opt/fleet/.fleet-agent.trial /opt/fleet/.fleet-agent.bad-sha256
if ! id fleet-probe >/dev/null 2>&1; then
  if command -v useradd >/dev/null 2>&1; then useradd -r -M -d /nonexistent -s /sbin/nologin fleet-probe
  else adduser -S -D -H -h /nonexistent -s /sbin/nologin fleet-probe; fi
fi
umask 077
printf 'server_url = "%s"\ntoken = "%s"\nname = "%s"\n' "$SERVER_URL" "$TOKEN" "$NAME" > /etc/fleet/probe.toml.tmp
chown fleet-probe /etc/fleet/probe.toml.tmp
chmod 600 /etc/fleet/probe.toml.tmp
mv -f /etc/fleet/probe.toml.tmp /etc/fleet/probe.toml
chown -R fleet-probe /opt/fleet
umask 022
if [ "$INIT" = systemd ]; then
  cat > /etc/systemd/system/fleet-probe.service <<'EOF'
# fleet 探测点：测线路机的客户入口通不通（中控装的；面板上删掉探测点后按提示清理）
[Unit]
Description=fleet probe point
After=network-online.target
Wants=network-online.target

[Service]
User=fleet-probe
ExecStart=/opt/fleet/fleet-agent probe -c /etc/fleet/probe.toml
Restart=always
RestartSec=5
MemoryMax=64M
NoNewPrivileges=yes
ProtectSystem=full
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload
  systemctl enable fleet-probe >/dev/null 2>&1
  systemctl restart fleet-probe
else
  cat > /etc/init.d/fleet-probe <<'EOF'
#!/sbin/openrc-run
# fleet 探测点：测线路机的客户入口通不通（中控装的；面板上删掉探测点后按提示清理）
description="fleet probe point"
supervisor=supervise-daemon
command=/opt/fleet/fleet-agent
command_args="probe -c /etc/fleet/probe.toml"
command_user=fleet-probe
respawn_delay=5
respawn_max=0
output_log=/var/log/fleet-probe.log
error_log=/var/log/fleet-probe.log

depend() {
	need net
	after firewall
}

start_pre() {
	checkpath -f -o fleet-probe -m 0640 /var/log/fleet-probe.log
}
EOF
  chmod 755 /etc/init.d/fleet-probe
  rc-update add fleet-probe default >/dev/null 2>&1 || true
  rc-service fleet-probe restart >/dev/null 2>&1 || rc-service fleet-probe start
fi
echo "fleet_probe=ok init=$INIT"
"#;

/// 删掉探测点后在它上面执行的清理（systemd 与 OpenRC 都写上，不在的那一套报错忽略）
pub const CLEANUP: &str = "systemctl disable --now fleet-probe 2>/dev/null; rm -f /etc/systemd/system/fleet-probe.service; \
     rc-service fleet-probe stop 2>/dev/null; rc-update del fleet-probe default 2>/dev/null; rm -f /etc/init.d/fleet-probe; \
     rm -rf /opt/fleet /etc/fleet/probe.toml /var/log/fleet-probe.log; deluser fleet-probe 2>/dev/null || userdel fleet-probe";

/// 安装脚本连同参数
pub fn script(server_url: &str, token: &str, name: &str, fetch: bool) -> String {
    vars(&[("SERVER_URL", server_url.trim_end_matches('/')), ("TOKEN", token), ("NAME", name), ("FETCH", if fetch { "1" } else { "0" })]) + INSTALL
}

/// 一次性安装命令取到的整份脚本
pub fn bootstrap_script(server_url: &str, token: &str, name: &str) -> String {
    format!("#!/bin/sh\n# fleet：探测点「{name}」的安装脚本（一次性）\n{}echo \"fleet：装好了，半分钟内面板上显示在线\"\n", script(server_url, token, name, true))
}

/// 一次性安装命令（curl 或 wget，Alpine 默认只有 busybox wget）
pub fn bootstrap_cmd(public_url: &str, token: &str) -> String {
    let u = format!("{}/api/v1/probe-bootstrap/{token}", public_url.trim_end_matches('/'));
    format!("{{ curl -fsS {u} 2>/dev/null || wget -qO- {u}; }} | sh")
}

/// 发一个一次性安装令牌：(明文, sha256, 过期时间)
pub fn new_bootstrap() -> (String, Vec<u8>, OffsetDateTime) {
    let t = crate::crypto::random_token(32);
    let h = crate::crypto::sha256(t.as_bytes());
    (t, h, OffsetDateTime::now_utc() + time::Duration::minutes(BOOTSTRAP_MINUTES))
}

/// 中控的登录公钥（一行），读不到为 None
async fn central_pubkey(st: &AppState) -> Option<String> {
    let s = tokio::fs::read_to_string(st.cfg.ssh_key.with_extension("pub")).await.ok()?;
    s.lines().next().map(|l| l.trim().to_string()).filter(|l| l.starts_with("ssh-"))
}

/// 登不进时的说明：中控公钥全文，加进探测点 root 的 authorized_keys；或改用一次性安装命令
async fn login_help(st: &AppState, host: &str, port: u16, name: &str, err: &str) -> String {
    let key = central_pubkey(st).await.unwrap_or_else(|| format!("（读不到 {}）", st.cfg.ssh_key.with_extension("pub").display()));
    format!(
        "中控用自己的密钥登录不了 root@{host}:{port}（{err}）。在探测点上用 root 把中控的公钥加进 authorized_keys 后重试：\n\
         mkdir -p /root/.ssh && chmod 700 /root/.ssh && echo '{key}' >> /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys\n\
         中控公钥：{key}\n\
         或者不让中控 SSH：fleetctl probe install {name}（不给 --host）打印一次性安装命令，在探测点上执行"
    )
}

/// 开始经 SSH 安装（接口调）：已有安装在跑 409；作废没用的安装命令；写「已开始」的审计；后台去跑
pub async fn start_ssh(st: &AppState, id: Uuid, host: String, port: u16, reset_host_key: bool, actor: &str) -> ApiResult<()> {
    if crate::jobs::dist_agent_sha256(st).await.is_err() {
        return Err(ApiError::conflict(format!("中控上没有可推送的 fleet-agent（{}）", st.cfg.dist_dir.join("fleet-agent").display())));
    }
    let mut tx = st.db.begin().await?;
    let cur: Option<(String, Option<String>)> =
        sqlx::query_as("select name, install_state from probe_point where id = $1 for update").bind(id).fetch_optional(&mut *tx).await?;
    let name = match cur {
        None => return Err(ApiError::not_found("没有这个探测点")),
        Some((_, Some(s))) if s == "running" => return Err(ApiError::conflict("这个探测点正在安装，等它结束再来")),
        Some((n, _)) => n,
    };
    sqlx::query(
        "update probe_point set install_state = 'running', install_step = '连接探测点', install_error = null, install_host = $2, install_port = $3,
            install_started_at = now(), install_finished_at = null, bootstrap_token_hash = null, bootstrap_expires_at = null, updated_at = now()
         where id = $1",
    )
    .bind(id)
    .bind(&host)
    .bind(port as i32)
    .execute(&mut *tx)
    .await?;
    let aid = crate::db::audit_insert(
        &mut *tx,
        actor,
        "probe_point.install",
        "probe_point",
        Some(id),
        json!({"name": name, "method": "ssh", "host": host, "port": port, "reset_host_key": reset_host_key, "started": true}),
    )
    .await?;
    tx.commit().await?;
    super::changed(st);
    let st = st.clone();
    tokio::spawn(async move {
        let res = AssertUnwindSafe(run_ssh(&st, id, &host, port, reset_host_key, &name)).catch_unwind().await;
        let err = match res {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some("安装过程中中控出了意外（内部错误），请重试".to_string()),
        };
        if let Err(e) = finish(&st, id, aid, err.as_deref()).await {
            tracing::error!(probe_point = %id, "写探测点安装结果失败：{e:#}");
        }
        super::changed(&st);
    });
    Ok(())
}

async fn step(st: &AppState, id: Uuid, s: &str) {
    let _ = sqlx::query("update probe_point set install_step = $2 where id = $1 and install_state = 'running'").bind(id).bind(s).execute(&st.db).await;
    st.emit(crate::state::Event::Settings);
}

async fn run_ssh(st: &AppState, id: Uuid, host: &str, port: u16, reset: bool, name: &str) -> Result<(), String> {
    crate::ssh::ssh_probe(host, port, Duration::from_secs(5)).await.map_err(|why| format!("SSH {host}:{port} {why}"))?;
    crate::ssh::pin_host_key(&st.cfg.known_hosts, host, port, reset).await.map_err(|e| match crate::ssh::kind_of(&e) {
        ErrKind::HostKey => format!(
            "{host}:{port} 的主机指纹和中控记下的不一样（{}）。确认是同一台机器重装过系统，再加 --reset-host-key 重装；否则可能是 IP 被冒用",
            e.to_string().split('：').nth(1).unwrap_or("指纹变了")
        ),
        _ => format!("记录 {host}:{port} 的主机指纹失败：{e:#}"),
    })?;
    let t = Target { ip: host.to_string(), port, key: st.cfg.ssh_key.clone(), known_hosts: st.cfg.known_hosts.clone(), password: None };
    step(st, id, "登录探测点").await;
    let out = t.run_sh("id -u; uname -m", Duration::from_secs(30)).await.map_err(|e| format!("登录 {host}:{port} 失败：{e:#}"))?;
    if out.ssh_kind() == Some(ErrKind::Auth) {
        return Err(login_help(st, host, port, name, out.stderr.trim()).await);
    }
    if !out.ok() {
        return Err(out.fail(&format!("登录 {host}:{port} 失败")).to_string());
    }
    let mut it = out.stdout.lines();
    if it.next().map(str::trim) != Some("0") {
        return Err("登录上去的不是 root：中控要以 root 登录才能装服务".into());
    }
    if let Some(arch) = it.next().map(str::trim).filter(|a| !matches!(*a, "x86_64" | "amd64")) {
        return Err(format!("探测点是 {arch}，dist 里的 fleet-agent 只有 x86_64 的"));
    }
    step(st, id, "上传 fleet-agent").await;
    t.push(&st.cfg.dist_dir.join("fleet-agent"), "/opt/fleet/fleet-agent", "755", Duration::from_secs(300)).await.map_err(|e| format!("{e:#}"))?;
    step(st, id, "安装服务").await;
    let token = crate::crypto::random_token(32);
    sqlx::query("update probe_point set token_hash = $2, upgrade_sha256 = null, upgrade_fetched_at = null, upgrade_failed = null, updated_at = now() where id = $1")
        .bind(id)
        .bind(crate::crypto::sha256(token.as_bytes()))
        .execute(&st.db)
        .await
        .map_err(|e| format!("写令牌失败：{e}"))?;
    let out = t.run_sh(&script(&st.cfg.public_url, &token, name, false), Duration::from_secs(120)).await.map_err(|e| format!("安装服务失败：{e:#}"))?;
    if !out.ok() {
        return Err(out.fail("安装服务失败").to_string());
    }
    step(st, id, "等待首次上报").await;
    wait_first_report(st, id).await
}

/// 等这次安装开始之后的第一份上报
async fn wait_first_report(st: &AppState, id: Uuid) -> Result<(), String> {
    let until = tokio::time::Instant::now() + FIRST_REPORT_WAIT;
    while tokio::time::Instant::now() < until {
        let ok: Option<bool> = sqlx::query_scalar("select last_seen_at >= install_started_at from probe_point where id = $1")
            .bind(id)
            .fetch_optional(&st.db)
            .await
            .map_err(|e| e.to_string())?
            .flatten();
        match ok {
            None if !exists(st, id).await => return Err("安装过程中探测点被删掉了".into()),
            Some(true) => return Ok(()),
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
    Err(format!(
        "服务装好了，但 {} 秒内没收到它的上报：在探测点上看日志（OpenRC：tail /var/log/fleet-probe.log；systemd：journalctl -u fleet-probe），\
         多半是连不上中控（{}）",
        FIRST_REPORT_WAIT.as_secs(),
        st.cfg.public_url
    ))
}

async fn exists(st: &AppState, id: Uuid) -> bool {
    sqlx::query_scalar::<_, i32>("select 1 from probe_point where id = $1").bind(id).fetch_optional(&st.db).await.ok().flatten().is_some()
}

async fn finish(st: &AppState, id: Uuid, audit_id: i64, err: Option<&str>) -> sqlx::Result<()> {
    sqlx::query(
        "update probe_point set install_state = case when $2::text is null then 'done' else 'failed' end, install_step = null, install_error = $2,
            install_finished_at = now(), updated_at = now()
         where id = $1 and install_state = 'running'",
    )
    .bind(id)
    .bind(err)
    .execute(&st.db)
    .await?;
    let result = match err {
        None => json!({"result": "ok"}),
        Some(e) => json!({"result": "failed", "error": e}),
    };
    crate::db::audit_finish(&st.db, audit_id, result).await
}

/// 中控重启时还在「安装中」的：任务已经没了，标为失败（probe_points::run_forever 启动时一次）
pub async fn mark_interrupted(st: &AppState) -> sqlx::Result<()> {
    sqlx::query(
        "update probe_point set install_state = 'failed', install_step = null, install_error = '中控重启，安装中断：请重新安装', install_finished_at = now()
         where install_state = 'running'",
    )
    .execute(&st.db)
    .await?;
    Ok(())
}

/// 发一条一次性安装命令（作废这个探测点之前没用的那条），返回 (命令, 过期时间)
pub async fn issue_bootstrap(st: &AppState, id: Uuid, actor: &str) -> ApiResult<(String, OffsetDateTime)> {
    let (t, h, exp) = new_bootstrap();
    let mut tx = st.db.begin().await?;
    let name: Option<String> = sqlx::query_scalar(
        "update probe_point set bootstrap_token_hash = $2, bootstrap_expires_at = $3, updated_at = now() where id = $1 and coalesce(install_state, '') <> 'running' returning name",
    )
    .bind(id)
    .bind(h)
    .bind(exp)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(name) = name else {
        return Err(if exists(st, id).await { ApiError::conflict("这个探测点正在经 SSH 安装，等它结束再来") } else { ApiError::not_found("没有这个探测点") });
    };
    audit(&mut tx, actor, "probe_point.install", "probe_point", Some(id), json!({"name": name, "method": "command"})).await?;
    tx.commit().await?;
    st.emit(crate::state::Event::Settings);
    Ok((bootstrap_cmd(&st.cfg.public_url, &t), exp))
}

/// 一次性安装命令取脚本：令牌对、没过期才给，给了就作废；同时发上报令牌（只在这份脚本里）
pub async fn fetch(st: &AppState, ip: Option<std::net::IpAddr>, token: &str) -> anyhow::Result<Option<String>> {
    if !crate::routers::token_shape_ok(token) {
        return Ok(None);
    }
    let report_token = crate::crypto::random_token(32);
    let mut tx = st.db.begin().await?;
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "update probe_point set bootstrap_token_hash = null, bootstrap_expires_at = null, token_hash = $2, upgrade_sha256 = null,
               upgrade_fetched_at = null, upgrade_failed = null, updated_at = now()
         where bootstrap_token_hash = $1 and bootstrap_expires_at > now() returning id, name",
    )
    .bind(crate::crypto::sha256(token.as_bytes()))
    .bind(crate::crypto::sha256(report_token.as_bytes()))
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, name)) = row else { return Ok(None) };
    audit(&mut tx, "probe-bootstrap", "probe_point.bootstrap_used", "probe_point", Some(id), json!({"name": name, "ip": ip.map(|i| i.to_string())})).await?;
    tx.commit().await?;
    st.emit(crate::state::Event::Settings);
    Ok(Some(bootstrap_script(&st.cfg.public_url, &report_token, &name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_command_and_script() {
        let c = bootstrap_cmd("https://fleet.test/", "TOK");
        assert_eq!(c, "{ curl -fsS https://fleet.test/api/v1/probe-bootstrap/TOK 2>/dev/null || wget -qO- https://fleet.test/api/v1/probe-bootstrap/TOK; } | sh");
        let s = bootstrap_script("https://fleet.test/", "t'k", "香港");
        assert!(s.starts_with("#!/bin/sh\n"), "{s}");
        assert!(s.contains("SERVER_URL='https://fleet.test'\nTOKEN='t'\\''k'\nNAME='香港'\nFETCH='1'\n"), "{s}");
        assert!(s.contains("supervisor=supervise-daemon") && s.contains("User=fleet-probe") && s.contains("command_user=fleet-probe"));
    }

    /// 在本机用 sh 跑一遍（路径换到临时目录，id / chown / 服务管理命令换成桩）：OpenRC 与 systemd 各一遍，写出的文件对，配置 0600
    #[cfg(unix)]
    #[test]
    fn install_script_runs_for_openrc_and_systemd() {
        use std::os::unix::fs::PermissionsExt;
        for init in ["openrc", "systemd"] {
            let dir = std::env::temp_dir().join(format!("fleet-probe-{}", Uuid::new_v4().simple()));
            let root = dir.display().to_string();
            let bin = dir.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let stub = |name: &str, body: &str| {
                let p = bin.join(name);
                std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            };
            stub("id", "[ \"$1\" = -u ] && echo 0; exit 0");
            stub("uname", "echo x86_64");
            stub("chown", "exit 0");
            stub("useradd", &format!("echo useradd >> {root}/calls"));
            for c in ["systemctl", "rc-update", "rc-service"] {
                stub(c, &format!("echo \"{c} $*\" >> {root}/calls"));
            }
            if init == "openrc" {
                std::fs::create_dir_all(dir.join("sbin")).unwrap();
                stub("openrc-run", "");
                std::fs::copy(bin.join("openrc-run"), dir.join("sbin/openrc-run")).unwrap();
                std::fs::create_dir_all(dir.join("etc/init.d")).unwrap();
            } else {
                std::fs::create_dir_all(dir.join("run/systemd/system")).unwrap();
                std::fs::create_dir_all(dir.join("etc/systemd/system")).unwrap();
            }
            std::fs::create_dir_all(dir.join("opt/fleet")).unwrap();
            std::fs::write(dir.join("opt/fleet/fleet-agent"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(dir.join("opt/fleet/fleet-agent"), std::fs::Permissions::from_mode(0o755)).unwrap();
            // 上次升级留下的标记：重装时删掉
            for m in [".fleet-agent.trial", ".fleet-agent.bad-sha256"] {
                std::fs::write(dir.join("opt/fleet").join(m), "x").unwrap();
            }
            let s = script("https://fleet.test", "TOKEN123", "香港", false);
            let s = ["/etc/", "/opt/", "/run/", "/sbin/"].iter().fold(s, |s, p| s.replace(p, &format!("{root}{p}")));
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&s)
                .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()))
                .output()
                .unwrap();
            assert!(out.status.success(), "{init}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("fleet_probe=ok init={init}")));
            let read = |p: &str| std::fs::read_to_string(dir.join(p)).unwrap();
            assert_eq!(read("etc/fleet/probe.toml"), "server_url = \"https://fleet.test\"\ntoken = \"TOKEN123\"\nname = \"香港\"\n");
            assert_eq!(std::fs::metadata(dir.join("etc/fleet/probe.toml")).unwrap().permissions().mode() & 0o777, 0o600);
            assert!(!dir.join("opt/fleet/.fleet-agent.trial").exists() && !dir.join("opt/fleet/.fleet-agent.bad-sha256").exists(), "{init}：升级标记没删");
            let calls = read("calls");
            if init == "openrc" {
                let svc = read("etc/init.d/fleet-probe");
                assert!(svc.starts_with(&format!("#!{root}/sbin/openrc-run\n")) && svc.contains(&format!("command_args=\"probe -c {root}/etc/fleet/probe.toml\"")), "{svc}");
                assert!(calls.contains("rc-update add fleet-probe default") && calls.contains("rc-service fleet-probe restart"), "{calls}");
            } else {
                let svc = read("etc/systemd/system/fleet-probe.service");
                assert!(svc.contains(&format!("ExecStart={root}/opt/fleet/fleet-agent probe -c {root}/etc/fleet/probe.toml")), "{svc}");
                assert!(calls.contains("systemctl enable fleet-probe") && calls.contains("systemctl restart fleet-probe"), "{calls}");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
