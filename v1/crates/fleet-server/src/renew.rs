//! 自动续费：开了自动续费（subscription.auto_renew）的在用机器，账单时区的今天晚于到期日时按订阅周期顺延，推到不早于今天
//! （账单日不变、小月按月末，同手动「按周期续费」）；每推一期记一条续费记录（金额取生效价格，备注「自动续费」，操作人「系统」）并写审计。
//! 每小时的维护任务（lib.rs housekeeping）在备用到期回收（spare.rs）之前调 `run_once`：开了自动续费的备用机器不会因为「过期」被回收。
//! 每台机器单独处理：一台出错记日志后继续下一台，不拖累排在后面的。
//! 只记账，不会向供应商付款（供应商那边要自己设好自动扣费）。

use crate::db::{audit, new_id};
use crate::meta;
use crate::spare::ACTOR;
use crate::state::{AppState, Event};
use serde_json::json;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

/// 续费记录的备注
pub const NOTE: &str = "自动续费";

/// 从 `due` 起按周期一期一期往后推，直到不早于 `today`：[(旧到期日, 新到期日)]。到期日不早于今天的不用推（空）
pub fn periods(due: Date, today: Date, months: u32, bday: u8) -> Vec<(Date, Date)> {
    let mut out = vec![];
    let mut cur = due;
    while cur < today {
        let Some(next) = meta::add_months_on(cur, i64::from(months), bday) else { break };
        out.push((cur, next));
        cur = next;
    }
    out
}

#[derive(sqlx::FromRow)]
struct Due {
    id: Uuid,
    name: String,
    due: Date,
    bday: i32,
    cycle: String,
    price: Option<i32>,
    currency: String,
    today: Date,
}

/// `now`：按这一刻算各订阅账单时区里的「今天」（测试传固定时刻）。返回续了费的机器
pub async fn run_once(st: &AppState, now: OffsetDateTime) -> anyhow::Result<Vec<Uuid>> {
    let rows: Vec<Due> = sqlx::query_as(
        "select s.machine_id id, display_name(m.kind::text, m.label, t.name, m.number) name, s.due_date due,
                coalesce(s.billing_day::int, extract(day from s.due_date)::int) bday, e.cycle::text cycle, e.price_cents price, e.currency,
                ($1::timestamptz at time zone e.billing_tz)::date today
         from subscription s join subscription_eff e on e.machine_id = s.machine_id join machine m on m.id = s.machine_id left join team t on t.id = m.team_id
         where e.auto_renew and m.lifecycle = 'active' and s.due_date < ($1::timestamptz at time zone e.billing_tz)::date
         order by s.machine_id",
    )
    .bind(now)
    .fetch_all(&st.db)
    .await?;
    let mut done = vec![];
    // 每台自己的事务，一台出错记日志后继续下一台：不然排在后面的机器全续不上，到期后还会被备用回收误清
    for r in rows {
        match renew_one(st, &r).await {
            Ok(true) => done.push(r.id),
            Ok(false) => {}
            Err(e) => tracing::error!(machine = %r.id, "{} 自动续费失败：{e:#}", r.name),
        }
    }
    Ok(done)
}

/// 续一台：true = 顺延了
async fn renew_one(st: &AppState, r: &Due) -> anyhow::Result<bool> {
    let Some(months) = meta::cycle_months(&r.cycle) else {
        tracing::warn!(machine = %r.id, "{} 开了自动续费，但不认识它的计费周期「{}」：没有顺延", r.name, r.cycle);
        return Ok(false);
    };
    let ps = periods(r.due, r.today, months, r.bday.clamp(1, 31) as u8);
    let Some(&(_, last)) = ps.last() else { return Ok(false) };
    let mut tx = st.db.begin().await?;
    // 锁住订阅再核对一次：读出来之后有人手动登记了续费的，这一轮不动
    let cur: Option<Date> = sqlx::query_scalar("select due_date from subscription where machine_id = $1 for update").bind(r.id).fetch_optional(&mut *tx).await?;
    if cur != Some(r.due) {
        return Ok(false);
    }
    for (old, new) in &ps {
        sqlx::query("insert into renewal (id, machine_id, old_due, new_due, amount_cents, currency, note, actor) values ($1, $2, $3, $4, $5, $6, $7, $8)")
            .bind(new_id())
            .bind(r.id)
            .bind(old)
            .bind(new)
            .bind(r.price)
            .bind(&r.currency)
            .bind(NOTE)
            .bind(ACTOR)
            .execute(&mut *tx)
            .await?;
        audit(&mut tx, ACTOR, "renewal", "machine", Some(r.id), json!({"name": r.name, "old_due": old, "new_due": new, "amount_cents": r.price, "currency": r.currency, "cycles": 1, "auto": true}))
            .await?;
    }
    // 账单日不变（推出来的日子都落在账单日上）
    sqlx::query("update subscription set due_date = $2, updated_at = now() where machine_id = $1").bind(r.id).bind(last).execute(&mut *tx).await?;
    tx.commit().await?;
    tracing::info!(machine = %r.id, "{} 自动续费：到期日 {} → {}（{} 期）", r.name, r.due, last, ps.len());
    st.emit_keep_access(Event::Machine { id: r.id });
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn periods_reach_today_on_billing_day() {
        // 到期日当天和之前不用推；过了一天推一期
        assert!(periods(date!(2026 - 10 - 20), date!(2026 - 10 - 20), 1, 20).is_empty());
        assert_eq!(periods(date!(2026 - 10 - 20), date!(2026 - 10 - 21), 1, 20), [(date!(2026 - 10 - 20), date!(2026 - 11 - 20))]);
        // 漏了几期：一期一期推到不早于今天，账单日 31 号在小月按月末、之后回到 31 号
        assert_eq!(
            periods(date!(2026 - 01 - 31), date!(2026 - 03 - 15), 1, 31),
            [(date!(2026 - 01 - 31), date!(2026 - 02 - 28)), (date!(2026 - 02 - 28), date!(2026 - 03 - 31))]
        );
        // 季付
        assert_eq!(periods(date!(2026 - 10 - 01), date!(2026 - 10 - 02), 3, 1), [(date!(2026 - 10 - 01), date!(2027 - 01 - 01))]);
    }
}
