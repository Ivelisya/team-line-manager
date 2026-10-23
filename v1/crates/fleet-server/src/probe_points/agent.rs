//! 探测点用的 `/agent/v1/probe/*`（挂在 agent_api 的路由下）。鉴权：`Authorization: Bearer <令牌>`，令牌是探测点自己的
//! （probe_point.token_hash，安装时发，重装 / 删除即作废；与机器的 agent 令牌是两套，互相用不了）。
//! - `GET targets` → `fleet_proto::ProbeTargets`：在用的、有在用入口的线路机（public_host + 在用入口端口）、对照目标、要不要升级；
//!   停用的探测点拿到空目标（照常上报心跳，不测）；
//! - `POST report` ← `fleet_proto::ProbeReport`：写进 relay_reach（probe_point_id 是它，时间取中控收到的时刻），刷新最近上报、版本、来源地址；
//!   只收在用线路机的在用入口端口，别的丢掉；服务端再按 `keep_round` 核一遍（三台以上全不通整轮不记）；
//! - `GET binary`：dist 里的 fleet-agent（一次性安装命令下载它；`upgrade` 时探测点自己下载替换）。
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
use serde_json::json;
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;
use uuid::Uuid;

/// 每轮间隔（同中控自己的检查）
pub const INTERVAL_S: u32 = 30;
/// 同一个探测点两份上报至少隔这么久
const MIN_GAP_S: f64 = 10.0;
/// 一份上报最多这么多条（线路机数，远大于实际）
const MAX_RESULTS: usize = 1000;

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
    let p = authenticate(&st, &h).await?;
    let relays = if p.enabled {
        reach::targets(&st.db)
            .await?
            .into_iter()
            .map(|(relay_id, host, ports)| ProbeTarget { relay_id, host, ports: ports.into_iter().filter_map(|x| u16::try_from(x).ok()).collect() })
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
    // 刷新最近上报；离上一份不到 10 秒的不收（不刷新）。升级到位（报上来的程序 sha256 就是要它换的）清掉升级请求
    let prev: Option<(Option<OffsetDateTime>,)> = sqlx::query_as(
        "with o as (select last_seen_at from probe_point where id = $1 for update)
         update probe_point set last_seen_at = now(), last_ip = $2::inet, version = $3,
                upgrade_sha256 = case when upgrade_sha256 = $4 then null else upgrade_sha256 end
         where id = $1 and (last_seen_at is null or last_seen_at < now() - make_interval(secs => $5))
         returning (select last_seen_at from o)",
    )
    .bind(p.id)
    .bind(ip.map(|i| i.to_string()))
    .bind(&version)
    .bind(r.sha256.as_deref())
    .bind(MIN_GAP_S)
    .fetch_optional(&st.db)
    .await?;
    let Some((prev,)) = prev else {
        return Ok((StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "10")], Json(json!({"error": {"code": "too_frequent", "message": "上报太频繁"}}))).into_response());
    };
    // 刚回来（之前离线或从没上报过）：它若是主来源，实际用的那一路就变了
    let back = prev.is_none_or(|t| (OffsetDateTime::now_utc() - t).whole_seconds() >= super::OFFLINE_S);
    let accepted = if p.enabled && r.discarded.is_none() { ingest(&st, p.id, &r).await? } else { 0 };
    if back {
        super::changed(&st);
    }
    if let Some(why) = r.discarded.as_deref() {
        tracing::debug!(probe_point = %p.id, "探测点整轮作废：{why}");
    }
    Ok(Json(json!({"accepted": accepted, "interval_s": INTERVAL_S})).into_response())
}

/// 只收在用线路机的在用入口端口；服务端再按 keep_round 核一遍。返回写入的条数
async fn ingest(st: &AppState, id: Uuid, r: &ProbeReport) -> anyhow::Result<usize> {
    let live: HashMap<Uuid, HashSet<i32>> = reach::targets(&st.db).await?.into_iter().map(|(rid, _, ps)| (rid, ps.into_iter().collect())).collect();
    let mut seen = HashSet::new();
    let samples: Vec<Sample> = r
        .results
        .iter()
        .filter(|x| live.get(&x.relay_id).is_some_and(|ps| ps.contains(&i32::from(x.port))) && seen.insert(x.relay_id))
        .map(|x| Sample {
            relay: x.relay_id,
            port: i32::from(x.port),
            ok: x.ok,
            // 失败原因带上端口（同中控的），截短：告警摘要里放得下
            detail: (!x.ok).then(|| format!("端口 {}：{}", x.port, x.detail.as_deref().unwrap_or("连不上").chars().take(80).collect::<String>())),
            rtt_ms: x.rtt_ms.filter(|v| v.is_finite() && *v >= 0.0 && *v < 60_000.0).filter(|_| x.ok),
        })
        .collect();
    let oks: Vec<bool> = samples.iter().map(|s| s.ok).collect();
    if samples.is_empty() || !reach::keep_round(&oks, true) {
        return Ok(0);
    }
    reach::record(st, Some(id), OffsetDateTime::now_utc(), &samples).await?;
    Ok(samples.len())
}

/// dist 里的 fleet-agent（探测点的安装命令下载、自己升级）
async fn binary(State(st): State<AppState>, h: HeaderMap) -> ApiResult<Response> {
    authenticate(&st, &h).await?;
    let path = st.cfg.dist_dir.join("fleet-agent");
    let data = tokio::fs::read(&path).await.map_err(|e| ApiError::conflict(format!("中控上没有可下载的 fleet-agent（{}：{e}）", path.display())))?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response())
}
