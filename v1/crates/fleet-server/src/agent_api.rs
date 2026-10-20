//! `/agent/v1/*`：边缘上报、期望状态长轮询、应用回报；`/echo` 回显来源 IP。

use crate::crypto::sha256;
use crate::entries;
use crate::error::ApiJson;
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Event};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use fleet_proto::{ApplyReport, Report, ReportAck};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use time::{Date, OffsetDateTime};
use tower_http::decompression::RequestDecompressionLayer;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        // 上报体可以是 gzip（agent 发送时压缩）：解压层在 handler 之前，默认 2 MiB 的请求体上限（DefaultBodyLimit）
        // 由 Json 提取器施加，量的是解压后的字节，压缩炸弹也被它截住。不带 Content-Encoding 的旧 agent 原样放行。
        .route("/report", post(report).layer(RequestDecompressionLayer::new()))
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

const DAY_MS: i64 = 86_400_000;

/// 时间戳落在 `[lo, hi]`（Unix 毫秒）内才收，否则丢弃。
/// 分区只覆盖保留期以内到未来几天：窗口外的样本若硬写会让整份上报的事务失败，agent 又会对同一个 seq 无限重试。
fn ts_in(ms: i64, lo: i64, hi: i64) -> Option<OffsetDateTime> {
    if ms < lo || ms > hi {
        return None;
    }
    OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).ok()
}

/// 过滤出窗口内的样本，窗口外的计入 `dropped`
fn windowed<'a, T>(items: impl Iterator<Item = &'a T>, ms: impl Fn(&T) -> i64, lo: i64, hi: i64, dropped: &mut usize) -> Vec<(&'a T, OffsetDateTime)> {
    let mut out = vec![];
    for it in items {
        match ts_in(ms(it), lo, hi) {
            Some(t) => out.push((it, t)),
            None => *dropped += 1,
        }
    }
    out
}

/// 样本所在的天没有分区（agent 补传几天前的数据）时按需补建；建不出来的天丢弃其样本。
async fn with_partitions<T>(st: &AppState, table: &str, mut items: Vec<(T, OffsetDateTime)>, today: Date, dropped: &mut usize) -> Vec<(T, OffsetDateTime)> {
    // 昨天到明天的分区常规维护一定建好了，只检查窗口里更早/更晚的天
    let odd: BTreeSet<Date> = items
        .iter()
        .map(|(_, t)| t.date())
        .filter(|d| *d < today - time::Duration::days(1) || *d > today + time::Duration::days(1))
        .collect();
    if odd.is_empty() {
        return items;
    }
    let failed = crate::db::ensure_partitions(&st.db, table, &odd).await;
    if !failed.is_empty() {
        let before = items.len();
        items.retain(|(_, t)| !failed.contains(&t.date()));
        *dropped += before - items.len();
    }
    items
}

/// 丢弃样本的告警日志：每台机器每分钟最多一条
fn warn_dropped(machine: Uuid, dropped: usize, seq: u64) {
    if dropped == 0 {
        return;
    }
    static LAST: OnceLock<Mutex<HashMap<Uuid, Instant>>> = OnceLock::new();
    let mut m = LAST.get_or_init(Default::default).lock().unwrap();
    if m.get(&machine).is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
        m.insert(machine, Instant::now());
        tracing::warn!(%machine, seq, dropped, "上报里有时间戳超出可收范围（过期或在未来）的样本，已丢弃");
    }
}

fn hour_start(t: OffsetDateTime) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(t.unix_timestamp().div_euclid(3600) * 3600).unwrap_or(t)
}

async fn report(State(st): State<AppState>, h: HeaderMap, ApiJson(r): ApiJson<Report>) -> ApiResult<Json<ReportAck>> {
    let a = authenticate(&st, &h).await?;
    let now = OffsetDateTime::now_utc();
    let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as i64;
    let hi = now_ms + DAY_MS;
    let raw_lo = now_ms - st.cfg.retention.raw_days as i64 * DAY_MS;
    let acc_lo = now_ms - st.cfg.retention.access_days as i64 * DAY_MS;
    let today = now.date();
    let mut dropped = 0usize;

    // ── 入库前整理：时间窗口过滤、补建分区。单条坏样本绝不能让整份上报失败 ──
    let host = windowed(r.host.iter(), |s| s.ts_ms, raw_lo, hi, &mut dropped);
    let host = with_partitions(&st, "host_metric", host, today, &mut dropped).await;
    let (mut es, mut ps, mut ac) = (vec![], vec![], vec![]);
    // 线路机数据：只收属于这台线路机的入口
    if a.kind == "relay" && (!r.entries.is_empty() || !r.probes.is_empty() || !r.access.is_empty()) {
        let mine: Vec<(Uuid,)> = sqlx::query_as("select id from entry where relay_id = $1").bind(a.machine_id).fetch_all(&st.db).await?;
        let mine: std::collections::HashSet<Uuid> = mine.into_iter().map(|(e,)| e).collect();
        es = windowed(r.entries.iter().filter(|e| mine.contains(&e.entry_id)), |e| e.ts_ms, raw_lo, hi, &mut dropped);
        es = with_partitions(&st, "entry_traffic", es, today, &mut dropped).await;
        ps = windowed(r.probes.iter().filter(|p| mine.contains(&p.entry_id)), |p| p.ts_ms, raw_lo, hi, &mut dropped);
        ps = with_partitions(&st, "probe_result", ps, today, &mut dropped).await;
        ac = windowed(r.access.iter().filter(|x| mine.contains(&x.entry_id)).take(5000), |x| x.ts_ms, acc_lo, hi, &mut dropped);
        ac = with_partitions(&st, "access_event", ac, today, &mut dropped).await;
    }
    warn_dropped(a.machine_id, dropped, r.seq);

    // 有新原始数据的整点小时：标脏，小时汇总补算。当前小时的汇总循环每轮都会算，不用标。
    let cur_hour = hour_start(now);
    let dirty: Vec<OffsetDateTime> = host
        .iter()
        .map(|(_, t)| *t)
        .chain(es.iter().map(|(_, t)| *t))
        .chain(ps.iter().map(|(_, t)| *t))
        .map(hour_start)
        .filter(|h| *h < cur_hour)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut tx = st.db.begin().await?;
    let last: Option<(i64, Option<String>)> =
        sqlx::query_as("select last_seq, boot_id from agent_state where machine_id = $1 for update").bind(a.machine_id).fetch_optional(&mut *tx).await?;
    // 去重：同一份数据重发（seq 不增）直接确认；但这仍说明 agent 活着，要刷新最近上报时间
    if let Some((seq, _)) = &last {
        if r.seq as i64 <= *seq {
            sqlx::query("update agent_state set last_report_at = now(), last_seq = greatest(last_seq, $2) where machine_id = $1")
                .bind(a.machine_id)
                .bind(r.seq as i64)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(Json(ReportAck { accepted_seq: *seq as u64, interval_s: 10 }));
        }
    }
    let info = r.info.as_ref().map(|i| serde_json::to_value(i).unwrap_or_default());
    let xray = r.xray.as_ref().map(|x| serde_json::to_value(x).unwrap_or_default());
    let agent_health = r.agent_health.as_ref().map(|h| serde_json::to_value(h).unwrap_or_default());
    // info 是合并而不是覆盖：只报系统信息时不能把之前记下的 xray / agent 健康状态冲掉
    sqlx::query(
        "insert into agent_state (machine_id, agent_version, boot_id, last_seq, last_report_at, info)
         values ($1, $2, $3, $4, now(), coalesce($5, '{}'::jsonb) || jsonb_strip_nulls(jsonb_build_object('xray', $6::jsonb, 'agent_health', $7::jsonb)))
         on conflict (machine_id) do update set agent_version = excluded.agent_version, boot_id = excluded.boot_id,
           last_seq = greatest(agent_state.last_seq, excluded.last_seq), last_report_at = now(),
           info = agent_state.info || coalesce($5, '{}'::jsonb) || jsonb_strip_nulls(jsonb_build_object('xray', $6::jsonb, 'agent_health', $7::jsonb))",
    )
    .bind(a.machine_id)
    .bind(&r.agent_version)
    .bind(&r.boot_id)
    .bind(r.seq as i64)
    .bind(info)
    .bind(xray)
    .bind(agent_health)
    .execute(&mut *tx)
    .await?;

    if !host.is_empty() {
        let n = host.len();
        let (mut t, mut cpu, mut l1, mut mu, mut mt, mut du, mut dt, mut rx, mut txb, mut tc) = (
            Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n),
            Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n),
        );
        for (s, at) in &host {
            t.push(*at);
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

    if !es.is_empty() {
        sqlx::query(
            "insert into entry_traffic (entry_id, ts, up_bytes, down_bytes, conns)
             select * from unnest($1::uuid[], $2::timestamptz[], $3::bigint[], $4::bigint[], $5::int[])",
        )
        .bind(es.iter().map(|(e, _)| e.entry_id).collect::<Vec<_>>())
        .bind(es.iter().map(|(_, t)| *t).collect::<Vec<_>>())
        .bind(es.iter().map(|(e, _)| e.up_bytes as i64).collect::<Vec<_>>())
        .bind(es.iter().map(|(e, _)| e.down_bytes as i64).collect::<Vec<_>>())
        .bind(es.iter().map(|(e, _)| e.conns as i32).collect::<Vec<_>>())
        .execute(&mut *tx)
        .await?;
    }
    if !ps.is_empty() {
        sqlx::query(
            "insert into probe_result (entry_id, ts, kind, ok, latency_ms, detail)
             select * from unnest($1::uuid[], $2::timestamptz[], $3::text[], $4::bool[], $5::real[], $6::text[])",
        )
        .bind(ps.iter().map(|(p, _)| p.entry_id).collect::<Vec<_>>())
        .bind(ps.iter().map(|(_, t)| *t).collect::<Vec<_>>())
        .bind(ps.iter().map(|(p, _)| serde_json::to_value(p.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()).collect::<Vec<_>>())
        .bind(ps.iter().map(|(p, _)| p.ok).collect::<Vec<_>>())
        .bind(ps.iter().map(|(p, _)| p.latency_ms).collect::<Vec<_>>())
        .bind(ps.iter().map(|(p, _)| p.detail.clone()).collect::<Vec<_>>())
        .execute(&mut *tx)
        .await?;
    }
    if !ac.is_empty() {
        sqlx::query(
            "insert into access_event (entry_id, ts, src_ip, dest_host, dest_port, network, blocked)
             select * from unnest($1::uuid[], $2::timestamptz[], $3::inet[], $4::text[], $5::int[], $6::text[], $7::bool[])",
        )
        .bind(ac.iter().map(|(x, _)| x.entry_id).collect::<Vec<_>>())
        .bind(ac.iter().map(|(_, t)| *t).collect::<Vec<_>>())
        .bind(ac.iter().map(|(x, _)| x.src_ip.map(ipnetwork::IpNetwork::from)).collect::<Vec<_>>())
        .bind(ac.iter().map(|(x, _)| x.dest_host.chars().take(253).collect::<String>()).collect::<Vec<_>>())
        .bind(ac.iter().map(|(x, _)| x.dest_port.map(|p| p as i32)).collect::<Vec<_>>())
        .bind(ac.iter().map(|(x, _)| x.network.clone()).collect::<Vec<_>>())
        .bind(ac.iter().map(|(x, _)| x.blocked).collect::<Vec<_>>())
        .execute(&mut *tx)
        .await?;
    }
    if !dirty.is_empty() {
        sqlx::query(
            "insert into rollup_dirty (hour, touched_at) select t.h, clock_timestamp() from unnest($1::timestamptz[]) as t(h)
             on conflict (hour) do update set touched_at = excluded.touched_at",
        )
        .bind(&dirty)
        .execute(&mut *tx)
        .await?;
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
        // 用「不等于」而不是「大于」：数据库回滚/恢复后代数可能变小，agent 手里的代数反而更大
        if g != after {
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
