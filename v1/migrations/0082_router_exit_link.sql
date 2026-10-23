-- 0082：软路由出口与 fleet 入口「已建立的对应」。出口↔入口只按路由上报的 server:port 对上，路由上有人加一个指向别人入口的出口
-- （凭据乱填）就会被判成参数不一致，运维默认「同步全部」时作业会把那个入口的真凭据写进这台路由。所以默认只同步可信的对应：
-- 中控作业曾把这个出口写成指向该入口的（source = 作业类型），或这个出口在这台路由上曾经参数一致（source = observed，采集时记）。
-- 只增不改；出口、入口、路由删了跟着删。
create table if not exists router_exit_link (
    router_id uuid not null references router on delete cascade,
    exit      text not null,
    entry_id  uuid not null references entry on delete cascade,
    since     timestamptz not null default now(),
    source    text not null,
    primary key (router_id, exit, entry_id)
);

-- 升级时按最近一次采集的快照回填：参数一致的，以及参数不一致但入口和路由同团队（普通团队、不是直出 / 备用）的——
-- 后一类拿走的凭据本来就属于这个团队，不构成越权。其余的要等中控作业写过或采集到一致才算
insert into router_exit_link (router_id, exit, entry_id, source)
select s.router_id, x ->> 'name', e.id, 'migrate'
from router_status s
join router r on r.id = s.router_id
cross join lateral jsonb_array_elements(case when jsonb_typeof(s.data -> 'exits') = 'array' then s.data -> 'exits' else '[]'::jsonb end) x
join entry e on e.id = nullif(x -> 'entry' ->> 'id', '')::uuid
join machine n on n.id = e.node_id
left join team t on t.id = n.team_id
where (x ->> 'sync' = 'ok'
       or (x ->> 'sync' = 'drift' and r.team_id is not null and n.team_id = r.team_id and e.node_id <> e.relay_id and not coalesce(t.spare, false)))
on conflict do nothing;
