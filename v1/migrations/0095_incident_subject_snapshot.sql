-- 0095：告警记下触发时对象的身份（incident::reconcile 新开时写，之后不改）。可重复执行。
-- 机器会改名、被替换（替换时新旧节点交换编号），只存 subject_id、展示时取现名的话，历史告警会挂到别的名字下（「备用-01 出口不通」其实是当时的团队2-07）。
-- subject_name：触发时的显示名（各引擎的 Want.name：机器的显示名、软路由 / 探测点的名字）；subject_team、subject_ip：机器告警触发时的团队名、出口 IP（别的引擎为空）。
-- 回填：机器告警的 detail.name 是最近一次改摘要时的名字（比现名接近当时）；没有的留空，展示时退回现名。团队、IP 无从得知，留空
alter table incident add column if not exists subject_name text;
alter table incident add column if not exists subject_team text;
alter table incident add column if not exists subject_ip text;

update incident set subject_name = detail->>'name' where subject_name is null and engine = 'machine' and detail ? 'name';
