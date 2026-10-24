-- 0088：月报开关。每月 1 号（组织时区）发的上月摘要（reports.rs）只发给开了它的渠道，默认关；不看渠道的最低级别
alter table notify_channel add column if not exists monthly_report boolean not null default false;
