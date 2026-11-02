-- 0098：计费台账两处收尾。可重复执行。
-- 1. 旧格式的 init 记录（每个节点一条，detail 只有 {region, start_date}，节点名在 machine_name）按 (team_id, 起算日) 合并成一条新格式
--    （detail = {start_date, count, nodes:[{name, region}]}，同 ledger.rs 的 log_init），at 取组内最小值；合并后旧行删掉，第二次执行无旧行即无操作。
with old as (
    select * from billing_event where kind = 'init' and not jsonb_exists(detail, 'count')
), merged as (
    insert into billing_event (id, team_id, slot_id, at, kind, machine_name, other_name, detail)
    select gen_random_uuid(), team_id, null, min(at), 'init', null, null,
           jsonb_build_object('start_date', detail -> 'start_date', 'count', count(*),
                              'nodes', jsonb_agg(jsonb_build_object('name', machine_name, 'region', detail -> 'region') order by at, machine_name))
    from old group by team_id, detail -> 'start_date'
    returning 1
)
delete from billing_event where id in (select id from old);

-- 2. 名额空出的原因落库（原来读取时按「现在」是否不计费推断，节点后来恢复计费就变了）：exempt = 空出时节点标着不计费；left = 离开团队、退役、进备用或被删。
--    对账空出时写，被接回（原节点回来、替换）时清空，名额结束时保留；手动名额恒为 null。
alter table billing_slot add column if not exists vacate_reason text check (vacate_reason in ('exempt', 'left'));
-- 回填现有没有占用者的系统名额：库里没存当时的状态，只能按现在的 billing_exempt 近似一次
update billing_slot s set vacate_reason = case when exists (select 1 from billing_exempt x where x.machine_id = s.prev_holder_id) then 'exempt' else 'left' end
where not s.manual and s.holder_id is null and s.vacate_reason is null;
