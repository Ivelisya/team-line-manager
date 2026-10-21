-- 0014：失败作业索引、来源小时表按线路机分开、原始访问记录加 id、会话去掉没用的列

-- 机器详情 / 列表按机器查最近失败的作业（上架失败、退役失败）
create index job_failed_by_target on job (target_id, created_at desc) where state = 'failed';

-- 来源小时表按线路机分开：线路机的访问概况 / 来源画像只算经它的流量（同一节点经别的线路机的不算）。
-- 旧数据按该小时该节点连接数最多的线路机归属（换线路机的那个小时会有偏差）
alter table access_source_hourly add column relay_id uuid;
update access_source_hourly s set relay_id = (
    select h.relay_id from access_hourly h where h.node_id = s.node_id and h.hour = s.hour
    group by h.relay_id order by sum(h.conns) desc, h.relay_id limit 1);
delete from access_source_hourly where relay_id is null;
alter table access_source_hourly alter column relay_id set not null,
    drop constraint access_source_hourly_pkey, add primary key (node_id, hour, relay_id, src_ip);
create index access_source_hourly_relay on access_source_hourly (relay_id, hour);

-- 原始访问记录加 id：翻页游标用 (ts, id)，同一毫秒的多条不会被跳过
create sequence access_event_id_seq;
alter table access_event add column id bigint not null default nextval('access_event_id_seq');
alter sequence access_event_id_seq owned by access_event.id;

-- 会话的 ip / user_agent 从来没写也没读过
alter table session drop column ip, drop column user_agent;
