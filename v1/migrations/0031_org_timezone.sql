-- 0031：组织时区（fleet.toml 的 org_timezone，默认北京时间）只在配置里写一次：中控启动时写进 app_setting（0028 的设置表，key = org_timezone），
-- 库里的兜底都读 org_tz()。以前各处各写一个：订阅模板没有供应商时写死北京时间，供应商时区留空时是列默认值 UTC，网页又预填北京时间。各语句可重复执行。
-- 启动时按配置覆盖；这里的值只在第一次启动前（或只跑迁移时）用
insert into app_setting (key, value, updated_by) values ('org_timezone', '"Asia/Shanghai"', 'fleet.toml') on conflict do nothing;

create or replace function org_tz() returns text language sql stable as $$
    select coalesce((select value #>> '{}' from app_setting where key = 'org_timezone'), 'Asia/Shanghai')
$$;

-- 供应商时区可以不填：null = 跟随组织时区（不再是列默认值 UTC）。已有的值不动——以前「留空 = UTC」存下的 UTC 和有意填的 UTC 分不出来
alter table provider alter column timezone drop not null, alter column timezone drop default;

-- 订阅模板：账单时区跟随供应商，供应商没填或没有供应商时是组织时区（其余同 0026）
create or replace view sub_template as
select m.id machine_id, pl.price_cents, coalesce(pl.currency, 'USD') currency, coalesce(pl.cycle, 'monthly'::billing_cycle) cycle,
       coalesce(pv.timezone, org_tz()) billing_tz, pl.traffic_gb, coalesce(pv.traffic_reset, 'month') traffic_reset,
       coalesce(pv.remind_days, 7) remind_days
from machine m
left join plan pl on pl.id = m.plan_id
left join provider pv on pv.id = coalesce(pl.provider_id, m.provider_id);
