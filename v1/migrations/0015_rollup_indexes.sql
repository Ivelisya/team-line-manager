-- 0015：小时汇总与流量统计用的时间索引

-- 遥测小时汇总按 ts 范围扫原始表（不分机器/入口），(machine_id/entry_id, ts) 索引用不上；
-- 原始表按时间顺序追加写入，BRIN 很小、写入几乎不增加开销
create index host_metric_ts_brin   on host_metric   using brin (ts);
create index entry_traffic_ts_brin on entry_traffic using brin (ts);
create index probe_result_ts_brin  on probe_result  using brin (ts);

-- 今日 / 本月流量按 hour 过滤整张入口小时流量表
create index entry_traffic_hourly_hour on entry_traffic_hourly (hour);
