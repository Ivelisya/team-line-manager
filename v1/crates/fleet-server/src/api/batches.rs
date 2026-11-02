//! 计费台账（迁移 0097，派生口径在 batches.rs，对账在 ledger.rs）与客户到期页。
//! 计费名额 = 团队里一个按月计费的节点位置；批次、到期、月度应收都从名额派生。名额由对账自动增减，这里只有：看（批次、名额、待定、增减记录、月度应收）、
//! 改名额（起算日、地区、备注、结束 / 恢复）、手动名额（系统外的节点）、节点设不计费、一次性初始化。读要 read、写要 write（写审计）。
//! 每个团队一个只读分享链接（令牌 32 字节随机，库里存 sha256 用来查、SecretBox 密文用来随时再复制链接），
//! 公开接口 `GET /api/v1/public/customer/{令牌}` 不登录（和 client_subs 的 /sub/{令牌} 一样没有 Actor 提取器，登录只在提取器里校验，所以不会被拦）。
//! 客户页不出现 IP、供应商、价格；节点清单（nodes）里有节点名与各自的本期、下次续费日。

use super::ok;
use crate::auth::{self, Actor, ClientIp};
use crate::batches::{self as bt, Batch};
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
use time::{Date, OffsetDateTime};
use uuid::Uuid;

// ── 读取 ──────────────────────────────────────────────────────────────

const SLOT_COLS: &str = "s.id, s.team_id, s.region, s.start_date, s.end_date, s.holder_id, display_name(hm.kind::text, hm.label, ht.name, hm.number) holder_now,
                         s.holder_name, s.vacated_at, s.manual, s.note, s.vacate_reason";
const SLOT_FROM: &str = "billing_slot s left join machine hm on hm.id = s.holder_id left join team ht on ht.id = hm.team_id";

#[derive(sqlx::FromRow, Clone)]
struct SlotRow {
    id: Uuid,
    team_id: Uuid,
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

    fn state(&self, today: Date) -> &'static str {
        match self.end_date {
            Some(e) if e <= today => "ended",
            // 系统名额没有占用者就是空出待定（没结束，或结束日还在以后——对账只认没结束的，不会按这个日子结束）
            _ if !self.manual && self.holder_id.is_none() => "vacated",
            _ => "active",
        }
    }

    fn lite(&self) -> bt::Slot { bt::Slot { start: self.start_date, end: self.end_date, region: self.region.clone() } }
}

/// 团队的全部名额（含已结束的）
async fn team_slots(db: &mut PgConnection, team: Uuid) -> ApiResult<Vec<SlotRow>> {
    Ok(sqlx::query_as(&format!("select {SLOT_COLS} from {SLOT_FROM} where s.team_id = $1 order by s.start_date, s.created_at, s.id")).bind(team).fetch_all(&mut *db).await?)
}

async fn one_slot(db: &mut PgConnection, id: Uuid) -> ApiResult<SlotRow> {
    let r: Option<SlotRow> = sqlx::query_as(&format!("select {SLOT_COLS} from {SLOT_FROM} where s.id = $1")).bind(id).fetch_optional(&mut *db).await?;
    r.ok_or_else(|| ApiError::not_found("没有这个计费节点"))
}

fn derive_all(slots: &[SlotRow], today: Date) -> Vec<Batch> { bt::derive_batches(&slots.iter().map(SlotRow::lite).collect::<Vec<_>>(), today) }

fn slot_json(s: &SlotRow, batches: &[Batch], today: Date) -> Value {
    let key = bt::key(s.start_date, &s.region);
    json!({
        "id": s.id, "batch_seq": batches.iter().find(|b| b.key == key).map(|b| b.seq), "region": s.region, "start_date": bt::fmt_date(s.start_date),
        "state": s.state(today), "name": s.name(),
        "holder": s.holder_id.map(|id| json!({"id": id, "name": s.holder_now.clone().unwrap_or_default()})),
        "last_holder_name": if s.holder_id.is_none() && !s.manual { s.holder_name.clone() } else { None },
        // 系统名额没有占用者（空出 / 已结束）的原因，对账空出时落库：exempt = 原占用者被设为不计费（节点还在团队里）；left = 离开团队、退役、进备用或被删；其余 null
        "left_reason": s.vacate_reason,
        "vacated_at": s.vacated_at.map(meta::fmt_ts), "end_date": s.end_date.map(bt::fmt_date), "manual": s.manual, "note": s.note,
    })
}

/// 一个名额的 Slot（批次编号按团队现在的名额重新派生）
async fn slot_out(db: &mut PgConnection, id: Uuid) -> ApiResult<Value> {
    let s = one_slot(db, id).await?;
    let today = bt::today();
    let all = team_slots(db, s.team_id).await?;
    Ok(slot_json(&s, &derive_all(&all, today), today))
}

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

/// `GET /teams/{id}/batches`：派生批次（含已到期）、名额、待定节点、个数与客户页链接状态
pub async fn list(_a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut conn = st.db.acquire().await?;
    let name = team_name(&mut conn, team).await?;
    let today = bt::today();
    let mut slots = team_slots(&mut conn, team).await?;
    let derived = derive_all(&slots, today);
    let batches: Vec<Value> = derived.iter().filter_map(|b| bt::derive(b, today).map(|d| bt::admin_json(b, &d))).collect();
    slots.sort_by_key(|s| (s.start_date, s.name()));
    let pending: Vec<(Uuid, String, OffsetDateTime)> = sqlx::query_as(
        "select p.machine_id, display_name(m.kind::text, m.label, t.name, m.number), p.seen_at
         from billing_pending p join machine m on m.id = p.machine_id left join team t on t.id = m.team_id where p.team_id = $1 order by p.seen_at, m.number",
    )
    .bind(team)
    .fetch_all(&mut *conn)
    .await?;
    let count = |st: &str| slots.iter().filter(|s| s.state(today) == st).count();
    let page = page_json(&st, &mut conn, team).await?;
    let initialized = ledger::initialized(&st.db).await?;
    Ok(ok(json!({
        "team": {"id": team, "name": name}, "today": bt::fmt_date(today), "initialized": initialized, "page": page, "batches": batches,
        "slots": slots.iter().map(|s| slot_json(s, &derived, today)).collect::<Vec<_>>(),
        "pending": pending.iter().map(|(id, n, at)| json!({"machine": {"id": id, "name": n}, "seen_at": meta::fmt_ts(*at), "decide_at": meta::fmt_ts(*at + ledger::GRACE)})).collect::<Vec<_>>(),
        "totals": {"active": count("active"), "pending": pending.len(), "vacated": count("vacated")},
    })))
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

#[derive(Deserialize)]
pub struct MonthQ {
    month: Option<String>,
}

/// `GET /teams/{id}/receivable?month=2026-11`（默认本月）：各批起点落在这个月的期、合计节点月、月初 / 月末（本月是今天）有效个数、本月增减。
/// 还没到起点的期是预估（`projected`：现在没结束的名额——在用与空出待定——都按续下去算）；以后的月份整月是预估，月初 / 月末个数为 null、没有增减
pub async fn receivable(_a: Actor, State(st): State<AppState>, ApiPath(team): ApiPath<Uuid>, ApiQuery(q): ApiQuery<MonthQ>) -> ApiResult<Json<Value>> {
    let today = bt::today();
    let month = match q.month.as_deref() {
        Some(m) => bt::parse_month(m).ok_or_else(|| ApiError::bad_request("月份要写成 2026-11 这样"))?,
        None => today.replace_day(1).map_err(|_| ApiError::internal("日期出错"))?,
    };
    let next = meta::add_months(month, 1).ok_or_else(|| ApiError::bad_request("日期超出范围"))?;
    let mut conn = st.db.acquire().await?;
    team_name(&mut conn, team).await?;
    let slots: Vec<bt::Slot> = team_slots(&mut conn, team).await?.iter().map(SlotRow::lite).collect();
    let batches = bt::derive_batches(&slots, today);
    let lines = bt::receivable(&slots, &batches, month);
    let future = month > today;
    let last = if (month..next).contains(&today) { today } else { next.previous_day().unwrap_or(next) };
    let off = meta::org_offset();
    let rows: Vec<EventRow> = if future { vec![] } else { sqlx::query_as(&format!(
        "select {EVENT_COLS} from billing_event where team_id = $1 and at >= $2 and at < $3 and kind in ('add', 'remove', 'swap', 'manual_add', 'manual_end')
         order by at, id"
    ))
    .bind(team)
    .bind(month.midnight().assume_offset(off))
    .bind(next.midnight().assume_offset(off))
    .fetch_all(&mut *conn)
    .await? };
    Ok(ok(json!({
        "month": format!("{:04}-{:02}", month.year(), u8::from(month.month())),
        "lines": lines.iter().map(|l| json!({"batch_seq": l.seq, "region": l.region, "period_start": bt::fmt_date(l.start), "period_end": bt::fmt_date(l.end), "count": l.count, "projected": l.start > today})).collect::<Vec<_>>(),
        "total": lines.iter().map(|l| i64::from(l.count)).sum::<i64>(),
        "projected": future || lines.iter().any(|l| l.start > today),
        "start_count": (!future).then(|| bt::active_on(&slots, month)), "end_count": (!future).then(|| bt::active_on(&slots, last)),
        "events": rows.iter().map(event_json).collect::<Vec<_>>(),
    })))
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

fn check_range(start: Date, end: Date) -> ApiResult<()> {
    if end <= start {
        return Err(ApiError::bad_request(format!("结束日 {} 要晚于起算日 {}", bt::fmt_date(end), bt::fmt_date(start))));
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
    let start = date_in(&b.start_date, "起算日")?;
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
    log_audit(&mut tx, &a, "billing.slot_add", ids.first().copied(), &tname, format!("{} × {count}，{} 起算{}", region, bt::fmt_date(start), if note.is_empty() { String::new() } else { format!("（{note}）") }))
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

/// `PATCH /billing-slots/{id}` `{start_date?, region?, note?, end_date?}`：改起算日 / 地区 / 备注（记 edit）；
/// end_date 只能给手动名额或已空出的名额（记 manual_end），有节点占着的 409；end_date: null = 恢复，只限手动名额 → `Slot`
pub async fn slot_patch(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<PatchIn>) -> ApiResult<Json<Value>> {
    let start = b.start_date.as_deref().map(|s| date_in(s, "起算日")).transpose()?;
    let region = b.region.as_deref().map(check_region).transpose()?;
    let note = b.note.as_deref().map(check_note).transpose()?;
    let end = match &b.end_date {
        None => None,
        Some(None) => Some(None),
        Some(Some(s)) => Some(Some(date_in(s, "结束日")?)),
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
            if old.holder_id.is_some() {
                return Err(ApiError::conflict("节点还在团队里；不收这个节点的钱请把节点设为不计费"));
            }
            if e.is_none() && !old.manual {
                return Err(ApiError::conflict("只有手动添加的计费节点能恢复；系统节点回到团队后会自动计费"));
            }
            if e.is_some() && !old.manual && old.vacated_at.is_none() && old.end_date.is_none() {
                return Err(ApiError::conflict("这个计费节点没有空出，不能手动结束"));
            }
            if e.is_some_and(|d| !old.manual && d > bt::today()) {
                return Err(ApiError::bad_request("空出的计费节点结束日不能晚于今天"));
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
    let now_row = one_slot(&mut tx, id).await?;
    let name = now_row.name();
    let mut what = vec![];
    if old.start_date != now_row.start_date {
        what.push(format!("起算日 {} → {}", bt::fmt_md(old.start_date), bt::fmt_md(now_row.start_date)));
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
        (Some(_), None) => what.push("恢复计费".into()),
        (Some(a), Some(b)) if a != b => what.push(format!("结束日 {} → {}", bt::fmt_md(a), bt::fmt_md(b))),
        _ => {}
    }
    if !what.is_empty() {
        ledger::log(&mut tx, team, Some(id), at, "edit", Some(&name), None, json!({"what": what.join("，")})).await?;
    }
    let mut all = what.clone();
    if let (None, Some(e)) = (old.end_date, now_row.end_date) {
        all.push(format!("结束于 {}", bt::fmt_date(e)));
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
    let what = format!("删除了手动计费节点（{}，{} 起算）", s.region, bt::fmt_md(s.start_date));
    ledger::log(&mut tx, team, Some(id), OffsetDateTime::now_utc(), "edit", Some(&s.name()), None, json!({"what": what})).await?;
    log_audit(&mut tx, &a, "billing.slot_delete", Some(id), &tname, format!("{} · {what}", s.name())).await?;
    tx.commit().await?;
    st.emit(Event::Billing { id: team });
    Ok(ok(json!({"id": id})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchIn {
    start_date: String,
}

/// `PATCH /teams/{id}/batches/{key}` `{start_date}`：整批改起算日（key = Batch.key，如 `2026-10-01|美区`，URL 里要转义）；该批全部名额一起改，
/// 记一条 edit 和一条审计。新起算日要早于该批所有已结束名额的结束日（否则 400）；改到与别的批次同日同地区就并进那一批 → 新的 `Batch`
pub async fn batch_patch(a: Actor, State(st): State<AppState>, ApiPath((team, key)): ApiPath<(Uuid, String)>, ApiJson(b): ApiJson<BatchIn>) -> ApiResult<Json<Value>> {
    let new_start = date_in(&b.start_date, "起算日")?;
    let (old_start, region) = key.split_once('|').and_then(|(d, r)| Some((bt::parse_date(d)?, r.to_string()))).ok_or_else(|| ApiError::bad_request("批次 key 要写成 2026-10-01|美区 这样"))?;
    let mut tx = st.db.begin().await?;
    let tname = lock_billing_team(&mut tx, team).await?;
    let today = bt::today();
    let before = derive_all(&team_slots(&mut tx, team).await?, today);
    let seq = before.iter().find(|x| x.key == key).map(|x| x.seq).ok_or_else(|| ApiError::not_found("没有这个批次"))?;
    let (n, min_end): (i64, Option<Date>) = sqlx::query_as("select count(*), min(end_date) from billing_slot where team_id = $1 and region = $2 and start_date = $3")
        .bind(team)
        .bind(&region)
        .bind(old_start)
        .fetch_one(&mut *tx)
        .await?;
    if let Some(e) = min_end {
        if new_start >= e {
            return Err(ApiError::bad_request(format!("起算日 {} 要早于已结束名额的结束日 {}", bt::fmt_date(new_start), bt::fmt_date(e))));
        }
    }
    if new_start != old_start {
        sqlx::query("update billing_slot set start_date = $4 where team_id = $1 and region = $2 and start_date = $3").bind(team).bind(&region).bind(old_start).bind(new_start).execute(&mut *tx).await?;
        let label = format!("第{seq:02}批 · {region}");
        let what = format!("起算日 {} → {}（整批 {n} 个）", bt::fmt_md(old_start), bt::fmt_md(new_start));
        ledger::log(&mut tx, team, None, OffsetDateTime::now_utc(), "edit", Some(&label), None, json!({"what": what})).await?;
        log_audit(&mut tx, &a, "billing.batch_update", None, &tname, format!("{label}：{what}")).await?;
    }
    let after = derive_all(&team_slots(&mut tx, team).await?, today);
    let out = after.iter().find(|x| x.key == bt::key(new_start, &region)).and_then(|x| bt::derive(x, today).map(|d| bt::admin_json(x, &d))).ok_or_else(|| ApiError::internal("批次派生失败"))?;
    tx.commit().await?;
    st.emit(Event::Billing { id: team });
    Ok(ok(out))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExemptIn {
    exempt: bool,
}

/// `PUT /machines/{id}/billing-exempt` `{exempt}`：节点设为不计费 / 恢复计费（记 exempt；之后对账把它当离开 / 加入，照样走 24 小时配对）
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

/// `POST /billing/init` `{since, dry_run?}`：一次性初始化——对还没有任何名额的非备用团队，把现在每个可计费节点建一个名额（起算日 = since，地区按国家，记 init）；
/// 已有名额的团队跳过。第一次初始化之后对账才开始（app_setting 的 billing_since）。返回每个团队建了几个、哪些节点
pub async fn init(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<InitIn>) -> ApiResult<Json<Value>> {
    let since = date_in(&b.since, "起算日")?;
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
        audit(&mut tx, &a.name, "billing.init", "billing_slot", None, json!({"team": "", "what": format!("{} 起算，建了 {total} 个计费节点", bt::fmt_date(since))})).await?;
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

/// 客户页的节点清单：收费中的名额（没结束，或结束日在今天以后；含空出待定的），各自的本期（含今天的那一期，[period_start, period_end)），
/// 按下次续费日（period_end）、名字排。名字：占用者现在的名字；空出待定的是原占用者；手动的写备注，没有备注写「手动添加的节点」。
/// renews：本期结束后还续不续（结束日晚于 period_end 或没结束）。不含 IP、供应商、价格
fn public_nodes(slots: &[SlotRow], today: Date) -> Vec<Value> {
    let mut out: Vec<(Date, String, Value)> = slots
        .iter()
        .filter(|s| s.state(today) != "ended")
        .filter_map(|s| {
            let (ps, pe) = bt::period(s.start_date, bt::last_period(s.start_date, &[None], today)?)?;
            let name = match (s.manual, s.note.is_empty()) {
                (true, true) => "手动添加的节点".to_string(),
                (true, false) => s.note.clone(),
                _ => s.name(),
            };
            let v = json!({
                "name": name, "region": s.region, "start_date": bt::fmt_date(s.start_date),
                "period_start": bt::fmt_date(ps), "period_end": bt::fmt_date(pe), "renews": s.end_date.is_none_or(|e| e > pe),
            });
            Some((pe, name, v))
        })
        .collect();
    out.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    out.into_iter().map(|(_, _, v)| v).collect()
}

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
    let today = bt::today();
    let slots = team_slots(&mut *st.db.acquire().await?, team).await?;
    let mut page = bt::public_page(&name, today, &derive_all(&slots, today));
    page["nodes"] = Value::Array(public_nodes(&slots, today));
    let mut r = ok(page).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}
