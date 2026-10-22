-- 0026：商家（供应商）→ 套餐 → 机器，订阅跟随模板。
-- 以前上架时把套餐的价格 / 币种 / 周期、供应商时区抄进订阅（之后改套餐不会跟着变），流量上限、重置日、提醒天数也是每台手填。
-- 现在订阅只存到期日、自动续费和「单独设置」的值（null = 跟随）：价格、币种、周期、流量上限跟随套餐，
-- 账单时区、提前提醒天数、流量重置方式跟随供应商。生效值只在 subscription_eff 一处算。各语句可重复执行。

-- 供应商的模板字段。traffic_reset：month = 账单时区的自然月（每月 1 号）；billing = 每个账单日（到期日的那一天，小月按月末）
alter table provider add column if not exists traffic_reset text not null default 'month' check (traffic_reset in ('month', 'billing'));
alter table provider add column if not exists remind_days int not null default 7 check (remind_days between 0 and 60);

-- 这几项变成「单独设置」：null = 跟随模板，没有列默认值（默认值只在 sub_template 里）
alter table subscription alter column price_cents drop not null,
    alter column currency drop not null, alter column currency drop default,
    alter column cycle drop not null, alter column cycle drop default,
    alter column billing_tz drop not null, alter column billing_tz drop default,
    alter column remind_days drop not null, alter column remind_days drop default;

-- 机器的供应商就是套餐的（套餐属于一个供应商）。以前两处各存一份、没人保证一致：按套餐的修正机器上的那份
-- （不清空：同机 IP 节点、没有套餐的机器仍靠 machine.provider_id，读它的地方也不少）。之后改套餐 / 上架都按套餐的写
update machine m set provider_id = pl.provider_id from plan pl where pl.id = m.plan_id and m.provider_id is distinct from pl.provider_id;

-- 机器的订阅模板：套餐给价格、币种、周期、流量上限（null = 不限），供应商给账单时区、提前提醒天数、流量重置方式。
-- 供应商取套餐的，没有套餐取机器的；都没有时用默认值（USD、按月、北京时间、提前 7 天、自然月）。没有套餐时价格为 null（订阅必须单独设置价格）
create or replace view sub_template as
select m.id machine_id, pl.price_cents, coalesce(pl.currency, 'USD') currency, coalesce(pl.cycle, 'monthly'::billing_cycle) cycle,
       coalesce(pv.timezone, 'Asia/Shanghai') billing_tz, pl.traffic_gb, coalesce(pv.traffic_reset, 'month') traffic_reset,
       coalesce(pv.remind_days, 7) remind_days
from machine m
left join plan pl on pl.id = m.plan_id
left join provider pv on pv.id = coalesce(pl.provider_id, m.provider_id);

-- 跟随供应商时的流量重置日：billing = 到期日是几号就几号（月份没有这一天时按月末，见 views::period_start），month = 1 号
create or replace function sub_reset_day(rule text, due date) returns int language sql immutable as $$
    select case rule when 'billing' then extract(day from due)::int else 1 end
$$;

-- 订阅的生效值（机器视图、费用统计、到期提醒、流量周期、额度告警、可用率分天都读它）；overrides = 单独设置了的字段
create or replace view subscription_eff as
select s.machine_id, s.due_date, s.auto_renew, s.traffic_used_base, s.traffic_used_base_at, s.updated_at,
       coalesce(s.price_cents, t.price_cents, 0) price_cents,
       coalesce(s.currency, t.currency) currency,
       coalesce(s.cycle, t.cycle) cycle,
       coalesce(s.billing_tz, t.billing_tz) billing_tz,
       coalesce(s.remind_days, t.remind_days) remind_days,
       coalesce(s.traffic_gb, t.traffic_gb) traffic_gb,
       coalesce(s.traffic_reset_day, sub_reset_day(t.traffic_reset, s.due_date)) traffic_reset_day,
       array_remove(array[
           case when s.price_cents is not null then 'price_cents' end,
           case when s.currency is not null then 'currency' end,
           case when s.cycle is not null then 'cycle' end,
           case when s.billing_tz is not null then 'billing_tz' end,
           case when s.remind_days is not null then 'remind_days' end,
           case when s.traffic_gb is not null then 'traffic_gb' end,
           case when s.traffic_reset_day is not null then 'traffic_reset_day' end], null)::text[] overrides
from subscription s join sub_template t on t.machine_id = s.machine_id;

-- 和模板相同的单独设置改回跟随（null）：写订阅、上架后都调用；mid 为 null = 全部。
-- 流量上限 0 与套餐的 null 都是不限，也算相同
create or replace function subscription_normalize(mid uuid) returns void language sql as $$
    update subscription s set
        price_cents = nullif(s.price_cents, t.price_cents),
        currency = nullif(s.currency, t.currency),
        cycle = nullif(s.cycle, t.cycle),
        billing_tz = nullif(s.billing_tz, t.billing_tz),
        remind_days = nullif(s.remind_days, t.remind_days),
        traffic_gb = nullif(s.traffic_gb, coalesce(t.traffic_gb, 0)),
        traffic_reset_day = nullif(s.traffic_reset_day, sub_reset_day(t.traffic_reset, s.due_date))
    from sub_template t
    where t.machine_id = s.machine_id and (mid is null or s.machine_id = mid)
$$;

-- 供应商在账单日重置流量（如 GOMAMI）：它名下填了重置日的订阅全都填成到期日那一天，且至少有一台不是 1 号
-- （1 号到期的两种方式一样，不算证据）。只看数据，不按名字
update provider p set traffic_reset = 'billing'
where p.traffic_reset = 'month' and p.id in (
    select t.pid
    from subscription s
    join (select m.id, coalesce(pl.provider_id, m.provider_id) pid from machine m left join plan pl on pl.id = m.plan_id) t on t.id = s.machine_id
    where s.traffic_reset_day is not null and t.pid is not null
    group by t.pid
    having bool_and(s.traffic_reset_day = extract(day from s.due_date)::int) and bool_or(s.traffic_reset_day <> 1));

-- 已有订阅里和模板相同的值（上架时抄的价格、时区，手填的套餐流量、账单日，默认的 7 天提醒）改回跟随；不同的留作单独设置
select subscription_normalize(null);
