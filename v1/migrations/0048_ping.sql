-- 0048：三网 PING（线路机往外测线路质量，见 ping.rs、DESIGN.md「三网 PING」）。0047 留给别的改动。
-- 1. relay.ping_override：这台线路机单独设置的目标与模式（{"mode": "tcp"|"icmp"|null, "slots": [null | {"name","target"}] × 8}）；
--    null = 全部跟随全局（app_setting 的 ping_targets）。
-- 2. ping_result：线路机上的 agent 每分钟一轮、每个目标发 5 次（TCP 建连或 ICMP），记延迟中位数与丢了几次。每台每目标每分钟一行，
--    保留 7 天（ping.rs 的后台循环清理）。target / mode 记的是当时实际测的（改了目标之后新旧不混在一起算）。
-- 3. ping_hourly：小时汇总（发了几次、丢了几次、延迟的平均与最大），保留 90 天；后台每 5 分钟重算最近 7 小时（覆盖 agent 离线补传）。
alter table relay add column if not exists ping_override jsonb;

create table ping_result (
    relay_id  uuid not null references machine on delete cascade,
    slot      smallint not null,
    ts        timestamptz not null,
    target    text not null,
    mode      text not null,
    sent      smallint not null,
    lost      smallint not null,
    rtt_ms    real,
    primary key (relay_id, slot, ts)
);
create index ping_result_ts on ping_result (ts);

create table ping_hourly (
    relay_id  uuid not null references machine on delete cascade,
    slot      smallint not null,
    hour      timestamptz not null,
    target    text not null,
    sent      int not null,
    lost      int not null,
    rtt_avg   real,
    rtt_max   real,
    primary key (relay_id, slot, hour)
);
create index ping_hourly_hour on ping_hourly (hour);
