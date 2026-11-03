-- 0107：穿透入口可指定 bridge 端口。可重复执行。
-- bridge_port：节点上的 bridge 连线路机的这个端口（不填 = 连入口端口）。供应商只放行节点出站到少数端口（53 80 443 853 2096 8080 8443）时用：
--       线路机上这个入口的 Reality 入站同时听入口端口和 bridge_port。只在开穿透时写（fleetctl entry reverse --on --bridge-port N），
--       关穿透验收通过后清空；同一台线路机上不重复（已移除的入口不占）。端口与入口端口、AnyTLS 端口等的冲突由中控在开穿透时核对。
alter table entry add column if not exists bridge_port integer;
alter table entry drop constraint if exists entry_bridge_port_check;
alter table entry add constraint entry_bridge_port_check check (bridge_port between 1 and 65535);
create unique index if not exists entry_relay_bridge_port_uq on entry (relay_id, bridge_port) where bridge_port is not null and lifecycle <> 'removed';
