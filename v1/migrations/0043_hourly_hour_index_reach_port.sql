-- 0043：可重复执行。
-- 1. host_metric_hourly (hour)：网卡今天 / 本月流量（views.rs flows）按 hour 范围过滤整张表，主键 (machine_id, hour) 用不上，是全表顺扫，
--    耗时随总历史增长（表没有保留期）。入口小时流量表的同款索引 0015 已建（entry_traffic_hourly_hour），实验室表小所以计划选了顺扫，不用补。
--    用普通 create index：sqlx 迁移在事务里，不能 create index concurrently；这张表每小时批量写一次，生产几万行、
--    实验室一年约百万行量级，建索引是秒级以内，期间只挡这一次小时写入（等锁而已）。
-- 2. relay_reach (relay_id, port, ts desc)：机器视图里每条入口取中控对它端口的最近一次检查（views.rs reach_of），逐条走这个索引。
create index if not exists host_metric_hourly_hour on host_metric_hourly (hour);
create index if not exists relay_reach_relay_port_ts on relay_reach (relay_id, port, ts desc);
