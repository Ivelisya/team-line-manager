-- 0093：穿透入口（DESIGN.md「穿透入口」）。可重复执行。
-- mode：normal = 线路机连节点的 ssserver（原来的方式）；reverse = 节点上的 bridge 主动连这个入口自己的 REALITY 入站，
--       客户流量经这条隧道交给节点（供应商丢入站 SYN 的节点用）。存量入口一律 normal，不自动切换。
-- bridge_uuid：bridge 连入口时用的客户端 UUID（与客户的 client_uuid 分开：Xray 只许这个客户端建隧道、不许它当代理用）。
--       每个入口一个，建表时就生成，开关穿透不换；切回普通模式后它不在期望状态里，等于失效。
-- reverse_kick_at：中控决定让节点重启 bridge 的时刻（线路机报这个入口的隧道没挂上满 30 秒）。节点 agent 的回执带上它，同一个值只重启一次；
--       同一入口 3 分钟内不重复叫
alter table entry add column if not exists mode text not null default 'normal';
alter table entry drop constraint if exists entry_mode_check;
alter table entry add constraint entry_mode_check check (mode in ('normal', 'reverse'));
alter table entry add column if not exists bridge_uuid uuid not null default gen_random_uuid();
alter table entry add column if not exists reverse_kick_at timestamptz;
