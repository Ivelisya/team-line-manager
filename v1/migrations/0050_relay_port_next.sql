-- 0050：入口端口永不复用（entries::create）。可重复执行。
-- 以前新入口的端口取这台线路机 max(entry.port) + 1；入口在机器物理删除时被删掉（jobs::steps purge），如果删的正好是端口最大的那条，
-- 下一条新入口就拿到同一个端口：客户路由上残留的旧出口会被认成新节点的入口（clash.rs 靠「端口永不复用」配对），旧分享链接也连到新节点。
-- relay.port_next：下一条新入口从这里分（不低于 port_min），分完 +1，与入口还在不在无关。
-- 初值取 max(现有入口的端口, 回收站快照里这台线路机的入口端口) + 1：回收站快照（recycle.rs）的 entries 是
-- [{relay: 线路机的标签, node, port, …}]，按线路机的标签对上（快照里没有线路机 id；线路机改过标签的对不上，那些端口早于现有入口，一般也更小）。
-- 只升不降（重复执行不会退回去）。
alter table relay add column if not exists port_next int not null default 0;

update relay r set port_next = greatest(r.port_next, x.p + 1)
from (select r2.machine_id,
             greatest(coalesce((select max(e.port) from entry e where e.relay_id = r2.machine_id), 0),
                      coalesce((select max((j->>'port')::int)
                                from machine m, recycle_bin b,
                                     jsonb_array_elements(case when jsonb_typeof(b.snapshot->'entries') = 'array' then b.snapshot->'entries' else '[]'::jsonb end) j
                                where m.id = r2.machine_id and b.source = 'fleet' and j->>'relay' = coalesce(m.label, '') and j->>'port' ~ '^[0-9]+$'), 0)) p
      from relay r2) x
where x.machine_id = r.machine_id and x.p > 0;
