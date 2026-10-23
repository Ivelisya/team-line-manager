-- 0060：访问监控的天级汇总。7d / 30d 窗口原来直接聚合 access_hourly（30 天就是 720 个小时的全部行），
-- 改成整天读这张表、首尾不满一天的部分读小时表（access::cte）。
-- 一天 = UTC 零点起 24 小时（与小时表的 UTC 整点对齐）。每行是那天各小时行的合计：conns、blocked 相加，sources 取各小时的最大值
-- （同小时表跨小时的口径），kind 取各小时里的（同一目标各小时相同）。由访问汇总在重算某个小时的同一事务里重算它所在的那天
-- （access::rollup_hour），保留期同小时汇总（access::prune）。主键与索引照 access_hourly。

create table access_daily (
    node_id    uuid not null,
    relay_id   uuid not null,
    day        timestamptz not null,
    dest_host  text not null,
    kind       text not null check (kind in ('domain', 'ip')),
    conns      bigint not null,
    blocked    bigint not null default 0,
    sources    int not null default 0,
    primary key (node_id, day, relay_id, dest_host)
);
create index access_daily_day on access_daily (day);
create index access_daily_relay on access_daily (relay_id, day);
create index access_daily_dest on access_daily (dest_host, day);
-- 当天的行每 5 分钟重算一次，只改数值列：留空间走 HOT 更新（同 0016 对小时表的设置）
alter table access_daily set (fillfactor = 70);

-- 已有的小时汇总按天补进来（生产几十万到一两百万行，秒级）
insert into access_daily (node_id, relay_id, day, dest_host, kind, conns, blocked, sources)
select node_id, relay_id, date_trunc('day', hour, 'UTC'), dest_host, min(kind), sum(conns), sum(blocked), max(sources)
from access_hourly
group by node_id, relay_id, date_trunc('day', hour, 'UTC'), dest_host;
