-- 0028：面板里可改的设置（fleet-server settings.rs）。一个 key 一行，没有这一行 = 没在面板里改过，按配置文件的默认值。
-- 目前只有访问记录保留期 access_retention = {"raw_days": 原始记录天数, "hourly_days": 汇总天数}（各 1–90，汇总不短于原始记录），
-- 改动写审计（settings.access_retention）
create table app_setting (
    key         text primary key,
    value       jsonb not null,
    updated_at  timestamptz not null default now(),
    updated_by  text not null
);
