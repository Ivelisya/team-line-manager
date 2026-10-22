//! 一键部署：中控能直接连到路由的 SSH 时（公网映射、同一网段……），填上路由的地址、账号与密码，中控登录上去把接入做完。
//! 路由在 NAT 后面、中控连不到它时，用「在路由上执行接入命令」（接入脚本相同，见 scripts.rs）。
//!
//! 接口立即返回，部署在后台跑（一台一个任务，不进作业引擎：作业的目标绑在机器表上），进度与结果写在 router 的 deploy_* 列，
//! 每一步发实时事件 routers：登录路由 → 上传密钥 → 启动隧道 → 等待隧道连上。
//! - 密码只在这个任务的内存里，经环境变量 SSHPASS 交给 sshpass，不上命令行、不落库、不进日志与审计；不填密码就用中控现有的密钥登录。
//! - 主机公钥：先 keyscan 记下再登录（之后严格核对）；这台路由已经记过的，不一样就停下，不把密码交出去。登录成功后记到 router.host_key，
//!   之后经隧道登录也按它核对，不再首次信任。
//! - 审计记谁、哪台、目标地址、用没用密码、结果，不记密码（同远程执行：先写一条「已开始」，结束时补上结果）。
//! - 中控重启时还没跑完的，标为失败（[`mark_interrupted`]）。

use super::scripts::Bootstrap;
use super::tunnel::{self, Login};
use crate::db::new_id;
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Event};
use futures::FutureExt;
use serde_json::json;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

/// 部署完等隧道连上最多这么久
const TUNNEL_WAIT: Duration = Duration::from_secs(60);

/// 校验一键部署的目标：地址只许主机名 / IP，用户名同 Linux 账号名；空密码当没给（用中控密钥）
pub fn login(host: &str, port: Option<u16>, user: Option<&str>, password: Option<String>) -> Result<Login, String> {
    let host = host.trim();
    if host.is_empty() || host.len() > 253 || host.starts_with('-') || !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':')) {
        return Err("ssh.host 要填路由的 IP 或主机名".into());
    }
    let port = port.unwrap_or(22);
    if port == 0 {
        return Err("ssh.port 不对".into());
    }
    let user = user.map(str::trim).filter(|u| !u.is_empty()).unwrap_or("root");
    let ok_user = user.len() <= 32
        && user.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && user.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'));
    if !ok_user {
        return Err("ssh.user 不对（一般是 root）".into());
    }
    let password = password.filter(|p| !p.is_empty());
    if password.as_ref().is_some_and(|p| p.len() > 1024 || p.contains(['\n', '\r', '\0'])) {
        return Err("ssh.password 不对".into());
    }
    Ok(Login { host: host.to_string(), port, user: user.to_string(), password })
}

/// 开始部署（接口调）：已有部署在跑 409；清掉还没用的接入令牌（免得它之后被取走、换掉这次装上的隧道密钥）；写「已开始」的审计；后台去跑
pub async fn start(st: &AppState, id: Uuid, l: Login, actor: &str) -> ApiResult<()> {
    let mut tx = st.db.begin().await?;
    let cur: Option<(String, Option<String>)> =
        sqlx::query_as("select name, deploy_state from router where id = $1 for update").bind(id).fetch_optional(&mut *tx).await?;
    let name = match cur {
        None => return Err(ApiError::not_found("没有这台软路由")),
        Some((_, Some(s))) if s == "running" => return Err(ApiError::conflict("这台路由正在部署，等它结束再来（进度见 deploy）")),
        Some((n, _)) => n,
    };
    sqlx::query(
        "update router set deploy_state = 'running', deploy_step = '登录路由', deploy_error = null, deploy_host = $2, deploy_port = $3, deploy_user = $4,
            deploy_started_at = now(), deploy_finished_at = null, bootstrap_token_hash = null, bootstrap_expires_at = null, updated_at = now()
         where id = $1",
    )
    .bind(id)
    .bind(&l.host)
    .bind(l.port as i32)
    .bind(&l.user)
    .execute(&mut *tx)
    .await?;
    let audit = crate::db::audit_insert(
        &mut *tx,
        actor,
        "router.deploy",
        "router",
        Some(id),
        json!({"name": name, "host": l.host, "port": l.port, "user": l.user, "password": l.password.is_some(), "started": true}),
    )
    .await?;
    tx.commit().await?;
    st.emit(Event::Routers);
    let st = st.clone();
    tokio::spawn(async move {
        let res = AssertUnwindSafe(run(&st, id, &l)).catch_unwind().await;
        let err = match res {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some("部署过程中中控出了意外（内部错误），请重试".to_string()),
        };
        if let Err(e) = finish(&st, id, audit, err.as_deref()).await {
            tracing::error!(router = %id, "写部署结果失败：{e:#}");
        }
        st.emit(Event::Routers);
    });
    Ok(())
}

async fn finish(st: &AppState, id: Uuid, audit: i64, err: Option<&str>) -> sqlx::Result<()> {
    sqlx::query(
        "update router set deploy_state = case when $2::text is null then 'done' else 'failed' end, deploy_step = null, deploy_error = $2,
            deploy_finished_at = now(), updated_at = now()
         where id = $1 and deploy_state = 'running'",
    )
    .bind(id)
    .bind(err)
    .execute(&st.db)
    .await?;
    let result = match err {
        None => json!({"result": "ok"}),
        Some(e) => json!({"result": "failed", "error": e}),
    };
    crate::db::audit_finish(&st.db, audit, result).await
}

/// 中控重启时还在「部署中」的：任务已经没了，标为失败（每个进程启动时一次）
pub async fn mark_interrupted(st: &AppState) -> sqlx::Result<()> {
    let n = sqlx::query(
        "update router set deploy_state = 'failed', deploy_step = null, deploy_error = '中控重启，部署中断：请重新部署', deploy_finished_at = now()
         where deploy_state = 'running'",
    )
    .execute(&st.db)
    .await?
    .rows_affected();
    if n > 0 {
        tracing::warn!("{n} 台软路由的一键部署因中控重启中断，已标为失败");
    }
    Ok(())
}

async fn step(st: &AppState, id: Uuid, s: &str) -> Result<(), String> {
    sqlx::query("update router set deploy_step = $2 where id = $1 and deploy_state = 'running'")
        .bind(id)
        .bind(s)
        .execute(&st.db)
        .await
        .map_err(|e| format!("写部署进度失败：{e}"))?;
    st.emit(Event::Routers);
    Ok(())
}

/// 远端脚本输出里 `fleet：` 开头的说明（缺 OpenSSH 客户端之类），没有就用 stderr 的最后几行
fn remote_reason(o: &crate::ssh::Output) -> String {
    let own: Vec<&str> = o.stderr.lines().filter(|l| l.starts_with("fleet：")).collect();
    if !own.is_empty() {
        return own.join("；").trim_start_matches("fleet：").to_string();
    }
    let tail: Vec<&str> = o.stderr.lines().rev().filter(|l| !l.trim().is_empty()).take(3).collect();
    format!("退出码 {}：{}", o.code, tail.into_iter().rev().collect::<Vec<_>>().join(" / "))
}

/// 在路由上跑一段脚本，要求输出里有 `marker`
async fn remote(st: &AppState, id: Uuid, l: &Login, kh: &PathBuf, script: &str, marker: &str, what: &str) -> Result<(), String> {
    let o = crate::ssh::exec(tunnel::direct(&st.cfg, id, l, kh), script, Duration::from_secs(60)).await.map_err(|e| format!("{what}失败：{e:#}"))?;
    let o = tunnel::normalize(o, l.password.is_some());
    if o.code == 255 {
        return Err(format!("{what}失败：{}", tunnel::explain(&o, false)));
    }
    if !o.stdout.contains(marker) {
        return Err(format!("{what}失败：{}", remote_reason(&o)));
    }
    Ok(())
}

async fn run(st: &AppState, id: Uuid, l: &Login) -> Result<(), String> {
    let pinned: Option<String> = sqlx::query_scalar::<_, Option<String>>("select host_key from router where id = $1")
        .bind(id)
        .fetch_optional(&st.db)
        .await
        .map_err(|e| format!("读路由失败：{e}"))?
        .ok_or("路由已经删掉了")?;
    let target = format!("{}:{}", l.host, l.port);
    // 登录路由：主机公钥先核对 / 记下，再登录
    if !crate::ssh::tcp_ok(&l.host, l.port, Duration::from_secs(5)).await {
        return Err(format!("连不上 {target}：路由的 SSH 端口没开、被防火墙挡住，或地址、端口不对（中控连不到路由时改用「在路由上执行接入命令」）"));
    }
    let key = tunnel::keyscan(&l.host, l.port)
        .await
        .map_err(|e| format!("{e:#}"))?
        .ok_or_else(|| format!("从 {target} 拿不到 ed25519 主机公钥：那里不是 SSH，或路由的 dropbear 没有 ed25519 主机密钥"))?;
    if let Some(old) = pinned.as_deref().filter(|o| *o != key) {
        return Err(format!(
            "{target} 的主机公钥（{}）和这台路由记录的（{}）不一样：可能连错了机器，没有登录。确认路由重装过的话，先「重新生成接入命令」清掉旧记录再部署",
            crate::ssh::fingerprint(&key),
            crate::ssh::fingerprint(old)
        ));
    }
    tokio::fs::create_dir_all(&st.cfg.routers.control_dir).await.map_err(|e| format!("建 {} 失败：{e}", st.cfg.routers.control_dir.display()))?;
    let kh = st.cfg.routers.control_dir.join(format!("deploy-{id}-{}.known_hosts", new_id().simple()));
    tokio::fs::write(&kh, format!("router-{id} {key}\n")).await.map_err(|e| format!("写临时 known_hosts 失败：{e}"))?;
    let res = run_steps(st, id, l, &kh, &key).await;
    let _ = tokio::fs::remove_file(&kh).await;
    res
}

async fn run_steps(st: &AppState, id: Uuid, l: &Login, kh: &PathBuf, key: &str) -> Result<(), String> {
    remote(st, id, l, kh, "echo fleet_login=ok\n", "fleet_login=ok", "登录路由").await?;
    // 上传密钥：先把新的隧道公钥、路由的主机公钥记进库（旧隧道密钥随之撤销，发过密钥的换个隧道端口），再把私钥送过去
    step(st, id, "上传密钥").await?;
    let central = super::central_keys(&st.cfg).await.map_err(|e| format!("{e:#}"))?;
    let ssh_host = st.cfg.routers.ssh_host_of(&st.cfg.public_url).map_err(|e| format!("{e:#}"))?;
    let tk = super::tunnel_keypair(&format!("fleet-tunnel router-{id}"));
    let db = |e: sqlx::Error| format!("记录隧道公钥失败：{e}");
    let mut tx = st.db.begin().await.map_err(db)?;
    let port = super::rotate_port(&mut tx, &st.cfg, id).await.map_err(|e| e.message)?;
    sqlx::query(
        "update router set tunnel_pubkey = $2, tunnel_key_at = now(), host_key = $3, bootstrap_token_hash = null, bootstrap_expires_at = null,
            updated_at = now()
         where id = $1",
    )
    .bind(id)
    .bind(&tk.public)
    .bind(key)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    // 旧隧道作废了：新隧道连上、采集到之前算「等待接入」
    sqlx::query("update router_status set reachable = false, error = null where router_id = $1").bind(id).execute(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    super::sync_files(st).await.map_err(|e| format!("写隧道公钥清单失败：{e:#}"))?;
    tunnel::close_master(&st.cfg, id).await;
    let b = Bootstrap {
        router_id: id,
        tunnel_port: port,
        ssh_host: &ssh_host,
        ssh_port: st.cfg.routers.ssh_port,
        tunnel_key: &tk.private_pem,
        central_hostkey: &central.host_key,
        central_pubkey: &central.login_pub,
    };
    remote(st, id, l, kh, &b.keys(), "fleet_keys=ok", "上传密钥").await?;
    step(st, id, "启动隧道").await?;
    remote(st, id, l, kh, &b.service(), "fleet_service=ok", "启动隧道").await?;
    step(st, id, "等待隧道连上").await?;
    let t0 = tokio::time::Instant::now();
    let mut last = String::new();
    while t0.elapsed() < TUNNEL_WAIT {
        tokio::time::sleep(Duration::from_secs(3)).await;
        match crate::ssh::exec(tunnel::via_tunnel(&st.cfg, id, port), "echo fleet_tunnel=ok\n", Duration::from_secs(15)).await {
            Ok(o) if o.stdout.contains("fleet_tunnel=ok") => return Ok(()),
            Ok(o) => last = tunnel::explain(&o, true),
            Err(e) => last = format!("{e:#}"),
        }
    }
    // 没连上：读路由上 fleet-tunnel（ssh）的日志，把原因带出来
    let logs = match crate::ssh::exec(tunnel::direct(&st.cfg, id, l, kh), "logread 2>/dev/null | grep -E 'fleet-tunnel|ssh\\[' | tail -n 5\n", Duration::from_secs(20)).await {
        Ok(o) => o.stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" / "),
        Err(_) => String::new(),
    };
    Err(format!(
        "隧道 {} 秒内没有连上（{last}）：路由连不到中控 {ssh_host}:{}，或中控的 sshd 还没装好 fleet-tunnel 账号（见 OPS.md「软路由接入与隧道」）{}",
        TUNNEL_WAIT.as_secs(),
        st.cfg.routers.ssh_port,
        if logs.is_empty() { String::new() } else { format!("。路由上的日志：{logs}") }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_validation() {
        let l = login(" 192.168.31.70 ", None, None, Some(String::new())).unwrap();
        assert_eq!((l.host.as_str(), l.port, l.user.as_str(), l.password.is_none()), ("192.168.31.70", 22, "root", true), "空密码当没给");
        assert!(login("router.lan", Some(2222), Some("admin"), Some("pw".into())).is_ok());
        assert!(login("2001:db8::1", None, None, None).is_ok());
        for bad in ["", "-oProxyCommand=x", "a b", "a;b", "a@b"] {
            assert!(login(bad, None, None, None).is_err(), "{bad}");
        }
        assert!(login("h", Some(0), None, None).is_err());
        assert!(login("h", None, Some("Root"), None).is_err() && login("h", None, Some("-x"), None).is_err());
        assert!(login("h", None, None, Some("a\nb".into())).is_err());
    }

    #[test]
    fn remote_reason_prefers_own_messages() {
        let o = |stderr: &str| crate::ssh::Output { code: 2, stdout: String::new(), stderr: stderr.into() };
        assert_eq!(remote_reason(&o("x\nfleet：路由上没有 OpenSSH 客户端\n")), "路由上没有 OpenSSH 客户端");
        assert_eq!(remote_reason(&o("a\nb\nc\nd\n")), "退出码 2：b / c / d");
    }
}
