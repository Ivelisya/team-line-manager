-- 0012：告警恢复后短时间内复发时重开原告警（不再新建），记复发次数
alter table incident add column reopen_count int not null default 0;
