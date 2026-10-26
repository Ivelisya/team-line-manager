-- 0091：软路由指标里单记 fleet 出口（对上了面板入口的）的总数与可用数。
-- 曲线页「可用出口」原来是全部出口（含客户自己加的），和卡片、告警只数 fleet 出口的口径不一致；之后的点两列都有值。
-- 旧的点这两列为空，曲线读的时候退回 exits_total / exits_alive（全部出口）。
alter table router_metric
    add column if not exists fleet_total int,
    add column if not exists fleet_alive int;
alter table router_metric_hourly
    add column if not exists fleet_total real,
    add column if not exists fleet_alive real;
