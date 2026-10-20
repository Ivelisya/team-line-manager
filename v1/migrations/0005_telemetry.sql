-- 0005：遥测汇总补算与告警通知

-- 小时汇总脏标记加「最近一次被标脏的时间」：汇总循环只删除它读到之前就已标脏的行，
-- 汇总期间新落库的数据对应的标记不会被误删。
alter table rollup_dirty add column touched_at timestamptz not null default now();

-- 通知记录的类型：firing 触发、repeat 持续提醒、resolved 恢复、test 渠道测试
alter table notify_log add column kind text not null default 'firing';
create index notify_log_ts on notify_log (ts desc);

-- 告警最近一次发出通知（触发或持续提醒）的时间；null = 从未通知过（被静默或没有渠道）
alter table incident add column notified_at timestamptz;
