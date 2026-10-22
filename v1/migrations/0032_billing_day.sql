-- 0032：订阅的账单日（billing_day，每月几号）单独存一份。以前按周期续费从当前到期日推，小月落到月末之后就回不去了：
-- 1-31 → 2-28 → 3-28，按账单日重置流量（供应商 traffic_reset = billing）的重置日也跟着漂。
-- 现在到期日被直接设定时（改订阅改了到期日、上架、续费给新到期日）记下它是几号；按周期续费不动它，新到期日落在目标月的这一天（没有就月末）。
-- 各语句可重复执行。

alter table subscription add column if not exists billing_day smallint check (billing_day between 1 and 31);
-- 已有的取当前到期日几号（已经漂了的找不回来，下次直接设到期日时改正）
update subscription set billing_day = extract(day from due_date)::smallint where billing_day is null;

-- 跟随供应商时的流量重置日：billing = 账单日（没记账单日的取到期日几号；月份没有这一天时按月末，见 views::period_start），month = 1 号
create or replace function sub_reset_day(rule text, due date, billing_day int) returns int language sql immutable as $$
    select case rule when 'billing' then coalesce(billing_day, extract(day from due)::int) else 1 end
$$;

-- 同 0026，只是重置日按账单日算（列不变：0026 照旧可以重跑）
create or replace view subscription_eff as
select s.machine_id, s.due_date, s.auto_renew, s.traffic_used_base, s.traffic_used_base_at, s.updated_at,
       coalesce(s.price_cents, t.price_cents, 0) price_cents,
       coalesce(s.currency, t.currency) currency,
       coalesce(s.cycle, t.cycle) cycle,
       coalesce(s.billing_tz, t.billing_tz) billing_tz,
       coalesce(s.remind_days, t.remind_days) remind_days,
       coalesce(s.traffic_gb, t.traffic_gb) traffic_gb,
       coalesce(s.traffic_reset_day, sub_reset_day(t.traffic_reset, s.due_date, s.billing_day)) traffic_reset_day,
       array_remove(array[
           case when s.price_cents is not null then 'price_cents' end,
           case when s.currency is not null then 'currency' end,
           case when s.cycle is not null then 'cycle' end,
           case when s.billing_tz is not null then 'billing_tz' end,
           case when s.remind_days is not null then 'remind_days' end,
           case when s.traffic_gb is not null then 'traffic_gb' end,
           case when s.traffic_reset_day is not null then 'traffic_reset_day' end], null)::text[] overrides
from subscription s join sub_template t on t.machine_id = s.machine_id;

create or replace function subscription_normalize(mid uuid) returns void language sql as $$
    update subscription s set
        price_cents = nullif(s.price_cents, t.price_cents),
        currency = nullif(s.currency, t.currency),
        cycle = nullif(s.cycle, t.cycle),
        billing_tz = nullif(s.billing_tz, t.billing_tz),
        remind_days = nullif(s.remind_days, t.remind_days),
        traffic_gb = nullif(s.traffic_gb, coalesce(t.traffic_gb, 0)),
        traffic_reset_day = nullif(s.traffic_reset_day, sub_reset_day(t.traffic_reset, s.due_date, s.billing_day))
    from sub_template t
    where t.machine_id = s.machine_id and (mid is null or s.machine_id = mid)
$$;

drop function if exists sub_reset_day(text, date);
