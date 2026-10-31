-- 0094：告警的「近 7 天第 N 次」在触发时定下（incident::reconcile 新开、重开时写），之后不随同对象的新告警变。可重复执行。
-- occurrence_seq：同（引擎, 规则, 对象）在这条触发（或最近一次重开）时近 7 天出现的次数，含这次；开过的条数 + 恢复后 30 分钟内复发合并进去的次数（同原来列表实时算的口径）。
-- 回填：每条按自己的 opened_at 往前 7 天算（含自己与自己的复发次数）。存量的复发是什么时候重开的没有记录，一律算在它所在那条上
alter table incident add column if not exists occurrence_seq int;

update incident i set occurrence_seq = (
    select count(*) + coalesce(sum(p.reopen_count), 0) from incident p
    where p.engine = i.engine and p.rule = i.rule and p.subject_id = i.subject_id
      and p.opened_at > i.opened_at - interval '7 days' and (p.opened_at, p.id) <= (i.opened_at, i.id))
where i.occurrence_seq is null;

alter table incident alter column occurrence_seq set default 1;
alter table incident alter column occurrence_seq set not null;
