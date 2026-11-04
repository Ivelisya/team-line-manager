//! 计费台账（名额：迁移 0097，对账在 ledger.rs；每一期已支付 / 待支付：迁移 0100，口径在 batches.rs）与客户页。
//! 计费名额 = 团队里一个按月计费的节点位置，由对账自动增减。每个节点从开始收费日起每满一个月收一次（一期），每一期只有已支付 / 待支付（默认待支付，手动标记）；
//! 按月看 = 每个节点起点落在这个月的那一期。这里有：看（名额、增减记录、某月清单）、标记已支付 / 改回待支付、改名额（开始收费日、地区、备注、停止 / 恢复）、
//! 手动名额（系统外的节点）、节点设不收费、一次性初始化。读要 read、写要 write（写审计）。
//! 每个团队一个只读分享链接（令牌 32 字节随机，库里存 sha256 用来查、SecretBox 密文用来随时再复制链接），
//! 公开接口 `POST /api/v1/public/customer/{令牌}?month=` 不登录（和 client_subs 的 /sub/{令牌} 一样没有 Actor 提取器，登录只在提取器里校验，所以不会被拦）。
//! 客户页只有某月每个节点的名字、地区、这一期的区间、已付 / 待付，不出现 IP、供应商、价格。

use super::ok;
use crate::auth::{self, Actor, ClientIp};
use crate::batches as bt;
use crate::crypto::{random_token, sha256};
use crate::db::{audit, new_id, Tx};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::ledger::{self, manual_name};
use crate::meta;
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use std::collections::HashMap;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

// ── 读取 ──────────────────────────────────────────────────────────────

const SLOT_COLS: &str = "s.id, s.region, s.start_date, s.end_date, s.holder_id, display_name(hm.kind::text, hm.label, ht.name, hm.number) holder_now,
                         s.holder_name, s.vacated_at, s.manual, s.note, s.vacate_reason";
const SLOT_FROM: &str = "billing_slot s left join machine hm on hm.id = s.holder_id left join team ht on ht.id = hm.team_id";

#[derive(sqlx::FromRow, Clone)]
struct SlotRow {
    id: Uuid,
    region: String,
    start_date: Date,
    end_date: Option<Date>,
    holder_id: Option<Uuid>,
    /// 占用者现在的显示名
    holder_now: Option<String>,
    /// 占用者（空出后是原占用者）最近的显示名快照
    holder_name: Option<String>,
    vacated_at: Option<OffsetDateTime>,
    manual: bool,
    note: String,
    /// 空出 / 已结束的系统名额为什么没有占用者（对账空出时落库）：exempt = 原占用者被设为不计费，left = 离开团队等；其余 null
    vacate_reason: Option<String>,
}

impl SlotRow {
    /// 列表与记录里的名字：占用者现在的名字；手动名额「手动 · 备注」；空出 / 结束的是原占用者当时的名字
    fn name(&self) -> String {
        if self.manual {
            return manual_name(&self.note);
        }
        self.holder_now.clone().or_else(|| self.holder_name.clone()).unwrap_or_else(|| "（已删除的节点）".into())
    }

    fn state(&self, today: Date) -> &'static str { if self.end_date.is_some_and(|e| e <= today) { "ended" } else { "active" } }
}

/// 团队的全部名额（含已结束的）
async fn team_slots(db: &mut PgConnection, team: Uuid) -> ApiResult<Vec<SlotRow>> {
    Ok(sqlx::query_as(&format!("select {SLOT_COLS} from {SLOT_FROM} where s.team_id = $1 order by s.start_date, s.created_at, s.id")).bind(team).fetch_all(&mut *db).await?)
}

async fn one_slot(db: &mut PgConnection, id: Uuid) -> ApiResult<SlotRow> {
    let r: Option<SlotRow> = sqlx::query_as(&format!("select {SLOT_COLS} from {SLOT_FROM} where s.id = $1")).bind(id).fetch_optional(&mut *db).await?;
    r.ok_or_else(|| ApiError::not_found("没有这个计费节点"))
}

fn slot_json(s: &SlotRow, today: Date) -> Value {
    json!({
        "id": s.id, "region": s.region, "start_date": bt::fmt_date(s.start_date),
        "state": s.state(today), "name": s.name(),
        "holder": s.holder_id.map(|id| json!({"id": id, "name": s.holder_now.clone().unwrap_or_default()})),
        "last_holder_name": if s.holder_id.is_none() && !s.manual { s.holder_name.clone() } else { None },
        // 系统名额没有占用者（已结束）的原因，对账结束时落库：exempt = 原占用者被设为不计费（节点还在团队里）；left = 离开团队、退役、进备用或被删；其余 null
        "left_reason": s.vacate_reason,
        "vacated_at": s.vacated_at.map(meta::fmt_ts), "end_date": s.end_date.map(bt::fmt_date), "manual": s.manual, "note": s.note,
    })
}

/// 一个名额的 Slot
async fn slot_out(db: &mut PgConnection, id: Uuid) -> ApiResult<Value> { Ok(slot_json(&one_slot(db, id).await?, bt::today())) }

async fn team_name(db: &mut PgConnection, team: Uuid) -> ApiResult<String> {
    sqlx::query_scalar("select name from team where id = $1").bind(team).fetch_optional(&mut *db).await?.ok_or_else(|| ApiError::not_found("没有这个团队"))
}

#[derive(sqlx::FromRow)]
struct EventRow {
    id: Uuid,
    at: OffsetDateTime,
    kind: String,
    machine_name: Option<String>,
    other_name: Option<String>,
    slot_id: Option<Uuid>,
    detail: Value,
}

fn event_json(e: &EventRow) -> Value {
    json!({
        "id": e.id, "at": meta::fmt_ts(e.at), "kind": e.kind, "machine_name": e.machine_name, "other_name": e.other_name, "slot_id": e.slot_id,
        "text": ledger::event_text(&e.kind, e.machine_name.as_deref(), e.other_name.as_deref(), &e.detail),
    })
}

const EVENT_COLS: &str = "id, at, kind, machine_name, other_name, slot_id, detail";

/// `GET /teams/{id}/batches`：名额（含已结束的）、个数与客户页链接状态
pub async fn list(_a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut conn = st.db.acquire().await?;
    let name = team_name(&mut conn, team).await?;
    let today = bt::today();
    let mut slots = team_slots(&mut conn, team).await?;
    slots.sort_by_key(|s| (s.start_date, s.name()));
    let count = |st: &str| slots.iter().filter(|s| s.state(today) == st).count();
    let page = page_json(&st, &mut conn, team).await?;
    let initialized = ledger::initialized(&st.db).await?;
    Ok(ok(json!({
        "team": {"id": team, "name": name}, "today": bt::fmt_date(today), "initialized": initialized, "page": page,
        "slots": slots.iter().map(|s| slot_json(s, today)).collect::<Vec<_>>(),
        "totals": {"active": count("active"), "ended": count("ended")},
    })))
}

// ── 按月看：每个节点起点落在这个月的那一期，已支付 / 待支付 ──────────────────

#[derive(Deserialize)]
pub struct MonthQ {
    month: Option<String>,
}

/// `?month=2026-11`（默认本月）→ 该月 1 日
fn month_in(m: Option<&str>) -> ApiResult<Date> {
    match m.filter(|m| !m.trim().is_empty()) {
        Some(m) => bt::parse_month(m).ok_or_else(|| ApiError::bad_request("月份要写成 2026-11 这样")),
        None => Ok(bt::this_month()),
    }
}

/// 某月清单的一行：名额、起点落在这个月的那一期 [ps, pe)、已支付时刻（待支付为 None）
struct MonthRow {
    s: SlotRow,
    ps: Date,
    pe: Date,
    paid_at: Option<OffsetDateTime>,
}

/// 某月的清单（按本期生效日、节点名排列，收款状态不影响顺序）与可以翻看的月份
async fn month_rows(db: &mut PgConnection, team: Uuid, month: Date) -> ApiResult<(Vec<Date>, Vec<MonthRow>)> {
    let next = meta::add_months(month, 1).ok_or_else(|| ApiError::bad_request("日期超出范围"))?;
    let slots = team_slots(db, team).await?;
    let paid: HashMap<(Uuid, Date), OffsetDateTime> = sqlx::query_as::<_, (Uuid, Date, OffsetDateTime)>(
        "select p.slot_id, p.period_start, p.paid_at from billing_paid p join billing_slot s on s.id = p.slot_id
         where s.team_id = $1 and p.period_start >= $2 and p.period_start < $3",
    )
    .bind(team)
    .bind(month)
    .bind(next)
    .fetch_all(&mut *db)
    .await?
    .into_iter()
    .map(|(id, ps, at)| ((id, ps), at))
    .collect();
    let months = bt::months(slots.iter().map(|s| s.start_date), bt::this_month());
    let mut rows: Vec<MonthRow> = slots
        .into_iter()
        .filter_map(|s| {
            let (ps, pe) = bt::period_in(s.start_date, s.end_date, month)?;
            let paid_at = paid.get(&(s.id, ps)).copied();
            Some(MonthRow { s, ps, pe, paid_at })
        })
        .collect();
    rows.sort_by_cached_key(|r| (r.ps, r.s.name(), r.s.id));
    Ok((months, rows))
}

fn admin_node(s: &SlotRow, ps: Date, pe: Date, paid_at: Option<OffsetDateTime>, today: Date) -> Value {
    json!({
        "slot_id": s.id, "name": s.name(), "region": s.region, "start_date": bt::fmt_date(s.start_date),
        "period_start": bt::fmt_date(ps), "period_end": bt::fmt_date(pe), "upcoming": ps > today,
        "manual": s.manual, "note": s.note, "holder_id": s.holder_id, "ended": s.state(today) == "ended",
        "paid": paid_at.is_some(), "paid_at": paid_at.map(meta::fmt_ts),
    })
}

/// 所有已开始且待支付的期次，管理端与客户页共用。
async fn outstanding_rows(db: &mut PgConnection, team: Uuid, today: Date) -> ApiResult<Vec<MonthRow>> {
    let slots = team_slots(db, team).await?;
    let paid: std::collections::HashSet<(Uuid, Date)> = sqlx::query_as(
        "select p.slot_id, p.period_start from billing_paid p join billing_slot s on s.id = p.slot_id where s.team_id = $1",
    ).bind(team).fetch_all(&mut *db).await?.into_iter().collect();
    let mut rows = Vec::new();
    for slot in slots {
        for k in 0.. {
            let Some((ps, pe)) = bt::period(slot.start_date, k) else { break };
            if ps > today || slot.end_date.is_some_and(|end| ps >= end) { break; }
            if !paid.contains(&(slot.id, ps)) {
                rows.push(MonthRow { s: slot.clone(), ps, pe, paid_at: None });
            }
        }
    }
    rows.sort_by_cached_key(|r| (r.ps, r.s.name(), r.s.id));
    Ok(rows)
}

pub async fn outstanding(_a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut db = st.db.acquire().await?;
    let name = team_name(&mut db, team).await?;
    let today = bt::today();
    let rows = outstanding_rows(&mut db, team, today).await?;
    let nodes: Vec<_> = rows.iter().map(|r| admin_node(&r.s, r.ps, r.pe, None, today)).collect();
    Ok(ok(json!({"team": {"id": team, "name": name}, "today": bt::fmt_date(today), "nodes": nodes})))
}

/// 管理端的月份清单（GET 与标记后都回它）
async fn month_view(st: &AppState, db: &mut PgConnection, team: Uuid, month: Date) -> ApiResult<Value> {
    let name = team_name(db, team).await?;
    let today = bt::today();
    let (months, rows) = month_rows(db, team, month).await?;
    let page = page_json(st, db, team).await?;
    let initialized = ledger::initialized(&st.db).await?;
    let nodes: Vec<_> = rows.iter().map(|r| admin_node(&r.s, r.ps, r.pe, r.paid_at, today)).collect();
    Ok(json!({
        "team": {"id": team, "name": name}, "today": bt::fmt_date(today), "initialized": initialized, "page": page,
        "month": bt::fmt_month(month), "months": months.into_iter().map(bt::fmt_month).collect::<Vec<_>>(),
        "nodes": nodes,
    }))
}

/// `GET /teams/{id}/billing/month?month=2026-11`（默认本月）：每个节点起点落在这个月的那一期（区间、已支付 / 待支付；按本期生效日、节点名排列）、个数、
/// 可以翻看的月份（最早的开始收费月或本月 → 下个月）、客户页链接状态
pub async fn month(_a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>, ApiQuery(q): ApiQuery<MonthQ>) -> ApiResult<Json<Value>> {
    let month = month_in(q.month.as_deref())?;
    let mut conn = st.db.acquire().await?;
    Ok(ok(month_view(&st, &mut conn, team, month).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaidIn {
    month: String,
    slot_ids: Vec<Uuid>,
    paid: bool,
}

/// `POST /teams/{id}/billing/paid` `{month, slot_ids, paid}`：把已经生效的期次标为已支付，或撤销支付标记。
/// 节点这个月没有要收费的期（不是这个团队的、还没开始、已停止）→ 400，什么都不改。真有变化的记一条 paid / unpaid 增减记录和一条审计 → 该月清单（同 GET）
pub async fn paid(a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>, ApiJson(b): ApiJson<PaidIn>) -> ApiResult<Json<Value>> {
    let month = bt::parse_month(&b.month).ok_or_else(|| ApiError::bad_request("月份要写成 2026-11 这样"))?;
    if b.slot_ids.is_empty() || b.slot_ids.len() > 500 {
        return Err(ApiError::bad_request("一次标记 1–500 个节点"));
    }
    let mut tx = st.db.begin().await?;
    let tname = lock_billing_team(&mut tx, team).await?;
    let (_, rows) = month_rows(&mut tx, team, month).await?;
    let mut ids = vec![];
    let mut starts = vec![];
    let mut wrong = vec![];
    for id in &b.slot_ids {
        match rows.iter().find(|r| r.s.id == *id) {
            Some(r) => {
                ids.push(*id);
                starts.push(r.ps);
            }
            None => wrong.push(id.to_string()[..8].to_string()),
        }
    }
    if !wrong.is_empty() {
        return Err(ApiError::bad_request(format!("{} 月没有要收这些节点的钱（不是这个团队的，或这个月不收费）：{}", u8::from(month.month()), wrong.join("、"))));
    }
    let today = bt::today();
    if b.paid && starts.iter().any(|start| *start > today) {
        return Err(ApiError::bad_request("账期尚未生效，暂不能标记支付"));
    }
    let changed: Vec<Uuid> = if b.paid {
        sqlx::query_scalar("insert into billing_paid (slot_id, period_start) select * from unnest($1::uuid[], $2::date[]) on conflict do nothing returning slot_id")
    } else {
        sqlx::query_scalar("delete from billing_paid p using unnest($1::uuid[], $2::date[]) x(slot_id, period_start) where p.slot_id = x.slot_id and p.period_start = x.period_start returning p.slot_id")
    }
    .bind(&ids)
    .bind(&starts)
    .fetch_all(&mut *tx)
    .await?;
    if !changed.is_empty() {
        let names: Vec<String> = rows.iter().filter(|r| changed.contains(&r.s.id)).map(|r| r.s.name()).collect();
        let kind = if b.paid { "paid" } else { "unpaid" };
        let detail = json!({"month": bt::fmt_month(month), "names": names});
        ledger::log(&mut tx, team, None, OffsetDateTime::now_utc(), kind, None, None, detail.clone()).await?;
        log_audit(&mut tx, &a, &format!("billing.{kind}"), None, &tname, ledger::event_text(kind, None, None, &detail)).await?;
    }
    let out = month_view(&st, &mut tx, team, month).await?;
    tx.commit().await?;
    if !changed.is_empty() {
        st.emit(Event::Billing { id: team });
    }
    Ok(ok(out))
}

#[derive(Deserialize)]
pub struct EventsQ {
    limit: Option<i64>,
    before: Option<Uuid>,
}

/// `GET /teams/{id}/billing-events?limit=200&before=<id>`：增减记录，新的在前；before = 上一页最后一条的 id
pub async fn events(_a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>, ApiQuery(q): ApiQuery<EventsQ>) -> ApiResult<Json<Value>> {
    let mut conn = st.db.acquire().await?;
    team_name(&mut conn, team).await?;
    let limit = q.limit.unwrap_or(200).clamp(1, 500);
    let rows: Vec<EventRow> = sqlx::query_as(&format!(
        "select {EVENT_COLS} from billing_event where team_id = $1
           and ($2::uuid is null or (at, id) < (select at, id from billing_event where id = $2))
         order by at desc, id desc limit $3"
    ))
    .bind(team)
    .bind(q.before)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    Ok(ok(rows.iter().map(event_json).collect::<Vec<_>>()))
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
        return Err(ApiError::bad_request("备注最多 60 个字"));
    }
    Ok(s.to_string())
}

/// 开始收费日最晚今天加一个月（再晚的期在月份清单里翻不到）
fn check_start(d: Date) -> ApiResult<Date> {
    let max = bt::latest_start(bt::today());
    if d > max {
        return Err(ApiError::bad_request(format!("开始收费日最晚填到 {}（今天起一个月内）", bt::fmt_date(max))));
    }
    Ok(d)
}

fn check_range(start: Date, end: Date) -> ApiResult<()> {
    if end <= start {
        return Err(ApiError::bad_request(format!("停止收费日 {} 要晚于开始收费日 {}", bt::fmt_date(end), bt::fmt_date(start))));
    }
    Ok(())
}

// ── 写 ────────────────────────────────────────────────────────────────

/// 计费类审计：detail 带团队与一句话 what（audit.rs 的摘要据此显示）
async fn log_audit(tx: &mut Tx<'_>, a: &Actor, action: &str, target: Option<Uuid>, team: &str, what: String) -> ApiResult<()> {
    audit(tx, &a.name, action, "billing_slot", target, json!({"team": team, "what": what})).await?;
    Ok(())
}

/// 团队存在、不是备用团队，并取它的计费锁；返回团队名
async fn lock_billing_team(tx: &mut Tx<'_>, team: Uuid) -> ApiResult<String> {
    let t: Option<(String, bool)> = sqlx::query_as("select name, spare from team where id = $1").bind(team).fetch_optional(&mut **tx).await?;
    let (name, spare) = t.ok_or_else(|| ApiError::not_found("没有这个团队"))?;
    if spare {
        return Err(ApiError::bad_request("备用团队不计费"));
    }
    ledger::lock_team(tx, team).await?;
    Ok(name)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddIn {
    region: String,
    start_date: String,
    count: Option<i32>,
    note: Option<String>,
}

/// `POST /teams/{id}/billing-slots` `{region, start_date, count?, note}`：手动名额（系统外的节点，如墨西哥家宽），每个记一条 manual_add → `[Slot]`
pub async fn slot_add(a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>, ApiJson(b): ApiJson<AddIn>) -> ApiResult<Json<Value>> {
    let region = check_region(&b.region)?;
    let note = check_note(b.note.as_deref().unwrap_or(""))?;
    let start = date_in(&b.start_date, "开始收费日").and_then(check_start)?;
    let count = b.count.unwrap_or(1);
    if !(1..=50).contains(&count) {
        return Err(ApiError::bad_request("一次添加 1–50 个"));
    }
    let mut tx = st.db.begin().await?;
    let tname = lock_billing_team(&mut tx, team).await?;
    let now = OffsetDateTime::now_utc();
    let mut ids = vec![];
    for _ in 0..count {
        let id = new_id();
        sqlx::query("insert into billing_slot (id, team_id, region, start_date, manual, note) values ($1, $2, $3, $4, true, $5)")
            .bind(id)
            .bind(team)
            .bind(&region)
            .bind(start)
            .bind(&note)
            .execute(&mut *tx)
            .await?;
        ledger::log(&mut tx, team, Some(id), now, "manual_add", Some(&manual_name(&note)), None, json!({"region": region, "start_date": bt::fmt_date(start)})).await?;
        ids.push(id);
    }
    log_audit(&mut tx, &a, "billing.slot_add", ids.first().copied(), &tname, format!("{region} {count} 个，{}开始收费{}", bt::fmt_cn(start), if note.is_empty() { String::new() } else { format!("（{note}）") }))
        .await?;
    let mut out = vec![];
    for id in &ids {
        out.push(slot_out(&mut tx, *id).await?);
    }
    tx.commit().await?;
    st.emit(Event::Billing { id: team });
    Ok(ok(out))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchIn {
    start_date: Option<String>,
    region: Option<String>,
    note: Option<String>,
    /// 没传 = 不改；null = 恢复（只限手动名额）
    #[serde(default, deserialize_with = "super::some")]
    end_date: Option<Option<String>>,
}

/// 开始收费日改了：这个名额已支付的期按期序号 k 平移到新开始日的第 k 期（保留已支付时刻）；不在旧开始日期序列上、或新的期超出日期范围的删掉。返回 (挪了几条, 删了几条)
async fn shift_paid(tx: &mut Tx<'_>, id: Uuid, old: Date, new: Date) -> ApiResult<(usize, usize)> {
    let rows: Vec<(Date, OffsetDateTime)> = sqlx::query_as("delete from billing_paid where slot_id = $1 returning period_start, paid_at").bind(id).fetch_all(&mut **tx).await?;
    let mut moved = 0;
    for (ps, at) in &rows {
        let Some(nps) = bt::period_no(old, *ps).and_then(|k| bt::period(new, k)).map(|p| p.0) else { continue };
        sqlx::query("insert into billing_paid (slot_id, period_start, paid_at) values ($1, $2, $3)").bind(id).bind(nps).bind(*at).execute(&mut **tx).await?;
        moved += 1;
    }
    Ok((moved, rows.len() - moved))
}

/// `PATCH /billing-slots/{id}` `{start_date?, region?, note?, end_date?}`：改开始收费日（最晚今天加一个月；已支付的期按期序号跟着挪到新的期）/ 地区 / 备注（记 edit）；
/// end_date 只能给手动名额（记 manual_end）或改已结束的系统名额的结束日，有节点占着的 409；end_date: null = 恢复，只限手动名额 → `Slot`
pub async fn slot_patch(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<PatchIn>) -> ApiResult<Json<Value>> {
    let start = b.start_date.as_deref().map(|s| date_in(s, "开始收费日").and_then(check_start)).transpose()?;
    let region = b.region.as_deref().map(check_region).transpose()?;
    let note = b.note.as_deref().map(check_note).transpose()?;
    let end = match &b.end_date {
        None => None,
        Some(None) => Some(None),
        Some(Some(s)) => Some(Some(date_in(s, "停止收费日")?)),
    };
    let mut tx = st.db.begin().await?;
    let team: Uuid = sqlx::query_scalar("select team_id from billing_slot where id = $1").bind(id).fetch_optional(&mut *tx).await?.ok_or_else(|| ApiError::not_found("没有这个计费节点"))?;
    let tname = team_name(&mut tx, team).await?;
    ledger::lock_team(&mut tx, team).await?;
    let old = one_slot(&mut tx, id).await?;
    let new_start = start.unwrap_or(old.start_date);
    let new_end = match end {
        None => old.end_date,
        Some(e) => {
            if old.holder_id.is_some() || (!old.manual && old.end_date.is_none()) {
                return Err(ApiError::conflict("节点还在团队里；不收这个节点的钱请把节点设为不计费"));
            }
            if e.is_none() && !old.manual {
                return Err(ApiError::conflict("只有手动添加的计费节点能恢复；系统节点回到团队后会自动计费"));
            }
            if e.is_some_and(|d| !old.manual && d > bt::today()) {
                return Err(ApiError::bad_request("系统节点的停止收费日不能晚于今天"));
            }
            e
        }
    };
    if let Some(e) = new_end {
        check_range(new_start, e)?;
    }
    sqlx::query("update billing_slot set start_date = $2, region = coalesce($3, region), note = coalesce($4, note), end_date = $5 where id = $1")
        .bind(id)
        .bind(new_start)
        .bind(&region)
        .bind(&note)
        .bind(new_end)
        .execute(&mut *tx)
        .await?;
    let shifted = if old.start_date != new_start { Some(shift_paid(&mut tx, id, old.start_date, new_start).await?) } else { None };
    let now_row = one_slot(&mut tx, id).await?;
    let name = now_row.name();
    let mut what = vec![];
    if old.start_date != now_row.start_date {
        let mut w = format!("开始收费日 {} → {}", bt::fmt_cn(old.start_date), bt::fmt_cn(now_row.start_date));
        if let Some((moved, dropped)) = shifted {
            if moved > 0 {
                w.push_str(&format!("（{moved} 期的已支付按期序号挪到了新的期）"));
            }
            if dropped > 0 {
                w.push_str(&format!("（{dropped} 条对不上期的已支付记录已清掉）"));
            }
        }
        what.push(w);
    }
    if old.region != now_row.region {
        what.push(format!("地区 {} → {}", old.region, now_row.region));
    }
    if old.note != now_row.note {
        what.push("改了备注".into());
    }
    let at = OffsetDateTime::now_utc();
    match (old.end_date, now_row.end_date) {
        (None, Some(e)) => {
            let until = bt::billed_until(now_row.start_date, e).map(bt::fmt_date);
            ledger::log(&mut tx, team, Some(id), at, "manual_end", Some(&name), None, json!({"end_date": bt::fmt_date(e), "billed_until": until})).await?;
        }
        (Some(_), None) => what.push("恢复收费".into()),
        (Some(a), Some(b)) if a != b => what.push(format!("停止收费日 {} → {}", bt::fmt_cn(a), bt::fmt_cn(b))),
        _ => {}
    }
    if !what.is_empty() {
        ledger::log(&mut tx, team, Some(id), at, "edit", Some(&name), None, json!({"what": what.join("，")})).await?;
    }
    let mut all = what.clone();
    if let (None, Some(e)) = (old.end_date, now_row.end_date) {
        all.push(format!("{}停止收费", bt::fmt_cn(e)));
    }
    log_audit(&mut tx, &a, "billing.slot_update", Some(id), &tname, format!("{name}：{}", if all.is_empty() { "没有改动".into() } else { all.join("，") })).await?;
    let out = slot_out(&mut tx, id).await?;
    tx.commit().await?;
    st.emit(Event::Billing { id: team });
    Ok(ok(out))
}

/// `DELETE /billing-slots/{id}`：只能删手动名额（录错了）；系统节点的名额由对账结束
pub async fn slot_delete(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let team: Uuid = sqlx::query_scalar("select team_id from billing_slot where id = $1").bind(id).fetch_optional(&mut *tx).await?.ok_or_else(|| ApiError::not_found("没有这个计费节点"))?;
    let tname = team_name(&mut tx, team).await?;
    ledger::lock_team(&mut tx, team).await?;
    let s = one_slot(&mut tx, id).await?;
    if !s.manual {
        return Err(ApiError::conflict("只有手动添加的计费节点能删除；系统节点离开团队后由对账自动结束，也可以把节点设为不计费"));
    }
    sqlx::query("delete from billing_slot where id = $1").bind(id).execute(&mut *tx).await?;
    let what = format!("删除了手动添加的节点（{}，{}开始收费）", s.region, bt::fmt_cn(s.start_date));
    ledger::log(&mut tx, team, Some(id), OffsetDateTime::now_utc(), "edit", Some(&s.name()), None, json!({"what": what})).await?;
    log_audit(&mut tx, &a, "billing.slot_delete", Some(id), &tname, format!("{} · {what}", s.name())).await?;
    tx.commit().await?;
    st.emit(Event::Billing { id: team });
    Ok(ok(json!({"id": id})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExemptIn {
    exempt: bool,
}

/// `PUT /machines/{id}/billing-exempt` `{exempt}`：节点设为不计费 / 恢复计费（记 exempt；随即对账，当它离开 / 加入：名额结束 / 接回或新建）
pub async fn exempt(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<ExemptIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let m: Option<(String, Option<Uuid>, Option<String>, String)> = sqlx::query_as(
        "select m.kind::text, m.team_id, t.name, display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = $1 and m.recycled_at is null",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let (kind, team, tname, name) = m.ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    if kind == "relay" {
        return Err(ApiError::bad_request("线路机不计费，不用设"));
    }
    if let Some(t) = team {
        ledger::lock_team(&mut tx, t).await?;
    }
    let changed = if b.exempt {
        sqlx::query("insert into billing_exempt (machine_id) values ($1) on conflict do nothing").bind(id).execute(&mut *tx).await?.rows_affected()
    } else {
        sqlx::query("delete from billing_exempt where machine_id = $1").bind(id).execute(&mut *tx).await?.rows_affected()
    } > 0;
    if changed {
        if let Some(t) = team {
            ledger::log(&mut tx, t, None, OffsetDateTime::now_utc(), "exempt", Some(&name), None, json!({"exempt": b.exempt})).await?;
        }
        audit(&mut tx, &a.name, "billing.exempt", "machine", Some(id), json!({"team": tname.unwrap_or_default(), "name": name, "what": if b.exempt { "设为不计费" } else { "恢复计费" }}))
            .await?;
    }
    tx.commit().await?;
    if changed {
        st.emit(Event::Machine { id });
        if let Some(t) = team {
            st.emit(Event::Billing { id: t });
            // 马上对账一次，设了不计费 / 恢复计费立刻反映在名额上（没初始化过不动）
            let r = match ledger::initialized(&st.db).await {
                Ok(true) => ledger::reconcile_team(&st, t, OffsetDateTime::now_utc()).await.map(|_| ()),
                Ok(false) => Ok(()),
                Err(e) => Err(e.into()),
            };
            if let Err(e) = r {
                tracing::error!("计费对账（设不计费后）失败：{e:#}");
            }
        }
    }
    Ok(ok(json!({"machine_id": id, "exempt": b.exempt})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitIn {
    since: String,
    /// 只预览：返回会建的，不写
    #[serde(default)]
    dry_run: bool,
}

/// `POST /billing/init` `{since, dry_run?}`：一次性初始化——对还没有任何名额的非备用团队，把现在每个可计费节点建一个名额（开始收费日 = since，地区按国家，记 init）；
/// 已有名额的团队跳过。第一次初始化之后对账才开始（app_setting 的 billing_since）。返回每个团队建了几个、哪些节点
pub async fn init(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<InitIn>) -> ApiResult<Json<Value>> {
    let since = date_in(&b.since, "开始收费日").and_then(check_start)?;
    let mut tx = st.db.begin().await?;
    let teams: Vec<(Uuid, String)> = sqlx::query_as("select id, name from team where not spare order by sort_order, name").fetch_all(&mut *tx).await?;
    let now = OffsetDateTime::now_utc();
    let mut out = vec![];
    let mut total = 0;
    let mut touched = vec![];
    for (team, tname) in teams {
        ledger::lock_team(&mut tx, team).await?;
        let existing: i64 = sqlx::query_scalar("select count(*) from billing_slot where team_id = $1").bind(team).fetch_one(&mut *tx).await?;
        if existing > 0 {
            out.push(json!({"team": {"id": team, "name": tname}, "skipped": true, "existing": existing, "count": 0, "nodes": []}));
            continue;
        }
        let nodes = ledger::billable_nodes(&mut tx, team).await?;
        let regions = ledger::regions(&st, &nodes.iter().collect::<Vec<_>>()).await;
        if !b.dry_run {
            for n in &nodes {
                ledger::create_slot(&mut tx, team, n, &regions[&n.id], since).await?;
            }
            if !nodes.is_empty() {
                let listed: Vec<(&ledger::Node, &str)> = nodes.iter().map(|n| (n, regions[&n.id].as_str())).collect();
                ledger::log_init(&mut tx, team, since, &listed, now).await?;
                touched.push(team);
            }
        }
        total += nodes.len();
        out.push(json!({"team": {"id": team, "name": tname}, "skipped": false, "existing": 0, "count": nodes.len(),
                        "nodes": nodes.iter().map(|n| json!({"id": n.id, "name": n.name, "region": regions[&n.id]})).collect::<Vec<_>>()}));
    }
    if !b.dry_run {
        sqlx::query("insert into app_setting (key, value, updated_by) values ($1, to_jsonb($2::text), $3) on conflict (key) do nothing")
            .bind(ledger::SINCE_KEY)
            .bind(bt::fmt_date(since))
            .bind(&a.name)
            .execute(&mut *tx)
            .await?;
        audit(&mut tx, &a.name, "billing.init", "billing_slot", None, json!({"team": "", "what": format!("{}开始收费，建了 {total} 个计费节点", bt::fmt_cn(since))})).await?;
        tx.commit().await?;
        for team in touched {
            st.emit(Event::Billing { id: team });
        }
    }
    Ok(ok(json!({"since": bt::fmt_date(since), "dry_run": b.dry_run, "total": total, "teams": out})))
}

// ── 管理：客户页链接 ──────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct PageRow {
    token_enc: Vec<u8>,
    password_enc: Option<Vec<u8>>,
    enabled: bool,
    created_at: OffsetDateTime,
    token_at: OffsetDateTime,
    last_view_at: Option<OffsetDateTime>,
    last_view_ip: Option<String>,
}

const PAGE_COLS: &str = "token_enc, password_enc, enabled, created_at, token_at, last_view_at, host(last_view_ip) last_view_ip";

fn link(st: &AppState, token: &str) -> String { format!("{}/c/{token}", st.cfg.public_url.trim_end_matches('/')) }

fn page_of(st: &AppState, r: &PageRow) -> Value {
    // 密钥换了解不开时不报错（月份清单照常能看），链接给 null，换新链接即可
    let token = st.secrets.open_str(&r.token_enc).map_err(|e| tracing::warn!("客户页令牌解密失败：{e:#}")).ok();
    json!({
        "password": r.password_enc.as_deref().and_then(|p| st.secrets.open_str(p).ok()),
        "enabled": r.enabled, "link": token.map(|t| link(st, &t)),
        "created_at": meta::fmt_ts(r.created_at), "token_at": meta::fmt_ts(r.token_at),
        "last_view_at": r.last_view_at.map(meta::fmt_ts), "last_view_ip": r.last_view_ip,
    })
}

/// 自动初始化只补齐尚无密码的旧分享，已有链接和密码保持不变。
async fn rotate_page(st: &AppState, db: &mut PgConnection, team: Uuid, automatic: bool) -> ApiResult<bool> {
    let token = random_token(32);
    let result = sqlx::query(
        "insert into customer_page (team_id, token_hash, token_enc, password_enc)
         select id, $2, $3, $4 from team where id = $1 and (not $5 or not spare)
         on conflict (team_id) do update set token_hash = excluded.token_hash, token_enc = excluded.token_enc,
             password_enc = excluded.password_enc, token_at = now(), last_view_at = null, last_view_ip = null
         where not $5 or (customer_page.enabled and customer_page.password_enc is null)",
    ).bind(team).bind(sha256(token.as_bytes())).bind(st.secrets.seal_str(&token))
        .bind(st.secrets.seal_str(&random_token(12))).bind(automatic).execute(db).await?;
    Ok(result.rows_affected() > 0)
}

/// 为团队建立固定密码分享，并补齐旧分享的密码。
pub async fn ensure_customer_pages(st: &AppState) -> ApiResult<()> {
    if !ledger::initialized(&st.db).await? { return Ok(()); }
    let mut db = st.db.acquire().await?;
    let teams: Vec<Uuid> = sqlx::query_scalar(
        "select t.id from team t left join customer_page p on p.team_id = t.id
         where not t.spare and (p.team_id is null or (p.enabled and p.password_enc is null))",
    ).fetch_all(&mut *db).await?;
    for team in teams {
        if rotate_page(st, &mut db, team, true).await? {
            st.emit(Event::Billing { id: team });
        }
    }
    Ok(())
}

/// 团队的客户页链接状态；还没建过是 null
async fn page_json(st: &AppState, db: &mut PgConnection, team: Uuid) -> ApiResult<Value> {
    rotate_page(st, db, team, true).await?;
    let r: Option<PageRow> = sqlx::query_as(&format!("select {PAGE_COLS} from customer_page where team_id = $1")).bind(team).fetch_optional(&mut *db).await?;
    Ok(r.map(|r| page_of(st, &r)).unwrap_or(Value::Null))
}

/// `POST /teams/{id}/customer-page`：没有就建，有就换新令牌（旧链接立即失效；启用 / 停用状态不变）
pub async fn page_create(a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let tname: String = sqlx::query_scalar("select name from team where id = $1 for no key update")
        .bind(team)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个团队"))?;
    let had: bool = sqlx::query_scalar("select exists (select 1 from customer_page where team_id = $1)").bind(team).fetch_one(&mut *tx).await?;
    rotate_page(&st, &mut tx, team, false).await?;
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

/// `POST /api/v1/public/customer/{令牌}?month=2026-11`：未指定月份时定位到最近有已生效账期的月份。
/// 每个节点只有名字（手动的写备注）、地区、这一期 [period_start, period_end)、paid，不含 IP、供应商、价格。令牌不对或已停用一律 404（不区分）。令牌按 sha256 查库；
/// 和订阅拉取一样，不对的记一次失败、与登录共用按来源的限速（键按令牌哈希分开，别人猜错的不会把对的令牌一起封掉）。
/// 最近查看时间与来源 10 分钟内只写一次。X-Robots-Tag 由 lib.rs 的响应头中间件统一加
#[derive(Deserialize)]
pub struct CustomerAccess {
    password: String,
}

pub async fn public_view(State(st): State<AppState>, ClientIp(ip): ClientIp, ApiPath(token): ApiPath<String>, ApiQuery(q): ApiQuery<MonthQ>, ApiJson(access): ApiJson<CustomerAccess>) -> ApiResult<Response> {
    // 账期口径与管理端一致，但客户不展示尚未生效的期次或未来月份。
    let today = bt::today();
    let this_month = bt::month_of(today);
    let requested_month = q.month.as_deref().filter(|month| !month.is_empty());
    let mut month = month_in(requested_month)?.min(this_month);
    let hash = sha256(token.as_bytes());
    let key = format!("cust:{}", &hex::encode(&hash)[..16]);
    let attempt = auth::begin_attempt(&st, &key, ip.map(auth::ip_group)).await.map_err(|e| {
        if e.status == StatusCode::TOO_MANY_REQUESTS {
            ApiError { message: "访问次数过多，请稍后再试".into(), ..e }
        } else {
            e
        }
    })?;
    let row: Option<(Uuid, String, Vec<u8>)> = sqlx::query_as(
        "select p.team_id, t.name, p.password_enc
         from customer_page p join team t on t.id = p.team_id
         where p.token_hash = $1 and p.enabled and p.password_enc is not null",
    ).bind(&hash).fetch_optional(&st.db).await?;
    let (team, name, sealed) = row.ok_or_else(|| ApiError::not_found("链接无效或已停用"))?;
    let password = st.secrets.open(&sealed).map_err(|_| ApiError::not_found("请重新生成分享链接"))?;
    if sha256(&password) != sha256(access.password.as_bytes()) {
        return Err(ApiError::forbidden("查看密码不正确"));
    }
    auth::forget_attempt(&st, attempt).await?;
    sqlx::query(
        "update customer_page set last_view_at = now(), last_view_ip = $2
         where team_id = $1 and token_hash = $3 and (last_view_at is null or last_view_at < now() - interval '10 minutes')",
    )
    .bind(team)
    .bind(ip.map(ipnetwork::IpNetwork::from))
    .bind(&hash)
    .execute(&st.db)
    .await?;
    let mut db = st.db.acquire().await?;
    if requested_month.is_none() {
        let slots: Vec<(Date, Option<Date>)> = sqlx::query_as("select start_date, end_date from billing_slot where team_id = $1")
            .bind(team).fetch_all(&mut *db).await?;
        month = slots.into_iter().filter_map(|(start, end)| bt::latest_started_month(start, end, today)).max().unwrap_or(this_month);
    }
    let (months, rows) = month_rows(&mut db, team, month).await?;
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for row in outstanding_rows(&mut db, team, today).await? {
        *counts.entry(bt::fmt_month(row.ps)).or_default() += 1;
    }
    let outstanding: Vec<_> = counts.into_iter().map(|(month, count)| json!({"month": month, "count": count})).collect();
    let nodes: Vec<Value> = rows.iter().filter(|r| r.ps <= today).map(|r| json!({
        "name": r.s.name(), "region": r.s.region, "period_start": bt::fmt_date(r.ps), "period_end": bt::fmt_date(r.pe),
        "paid": r.paid_at.is_some(), "manual": r.s.manual, "ended": r.s.state(today) == "ended",
    })).collect();
    let page = json!({
        "team": name, "today": bt::fmt_date(today),
        "month": bt::fmt_month(month), "months": months.into_iter().filter(|m| *m <= this_month).map(bt::fmt_month).collect::<Vec<_>>(),
        "outstanding": outstanding, "nodes": nodes,
    });
    let mut r = ok(page).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}
