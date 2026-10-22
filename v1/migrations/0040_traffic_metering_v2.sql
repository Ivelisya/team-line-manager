-- 0040：流量计量 v2。
-- 1. 供应商的流量重置方式加 rolling30（滚动 30 天，Blossom）：周期起点 = 账单时区的今天往前 29 天，没有重置日
--    （views.rs）。跟随供应商时的生效重置日为 null；订阅单独设置了重置日的照旧按月。
-- 2. 网卡小时汇总改为跨缺口求差（db::rollup_hour：本小时第一个样本的前值 = 该机器本小时之前的最后一个样本）：
--    原始数据还在的最近 3 天全部标脏，上线后几轮按新规则重算（原始数据已删的小时不产生新行，旧行不动；一个小时的原始数据
--    要么整天分区都在、要么整个删了，不会只剩半小时）。新库（没有原始数据）不标。
-- 3. 按面板校准（subscription.traffic_used_base / _at）停用：接口不再接受新值，已录的只读保留到各自下一个周期自动失效，
--    之后的版本再删列。
-- 各语句可重复执行。

alter table provider drop constraint if exists provider_traffic_reset_check;
alter table provider add constraint provider_traffic_reset_check check (traffic_reset in ('month', 'billing', 'rolling30'));

-- 同 0032，rolling30 没有重置日（subscription_eff / subscription_normalize 不用改：nullif(x, null) = x，单独设置的照旧保留）
create or replace function sub_reset_day(rule text, due date, billing_day int) returns int language sql immutable as $$
    select case rule when 'billing' then coalesce(billing_day, extract(day from due)::int) when 'rolling30' then null else 1 end
$$;

insert into rollup_dirty (hour, touched_at)
select h, now() from generate_series(date_trunc('hour', now()) - interval '3 days', date_trunc('hour', now()) - interval '1 hour', interval '1 hour') h
where exists (select 1 from host_metric)
on conflict (hour) do nothing;

comment on column subscription.traffic_used_base is '只读：割接时按供应商面板录的接入前读数（字节），接口已不收新值，进入下一个周期自动失效，待删';
comment on column subscription.traffic_used_base_at is '只读：接入前读数的录入时刻，待删';
