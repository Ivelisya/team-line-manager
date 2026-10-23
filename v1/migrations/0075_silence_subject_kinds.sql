-- 0075：静默 / 维护窗口的对象可以是机器、软路由或探测点（以前只能是机器，路由与探测点只能被「全部」的静默连带）。可重复执行。
-- 三种对象各一列外键（subject_id 改名 machine_id，旧数据都是机器），至多填一列，都不填 = 全部：
-- 存在性由外键保证，删机器 / 路由 / 探测点时连带删掉它的静默，不用各删除流程自己记得清理。
-- 接口仍是 subject_kind（machine / router / probe_point）+ subject_id，见 api/incidents.rs。
do $$
begin
    if exists (select 1 from information_schema.columns where table_schema = current_schema() and table_name = 'silence' and column_name = 'subject_id') then
        alter table silence rename column subject_id to machine_id;
        alter table silence rename constraint silence_subject_fk to silence_machine_fk;
        alter index silence_subject rename to silence_machine;
    end if;
end $$;
alter table silence add column if not exists router_id uuid references router on delete cascade;
alter table silence add column if not exists probe_point_id uuid references probe_point on delete cascade;
alter table silence drop constraint if exists silence_one_subject;
alter table silence add constraint silence_one_subject check (num_nonnulls(machine_id, router_id, probe_point_id) <= 1);
create index if not exists silence_router on silence (router_id) where router_id is not null;
create index if not exists silence_probe_point on silence (probe_point_id) where probe_point_id is not null;
