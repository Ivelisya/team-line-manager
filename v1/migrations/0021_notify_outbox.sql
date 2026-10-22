-- 0021：告警通知送达前不丢。待发的通知写进库里（原来在内存队列里，中控重启、部署就丢），发送循环按渠道逐条发，
-- 发成功或放弃（入队满 30 分钟还没发出）才删掉并记 notify_log；渠道连续失败的状态也落库，渠道连续失败的告警看它

create table notify_outbox (
    id          bigserial primary key,                          -- 同一渠道按 id 顺序发
    channel_id  uuid not null references notify_channel on delete cascade,
    incident_id uuid references incident on delete cascade,
    notice      jsonb not null,                                 -- 入队时的整条通知（notify::Notice）：按入队时的样子发
    created_at  timestamptz not null default now()
);
create index notify_outbox_channel on notify_outbox (channel_id, id);
-- 删告警（保留期清理、彻底删除机器）时级联删它没发出的通知
create index notify_outbox_incident on notify_outbox (incident_id);

-- 渠道从什么时候起一直发不出去（null = 最近一次发成功了，或从没失败过）、连续失败几次、最近一次的原因。
-- 每次发送（含渠道测试）后更新；连续失败满 5 分钟开告警 central_channel_failing（selfmon.rs）
alter table notify_channel
    add column failing_since timestamptz,
    add column fail_count    int not null default 0,
    add column last_error    text;
