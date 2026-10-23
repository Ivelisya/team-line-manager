//! 待付清单（GET /billing/due）与批量登记续费（POST /renewals/batch）。单台续费的逻辑在 machines::renew_in_tx，批量一个事务里逐台调它。

use super::ok;
use crate::auth::Actor;
use crate::db::audit;
use crate::error::{ApiError, ApiJson, ApiQuery, ApiResult};
use crate::fx;
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use time::Date;
use uuid::Uuid;

/// 默认看未来多少天
const DEFAULT_DAYS: i64 = 30;
/// 一次批量最多多少台
const MAX_BATCH: usize = 200;

#[derive(Deserialize)]
pub struct DueQuery {
    /// 未来多少天内到期（含已过期，按各订阅账单时区的今天算；默认 30）
    days: Option<i64>,
    /// 或者给一个日期：到期日不晚于它（与 days 二选一）
    until: Option<Date>,
    /// 带上备用团队的机器（它们等到期丢弃，默认不算待付）
    #[serde(default)]
    include_spare: bool,
}

#[derive(sqlx::FromRow)]
struct DueRow {
    id: Uuid,
    name: String,
    kind: String,
    provider_id: Option<Uuid>,
    provider: Option<String>,
    panel_url: Option<String>,
    due_date: Date,
    due_days: i32,
    auto_renew: bool,
    price_cents: i32,
    currency: String,
    cycle: String,
    spare: bool,
}

/// 待付清单：在用机器里到期日落在窗口内的，按 供应商 × 到期日 分组，每组列机器、原币种小计与 USD 合计（`total`，同总览月费合计的形状）。
/// 开了自动续费的不算待付，单列在 `auto`（到期后由中控按周期顺延，只记账）。`groups[].machines[].price_cents` 是订阅的每期价格（登记已付的默认金额）
pub async fn due(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<DueQuery>) -> ApiResult<Json<Value>> {
    if q.days.is_some() && q.until.is_some() {
        return Err(ApiError::bad_request("days 与 until 给一个"));
    }
    let days = q.days.unwrap_or(DEFAULT_DAYS);
    if !(0..=3650).contains(&days) {
        return Err(ApiError::bad_request("days 要在 0–3650 之间"));
    }
    let rows: Vec<DueRow> = sqlx::query_as(
        "select m.id, display_name(m.kind::text, m.label, t.name, m.number) name, m.kind::text kind, pv.id provider_id, pv.name provider, pv.panel_url,
                s.due_date, (s.due_date - (now() at time zone e.billing_tz)::date) due_days, e.auto_renew, e.price_cents, e.currency, e.cycle::text cycle,
                coalesce(t.spare, false) spare
         from subscription s join subscription_eff e on e.machine_id = s.machine_id join machine m on m.id = s.machine_id
         left join team t on t.id = m.team_id left join plan pl on pl.id = m.plan_id left join provider pv on pv.id = coalesce(pl.provider_id, m.provider_id)
         where m.lifecycle = 'active' and ($3 or not coalesce(t.spare, false))
           and s.due_date <= coalesce($1::date, (now() at time zone e.billing_tz)::date + $2::int)
         order by s.due_date, pv.name nulls last, m.kind, t.sort_order, m.number, m.id",
    )
    .bind(q.until)
    .bind(days as i32)
    .bind(q.include_spare)
    .fetch_all(&st.db)
    .await?;
    let rate = fx::current(&st.db).await?;
    let mut total = fx::Total::default();
    let mut groups: Vec<(Option<Uuid>, Date, Value, fx::Total, Vec<Value>)> = vec![];
    let mut auto = vec![];
    for r in rows {
        let provider = json!({"id": r.provider_id, "name": r.provider, "panel_url": r.panel_url});
        let (usd, r_) = fx::convert(&rate, i64::from(r.price_cents), &r.currency);
        let m = json!({
            "id": r.id, "name": r.name, "kind": r.kind, "due_date": r.due_date, "due_days": r.due_days, "spare": r.spare, "cycle": r.cycle,
            "price_cents": r.price_cents, "currency": r.currency, "price_usd_cents": usd,
        });
        if r.auto_renew {
            auto.push(json!({"provider": provider, "machine": m}));
            continue;
        }
        total.add(&r.currency, i64::from(r.price_cents), usd.zip(r_));
        let g = match groups.iter().position(|g| g.0 == r.provider_id && g.1 == r.due_date) {
            Some(i) => i,
            None => {
                groups.push((r.provider_id, r.due_date, provider, fx::Total::default(), vec![]));
                groups.len() - 1
            }
        };
        groups[g].3.add(&r.currency, i64::from(r.price_cents), usd.zip(r_));
        groups[g].4.push(m);
    }
    let groups: Vec<Value> = groups
        .into_iter()
        .map(|(_, due_date, provider, t, machines)| json!({"provider": provider, "due_date": due_date, "machines": machines, "total": t}))
        .collect();
    Ok(ok(json!({"days": q.until.is_none().then_some(days), "until": q.until, "groups": groups, "auto": auto, "total": total})))
}

#[derive(Deserialize)]
pub struct BatchItem {
    machine_id: Uuid,
    /// 实付金额（分，订阅的币种）；没给 = 订阅价格 × 周期数
    amount_cents: Option<i32>,
}

#[derive(Deserialize)]
pub struct BatchIn {
    items: Vec<BatchItem>,
    /// 每台按订阅周期推几个周期（默认 1）
    cycles: Option<u32>,
    /// 账单号：写进每条续费记录的备注（「账单 xxx」）
    #[serde(rename = "ref")]
    bill_ref: Option<String>,
    note: Option<String>,
    /// 只预览：照常校验、写，读出结果后整个回滚，不记审计、不广播
    #[serde(default)]
    dry_run: bool,
}

/// 批量登记续费：一个事务里给每台登记（逻辑同单台 POST /machines/{id}/renewals，按周期推、金额默认订阅价），有一台不行整批都不登记；
/// 审计只记一条（列出机器）。`dry_run` 只预览。返回 `{dry_run, items: [{machine_id, name, old_due, new_due, amount_cents, currency, usd_cents}], total}`
pub async fn renew_batch(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<BatchIn>) -> ApiResult<Json<Value>> {
    if b.items.is_empty() || b.items.len() > MAX_BATCH {
        return Err(ApiError::bad_request(format!("一次登记 1–{MAX_BATCH} 台")));
    }
    let mut ids: Vec<Uuid> = b.items.iter().map(|i| i.machine_id).collect();
    ids.sort();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err(ApiError::bad_request("机器重复了"));
    }
    let names: Vec<(Uuid, String)> = sqlx::query_as(
        "select m.id, display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = any($1)",
    )
    .bind(&ids)
    .fetch_all(&st.db)
    .await?;
    if names.len() != ids.len() {
        return Err(ApiError::not_found("有机器不存在"));
    }
    let bill_ref = b.bill_ref.as_deref().map(str::trim).filter(|r| !r.is_empty());
    if bill_ref.is_some_and(|r| r.chars().count() > 64) {
        return Err(ApiError::bad_request("账单号不超过 64 个字"));
    }
    let note = [bill_ref.map(|r| format!("账单 {r}")), b.note.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(String::from)].into_iter().flatten().collect::<Vec<_>>().join("，");
    let cycles = b.cycles.unwrap_or(1);
    let rate = fx::current(&st.db).await?;
    let mut tx = st.db.begin().await?;
    let mut total = fx::Total::default();
    let mut items = vec![];
    // 按 id 顺序加锁（并发的批量不互相死锁），返回按请求的顺序
    let mut done = std::collections::HashMap::new();
    for id in &ids {
        let amount = b.items.iter().find(|i| i.machine_id == *id).and_then(|i| i.amount_cents);
        let name = &names.iter().find(|n| n.0 == *id).expect("上面核对过").1;
        let one = super::machines::RenewIn { new_due: None, cycles: Some(cycles), amount_cents: amount, note: Some(note.clone()) };
        let r = super::machines::renew_in_tx(&mut tx, &a.name, *id, &one, true).await.map_err(|mut e| {
            e.message = format!("{name}：{}", e.message);
            e
        })?;
        done.insert(*id, r);
    }
    for i in &b.items {
        let r = &done[&i.machine_id];
        let name = &names.iter().find(|n| n.0 == i.machine_id).expect("上面核对过").1;
        let cents = i64::from(r.amount_cents.unwrap_or(0));
        let (usd, rt) = fx::convert(&rate, cents, &r.currency);
        total.add(&r.currency, cents, usd.zip(rt));
        items.push(json!({"machine_id": i.machine_id, "name": name, "old_due": r.old_due, "new_due": r.new_due, "amount_cents": r.amount_cents, "currency": r.currency, "usd_cents": usd}));
    }
    if b.dry_run {
        tx.rollback().await?;
        return Ok(ok(json!({"dry_run": true, "items": items, "total": total})));
    }
    audit(&mut tx, &a.name, "renewal.batch", "machine", None, json!({"ref": bill_ref, "note": b.note, "cycles": cycles, "machines": items})).await?;
    tx.commit().await?;
    for id in &ids {
        st.emit(Event::Machine { id: *id });
    }
    Ok(ok(json!({"dry_run": false, "items": items, "total": total})))
}
