-- 0041：生命周期改为「退役 = 进备用 → 到期满 1 天 → 清理 → 回收站」（见 spare.rs、recycle.rs）。可重复执行。
-- 1. machine.spare_since / spare_from：进备用团队的时刻、进备用前的显示名（「原 团队2-07」），离开备用时清空（machines::patch_tx 统一维护）。
--    machine.recycled_at：进回收站的时刻。回收后机器从所有视图里消失；它的入口与小时流量还留着给线路机的统计用，
--    db::purge_blocker 放行后由中控自动物理删除（最多约 35 天）。
-- 2. 备用团队只有一个（唯一索引 team_one_spare）：多于一个时报错，先只留一个再升级；一个都没有时把名叫「备用」的团队设为备用，
--    没有这个名字的就新建一个「备用」（排在最后）。改名用现成的 PATCH /teams/{id}。
-- 3. recycle_bin：回收站（审计表）。每台回收的机器一行：退役前的名字、团队、供应商与套餐、价格周期、上架 / 退役 / 回收时间、原因、
--    流量合计与回收时的快照（不含秘密）；旧面板的退役机器也导进这里（source = legacy）。不受保留期清理影响，一直保留。
alter table machine add column if not exists spare_since timestamptz,
                    add column if not exists spare_from text,
                    add column if not exists recycled_at timestamptz;

do $$
declare n int := (select count(*) from team where spare);
begin
    if n > 1 then
        raise exception '有 % 个备用团队：备用团队只能有一个，先把其余的取消备用（机器移进留下的那个）再升级', n;
    end if;
end $$;
create unique index if not exists team_one_spare on team (spare) where spare;
update team set spare = true where name = '备用' and not exists (select 1 from team where spare);
insert into team (id, name, sort_order, spare)
select gen_random_uuid(), '备用', 999, true where not exists (select 1 from team where spare);

create table if not exists recycle_bin (
    id           bigint generated always as identity primary key,
    source       text not null check (source in ('fleet', 'legacy')),
    -- fleet：machine.id；legacy：旧面板资产 uuid（没有就用「供应商:service_id」）
    source_id    text not null,
    -- fleet 才有；不设外键（机器行之后会被物理删除）
    machine_id   uuid,
    kind         text not null check (kind in ('vps', 'proxy', 'relay')),
    -- 退役前的名字：spare_from，没进过备用就是当时的显示名
    name         text not null,
    -- 回收时的名字（备用-03），立即回收的和 name 相同
    last_name    text,
    -- 退役前的团队名
    team         text,
    ip           inet,
    -- 同机 IP 节点：宿主机当时的名字
    host_name    text,
    provider     text,
    plan         text,
    service_ref  text,
    price_cents  int,
    currency     text,
    -- subscription_eff 的生效值
    cycle        text,
    activated_at timestamptz,
    -- 退役时刻：spare_since，立即回收的是退役作业完成的时刻
    retired_at   timestamptz,
    due_date     date,
    recycled_at  timestamptz not null default now(),
    reason       text,
    -- 入口业务流量合计（字节，整个生命周期；线路机按它上面全部入口算）
    traffic_up   bigint,
    traffic_down bigint,
    -- 网卡累计（host_metric_hourly，有网卡读数的才有）
    nic_rx       bigint,
    nic_tx       bigint,
    -- 系统 / 用户名 / 导入人
    actor        text not null,
    snapshot     jsonb not null,
    unique (source, source_id)
);
create index if not exists recycle_bin_at on recycle_bin (recycled_at desc);

-- 4. 续费告警合成一条规则：即将到期与已过期都是 renewal_due（到期后从警告升到严重，不再先发一条「已恢复」）。
--    还开着的 renewal_overdue 改名接上（同一台不会同时开着两条：即将到期与已过期互斥），已恢复的旧记录不动
update incident set rule = 'renewal_due' where rule = 'renewal_overdue' and state = 'firing';
