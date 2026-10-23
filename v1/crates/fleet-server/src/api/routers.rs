//! 软路由的接口：列表、详情、指标曲线、新建 / 改 / 删、重新生成接入命令、一键部署，以及给路由取接入脚本的公开接口（不要登录，一次性令牌）。
//! 连接、隧道、采集、告警见 routers/。

use super::ok;
use crate::auth::{Actor, ClientIp};
use crate::db::{audit, new_id};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::routers::{self, deploy, scripts::Bootstrap, tunnel};
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use uuid::Uuid;

const MAX_NAME: usize = 60;
const MAX_NOTE: usize = 2000;

/// 删掉路由记录后，路由上要清掉的（中控只撤销了隧道公钥，路由上的东西它不去动）
pub const CLEANUP: &str = "/etc/init.d/fleet-tunnel disable; /etc/init.d/fleet-tunnel stop; rm -rf /etc/init.d/fleet-tunnel /etc/fleet-tunnel；\
     再从 /etc/dropbear/authorized_keys 删掉中控的公钥（不再让中控登录时）";

pub async fn list(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let rows = routers::load(&st.db, None).await?;
    Ok(ok(rows.iter().map(|r| r.view(false)).collect::<Vec<_>>()))
}

pub async fn get_one(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let rows = routers::load(&st.db, Some(id)).await?;
    rows.first().map(|r| ok(r.view(true))).ok_or_else(|| ApiError::not_found("没有这台软路由"))
}

fn check_name(n: &str) -> ApiResult<String> {
    let n = n.trim();
    if n.is_empty() || n.chars().count() > MAX_NAME {
        return Err(ApiError::bad_request(format!("名称不能为空，最长 {MAX_NAME} 个字")));
    }
    Ok(n.to_string())
}

fn check_note(n: &str) -> ApiResult<()> {
    if n.chars().count() > MAX_NOTE {
        return Err(ApiError::bad_request(format!("备注最长 {MAX_NOTE} 个字")));
    }
    Ok(())
}

async fn team_name(st: &AppState, id: Option<Uuid>) -> ApiResult<Option<String>> {
    let Some(id) = id else { return Ok(None) };
    let n: Option<String> = sqlx::query_scalar("select name from team where id = $1").bind(id).fetch_optional(&st.db).await?;
    n.map(Some).ok_or_else(|| ApiError::bad_request("team_id 指向的团队不存在"))
}

/// 一键部署的目标（密码只在内存里）
#[derive(Deserialize)]
pub struct SshIn {
    host: String,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
}

impl SshIn {
    fn login(self) -> ApiResult<tunnel::Login> { deploy::login(&self.host, self.port, self.user.as_deref(), self.password).map_err(ApiError::bad_request) }
}

#[derive(Deserialize)]
pub struct NewRouter {
    name: String,
    team_id: Option<Uuid>,
    #[serde(default)]
    note: String,
    /// 给了就一键部署（中控登录路由装好隧道），不发接入命令
    ssh: Option<SshIn>,
}

pub async fn create(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<NewRouter>) -> ApiResult<Json<Value>> {
    let name = check_name(&b.name)?;
    check_note(&b.note)?;
    let team = team_name(&st, b.team_id).await?;
    let login = b.ssh.map(SshIn::login).transpose()?;
    let id = new_id();
    // 一键部署的不发接入命令：免得多出一个 30 分钟内有效、能取走隧道私钥的令牌
    let token = login.is_none().then(routers::new_token);
    let mut tx = st.db.begin().await?;
    let port = routers::alloc_port(&mut tx, &st.cfg).await?;
    sqlx::query(
        "insert into router (id, name, team_id, note, tunnel_port, bootstrap_token_hash, bootstrap_expires_at) values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(&name)
    .bind(b.team_id)
    .bind(b.note.trim())
    .bind(port)
    .bind(token.as_ref().map(|t| t.1.clone()))
    .bind(token.as_ref().map(|t| t.2))
    .execute(&mut *tx)
    .await?;
    let method = if login.is_some() { "deploy" } else { "command" };
    audit(&mut tx, &a.name, "router.create", "router", Some(id), json!({"name": name, "team": team, "tunnel_port": port, "method": method})).await?;
    tx.commit().await?;
    st.emit(Event::Routers);
    match (login, token) {
        (Some(l), _) => {
            deploy::start(&st, id, l, &a.name).await?;
            Ok(ok(json!({"id": id})))
        }
        (None, Some((t, _, exp))) => Ok(ok(json!({"id": id, "bootstrap": bootstrap_out(&st, &t, exp)}))),
        (None, None) => unreachable!("没有一键部署时一定发了令牌"),
    }
}

fn bootstrap_out(st: &AppState, token: &str, expires: OffsetDateTime) -> Value {
    json!({"cmd": routers::bootstrap_cmd(&st.cfg.public_url, token), "expires_at": expires.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()})
}

#[derive(Deserialize)]
pub struct PatchRouter {
    name: Option<String>,
    #[serde(default, deserialize_with = "super::some")]
    team_id: Option<Option<Uuid>>,
    note: Option<String>,
}

pub async fn patch(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<PatchRouter>) -> ApiResult<Json<Value>> {
    let name = b.name.as_deref().map(check_name).transpose()?;
    if let Some(n) = &b.note {
        check_note(n)?;
    }
    if let Some(Some(t)) = b.team_id {
        team_name(&st, Some(t)).await?;
    }
    let mut tx = st.db.begin().await?;
    let cur: (String, Option<Uuid>, String) = sqlx::query_as("select name, team_id, note from router where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台软路由"))?;
    let after = (name.unwrap_or_else(|| cur.0.clone()), b.team_id.unwrap_or(cur.1), b.note.as_deref().map(str::trim).map(String::from).unwrap_or_else(|| cur.2.clone()));
    sqlx::query("update router set name = $2, team_id = $3, note = $4, updated_at = now() where id = $1")
        .bind(id)
        .bind(&after.0)
        .bind(after.1)
        .bind(&after.2)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &a.name,
        "router.update",
        "router",
        Some(id),
        json!({"name": after.0, "before": {"name": cur.0, "team_id": cur.1, "note": cur.2}, "after": {"name": after.0, "team_id": after.1, "note": after.2}}),
    )
    .await?;
    tx.commit().await?;
    st.emit(Event::Routers);
    get_one(a, State(st), ApiPath(id)).await
}

/// 删除：撤销隧道公钥（路由上的 fleet-tunnel 之后连不上；已经连着的那条要等它自己断），关掉它的告警（不发恢复），指标随记录删掉。
/// 路由上中控加的东西不去动，返回要在路由上执行的清理命令
pub async fn delete(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let (name, port): (String, i32) = sqlx::query_as("delete from router where id = $1 returning name, tunnel_port")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台软路由"))?;
    let closed = routers::alerts::close_all(&mut tx, id).await?;
    audit(&mut tx, &a.name, "router.delete", "router", Some(id), json!({"name": name, "tunnel_port": port})).await?;
    tx.commit().await?;
    tunnel::close_master(&st.cfg, id).await;
    st.emit(Event::Routers);
    if closed > 0 {
        st.emit(Event::Incidents);
    }
    routers::sync_files_or_500(&st).await?;
    Ok(ok(json!({"ok": true, "cleanup": CLEANUP})))
}

/// 重新生成接入命令：新的一次性令牌（30 分钟），旧隧道密钥立刻撤销、主机公钥清掉（新隧道连上后重记），隧道端口换一个。正在一键部署时 409
pub async fn bootstrap(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let (name, deploying): (String, bool) = sqlx::query_as("select name, deploy_state is not distinct from 'running' from router where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台软路由"))?;
    if deploying {
        return Err(ApiError::conflict("这台路由正在一键部署，等它结束再重新生成接入命令"));
    }
    let port = routers::rotate_port(&mut tx, &st.cfg, id).await?;
    let (t, h, exp) = routers::new_token();
    sqlx::query(
        "update router set tunnel_pubkey = null, tunnel_key_at = null, host_key = null, bootstrap_token_hash = $2, bootstrap_expires_at = $3, updated_at = now()
         where id = $1",
    )
    .bind(id)
    .bind(h)
    .bind(exp)
    .execute(&mut *tx)
    .await?;
    // 中控不再经旧隧道连：马上显示「等待接入」，不等下一轮采集（上一份快照留着）
    sqlx::query("update router_status set reachable = false, error = null where router_id = $1").bind(id).execute(&mut *tx).await?;
    audit(&mut tx, &a.name, "router.bootstrap", "router", Some(id), json!({"name": name, "tunnel_port": port})).await?;
    tx.commit().await?;
    tunnel::close_master(&st.cfg, id).await;
    st.emit(Event::Routers);
    routers::sync_files_or_500(&st).await?;
    Ok(ok(bootstrap_out(&st, &t, exp)))
}

/// 一键部署（重新部署）：立即返回，进度看路由的 deploy 字段与实时事件 routers
pub async fn deploy_one(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<SshIn>) -> ApiResult<Json<Value>> {
    let l = b.login()?;
    deploy::start(&st, id, l, &a.name).await?;
    Ok(ok(json!({"ok": true})))
}

// ── 取接入脚本（公开，不要登录）────────────────────────────────────────

/// 同一来源（IPv4 地址 / IPv6 的 /64）10 分钟最多取这么多次（令牌 32 字节随机，猜不到；这里只防刷库）
const FETCH_LIMIT: u32 = 20;
const FETCH_WINDOW: Duration = Duration::from_secs(600);
static FETCHES: Mutex<Option<HashMap<Option<IpAddr>, (Instant, u32)>>> = Mutex::new(None);

fn rate_ok(ip: Option<IpAddr>) -> bool {
    let mut g = FETCHES.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    if m.len() > 10_000 {
        m.retain(|_, (t, _)| t.elapsed() < FETCH_WINDOW);
    }
    let e = m.entry(ip.map(crate::auth::ip_group)).or_insert((Instant::now(), 0));
    if e.0.elapsed() >= FETCH_WINDOW {
        *e = (Instant::now(), 0);
    }
    e.1 += 1;
    e.1 <= FETCH_LIMIT
}

/// 给 `curl … | sh` 看的回应：成功是接入脚本；失败是一小段把原因打到 stderr、退出码 1 的脚本（curl 带 -f 时只显示 HTTP 状态）
fn script_response(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

fn script_error(status: StatusCode, msg: &str) -> Response { script_response(status, format!("echo 'fleet：{msg}' >&2\nexit 1\n")) }

/// `GET /api/v1/router-bootstrap/{令牌}`：令牌对、没过期才给接入脚本，给了就作废（只能用一次）；同时发一把新的隧道密钥（私钥只在这份脚本里）
pub async fn bootstrap_script(State(st): State<AppState>, ClientIp(ip): ClientIp, ApiPath(token): ApiPath<String>) -> Response {
    if !rate_ok(ip) {
        return script_error(StatusCode::TOO_MANY_REQUESTS, "请求太频繁，10 分钟后再试");
    }
    match fetch(&st, ip, &token).await {
        Ok(Some(s)) => script_response(StatusCode::OK, s),
        Ok(None) => script_error(StatusCode::NOT_FOUND, "接入链接不对、已过期或已经用过：在面板上重新生成接入命令"),
        Err(e) => {
            let eid = &Uuid::new_v4().simple().to_string()[..8];
            tracing::error!(error_id = eid, "生成接入脚本失败：{e:#}");
            script_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("中控生成接入脚本失败（编号 {eid}），请把编号告诉管理员查看日志"))
        }
    }
}

async fn fetch(st: &AppState, ip: Option<IpAddr>, token: &str) -> anyhow::Result<Option<String>> {
    if !routers::token_shape_ok(token) {
        return Ok(None);
    }
    // 先备好要用的（读不到中控的公钥就不作废令牌，修好配置后还能再取）
    let central = routers::central_keys(&st.cfg).await?;
    let ssh_host = st.cfg.routers.ssh_host_of(&st.cfg.public_url)?;
    let mut tx = st.db.begin().await?;
    let row: Option<(Uuid, String, i32)> = sqlx::query_as(
        "select id, name, tunnel_port from router where bootstrap_token_hash = $1 and bootstrap_expires_at > now() for update",
    )
    .bind(crate::crypto::sha256(token.as_bytes()))
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, name, port)) = row else { return Ok(None) };
    let key = routers::tunnel_keypair(&format!("fleet-tunnel router-{id}"));
    sqlx::query(
        "update router set bootstrap_token_hash = null, bootstrap_expires_at = null, tunnel_pubkey = $2, tunnel_key_at = now(), host_key = null,
            updated_at = now()
         where id = $1",
    )
    .bind(id)
    .bind(&key.public)
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, "router-bootstrap", "router.bootstrap_used", "router", Some(id), json!({"name": name, "ip": ip.map(|i| i.to_string()), "tunnel_port": port})).await?;
    tx.commit().await?;
    routers::sync_files(st).await?;
    tunnel::close_master(&st.cfg, id).await;
    st.emit(Event::Routers);
    let b = Bootstrap {
        router_id: id,
        tunnel_port: port,
        ssh_host: &ssh_host,
        ssh_port: st.cfg.routers.ssh_port,
        tunnel_key: &key.private_pem,
        central_hostkey: &central.host_key,
        central_pubkey: &central.login_pub,
    };
    Ok(Some(b.full()))
}

// ── 指标曲线 ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RangeQ {
    range: Option<String>,
}

/// range → (回看多久, 点的间隔秒数)：1 小时 30 秒一点（原始点）；6 / 24 小时 5 分钟一点（原始点取平均）；7 / 30 天 1 小时一点（小时汇总）
fn range_of(q: &RangeQ) -> ApiResult<(time::Duration, i64)> {
    Ok(match q.range.as_deref().unwrap_or("24h") {
        "1h" => (time::Duration::hours(1), 30),
        "6h" => (time::Duration::hours(6), 300),
        "24h" => (time::Duration::hours(24), 300),
        "7d" => (time::Duration::days(7), 3600),
        "30d" => (time::Duration::days(30), 3600),
        _ => return Err(ApiError::bad_request("range 只能是 1h/6h/24h/7d/30d")),
    })
}

async fn exists(st: &AppState, id: Uuid) -> ApiResult<()> {
    let n: Option<i32> = sqlx::query_scalar("select 1 from router where id = $1").bind(id).fetch_optional(&st.db).await?;
    n.map(|_| ()).ok_or_else(|| ApiError::not_found("没有这台软路由"))
}

/// 一个点的字段（原始点与 5 分钟平均共用；占用都换成百分比）
const POINT: &str = "'cpu_pct', cpu_pct, 'load1', load1, 'mem_pct', mem_pct, 'disk_pct', disk_pct, 'conntrack_pct', conntrack_pct, 'temp_c', temp_c,
    'wan_rx_bps', wan_rx_bps, 'wan_tx_bps', wan_tx_bps, 'proxy_up_bps', proxy_up_bps, 'proxy_down_bps', proxy_down_bps,
    'conns', conns, 'clients', clients, 'exits_alive', exits_alive";

pub async fn metrics(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<RangeQ>) -> ApiResult<Json<Value>> {
    let (back, step) = range_of(&q)?;
    exists(&st, id).await?;
    let sql = match step {
        30 => format!(
            "select coalesce(json_agg(json_build_object('ts', ts, {POINT}) order by ts), '[]') from (
               select ts, cpu_pct, load1, 100.0 * mem_used / nullif(mem_total, 0) mem_pct, 100.0 * disk_used / nullif(disk_total, 0) disk_pct,
                 100.0 * conntrack / nullif(conntrack_max, 0) conntrack_pct, temp_c, wan_rx_bps, wan_tx_bps, proxy_up_bps, proxy_down_bps,
                 conns, clients, exits_alive
               from router_metric where router_id = $1 and ts >= $2) a"
        ),
        300 => format!(
            "select coalesce(json_agg(json_build_object('ts', ts, {POINT}) order by ts), '[]') from (
               select date_bin('5 minutes', ts, timestamptz 'epoch') ts, avg(cpu_pct) cpu_pct, avg(load1) load1,
                 100.0 * avg(mem_used) / nullif(max(mem_total), 0) mem_pct, 100.0 * avg(disk_used) / nullif(max(disk_total), 0) disk_pct,
                 100.0 * avg(conntrack) / nullif(max(conntrack_max), 0) conntrack_pct, avg(temp_c) temp_c,
                 avg(wan_rx_bps)::bigint wan_rx_bps, avg(wan_tx_bps)::bigint wan_tx_bps, avg(proxy_up_bps)::bigint proxy_up_bps,
                 avg(proxy_down_bps)::bigint proxy_down_bps, avg(conns) conns, avg(clients) clients, avg(exits_alive) exits_alive
               from router_metric where router_id = $1 and ts >= $2 group by 1) a"
        ),
        _ => format!(
            "select coalesce(json_agg(json_build_object('ts', hour, {POINT}, 'cpu_max', cpu_pct_max, 'wan_rx_bps_max', wan_rx_bps_max,
                 'wan_tx_bps_max', wan_tx_bps_max, 'proxy_up_bps_max', proxy_up_bps_max, 'proxy_down_bps_max', proxy_down_bps_max) order by hour), '[]') from (
               select hour, cpu_pct, cpu_pct_max, load1, 100.0 * mem_used / nullif(mem_total, 0) mem_pct, 100.0 * disk_used / nullif(disk_total, 0) disk_pct,
                 100.0 * conntrack / nullif(conntrack_max, 0) conntrack_pct, temp_c, wan_rx_bps, wan_rx_bps_max, wan_tx_bps, wan_tx_bps_max,
                 proxy_up_bps, proxy_up_bps_max, proxy_down_bps, proxy_down_bps_max, conns, clients, exits_alive
               from router_metric_hourly where router_id = $1 and hour >= $2) a"
        ),
    };
    let points: Value = sqlx::query_scalar(&sql).bind(id).bind(OffsetDateTime::now_utc() - back).fetch_one(&st.db).await?;
    Ok(ok(json!({"step_s": step, "points": points})))
}

/// 各出口的指标点。每个点都带 `tests`（这一点里测了几次延迟）与 `tests_ok`（其中通的次数），三种粒度同一口径：
/// 原始点（30 秒）测了的那一点 tests = 1（tests_ok 通为 1、不通为 0），没测的都是 0（alive 为 null）；5 分钟点是这 5 分钟里的次数（一般 1 次，alive = 全通）；
/// 小时点是这一小时的次数（alive = 全通，另带 alive_pct）。fleetctl router exits 按它们数测试次数与失败次数
pub async fn exit_metrics(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<RangeQ>) -> ApiResult<Json<Value>> {
    let (back, step) = range_of(&q)?;
    exists(&st, id).await?;
    let inner = match step {
        30 => "select exit, ts, delay_ms, alive, conns, up_bps, down_bps, null::float8 alive_pct,
                  (alive is not null)::int tests, coalesce(alive, false)::int tests_ok
                from router_exit_metric where router_id = $1 and ts >= $2",
        300 => "select exit, date_bin('5 minutes', ts, timestamptz 'epoch') ts, avg(delay_ms) filter (where alive) delay_ms, bool_and(alive) alive,
                  avg(conns) conns, avg(up_bps)::bigint up_bps, avg(down_bps)::bigint down_bps, null::float8 alive_pct,
                  count(alive)::int tests, (count(*) filter (where alive))::int tests_ok
                from router_exit_metric where router_id = $1 and ts >= $2 group by 1, 2",
        _ => "select exit, hour ts, delay_ms, case when tests > 0 then tests_ok = tests end alive, conns, up_bps, down_bps,
                case when tests > 0 then 100.0 * tests_ok / tests end alive_pct, tests, tests_ok
              from router_exit_metric_hourly where router_id = $1 and hour >= $2",
    };
    // 小时汇总的点另带 alive_pct（这一小时测通的比例）
    let extra = if step == 3600 { ", 'alive_pct', alive_pct" } else { "" };
    let sql = format!(
        "select coalesce(json_agg(json_build_object('name', exit, 'points', pts) order by exit), '[]') from (
           select exit, json_agg(json_build_object('ts', ts, 'delay_ms', delay_ms, 'alive', alive, 'conns', conns, 'up_bps', up_bps, 'down_bps', down_bps,
                                                   'tests', tests, 'tests_ok', tests_ok{extra})
                                 order by ts) pts
           from ({inner}) p group by exit) x"
    );
    let exits: Value = sqlx::query_scalar(&sql).bind(id).bind(OffsetDateTime::now_utc() - back).fetch_one(&st.db).await?;
    Ok(ok(json!({"step_s": step, "exits": exits})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let r = |s: &str| range_of(&RangeQ { range: Some(s.into()) }).map(|x| x.1).ok();
        assert_eq!([r("1h"), r("6h"), r("24h"), r("7d"), r("30d"), r("2h")], [Some(30), Some(300), Some(300), Some(3600), Some(3600), None]);
        assert_eq!(range_of(&RangeQ { range: None }).unwrap().1, 300);
    }

    #[test]
    fn fetch_rate_limit_per_source() {
        let ip: Option<IpAddr> = Some("203.0.113.77".parse().unwrap());
        assert!((0..FETCH_LIMIT).all(|_| rate_ok(ip)));
        assert!(!rate_ok(ip), "超过上限");
        assert!(rate_ok(Some("203.0.113.78".parse().unwrap())), "别的来源不受影响");
    }
}
