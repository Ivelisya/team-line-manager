-- 替换节点把节点级访问策略改挂到新节点时也记一版（kind = 'replace'），版本列表和回滚才对得上现状。
alter table policy_version drop constraint policy_version_kind_check;
alter table policy_version add constraint policy_version_kind_check check (kind in ('publish', 'rollback', 'replace'));
