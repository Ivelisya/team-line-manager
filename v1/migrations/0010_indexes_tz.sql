-- 0010：补索引；账单时区改为写入时校验，local_date 不再需要异常块
-- 删告警时 notify_log.incident_id 的外键要扫表；保留期清理按 ts 删审计；按线路机查入口（含已 removed 的）
create index notify_log_incident on notify_log (incident_id);
create index audit_log_ts on audit_log (ts);
create index entry_relay_idx on entry (relay_id);

-- 历史上 billing_tz 是自由文本，写错的值一直靠 local_date 的异常块退回北京时间；这里一次性改成北京时间（列默认值）
do $$
declare r record;
begin
    for r in select distinct billing_tz from subscription loop
        begin
            perform now() at time zone r.billing_tz;
        exception when others then
            update subscription set billing_tz = 'Asia/Shanghai' where billing_tz = r.billing_tz;
        end;
    end loop;
end
$$;

-- 某个账单时区里的「今天」；billing_tz 已在写入时校验（api::machines::check_tz）
create or replace function local_date(tz text) returns date language sql stable as $$
    select (now() at time zone tz)::date
$$;
