//! 远程执行与日志：中控用自己的 SSH 密钥和 known_hosts 登录机器跑命令，全程写审计。
//! 另有 `fleetctl ssh`（在中控本机直接 ssh）登录前报的审计。
//!
//! 命令原样当作 bash 脚本执行（经 base64 包一层，再用 `timeout` 兜底并把 stdin 接到 /dev/null，
//! 避免命令读 stdin 时吃掉脚本本身）。

use super::machines::display_name_of;
use super::ok;
use crate::auth::Actor;
use crate::db::audit;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::ssh::{kind_of, ErrKind, Target};
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use uuid::Uuid;

/// 每个输出流最多保留的字节数
const MAX_OUT: usize = 256 * 1024;
/// 命令文本上限
const MAX_CMD: usize = 64 * 1024;
const DEFAULT_TIMEOUT_S: u64 = 60;
/// 同时在跑的 ssh 会话数（中控内存只有几 GB）
static EXEC_SEM: Semaphore = Semaphore::const_new(8);

/// 按字节截断到字符边界；返回是否被截断
fn truncate(mut s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s.truncate(n);
    (s, true)
}

/// 把用户命令包成远端执行的脚本。不用 `exec`：命令不理会 TERM、5 秒后被 KILL 时，timeout 连同自己一起被杀，
/// 外层 bash 还在才能报 137；`exec` 掉外层的话整个会话被信号杀死，ssh 只会报 255，看上去像没连上
fn wrap(command: &str, timeout_s: u64) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(command.as_bytes());
    format!("timeout -k 5 {timeout_s} bash -c \"$(printf %s '{b64}' | base64 -d)\" </dev/null\n")
}

/// 远端 timeout 杀掉了命令：到点 TERM 退出 124；不理会 TERM、再过 5 秒被 KILL 是 137。
/// 137 也可能是命令自己被 KILL（比如内存不足），没到时限的不算超时
fn remote_timed_out(code: i32, duration_ms: u64, timeout_s: u64) -> bool { code == 124 || (code == 137 && duration_ms >= timeout_s * 1000) }

struct Ran {
    exit_code: i32,
    stdout: String,
    stderr: String,
    duration_ms: u64,
    truncated: bool,
    timed_out: bool,
}

/// 在机器上跑一段脚本并写审计。`action` 是审计动作名（exec / machine.logs）。
async fn run_on(st: &AppState, a: &Actor, id: Uuid, command: &str, timeout_s: u64, action: &str) -> ApiResult<Ran> {
    let (name, kind, lifecycle) = display_name_of(&st.db, id).await?;
    if lifecycle == "retired" {
        return Err(ApiError::conflict(format!("{name} 已退役，不能再登录执行命令")));
    }
    if kind == "proxy" {
        return Err(ApiError::conflict(format!("{name} 是家宽代理，没有 SSH，不能执行命令")));
    }
    // 同机 IP 节点没有自己的 SSH：登录它的宿主机
    let (ip, port): (String, i32) = sqlx::query_as(
        "select host(coalesce(h.primary_ip, m.primary_ip)), coalesce(h.ssh_port, m.ssh_port) from machine m left join machine h on h.id = m.host_id where m.id = $1",
    )
    .bind(id)
    .fetch_one(&st.db)
    .await?;
    let target = Target { ip, port: port as u16, key: st.cfg.ssh_key.clone(), known_hosts: st.cfg.known_hosts.clone(), password: None };
    // 排队最多 30 秒：等不到就让调用方稍后再试，不让请求无限挂着
    let _permit = match tokio::time::timeout(Duration::from_secs(30), EXEC_SEM.acquire()).await {
        Ok(p) => p.map_err(|_| ApiError::internal("执行通道已关闭"))?,
        Err(_) => return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "busy", "同时执行的命令太多，稍后再试")),
    };
    // 先提交一条「已开始」的审计再执行：客户端断开或中控重启时，已执行的命令也有记录；执行完再把结果补进这一条
    let audit_id = crate::db::audit_insert(&st.db, &a.name, action, "machine", Some(id), json!({"name": name, "command": command, "started": true})).await?;
    // 命令已经执行过了：补写结果失败只记日志，照样把结果返回（「已开始」那一条审计还在）
    let finish = |extra: Value| async move {
        if let Err(e) = crate::db::audit_finish(&st.db, audit_id, extra).await {
            tracing::error!(audit_id, "补写执行结果到审计失败：{e:#}");
        }
    };
    let t0 = Instant::now();
    // 远端 timeout 负责杀命令；本地多给 15 秒留给建连与收尾。只跑一次：255 也可能是命令已经执行过，不能重试
    let res = target.run_once(&wrap(command, timeout_s), Duration::from_secs(timeout_s + 15)).await;
    let duration_ms = t0.elapsed().as_millis() as u64;
    match res {
        Ok(o) => {
            let timed_out = remote_timed_out(o.code, duration_ms, timeout_s);
            let (stdout, t1) = truncate(o.stdout, MAX_OUT);
            let (stderr, t2) = truncate(o.stderr, MAX_OUT);
            finish(json!({"exit_code": o.code, "duration_ms": duration_ms, "timed_out": timed_out})).await;
            Ok(Ran { exit_code: o.code, stdout, stderr, duration_ms, truncated: t1 || t2, timed_out })
        }
        Err(e) => {
            let msg = format!("{e:#}");
            finish(json!({"error": msg, "duration_ms": duration_ms})).await;
            // 本地兜底的超时（远端 timeout 没能结束会话）：同样按超时返回
            if kind_of(&e) == ErrKind::Timeout {
                Ok(Ran { exit_code: 124, stdout: String::new(), stderr: format!("登录或执行超过 {timeout_s} 秒，已终止"), duration_ms, truncated: false, timed_out: true })
            } else {
                Err(ApiError::new(StatusCode::BAD_GATEWAY, "ssh_failed", format!("无法通过 SSH 执行：{msg}")))
            }
        }
    }
}

fn ran_json(r: &Ran, extra: Value) -> Json<Value> {
    let mut v = json!({"exit_code": r.exit_code, "stdout": r.stdout, "stderr": r.stderr, "duration_ms": r.duration_ms, "truncated": r.truncated, "timed_out": r.timed_out});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    ok(v)
}

#[derive(Deserialize)]
pub struct ExecIn {
    command: String,
    timeout_s: Option<u64>,
}

pub async fn exec(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<ExecIn>) -> ApiResult<Json<Value>> {
    a.require_scope("exec")?;
    if b.command.trim().is_empty() {
        return Err(ApiError::bad_request("command 不能为空"));
    }
    if b.command.len() > MAX_CMD {
        return Err(ApiError::bad_request("命令太长（上限 64 KiB）"));
    }
    let t = b.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S);
    if !(1..=600).contains(&t) {
        return Err(ApiError::bad_request("timeout_s 要在 1 到 600 之间"));
    }
    let r = run_on(&st, &a, id, &b.command, t, "exec").await?;
    Ok(ran_json(&r, json!({})))
}

#[derive(Deserialize)]
pub struct SshLoginIn {
    /// machine | router
    kind: String,
    id: Uuid,
    /// 带了远端命令（命令本身不记）
    #[serde(default)]
    command: bool,
}

/// `fleetctl ssh` 登录前记一条审计 `ssh.login`：它在中控本机用中控的密钥直接 ssh，不经这里执行，只能事先报一声。
/// 记谁（哪把密钥 / 哪个用户）、哪台、带没带命令；命令内容不记（交互式会话本来也记不下，要留命令全文的用 exec）。
/// 对象名按库里的写，不信客户端；已退役、家宽代理同 exec 拒绝
pub async fn audit_ssh(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<SshLoginIn>) -> ApiResult<Json<Value>> {
    a.require_scope("exec")?;
    let name = match b.kind.as_str() {
        "machine" => {
            let (name, kind, lifecycle) = display_name_of(&st.db, b.id).await?;
            if lifecycle == "retired" {
                return Err(ApiError::conflict(format!("{name} 已退役，不能再登录")));
            }
            if kind == "proxy" {
                return Err(ApiError::conflict(format!("{name} 是家宽代理，没有 SSH")));
            }
            name
        }
        "router" => sqlx::query_scalar::<_, String>("select name from router where id = $1")
            .bind(b.id)
            .fetch_optional(&st.db)
            .await?
            .ok_or_else(|| ApiError::not_found("没有这台软路由"))?,
        _ => return Err(ApiError::bad_request("kind 只能是 machine / router")),
    };
    let mut tx = st.db.begin().await?;
    audit(&mut tx, &a.name, "ssh.login", &b.kind, Some(b.id), json!({"name": name, "command": b.command})).await?;
    tx.commit().await?;
    Ok(ok(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct LogsQ {
    unit: Option<String>,
    lines: Option<u32>,
}

fn clamp_lines(l: Option<u32>) -> u32 { l.unwrap_or(200).clamp(1, 2000) }

/// 白名单 unit → systemd 单元名
fn unit_name(u: &str) -> Option<&'static str> {
    match u {
        "fleet-agent" => Some("fleet-agent"),
        "fleet-xray" => Some("fleet-xray"),
        "ssserver" | "fleet-ssserver" => Some("fleet-ssserver"),
        _ => None,
    }
}

pub async fn machine_logs(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<LogsQ>) -> ApiResult<Json<Value>> {
    a.require_scope("exec")?;
    let want = q.unit.as_deref().unwrap_or("fleet-agent");
    let mut unit = unit_name(want).ok_or_else(|| ApiError::bad_request("unit 只能是 fleet-agent / fleet-xray / ssserver"))?.to_string();
    // 同机 IP 节点的 ssserver 是宿主机上的模板实例 fleet-ssserver@<端口>（fleet-agent 就是宿主机的）
    let ip_port: Option<i32> =
        sqlx::query_scalar("select o.port from machine m join node_outbound o on o.machine_id = m.id where m.id = $1 and m.host_id is not null")
            .bind(id)
            .fetch_optional(&st.db)
            .await?;
    if let (Some(p), "fleet-ssserver") = (ip_port, unit.as_str()) {
        unit = format!("fleet-ssserver@{p}");
    }
    let n = clamp_lines(q.lines);
    let cmd = format!("journalctl -u {unit} -n {n} --no-pager -o short-iso");
    let r = run_on(&st, &a, id, &cmd, 30, "machine.logs").await?;
    Ok(ran_json(&r, json!({"unit": unit, "lines": n})))
}

/// 中控自己的日志（本机 journalctl）。fleet 用户通常没权限读 journal，失败时给出明确提示。
pub async fn server_logs(a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<LogsQ>) -> ApiResult<Json<Value>> {
    a.require_scope("exec")?;
    let n = clamp_lines(q.lines);
    let t0 = Instant::now();
    let hint = "中控进程读不到 journal：请在中控机上执行 `usermod -aG systemd-journal fleet` 后重启 fleet-server，或直接在中控机上跑 `journalctl -u fleet-server`";
    let out = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new("journalctl")
            .args(["-u", "fleet-server", "-n", &n.to_string(), "--no-pager", "-o", "short-iso"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| ApiError::internal("journalctl 超过 20 秒没返回"))?
    .map_err(|e| ApiError::internal(format!("无法运行 journalctl：{e}。{hint}")))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() || stdout.trim().is_empty() || stdout.trim_start().starts_with("-- No entries") {
        // 4xx 才会把原因带给客户端（5xx 一律隐藏细节），这是可操作的配置问题，要让调用方看到
        return Err(ApiError::new(axum::http::StatusCode::FAILED_DEPENDENCY, "journal_denied", format!("读取中控日志失败：{}。{hint}", stderr.trim())));
    }
    let (stdout, truncated) = truncate(stdout, MAX_OUT);
    let mut tx = st.db.begin().await?;
    audit(&mut tx, &a.name, "server.logs", "server", None, json!({"lines": n})).await?;
    tx.commit().await?;
    Ok(ok(json!({"exit_code": 0, "stdout": stdout, "stderr": stderr, "duration_ms": t0.elapsed().as_millis() as u64, "truncated": truncated, "timed_out": false, "unit": "fleet-server", "lines": n})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_on_char_boundary() {
        let (s, t) = truncate("你好世界".to_string(), 7);
        assert!(t);
        assert_eq!(s, "你好");
        let (s, t) = truncate("abc".to_string(), 10);
        assert!(!t);
        assert_eq!(s, "abc");
    }

    #[test]
    fn wrap_has_no_raw_command() {
        let w = wrap("echo 'hi'; cat </dev/stdin", 30);
        assert!(w.starts_with("timeout -k 5 30 bash -c"));
        assert!(!w.contains("echo 'hi'"));
    }

    #[test]
    fn timeout_exit_codes() {
        assert!(remote_timed_out(124, 1_000, 30));
        assert!(remote_timed_out(137, 35_000, 30), "不理会 TERM 被强杀");
        assert!(!remote_timed_out(137, 2_000, 30), "没到时限就被 KILL 的不是超时");
        assert!(!remote_timed_out(255, 35_000, 30));
    }

    #[test]
    fn unit_whitelist() {
        assert_eq!(unit_name("ssserver"), Some("fleet-ssserver"));
        assert_eq!(unit_name("sshd"), None);
        assert_eq!(unit_name("fleet-agent; reboot"), None);
    }
}
