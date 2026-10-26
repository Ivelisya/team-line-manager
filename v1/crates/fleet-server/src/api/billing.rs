//! 待付清单（GET /billing/due）与批量登记续费（POST /renewals/batch）。单台续费的逻辑在 machines::renew_in_tx，批量一个事务里逐台调它。

use super::ok;
use crate::auth::Actor;
use crate::db::audit;
use crate::error::{ApiError, ApiJson, ApiQuery, ApiResult};
use super::machines;
use crate::fx;
use crate::meta;
use crate::routers;
use crate::traffic;
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
    /// 带上备用团队的闲置备用机（它们等到期丢弃，默认不算待付）。还被软路由出口指着、或上面有在用同机 IP 节点的备用机不闲置，不用带这个也在清单里
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
    billing_tz: String,
    auto_renew: bool,
    price_cents: i32,
    currency: String,
    cycle: String,
    spare: bool,
    /// 备用机上还在用的同机 IP 节点（不在备用团队、没退役的，显示名用「、」连起来），没有为 null
    live_ips: Option<String>,
    /// 上架已满 `traffic::IDLE_DAYS` 天（不满不判闲置）
    aged: bool,
}

/// 待付清单：在用机器里到期日落在窗口内的，按 供应商 × 到期日 × 账单时区 分组（剩余天数 `due_days` 按各自账单时区的今天算，
/// 同一天到期、时区不同的差一天，分开成组、组上带 `billing_tz`），每组列机器、原币种小计与 USD 合计（`total`，同总览月费合计的形状）。
/// 开了自动续费的不算待付，单列在 `auto`（到期后由中控按周期顺延，只记账）。`groups[].machines[].price_cents` 是订阅的每期价格（登记已付的默认金额）。
/// 每台另带续不续费的依据（近 30 天）：`monthly_usd_cents` 月费、`bytes_30d` 业务流量、`usd_per_gb`（月费 ÷ 近 30 天业务流量，没有月费或没流量为 null）、
/// `avail_pct`（出口探测成功率，没有探测为 null）、`idle`（闲置，判定见 traffic.rs）与 `avg7`（近 7 天日均业务流量，字节）。
/// 备用团队里还被软路由出口指着、或上面有在用同机 IP 节点的机器（不自动回收，不算闲置备用）默认也在清单里，机器项 `spare` 为 true、`spare_hold` 写明原因；其余备用机要 `include_spare`
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
                s.due_date, (s.due_date - (now() at time zone e.billing_tz)::date) due_days, e.billing_tz, e.auto_renew, e.price_cents, e.currency, e.cycle::text cycle,
                coalesce(t.spare, false) spare,
                (select string_agg(display_name(x.kind::text, x.label, tx.name, x.number), '、' order by tx.sort_order nulls first, tx.name, x.number, x.id)
                 from machine x left join team tx on tx.id = x.team_id
                 where x.host_id = m.id and x.lifecycle <> 'retired' and not coalesce(tx.spare, false)) live_ips,
                coalesce(m.activated_at <= now() - make_interval(days => $3), false) aged
         from subscription s join subscription_eff e on e.machine_id = s.machine_id join machine m on m.id = s.machine_id
         left join team t on t.id = m.team_id left join plan pl on pl.id = m.plan_id left join provider pv on pv.id = coalesce(pl.provider_id, m.provider_id)
         where m.lifecycle = 'active'
           and s.due_date <= coalesce($1::date, (now() at time zone e.billing_tz)::date + $2::int)
         order by s.due_date, pv.name nulls last, m.kind, t.sort_order, m.number, m.id",
    )
    .bind(q.until)
    .bind(days as i32)
    .bind(traffic::IDLE_DAYS as i32)
    .fetch_all(&st.db)
    .await?;
    // 备用团队的机器：闲置的等到期丢弃（要 include_spare 才带上）；还被出口指着、上面有在用 IP 节点的不自动回收，照常续费
    let spare_ids: Vec<Uuid> = rows.iter().filter(|r| r.spare).map(|r| r.id).collect();
    let pointed = routers::exits_using_with_ip_nodes(&st.db, &spare_ids).await?;
    let hold = |r: &DueRow| -> Option<String> {
        let mut why = vec![];
        if let Some(x) = &r.live_ips {
            why.push(format!("同机 IP 节点 {x} 还在用"));
        }
        if let Some(x) = pointed.get(&r.id) {
            why.push(format!("软路由出口 {} 还指着", x.iter().map(|e| e.label()).collect::<Vec<_>>().join("、")));
        }
        (r.spare && !why.is_empty()).then(|| why.join("；"))
    };
    let rows: Vec<(DueRow, Option<String>)> = rows
        .into_iter()
        .map(|r| {
            let h = hold(&r);
            (r, h)
        })
        .filter(|(r, h)| q.include_spare || !r.spare || h.is_some())
        .collect();
    let rate = fx::current(&st.db).await?;
    // 续不续费的依据：近 30 天的业务流量（USD/GB、闲置，口径同流量页）与可用率（出口探测成功率，同 availability / fleetctl uptime，按这台的账单时区分天）
    let uses = traffic::usage(&st).await?;
    let tzs: Vec<(Uuid, String)> = rows.iter().map(|(r, _)| (r.id, r.billing_tz.clone())).collect();
    let mut avail = machines::availability_many(&st.db, &tzs, traffic::COST_DAYS as i32).await?;
    let mut total = fx::Total::default();
    // 按 (供应商, 到期日, 账单时区) 分组：供应商 JSON、组内合计、组内机器
    type Group = (Option<Uuid>, Date, String, Value, fx::Total, Vec<Value>);
    let mut groups: Vec<Group> = vec![];
    let mut auto = vec![];
    for (r, spare_hold) in rows {
        let egress_pct = avail.remove(&r.id).and_then(|(_, _, egress_pct)| egress_pct);
        let provider = json!({"id": r.provider_id, "name": r.provider, "panel_url": r.panel_url});
        let (usd, r_) = fx::convert(&rate, i64::from(r.price_cents), &r.currency);
        let u = uses.get(&r.id).copied().unwrap_or_default();
        let monthly_usd = fx::convert(&rate, meta::monthly_cents(r.price_cents, &r.cycle), &r.currency).0;
        let m = json!({
            "id": r.id, "name": r.name, "kind": r.kind, "due_date": r.due_date, "due_days": r.due_days, "billing_tz": r.billing_tz, "spare": r.spare, "spare_hold": spare_hold, "cycle": r.cycle,
            "price_cents": r.price_cents, "currency": r.currency, "price_usd_cents": usd,
            "monthly_usd_cents": monthly_usd, "bytes_30d": u.bytes30, "usd_per_gb": traffic::usd_per_gb(monthly_usd, u.bytes30, traffic::COST_DAYS),
            "idle": traffic::is_idle(u.sum7, r.aged), "avg7": u.sum7 / traffic::IDLE_DAYS, "avail_pct": egress_pct,
        });
        if r.auto_renew {
            auto.push(json!({"provider": provider, "machine": m}));
            continue;
        }
        total.add(&r.currency, i64::from(r.price_cents), usd.zip(r_));
        let g = match groups.iter().position(|g| g.0 == r.provider_id && g.1 == r.due_date && g.2 == r.billing_tz) {
            Some(i) => i,
            None => {
                groups.push((r.provider_id, r.due_date, r.billing_tz.clone(), provider, fx::Total::default(), vec![]));
                groups.len() - 1
            }
        };
        groups[g].4.add(&r.currency, i64::from(r.price_cents), usd.zip(r_));
        groups[g].5.push(m);
    }
    let groups: Vec<Value> = groups
        .into_iter()
        .map(|(_, due_date, tz, provider, t, machines)| json!({"provider": provider, "due_date": due_date, "billing_tz": tz, "machines": machines, "total": t}))
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
        st.emit_keep_access(Event::Machine { id: *id });
    }
    Ok(ok(json!({"dry_run": false, "items": items, "total": total})))
}
