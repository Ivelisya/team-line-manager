-- 0009：访问汇总补算迟到数据
-- agent 断网很久后补传的访问记录，所在小时早就过了「最近两小时」；上报时把这些小时记进 access_dirty，
-- 汇总循环每轮补算（和遥测的 rollup_dirty 各管各的，互不删对方的标记）。
create table access_dirty (
    hour       timestamptz primary key,
    touched_at timestamptz not null default now()
);

-- 已经被来源画像（node_source_seen）累加过的小时，之后又被补算而变化的连接数差值。
-- 汇总补算写入、ipintel 同步消费（消费时减去已处理的差值，减到 0 的行删除）。
create table access_source_late (
    node_id uuid        not null,
    hour    timestamptz not null,
    src_ip  inet        not null,
    delta   int         not null,
    primary key (node_id, hour, src_ip)
);
create index access_source_late_hour on access_source_late (hour);

-- 某个账单时区里的「今天」；时区名写错时退回北京时间，不让整个机器列表查询报错（billing_tz 是自由文本）
create function local_date(tz text) returns date language plpgsql stable as $$
begin
    return (now() at time zone tz)::date;
exception when others then
    return (now() at time zone 'Asia/Shanghai')::date;
end
$$;
