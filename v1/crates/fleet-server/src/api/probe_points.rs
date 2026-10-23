//! 探测点的接口：列表、新增（经 SSH 安装或一次性安装命令）、改（名称、备注、启停）、删除、重装、升级，主探测来源的设置，
//! 以及给探测点取安装脚本的公开接口（不要登录，一次性令牌）。逻辑见 probe_points/。

use super::ok;
use crate::auth::{Actor, ClientIp};
use crate::db::{audit, new_id};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::probe_points::{self, install, Setting};
use crate::reach::{self, Window};
use crate::state::AppState;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

const MAX_NAME: usize = 40;
const MAX_NOTE: usize = 2000;

/// 名称：1–40 个字，不含引号、反斜杠、控制字符（写进探测点的 probe.toml、告警摘要的 SQL 字面量），不能叫 central / 中控（fleetctl probe primary 用它指中控）
fn check_name(n: &str) -> ApiResult<String> {
    let n = n.trim();
    if n.is_empty() || n.chars().count() > MAX_NAME {
        return Err(ApiError::bad_request(format!("名称不能为空，最长 {MAX_NAME} 个字")));
    }
    if n.chars().any(|c| c.is_control() || matches!(c, '"' | '\'' | '\\' | '`' | '$')) {
        return Err(ApiError::bad_request("名称里不能有引号、反斜杠、$、` 与控制字符"));
    }
    if n.eq_ignore_ascii_case("central") || n == "中控" {
        return Err(ApiError::bad_request("central / 中控 是保留名（指中控自己的检查）"));
    }
    Ok(n.to_string())
}

fn check_note(n: &str) -> ApiResult<()> {
    if n.chars().count() > MAX_NOTE {
        return Err(ApiError::bad_request(format!("备注最长 {MAX_NOTE} 个字")));
    }
    Ok(())
}

async fn name_free(st: &AppState, name: &str, except: Option<Uuid>) -> ApiResult<()> {
    let n: Option<Uuid> = sqlx::query_scalar("select id from probe_point where lower(name) = lower($1)").bind(name).fetch_optional(&st.db).await?;
    match n {
        Some(id) if Some(id) != except => Err(ApiError::conflict("已经有同名的探测点")),
        _ => Ok(()),
    }
}

/// 经 SSH 安装的目标：地址只许 IP / 主机名
#[derive(Deserialize)]
pub struct SshIn {
    host: String,
    port: Option<u16>,
    #[serde(default)]
    reset_host_key: bool,
}

impl SshIn {
    fn checked(&self) -> ApiResult<(String, u16)> {
        let h = self.host.trim();
        if h.is_empty() || h.len() > 253 || h.starts_with('-') || !h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':')) {
            return Err(ApiError::bad_request("ssh.host 要填探测点的 IP 或主机名"));
        }
        let port = self.port.unwrap_or(22);
        if port == 0 {
            return Err(ApiError::bad_request("ssh.port 不对"));
        }
        Ok((h.to_string(), port))
    }
}

#[derive(Deserialize)]
pub struct ListQ {
    /// 到各线路机的统计看多久：1h（默认）/ 24h / 7d / 30d
    window: Option<String>,
}

fn window(s: Option<&str>) -> ApiResult<Window> { Window::parse(s).ok_or_else(|| ApiError::bad_request("window 只能是 1h、24h、7d、30d")) }

pub async fn list(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<ListQ>) -> ApiResult<Json<Value>> {
    Ok(ok(probe_points::list(&st, window(q.window.as_deref())?).await?))
}

async fn one(st: &AppState, id: Uuid) -> ApiResult<Value> {
    probe_points::list(st, Window::H1).await?.into_iter().find(|p| p["id"] == json!(id)).ok_or_else(|| ApiError::not_found("没有这个探测点"))
}

/// 线路机抽屉「连接质量」：各路来源（中控、各探测点）近 24 小时 / 7 天 / 30 天连这台线路机客户入口的成功率与延迟（小时汇总，reach::quality）。
/// `{sources: [{source: "central" | 探测点 id, label, primary, windows: {"24h": {total, ok, avg_ms, median_ms} | null, "7d": …, "30d": …}}]}`：
/// 中控在前、探测点按名字；30 天里一次都没测过的来源不出现
pub async fn relay_quality(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let kind: Option<String> = sqlx::query_scalar("select kind::text from machine where id = $1").bind(id).fetch_optional(&st.db).await?;
    match kind.as_deref() {
        None => return Err(ApiError::not_found("没有这台机器")),
        Some("relay") => {}
        Some(_) => return Err(ApiError::bad_request("只有线路机有客户入口连通性")),
    }
    let set = probe_points::setting(&st.db).await?;
    let names: Vec<(Uuid, String)> = sqlx::query_as("select id, name from probe_point order by name").fetch_all(&st.db).await?;
    let wins = [Window::H24, Window::D7, Window::D30];
    let mut stats: std::collections::HashMap<Option<Uuid>, serde_json::Map<String, Value>> = Default::default();
    for w in wins {
        for q in reach::quality(&st.db, w, Some(id)).await? {
            stats.entry(q.probe_point_id).or_default().insert(w.as_str().into(), json!({"total": q.total, "ok": q.ok, "avg_ms": q.avg_ms, "median_ms": q.median_ms}));
        }
    }
    let srcs = std::iter::once((None, "中控".to_string())).chain(names.into_iter().map(|(p, n)| (Some(p), probe_points::label_of(&n))));
    let sources: Vec<Value> = srcs
        .filter_map(|(p, label)| {
            let mut ws = stats.remove(&p)?;
            let windows: serde_json::Map<String, Value> = wins.iter().map(|w| (w.as_str().to_string(), ws.remove(w.as_str()).unwrap_or(Value::Null))).collect();
            Some(json!({"source": p.map_or_else(|| json!("central"), |id| json!(id)), "label": label, "primary": set.primary == p, "windows": windows}))
        })
        .collect();
    Ok(ok(json!({"sources": sources})))
}

pub async fn get_one(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> { Ok(ok(one(&st, id).await?)) }

#[derive(Deserialize)]
pub struct NewProbe {
    name: String,
    #[serde(default)]
    note: String,
    /// 给了就由中控经 SSH 安装，不发一次性安装命令
    ssh: Option<SshIn>,
}

fn bootstrap_out(cmd: &str, exp: OffsetDateTime) -> Value {
    json!({"cmd": cmd, "expires_at": exp.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()})
}

pub async fn create(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<NewProbe>) -> ApiResult<Json<Value>> {
    let name = check_name(&b.name)?;
    check_note(&b.note)?;
    let ssh = b.ssh.as_ref().map(SshIn::checked).transpose()?;
    name_free(&st, &name, None).await?;
    let id = new_id();
    let mut tx = st.db.begin().await?;
    sqlx::query("insert into probe_point (id, name, note) values ($1, $2, $3)").bind(id).bind(&name).bind(b.note.trim()).execute(&mut *tx).await?;
    audit(&mut tx, &a.name, "probe_point.create", "probe_point", Some(id), json!({"name": name, "method": if ssh.is_some() { "ssh" } else { "command" }})).await?;
    tx.commit().await?;
    match ssh {
        Some((host, port)) => {
            install::start_ssh(&st, id, host, port, b.ssh.as_ref().is_some_and(|s| s.reset_host_key), &a.name).await?;
            Ok(ok(json!({"id": id})))
        }
        None => {
            let (cmd, exp) = install::issue_bootstrap(&st, id, &a.name).await?;
            Ok(ok(json!({"id": id, "bootstrap": bootstrap_out(&cmd, exp)})))
        }
    }
}

#[derive(Deserialize)]
pub struct InstallIn {
    ssh: Option<SshIn>,
}

/// 重装：给 ssh 就由中控经 SSH 装（换新令牌），否则发一条新的一次性安装命令（执行后换新令牌）
pub async fn reinstall(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<InstallIn>) -> ApiResult<Json<Value>> {
    match b.ssh {
        Some(s) => {
            let (host, port) = s.checked()?;
            install::start_ssh(&st, id, host, port, s.reset_host_key, &a.name).await?;
            Ok(ok(json!({"id": id})))
        }
        None => {
            let (cmd, exp) = install::issue_bootstrap(&st, id, &a.name).await?;
            Ok(ok(json!({"id": id, "bootstrap": bootstrap_out(&cmd, exp)})))
        }
    }
}

#[derive(Deserialize)]
pub struct PatchProbe {
    name: Option<String>,
    note: Option<String>,
    enabled: Option<bool>,
}

pub async fn patch(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<PatchProbe>) -> ApiResult<Json<Value>> {
    let name = b.name.as_deref().map(check_name).transpose()?;
    if let Some(n) = &b.note {
        check_note(n)?;
    }
    if let Some(n) = &name {
        name_free(&st, n, Some(id)).await?;
    }
    let mut tx = st.db.begin().await?;
    let cur: Option<(String, String, bool)> =
        sqlx::query_as("select name, note, enabled from probe_point where id = $1 for update").bind(id).fetch_optional(&mut *tx).await?;
    let Some((old_name, old_note, old_enabled)) = cur else { return Err(ApiError::not_found("没有这个探测点")) };
    if b.enabled == Some(false) && old_enabled && probe_points::setting(&st.db).await?.primary == Some(id) {
        return Err(ApiError::conflict(format!("「{old_name}」是主探测来源：先把主来源改成中控或别的探测点再停用")));
    }
    let new = (name.clone().unwrap_or(old_name.clone()), b.note.as_deref().map(str::trim).unwrap_or(&old_note).to_string(), b.enabled.unwrap_or(old_enabled));
    if new == (old_name.clone(), old_note.clone(), old_enabled) {
        return Ok(ok(one(&st, id).await?));
    }
    sqlx::query("update probe_point set name = $2, note = $3, enabled = $4, updated_at = now() where id = $1")
        .bind(id)
        .bind(&new.0)
        .bind(&new.1)
        .bind(new.2)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &a.name,
        "probe_point.update",
        "probe_point",
        Some(id),
        json!({"name": new.0, "before": {"name": old_name, "note": old_note, "enabled": old_enabled}, "after": {"name": new.0, "note": new.1, "enabled": new.2}}),
    )
    .await?;
    tx.commit().await?;
    probe_points::changed(&st);
    Ok(ok(one(&st, id).await?))
}

pub async fn delete(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let cur: Option<(String, Option<String>, Option<i32>)> =
        sqlx::query_as("select name, install_host, install_port from probe_point where id = $1 for update").bind(id).fetch_optional(&mut *tx).await?;
    let Some((name, host, port)) = cur else { return Err(ApiError::not_found("没有这个探测点")) };
    if probe_points::setting(&st.db).await?.primary == Some(id) {
        return Err(ApiError::conflict(format!("「{name}」是主探测来源：先把主来源改成中控或别的探测点再删除")));
    }
    // 它的检查记录一起删（relay_reach on delete cascade）；令牌随行删除作废
    sqlx::query("delete from probe_point where id = $1").bind(id).execute(&mut *tx).await?;
    audit(&mut tx, &a.name, "probe_point.delete", "probe_point", Some(id), json!({"name": name, "host": host})).await?;
    tx.commit().await?;
    // 经 SSH 装过的：中控 known_hosts 里的记录删掉（同一个地址再装按新机器首次信任）
    if let (Some(h), Some(p)) = (host, port) {
        if let Err(e) = crate::ssh::forget_host(&st.cfg.known_hosts, &h, p as u16).await {
            tracing::warn!("从 known_hosts 删除探测点 {h}:{p} 失败：{e:#}");
        }
    }
    probe_points::changed(&st);
    Ok(ok(json!({"ok": true, "cleanup": install::CLEANUP})))
}

/// 升级：记下 dist 里程序的 sha256，探测点下次取目标（30 秒内）时自己下载、核对、替换、重启。返回要升到的版本
pub async fn upgrade(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let path = st.cfg.dist_dir.join("fleet-agent");
    let sha = crate::jobs::dist_agent_sha256(&st).await.map_err(|e| ApiError::conflict(format!("中控上没有可推送的 fleet-agent（{}：{e}）", path.display())))?;
    let version = probe_points::agent::dist_version(&st).await.ok_or_else(|| ApiError::conflict(format!("中控上的 fleet-agent（{}）读不出版本号", path.display())))?;
    let mut tx = st.db.begin().await?;
    let row: Option<(String, Option<String>, bool)> =
        sqlx::query_as("update probe_point set upgrade_sha256 = $2, updated_at = now() where id = $1 returning name, version, token_hash is not null")
            .bind(id)
            .bind(&sha)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((name, from, installed)) = row else { return Err(ApiError::not_found("没有这个探测点")) };
    if !installed {
        return Err(ApiError::conflict(format!("「{name}」还没装好，先安装")));
    }
    audit(&mut tx, &a.name, "probe_point.upgrade", "probe_point", Some(id), json!({"name": name, "from": from, "to": version, "sha256": sha})).await?;
    tx.commit().await?;
    st.emit(crate::state::Event::Settings);
    Ok(ok(json!({"id": id, "name": name, "from": from, "version": version, "sha256": sha})))
}

// ── 主探测来源 ───────────────────────────────────────────────────────────

/// `{primary: "central" | 探测点 id, primary_name, central_enabled, effective: {state, label, probe_point_id, primary_label}}`
async fn source_view(st: &AppState) -> ApiResult<Value> {
    let s = probe_points::setting(&st.db).await?;
    let eff = probe_points::effective(&st).await?;
    let name: Option<String> = match s.primary {
        Some(id) => sqlx::query_scalar("select name from probe_point where id = $1").bind(id).fetch_optional(&st.db).await?,
        None => None,
    };
    Ok(json!({
        "primary": s.primary.map_or_else(|| json!("central"), |id| json!(id)),
        "primary_name": name.unwrap_or_else(|| "中控".into()),
        "central_enabled": s.central,
        "effective": eff,
        "offline_after_s": probe_points::OFFLINE_S,
    }))
}

pub async fn get_source(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> { Ok(ok(source_view(&st).await?)) }

/// 两项都可选，没给的保持原样；primary 写 "central" 指中控
#[derive(Deserialize)]
pub struct SourceIn {
    primary: Option<String>,
    central_enabled: Option<bool>,
}

pub async fn put_source(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<SourceIn>) -> ApiResult<Json<Value>> {
    let cur = probe_points::setting(&st.db).await?;
    let primary = match b.primary.as_deref().map(str::trim) {
        None => cur.primary,
        Some("central") => None,
        Some(s) => Some(Uuid::try_parse(s).map_err(|_| ApiError::bad_request("primary 要写 central 或探测点 id"))?),
    };
    probe_points::save_setting(&st, &a.name, Setting { primary, central: b.central_enabled.unwrap_or(cur.central) }).await?;
    Ok(ok(source_view(&st).await?))
}

// ── 取安装脚本（公开，不要登录）────────────────────────────────────────

fn script_response(status: StatusCode, body: String) -> Response { (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response() }

fn script_error(status: StatusCode, msg: &str) -> Response { script_response(status, format!("echo 'fleet：{msg}' >&2\nexit 1\n")) }

/// `GET /api/v1/probe-bootstrap/{令牌}`：令牌对、没过期才给安装脚本，给了就作废（只能用一次）；同时发上报令牌（只在这份脚本里）
pub async fn bootstrap_script(State(st): State<AppState>, ClientIp(ip): ClientIp, ApiPath(token): ApiPath<String>) -> Response {
    if !super::routers::rate_ok(ip) {
        return script_error(StatusCode::TOO_MANY_REQUESTS, "请求太频繁，10 分钟后再试");
    }
    match install::fetch(&st, ip, &token).await {
        Ok(Some(s)) => script_response(StatusCode::OK, s),
        Ok(None) => script_error(StatusCode::NOT_FOUND, "安装链接不对、已过期或已经用过：fleetctl probe install <名称> 重新生成"),
        Err(e) => {
            let eid = &Uuid::new_v4().simple().to_string()[..8];
            tracing::error!(error_id = eid, "生成探测点安装脚本失败：{e:#}");
            script_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("中控生成安装脚本失败（编号 {eid}），请把编号告诉管理员查看日志"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(check_name(" 香港 ").unwrap(), "香港");
        for bad in ["", "a\"b", "a'b", "a\\b", "a$b", "central", "中控", &"长".repeat(41)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ssh_target() {
        let s = |h: &str, p| SshIn { host: h.into(), port: p, reset_host_key: false }.checked();
        assert_eq!(s("47.0.0.1", None).unwrap(), ("47.0.0.1".to_string(), 22));
        assert!(s("-oProxyCommand=x", None).is_err() && s("a b", None).is_err() && s("h", Some(0)).is_err());
    }
}
