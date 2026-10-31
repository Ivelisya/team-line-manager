-- 0095：告警记下触发时对象的身份（incident::reconcile 新开时写，之后不改）。可重复执行。
-- 机器会改名、被替换（替换时新旧节点交换编号），只存 subject_id、展示时取现名的话，历史告警会挂到别的名字下（「备用-01 出口不通」其实是当时的团队2-07）。
-- subject_name：触发时的显示名（各引擎的 Want.name：机器的显示名、软路由 / 探测点的名字）；subject_team、subject_ip：机器告警触发时的团队名、出口 IP（别的引擎为空）。
-- 回填：机器告警的 detail.name 是最近一次改摘要时的名字（比现名接近当时）；没有的留空，展示时退回现名。团队、IP 无从得知，留空
alter table incident add column if not exists subject_name text;
alter table incident add column if not exists subject_team text;
alter table incident add column if not exists subject_ip text;

update incident set subject_name = detail->>'name' where subject_name is null and engine = 'machine' and detail ? 'name';

-- replaced_by：告警开着的机器后来被替换下来时，接替它的节点（replace 作业交换编号时写，见 incident::mark_replaced）。
-- 回填：开了之后最早成功完成的那次 replace 作业（之后在备用里开的不算）；作业记录 90 天后会被清理，所以固化到告警上
alter table incident add column if not exists replaced_by uuid;

update incident i set replaced_by = (select (j.params->>'new_id')::uuid from job j
                                        where j.kind = 'replace' and j.state = 'succeeded' and j.target_id = i.subject_id and j.finished_at >= i.opened_at
                                        order by j.finished_at limit 1)
 where i.engine = 'machine' and i.replaced_by is null;
