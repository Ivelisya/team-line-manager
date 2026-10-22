-- 0042：0041 的补充。可重复执行。
-- 1. relay_reach.port：这一次连的入口端口（reach.rs 轮流连各端口）。relay_unreachable 按端口判：同一个端口最近 3 次都连不上就报
--    （views.rs 的 r_unreach），整台不通照旧按最近 3 轮。旧行为 null，只参与「整台不通」。
-- 2. 0041 之前就在备用团队里的机器没有 spare_since / spare_from：按审计回填——最近一条把它从别的团队改进备用团队的
--    machine.rename（PATCH、退役进备用、替换节点交换编号都走 machines::patch_tx，都记这一条）。时刻取审计的 ts；
--    原名取 detail.old_name，10-22 之前的记录没有这个键，按 detail.before 的团队（现在的名字）、编号、标签拼；
--    团队已删除的拼不出，原名留空。查不到审计的两列都留空。
-- 3. 0041 把还开着的 renewal_overdue 告警改成了 renewal_due，静默没跟着改：还没结束的按 renewal_overdue 建的静默改成 renewal_due。
alter table relay_reach add column if not exists port int;

update machine m set spare_since = a.ts, spare_from = a.old_name
from (select distinct on (l.target_id) l.target_id, l.ts,
             coalesce(l.detail->>'old_name',
                      (select display_name(mm.kind::text, l.detail->'before'->>'label', bt.name, (l.detail->'before'->>'number')::int)
                       from team bt where bt.id::text = l.detail->'before'->>'team_id')) old_name
      from audit_log l join machine mm on mm.id = l.target_id join team st on st.id = mm.team_id and st.spare
      where l.action = 'machine.rename'
        and l.detail->'after'->>'team_id' = st.id::text
        and l.detail->'before'->>'team_id' is distinct from st.id::text
      order by l.target_id, l.id desc) a
where m.id = a.target_id and m.spare_since is null and m.recycled_at is null;

update silence set rule = 'renewal_due' where rule = 'renewal_overdue' and deleted_at is null and ends_at > now();
