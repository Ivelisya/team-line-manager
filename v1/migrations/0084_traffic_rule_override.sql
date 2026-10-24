-- 0084：流量计费方式（单向 / 双向）订阅可以单独设置。有的机器流量只记入或出其中一个（单向计费），有的记入+出（双向），
-- 以前只能按供应商一刀切（provider.traffic_rule）。现在新增取值 in（只算下行 / 入站），订阅里 traffic_rule 为 null = 跟随供应商。
-- 各语句可重复执行。

-- 供应商的取值加 in（列上的行内 check 默认名 provider_traffic_rule_check，见 0020）
alter table provider drop constraint if exists provider_traffic_rule_check;
alter table provider add constraint provider_traffic_rule_check check (traffic_rule in ('min', 'max', 'sum', 'out', 'in'));

-- 订阅单独设置：null = 跟随供应商
alter table subscription add column if not exists traffic_rule text;
alter table subscription drop constraint if exists subscription_traffic_rule_check;
alter table subscription add constraint subscription_traffic_rule_check check (traffic_rule in ('min', 'max', 'sum', 'out', 'in'));

-- 模板末尾加供应商的计费方式（没有供应商时 max，与 provider 列默认一致）；视图只能在末尾加列，现有列顺序不动（其余同 0031）
create or replace view sub_template as
select m.id machine_id, pl.price_cents, coalesce(pl.currency, 'USD') currency, coalesce(pl.cycle, 'monthly'::billing_cycle) cycle,
       coalesce(pv.timezone, org_tz()) billing_tz, pl.traffic_gb, coalesce(pv.traffic_reset, 'month') traffic_reset,
       coalesce(pv.remind_days, 7) remind_days,
       coalesce(pv.traffic_rule, 'max') traffic_rule
from machine m
left join plan pl on pl.id = m.plan_id
left join provider pv on pv.id = coalesce(pl.provider_id, m.provider_id);

-- 生效值末尾加 traffic_rule，overrides 多一个 'traffic_rule'（其余同 0032）
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
           case when s.traffic_reset_day is not null then 'traffic_reset_day' end,
           case when s.traffic_rule is not null then 'traffic_rule' end], null)::text[] overrides,
       coalesce(s.traffic_rule, t.traffic_rule) traffic_rule
from subscription s join sub_template t on t.machine_id = s.machine_id;

-- 与模板相同的单独设置改回 null（跟随）（其余同 0032）
create or replace function subscription_normalize(mid uuid) returns void language sql as $$
    update subscription s set
        price_cents = nullif(s.price_cents, t.price_cents),
        currency = nullif(s.currency, t.currency),
        cycle = nullif(s.cycle, t.cycle),
        billing_tz = nullif(s.billing_tz, t.billing_tz),
        remind_days = nullif(s.remind_days, t.remind_days),
        traffic_gb = nullif(s.traffic_gb, coalesce(t.traffic_gb, 0)),
        traffic_reset_day = nullif(s.traffic_reset_day, sub_reset_day(t.traffic_reset, s.due_date, s.billing_day)),
        traffic_rule = nullif(s.traffic_rule, t.traffic_rule)
    from sub_template t
    where t.machine_id = s.machine_id and (mid is null or s.machine_id = mid)
$$;
