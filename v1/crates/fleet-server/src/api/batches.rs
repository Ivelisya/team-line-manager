//! 客户批次账与客户到期页（迁移 0095，派生口径在 batches.rs）。
//! 批次账是「卖给客户的节点什么时候到期」，与供应商订阅无关。管理接口读要 read、写要 write（写审计）；
//! 每个团队一个只读分享链接（令牌 32 字节随机，库里存 sha256 用来查、SecretBox 密文用来随时再复制链接），
//! 公开接口 `GET /api/v1/public/customer/{令牌}` 不登录（和 client_subs 的 /sub/{令牌} 一样没有 Actor 提取器，登录只在提取器里校验，所以不会被拦）。
//! 客户页不出现 IP、供应商、价格、节点名。

use super::ok;
use crate::auth::{self, Actor, ClientIp};
use crate::batches::{self as bt, Batch, Period};
use crate::crypto::{random_token, sha256};
use crate::db::{audit, new_id, Tx};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::meta;
use crate::state::AppState;
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

// ── 读取 ──────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct BatchRow {
    id: Uuid,
    seq: i32,
    region: String,
    note: String,
}

#[derive(sqlx::FromRow)]
struct PeriodRow {
    id: Uuid,
    batch_id: Uuid,
    start_date: Date,
    end_date: Date,
    months: i32,
    count: i32,
}

fn assemble(rows: Vec<BatchRow>, periods: Vec<PeriodRow>) -> Vec<Batch> {
    rows.into_iter()
        .map(|b| Batch {
            periods: periods
                .iter()
                .filter(|p| p.batch_id == b.id)
                .map(|p| Period { id: p.id, start: p.start_date, end: p.end_date, months: p.months, count: p.count })
                .collect(),
            id: b.id,
            seq: b.seq,
            region: b.region,
            note: b.note,
        })
        .collect()
}

/// 团队的全部批次（含已到期），按 seq 升序，段按起点升序
async fn load_team(db: &mut PgConnection, team: Uuid) -> ApiResult<Vec<Batch>> {
    let rows: Vec<BatchRow> = sqlx::query_as("select id, seq, region, note from customer_batch where team_id = $1 order by seq").bind(team).fetch_all(&mut *db).await?;
    let periods: Vec<PeriodRow> = sqlx::query_as(
        "select p.id, p.batch_id, p.start_date, p.end_date, p.months, p.count from customer_batch_period p join customer_batch b on b.id = p.batch_id
         where b.team_id = $1 order by p.start_date, p.created_at, p.id",
    )
    .bind(team)
    .fetch_all(&mut *db)
    .await?;
    Ok(assemble(rows, periods))
}

/// 一个批次与它的段，同时给出所属团队
async fn load_one(db: &mut PgConnection, id: Uuid) -> ApiResult<(Uuid, Batch)> {
    let row: Option<(Uuid, i32, String, String, Uuid)> = sqlx::query_as("select id, seq, region, note, team_id from customer_batch where id = $1").bind(id).fetch_optional(&mut *db).await?;
    let (id, seq, region, note, team) = row.ok_or_else(|| ApiError::not_found("没有这个批次"))?;
    let periods: Vec<PeriodRow> =
        sqlx::query_as("select id, batch_id, start_date, end_date, months, count from customer_batch_period where batch_id = $1 order by start_date, created_at, id")
            .bind(id)
            .fetch_all(&mut *db)
            .await?;
    let b = assemble(vec![BatchRow { id, seq, region, note }], periods).remove(0);
    Ok((team, b))
}

fn batch_json(b: &Batch) -> ApiResult<Value> {
    let d = bt::derive(b, bt::today()).ok_or_else(|| ApiError::internal("批次没有任何一段"))?;
    Ok(bt::admin_json(b, &d))
}

async fn team_name(db: &mut PgConnection, team: Uuid) -> ApiResult<String> {
    sqlx::query_scalar("select name from team where id = $1").bind(team).fetch_optional(&mut *db).await?.ok_or_else(|| ApiError::not_found("没有这个团队"))
}

// ── 校验 ──────────────────────────────────────────────────────────────

fn date_in(s: &str, what: &str) -> ApiResult<Date> { bt::parse_date(s).ok_or_else(|| ApiError::bad_request(format!("{what}要写成 2026-10-31 这样的日期"))) }

fn check_region(s: &str) -> ApiResult<String> {
    let s = s.trim();
    if s.is_empty() || s.chars().count() > 20 {
        return Err(ApiError::bad_request("地区要 1–20 个字，如 美区、墨西哥"));
    }
    Ok(s.to_string())
}

fn check_note(s: &str) -> ApiResult<String> {
    let s = s.trim();
    if s.chars().count() > 60 {
        return Err(ApiError::bad_request("说明最多 60 个字"));
    }
    Ok(s.to_string())
}

fn check_seq(n: i32) -> ApiResult<i32> {
    if !(1..=999).contains(&n) {
        return Err(ApiError::bad_request("批次编号要在 1–999 之间"));
    }
    Ok(n)
}

fn check_months(n: i32) -> ApiResult<i32> {
    if !(1..=36).contains(&n) {
        return Err(ApiError::bad_request("月数要在 1–36 之间"));
    }
    Ok(n)
}

fn check_count(n: i32) -> ApiResult<i32> {
    if !(1..=999).contains(&n) {
        return Err(ApiError::bad_request("节点个数要在 1–999 之间"));
    }
    Ok(n)
}

fn check_range(start: Date, end: Date) -> ApiResult<()> {
    if end <= start {
        return Err(ApiError::bad_request(format!("到期日 {} 要晚于开始日 {}", bt::fmt_date(end), bt::fmt_date(start))));
    }
    Ok(())
}

/// 开始日加 months 个日历月（月底对齐）
fn plus_months(start: Date, months: i32) -> ApiResult<Date> { meta::add_months(start, i64::from(months)).ok_or_else(|| ApiError::bad_request("日期超出范围")) }

fn span(p: &Period) -> String { format!("{} → {}", bt::fmt_date(p.start), bt::fmt_date(p.end)) }

/// 同一批次的段不得重叠（首尾相接可以）：409 写明哪两段
fn check_no_overlap(b: &Batch) -> ApiResult<()> {
    for (i, a) in b.periods.iter().enumerate() {
        for c in &b.periods[i + 1..] {
            if bt::overlaps(a, c) {
                return Err(ApiError::conflict(format!("第{:02}批里有两段日期重叠：{} 与 {}", b.seq, span(a), span(c))));
            }
        }
    }
    Ok(())
}

// ── 审计 ──────────────────────────────────────────────────────────────

/// 批次类审计：detail 带团队、编号、地区与一句话 what（audit.rs 的摘要据此显示）
async fn log_batch(tx: &mut Tx<'_>, a: &Actor, action: &str, team: &str, b: &Batch, what: String) -> ApiResult<()> {
    audit(tx, &a.name, action, "customer_batch", Some(b.id), json!({"team": team, "seq": b.seq, "region": b.region, "what": what})).await?;
    Ok(())
}

// ── 管理：批次 ────────────────────────────────────────────────────────

/// `GET /teams/{id}/batches`：团队的批次（含已到期）与客户页链接状态
pub async fn list(_a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut conn = st.db.acquire().await?;
    let name = team_name(&mut conn, team).await?;
    let today = bt::today();
    let mut batches = vec![];
    for b in load_team(&mut conn, team).await? {
        if let Some(d) = bt::derive(&b, today) {
            batches.push(bt::admin_json(&b, &d));
        }
    }
    let page = page_json(&st, &mut conn, team).await?;
    Ok(ok(json!({"team": {"id": team, "name": name}, "today": bt::fmt_date(today), "page": page, "batches": batches})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateIn {
    region: String,
    start_date: String,
    months: i32,
    count: i32,
    end_date: Option<String>,
    note: Option<String>,
    seq: Option<i32>,
}

/// `POST /teams/{id}/batches`：建批次与首段。end_date 默认 = 开始日加 months 个日历月；seq 默认 = 团队内最大 + 1，冲突 409
pub async fn create(a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>, ApiJson(b): ApiJson<CreateIn>) -> ApiResult<Json<Value>> {
    let region = check_region(&b.region)?;
    let note = check_note(b.note.as_deref().unwrap_or(""))?;
    let (months, count) = (check_months(b.months)?, check_count(b.count)?);
    let start = date_in(&b.start_date, "开始日期")?;
    let end = match &b.end_date {
        Some(e) => date_in(e, "到期日期")?,
        None => plus_months(start, months)?,
    };
    check_range(start, end)?;
    let mut tx = st.db.begin().await?;
    // 锁团队行：并发新建同一团队的批次时「最大编号 + 1」不会取到同一个
    let tname: String = sqlx::query_scalar("select name from team where id = $1 for no key update")
        .bind(team)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个团队"))?;
    let seq = match b.seq {
        Some(s) => check_seq(s)?,
        None => {
            let next: i32 = sqlx::query_scalar("select coalesce(max(seq), 0) + 1 from customer_batch where team_id = $1").bind(team).fetch_one(&mut *tx).await?;
            if next > 999 {
                return Err(ApiError::bad_request("这个团队的批次编号已经用到 999，请自己指定一个空闲编号"));
            }
            next
        }
    };
    let id = new_id();
    sqlx::query("insert into customer_batch (id, team_id, seq, region, note) values ($1, $2, $3, $4, $5)").bind(id).bind(team).bind(seq).bind(&region).bind(&note).execute(&mut *tx).await?;
    sqlx::query("insert into customer_batch_period (id, batch_id, start_date, end_date, months, count) values ($1, $2, $3, $4, $5, $6)")
        .bind(new_id())
        .bind(id)
        .bind(start)
        .bind(end)
        .bind(months)
        .bind(count)
        .execute(&mut *tx)
        .await?;
    let (_, batch) = load_one(&mut tx, id).await?;
    log_batch(&mut tx, &a, "customer_batch.create", &tname, &batch, format!("{} 至 {}，{} 个 × {} 月", bt::fmt_date(start), bt::fmt_date(end), count, months)).await?;
    tx.commit().await?;
    Ok(ok(batch_json(&batch)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchIn {
    region: Option<String>,
    note: Option<String>,
    seq: Option<i32>,
}

/// `PATCH /batches/{id}` `{region?, note?, seq?}`
pub async fn patch(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<PatchIn>) -> ApiResult<Json<Value>> {
    let region = b.region.as_deref().map(check_region).transpose()?;
    let note = b.note.as_deref().map(check_note).transpose()?;
    let seq = b.seq.map(check_seq).transpose()?;
    let mut tx = st.db.begin().await?;
    lock_batch(&mut tx, id).await?;
    let (team, old) = load_one(&mut tx, id).await?;
    sqlx::query("update customer_batch set region = coalesce($2, region), note = coalesce($3, note), seq = coalesce($4, seq), updated_at = now() where id = $1")
        .bind(id)
        .bind(&region)
        .bind(&note)
        .bind(seq)
        .execute(&mut *tx)
        .await?;
    let (_, now) = load_one(&mut tx, id).await?;
    let mut what = vec![];
    if old.seq != now.seq {
        what.push(format!("编号 {:02} → {:02}", old.seq, now.seq));
    }
    if old.region != now.region {
        what.push(format!("地区 {} → {}", old.region, now.region));
    }
    if old.note != now.note {
        what.push("改了说明".to_string());
    }
    let tname = team_name(&mut tx, team).await?;
    log_batch(&mut tx, &a, "customer_batch.update", &tname, &now, if what.is_empty() { "没有改动".into() } else { what.join("，") }).await?;
    tx.commit().await?;
    Ok(ok(batch_json(&now)?))
}

/// 批次行加锁：同一批次的续期、改段、删段互相排队，重叠校验看到的是最新的段
async fn lock_batch(tx: &mut Tx<'_>, id: Uuid) -> ApiResult<()> {
    let n: Option<Uuid> = sqlx::query_scalar("select id from customer_batch where id = $1 for update").bind(id).fetch_optional(&mut **tx).await?;
    n.map(|_| ()).ok_or_else(|| ApiError::not_found("没有这个批次"))
}

/// `DELETE /batches/{id}`
pub async fn delete(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    lock_batch(&mut tx, id).await?;
    let (team, b) = load_one(&mut tx, id).await?;
    let tname = team_name(&mut tx, team).await?;
    sqlx::query("delete from customer_batch where id = $1").bind(id).execute(&mut *tx).await?;
    log_batch(&mut tx, &a, "customer_batch.delete", &tname, &b, format!("连同 {} 段记录", b.periods.len())).await?;
    tx.commit().await?;
    Ok(ok(json!({"id": id})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenewIn {
    months: i32,
    count: Option<i32>,
    end_date: Option<String>,
}

/// `POST /batches/{id}/renew`：追加一段，起点 = 现在的到期日，个数默认 = 现在的个数，终点默认 = 起点加 months 个日历月
pub async fn renew(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<RenewIn>) -> ApiResult<Json<Value>> {
    let months = check_months(b.months)?;
    let count = b.count.map(check_count).transpose()?;
    let end = b.end_date.as_deref().map(|e| date_in(e, "到期日期")).transpose()?;
    let mut tx = st.db.begin().await?;
    lock_batch(&mut tx, id).await?;
    let (team, cur) = load_one(&mut tx, id).await?;
    let d = bt::derive(&cur, bt::today()).ok_or_else(|| ApiError::internal("批次没有任何一段"))?;
    let (start, count) = (d.end, count.unwrap_or(d.count));
    let end = match end {
        Some(e) => e,
        None => plus_months(start, months)?,
    };
    check_range(start, end)?;
    sqlx::query("insert into customer_batch_period (id, batch_id, start_date, end_date, months, count) values ($1, $2, $3, $4, $5, $6)")
        .bind(new_id())
        .bind(id)
        .bind(start)
        .bind(end)
        .bind(months)
        .bind(count)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update customer_batch set updated_at = now() where id = $1").bind(id).execute(&mut *tx).await?;
    let (_, now) = load_one(&mut tx, id).await?;
    let tname = team_name(&mut tx, team).await?;
    log_batch(&mut tx, &a, "customer_batch.renew", &tname, &now, format!("续期 {} 月，{} 至 {}，{} 个", months, bt::fmt_date(start), bt::fmt_date(end), count)).await?;
    tx.commit().await?;
    Ok(ok(batch_json(&now)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeriodIn {
    start_date: Option<String>,
    end_date: Option<String>,
    months: Option<i32>,
    count: Option<i32>,
}

/// `PATCH /batch-periods/{id}`：改一段的日期、月数、个数；改完同批次的段不能重叠
pub async fn patch_period(a: Actor, State(st): State<AppState>, ApiPath(pid): ApiPath<Uuid>, ApiJson(b): ApiJson<PeriodIn>) -> ApiResult<Json<Value>> {
    let start = b.start_date.as_deref().map(|s| date_in(s, "开始日期")).transpose()?;
    let end = b.end_date.as_deref().map(|s| date_in(s, "到期日期")).transpose()?;
    let months = b.months.map(check_months).transpose()?;
    let count = b.count.map(check_count).transpose()?;
    let mut tx = st.db.begin().await?;
    let bid = period_batch(&mut tx, pid).await?;
    lock_batch(&mut tx, bid).await?;
    let (team, mut cur) = load_one(&mut tx, bid).await?;
    let p = cur.periods.iter_mut().find(|p| p.id == pid).ok_or_else(|| ApiError::not_found("没有这一段"))?;
    let before = span(p);
    p.start = start.unwrap_or(p.start);
    p.end = end.unwrap_or(p.end);
    p.months = months.unwrap_or(p.months);
    p.count = count.unwrap_or(p.count);
    check_range(p.start, p.end)?;
    let p = p.clone();
    check_no_overlap(&cur)?;
    sqlx::query("update customer_batch_period set start_date = $2, end_date = $3, months = $4, count = $5 where id = $1")
        .bind(pid)
        .bind(p.start)
        .bind(p.end)
        .bind(p.months)
        .bind(p.count)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update customer_batch set updated_at = now() where id = $1").bind(bid).execute(&mut *tx).await?;
    let (_, now) = load_one(&mut tx, bid).await?;
    let tname = team_name(&mut tx, team).await?;
    log_batch(&mut tx, &a, "customer_batch.period_update", &tname, &now, format!("改一段：{before} → {}，{} 个 × {} 月", span(&p), p.count, p.months)).await?;
    tx.commit().await?;
    Ok(ok(batch_json(&now)?))
}

async fn period_batch(tx: &mut Tx<'_>, pid: Uuid) -> ApiResult<Uuid> {
    let bid: Option<Uuid> = sqlx::query_scalar("select batch_id from customer_batch_period where id = $1").bind(pid).fetch_optional(&mut **tx).await?;
    bid.ok_or_else(|| ApiError::not_found("没有这一段"))
}

/// `DELETE /batch-periods/{id}`：批次只剩这一段时 409
pub async fn delete_period(a: Actor, State(st): State<AppState>, ApiPath(pid): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let bid = period_batch(&mut tx, pid).await?;
    lock_batch(&mut tx, bid).await?;
    let (team, cur) = load_one(&mut tx, bid).await?;
    let gone = cur.periods.iter().find(|p| p.id == pid).ok_or_else(|| ApiError::not_found("没有这一段"))?.clone();
    if cur.periods.len() <= 1 {
        return Err(ApiError::conflict("只剩这一段，要删就删整个批次"));
    }
    sqlx::query("delete from customer_batch_period where id = $1").bind(pid).execute(&mut *tx).await?;
    sqlx::query("update customer_batch set updated_at = now() where id = $1").bind(bid).execute(&mut *tx).await?;
    let (_, now) = load_one(&mut tx, bid).await?;
    let tname = team_name(&mut tx, team).await?;
    log_batch(&mut tx, &a, "customer_batch.period_delete", &tname, &now, format!("删一段：{}，{} 个 × {} 月", span(&gone), gone.count, gone.months)).await?;
    tx.commit().await?;
    Ok(ok(batch_json(&now)?))
}

// ── 管理：客户页链接 ──────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct PageRow {
    token_enc: Vec<u8>,
    enabled: bool,
    created_at: OffsetDateTime,
    token_at: OffsetDateTime,
    last_view_at: Option<OffsetDateTime>,
    last_view_ip: Option<String>,
}

const PAGE_COLS: &str = "token_enc, enabled, created_at, token_at, last_view_at, host(last_view_ip) last_view_ip";

fn link(st: &AppState, token: &str) -> String { format!("{}/c/{token}", st.cfg.public_url.trim_end_matches('/')) }

fn page_of(st: &AppState, r: &PageRow) -> Value {
    // 密钥换了解不开时不报错（批次照常能看），链接给 null，换新链接即可
    let token = st.secrets.open_str(&r.token_enc).map_err(|e| tracing::warn!("客户页令牌解密失败：{e:#}")).ok();
    json!({
        "enabled": r.enabled, "link": token.map(|t| link(st, &t)),
        "created_at": meta::fmt_ts(r.created_at), "token_at": meta::fmt_ts(r.token_at),
        "last_view_at": r.last_view_at.map(meta::fmt_ts), "last_view_ip": r.last_view_ip,
    })
}

/// 团队的客户页链接状态；还没建过是 null
async fn page_json(st: &AppState, db: &mut PgConnection, team: Uuid) -> ApiResult<Value> {
    let r: Option<PageRow> = sqlx::query_as(&format!("select {PAGE_COLS} from customer_page where team_id = $1")).bind(team).fetch_optional(&mut *db).await?;
    Ok(r.map(|r| page_of(st, &r)).unwrap_or(Value::Null))
}

/// `POST /teams/{id}/customer-page`：没有就建，有就换新令牌（旧链接立即失效；启用 / 停用状态不变）
pub async fn page_create(a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let token = random_token(32);
    let mut tx = st.db.begin().await?;
    let tname: String = sqlx::query_scalar("select name from team where id = $1 for no key update")
        .bind(team)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个团队"))?;
    let had: bool = sqlx::query_scalar("select exists (select 1 from customer_page where team_id = $1)").bind(team).fetch_one(&mut *tx).await?;
    // 换新令牌时最近查看清空：看得出客户有没有换上新链接（同客户端订阅的轮换）
    sqlx::query(
        "insert into customer_page (team_id, token_hash, token_enc) values ($1, $2, $3)
         on conflict (team_id) do update set token_hash = excluded.token_hash, token_enc = excluded.token_enc, token_at = now(), last_view_at = null, last_view_ip = null",
    )
    .bind(team)
    .bind(sha256(token.as_bytes()))
    .bind(st.secrets.seal_str(&token))
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, &a.name, if had { "customer_page.rotate" } else { "customer_page.create" }, "team", Some(team), json!({"team": tname})).await?;
    let page = page_json(&st, &mut tx, team).await?;
    tx.commit().await?;
    Ok(ok(page))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageIn {
    enabled: bool,
}

/// `PATCH /teams/{id}/customer-page` `{enabled}`：停用后公开接口回 404，令牌不变
pub async fn page_patch(a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>, ApiJson(b): ApiJson<PageIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let tname = team_name(&mut tx, team).await?;
    let n = sqlx::query("update customer_page set enabled = $2 where team_id = $1").bind(team).bind(b.enabled).execute(&mut *tx).await?.rows_affected();
    if n == 0 {
        return Err(ApiError::not_found("这个团队还没有客户页链接"));
    }
    audit(&mut tx, &a.name, if b.enabled { "customer_page.enable" } else { "customer_page.disable" }, "team", Some(team), json!({"team": tname})).await?;
    let page = page_json(&st, &mut tx, team).await?;
    tx.commit().await?;
    Ok(ok(page))
}

/// `DELETE /teams/{id}/customer-page`
pub async fn page_delete(a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let tname = team_name(&mut tx, team).await?;
    let n = sqlx::query("delete from customer_page where team_id = $1").bind(team).execute(&mut *tx).await?.rows_affected();
    if n == 0 {
        return Err(ApiError::not_found("这个团队还没有客户页链接"));
    }
    audit(&mut tx, &a.name, "customer_page.delete", "team", Some(team), json!({"team": tname})).await?;
    tx.commit().await?;
    Ok(ok(json!({"team_id": team})))
}

// ── 公开（不登录）─────────────────────────────────────────────────────

/// `GET /api/v1/public/customer/{令牌}`：令牌不对或已停用一律 404（不区分）。令牌按 sha256 查库；
/// 和订阅拉取一样，不对的记一次失败、与登录共用按来源的限速（键按令牌哈希分开，别人猜错的不会把对的令牌一起封掉）。
/// 最近查看时间与来源 10 分钟内只写一次。X-Robots-Tag 由 lib.rs 的响应头中间件统一加
pub async fn public_view(State(st): State<AppState>, ClientIp(ip): ClientIp, ApiPath(token): ApiPath<String>) -> ApiResult<Response> {
    let hash = sha256(token.as_bytes());
    let key = format!("cust:{}", &hex::encode(&hash)[..16]);
    let attempt = auth::begin_attempt(&st, &key, ip.map(auth::ip_group)).await.map_err(|e| {
        if e.status == StatusCode::TOO_MANY_REQUESTS {
            ApiError { message: "访问次数过多，请稍后再试".into(), ..e }
        } else {
            e
        }
    })?;
    let team: Option<(Uuid, String)> =
        sqlx::query_as("select p.team_id, t.name from customer_page p join team t on t.id = p.team_id where p.token_hash = $1 and p.enabled").bind(&hash).fetch_optional(&st.db).await?;
    let (team, name) = team.ok_or_else(|| ApiError::not_found("链接不对或已停用"))?;
    auth::forget_attempt(&st, attempt).await?;
    sqlx::query(
        "update customer_page set last_view_at = now(), last_view_ip = $2
         where team_id = $1 and (last_view_at is null or last_view_at < now() - interval '10 minutes')",
    )
    .bind(team)
    .bind(ip.map(ipnetwork::IpNetwork::from))
    .execute(&st.db)
    .await?;
    let batches = load_team(&mut *st.db.acquire().await?, team).await?;
    let mut r = ok(bt::public_page(&name, bt::today(), &batches)).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}
