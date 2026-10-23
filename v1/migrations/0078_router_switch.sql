-- 0078：软路由 select 组的临时切换（经 mihomo API PUT /proxies/<组>，不改配置、不重启 OpenClash，见 routers/switch.rs）。
-- 一组同时只有一条进行中的（state = active）：from_exit 是第一次切之前选中的（切回、到时自动切回都回到它），to_exit 是现在切到的。
-- back_at 为空 = 不自动切回。结束时 state 记怎么结束的：back（切回了：手动或到时）/ reverted（路由自己改回了：重启、OpenClash 重载后
-- 组选择回到配置默认值，采集时发现选中的不是 to_exit，待切回随之取消）/ gone（路由上已经没有这个组）。
create table if not exists router_switch (
    id          uuid primary key,
    router_id   uuid not null references router on delete cascade,
    grp         text not null,
    from_exit   text not null,
    to_exit     text not null,
    back_at     timestamptz,
    state       text not null default 'active' check (state in ('active', 'back', 'reverted', 'gone')),
    created_by  text not null,
    -- 切换在路由上生效（PUT 之后读回确认）的时刻：采集比对只看这之后开始的采集
    switched_at timestamptz not null,
    ended_at    timestamptz,
    end_note    text
);
create unique index if not exists router_switch_active on router_switch (router_id, grp) where state = 'active';
create index if not exists router_switch_due on router_switch (back_at) where state = 'active' and back_at is not null;
