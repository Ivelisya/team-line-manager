-- 通知渠道只留 Telegram 和飞书：删掉企业微信、通用 webhook 的渠道（待发的随外键删，发送记录的渠道置空），约束改成只允许这两种
delete from notify_channel where kind not in ('telegram', 'feishu');
alter table notify_channel drop constraint notify_channel_kind_check;
alter table notify_channel add constraint notify_channel_kind_check check (kind in ('telegram', 'feishu'));
