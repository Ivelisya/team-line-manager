//! 远程执行与日志：中控用自己的 SSH 密钥和 known_hosts 登录机器跑命令，全程写审计。
//!
//! 命令原样当作 bash 脚本执行（经 base64 包一层，再用 `timeout` 兜底并把 stdin 接到 /dev/null，
//! 避免命令读 stdin 时吃掉脚本本身）。

use super::machines::display_name_of;
use super::ok;
use crate::auth::Actor;
use crate::db::audit;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::ssh::Target;
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

/// 把用户命令包成远端执行的脚本
fn wrap(command: &str, timeout_s: u64) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(command.as_bytes());
    format!("exec timeout -k 5 {timeout_s} bash -c \"$(printf %s '{b64}' | base64 -d)\" </dev/null\n")
}

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
    let (ip, port): (String, i32) = sqlx::query_as("select host(primary_ip), ssh_port from machine where id = $1")
        .bind(id)
        .fetch_one(&st.db)
        .await?;
    let target = Target { ip, port: port as u16, key: st.cfg.ssh_key.clone(), known_hosts: st.cfg.known_hosts.clone(), password: None };
    let _permit = EXEC_SEM.acquire().await.map_err(|_| ApiError::internal("执行通道已关闭"))?;
    // 先提交一条「已开始」的审计再执行：客户端断开或中控重启时，已执行的命令也有记录；执行完再把结果补进这一条
    let audit_id: i64 = sqlx::query_scalar(
        "insert into audit_log (actor, action, target_kind, target_id, detail) values ($1, $2, 'machine', $3, $4) returning id",
    )
    .bind(&a.name)
    .bind(action)
    .bind(id)
    .bind(json!({"name": name, "command": command, "started": true}))
    .fetch_one(&st.db)
    .await?;
    let finish = |extra: Value| {
        sqlx::query("update audit_log set detail = (detail - 'started'::text) || $2 where id = $1").bind(audit_id).bind(extra).execute(&st.db)
    };
    let t0 = Instant::now();
    // 远端 timeout 负责杀命令；本地多给 15 秒留给建连与收尾
    let res = target.run(&wrap(command, timeout_s), Duration::from_secs(timeout_s + 15)).await;
    let duration_ms = t0.elapsed().as_millis() as u64;
    match res {
        Ok(o) => {
            let timed_out = o.code == 124;
            let (stdout, t1) = truncate(o.stdout, MAX_OUT);
            let (stderr, t2) = truncate(o.stderr, MAX_OUT);
            finish(json!({"exit_code": o.code, "duration_ms": duration_ms, "timed_out": timed_out})).await?;
            Ok(Ran { exit_code: o.code, stdout, stderr, duration_ms, truncated: t1 || t2, timed_out })
        }
        Err(e) => {
            let msg = format!("{e:#}");
            finish(json!({"error": msg, "duration_ms": duration_ms})).await?;
            if msg.contains("超时") {
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
    let unit = unit_name(want).ok_or_else(|| ApiError::bad_request("unit 只能是 fleet-agent / fleet-xray / ssserver"))?;
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
        assert!(w.starts_with("exec timeout -k 5 30 bash -c"));
        assert!(!w.contains("echo 'hi'"));
    }

    #[test]
    fn unit_whitelist() {
        assert_eq!(unit_name("ssserver"), Some("fleet-ssserver"));
        assert_eq!(unit_name("sshd"), None);
        assert_eq!(unit_name("fleet-agent; reboot"), None);
    }
}
