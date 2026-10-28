-- 0096：团队编号唯一改成可延迟检查的排他约束（同名 machine_number_live，规则不变：未退役的非线路机在团队里编号唯一）。可重复执行。
-- 唯一索引不能延迟检查，替换节点交换两台的编号（新节点接旧编号、旧节点接新节点空出来的号）只能先把旧节点放到临时号再改过去，
-- 审计里多出临时名（「备用-04」）和链式改名。改成 deferrable 后作业在事务里 set constraints machine_number_live deferred，直接交换、提交时再查；
-- 其余写入照旧当场检查（initially immediate）
drop index if exists machine_number_live;
alter table machine drop constraint if exists machine_number_live;
alter table machine add constraint machine_number_live
    exclude using btree (team_id with =, number with =) where (kind <> 'relay' and lifecycle <> 'retired')
    deferrable initially immediate;
