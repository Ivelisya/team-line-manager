-- 0085：几处按时间 / 按告警取数的索引。各语句可重复执行。
--
-- 1. probe_hourly（主键 entry_id, hour, kind）、router_exit_metric_hourly（主键 router_id, exit, hour）：
--    db.rs 保留期每小时 delete where hour < now() - 180 天，主键以 hour 不在前，只能全表顺扫。
--    按小时写入、天然按时间排布，加 BRIN (hour) 几乎不占空间。router_metric_hourly 很小不加；relay_reach_hourly 已有 hour 索引。
-- 2. router_exit_metric（原始表，每 30 秒每出口一行）：网页按 `router_id = $1 and ts >= $2` 取最近一段，
--    主键 (router_id, exit, ts) 用不上时间范围；主键改 (router_id, ts, exit)（唯一性不变），按时间查正好用上，写入也近似追加。
--    别处没有查询依赖 (router_id, exit, ...) 前缀：小时汇总与保留期清理只按 ts（走原有的 BRIN），删路由的级联只要 router_id 前缀。
-- 3. notify_log：告警评估每轮对每条进行中告警取「最近一条送达的通知」（alerts.rs TOLD_RESOLVED_SQL：
--    where incident_id = ? and ok and kind in (…) order by ts desc, id desc limit 1），原来的单列 (incident_id) 索引取出这条告警全部通知再排序。
--    新索引 (incident_id, ts desc, id desc) 直接按序取；它的 incident_id 前缀也足够外键（删告警）用，旧的单列索引删掉。
-- 4. incident：「常复发」列表按 opened_at 取近 7 天，没有以 opened_at 开头的索引，只能全表扫。

create index if not exists probe_hourly_hour_brin on probe_hourly using brin (hour);
create index if not exists router_exit_metric_hourly_hour_brin on router_exit_metric_hourly using brin (hour);

do $$
begin
    if (select array_agg(a.attname::text order by k.ord)
        from pg_index i
        cross join lateral unnest(i.indkey) with ordinality k(attnum, ord)
        join pg_attribute a on a.attrelid = i.indrelid and a.attnum = k.attnum
        where i.indrelid = 'router_exit_metric'::regclass and i.indisprimary) is distinct from array['router_id', 'ts', 'exit'] then
        alter table router_exit_metric drop constraint router_exit_metric_pkey;
        alter table router_exit_metric add primary key (router_id, ts, exit);
    end if;
end $$;

create index if not exists notify_log_incident_ts on notify_log (incident_id, ts desc, id desc);
drop index if exists notify_log_incident;

create index if not exists incident_opened_at on incident (opened_at desc);
