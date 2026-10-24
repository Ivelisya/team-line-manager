-- 0086：probe_hourly、router_exit_metric_hourly 的 hour 索引由 BRIN 改 btree。各语句可重复执行。
--
-- 0085 给这两张表建了 BRIN (hour)，前提是「按小时写入、天然按时间排布」。实际当前小时每 5 分钟 upsert 一次，
-- 保留期每小时又删掉最老的行，删出的空页被后面的写入复用，页范围里混进新旧不同的 hour，BRIN 的范围只会变宽，
-- 久了每个范围都覆盖整个时间轴，等于全表顺扫。保留期的 delete where hour < … 改用 btree (hour) 才稳定走索引。
-- 0085 已应用到实验室，不改它（sqlx 校验和），这里补一个迁移换掉。

drop index if exists probe_hourly_hour_brin;
drop index if exists router_exit_metric_hourly_hour_brin;
create index if not exists probe_hourly_hour on probe_hourly (hour);
create index if not exists router_exit_metric_hourly_hour on router_exit_metric_hourly (hour);
