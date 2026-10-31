//! 探测点用的 `/agent/v1/probe/*`（挂在 agent_api 的路由下）。鉴权：`Authorization: Bearer <令牌>`，令牌是探测点自己的
//! （probe_point.token_hash，安装时发，重装 / 删除即作废；与机器的 agent 令牌是两套，互相用不了）。
//! - `GET targets` → `fleet_proto::ProbeTargets`：在用的、有在用入口的线路机（public_host + 在用入口端口）、对照目标、要不要升级；
//!   停用的探测点拿到空目标（照常上报心跳，不测）；
//! - `POST report` ← `fleet_proto::ProbeReport`：写进 relay_reach（probe_point_id 是它，时间取中控收到的时刻），刷新最近上报、版本、来源地址；
//!   只收在用线路机的在用入口端口，别的丢掉。带 `all_down`（对照目标通、三台以上线路机全部连不上）的照记，行上标 all_down；
//!   没作废的轮次刷新 `last_kept_at`（最近一次被采纳），作废的记下原因（`discard_reason`）：主来源超过 3 分钟没有被采纳的轮次按离线处理（probe_points/）；
//! - `GET binary`：dist 里的 fleet-agent（一次性安装命令下载它；`upgrade` 时探测点自己下载替换，记下这次升级第一次下载的时刻）。
//!
//! 升级失败不反复下发：下载过程序、过了 [`UPGRADE_FAIL_S`]（探测点的试运行时长）还没报上目标 sha256，就是这次没升成：
//! 下载失败、核对 sha256 或试跑 `--version` 不过（没换上，探测点没有任何标记）、或换上后没能上报已经换回了旧版本。照旧下发会每轮反复下载、换上、换回。
//! 这时（它下次取目标时）清掉升级请求、记下 upgrade_failed（列表显示「升级到 X 失败，已回到 Y」），要手动再触发；之后报上了目标 sha256 就清掉。
//! 新探测点在上报里带上它记着的坏版本（`bad_sha256`：换上过、没能上报、自己换回来了的；它不会再换，也就不去下载，上面的判定等不到）：
//! 存进 probe_point.bad_sha256；要它升级到的正是这个就当场记成升级失败、不再下发；手动升级到这个版本直接 409（api/probe_points.rs）。
//!
//! 上报太频繁（同一个探测点 10 秒内第二份）回 429：正常每 30 秒一份。

use crate::auth::ClientIp;
use crate::crypto::sha256;
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::reach::{self, Sample};
use crate::state::AppState;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use fleet_proto::{ProbeReport, ProbeTarget, ProbeTargets, ProbeUpgrade};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;
use uuid::Uuid;

/// 每轮间隔（同中控自己的检查）
pub const INTERVAL_S: u32 = 30;
/// 同一个探测点两份上报至少隔这么久
const MIN_GAP_S: f64 = 10.0;
/// 一份上报最多这么多条（线路机数，远大于实际）
const MAX_RESULTS: usize = 1000;
/// 下载程序之后这么久还没报上目标 sha256 就算升级失败（同探测点的试运行时长，fleet-agent probe_point::TRIAL_WINDOW）
pub const UPGRADE_FAIL_S: f64 = 300.0;

pub fn router() -> Router<AppState> {
    Router::new().route("/probe/targets", get(targets)).route("/probe/report", post(report)).route("/probe/binary", get(binary))
}

struct Probe {
    id: Uuid,
    enabled: bool,
    upgrade: Option<String>,
}

async fn authenticate(st: &AppState, h: &HeaderMap) -> ApiResult<Probe> {
    let tok = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).ok_or_else(ApiError::unauthorized)?;
    let row: Option<(Uuid, bool, Option<String>)> = sqlx::query_as("select id, enabled, upgrade_sha256 from probe_point where token_hash = $1")
        .bind(sha256(tok.trim().as_bytes()))
        .fetch_optional(&st.db)
        .await?;
    let (id, enabled, upgrade) = row.ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "token_revoked", "令牌无效或已撤销（探测点被删除或重装过）"))?;
    Ok(Probe { id, enabled, upgrade })
}

async fn targets(State(st): State<AppState>, h: HeaderMap) -> ApiResult<Json<ProbeTargets>> {
    let mut p = authenticate(&st, &h).await?;
    if p.upgrade.is_some() {
        let failed: Option<(Value,)> = sqlx::query_as(
            "update probe_point set upgrade_failed = jsonb_build_object('sha256', upgrade_sha256, 'to', upgrade_version, 'from', version, 'at', now()),
                    upgrade_sha256 = null, upgrade_fetched_at = null
             where id = $1 and upgrade_sha256 is not null and upgrade_fetched_at <= now() - make_interval(secs => $2)
             returning upgrade_failed",
        )
        .bind(p.id)
        .bind(UPGRADE_FAIL_S)
        .fetch_optional(&st.db)
        .await?;
        if let Some((f,)) = failed {
            tracing::warn!(probe_point = %p.id, "探测点升级到 {} 失败（下载后 {UPGRADE_FAIL_S} 秒内没报上新版本），已回到 {}：不再自动下发", f["to"], f["from"]);
            p.upgrade = None;
            st.emit(crate::state::Event::Settings);
        }
    }
    let relays = if p.enabled {
        reach::targets(&st.db)
            .await?
            .into_iter()
            .map(|(relay_id, host, ports)| ProbeTarget { relay_id, host, ports: ports.into_iter().filter_map(|(x, _)| u16::try_from(x).ok()).collect() })
            .collect()
    } else {
        vec![]
    };
    let upgrade = match p.upgrade {
        Some(sha) => Some(ProbeUpgrade { sha256: sha, version: dist_version(&st).await.unwrap_or_default() }),
        None => None,
    };
    Ok(Json(ProbeTargets { interval_s: INTERVAL_S, controls: reach::CONTROLS.iter().map(|s| s.to_string()).collect(), relays, upgrade }))
}

/// dist 里 fleet-agent 的版本号（`--version` 的第二段）；读不出为 None
pub(crate) async fn dist_version(st: &AppState) -> Option<String> {
    let path = st.cfg.dist_dir.join("fleet-agent");
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), tokio::process::Command::new(&path).arg("--version").output()).await.ok()?.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).split_whitespace().nth(1).map(String::from)).flatten()
}

async fn report(State(st): State<AppState>, ClientIp(ip): ClientIp, h: HeaderMap, ApiJson(r): ApiJson<ProbeReport>) -> ApiResult<Response> {
    let p = authenticate(&st, &h).await?;
    if r.results.len() > MAX_RESULTS {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "too_many", format!("一份最多 {MAX_RESULTS} 条")));
    }
    let version: String = r.version.chars().take(64).collect();
    // 刷新最近上报；离上一份不到 10 秒的不收（不刷新）。升级到位（报上来的程序 sha256 就是要它换的）清掉升级请求（记成失败了的也清掉）。
    // 返回改之前的最近上报：自连接读旧版本（写成 `with o as (select … for update) … returning (select … from o)` 时，
    // CTE 在 returning 里才求值，那一行已被本语句改过、看不到，恒为 null——每一份都会被当成「刚回来」）
    // 要它升级到的正是它记着的坏版本（换上过、没能上报、自己换回来了）：它不会再换（也不下载），等下载后的失败判定永远等不到，
    // 这里直接记成升级失败、不再下发
    let bad: Option<String> = r.bad_sha256.as_deref().map(|s| s.chars().take(64).collect());
    let prev: Option<(Option<OffsetDateTime>, bool)> = sqlx::query_as(
        "update probe_point p set last_seen_at = now(), last_ip = $2::inet, version = $3, bad_sha256 = $6,
                upgrade_sha256 = case when p.upgrade_sha256 in ($4, $6) then null else p.upgrade_sha256 end,
                upgrade_fetched_at = case when p.upgrade_sha256 in ($4, $6) then null else p.upgrade_fetched_at end,
                upgrade_failed = case when p.upgrade_failed->>'sha256' = $4 then null
                                      when p.upgrade_sha256 = $6 then jsonb_build_object('sha256', p.upgrade_sha256, 'to', p.upgrade_version, 'from', $3::text, 'at', now())
                                      else p.upgrade_failed end
         from probe_point o
         where p.id = $1 and o.id = p.id and (p.last_seen_at is null or p.last_seen_at < now() - make_interval(secs => $5))
         returning o.last_seen_at, coalesce(o.upgrade_sha256 = $6 and o.upgrade_sha256 is distinct from $4, false)",
    )
    .bind(p.id)
    .bind(ip.map(|i| i.to_string()))
    .bind(&version)
    .bind(r.sha256.as_deref())
    .bind(MIN_GAP_S)
    .bind(&bad)
    .fetch_optional(&st.db)
    .await?;
    let Some((prev, bad_hit)) = prev else {
        return Ok((StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "10")], Json(json!({"error": {"code": "too_frequent", "message": "上报太频繁"}}))).into_response());
    };
    if bad_hit {
        tracing::warn!(probe_point = %p.id, "要探测点升级到的版本之前在它上面换上后没能上报、已经换回过（现在跑的是 {version}）：记成升级失败，不再下发");
        st.emit(crate::state::Event::Settings);
    }
    // 刚回来（之前离线或从没上报过）：它若是主来源，实际用的那一路就变了
    let now = OffsetDateTime::now_utc();
    let back = prev.is_none_or(|t| st.report_gaps.silent_s(t, now) >= super::OFFLINE_S);
    let kept = p.enabled && r.discarded.is_none();
    let accepted = if kept { ingest(&st, p.id, &r).await? } else { 0 };
    // 最近一次被采纳的轮次：采纳了就是现在；作废的记下原因。刚回来的从现在算起（离线那段不算作废，回来后又满 3 分钟都作废才算）。
    // 停用的不测，不动它（启用时多半正离线，回来时从那一刻算起）。作废满 3 分钟、又被采纳时实际用的那一路会变，由 run_forever 发事件
    if p.enabled {
        let why: Option<String> = r.discarded.as_deref().map(|w| w.chars().take(200).collect());
        sqlx::query("update probe_point set last_kept_at = case when $2 or $3 or last_kept_at is null then now() else last_kept_at end, discard_reason = $4 where id = $1")
            .bind(p.id)
            .bind(kept)
            .bind(back)
            .bind(&why)
            .execute(&st.db)
            .await?;
    }
    if back {
        super::changed(&st);
    }
    if let Some(why) = r.discarded.as_deref() {
        tracing::debug!(probe_point = %p.id, "探测点整轮作废：{why}");
    }
    Ok(Json(json!({"accepted": accepted, "interval_s": INTERVAL_S})).into_response())
}

/// 只收在用线路机的在用入口端口。返回写入的条数
async fn ingest(st: &AppState, id: Uuid, r: &ProbeReport) -> anyhow::Result<usize> {
    // 要连的端口 → 入口端口（只留 AnyTLS 的线路机连的是 AnyTLS 端口，记录按入口端口记，见 reach::targets）
    let live: HashMap<Uuid, HashMap<i32, i32>> = reach::targets(&st.db).await?.into_iter().map(|(rid, _, ps)| (rid, ps.into_iter().collect())).collect();
    let mut seen = HashSet::new();
    let samples: Vec<Sample> = r
        .results
        .iter()
        .filter(|x| live.get(&x.relay_id).is_some_and(|ps| ps.contains_key(&i32::from(x.port))) && seen.insert(x.relay_id))
        .map(|x| Sample {
            relay: x.relay_id,
            port: live[&x.relay_id][&i32::from(x.port)],
            ok: x.ok,
            // 失败原因带上端口（同中控的），截短：告警摘要里放得下
            detail: (!x.ok).then(|| format!("端口 {}：{}", x.port, x.detail.as_deref().unwrap_or("连不上").chars().take(80).collect::<String>())),
            rtt_ms: x.rtt_ms.filter(|v| v.is_finite() && *v >= 0.0 && *v < 60_000.0).filter(|_| x.ok),
        })
        .collect();
    if samples.is_empty() {
        return Ok(0);
    }
    reach::record(st, Some(id), OffsetDateTime::now_utc(), &samples, r.all_down).await?;
    Ok(samples.len())
}

/// dist 里的 fleet-agent（探测点的安装命令下载、自己升级）
async fn binary(State(st): State<AppState>, h: HeaderMap) -> ApiResult<Response> {
    let p = authenticate(&st, &h).await?;
    // 升级的下载：记下这次请求第一次下载的时刻（判断升级失败从它算起；安装命令下载时没有升级请求，不记）
    sqlx::query("update probe_point set upgrade_fetched_at = coalesce(upgrade_fetched_at, now()) where id = $1 and upgrade_sha256 is not null")
        .bind(p.id)
        .execute(&st.db)
        .await?;
    let path = st.cfg.dist_dir.join("fleet-agent");
    let data = tokio::fs::read(&path).await.map_err(|e| ApiError::conflict(format!("中控上没有可下载的 fleet-agent（{}：{e}）", path.display())))?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response())
}
