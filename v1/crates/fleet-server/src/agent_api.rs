//! `/agent/v1/*`：边缘上报、期望状态长轮询、应用回报；`/echo` 回显来源 IP。

use crate::crypto::sha256;
use crate::db::floor_hour;
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
use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock};
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
        // 探测点（令牌是探测点自己的，见 probe_points/agent.rs）
        .merge(crate::probe_points::agent::router())
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
    // 查令牌与刷新最近使用时间一次往返；最近使用时间一分钟内不重复写（agent 每 10 秒上报一次，没必要每次都改这一行）
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "with a as (select m.id, m.kind::text kind from agent_token t join machine m on m.id = t.machine_id
                    where t.token_hash = $1 and t.revoked_at is null and m.lifecycle <> 'retired'),
              _u as (update agent_token set last_used_at = now()
                     where token_hash = $1 and revoked_at is null and (last_used_at is null or last_used_at < now() - interval '1 minute')
                       and exists (select 1 from a))
         select id, kind from a",
    )
    .bind(sha256(tok.trim().as_bytes()))
    .fetch_optional(&st.db)
    .await
    .map_err(|e| {
        // 库连不上时最先失败的就是这里：记进中控收不到上报的时段（report_gaps::ReportGaps；只认连接断了，池子被挤满不算）
        if crate::db::connection_lost(&e) {
            st.report_gaps.failed(OffsetDateTime::now_utc());
        }
        ingest_error(e)
    })?;
    let (machine_id, kind) = row.ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "token_revoked", "令牌无效或已撤销"))?;
    Ok(AgentId { machine_id, kind })
}

const DAY_MS: i64 = 86_400_000;
/// 样本时间戳最多比中控超前这么久（毫秒）。agent 时钟快得更多时样本丢弃，健康原因 clock_skew（> 60 秒）早已报出
const FUTURE_MS: i64 = 10 * 60_000;

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
    let mut m = lock(LAST.get_or_init(Default::default));
    if m.get(&machine).is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
        m.insert(machine, Instant::now());
        tracing::warn!(%machine, seq, dropped, "上报里有时间戳超出可收范围（过期或在未来）或超出主机样本余额的样本，已丢弃");
    }
}

/// 上报入库失败：数据库错误单独保留，按 SQLSTATE 分类后再回给 agent
enum IngestErr {
    Api(ApiError),
    Db(sqlx::Error),
    /// 超过上报频率：429，带 Retry-After（秒）
    Limited(u64),
}

impl From<ApiError> for IngestErr {
    fn from(e: ApiError) -> Self { Self::Api(e) }
}
impl From<sqlx::Error> for IngestErr {
    fn from(e: sqlx::Error) -> Self { Self::Db(e) }
}

/// agent 只把 4xx 移入 dead/、5xx 一直重试：
/// 数据错误（SQLSTATE 22 类：坏字节 / NUL / 越界；23 类：违反约束）→ 422，这份报告重发也不会成功；
/// 其它（连接池超时、断连、磁盘满、只读…）→ 503，中控恢复后原样重发
fn ingest_error(e: sqlx::Error) -> ApiError {
    let class = e.as_database_error().and_then(|d| d.code()).map(|c| c.chars().take(2).collect::<String>());
    match class.as_deref() {
        Some("22" | "23") => ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "bad_report", format!("上报数据无法入库，已拒收：{e}")),
        _ => ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", format!("上报入库失败（中控暂时不可用）：{e}")),
    }
}

/// 每台机器的上报令牌桶：正常 10 s 一份，但积压补传（一轮最多 200 份）、访问日志追赶（每份最多 2000 条）会连发。
/// 容量够补完 6 小时离线队列（离线时 60 s 建一份）再加追赶，之后每秒补一份（每秒 2000 条访问记录，远超线路机的量）
const REPORT_BURST: f64 = 600.0;
const REPORT_REFILL_PER_S: f64 = 1.0;
/// 主机样本另记余额：agent 10 秒采一个，这里按两倍速（5 秒一个）补充，容量 24 小时的量。
/// 积压补传最多 6 小时离线队列 + 12 小时内存缓冲，离线期间余额按两倍速回满，正常 agent 扣不完；
/// 一台机器每天最多入库约 0.2 × 86400 + 8640 ≈ 2.6 万条（正常 8640），超出的丢弃（计入 dropped）
const HOST_BURST: f64 = 8640.0;
const HOST_REFILL_PER_S: f64 = 0.2;

struct Bucket {
    tokens: f64,
    at: Instant,
    /// 入库提交过的最大 seq：同一份重发（网络错误、503 后重试）和重复的不再扣
    seq: u64,
    /// 主机样本的余额
    host: f64,
}

static BUCKETS: LazyLock<Mutex<HashMap<Uuid, Bucket>>> = LazyLock::new(Default::default);

/// 本文件的几把锁里只做算术和集合增删，别处 panic 留下的中毒标记不影响数据，照常用；
/// InFlight 的 drop 可能在 panic 展开途中执行，这时再 panic 进程就直接中止了
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> { m.lock().unwrap_or_else(|e| e.into_inner()) }

/// 只看不扣：新的一份（seq 大于提交过的）要有一个令牌，返回按余额能收的主机样本数；扣不到令牌返回还要等几秒。
/// 重发的（seq 不大于提交过的）放行、返回 None：这份已经入过库，调用方只刷新最近上报时间
fn peek_token(m: &mut HashMap<Uuid, Bucket>, machine: Uuid, seq: u64, host: usize, now: Instant) -> Result<Option<usize>, u64> {
    let b = m.entry(machine).or_insert(Bucket { tokens: REPORT_BURST, at: now, seq: 0, host: HOST_BURST });
    let dt = now.saturating_duration_since(b.at).as_secs_f64();
    b.tokens = (b.tokens + dt * REPORT_REFILL_PER_S).min(REPORT_BURST);
    b.host = (b.host + dt * HOST_REFILL_PER_S).min(HOST_BURST);
    b.at = now;
    if seq <= b.seq {
        return Ok(None);
    }
    if b.tokens < 1.0 {
        return Err(((1.0 - b.tokens) / REPORT_REFILL_PER_S).ceil().max(1.0) as u64);
    }
    Ok(Some(host.min(b.host.max(0.0) as usize)))
}

/// 入库提交之后才扣令牌与主机样本余额、推进 seq：事务失败的那份不扣也不推进，
/// 否则 agent 故意让一份带极大 seq 的报告入库失败，之后的每一份都会被当成重发、不扣令牌也不限样本。
/// 同一台机器的上报一次只处理一份（InFlight），看余额到扣之间不会有别的一份插进来，余额扣不成负数
fn charge(m: &mut HashMap<Uuid, Bucket>, machine: Uuid, seq: u64, host: usize) {
    if let Some(b) = m.get_mut(&machine).filter(|b| seq > b.seq) {
        b.tokens -= 1.0;
        b.host -= host as f64;
        b.seq = seq;
    }
}

/// 应用回报的限流（每台线路机）：正常一代一份，失败按退避重发；突发 120 份，之后 2 秒一份
const APPLIED_BURST: f64 = 120.0;
const APPLIED_REFILL_PER_S: f64 = 0.5;
static APPLIED: LazyLock<Mutex<HashMap<Uuid, (f64, Instant)>>> = LazyLock::new(Default::default);

fn take_applied(m: &mut HashMap<Uuid, (f64, Instant)>, relay: Uuid, now: Instant) -> Result<(), u64> {
    let (tokens, at) = m.entry(relay).or_insert((APPLIED_BURST, now));
    *tokens = (*tokens + now.saturating_duration_since(*at).as_secs_f64() * APPLIED_REFILL_PER_S).min(APPLIED_BURST);
    *at = now;
    if *tokens < 1.0 {
        return Err(((1.0 - *tokens) / APPLIED_REFILL_PER_S).ceil().max(1.0) as u64);
    }
    *tokens -= 1.0;
    Ok(())
}

/// 每台机器同时只处理一份上报：令牌入库提交后才扣，同一台机器并发的几份（seq 各不相同）会一起看到余额、都放进来，
/// 限流就形同虚设。agent 本来就一份确认了才发下一份，正常碰不到；碰到的回 429，1 秒后重发。
/// 应用回报不用：它的令牌在看余额时就同步扣掉（take_applied），入库时 on_applied 又锁着线路机行
static INFLIGHT: LazyLock<Mutex<HashSet<Uuid>>> = LazyLock::new(Default::default);

/// 占着「这台机器正在处理上报」的位置，处理完（含出错、连接断开时请求被取消）放掉
struct InFlight(Uuid);

impl InFlight {
    /// 占不到时不能先造出 Self 再丢掉（then_some 是先求值的）：丢掉时的 drop 会在还持有锁时再锁一次（死锁），还会把别人占着的位置删掉
    fn begin(machine: Uuid) -> Option<Self> { lock(&INFLIGHT).insert(machine).then(|| Self(machine)) }
}

impl Drop for InFlight {
    fn drop(&mut self) { lock(&INFLIGHT).remove(&self.0); }
}

/// 429 + Retry-After（秒）
fn too_many(wait: u64, what: &str) -> Response {
    let mut resp = ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", format!("{what}太频繁，{wait} 秒后再发")).into_response();
    resp.headers_mut().insert(axum::http::header::RETRY_AFTER, wait.into());
    resp
}

/// 每份报告的条数上限：agent 自己最多就发这么多（fleet-agent reporter.rs 的 MAX_*_PER_REPORT），多出来的不收
const MAX_HOST: usize = 360;
const MAX_ENTRIES: usize = 3000;
const MAX_PROBES: usize = 1000;
const MAX_ACCESS: usize = 2000;
/// 自由文本（错误信息、系统信息）截到这么多字符
const MAX_TEXT: usize = 512;

/// 截到 n 个字符，并去掉 NUL：PostgreSQL 的 text / jsonb 不收 \0，带一个就让整份上报的事务失败
fn clip(s: &mut String, n: usize) {
    s.retain(|c| c != '\0');
    if let Some((i, _)) = s.char_indices().nth(n) {
        s.truncate(i);
    }
}

/// u64 计数器入 bigint / int：超出的夹到上限，不回绕成负数
fn i64c(v: u64) -> i64 { v.min(i64::MAX as u64) as i64 }
fn i32c(v: u32) -> i32 { v.min(i32::MAX as u32) as i32 }

/// 入库前截断：条数、自由文本（agent 给的字符串都过 clip，顺带去掉 NUL）
fn sanitize(r: &mut Report) {
    r.host.truncate(MAX_HOST);
    r.entries.truncate(MAX_ENTRIES);
    r.probes.truncate(MAX_PROBES);
    r.access.truncate(MAX_ACCESS);
    clip(&mut r.boot_id, 64);
    clip(&mut r.agent_version, 64);
    for p in &mut r.probes {
        if let Some(d) = &mut p.detail {
            clip(d, MAX_TEXT);
        }
    }
    for x in &mut r.access {
        clip(&mut x.network, 8);
        clip(&mut x.dest_host, 253);
    }
    if let Some(i) = &mut r.info {
        for s in [&mut i.hostname, &mut i.kernel, &mut i.os, &mut i.arch] {
            clip(s, MAX_TEXT);
        }
        i.ipv4.truncate(64);
        i.ipv6.truncate(64);
    }
    if let Some(v) = r.xray.as_mut().and_then(|x| x.version.as_mut()) {
        clip(v, 64);
    }
    if let Some(h) = &mut r.agent_health {
        for s in [&mut h.last_apply_error, &mut h.disk_write_error].into_iter().flatten() {
            clip(s, MAX_TEXT);
        }
    }
}

async fn report(State(st): State<AppState>, h: HeaderMap, ApiJson(r): ApiJson<Report>) -> Result<Response, ApiError> {
    let res = ingest(&st, &h, r).await;
    // 中控收不到上报的时段（report_gaps::ReportGaps）：入库了、库不可用没入成
    match &res {
        Ok(_) => {
            // 库刚恢复、记下了一段中断：存进库里，之后马上重启也接着算
            if st.report_gaps.ok(OffsetDateTime::now_utc()) {
                let st = st.clone();
                tokio::spawn(async move {
                    if let Err(e) = st.report_gaps.save(&st.db).await {
                        tracing::warn!("保存中控收不到上报的时段失败：{e}");
                    }
                });
            }
        }
        Err(IngestErr::Db(e)) if crate::db::connection_lost(e) => st.report_gaps.failed(OffsetDateTime::now_utc()),
        _ => {}
    }
    match res {
        Ok(ack) => Ok(ack.into_response()),
        Err(IngestErr::Limited(wait)) => Ok(too_many(wait, "上报")),
        Err(IngestErr::Api(e)) => Err(e),
        Err(IngestErr::Db(e)) => Err(ingest_error(e)),
    }
}

async fn ingest(st: &AppState, h: &HeaderMap, mut r: Report) -> Result<Json<ReportAck>, IngestErr> {
    let a = authenticate(st, h).await?;
    // 到 charge 为止都占着：同一台机器的下一份等这一份扣完令牌再看余额
    let _busy = InFlight::begin(a.machine_id).ok_or(IngestErr::Limited(1))?;
    sanitize(&mut r);
    let now = OffsetDateTime::now_utc();
    let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as i64;
    let hi = now_ms + FUTURE_MS;
    let raw_lo = now_ms - st.cfg.retention.raw_days as i64 * DAY_MS;
    // 访问记录只收原始记录保留期（面板里设的）以内的：更早的分区已经删了
    let acc_lo = now_ms - st.access_retention().raw_days as i64 * DAY_MS;
    let today = now.date();
    let mut dropped = 0usize;

    // ── 入库前整理：时间窗口过滤、补建分区。单条坏样本绝不能让整份上报失败 ──
    let mut host = windowed(r.host.iter(), |s| s.ts_ms, raw_lo, hi, &mut dropped);
    let Some(keep) = peek_token(&mut lock(&BUCKETS), a.machine_id, r.seq, host.len(), Instant::now()).map_err(IngestErr::Limited)? else {
        return Ok(resent(st, a.machine_id).await?);
    };
    // 超出余额的丢最早的：最新的读数要进实时视图
    if keep < host.len() {
        dropped += host.len() - keep;
        host.drain(..host.len() - keep);
    }
    let host = with_partitions(st, "host_metric", host, today, &mut dropped).await;
    let (mut es, mut ps, mut ac) = (vec![], vec![], vec![]);
    // 线路机数据：只收属于这台线路机的入口
    if a.kind == "relay" && (!r.entries.is_empty() || !r.probes.is_empty() || !r.access.is_empty()) {
        let mine: Vec<(Uuid,)> = sqlx::query_as("select id from entry where relay_id = $1").bind(a.machine_id).fetch_all(&st.db).await?;
        let mine: std::collections::HashSet<Uuid> = mine.into_iter().map(|(e,)| e).collect();
        es = windowed(r.entries.iter().filter(|e| mine.contains(&e.entry_id)), |e| e.ts_ms, raw_lo, hi, &mut dropped);
        es = with_partitions(st, "entry_traffic", es, today, &mut dropped).await;
        ps = windowed(r.probes.iter().filter(|p| mine.contains(&p.entry_id)), |p| p.ts_ms, raw_lo, hi, &mut dropped);
        ps = with_partitions(st, "probe_result", ps, today, &mut dropped).await;
        ac = windowed(r.access.iter().filter(|x| mine.contains(&x.entry_id)), |x| x.ts_ms, acc_lo, hi, &mut dropped);
        ac = with_partitions(st, "access_event", ac, today, &mut dropped).await;
    }
    warn_dropped(a.machine_id, dropped, r.seq);

    // 有新原始数据的整点小时：标脏，小时汇总补算。当前小时的汇总循环每轮都会算，不用标。
    // 主机样本连下一小时一起标：下一小时第一个网卡增量的前值可能就是它（多算一次是幂等的）
    let cur_hour = floor_hour(now);
    let dirty: Vec<OffsetDateTime> = host
        .iter()
        .flat_map(|(_, t)| [floor_hour(*t), floor_hour(*t) + time::Duration::hours(1)])
        .chain(es.iter().map(|(_, t)| floor_hour(*t)))
        .chain(ps.iter().map(|(_, t)| floor_hour(*t)))
        .filter(|h| *h < cur_hour)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    // 访问记录单独标脏（access_dirty，由访问汇总循环消费），和遥测的 rollup_dirty 互不干扰
    let access_dirty: Vec<OffsetDateTime> = ac.iter().map(|(_, t)| floor_hour(*t)).filter(|h| *h < cur_hour).collect::<BTreeSet<_>>().into_iter().collect();

    let mut tx = st.db.begin().await?;
    // 主连接池不设语句超时：一条卡住的语句（如插分区表时排在建 / 摘分区的父表锁后面）会让这份一直占着处理位置，
    // 这台机器之后的上报就一直 429。10 秒（加上等连接最多 5 秒，在 agent 的 15 秒超时以内）没完就取消、回 503，agent 退避重发
    sqlx::query("set local statement_timeout = '10s'").execute(&mut *tx).await?;
    let prev: Option<(i64, bool)> =
        sqlx::query_as("select last_seq, coalesce(last_report_at > now() - interval '30 seconds', false) from agent_state where machine_id = $1 for update")
            .bind(a.machine_id)
            .fetch_optional(&mut *tx)
            .await?;
    let last = prev.map(|p| p.0);
    // 去重（中控重启后内存里的 seq 没了，靠库里的）：同一份数据重发（seq 不增）直接确认；但这仍说明 agent 活着，要刷新最近上报时间
    if let Some(seq) = &last {
        if i64c(r.seq) <= *seq {
            sqlx::query("update agent_state set last_report_at = now(), last_seq = greatest(last_seq, $2) where machine_id = $1")
                .bind(a.machine_id)
                .bind(i64c(r.seq))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(Json(ReportAck { accepted_seq: *seq as u64, interval_s: 10 }));
        }
    }
    let info = r.info.as_ref().map(|i| serde_json::to_value(i).unwrap_or_default());
    // Xray 状态记上报它的 agent 版本：升级 agent 时要认新版本自己报的（新进程最先补发的是旧进程退出时落盘的那份，
    // 带的是旧进程的 Xray 状态；之后几份不带 Xray 状态，合并时旧值留着。见 jobs::steps::wait_report）
    let xray = r.xray.as_ref().map(|x| {
        let mut v = serde_json::to_value(x).unwrap_or_default();
        v["agent_version"] = r.agent_version.clone().into();
        v
    });
    let agent_health = r.agent_health.as_ref().map(|h| serde_json::to_value(h).unwrap_or_default());
    // 时钟偏差 = 中控收到时刻 − sent_at_ms（正数 = agent 时钟慢）。sent_at_ms 是建报时刻，积压补发、失败重发的报告会偏大，
    // 只认新鲜的：建报时离线队列为空（之前的报告都已确认，建报一定晚于中控收到上一份）且上一份是 30 秒内收到的，误差不超过 30 秒
    let fresh = r.agent_health.as_ref().is_some_and(|h| h.queue_len == 0) && prev.is_some_and(|p| p.1);
    let skew = fresh.then(|| now_ms.saturating_sub(r.sent_at_ms));
    // info 是合并而不是覆盖：只报系统信息时不能把之前记下的 xray / agent 健康状态 / 时钟偏差冲掉
    sqlx::query(
        "insert into agent_state (machine_id, agent_version, boot_id, last_seq, last_report_at, info)
         values ($1, $2, $3, $4, now(), coalesce($5, '{}'::jsonb) || jsonb_strip_nulls(jsonb_build_object('xray', $6::jsonb, 'agent_health', $7::jsonb, 'clock_skew_ms', $8::bigint)))
         on conflict (machine_id) do update set agent_version = excluded.agent_version, boot_id = excluded.boot_id,
           last_seq = greatest(agent_state.last_seq, excluded.last_seq), last_report_at = now(),
           info = agent_state.info || coalesce($5, '{}'::jsonb) || jsonb_strip_nulls(jsonb_build_object('xray', $6::jsonb, 'agent_health', $7::jsonb, 'clock_skew_ms', $8::bigint))",
    )
    .bind(a.machine_id)
    .bind(&r.agent_version)
    .bind(&r.boot_id)
    .bind(i64c(r.seq))
    .bind(info)
    .bind(xray)
    .bind(agent_health)
    .bind(skew)
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
            mu.push(i64c(s.mem_used));
            mt.push(i64c(s.mem_total));
            du.push(i64c(s.disk_used));
            dt.push(i64c(s.disk_total));
            rx.push(i64c(s.net_rx_bytes));
            txb.push(i64c(s.net_tx_bytes));
            tc.push(i32c(s.tcp_conns));
        }
        sqlx::query(
            "insert into host_metric (machine_id, ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bytes, net_tx_bytes, tcp_conns)
             select $1, * from unnest($2::timestamptz[], $3::real[], $4::real[], $5::bigint[], $6::bigint[], $7::bigint[], $8::bigint[], $9::bigint[], $10::bigint[], $11::int[])
             on conflict do nothing",
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
        .bind(es.iter().map(|(e, _)| i64c(e.up_bytes)).collect::<Vec<_>>())
        .bind(es.iter().map(|(e, _)| i64c(e.down_bytes)).collect::<Vec<_>>())
        .bind(es.iter().map(|(e, _)| i32c(e.conns)).collect::<Vec<_>>())
        .execute(&mut *tx)
        .await?;
    }
    if !ps.is_empty() {
        sqlx::query(
            "insert into probe_result (entry_id, ts, kind, ok, latency_ms, detail, retrans)
             select * from unnest($1::uuid[], $2::timestamptz[], $3::text[], $4::bool[], $5::real[], $6::text[], $7::smallint[])",
        )
        .bind(ps.iter().map(|(p, _)| p.entry_id).collect::<Vec<_>>())
        .bind(ps.iter().map(|(_, t)| *t).collect::<Vec<_>>())
        .bind(ps.iter().map(|(p, _)| serde_json::to_value(p.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()).collect::<Vec<_>>())
        .bind(ps.iter().map(|(p, _)| p.ok).collect::<Vec<_>>())
        .bind(ps.iter().map(|(p, _)| p.latency_ms).collect::<Vec<_>>())
        .bind(ps.iter().map(|(p, _)| p.detail.clone()).collect::<Vec<_>>())
        // 旧 agent 不报（null）；超出 smallint 的按上限记（正常是个位数）
        .bind(ps.iter().map(|(p, _)| p.retrans.map(|r| r.min(i16::MAX as u32) as i16)).collect::<Vec<_>>())
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
        .bind(ac.iter().map(|(x, _)| x.dest_host.clone()).collect::<Vec<_>>())
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
    if !access_dirty.is_empty() {
        sqlx::query(
            "insert into access_dirty (hour, touched_at) select t.h, clock_timestamp() from unnest($1::timestamptz[]) as t(h)
             on conflict (hour) do update set touched_at = excluded.touched_at",
        )
        .bind(&access_dirty)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    charge(&mut lock(&BUCKETS), a.machine_id, r.seq, host.len());
    if last.is_none() {
        st.emit(Event::Machine { id: a.machine_id });
        // 上架中「等待首次上报」的作业
        crate::jobs::wake_for(st, a.machine_id).await;
    }
    // 新读数由 live.rs 合并推送（最多每 3 s 一次），不发失效事件
    st.metrics_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(Json(ReportAck { accepted_seq: r.seq, interval_s: 10 }))
}

/// 本进程已入库过的一份又发来（确认在路上丢了、503 后重试）：不查入口、不补建分区、不进去重事务，只刷新最近上报时间（agent 活着），
/// 确认库里记的最大 seq（不小于这份的）。行一定在：入库提交过才推进内存里的 seq
async fn resent(st: &AppState, machine: Uuid) -> sqlx::Result<Json<ReportAck>> {
    let last: i64 = sqlx::query_scalar("update agent_state set last_report_at = now() where machine_id = $1 returning last_seq")
        .bind(machine)
        .fetch_one(&st.db)
        .await?;
    Ok(Json(ReportAck { accepted_seq: last as u64, interval_s: 10 }))
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

/// 应用回报入库前截断：错误文本进告警摘要、通知和每个浏览器的健康推送；入口条数同上报的上限（不认识的 id 入库时本来就忽略）
fn sanitize_applied(r: &mut ApplyReport) {
    r.entries.truncate(MAX_ENTRIES);
    for s in r.entries.iter_mut().filter_map(|e| e.error.as_mut()).chain(r.error.as_mut()) {
        clip(s, MAX_TEXT);
    }
}

async fn applied(State(st): State<AppState>, h: HeaderMap, ApiJson(mut r): ApiJson<ApplyReport>) -> ApiResult<Response> {
    let a = authenticate(&st, &h).await?;
    if a.kind != "relay" {
        return Err(ApiError::forbidden("只有线路机回报应用结果"));
    }
    if let Err(wait) = take_applied(&mut lock(&APPLIED), a.machine_id, Instant::now()) {
        return Ok(too_many(wait, "应用回报"));
    }
    sanitize_applied(&mut r);
    let changed = entries::on_applied(&st.db, a.machine_id, &r).await?;
    if !r.ok {
        let mut e = r.error.clone().unwrap_or_default();
        clip(&mut e, 120);
        tracing::warn!(relay = %a.machine_id, generation = r.generation, "应用失败：{e}");
    }
    // 同一份回报重发、没变化的不推送（每个浏览器都会重取机器列表），也不叫醒作业：等这台线路机应用 / 摘除的作业
    // 只看已应用代数、应用错误和入口状态，这些没变，叫醒了也是接着等
    if changed {
        crate::jobs::wake_for(&st, a.machine_id).await;
        st.emit(Event::Relay { id: a.machine_id });
        st.emit(Event::Machines);
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// 回显请求方 IP（出口探测用）；取法同登录限速（X-Forwarded-For 最后一项）
pub async fn echo(State(st): State<AppState>, ConnectInfo(addr): ConnectInfo<SocketAddr>, h: HeaderMap) -> String {
    crate::auth::client_ip(&h, Some(addr.ip()), st.cfg.trust_forwarded_for).map(|ip| ip.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 看余额并扣（测试里每份都当入库成功）
    fn take_token(m: &mut HashMap<Uuid, Bucket>, machine: Uuid, seq: u64, host: usize, now: Instant) -> Result<Option<usize>, u64> {
        let n = peek_token(m, machine, seq, host, now)?;
        if let Some(n) = n {
            charge(m, machine, seq, n);
        }
        Ok(n)
    }

    #[test]
    fn one_in_flight_per_machine() {
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let g = InFlight::begin(a).unwrap();
        assert!(InFlight::begin(a).is_none());
        // 别的机器不受影响
        assert!(InFlight::begin(b).is_some());
        drop(g);
        assert!(InFlight::begin(a).is_some());
    }

    #[test]
    fn failed_report_does_not_make_later_ones_free() {
        let (mut m, id, t0) = (HashMap::new(), Uuid::now_v7(), Instant::now());
        // 一份带极大 seq 的报告看过余额、入库失败（没 charge）
        assert_eq!(peek_token(&mut m, id, 1 << 62, 360, t0), Ok(Some(360)));
        // 之后的照常扣令牌
        for seq in 1..=REPORT_BURST as u64 {
            assert!(take_token(&mut m, id, seq, 0, t0).is_ok(), "{seq}");
        }
        assert_eq!(take_token(&mut m, id, REPORT_BURST as u64 + 1, 0, t0), Err(1));
    }

    #[test]
    fn bucket_allows_burst_then_paces_and_never_charges_resends() {
        let (mut m, id, t0) = (HashMap::new(), Uuid::now_v7(), Instant::now());
        for seq in 1..=REPORT_BURST as u64 {
            assert!(take_token(&mut m, id, seq, 0, t0).is_ok(), "{seq}");
        }
        let next = REPORT_BURST as u64 + 1;
        assert_eq!(take_token(&mut m, id, next, 0, t0), Err(1));
        // 重发、重复的（seq 不大于扣过的）认作重发（None），不扣
        assert!(take_token(&mut m, id, next - 1, 0, t0) == Ok(None) && take_token(&mut m, id, 1, 0, t0) == Ok(None));
        // 过一秒补一个
        assert!(take_token(&mut m, id, next, 0, t0 + Duration::from_secs(1)).is_ok());
        assert_eq!(take_token(&mut m, id, next + 1, 0, t0 + Duration::from_secs(1)), Err(1));
        // 别的机器不受影响；正常 10 秒一份（每份一个主机样本）永远扣得到、样本全收
        let other = Uuid::now_v7();
        for k in 0..2000u64 {
            assert_eq!(take_token(&mut m, other, k + 1, 1, t0 + Duration::from_secs(k * 10)), Ok(Some(1)));
        }
    }

    #[test]
    fn host_budget_covers_backlog_but_caps_floods() {
        let (mut m, id, t0) = (HashMap::new(), Uuid::now_v7(), Instant::now());
        // 18 小时积压（6 小时队列 + 12 小时缓冲，10 秒一个）按每份 360 个连发：全收
        let backlog = 18 * 360;
        for k in 0..(backlog / 360) as u64 {
            assert_eq!(take_token(&mut m, id, k + 1, 360, t0), Ok(Some(360)));
        }
        // 之后猛灌：余额（24 小时的量）扣完就只收补充的部分
        let mut got = backlog;
        for k in 0..100u64 {
            got += take_token(&mut m, id, 1000 + k, 360, t0).unwrap().unwrap();
        }
        assert_eq!(got, HOST_BURST as usize);
        // 一小时后补 720 个（两倍于正常采样）
        assert_eq!(take_token(&mut m, id, 2000, 360, t0 + Duration::from_secs(3600)), Ok(Some(360)));
        assert_eq!(take_token(&mut m, id, 2001, 720, t0 + Duration::from_secs(3600)), Ok(Some(360)));
        // 重发的不扣
        assert_eq!(take_token(&mut m, id, 2001, 360, t0 + Duration::from_secs(3600)), Ok(None));
    }

    #[test]
    fn applied_bucket_paces_and_sanitize_clips_errors() {
        let (mut m, id, t0) = (HashMap::new(), Uuid::now_v7(), Instant::now());
        for _ in 0..APPLIED_BURST as u32 {
            assert!(take_applied(&mut m, id, t0).is_ok());
        }
        assert_eq!(take_applied(&mut m, id, t0), Err(2));
        assert!(take_applied(&mut m, id, t0 + Duration::from_secs(2)).is_ok());
        let e = fleet_proto::EntryApply { id: Uuid::nil(), ok: false, error: Some("x".repeat(600)) };
        let mut r = ApplyReport { generation: 1, ok: false, error: Some("错".repeat(100_000)), entries: vec![e; MAX_ENTRIES + 5] };
        sanitize_applied(&mut r);
        assert_eq!(r.entries.len(), MAX_ENTRIES);
        assert!(r.entries.iter().all(|e| e.error.as_ref().unwrap().len() == MAX_TEXT));
        assert_eq!(r.error.unwrap().chars().count(), MAX_TEXT);
    }

    #[test]
    fn sanitize_caps_rows_text_and_counters() {
        let mut r: Report = serde_json::from_value(serde_json::json!({
            "seq": 1, "boot_id": "b", "agent_version": "v", "sent_at_ms": 0,
            "probes": [{"entry_id": Uuid::nil(), "ts_ms": 0, "kind": "tcp", "ok": false, "detail": "错".repeat(5000)}],
            "agent_health": {"last_apply_error": "x".repeat(100_000)},
        }))
        .unwrap();
        let ev = fleet_proto::AccessEvent { entry_id: Uuid::nil(), ts_ms: 0, src_ip: None, dest_host: "a".into(), dest_port: None, network: "tcp".into(), blocked: false };
        r.access = vec![ev; 5000];
        r.boot_id = "a\0b".into();
        r.access[0].dest_host = "x\0.com".into();
        sanitize(&mut r);
        assert_eq!((r.boot_id.as_str(), r.access[0].dest_host.as_str()), ("ab", "x.com"), "NUL 去掉");
        assert_eq!(r.access.len(), MAX_ACCESS);
        assert_eq!(r.probes[0].detail.as_ref().unwrap().chars().count(), MAX_TEXT);
        assert_eq!(r.agent_health.unwrap().last_apply_error.unwrap().len(), MAX_TEXT);
        assert_eq!((i64c(u64::MAX), i64c(5), i32c(u32::MAX)), (i64::MAX, 5, i32::MAX));
    }
}
