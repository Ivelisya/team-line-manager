-- 0017：补外键列上的索引

-- 续费记录按机器列出；彻底删除机器时级联删它的续费记录也按 machine_id 找
create index renewal_machine on renewal (machine_id);

-- 原来的部分索引只含未撤销的令牌，彻底删除机器时级联删令牌（含已撤销的）用不上它，只能扫全表。
-- 按机器查令牌的地方（鉴权、轮换、撤销）都带 revoked_at is null，但每台机器只有几条令牌，
-- 全量索引一样够用：换成全量的，不两份都维护
drop index agent_token_machine;
create index agent_token_machine on agent_token (machine_id);

-- 删除通知渠道时外键 on delete set null 要按 channel_id 找它的发送记录
create index notify_log_channel on notify_log (channel_id);

-- 更正 0016 的说明（已执行过的迁移不能再改文件，校验和会对不上）：access_target_seen 并不小，
-- 它是 (节点, 目标) 的累计表，保留 90 天，随目标数增长。删掉 access_target_seen_last 是因为只有每小时一次的保留期清理用它，
-- 那一次顺序扫描的代价比汇总每 5 分钟改 last_seen 都要连带写索引（不能走 HOT、索引膨胀）小
