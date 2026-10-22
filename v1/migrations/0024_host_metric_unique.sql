-- 0024：主机样本按 (机器, 时刻) 唯一。被攻破的 agent 反复上报同一时刻的样本不再多占磁盘（上报入库 on conflict do nothing）。
-- 分区表的唯一索引必须含分区键 ts；之后常规维护与按需补建的分区（create table … partition of）自动带上它。
-- 原来的 (machine_id, ts desc) 普通索引由它代替（btree 可反向扫描）。先删掉已有的重复样本，同一时刻只留一条。
delete from host_metric h
using (select t, c from (select tableoid t, ctid c, row_number() over (partition by machine_id, ts) rn from host_metric) x where rn > 1) d
where h.tableoid = d.t and h.ctid = d.c;
drop index if exists host_metric_machine_id_ts_idx;
create unique index host_metric_machine_ts on host_metric (machine_id, ts);
