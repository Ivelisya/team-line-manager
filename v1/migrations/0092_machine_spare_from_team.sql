-- 0092：进备用前所在的团队（id）。可重复执行。
-- 撤销退役（宿主机从备用改回团队）时，随行的同机 IP 节点各回自己进备用前的团队，不再一律拉进宿主机的团队；
-- machines::patch_nobump 进备用时记下（与 spare_since / spare_from 同时写），移出备用时一起清掉。
-- 不加外键：团队删除后这一列仍留着旧 id，撤销时据此判断「原团队已不在，留在备用」；列为空（旧数据）则退回跟宿主机走。
-- 回填：已在备用团队里的机器，取最近一条把它改进备用团队的 machine.rename 审计里的 before.team_id（同 0042 的回填口径）。
alter table machine add column if not exists spare_from_team_id uuid;

update machine m set spare_from_team_id = a.before_team
from (select distinct on (l.target_id) l.target_id, (l.detail->'before'->>'team_id')::uuid before_team
      from audit_log l join machine mm on mm.id = l.target_id join team st on st.id = mm.team_id and st.spare
      where l.action = 'machine.rename'
        and l.detail->'after'->>'team_id' = st.id::text
        and l.detail->'before'->>'team_id' is distinct from st.id::text
        and l.detail->'before'->>'team_id' is not null
      order by l.target_id, l.id desc) a
where m.id = a.target_id and m.spare_since is not null and m.spare_from_team_id is null and m.recycled_at is null;
