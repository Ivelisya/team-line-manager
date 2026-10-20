//! `/agent/v1/*`：边缘上报、期望状态长轮询、应用回报；`/echo` 回显来源 IP。

use crate::error::ApiJson;
use crate::crypto::sha256;
use crate::entries;
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Event};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use fleet_proto::{ApplyReport, Report, ReportAck};
use serde::Deserialize;
use std::net::SocketAddr;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/report", post(report))
        .route("/desired", get(desired))
        .route("/applied", post(applied))
}

struct AgentId {
    machine_id: Uuid,
    kind: String,
}

async fn authenticate(st: &AppState, h: &HeaderMap) -> ApiResult<AgentId> {
    let tok = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(ApiError::unauthorized)?;
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "update agent_token t set last_used_at = now() from machine m
         where t.token_hash = $1 and t.revoked_at is null and m.id = t.machine_id and m.lifecycle <> 'retired'
         returning m.id, m.kind::text",
    )
    .bind(sha256(tok.trim().as_bytes()))
    .fetch_optional(&st.db)
    .await?;
    let (machine_id, kind) = row.ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "token_revoked", "令牌无效或已撤销"))?;
    Ok(AgentId { machine_id, kind })
}

fn ts(ms: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).unwrap_or_else(|_| OffsetDateTime::now_utc())
}

async fn report(State(st): State<AppState>, h: HeaderMap, ApiJson(r): ApiJson<Report>) -> ApiResult<Json<ReportAck>> {
    let a = authenticate(&st, &h).await?;
    let mut tx = st.db.begin().await?;
    let last: Option<(i64, Option<String>)> =
        sqlx::query_as("select last_seq, boot_id from agent_state where machine_id = $1 for update").bind(a.machine_id).fetch_optional(&mut *tx).await?;
    // 去重：同一份数据重发（seq 不增）直接确认
    if let Some((seq, _)) = &last {
        if r.seq as i64 <= *seq {
            tx.rollback().await?;
            return Ok(Json(ReportAck { accepted_seq: *seq as u64, interval_s: 10 }));
        }
    }
    let info = r.info.as_ref().map(|i| serde_json::to_value(i).unwrap_or_default());
    let xray = r.xray.as_ref().map(|x| serde_json::to_value(x).unwrap_or_default());
    sqlx::query(
        "insert into agent_state (machine_id, agent_version, boot_id, last_seq, last_report_at, info)
         values ($1, $2, $3, $4, now(), coalesce($5, '{}'::jsonb) || jsonb_strip_nulls(jsonb_build_object('xray', $6::jsonb)))
         on conflict (machine_id) do update set agent_version = excluded.agent_version, boot_id = excluded.boot_id,
           last_seq = excluded.last_seq, last_report_at = now(),
           info = case when $5 is null then agent_state.info else $5 end || jsonb_strip_nulls(jsonb_build_object('xray', $6::jsonb))",
    )
    .bind(a.machine_id)
    .bind(&r.agent_version)
    .bind(&r.boot_id)
    .bind(r.seq as i64)
    .bind(info)
    .bind(xray)
    .execute(&mut *tx)
    .await?;

    if !r.host.is_empty() {
        let n = r.host.len();
        let (mut t, mut cpu, mut l1, mut mu, mut mt, mut du, mut dt, mut rx, mut txb, mut tc) = (
            Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n),
            Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n),
        );
        for s in &r.host {
            t.push(ts(s.ts_ms));
            cpu.push(s.cpu_pct);
            l1.push(s.load1);
            mu.push(s.mem_used as i64);
            mt.push(s.mem_total as i64);
            du.push(s.disk_used as i64);
            dt.push(s.disk_total as i64);
            rx.push(s.net_rx_bytes as i64);
            txb.push(s.net_tx_bytes as i64);
            tc.push(s.tcp_conns as i32);
        }
        sqlx::query(
            "insert into host_metric (machine_id, ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bytes, net_tx_bytes, tcp_conns)
             select $1, * from unnest($2::timestamptz[], $3::real[], $4::real[], $5::bigint[], $6::bigint[], $7::bigint[], $8::bigint[], $9::bigint[], $10::bigint[], $11::int[])",
        )
        .bind(a.machine_id)
        .bind(&t)
        .bind(&cpu)
        .bind(&l1)
        .bind(&mu)
        .bind(&mt)
        .bind(&du)
        .bind(&dt)
        .bind(&rx)
        .bind(&txb)
        .bind(&tc)
        .execute(&mut *tx)
        .await?;
    }

    // 线路机数据：只收属于这台线路机的入口
    if a.kind == "relay" && (!r.entries.is_empty() || !r.probes.is_empty() || !r.access.is_empty()) {
        let mine: Vec<(Uuid,)> = sqlx::query_as("select id from entry where relay_id = $1").bind(a.machine_id).fetch_all(&mut *tx).await?;
        let mine: std::collections::HashSet<Uuid> = mine.into_iter().map(|(e,)| e).collect();
        let es: Vec<_> = r.entries.iter().filter(|e| mine.contains(&e.entry_id)).collect();
        if !es.is_empty() {
            sqlx::query(
                "insert into entry_traffic (entry_id, ts, up_bytes, down_bytes, conns)
                 select * from unnest($1::uuid[], $2::timestamptz[], $3::bigint[], $4::bigint[], $5::int[])",
            )
            .bind(es.iter().map(|e| e.entry_id).collect::<Vec<_>>())
            .bind(es.iter().map(|e| ts(e.ts_ms)).collect::<Vec<_>>())
            .bind(es.iter().map(|e| e.up_bytes as i64).collect::<Vec<_>>())
            .bind(es.iter().map(|e| e.down_bytes as i64).collect::<Vec<_>>())
            .bind(es.iter().map(|e| e.conns as i32).collect::<Vec<_>>())
            .execute(&mut *tx)
            .await?;
        }
        let ps: Vec<_> = r.probes.iter().filter(|p| mine.contains(&p.entry_id)).collect();
        if !ps.is_empty() {
            sqlx::query(
                "insert into probe_result (entry_id, ts, kind, ok, latency_ms, detail)
                 select * from unnest($1::uuid[], $2::timestamptz[], $3::text[], $4::bool[], $5::real[], $6::text[])",
            )
            .bind(ps.iter().map(|p| p.entry_id).collect::<Vec<_>>())
            .bind(ps.iter().map(|p| ts(p.ts_ms)).collect::<Vec<_>>())
            .bind(ps.iter().map(|p| serde_json::to_value(p.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()).collect::<Vec<_>>())
            .bind(ps.iter().map(|p| p.ok).collect::<Vec<_>>())
            .bind(ps.iter().map(|p| p.latency_ms).collect::<Vec<_>>())
            .bind(ps.iter().map(|p| p.detail.clone()).collect::<Vec<_>>())
            .execute(&mut *tx)
            .await?;
        }
        let ac: Vec<_> = r.access.iter().filter(|x| mine.contains(&x.entry_id)).take(5000).collect();
        if !ac.is_empty() {
            sqlx::query(
                "insert into access_event (entry_id, ts, src_ip, dest_host, dest_port, network, blocked)
                 select * from unnest($1::uuid[], $2::timestamptz[], $3::inet[], $4::text[], $5::int[], $6::text[], $7::bool[])",
            )
            .bind(ac.iter().map(|x| x.entry_id).collect::<Vec<_>>())
            .bind(ac.iter().map(|x| ts(x.ts_ms)).collect::<Vec<_>>())
            .bind(ac.iter().map(|x| x.src_ip.map(ipnetwork::IpNetwork::from)).collect::<Vec<_>>())
            .bind(ac.iter().map(|x| x.dest_host.chars().take(253).collect::<String>()).collect::<Vec<_>>())
            .bind(ac.iter().map(|x| x.dest_port.map(|p| p as i32)).collect::<Vec<_>>())
            .bind(ac.iter().map(|x| x.network.clone()).collect::<Vec<_>>())
            .bind(ac.iter().map(|x| x.blocked).collect::<Vec<_>>())
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    if last.is_none() {
        st.emit(Event::Machine { id: a.machine_id });
    }
    Ok(Json(ReportAck { accepted_seq: r.seq, interval_s: 10 }))
}

#[derive(Deserialize)]
struct DesiredQ {
    after: Option<u64>,
}

async fn desired(State(st): State<AppState>, h: HeaderMap, Query(q): Query<DesiredQ>) -> ApiResult<Response> {
    let a = authenticate(&st, &h).await?;
    if a.kind != "relay" {
        return Err(ApiError::forbidden("只有线路机有期望状态"));
    }
    let after = q.after.unwrap_or(0) as i64;
    // 先订阅再查，避免漏掉查完之后、订阅之前的变化
    let mut rx = st.relay_changed.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (g,): (i64,) = sqlx::query_as("select generation from relay where machine_id = $1").bind(a.machine_id).fetch_one(&st.db).await?;
        if g > after {
            let echo = format!("{}/echo", st.cfg.public_url.trim_end_matches('/'));
            let ds = entries::desired_state(&st.db, &st.secrets, a.machine_id, &echo).await?;
            return Ok(Json(ds).into_response());
        }
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => return Ok(StatusCode::NO_CONTENT.into_response()),
                Ok(Ok(id)) if id == a.machine_id => break,
                Ok(Ok(_)) => continue,
                // 落后太多：直接重查
                Ok(Err(_)) => break,
            }
        }
    }
}

async fn applied(State(st): State<AppState>, h: HeaderMap, ApiJson(r): ApiJson<ApplyReport>) -> ApiResult<StatusCode> {
    let a = authenticate(&st, &h).await?;
    if a.kind != "relay" {
        return Err(ApiError::forbidden("只有线路机回报应用结果"));
    }
    entries::on_applied(&st.db, a.machine_id, &r).await?;
    if !r.ok {
        tracing::warn!(relay = %a.machine_id, generation = r.generation, "应用失败：{:?}", r.error);
    }
    st.emit(Event::Relay { id: a.machine_id });
    st.emit(Event::Machines);
    Ok(StatusCode::NO_CONTENT)
}

/// 回显请求方 IP（出口探测用）
pub async fn echo(State(st): State<AppState>, ConnectInfo(addr): ConnectInfo<SocketAddr>, h: HeaderMap) -> String {
    if st.cfg.trust_forwarded_for {
        if let Some(ip) = h.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()) {
            return ip.trim().to_string();
        }
    }
    addr.ip().to_canonical().to_string()
}
