-- 0055：告警按引擎分地盘（incident.rs `Engine`）。可重复执行。
-- 以前各引擎靠 rule 的前缀（ipintel_ / central_ / router_ / probe_，其余是机器）认自己的告警，新加引擎忘了改前缀就会互相把对方的告警恢复掉。
-- engine 由写入的引擎自己带上，没有默认值（漏带写不进去）；回填按以前的前缀规则推出。
-- 同一条规则、同一个对象在不同引擎下互不相干，所以「开着的只有一条」的唯一约束也带上 engine。
alter table incident add column if not exists engine text;
update incident set engine = case
    when rule like 'ipintel!_%' escape '!' then 'ipintel'
    when rule like 'central!_%' escape '!' then 'central'
    when rule like 'router!_%' escape '!' then 'router'
    when rule like 'probe!_%' escape '!' then 'probe'
    else 'machine' end
 where engine is null;
alter table incident alter column engine set not null;
drop index if exists incident_open_one;
create unique index incident_open_one on incident (engine, rule, subject_id) where state = 'firing';
